//! Offline run-log cutover on an explicitly marked store copy. Never starts a runtime.

use basal_core::run_cutover::{Migration, Phase};
use basal_core::{Boundary, Durability, Hooks, Step, Store};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

const CRASH_EXIT: i32 = 86;
const USAGE: &str = "basal-cutover-rehearse --store <copy-dir> [--crash-at <phase>:<before|after>] [--measure-reads]\n\
Requires a regular REHEARSAL_COPY marker and an existing store.db; refuses the live data directory.\n\
Phases: sweep, create, finished_runs, live_runs, broca_calls, drop, page_size, finish.\n\
before/after mean before/after the phase COMMIT (work may already be on disk).\n\
Crash exits 86 without cleanup; rerun without --crash-at to resume.\n\
--measure-reads reports unavailable handlers and times the public single-run history loader 30 times.\n\
stdout is one JSON summary; migration logs go to stderr.";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Args {
    store: PathBuf,
    crash: Option<(Phase, bool)>,
    measure_reads: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = std::env::args_os().skip(1);
    let (mut store, mut crash, mut measure_reads) = (None, None, false);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                eprintln!("{USAGE}");
                return Ok(None);
            }
            Some("--store") if store.is_none() => {
                store = Some(PathBuf::from(
                    args.next().ok_or("--store needs a directory")?,
                ));
            }
            Some("--crash-at") if crash.is_none() => {
                let value = args.next().ok_or("--crash-at needs phase:before|after")?;
                let (phase, side) = value
                    .to_str()
                    .and_then(|v| v.split_once(':'))
                    .ok_or("--crash-at needs phase:before|after")?;
                let phase = Phase::STEPS
                    .into_iter()
                    .find(|p| p.as_str() == phase)
                    .ok_or("unknown crash phase; see --help")?;
                let committed = match side {
                    "before" => false,
                    "after" => true,
                    _ => return Err("crash side must be before or after".into()),
                };
                crash = Some((phase, committed));
            }
            Some("--measure-reads") if !measure_reads => measure_reads = true,
            _ => return Err(format!("unknown or repeated argument {arg:?}; see --help").into()),
        }
    }
    Ok(Some(Args {
        store: store.ok_or("--store is required; see --help")?,
        crash,
        measure_reads,
    }))
}

// Resolve missing tails too, so a missing live data directory cannot disable the guard.
fn canonical_future(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("cannot resolve data directory")?;
            Ok(canonical_future(parent)?.join(path.file_name().ok_or("invalid data directory")?))
        }
        Err(e) => Err(e.into()),
    }
}

fn validate_copy(path: &Path) -> Result<PathBuf> {
    let root = path.canonicalize()?;
    if !root.is_dir() {
        return Err("--store must be a directory".into());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is required to exclude the live store")?;
    let default_data = PathBuf::from(home).join(".local/share");
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_data.clone());
    // Also protect the default location when XDG_DATA_HOME is overridden.
    for data in [data_home, default_data] {
        let live = canonical_future(&data.join("cortexkit/basal"))?;
        if root.starts_with(&live) || live.starts_with(&root) {
            return Err(format!(
                "refusing live basal directory or its ancestor: {}",
                root.display()
            )
            .into());
        }
    }
    if !root.join("REHEARSAL_COPY").is_file() || !root.join("store.db").is_file() {
        return Err("copy needs an existing store.db and a REHEARSAL_COPY marker".into());
    }
    // SQLite sidecars, leases and run-log segments must not alias files outside the copy.
    validate_tree(&root)?;
    Ok(root)
}

fn validate_tree(path: &Path) -> Result<()> {
    let metadata = path.symlink_metadata()?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            validate_tree(&entry?.path())?;
        }
    } else if metadata.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(format!("refusing hard-linked copy file: {}", path.display()).into());
            }
        }
    } else {
        return Err(format!("refusing symlink or special file: {}", path.display()).into());
    }
    Ok(())
}

fn preflight_sweep(store: &Store, root: &Path) -> Result<Vec<Value>> {
    let requests = store.read(|c| {
        Ok(c.prepare(
            "SELECT request FROM journal_with_payloads WHERE kind_code=?1 AND request IS NOT NULL",
        )?
        .query_map([i64::from(basal_proto::Primitive::FsWrite.code())], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut refused = Vec::new();
    for request in requests {
        let Ok(value) = serde_json::from_str::<Value>(&request) else {
            continue;
        };
        let Some(path) = value["args"]["path"].as_str() else {
            continue;
        };
        let Ok(roots) = serde_json::from_value::<Vec<String>>(value["grant"]["roots"].clone())
        else {
            continue;
        };
        // The production sweep trusts historical grants. A copy must not clean their live roots.
        for candidate in std::iter::once(path).chain(roots.iter().map(String::as_str)) {
            let absolute = std::env::current_dir()?.join(candidate);
            match canonical_future(&absolute) {
                Ok(resolved) if resolved.starts_with(root) => (),
                Ok(resolved) => refused.push(json!({"path":candidate,"resolved":resolved,"write_target":path,"reason":"outside rehearsal copy"})),
                Err(error) => refused.push(json!({"path":candidate,"write_target":path,"reason":format!("cannot safely resolve: {error}")})),
            }
        }
    }
    Ok(refused)
}

fn snapshot(store: &Store, root: &Path, post: bool) -> Result<Value> {
    let mut value = store.read(|c| {
        let mut tables = Map::new();
        let mut statement = c.prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?;
        let names = statement.query_map([], |r| r.get::<_, String>(0))?;
        for name in names {
            let name = name?;
            let sql = format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""));
            let count: u64 = c.query_row(&sql, [], |r| r.get(0))?;
            tables.insert(name, json!(count));
        }
        let page_size: u64 = c.pragma_query_value(None, "page_size", |r| r.get(0))?;
        let journal_mode: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        let mut value = json!({"table_rows":tables,"page_size":page_size,"journal_mode":journal_mode});
        if post {
            let integrity = c.prepare("PRAGMA integrity_check")?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let foreign = c.prepare("PRAGMA foreign_key_check")?.query_map([], |r| {
                Ok(json!({"table":r.get::<_,String>(0)?,"rowid":r.get::<_,Option<i64>>(1)?,"parent":r.get::<_,String>(2)?,"fkid":r.get::<_,i64>(3)?}))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            let audit: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='migration_audit')", [], |r| r.get(0))?;
            value["integrity_check"] = json!(integrity);
            value["foreign_key_check"] = json!(foreign);
            value["migration_audit_gone"] = json!(!audit);
        }
        Ok(value)
    })?;
    value["migration_phase"] = json!(basal_core::run_cutover::phase(store)?.as_str());
    value["file_size_bytes"] = json!(std::fs::metadata(root.join("store.db"))?.len());
    value["wal_size_bytes"] = json!(match std::fs::metadata(root.join("store.db-wal")) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    });
    Ok(value)
}

fn post_failures(after: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, ok) in [
        ("page_size must be 1024", after["page_size"] == 1024),
        (
            "journal_mode must be WAL",
            after["journal_mode"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("wal")),
        ),
        (
            "integrity_check must be ok",
            after["integrity_check"] == json!(["ok"]),
        ),
        (
            "foreign_key_check must be empty",
            after["foreign_key_check"] == json!([]),
        ),
        (
            "migration_audit must be gone",
            after["migration_audit_gone"] == true,
        ),
        (
            "migration_phase must be done",
            after["migration_phase"] == "done",
        ),
    ] {
        if !ok {
            failures.push(name.to_owned());
        }
    }
    for table in ["live_runs", "run_history", "obligations", "broca_calls"] {
        if !after["table_rows"][table].is_u64() {
            failures.push(format!("missing row count for {table}"));
        }
    }
    failures
}

#[derive(Default)]
struct Fields(Map<String, Value>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), json!(value));
    }
}

// Keep the production events, including their own duration_ms, instead of timing hooks.
struct StderrLog(Arc<Mutex<Vec<Value>>>);
impl Subscriber for StderrLog {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        fields
            .0
            .insert("target".into(), json!(event.metadata().target()));
        fields
            .0
            .insert("level".into(), json!(event.metadata().level().as_str()));
        let value = Value::Object(fields.0);
        eprintln!("{value}");
        if event.metadata().target() == "migration" {
            self.0.lock().unwrap().push(value);
        }
    }
}

fn boundary(phase: Phase, after: bool) -> Boundary {
    let pair = match phase {
        Phase::Sweep => (
            Boundary::MigrationSweepReady,
            Boundary::MigrationSweepCommitted,
        ),
        Phase::Create => (
            Boundary::MigrationCreateReady,
            Boundary::MigrationCreateCommitted,
        ),
        Phase::FinishedRuns => (
            Boundary::MigrationFinishedReady,
            Boundary::MigrationFinishedCommitted,
        ),
        Phase::LiveRuns => (
            Boundary::MigrationLiveReady,
            Boundary::MigrationLiveCommitted,
        ),
        Phase::BrocaCalls => (
            Boundary::MigrationBrocaReady,
            Boundary::MigrationBrocaCommitted,
        ),
        Phase::Drop => (
            Boundary::MigrationDropReady,
            Boundary::MigrationDropCommitted,
        ),
        Phase::PageSize => (
            Boundary::MigrationPagesReady,
            Boundary::MigrationPagesCommitted,
        ),
        Phase::Finish => (
            Boundary::MigrationFinishReady,
            Boundary::MigrationFinishCommitted,
        ),
        Phase::Done => unreachable!("done is not a crash phase"),
    };
    if after { pair.1 } else { pair.0 }
}

struct Crash {
    cut: Option<(Phase, bool)>,
    before: Value,
    events: Arc<Mutex<Vec<Value>>>,
}
impl Hooks for Crash {
    fn at(&self, _: &str, at: &Boundary) -> Step {
        if let Some((phase, after)) = self.cut
            && *at == boundary(phase, after)
        {
            let mut summary = self.before.clone();
            summary["status"] = json!("crashed");
            summary["crash"] = json!({"phase":phase.as_str(),"side":if after {"after"} else {"before"},"boundary":format!("{at:?}"),"exit_code":CRASH_EXIT});
            summary["phase_logs"] = json!(*self.events.lock().unwrap());
            println!("{summary}");
            // No Rust destructors: leave transaction rollback and lease release to process death.
            std::process::exit(CRASH_EXIT);
        }
        Step::Continue
    }
}

fn rehearse(args: Args, events: Arc<Mutex<Vec<Value>>>, summary: &mut Value) -> Result<()> {
    let root = validate_copy(&args.store)?;
    summary["store"] = json!(root);
    // SAFETY: this executable is single-threaded, and no store/runtime exists yet.
    // Keep SQLite's VACUUM scratch files inside the marked copy on Unix too.
    unsafe {
        std::env::set_var("SQLITE_TMPDIR", &root);
    }
    let store = Arc::new(Store::open(root.join("store.db"), Durability::default())?);
    let initial = basal_core::run_cutover::phase(&store)?;
    summary["resumed_from"] = json!(initial.as_str());
    summary["before"] = snapshot(&store, &root, false)?;
    if initial == Phase::Sweep {
        let refused = preflight_sweep(&store, &root)?;
        if !refused.is_empty() {
            summary["unsafe_fs_write_cleanup"] = json!(refused);
            return Err("refusing pending fs.write sweep outside the copy; see unsafe_fs_write_cleanup paths".into());
        }
    }
    if let Some((phase, _)) = args.crash {
        let index = |p| Phase::STEPS.iter().position(|v| *v == p);
        if initial == Phase::Done || index(phase) < index(initial) {
            return Err("requested crash phase has already committed".into());
        }
    }
    let hooks = Arc::new(Crash {
        cut: args.crash,
        before: summary.clone(),
        events: events.clone(),
    });
    Migration::new(store.clone(), root.join("run-log"), hooks).run()?;
    summary["after"] = snapshot(&store, &root, true)?;
    summary["phase_logs"] = json!(*events.lock().unwrap());
    let failures = post_failures(&summary["after"]);
    summary["post_check_failures"] = json!(failures);
    if !failures.is_empty() {
        return Err("post-migration checks failed".into());
    }
    if args.measure_reads {
        summary["reads"] = measure_reads(&store)?;
    }
    Ok(())
}

fn measure_reads(store: &Store) -> Result<Value> {
    let mut reads = json!({
        "flow.list": {"status":"skipped","reason":"basal-module op_list performs listing SQL inline; reconciliation needs Runtime::flow_health, whose Store query helper basal_core::ops::flow_health_staged is crate-private"},
        "flow.overview": {"status":"skipped","reason":"basal-module phone_flows::entries is private; the handler also needs runtime reconciliation and catalog decoration"},
        "flow.details": {"status":"skipped","reason":"basal-module phone_flows::entries and installs are private; the history page uses Runtime::runs_for_flow, not a public Store-only reader"},
        "diagnostics.findings": {"status":"skipped","reason":"basal-module diagnostics_findings::store_findings_with is private and requires runtime-derived LogFindings; the handler also reads runtime, event reader and worker pool state"}
    });
    // Selection is not timed. This is a single-run loader, NOT any of the four handlers.
    let selected = store.read(|c| {
        Ok(c.query_row(
            "SELECT flow_id,count(*) FROM run_history GROUP BY flow_id ORDER BY count(*) DESC,flow_id LIMIT 1",
            [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)),
        ).optional()?)
    })?;
    let Some((flow, history_rows)) = selected else {
        reads["single_run_detail_proxy"] = json!({"status":"skipped","function":"basal_core::run_store_sql::history","reason":"no retained run history","samples":0});
        return Ok(reads);
    };
    let run: String = store.read(|c| {
        Ok(c.query_row("SELECT run_id FROM run_history WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC LIMIT 1", [&flow], |r| r.get(0))?)
    })?;
    let mut milliseconds = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = std::time::Instant::now();
        let detail = store.read(|c| basal_core::run_store_sql::history(c, &run))?;
        if detail.is_none() {
            return Err("selected history row disappeared".into());
        }
        std::hint::black_box(detail);
        milliseconds.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    milliseconds.sort_by(f64::total_cmp);
    reads["single_run_detail_proxy"] = json!({
        "status":"measured", "function":"basal_core::run_store_sql::history",
        "scope":"single finished-run detail only; not flow.list, flow.overview, flow.details or diagnostics.findings",
        "flow_id":flow,"retained_history_rows":history_rows,"run_id":run,"samples":30,
        "percentile_method":"nearest rank; no warmup; selection excluded",
        "p50_ms":milliseconds[14],"p95_ms":milliseconds[28],"max_ms":milliseconds[29]
    });
    Ok(reads)
}

fn main() {
    let mut summary = json!({"status":"failed"});
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = (|| -> Result<bool> {
        let Some(args) = parse_args()? else {
            return Ok(false);
        };
        tracing::subscriber::set_global_default(StderrLog(events.clone()))?;
        rehearse(args, events.clone(), &mut summary)?;
        Ok(true)
    })();
    match result {
        Ok(false) => (),
        Ok(true) => {
            summary["status"] = json!("ok");
            println!("{summary}");
        }
        Err(error) => {
            eprintln!("{error}");
            summary["error"] = json!(error.to_string());
            summary["phase_logs"] = json!(*events.lock().unwrap());
            println!("{summary}");
            std::process::exit(1);
        }
    }
}
