//! Dry runs: nothing executes in capture mode; a captured call the script
//! uses makes the rest of the trace partial; live mode runs only `query`
//! ops and only for the operator; each dry run has its own scratch store
//! and leaves the flow's real `kv` and runs untouched.

mod common;

use basal_core::RunState;
use basal_module::caller::Caller;
use common::{
    Fixture, Options, admit, agent, events_manifest, fixture, install, install_approved,
    schedule_manifest,
};
use serde_json::{Value, json};

fn dry_run(f: &Fixture, caller: &Caller, params: Value) -> Result<Value, String> {
    f.module
        .handle(caller, "flow.dry_run", params)
        .map(|v| v["summary"].clone())
        .map_err(|e| format!("{}: {}", e.code, e.message))
}

fn calls(run: &Value) -> Vec<Value> {
    run["calls"].as_array().cloned().unwrap_or_default()
}

#[test]
fn capture_mode_executes_no_host_call_and_replays_the_schedule_window() {
    let f = fixture("dry-capture", Options::default());
    let script = "const r = await ops.call('mock', 'echo', { due: trigger.due }).catch((e) => e.data.code);\n\
                  await sink.digest('SYNAPSE', { title: 'fire' }).catch(() => null);\n\
                  await sink.digest('SYNAPSE', { title: 'loud' }, 'wake').catch(() => null);\n\
                  return r;";
    // Hourly, so a 24-hour window holds 24 due times; ten are replayed.
    let reply = install(
        &f,
        &agent("SYNAPSE"),
        script,
        &schedule_manifest("flow-hourly", json!({ "interval": "1h" })),
    );
    let summary = &reply["dry_run"];
    assert_eq!(summary["mode"], "capture");
    assert_eq!(summary["window"]["kind"], "schedule");
    assert_eq!(summary["window"]["due_times_in_window"], 24);
    assert_eq!(summary["window"]["replayed"], 10);
    assert_eq!(summary["window"]["capped"], true);
    assert_eq!(summary["window"]["requested"]["to"], "2026-05-01T00:00:00Z");
    assert_eq!(summary["window"]["covered"]["from"], "2026-04-30T15:00:00Z");
    let runs = summary["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 10);
    for run in runs {
        assert_eq!(run["state"], "succeeded", "{run:#}");
        assert_eq!(
            run["result"], "captured",
            "the script saw a rejection, not a result"
        );
        let c = calls(run);
        assert_eq!(c[0]["op"], "echo");
        assert_eq!(c[0]["action"], "captured");
        assert_eq!(c[0]["outcome"]["rejected"], "captured");
        assert_eq!(c[1]["kind"], "sink.digest");
        assert_eq!(c[1]["action"], "captured");
        // The write names no action, which core reads as Wake; the cap for
        // SYNAPSE is piggyback.
        assert_eq!(c[1]["sink"]["requested"], "wake");
        assert_eq!(c[1]["sink"]["effective"], "piggyback");
        // Asking for more than the cap is refused before anything else.
        assert_eq!(c[2]["action"], "refused");
        assert_eq!(c[2]["outcome"]["rejected"], "denied");
    }
    assert_eq!(runs[0]["trigger"]["due"], "2026-04-30T15:00:00Z");
    assert_eq!(f.mock.total_sends(), 0, "nothing reached a host");

    // Asked for explicitly, by the owner, still nothing executes.
    let summary = dry_run(
        &f,
        &agent("SYNAPSE"),
        json!({ "flow_id": "flow-hourly", "window": "3h" }),
    )
    .expect("dry run");
    assert_eq!(summary["window"]["replayed"], 3);
    assert_eq!(f.mock.total_sends(), 0, "nothing reached a host");
}

#[test]
fn a_captured_call_the_script_uses_marks_the_trace_partial() {
    let f = fixture("dry-partial", Options::default());
    let synthetic = json!({ "trigger": { "kind": "synthetic" } });
    let run_of = |script: &str, id: &str| -> Value {
        install(&f, &agent("SYNAPSE"), script, &events_manifest(id));
        let mut params = synthetic.clone();
        params["flow_id"] = json!(id);
        dry_run(&f, &agent("SYNAPSE"), params).expect("dry run")
    };

    // The script reads the captured call's answer and acts on it.
    let used = run_of(
        "let n = 0;\n\
         try { n = (await ops.call('mock', 'echo', { n: 1 })).n; } catch (e) { n = -1; }\n\
         await sink.digest('SYNAPSE', { title: String(n) }, 'piggyback').catch(() => null);\n\
         return n;",
        "flow-uses",
    );
    assert_eq!(used["partial"], true, "{used:#}");
    let run = &used["runs"][0];
    assert_eq!(run["partial"], true);
    let c = calls(run);
    assert_eq!(
        c[0]["partial"], false,
        "the captured call itself is listed whole"
    );
    assert_eq!(
        c[1]["partial"], true,
        "the write after it was decided on a fiction"
    );

    // A script that stops on the captured rejection is partial too: the real
    // run would have gone on.
    let stopped = run_of(
        "const r = await ops.call('mock', 'echo', { n: 1 }); return r.n;",
        "flow-stops",
    );
    assert_eq!(stopped["partial"], true);
    assert_eq!(stopped["runs"][0]["state"], "failed");

    // A script that never waits for the call ignores it: the rest of its
    // trace is whole.
    let ignored = run_of(
        "ops.call('mock', 'echo', { n: 1 }).catch(() => null);\n\
         await sink.digest('SYNAPSE', { title: 'x' }, 'piggyback').catch(() => null);\n\
         return 1;",
        "flow-ignores",
    );
    assert_eq!(ignored["partial"], false, "{ignored:#}");
    assert!(
        calls(&ignored["runs"][0])
            .iter()
            .all(|c| c["partial"] == false)
    );
    assert_eq!(f.mock.total_sends(), 0);
}

#[test]
fn live_mode_runs_only_query_ops_and_only_for_the_operator() {
    let f = fixture("dry-live", Options::default());
    let script = "const a = await ops.call('mock', 'echo', { n: 1 });\n\
                  const b = await ops.call('mock', 'send', { n: 2 }).catch((e) => e.data.code);\n\
                  const s = await sink.digest('SYNAPSE', { title: 't' }, 'piggyback').catch((e) => e.data.code);\n\
                  return { a: a.n, b, s };";
    install(&f, &agent("SYNAPSE"), script, &events_manifest("flow-live"));
    let params =
        json!({ "flow_id": "flow-live", "mode": "live", "trigger": { "kind": "synthetic" } });

    for caller in [agent("SYNAPSE"), agent("ALF"), Caller::Core] {
        let refused = dry_run(&f, &caller, params.clone());
        assert!(
            matches!(&refused, Err(e) if e.starts_with("not_permitted")),
            "{}: {refused:?}",
            caller.label()
        );
    }
    assert_eq!(f.mock.total_sends(), 0);

    let summary = dry_run(&f, &Caller::Operator, params).expect("live dry run");
    let run = &summary["runs"][0];
    assert_eq!(
        run["result"],
        json!({ "a": 1, "b": "captured", "s": "captured" }),
        "{run:#}"
    );
    let c = calls(run);
    assert_eq!(c[0]["action"], "live");
    assert_eq!(c[0]["outcome"]["fulfilled"], true);
    assert_eq!(c[1]["action"], "captured", "a mutate op is never run live");
    assert_eq!(c[2]["action"], "captured", "a sink write is never run live");
    assert_eq!(
        f.mock.total_sends(),
        1,
        "exactly the query reached the host"
    );
    assert!(f.mock.effects().is_empty());
}

fn real_kv(f: &Fixture, flow: &str) -> Vec<(String, String, i64)> {
    f.module
        .rt
        .store()
        .read(|c| {
            let mut stmt =
                c.prepare("SELECT key, value, revision FROM kv WHERE flow_id = ?1 ORDER BY key")?;
            let rows = stmt
                .query_map([flow], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .expect("kv")
}

fn real_runs(f: &Fixture) -> i64 {
    f.module
        .rt
        .store()
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?))
        .expect("runs")
}

#[test]
fn each_dry_run_has_its_own_scratch_store_and_the_real_kv_and_runs_are_untouched() {
    let f = fixture("dry-scratch", Options::default());
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        "const n = ((await kv.get('count')) ?? 0) + 1;\n\
         await kv.set('count', n);\n\
         return n;",
        &events_manifest("flow-count"),
    );
    let run = admit(&f, &flow, "real-1");
    f.module.engine.run_until_idle(50).expect("idle");
    assert_eq!(
        f.module.rt.run(&run).expect("run").state,
        RunState::Succeeded
    );
    let kv_before = real_kv(&f, &flow);
    assert_eq!(kv_before.len(), 1);
    assert_eq!(kv_before[0].1, "1");
    let runs_before = real_runs(&f);

    for _ in 0..2 {
        let summary = dry_run(
            &f,
            &Caller::Operator,
            json!({ "flow_id": flow, "trigger": { "kind": "synthetic" } }),
        )
        .expect("dry run");
        // Each dry run starts from an empty kv of its own.
        assert_eq!(summary["runs"][0]["result"], 1, "{summary:#}");
        assert_eq!(calls(&summary["runs"][0])[1]["action"], "local");
    }
    assert_eq!(real_kv(&f, &flow), kv_before, "the flow's kv is untouched");
    assert_eq!(
        real_runs(&f),
        runs_before,
        "no run was added to the real store"
    );
    let leftovers: Vec<_> = std::fs::read_dir(f.dir.join("dry-run"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "scratch stores are removed: {leftovers:?}"
    );
}
