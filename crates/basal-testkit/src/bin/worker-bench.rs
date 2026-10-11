//! Measurements for the worker: spawn, respawn and warm-pool cost, IPC cost
//! per activation and per new host call, prefix replay, RSS with realistic
//! payloads, and a payload fuzzing run. It is a real parent process: point
//! it at any `ck-basal-worker` binary, including a signed one placed outside
//! the build directory.
//!
//! ```text
//! worker-bench --worker PATH [--out FILE] [--fuzz COUNT]
//! ```
//!
//! The committed run is `evidence/slice-1-measurements.json`, written with
//! `--out` and `--fuzz 2000` against a worker that
//! `script/sign-worker.sh place` had signed and copied to a scratch
//! directory.
//!
//! Every sample is kept in the output; summaries are the median and the
//! nearest-rank 95th percentile.

use basal_testkit::command::Command;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use basal_proto::ActivationResult;
use basal_testkit::{TestParent, WorkerProcess, fuzz};
use serde_json::{Value, json};

const HANDSHAKE: Duration = Duration::from_secs(30);
const FUZZ_SEED: u64 = 0xBA5A1;
const FUZZ_JS_TIME_MICROS: u64 = 200_000;

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn summary(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    if n == 0 {
        return json!({"runs": 0});
    }
    let median = sorted[n / 2];
    let p95 = sorted[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1];
    json!({"runs": n, "median": median, "p95": p95, "min": sorted[0], "max": sorted[n - 1], "samples": sorted})
}

fn shell(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn machine() -> Value {
    json!({
        "cpu": shell("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "model": shell("sysctl", &["-n", "hw.model"]),
        "logical_cores": shell("sysctl", &["-n", "hw.ncpu"]),
        "memory_bytes": shell("sysctl", &["-n", "hw.memsize"]),
        "os": shell("sw_vers", &["-productVersion"]),
        "os_build": shell("sw_vers", &["-buildVersion"]),
        "load_average_at_start": shell("sysctl", &["-n", "vm.loadavg"]),
    })
}

fn completed(parent: &mut TestParent, run: &str, script: &str) -> Duration {
    let report = parent.run(run, script);
    match report.result() {
        Some(ActivationResult::Completed { .. }) => report.wall,
        _ => panic!("benchmark activation failed: {report:#?}"),
    }
}

/// Time from spawning a fresh worker process to receiving its handshake
/// reply (`Welcome`).
fn spawn_cost(worker: &Path, runs: usize) -> (Value, f64) {
    let mut samples = Vec::new();
    let mut first = 0.0;
    for i in 0..runs {
        let start = Instant::now();
        let (w, _) = WorkerProcess::start(worker, HANDSHAKE).expect("spawn");
        let took = micros(start.elapsed());
        if i == 0 {
            first = took;
        }
        samples.push(took);
        drop(w);
    }
    (summary(&samples), first)
}

/// SIGKILL of a live worker, reaping it, and a replacement reaching Welcome.
fn respawn_cost(worker: &Path, runs: usize) -> Value {
    let mut samples = Vec::new();
    let (mut current, _) = WorkerProcess::start(worker, HANDSHAKE).expect("spawn");
    for _ in 0..runs {
        let start = Instant::now();
        current.kill();
        let (next, _) = WorkerProcess::start(worker, HANDSHAKE).expect("respawn");
        samples.push(micros(start.elapsed()));
        current = next;
    }
    summary(&samples)
}

fn activation_costs(worker: &PathBuf, runs: usize) -> Value {
    let mut warm = Vec::new();
    let mut parent = TestParent::new(worker);
    completed(&mut parent, "warmup", "return 1");
    for i in 0..runs {
        warm.push(micros(completed(&mut parent, &format!("w{i}"), "return 1")));
    }
    let mut cold = Vec::new();
    for i in 0..runs.min(50) {
        let start = Instant::now();
        let mut parent = TestParent::new(worker);
        completed(&mut parent, &format!("c{i}"), "return 1");
        cold.push(micros(start.elapsed()));
    }
    json!({
        "warm_pool_activation_us": summary(&warm),
        "cold_spawn_plus_activation_us": summary(&cold),
    })
}

/// Cost per host call that crosses the channel: the time of an activation
/// making K sequential calls, minus an activation making none, divided by K.
/// Measured for asynchronous op calls and for synchronous clock reads.
fn host_call_costs(worker: &PathBuf, batches: usize, k: usize) -> Value {
    let mut parent = TestParent::new(worker);
    parent.budgets.js_time_micros = 30_000_000;
    let async_script =
        format!("for (let i = 0; i < {k}; i++) {{ await ops.call('mock', 'echo', i); }} return 1;");
    let sync_script =
        format!("let t = 0; for (let i = 0; i < {k}; i++) {{ t += Date.now(); }} return 1;");
    let baseline_script = "for (let i = 0; i < 1; i++) {} return 1;";
    completed(&mut parent, "warmup", &async_script);
    let mut per_async = Vec::new();
    let mut per_sync = Vec::new();
    for b in 0..batches {
        let base = micros(completed(&mut parent, &format!("b{b}"), baseline_script));
        let a = micros(completed(&mut parent, &format!("a{b}"), &async_script));
        let s = micros(completed(&mut parent, &format!("s{b}"), &sync_script));
        per_async.push((a - base) / k as f64);
        per_sync.push((s - base) / k as f64);
        parent.journals.clear();
    }
    json!({
        "calls_per_batch": k,
        "async_op_call_us": summary(&per_async),
        "sync_clock_read_us": summary(&per_sync),
    })
}

/// Replays an N-call prefix shipped in one message, then one new call.
fn replay_costs(worker: &PathBuf, runs: usize, sizes: &[usize], payload_bytes: usize) -> Value {
    let mut out = serde_json::Map::new();
    let filler = "x".repeat(payload_bytes);
    for &n in sizes {
        let mut parent = TestParent::new(worker);
        parent.budgets.js_time_micros = 60_000_000;
        parent.budgets.max_value_bytes = 64 * 1024;
        let script = format!(
            "const pad = '{filler}'; for (let i = 0; i < {n}; i++) {{ await ops.call('mock', 'echo', {{ i, pad }}); }} \
             return (await ops.call('mock', 'echo', 'new')).length;"
        );
        completed(&mut parent, "record", &script);
        let recorded = parent.journal("record").clone();
        let mut samples = Vec::new();
        let mut new_calls = Vec::new();
        for i in 0..runs {
            let mut journal = recorded.clone();
            journal.truncate(n);
            parent.journals.insert("replay".into(), journal);
            let report = parent.run("replay", &script);
            assert!(
                matches!(report.result(), Some(ActivationResult::Completed { .. })),
                "replay {i} of {n}: {report:#?}"
            );
            new_calls.push(report.host_calls.len());
            samples.push(micros(report.wall));
        }
        assert!(new_calls.iter().all(|c| *c == 1), "{new_calls:?}");
        out.insert(
            format!("replay_{n}_calls_payload_{payload_bytes}B_then_one_new_us"),
            summary(&samples),
        );
    }
    Value::Object(out)
}

/// Peak RSS of one worker while an activation holds `count` 16 KiB values.
fn rss(worker: &PathBuf) -> Value {
    let mut parent = TestParent::new(worker);
    completed(&mut parent, "warmup", "return 1");
    let pid = parent.worker().expect("worker").pid();
    let idle = parent.worker().expect("worker").rss_kib();

    let raw =
        serde_json::to_string(&json!({"text": "y".repeat(16 * 1024 - 16)})).unwrap_or_default();
    let raw_literal = serde_json::to_string(&raw).unwrap_or_default();
    let mut points = serde_json::Map::new();
    for count in [1usize, 32, 128] {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let sampler = {
            let stop = stop.clone();
            let peak = peak.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let kib = basal_testkit::process::rss_kib(pid).unwrap_or(0);
                    peak.fetch_max(kib, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(2));
                }
            })
        };
        let script = format!(
            "const held = []; for (let i = 0; i < {count}; i++) {{ held.push(await ops.call('mock', 'raw', {{ raw: {raw_literal} }})); }} \
             await ops.call('mock', 'sleep', {{ ms: 200 }}); return held.length;"
        );
        parent.budgets.memory_bytes = 64 * 1024 * 1024;
        completed(&mut parent, &format!("rss{count}"), &script);
        stop.store(true, Ordering::Relaxed);
        let _ = sampler.join();
        let after = parent.worker().expect("worker").rss_kib();
        points.insert(
            format!("{count}_values_of_16KiB"),
            json!({"peak_rss_kib": peak.load(Ordering::Relaxed), "rss_after_kib": after}),
        );
        parent.journals.clear();
    }

    // Eight idle workers side by side: the resident memory each process
    // costs before it runs anything.
    let mut pool = Vec::new();
    for _ in 0..8 {
        pool.push(WorkerProcess::start(worker, HANDSHAKE).expect("spawn").0);
    }
    let idle_pool: Vec<u64> = pool.iter().filter_map(|w| w.rss_kib()).collect();
    json!({
        "idle_after_handshake_and_one_activation_kib": idle,
        "during_activation": points,
        "idle_pool_of_8_kib_each": idle_pool,
    })
}

fn main() {
    let mut worker = None;
    let mut out = None;
    let mut fuzz_count = 2000u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--worker" => {
                worker = Some(PathBuf::from(
                    args.next()
                        .unwrap_or_else(|| argument_error("--worker needs a path")),
                ))
            }
            "--out" => {
                out = Some(PathBuf::from(
                    args.next()
                        .unwrap_or_else(|| argument_error("--out needs a path")),
                ))
            }
            "--fuzz" => {
                fuzz_count = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| argument_error("--fuzz needs an unsigned count"))
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(64);
            }
        }
    }
    let Some(worker) = worker else {
        eprintln!("usage: worker-bench --worker PATH [--out FILE] [--fuzz COUNT]");
        std::process::exit(64);
    };

    let started = Instant::now();
    eprintln!("spawn");
    let (spawn, first_spawn) = spawn_cost(&worker, 50);
    eprintln!("respawn");
    let respawn = respawn_cost(&worker, 50);
    eprintln!("activations");
    let activations = activation_costs(&worker, 200);
    eprintln!("host calls");
    let calls = host_call_costs(&worker, 30, 1000);
    eprintln!("replay");
    let replay_small = replay_costs(&worker, 20, &[10, 100, 1000], 0);
    let replay_large = replay_costs(&worker, 10, &[100, 1000], 16 * 1024 - 64);
    eprintln!("rss");
    let rss = rss(&worker);
    eprintln!("fuzz");
    let mut parent = TestParent::new(&worker);
    parent.budgets.js_time_micros = FUZZ_JS_TIME_MICROS;
    parent.deadline = Duration::from_secs(30);
    let fuzz_started = Instant::now();
    let tally = fuzz::run(&mut parent, FUZZ_SEED, fuzz_count);
    let fuzz_wall = fuzz_started.elapsed();

    let report = json!({
        "worker": worker.display().to_string(),
        "machine": machine(),
        "first_spawn_to_welcome_us": first_spawn,
        "spawn_to_welcome_us": spawn,
        "kill_and_respawn_to_welcome_us": respawn,
        "activations": activations,
        "host_calls": calls,
        "replay": [replay_small, replay_large],
        "rss": rss,
        "fuzz": {
            "seed": FUZZ_SEED,
            "payloads": fuzz_count,
            "regex_cases": fuzz::regex_cases().len(),
            "js_time_budget_us": FUZZ_JS_TIME_MICROS,
            "wall_seconds": fuzz_wall.as_secs_f64(),
            "completed": tally.completed,
            "invalid_host_value": tally.invalid_value,
            "budget_js_time": tally.js_time,
            "budget_memory": tally.memory,
            "budget_stack": tally.stack,
            "script_error": tally.script_error,
            "unexpected": tally.unexpected,
        },
        "load_average_at_end": shell("sysctl", &["-n", "vm.loadavg"]),
        "total_seconds": started.elapsed().as_secs_f64(),
    });
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    match out {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, text + "\n") {
                eprintln!("writing {}: {e}", path.display());
                std::process::exit(1);
            }
        }
        None => println!("{text}"),
    }
}

fn argument_error(message: &str) -> ! {
    eprintln!("worker-bench: {message}");
    std::process::exit(64)
}
