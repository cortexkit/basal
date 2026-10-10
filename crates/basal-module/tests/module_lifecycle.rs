mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use basal_core::{Config, Durability, NoHooks, Store};
use basal_host::{MockCatalog, MockConsent, mock::MockHost};
use basal_module::dryrun::DryRunConfig;
use basal_module::module::{Hosts, Module, ModuleConfig};
use basal_module::pool::{Binding, Pool, PoolConfig, Spawn};
use basal_module::process::{SpawnError, WorkerLaunch, WorkerProcess};
use serde_json::{Value, json};

#[derive(Default)]
struct FailingSpawner(AtomicUsize);
impl Spawn for FailingSpawner {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(SpawnError::Exec("missing worker".into()))
    }
}

struct RecoverOnAttach(Arc<basal_core::broca::BrocaStore>);
impl basal_host::Host for RecoverOnAttach {
    fn classify(&self, _: &basal_proto::CallKind) -> basal_host::CallClass {
        basal_host::CallClass::Query
    }
    fn dispatch(
        &self,
        _: &basal_host::CallRequest,
    ) -> Result<basal_host::Dispatched, basal_host::TransportError> {
        unreachable!("startup recovery does not dispatch a flow")
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.0
    }
    fn attach(&self, _: Arc<dyn basal_host::CompletionSink>) {
        // BrocaHost also polls immediately when its completion sink attaches.
        // This read must succeed before any recovery can inspect saved calls.
        basal_host::broca::StateStore::load(self.0.as_ref())
            .expect("model store bound before recovery");
    }
}

#[test]
fn startup_binds_the_model_store_before_host_recovery() {
    let dir = common::scratch("bound-before-recovery");
    let snapshots = Arc::new(basal_core::broca::BrocaStore::default());
    let mut pool = PoolConfig::new("unused", WorkerLaunch::Plain);
    pool.warm_spares = 0;
    let config = ModuleConfig {
        store_path: dir.join("store.db"),
        durability: Durability { fullfsync: false },
        runtime: Config::default(),
        pool,
        engine: Default::default(),
        dry_run: DryRunConfig::new(dir.join("scratch")),
    };
    let hosts = Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(RecoverOnAttach(snapshots.clone())),
        catalog: Arc::new(MockCatalog::standard()),
        consent: Arc::new(MockConsent::new()),
        hooks: Arc::new(NoHooks),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Module::start_with_store(
            config,
            hosts,
            Arc::new(FailingSpawner::default()),
            |store| snapshots.bind(store).map_err(|e| e.to_string()),
        )
    }));
    let succeeded = matches!(&result, Ok(Ok(_)));
    if let Ok(Ok(module)) = result {
        module.pool.stop();
        drop(module);
    }
    drop(snapshots);
    std::fs::remove_dir_all(dir).unwrap();
    assert!(succeeded, "host recovery must read the initialized store");
}

#[test]
fn failed_spawns_wait_before_retrying() {
    let clock = basal_core::Clock::manual(0);
    let spawn = Arc::new(FailingSpawner::default());
    let mut config = PoolConfig::new("missing", WorkerLaunch::Plain);
    config.warm_spares = 0;
    let pool = Pool::new(config, spawn.clone(), clock, Arc::default());
    assert!(pool.acquire(Binding::Flow("f".into())).is_err());
    assert!(pool.acquire(Binding::Flow("f".into())).is_err());
    assert_eq!(spawn.0.load(Ordering::SeqCst), 1);
    pool.stop();
}

#[test]
fn failed_start_does_not_bind_or_retain_the_store() {
    let dir = common::scratch("failed-start");
    let scratch = dir.join("scratch");
    std::fs::write(&scratch, "not a directory").unwrap();
    let snapshots = Arc::new(basal_core::broca::BrocaStore::default());
    let spawn = Arc::new(FailingSpawner::default());
    let mut pool = PoolConfig::new("missing", WorkerLaunch::Plain);
    pool.warm_spares = 0;
    let config = ModuleConfig {
        store_path: dir.join("store.db"),
        durability: Durability { fullfsync: false },
        runtime: Config::default(),
        pool,
        engine: Default::default(),
        dry_run: DryRunConfig::new(&scratch),
    };
    let hosts = Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(MockHost::new()),
        catalog: Arc::new(MockCatalog::standard()),
        consent: Arc::new(MockConsent::new()),
        hooks: Arc::new(NoHooks),
    };
    let mut opened = None;
    let result = Module::start_with_store(config.clone(), hosts.clone(), spawn.clone(), |store| {
        opened = Some(Arc::downgrade(&store));
        snapshots.bind(store).map_err(|e| e.to_string())
    });
    assert!(result.is_err());
    assert!(
        opened.is_none(),
        "failed preparation must not bind adapters"
    );
    assert_eq!(spawn.0.load(Ordering::SeqCst), 0);
    let store = Store::open(&config.store_path, config.durability).expect("write lease released");
    drop(store);
    std::fs::remove_file(&scratch).unwrap();
    let module = Module::start_with_store(config, hosts, spawn, |store| {
        snapshots.bind(store).map_err(|e| e.to_string())
    })
    .unwrap_or_else(|e| panic!("retry: {e}"));
    module.pool.stop();
    drop(module);
    drop(snapshots);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn health_uses_the_same_owner_disable_kind_as_list() {
    let f = common::fixture(
        "disable-kind",
        common::Options {
            warm_spares: 0,
            ..Default::default()
        },
    );
    let agent = common::agent("SYNAPSE");
    let flow = common::install_approved(
        &f,
        &agent,
        "return 1;",
        &common::events_manifest("disabled"),
    );
    f.module
        .handle(&agent, "flow.disable", json!({"flow_id":flow}))
        .unwrap();
    let list = f
        .module
        .handle(
            &basal_module::caller::Caller::Operator,
            "flow.list",
            json!({}),
        )
        .unwrap();
    let health = f
        .module
        .handle(
            &basal_module::caller::Caller::Core,
            "flow.health",
            Value::Null,
        )
        .unwrap();
    assert_eq!(list["flows"][0]["disabled"]["by"], "owner");
    assert_eq!(health["flows"][0]["disabled"]["by"], "owner");
    assert_eq!(health["flows"][0]["disabled"]["actor"], "agent:SYNAPSE");
}

#[test]
fn local_package_operations_require_operator_attestation() {
    let f = common::fixture(
        "package-local",
        common::Options {
            warm_spares: 0,
            ..Default::default()
        },
    );
    for op in [
        "package.register",
        "package.get",
        "flow.instance.ensure",
        "flow.instance.remove",
    ] {
        let error = f
            .module
            .handle(&basal_module::caller::Caller::Local, op, json!({}))
            .unwrap_err();
        assert_eq!(error.code, "operator_attestation_required", "{op}");
    }
}
