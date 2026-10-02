//! A test parent process: basal-core's store and driver, real workers, and
//! the mock host, in one process the kill harness can `kill -9`.
//!
//! ```text
//! basal-test-parent --dir <scratch dir> --worker <ck-basal-worker>
//!                   [--kill-at "<boundary>#<occurrence>"] [--fullfsync]
//! ```
//!
//! It opens the store in `<dir>`, recovers any run a previous instance left
//! running, admits the representative run (the same trigger, so a restart
//! finds the same run), and drives it to its end, completing long-running
//! calls whenever the run suspends. The mock host's state lives in
//! `<dir>/mock.json`, synced before every reply, so effects survive the
//! kill like a remote system's would.
//!
//! With `--kill-at`, the process sends itself SIGKILL the moment it passes
//! that boundary. Otherwise it prints one JSON line: the run's summary and
//! every boundary it passed.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use basal_core::{Config, Durability, Runtime, Store};
use basal_host::MockCatalog;
use basal_host::mock::MockHost;
use basal_testkit::ProcessSource;
use basal_testkit::harness::{
    Point, Probe, REPRESENTATIVE, approve_spec, drive, summarize, test_manifest,
};
use serde_json::json;

struct Args {
    dir: PathBuf,
    worker: PathBuf,
    kill_at: Option<Point>,
    fullfsync: bool,
}

fn parse() -> Result<Args, String> {
    let mut dir = None;
    let mut worker = None;
    let mut kill_at = None;
    let mut fullfsync = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            "--worker" => worker = args.next().map(PathBuf::from),
            "--kill-at" => {
                let text = args.next().ok_or("--kill-at needs a point")?;
                kill_at = Some(Point::parse(&text).ok_or("bad point")?);
            }
            "--fullfsync" => fullfsync = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        dir: dir.ok_or("--dir is required")?,
        worker: worker.ok_or("--worker is required")?,
        kill_at,
        fullfsync,
    })
}

fn run(args: Args) -> Result<serde_json::Value, String> {
    let mock = MockHost::persistent(args.dir.join("mock.json")).map_err(|e| e.to_string())?;
    let probe = Arc::new(match args.kill_at {
        Some(point) => Probe::act_at(point, |_, boundary| {
            eprintln!("basal-test-parent: killed at {boundary:?}");
            // SAFETY: kill(2) on our own pid has no memory-safety
            // preconditions.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
        }),
        None => Probe::recording(),
    });
    let store = Store::open(
        args.dir.join("basal.db"),
        Durability {
            fullfsync: args.fullfsync,
        },
    )
    .map_err(|e| e.to_string())?;
    let config = Config {
        selector: Arc::new(basal_host::selector::FakeSelector::default()),
        activation_deadline: Duration::from_secs(30),
        install_gate: basal_core::InstallGate::Off,
        ..Config::default()
    };
    let rt = Runtime::new(
        Arc::new(store),
        Arc::new(mock.clone()),
        Arc::new(MockCatalog::standard()),
        probe.clone(),
        Some(Arc::new(ProcessSource::new(&args.worker))),
        config,
    );
    rt.recover().map_err(|e| e.to_string())?;
    // A restart finds the version it approved before and admits the same
    // trigger, which deduplicates to the run already in the store.
    let spec = approve_spec(
        &rt,
        REPRESENTATIVE,
        &test_manifest(),
        "trigger-1",
        basal_proto::JsonText::null(),
    )?;
    let admission = rt.admit(&spec).map_err(|e| e.to_string())?;
    let run_id = admission.run_id().ok_or("not admitted")?.to_owned();
    let run = drive(&rt, &mock, &run_id, Duration::from_secs(120)).map_err(|e| e.to_string())?;
    rt.quiesce();
    let summary = summarize(&rt, &mock, &run.run_id).map_err(|e| e.to_string())?;
    let points: Vec<String> = probe.points().iter().map(Point::render).collect();
    Ok(json!({"summary": summary.to_json(), "points": points}))
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("basal-test-parent: {e}");
            return ExitCode::from(64);
        }
    };
    match run(args) {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("basal-test-parent: {e}");
            ExitCode::FAILURE
        }
    }
}
