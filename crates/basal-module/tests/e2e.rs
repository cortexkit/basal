//! End to end with the real module process (`ck-basal-harness`: the module
//! against wire-shaped fake providers or mock hosts over stdio), the real
//! worker, and real kills.
//!
//! A schedule flow is installed and its card approved through the mock
//! consent; the clock jumps over a sleep gap so a catch-up fires; the
//! worker is killed mid-activation; the module process is killed with
//! SIGKILL between a sink write's effect and the commit of its outcome; and
//! after a restart the flow's sink writes have happened exactly once each.
//! A second test cuts the store under a running activation: the process
//! exits non-zero and the restart recovers the run.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use basal_module::fatal::EXIT_STORE_FAILURE;
use basal_testkit::channel::worker_binary;
use serde_json::{Value, json};

/// 2026-05-01T00:00:00Z, the harness's starting clock.
const T0: i64 = 1_777_593_600_000;
const HOUR: i64 = 3_600_000;

struct Harness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Harness {
    fn start(dir: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ck-basal-harness"))
            .arg("--dir")
            .arg(dir)
            .arg("--worker")
            .arg(worker_binary())
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the harness");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Sends a command; `None` when the process died before answering.
    fn send(&mut self, command: Value) -> Option<Value> {
        writeln!(self.stdin, "{command}").ok()?;
        self.stdin.flush().ok()?;
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&line).ok(),
        }
    }

    fn ok(&mut self, command: Value) -> Value {
        let reply = self.send(command.clone()).expect("the harness answered");
        assert_eq!(reply["ok"], true, "{command} -> {reply:#}");
        reply["result"].clone()
    }

    fn wait(mut self) -> ExitStatus {
        drop(self.stdin);
        self.child.wait().expect("wait")
    }

    fn quit(mut self) {
        self.ok(json!({ "cmd": "quit" }));
        let status = self.wait();
        assert!(status.success(), "{status:?}");
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("basal-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

const SCRIPT: &str = "const before = (await kv.get('fires')) ?? 0;\n\
const echo = await ops.call('mock', 'echo', { due: trigger.due });\n\
await sink.digest('SYNAPSE', { title: 'fire', body: trigger.due }, 'piggyback');\n\
await kv.set('fires', before + 1);\n\
return { due: echo.due, before, missed: trigger.catch_up ? trigger.catch_up.missed_count : 0 };";

fn manifest(id: &str, schedule: Value) -> String {
    json!({
        "id": id,
        "version": 1,
        "purpose": "Tell SYNAPSE every hour.",
        "trigger": { "schedule": schedule },
        "sinks": [ { "agent": "SYNAPSE", "digest_max": "piggyback" } ],
        "ops": [ { "module": "mock", "op": "echo" } ]
    })
    .to_string()
}

/// Installs as SYNAPSE and approves the card as the operator.
fn install_and_approve(h: &mut Harness, id: &str, schedule: Value) {
    let reply = h.ok(json!({
        "cmd": "op",
        "as": { "agent": "SYNAPSE" },
        "method": "flow.install",
        "params": { "script": SCRIPT, "manifest": manifest(id, schedule) },
    }));
    assert_eq!(reply["state"], "pending");
    let cards = h.ok(json!({ "cmd": "cards" }));
    assert_eq!(cards[0]["card_id"], reply["card_id"]);
    h.ok(
        json!({ "cmd": "decide", "card_id": reply["card_id"], "approve": true, "by": "operator" }),
    );
    let health = h.ok(json!({ "cmd": "op", "as": "operator", "method": "flow.health" }));
    assert_eq!(health["flows"][0]["approved_version"], 1, "{health:#}");
}

fn digests(effects: &Value) -> Vec<Value> {
    effects
        .as_array()
        .expect("effects")
        .iter()
        .filter(|e| e["op"] == "sink.digest")
        .cloned()
        .collect()
}

#[test]
fn a_schedule_flow_survives_a_catch_up_a_worker_kill_and_a_module_kill_with_each_write_once() {
    let dir = scratch("catch-up");

    // Install, approve, and sleep through five due times.
    let mut h = Harness::start(&dir, &["--routing-fake"]);
    install_and_approve(
        &mut h,
        "flow-hourly",
        json!({ "cron": "0 * * * *", "missed": "once" }),
    );
    h.ok(json!({ "cmd": "pump" }));
    assert_eq!(
        h.ok(json!({ "cmd": "runs" })),
        json!([]),
        "nothing due before 01:00"
    );
    h.ok(json!({ "cmd": "clock", "set_ms": T0 + 5 * HOUR + 10_000 }));
    h.quit();

    // The catch-up runs. Its worker is killed while the script waits on its
    // echo; on another worker it writes the digest, and the module is killed
    // after the sink applied the write and before its outcome is committed.
    let mut h = Harness::start(
        &dir,
        &[
            "--routing-fake",
            "--kill-worker-at",
            "CallCommitted { position: 1 }#1",
            "--kill-self-at",
            "HostAnswered { position: 2 }#1",
        ],
    );
    assert!(
        h.send(json!({ "cmd": "pump" })).is_none(),
        "the module died mid-run"
    );
    let status = h.wait();
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");

    // The restart recovers the run and finishes both fires.
    let mut h = Harness::start(&dir, &["--routing-fake"]);
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(runs.as_array().map(Vec::len), Some(2), "{runs:#}");
    assert_eq!(
        runs[0]["state"], "pending",
        "recovered from running: {runs:#}"
    );
    h.ok(json!({ "cmd": "pump" }));
    let runs = h.ok(json!({ "cmd": "runs" }));
    let ids: Vec<&str> = runs
        .as_array()
        .expect("runs")
        .iter()
        .map(|r| r["trigger_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ids,
        [
            "schedule:2026-05-01T04:00:00Z",
            "schedule:2026-05-01T05:00:00Z"
        ],
        "one catch-up fire for 01:00 to 04:00, then the on-time 05:00"
    );
    for run in runs.as_array().expect("runs") {
        assert_eq!(run["state"], "succeeded", "{runs:#}");
    }
    assert_eq!(runs[0]["broken"], 1, "the first run lost one worker");
    let first: Value =
        serde_json::from_str(runs[0]["result"].as_str().expect("result")).expect("json");
    let second: Value =
        serde_json::from_str(runs[1]["result"].as_str().expect("result")).expect("json");
    assert_eq!(
        first,
        json!({ "due": "2026-05-01T04:00:00Z", "before": 0, "missed": 4 })
    );
    assert_eq!(
        second,
        json!({ "due": "2026-05-01T05:00:00Z", "before": 1, "missed": 0 })
    );

    let effects = h.ok(json!({ "cmd": "effects" }));
    let writes = digests(&effects);
    assert_eq!(writes.len(), 2, "one digest write per fire: {effects:#}");
    assert_ne!(writes[0]["key"], writes[1]["key"]);
    assert!(
        writes[0]["args"]
            .as_str()
            .unwrap_or("")
            .contains("2026-05-01T04:00:00Z")
    );
    assert!(
        writes[1]["args"]
            .as_str()
            .unwrap_or("")
            .contains("2026-05-01T05:00:00Z")
    );
    // The write whose outcome the kill lost was sent again under its key
    // and applied once.
    assert_eq!(writes[0]["sends"], 2, "{effects:#}");
    assert_eq!(writes[1]["sends"], 1);
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_store_error_ends_the_process_non_zero_and_the_restart_recovers() {
    let dir = scratch("store-error");
    let mut h = Harness::start(&dir, &["--cut-store-at", "CallCommitted { position: 1 }#1"]);
    install_and_approve(&mut h, "flow-cut", json!({ "interval": "1h" }));
    h.ok(json!({ "cmd": "clock", "set_ms": T0 + HOUR + 10_000 }));
    // The store fails under the run's second call: the module exits rather
    // than serve on with the run stuck in running.
    let _ = h.send(json!({ "cmd": "pump" }));
    let status = h.wait();
    assert_eq!(status.code(), Some(EXIT_STORE_FAILURE), "{status:?}");

    let mut h = Harness::start(&dir, &[]);
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(
        runs[0]["state"], "pending",
        "recovery put the run back: {runs:#}"
    );
    h.ok(json!({ "cmd": "pump" }));
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(runs.as_array().map(Vec::len), Some(1));
    assert_eq!(runs[0]["state"], "succeeded", "{runs:#}");
    let effects = h.ok(json!({ "cmd": "effects" }));
    assert_eq!(digests(&effects).len(), 1, "{effects:#}");
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_broca_llm_suspends_survives_module_kill_and_settles_after_restart() {
    let dir = scratch("broca-restart");
    let mut h = Harness::start(&dir, &["--broca"]);
    let manifest = json!({
        "id": "flow-model", "version": 1, "purpose": "Obtain one model answer.",
        "trigger": {"schedule": {"cron": "0 * * * *", "missed": "once"}},
        "llm": {"iq": 0, "token_cap": {"tokens": 10000, "window": "1h"}, "max_output": 32}
    })
    .to_string();
    let installed = h.ok(json!({"cmd":"op","as":{"agent":"SYNAPSE"},"method":"flow.install","params":{"manifest":manifest,"script":"return await llm({prompt:'hello',max_output:999});"}}));
    h.ok(json!({"cmd":"decide","card_id":installed["card_id"],"approve":true,"by":"operator"}));
    h.ok(json!({"cmd":"clock","set_ms":T0+HOUR}));
    h.ok(json!({"cmd":"pump"}));
    let before = h.ok(json!({"cmd":"runs"}));
    assert_eq!(before[0]["state"], "suspended", "{before:#}");
    let sends = h.ok(json!({"cmd":"broca_sends"}));
    assert_eq!(sends.as_array().unwrap().len(), 1);
    assert_eq!(sends[0]["params"]["tools"], json!([]));
    assert_eq!(sends[0]["params"]["generation"]["max_output_tokens"], 32);
    h.child.kill().expect("kill the module");
    let status = h.wait();
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let mut h = Harness::start(&dir, &["--broca"]);
    assert_eq!(h.ok(json!({"cmd":"runs"}))[0]["state"], "suspended");
    h.ok(json!({"cmd":"broca_finish","text":"after restart","usage":{"input_tokens":7,"cache_write_tokens":2,"output_tokens":5,"cached_input_tokens":100,"reasoning_tokens":3}}));
    h.ok(json!({"cmd":"pump"}));
    let runs = h.ok(json!({"cmd":"runs"}));
    assert_eq!(runs[0]["state"], "succeeded", "{runs:#}");
    let result: Value = serde_json::from_str(runs[0]["result"].as_str().unwrap()).unwrap();
    assert_eq!(result, json!({"text":"after restart"}));
    assert_eq!(
        h.ok(json!({"cmd":"broca_sends"})).as_array().unwrap().len(),
        1
    );
    h.quit();
    let c = rusqlite::Connection::open(dir.join("basal.db")).unwrap();
    let row: (String,i64,i64,i64,i64) = c.query_row("SELECT state,input_tokens,cache_write_tokens,output_tokens,cached_input_tokens FROM token_ledger",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(row, ("settled".into(), 7, 2, 5, 100));
    let snapshot: String = c
        .query_row("SELECT snapshot FROM broca_calls", [], |r| r.get(0))
        .unwrap();
    let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(snapshot["acknowledged"], true);
    assert!(snapshot["cursor"].is_object());
    assert_eq!(snapshot["text"], "after restart");
}
