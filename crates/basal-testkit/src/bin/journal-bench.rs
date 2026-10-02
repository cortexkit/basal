//! Measurements for the parent side: the store and the activation driver.
//!
//! ```text
//! cargo run --release -p basal-testkit --bin journal-bench -- \
//!     --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json
//! ```
//!
//! - Commit cost: one small write transaction through the store, under
//!   `synchronous = FULL`, with and without `F_FULLFSYNC`.
//! - Replay of 10, 100 and 1000 recorded calls through the parent with the
//!   store: a run that made N calls and then suspended on a long call is
//!   resumed on a warm worker. The activation claims the run, reads the
//!   journal, ships the prefix, the worker replays it, the long call's
//!   outcome is released (one commit) and the run succeeds (one commit).
//! - A new call end to end: M sequential asynchronous calls, each journaled,
//!   dispatched on its own thread, answered, recorded, released and
//!   delivered (three commits per call), and M synchronous clock reads (one
//!   commit each), minus an activation with no calls, divided by M.
//!
//! Each sample uses its own copy of a recorded store, so samples do not
//! share state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use basal_core::{ActivationEnd, Config, Durability, NoHooks, Runtime, Store, TriggerSpec};
use basal_host::MockCatalog;
use basal_host::mock::MockHost;
use basal_host::{Completion, HostOutcome};
use basal_proto::JsonText;
use basal_testkit::ProcessSource;
use basal_testkit::harness::{approve_spec, scratch, test_manifest};
use serde_json::{Value, json};

struct Args {
    worker: PathBuf,
    out: Option<PathBuf>,
    samples: usize,
}

fn parse() -> Result<Args, String> {
    let mut worker = None;
    let mut out = None;
    let mut samples = 20;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--worker" => worker = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--samples" => {
                samples = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--samples needs a number")?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        worker: worker.ok_or("--worker is required")?,
        out,
        samples,
    })
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Median (upper middle for even counts) and nearest-rank p95.
fn stats(samples: &[f64]) -> Value {
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n == 0 {
        return json!({"n": 0});
    }
    let median = s[n / 2];
    let p95 = s[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1];
    json!({"n": n, "median_us": median, "p95_us": p95, "min_us": s[0], "max_us": s[n - 1], "samples_us": s})
}

/// Installs and approves `script` as the bench flow and returns its spec.
fn spec(rt: &Runtime, script: &str) -> Result<TriggerSpec, String> {
    let mut manifest = test_manifest();
    manifest["id"] = json!("bench");
    approve_spec(rt, script, &manifest, "t", JsonText::null())
}

fn runtime(
    dir: &Path,
    fullfsync: bool,
    mock: &MockHost,
    source: Arc<ProcessSource>,
) -> Result<Runtime, String> {
    let store =
        Store::open(dir.join("basal.db"), Durability { fullfsync }).map_err(|e| e.to_string())?;
    let config = Config {
        activation_deadline: Duration::from_secs(300),
        budgets: basal_proto::Budgets {
            js_time_micros: 30_000_000,
            ..basal_proto::Budgets::default()
        },
        // The bench makes up to 1000 calls in one run; the dispatch budget
        // is not what it measures.
        rate: basal_core::RateLimits {
            max_dispatches: u32::MAX,
            ..basal_core::RateLimits::default()
        },
        install_gate: basal_core::InstallGate::Off,
        ..Config::default()
    };
    Ok(Runtime::new(
        Arc::new(store),
        Arc::new(mock.clone()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        Some(source),
        config,
    ))
}

fn commit_cost(fullfsync: bool, samples: usize) -> Result<Value, String> {
    let dir = scratch("bench-commit");
    let store =
        Store::open(dir.join("basal.db"), Durability { fullfsync }).map_err(|e| e.to_string())?;
    let pragmas = store.pragmas().map_err(|e| e.to_string())?;
    store
        .write(|tx| {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS bench (id INTEGER PRIMARY KEY, v TEXT)")?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    let mut times = Vec::new();
    for i in 0..samples {
        let started = Instant::now();
        store
            .write(|tx| {
                tx.execute("INSERT INTO bench (v) VALUES (?1)", [format!("row {i}")])?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        times.push(micros(started.elapsed()));
    }
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    Ok(json!({
        "fullfsync": fullfsync,
        "synchronous": pragmas.synchronous,
        "fullfsync_in_effect": pragmas.fullfsync,
        "commit": stats(&times),
    }))
}

/// Records a run of `n` sequential calls that then suspends on an llm
/// call, and returns the closed store's directory and the run id.
fn record(n: usize, worker: &Path) -> Result<(PathBuf, String), String> {
    let dir = scratch("bench-record");
    let mock = MockHost::new();
    let source = Arc::new(ProcessSource::new(worker));
    let rt = runtime(&dir, false, &mock, source)?;
    let script = format!(
        "for (let i = 0; i < {n}; i++) {{ await ops.call('mock', 'echo', {{ i }}); }} \
         const x = await llm({{ prompt: 'p' }}); return x.done;"
    );
    let run_id = rt
        .admit(&spec(&rt, &script)?)
        .map_err(|e| e.to_string())?
        .run_id()
        .ok_or("not admitted")?
        .to_owned();
    loop {
        match rt.resume(&run_id).map_err(|e| e.to_string())? {
            ActivationEnd::Suspended { .. } => break,
            // Something arrived as it suspended; it is runnable again.
            ActivationEnd::Requeued => continue,
            other => return Err(format!("recording ended {other:?}")),
        }
    }
    rt.quiesce();
    drop(rt);
    Ok((dir, run_id))
}

fn replay(n: usize, fullfsync: bool, args: &Args) -> Result<Value, String> {
    let (recorded, run_id) = record(n, &args.worker)?;
    let source = Arc::new(ProcessSource::new(&args.worker));
    let mut times = Vec::new();
    let mut prefix_calls = 0;
    for _ in 0..args.samples {
        let dir = scratch("bench-replay");
        std::fs::copy(recorded.join("basal.db"), dir.join("basal.db"))
            .map_err(|e| e.to_string())?;
        let mock = MockHost::new();
        let rt = runtime(&dir, fullfsync, &mock, source.clone())?;
        let calls = rt.calls(&run_id).map_err(|e| e.to_string())?;
        prefix_calls = calls.len();
        let last = calls.last().ok_or("no calls")?;
        rt.complete(&Completion {
            run_id: run_id.clone(),
            position: last.position,
            handle: last.handle.clone().ok_or("no handle")?,
            outcome: HostOutcome::fulfilled(
                JsonText::new("{\"done\":true}").map_err(|e| e.to_string())?,
            ),
        })
        .map_err(|e| e.to_string())?;
        let mut worker = source.spawn().map_err(|e| e.to_string())?;
        let started = Instant::now();
        let end = rt
            .activate(&run_id, &mut worker)
            .map_err(|e| e.to_string())?;
        times.push(micros(started.elapsed()));
        if !matches!(end, ActivationEnd::Succeeded { .. }) {
            return Err(format!("replay of {n} ended {end:?}"));
        }
        if mock.total_sends() != 0 {
            return Err("a replayed call reached the host".into());
        }
        drop(rt);
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(recorded);
    Ok(
        json!({"recorded_calls": n, "prefix_rows": prefix_calls, "fullfsync": fullfsync, "activation": stats(&times)}),
    )
}

/// Time for one activation of `script` on a warm worker.
fn activation_time(script: &str, fullfsync: bool, worker: &Path) -> Result<f64, String> {
    let dir = scratch("bench-calls");
    let mock = MockHost::new();
    let source = Arc::new(ProcessSource::new(worker));
    let rt = runtime(&dir, fullfsync, &mock, source.clone())?;
    let run_id = rt
        .admit(&spec(&rt, script)?)
        .map_err(|e| e.to_string())?
        .run_id()
        .ok_or("not admitted")?
        .to_owned();
    let mut w = source.spawn().map_err(|e| e.to_string())?;
    let started = Instant::now();
    let end = rt.activate(&run_id, &mut w).map_err(|e| e.to_string())?;
    let t = micros(started.elapsed());
    if !matches!(end, ActivationEnd::Succeeded { .. }) {
        return Err(format!("ended {end:?}"));
    }
    rt.quiesce();
    drop(rt);
    let _ = std::fs::remove_dir_all(dir);
    Ok(t)
}

fn per_call(fullfsync: bool, calls: usize, batches: usize, worker: &Path) -> Result<Value, String> {
    let empty = "return 1;";
    let asynchronous = format!(
        "for (let i = 0; i < {calls}; i++) {{ await ops.call('mock', 'echo', {{ i }}); }} return 1;"
    );
    let synchronous =
        format!("let t = 0; for (let i = 0; i < {calls}; i++) {{ t = Date.now(); }} return 1;");
    let mut a = Vec::new();
    let mut s = Vec::new();
    for _ in 0..batches {
        let base = activation_time(empty, fullfsync, worker)?;
        a.push((activation_time(&asynchronous, fullfsync, worker)? - base) / calls as f64);
        s.push((activation_time(&synchronous, fullfsync, worker)? - base) / calls as f64);
    }
    Ok(json!({
        "fullfsync": fullfsync,
        "calls_per_batch": calls,
        "async_call": stats(&a),
        "sync_call": stats(&s),
    }))
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("journal-bench: {e}");
            std::process::exit(64);
        }
    };
    let result = (|| -> Result<Value, String> {
        let mut commit = Vec::new();
        for fullfsync in [false, true] {
            eprintln!("commit cost, fullfsync {fullfsync}");
            commit.push(commit_cost(fullfsync, 200)?);
        }
        let mut replays = Vec::new();
        for fullfsync in [false, true] {
            for n in [10, 100, 1000] {
                eprintln!("replay {n}, fullfsync {fullfsync}");
                replays.push(replay(n, fullfsync, &args)?);
            }
        }
        let mut calls = Vec::new();
        for fullfsync in [false, true] {
            eprintln!("per call, fullfsync {fullfsync}");
            calls.push(per_call(fullfsync, 200, 10, &args.worker)?);
        }
        Ok(json!({
            "worker": args.worker,
            "commit": commit,
            "replay": replays,
            "new_call": calls,
        }))
    })();
    match result {
        Ok(v) => {
            let text = serde_json::to_string_pretty(&v).unwrap_or_default();
            if let Some(out) = &args.out
                && let Err(e) = std::fs::write(out, &text)
            {
                eprintln!("journal-bench: writing {}: {e}", out.display());
            }
            println!("{text}");
        }
        Err(e) => {
            eprintln!("journal-bench: {e}");
            std::process::exit(1);
        }
    }
}
