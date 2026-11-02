//! Opt-in accounting benchmark, not a throughput or durability relaxation test.
//!
//! Run on Linux with:
//! `cargo test -p basal-testkit --test write_amplification --locked -- --ignored --nocapture --test-threads=1`
//! Set BASAL_WRITE_BENCH_OUT to save the complete JSON outside the build tree.

#![cfg(target_os = "linux")]

#[path = "write_amplification/probe.rs"]
mod probe;

use std::sync::Arc;

use basal_core::{ActivationEnd, Clock, Config, Durability, InstallGate, NoHooks, Runtime, Store};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, MockCatalog,
    TransportError, mock::MockHost,
};
use basal_proto::{CallKind, JsonText, Primitive};
use basal_testkit::harness::approve_spec;
use basal_testkit::{ProcessSource, worker_binary};
use serde_json::{Value, json};

use probe::{Io, Probe};

const N: usize = 500;
const INSTANCES: usize = 26;
const T0: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const SCRIPT: &str = r#"
const previous = await kv.get('cpu');
const sample = await ops.call('mock', 'echo', {
    cpu: 0.12, module: 'watched-module', detail: 'sample '.repeat(145)
});
const receipt = await sink.digest('ALF', {
    title: 'Module CPU watch', body: 'CPU within expected range. '.repeat(40)
}, 'piggyback');
if (receipt.disposition !== 'stored') throw new Error('digest was not stored');
await kv.set('cpu', { cpu: sample.cpu, detail: sample.detail });
return { ok: true, seen: previous !== null };
"#;

/// External services are in memory; digest returns a small stored receipt,
/// rather than MockHost's generic mutation reply that echoes the whole body.
struct SmallHost(MockHost);

impl Host for SmallHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.0.classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let dispatched = self.0.dispatch(request)?;
        if request.kind == CallKind::Primitive(Primitive::SinkDigest) {
            Ok(Dispatched::Completed(HostOutcome::fulfilled(JsonText::new(
                json!({"disposition": "stored", "fire_id": "wf_write_bench", "replayed": false}).to_string()
            ).unwrap())))
        } else {
            Ok(dispatched)
        }
    }
    fn now_ms(&self) -> f64 {
        self.0.now_ms()
    }
    fn random(&self) -> f64 {
        self.0.random()
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.0.attach(sink);
    }
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .read(
            |conn| Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?),
        )
        .unwrap()
}

fn checkpoint(probe: &Probe<'_>, store: &Store) {
    store
        .read(|conn| {
            probe.checkpoint(conn);
            Ok(())
        })
        .unwrap();
}

#[test]
#[ignore = "500 durable scheduled runs plus their retention tail; Linux I/O measurement"]
fn scheduled_small_run_write_amplification() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join(format!(
        "target/scratch/write-amplification-{}",
        std::process::id()
    ));
    assert!(!dir.exists(), "benchmark requires a fresh store");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basal.db");
    let store = Arc::new(Store::open(&path, Durability::default()).unwrap());
    let clock = Clock::manual(T0);
    let source = Arc::new(ProcessSource::new(worker_binary()));
    let rt = Runtime::new(
        store.clone(),
        Arc::new(SmallHost(MockHost::new())),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        Some(source),
        Config {
            clock: clock.clone(),
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    );
    // Stagger 26 one-minute schedules. Each measured tick has exactly one fire,
    // avoiding an unmeasured pending tail when 500 is not divisible by 26.
    for instance in 0..INSTANCES {
        clock.set(T0 + instance as i64 * 2_000);
        approve_spec(
            &rt,
            SCRIPT,
            &json!({
                "id": format!("cpu-watch-{instance:02}"), "version": 1,
                "purpose": "Measure scheduled small-run storage writes.",
                "trigger": { "schedule": { "interval": "1m" } },
                "ops": [{"module": "mock", "op": "echo"}],
                "sinks": [{"agent": "ALF", "digest_max": "piggyback"}]
            }),
            "unused",
            JsonText::null(),
        )
        .unwrap();
    }
    let pragmas = store.pragmas().unwrap();
    assert_eq!(pragmas.synchronous, 2);
    assert!(pragmas.fullfsync && pragmas.checkpoint_fullfsync && pragmas.journal_mode_wal);
    let settings = store
        .read(|conn| {
            let value = |name: &str| -> basal_core::Result<i64> {
                Ok(conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
            };
            let schema = conn
                .prepare(
                    "SELECT name,tbl_name,sql FROM sqlite_schema WHERE type='index' ORDER BY name",
                )?
                .query_map([], |row| {
                    Ok(json!({"name": row.get::<_, String>(0)?,
                "table": row.get::<_, String>(1)?, "sql": row.get::<_, Option<String>>(2)?}))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(
                json!({"sqlite": rusqlite::version(), "page_size": value("page_size")?,
            "wal_autocheckpoint": value("wal_autocheckpoint")?,
            "journal_size_limit": value("journal_size_limit")?,
            "cache_size": value("cache_size")?, "synchronous": pragmas.synchronous,
            "fullfsync": pragmas.fullfsync, "checkpoint_fullfsync": pragmas.checkpoint_fullfsync,
            "indexes": schema}),
            )
        })
        .unwrap();
    assert_eq!(settings["page_size"], 4096);
    store
        .read(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })
        .unwrap();
    let before = Io::read();
    let probe = Probe::start(
        &store,
        path.with_extension("db-wal"),
        settings["wal_autocheckpoint"].as_i64().unwrap() as i32,
    );
    let scheduler = rt.scheduler().unwrap();
    for run in 0..N {
        let at = T0 + 60_000 + (run / INSTANCES) as i64 * 60_000 + (run % INSTANCES) as i64 * 2_000;
        clock.set(at);
        let tick = scheduler.tick().unwrap();
        assert_eq!(tick.admitted.len(), 1, "one scheduled fire at {at}");
        let id = tick.admitted[0].admission.run_id().unwrap();
        assert!(matches!(
            rt.resume(id).unwrap(),
            ActivationEnd::Succeeded { .. }
        ));
        assert_eq!(rt.calls(id).unwrap().len(), 4);
    }
    rt.quiesce();
    assert_eq!(count(&store, "runs"), N as i64);
    assert_eq!(count(&store, "journal"), (4 * N) as i64);
    assert_eq!(count(&store, "call_audit"), (4 * N) as i64);
    assert_eq!(count(&store, "schedule_fires"), 0);
    let payload: i64 = store
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT SUM(args_bytes+request_bytes+COALESCE(value_bytes,0)) FROM journal",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    checkpoint(&probe, &store);
    let after_runs = Io::read();

    let last_end = clock.now_ms();
    probe.phase("payload_retention");
    clock.set(last_end + DAY);
    let mut payloads = 0;
    loop {
        let batch = rt.prune_payloads(clock.now_ms(), DAY).unwrap();
        payloads += batch;
        if batch == 0 {
            break;
        }
    }
    assert_eq!(payloads, N);
    assert_eq!(count(&store, "journal_payloads"), 0);
    assert_eq!(count(&store, "journal"), (4 * N) as i64);
    checkpoint(&probe, &store);
    let after_payloads = Io::read();

    probe.phase("history_retention");
    clock.set(last_end + 90 * DAY);
    let mut histories = 0;
    loop {
        let batch = rt.prune(clock.now_ms(), 90 * DAY).unwrap().pruned.len();
        histories += batch;
        if batch == 0 {
            break;
        }
    }
    assert_eq!(histories, N);
    // Use production maintenance for the auxiliary table batches too. Its
    // backlog cadence is one second; no retention horizon is shortened.
    for _ in 0..32 {
        rt.enforce_deadlines().unwrap();
        if count(&store, "call_audit") == 0 && count(&store, "rate_windows") == 0 {
            break;
        }
        clock.set(clock.now_ms() + 1_000);
    }
    assert_eq!(count(&store, "runs"), 0);
    assert_eq!(count(&store, "journal"), 0);
    assert_eq!(count(&store, "call_audit"), 0);
    assert_eq!(count(&store, "rate_windows"), 0);
    checkpoint(&probe, &store);
    let after_history = Io::read();
    let accounting = probe.report();
    for kind in accounting["kinds"].as_array().unwrap() {
        let sql = kind["sql"].as_array().unwrap();
        if sql.len() == 1
            && sql[0]
                .as_str()
                .unwrap()
                .starts_with("CREATE TABLE IF NOT EXISTS cortexkit_fence")
        {
            assert_eq!(
                kind["frames"], 0,
                "read-only fenced transactions must not inherit another commit's frames"
            );
            assert_eq!(kind["write_bytes"], 0);
        }
    }
    let attributed: u64 = accounting["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["write_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        attributed,
        after_history.writes - before.writes,
        "transaction and checkpoint attribution must cover the entire interval"
    );
    drop(probe);
    drop(scheduler);
    drop(rt);
    drop(store);
    let after_close = Io::read();
    let report: Value = json!({
        "runs": N, "instances": INSTANCES, "calls_per_run": 4,
        "settings": settings, "logical_payload_bytes": payload,
        "runs_io": after_runs.delta(before),
        "payload_retention_io": after_payloads.delta(after_runs),
        "history_retention_io": after_history.delta(after_payloads),
        "close_io": after_close.delta(after_history),
        "total_io": after_close.delta(before), "accounting": accounting,
    });
    if let Some(out) = std::env::var_os("BASAL_WRITE_BENCH_OUT") {
        let out = root.join(out);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!("{}", serde_json::to_string(&report).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}
