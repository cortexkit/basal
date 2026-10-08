use super::*;
use crate::pool::{PoolConfig, Spawn};
use crate::process::{SpawnError, WorkerLaunch, WorkerProcess};
use basal_core::{Config, Durability, NoHooks, Store};
use basal_host::{MockCatalog, MockConsent, mock::MockHost};
use std::sync::atomic::AtomicUsize;

struct RefuseOrPanic(bool);
impl Spawn for RefuseOrPanic {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        assert!(!self.0, "spawn panic");
        Err(SpawnError::Exec("missing worker".into()))
    }
}

fn engine(panic: bool) -> (Engine, std::path::PathBuf) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-engine-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.db");
    let clock = basal_core::Clock::manual(0);
    let rt = Runtime::new(
        Arc::new(Store::open(&path, Durability { fullfsync: false }).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            clock: clock.clone(),
            ..Config::default()
        },
    );
    let metrics = Arc::new(Metrics::default());
    let mut config = PoolConfig::new("missing", WorkerLaunch::Plain);
    config.warm_spares = 0;
    let pool = Pool::new(
        config,
        Arc::new(RefuseOrPanic(panic)),
        clock,
        metrics.clone(),
    );
    (
        Engine::new(
            rt,
            pool,
            Arc::new(MockConsent::new()),
            EngineConfig::default(),
            metrics,
            Fatal::new(),
        )
        .unwrap(),
        path,
    )
}

fn cleanup(engine: Engine, path: std::path::PathBuf) {
    engine.inner.pool.stop();
    drop(engine);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn panicking_activation_releases_run_and_flow_slots() {
    let (engine, path) = engine(true);
    engine.active().runs.insert("r".into());
    engine.active().flows.insert("f".into());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.activate("r".into(), "f".into())
    }));
    assert!(result.is_err(), "control reached the panicking spawner");
    assert_eq!(engine.active_count(), 0);
    assert!(engine.active().flows.is_empty());
    assert!(
        engine.inner.fatal.get().is_some(),
        "a panic may leave the run claimed and must trigger recovery"
    );
    engine.wait_quiet();
    cleanup(engine, path);
}

#[test]
fn journal_count_failure_is_fatal_not_zero_replayed_calls() {
    let (engine, path) = engine(false);
    engine.inner.rt.store().cut();
    engine.activate("r".into(), "f".into());
    assert!(engine.inner.fatal.get().is_some());
    cleanup(engine, path);
}
