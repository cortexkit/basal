//! Opt-in accounting benchmark, not a throughput or durability relaxation test.
//!
//! Run on Linux with:
//! `cargo test -p basal-testkit --test write_amplification --locked -- --ignored --nocapture --test-threads=1`
//! Set BASAL_WRITE_BENCH_OUT to save the complete JSON outside the build tree.
//! Set BASAL_WRITE_BENCH_PAGE_SIZE to create the benchmark database with a different
//! SQLite page size; production Store::open defaults are unchanged.

#![cfg(target_os = "linux")]

#[path = "write_amplification/probe.rs"]
mod probe;

use std::collections::BTreeMap;
use std::sync::Arc;

use basal_core::{ActivationEnd, Clock, Config, Durability, InstallGate, NoHooks, Runtime, Store};
use basal_core::{
    run_driver::Budgets,
    run_host::RecoveredHosts,
    run_log::{self, Lsn, RunLog, SqliteRegistry, measurement::Measurement},
    run_records::Record,
    run_store_sql::{self as sql, EpochLsn, GcFloor, Meta},
};
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
const READ_ONLY_SCRIPT: &str = r#"
return await ops.call('mock', 'echo', {
    cpu: 0.12, module: 'watched-module', detail: 'sample '.repeat(145)
});
"#;
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

fn open_benchmark_store(path: &std::path::Path, page_size: Option<u32>) -> Store {
    if let Some(page_size) = page_size {
        assert!(page_size.is_power_of_two() && (512..=65_536).contains(&page_size));
        assert!(!path.exists(), "page size must be set on a fresh database");
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.pragma_update(None, "page_size", page_size).unwrap();
        // Persist the empty database header before Store::open enables WAL and
        // creates tables. SQLite cannot change page size while in WAL mode.
        conn.execute_batch("VACUUM").unwrap();
    }
    Store::open(path, Durability::default()).unwrap()
}

#[test]
#[ignore = "500 durable scheduled runs plus their retention tail; Linux I/O measurement"]
fn scheduled_small_run_write_amplification() {
    measure(SCRIPT, 4, "four-call", 979_705.856, 200_000, false);
}

#[test]
#[ignore = "500 durable scheduled runs plus their retention tail; Linux I/O measurement"]
fn scheduled_read_only_write_amplification() {
    measure(
        READ_ONLY_SCRIPT,
        1,
        "read-only",
        596_058.112,
        100_000,
        false,
    );
}

#[test]
#[ignore = "20 five-minute ticks of 26 durable scheduled runs; Linux I/O measurement"]
fn batched_small_run_write_amplification() {
    measure(SCRIPT, 4, "four-call-batch", 979_705.856, 200_000, true);
}

#[test]
#[ignore = "20 five-minute ticks of 26 durable scheduled runs; Linux I/O measurement"]
fn batched_read_only_write_amplification() {
    measure(
        READ_ONLY_SCRIPT,
        1,
        "read-only-batch",
        596_058.112,
        100_000,
        true,
    );
}

fn measure(
    script: &str,
    calls: usize,
    label: &str,
    before_bytes_per_run: f64,
    limit: u64,
    batch: bool,
) {
    let n = if batch { 20 * INSTANCES } else { N };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join(format!(
        "target/scratch/write-amplification-{label}-{}",
        std::process::id()
    ));
    assert!(!dir.exists(), "benchmark requires a fresh store");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basal.db");
    let page_size = std::env::var("BASAL_WRITE_BENCH_PAGE_SIZE")
        .ok()
        .map(|value| value.parse::<u32>().expect("SQLite page size in bytes"));
    let store = Arc::new(open_benchmark_store(&path, page_size));
    let clock = Clock::manual(T0);
    let source = Arc::new(ProcessSource::new(worker_binary()));
    let epoch = EpochLsn {
        log_epoch: 1,
        lsn: Lsn::default(),
    };
    store
        .write(|tx| {
            sql::create_test_schema(
                tx,
                &Meta {
                    store_id: store.store_id().into(),
                    log_epoch: 1,
                    log_hwm: epoch,
                    commit_seq: 0,
                    gc_floor: GcFloor {
                        log_epoch: 1,
                        first_retained: 0,
                        predecessor: None,
                        synced_lsn: Lsn::default(),
                        live_starts: BTreeMap::new(),
                    },
                    migration_phase: "test".into(),
                },
            )
        })
        .unwrap();
    // Schema and flow installation are setup. Log
    // creation is sampled separately so its files, registry and syncs count too.
    checkpoint_unmeasured(&store);
    let creation_probe = Probe::start(&store, path.with_extension("db-wal"), 1000);
    let creation_before = creation_probe.initial_io();
    let measurement = Measurement::default();
    let log = Arc::new(
        RunLog::<Record>::create_measured(
            run_log::Config {
                directory: dir.join("run-log"),
                identity: run_log::Identity {
                    store_id: store.store_id().as_bytes().try_into().unwrap(),
                    log_epoch: 1,
                },
                lease_epoch: store.lease_epoch(),
                target_size: run_log::Config::TARGET_SIZE,
                hooks: Arc::new(NoHooks),
            },
            Arc::new(SqliteRegistry::new(store.clone(), 1)),
            &measurement,
        )
        .unwrap(),
    );
    checkpoint(&creation_probe, &store);
    let creation_after = Io::read();
    let creation_accounting = creation_probe.report();
    let creation_attributed: u64 = creation_accounting["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["write_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        creation_attributed
            + creation_accounting["sql_event_gap_write_bytes"]
                .as_u64()
                .unwrap(),
        creation_after.writes - creation_before.writes
    );
    drop(creation_probe);
    let creation_log = measurement.stats();
    let hosts = Arc::new(
        RecoveredHosts::recover(
            store.clone(),
            log.clone(),
            Arc::new(log.watermark()),
            &[],
            Budgets {
                dispatch_window_ms: 60_000,
                token_window_ms: DAY,
                dispatches: 100_000,
                tokens: u64::MAX,
                cost_micros: u64::MAX,
                cost_window: None,
            },
            Arc::new(NoHooks),
            clock.clone(),
        )
        .unwrap(),
    );
    let rt = Runtime::new_staged(
        hosts,
        Arc::new(SmallHost(MockHost::new())),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        Some(source),
        Config {
            clock: clock.clone(),
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    )
    .unwrap();
    // Stagger 26 one-minute schedules. Each measured tick has exactly one fire,
    // avoiding an unmeasured pending tail when 500 is not divisible by 26.
    for instance in 0..INSTANCES {
        clock.set(T0 + if batch { 0 } else { instance as i64 * 2_000 });
        approve_spec(
            &rt,
            script,
            &json!({
                "id": format!("cpu-watch-{instance:02}"), "version": 1,
                "purpose": "Measure scheduled small-run storage writes.",
                "trigger": { "schedule": { "interval": if batch { "5m" } else { "1m" } } },
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
    assert_eq!(settings["page_size"], page_size.unwrap_or(1024));
    store
        .read(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })
        .unwrap();
    let probe = Probe::start(
        &store,
        path.with_extension("db-wal"),
        settings["wal_autocheckpoint"].as_i64().unwrap() as i32,
    );
    let before = probe.initial_io();
    let scheduler = rt.scheduler().unwrap();
    for run in 0..if batch { 20 } else { N } {
        let at = if batch {
            T0 + (run as i64 + 1) * 300_000
        } else {
            T0 + 60_000 + (run / INSTANCES) as i64 * 60_000 + (run % INSTANCES) as i64 * 2_000
        };
        clock.set(at);
        let tick = scheduler.tick().unwrap();
        assert_eq!(
            tick.admitted.len(),
            if batch { INSTANCES } else { 1 },
            "scheduled fires at {at}"
        );
        for admitted in &tick.admitted {
            let id = admitted.admission.run_id().unwrap();
            let end = rt.resume(id).unwrap();
            assert!(
                matches!(end, ActivationEnd::Succeeded { .. }),
                "{label} tick={run} run={id}: {end:?}; persisted={:?}",
                rt.run(id)
            );
            let summary = store.read(|c| sql::history(c, id)).unwrap().unwrap();
            assert_eq!(summary.call_counts["outcome"]["fulfilled"], calls);
            assert_eq!(summary.state, basal_core::RunState::Succeeded);
            assert_eq!(summary.broken, 0);
            assert_eq!(summary.activation_count, 1);
            assert!(rt.calls(id).unwrap().is_empty(), "retain_detail stays off");
        }
    }
    rt.quiesce();
    assert_eq!(count(&store, "run_history"), n as i64);
    assert_eq!(count(&store, "run_identities"), n as i64);
    assert_eq!(count(&store, "live_runs"), 0);
    assert_eq!(count(&store, "run_calls"), 0);
    assert_eq!(count(&store, "schedule_fires"), 0);
    checkpoint(&probe, &store);
    let after_runs = Io::read();
    let runs_log = measurement.stats();

    let last_end = clock.now_ms();
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
    assert_eq!(histories, n);
    // Use production maintenance for the auxiliary table batches too. Its
    // backlog cadence is one second; no retention horizon is shortened.
    for _ in 0..32 {
        rt.enforce_deadlines().unwrap();
        if count(&store, "rate_windows") == 0 {
            break;
        }
        clock.set(clock.now_ms() + 1_000);
    }
    assert_eq!(count(&store, "run_history"), 0);
    assert_eq!(count(&store, "run_identities"), 0);
    assert_eq!(count(&store, "run_calls"), 0);
    assert_eq!(count(&store, "rate_windows"), 0);
    checkpoint(&probe, &store);
    let after_history = Io::read();
    let history_log = measurement.stats();
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
        }
    }
    let attributed: u64 = accounting["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["write_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        attributed + accounting["sql_event_gap_write_bytes"].as_u64().unwrap(),
        after_history.writes - before.writes,
        "transaction and checkpoint attribution must cover the entire interval"
    );
    drop(probe);
    drop(scheduler);
    drop(rt);
    drop(log);
    drop(store);
    let after_close = Io::read();
    let stats = measurement.stats();
    let creation_writes = creation_after.writes - creation_before.writes;
    let total_writes = after_close.writes - before.writes + creation_writes;
    let sqlite_writes: u64 = [&accounting, &creation_accounting]
        .iter()
        .flat_map(|a| a["kinds"].as_array().unwrap())
        .map(|kind| kind["sqlite_write_bytes"].as_u64().unwrap())
        .sum();
    let outside_sql_or_log = total_writes
        .checked_sub(sqlite_writes + stats.write_bytes)
        .unwrap();
    let after_bytes_per_run = total_writes as f64 / n as f64;
    let wal_frames: u64 = accounting["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["frames"].as_u64().unwrap())
        .sum();
    let wal_frame_bytes = wal_frames * (settings["page_size"].as_u64().unwrap() + 24);
    let report: Value = json!({
        "runs": n, "instances": INSTANCES, "calls_per_run": calls,
        "profile": if batch { "26 fires per five-minute tick, 20 ticks" } else { "500 sequential one-fire ticks" },
        "before_profile": "500 sequential one-fire ticks on legacy path, including 24h payload pruning",
        "settings": settings, "retain_detail": false,
        "before_bytes_per_run": before_bytes_per_run,
        "before_reference": if calls == 1 { "s11b-read-only-before.json" } else { "s11b-four-call-before.json" },
        "after_bytes_per_run": after_bytes_per_run, "limit_bytes_per_run": limit,
        "log_creation_io": creation_after.delta(creation_before),
        "log_creation_sql": creation_accounting,
        "runs_io": after_runs.delta(before),
        "history_retention_io": after_history.delta(after_runs),
        "close_io": after_close.delta(after_history),
        "total_write_bytes": total_writes,
        "outside_sql_or_log_write_bytes": outside_sql_or_log,
        "sqlite": {"write_bytes": sqlite_writes, "wal_frames": wal_frames,
            "wal_format_bytes": wal_frame_bytes},
        "run_log": {"write_bytes": stats.write_bytes, "append_bytes": stats.append_bytes,
            "file_syncs": stats.file_syncs, "directory_syncs": stats.directory_syncs,
            "segment_creates": stats.segment_creates, "rotations": stats.rotations,
            "creation": log_delta(creation_log, Default::default()),
            "runs": log_delta(runs_log, creation_log),
            "retention": log_delta(history_log, runs_log),
            "close": log_delta(stats, history_log)},
        // The SQL probe samples process-wide I/O between SQLite events. Its
        // per-kind allocation bytes can include intervening log writes; only
        // the top-level SQLite/log split is a physical-byte attribution.
        "process_io_between_sql_events": accounting,
    });
    if let Some(out) = std::env::var_os("BASAL_WRITE_BENCH_OUT") {
        let out = root.join(out);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    if let Some(out) = std::env::var_os("BASAL_WRITE_BENCH_OUT_DIR") {
        let out = root.join(out).join(format!("{label}-after.json"));
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!(
        "{label}: {n} runs, before(sequential)={before_bytes_per_run:.3}, after={after_bytes_per_run:.3} bytes/run, limit={limit}; SQLite={:.3}, run-log={:.3}, outside-SQL/log={:.3} bytes/run; log appends={}, file syncs={}, directory syncs={}, creates={}, rotations={}",
        sqlite_writes as f64 / n as f64,
        stats.write_bytes as f64 / n as f64,
        outside_sql_or_log as f64 / n as f64,
        stats.append_bytes,
        stats.file_syncs,
        stats.directory_syncs,
        stats.segment_creates,
        stats.rotations
    );
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(
        outside_sql_or_log, 0,
        "independently measured SQLite and run-log allocation bytes must cover the full lifetime"
    );
    assert!(
        total_writes < limit * n as u64,
        "{label}: {after_bytes_per_run} bytes/run >= {limit}"
    );
}

fn log_delta(after: run_log::measurement::Stats, before: run_log::measurement::Stats) -> Value {
    json!({"append_bytes": after.append_bytes - before.append_bytes,
        "write_bytes": after.write_bytes - before.write_bytes,
        "file_syncs": after.file_syncs - before.file_syncs,
        "directory_syncs": after.directory_syncs - before.directory_syncs,
        "segment_creates": after.segment_creates - before.segment_creates,
        "rotations": after.rotations - before.rotations})
}

fn checkpoint_unmeasured(store: &Store) {
    store
        .read(|c| {
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn benchmark_store_and_probe_use_requested_page_size() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../target/scratch/write-page-size-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for page_size in [1024, 2048, 4096, 8192] {
        let path = dir.join(format!("{page_size}.db"));
        let store = open_benchmark_store(&path, Some(page_size));
        let actual: u32 = store
            .read(|conn| Ok(conn.query_row("PRAGMA page_size", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(actual, page_size);
        checkpoint_unmeasured(&store);
        let probe = Probe::start(&store, path.with_extension("db-wal"), 1000);
        store
            .write(|tx| {
                tx.execute_batch("CREATE TABLE page_size_probe(value); INSERT INTO page_size_probe VALUES('payload');")?;
                Ok(())
            })
            .unwrap();
        let report = probe.report();
        let transaction = &report["transactions"][0];
        let frames = transaction["wal_frames"].as_u64().unwrap();
        assert!(frames > 0);
        assert_eq!(
            transaction["wal_frame_bytes"],
            frames * (u64::from(page_size) + 24)
        );
        assert!(
            transaction["pages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|page| page["object"] == "page_size_probe")
        );
        let kind = report["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|kind| kind["frames"] == frames)
            .unwrap();
        assert_eq!(
            kind["wal_frame_bytes"],
            frames * (u64::from(page_size) + 24)
        );
        drop(probe);
        drop(store);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn probe_maps_transaction_pages_to_tables_and_indexes_and_reconciles_gaps() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../target/scratch/write-probe-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("core.db"), Durability::default()).unwrap();
    let page_size = store
        .read(|conn| Ok(conn.pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))?))
        .unwrap();
    assert_eq!(page_size, 1024);
    checkpoint_unmeasured(&store);
    let probe = Probe::start(&store, dir.join("core.db-wal"), 1000);
    let before = probe.initial_io();
    store.write(|tx| {
        tx.execute_batch("CREATE TABLE probe_rows(id INTEGER PRIMARY KEY, value TEXT); CREATE INDEX probe_values ON probe_rows(value); INSERT INTO probe_rows VALUES(1,'payload');")?;
        Ok(())
    }).unwrap();
    // A write outside SQL must appear as an explicit event gap, not be lost at
    // the checkpoint boundary. Sync so the OS allocation counter has advanced.
    let file = std::fs::File::create(dir.join("outside-sql")).unwrap();
    use std::io::Write;
    (&file).write_all(&[7; 8192]).unwrap();
    file.sync_all().unwrap();
    checkpoint(&probe, &store);
    let after = Io::read();
    let report = probe.report();
    let attributed: u64 = report["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["write_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        attributed + report["sql_event_gap_write_bytes"].as_u64().unwrap(),
        after.writes - before.writes
    );
    assert!(report["sql_event_gap_write_bytes"].as_u64().unwrap() >= 8192);
    let pages: Vec<_> = report["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["pages"].as_array().unwrap())
        .collect();
    for owner in ["probe_rows", "probe_values", "sqlite_schema"] {
        assert!(
            pages.iter().any(|page| page["object"] == owner),
            "{owner}: {pages:?}"
        );
    }
    let frames: u64 = report["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["wal_frames"].as_u64().unwrap())
        .sum();
    assert_eq!(frames as usize, pages.len());
    for item in report["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .chain(report["kinds"].as_array().unwrap())
    {
        let count = item
            .get("wal_frames")
            .or_else(|| item.get("frames"))
            .unwrap()
            .as_u64()
            .unwrap();
        assert_eq!(
            item["wal_frame_bytes"].as_u64().unwrap(),
            count * (page_size + 24)
        );
    }
    drop(probe);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
