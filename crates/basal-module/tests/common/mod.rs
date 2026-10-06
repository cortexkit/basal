//! Shared helpers: a module started in this process against the mocks, on a
//! manual clock, with the real worker binary.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use basal_core::{Clock, Config, Durability, Hooks, InstallGate, NoHooks};
use basal_host::mock::MockHost;
use basal_host::{CardDecision, MockCatalog, MockConsent};
use basal_module::caller::Caller;
use basal_module::dryrun::DryRunConfig;
use basal_module::engine::EngineConfig;
use basal_module::module::{Hosts, Module, ModuleConfig};
use basal_module::pool::{PoolConfig, ProcessSpawner, Spawn};
use basal_module::process::{SpawnError, WorkerLaunch, WorkerProcess};
use basal_testkit::channel::worker_binary;
use serde_json::{Value, json};

/// 2026-05-01T00:00:00Z.
pub const T0: i64 = 1_777_593_600_000;
pub const HOUR: i64 = 3_600_000;

pub struct Fixture {
    pub module: Module,
    pub mock: MockHost,
    pub consent: MockConsent,
    pub catalog: MockCatalog,
    pub clock: Clock,
    pub dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.module.pool.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn scratch(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("basal-module-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

pub struct Options {
    pub hosts: Option<Hosts>,
    pub selector: Arc<dyn basal_host::selector::ModelSelector>,
    pub warm_spares: usize,
    pub max_concurrent: usize,
    pub spawner: Option<Arc<dyn Spawn>>,
    pub hooks: Arc<dyn Hooks>,
    pub default_deadline: Duration,
    pub max_activations: u32,
    pub idle_retire: Duration,
    pub activation_deadline: Duration,
    pub pool_wait_timeout: Option<Duration>,
    /// Off unless a test is about the install gate: the mock consent plane
    /// is not core, so no core approved these tests' flows.
    pub install_gate: InstallGate,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: None,
            selector: Arc::new(basal_host::selector::FakeSelector::default()),
            warm_spares: 1,
            max_concurrent: 4,
            spawner: None,
            hooks: Arc::new(NoHooks),
            default_deadline: Duration::from_secs(600),
            max_activations: 256,
            idle_retire: Duration::from_secs(600),
            activation_deadline: Duration::from_secs(60),
            pool_wait_timeout: None,
            install_gate: InstallGate::Off,
        }
    }
}

pub fn pool_config(o: &Options) -> PoolConfig {
    let mut pool = PoolConfig::new(worker_binary(), WorkerLaunch::Plain);
    pool.warm_spares = o.warm_spares;
    pool.max_activations = o.max_activations;
    pool.idle_retire = o.idle_retire;
    if let Some(timeout) = o.pool_wait_timeout {
        pool.handshake_timeout = timeout;
        pool.acquire_timeout = timeout;
    }
    pool
}

pub fn fixture(tag: &str, o: Options) -> Fixture {
    fixture_with_store(tag, o, |_| Ok(()))
}

pub fn fixture_with_store(
    tag: &str,
    o: Options,
    initialize: impl FnOnce(Arc<basal_core::Store>) -> Result<(), String>,
) -> Fixture {
    let dir = scratch(tag);
    let clock = Clock::manual(T0);
    let mock = MockHost::new();
    let consent = MockConsent::new();
    let catalog = MockCatalog::standard();
    let pool = pool_config(&o);
    let spawner = o
        .spawner
        .clone()
        .unwrap_or_else(|| Arc::new(ProcessSpawner::new(&pool)));
    let mut runtime = Config {
        selector: o.selector.clone(),
        clock: clock.clone(),
        activation_deadline: o.activation_deadline,
        install_gate: o.install_gate,
        ..Config::default()
    };
    runtime.limits.default_deadline = o.default_deadline;
    let config = ModuleConfig {
        store_path: dir.join("basal.db"),
        durability: Durability { fullfsync: false },
        runtime,
        pool,
        engine: EngineConfig {
            max_concurrent_activations: o.max_concurrent,
            ..EngineConfig::default()
        },
        dry_run: DryRunConfig::new(dir.join("dry-run")),
    };
    let module = Module::start_with_store(
        config,
        o.hosts.unwrap_or_else(|| Hosts {
            host: Arc::new(mock.clone()),
            catalog: Arc::new(catalog.clone()),
            consent: Arc::new(consent.clone()),
            hooks: o.hooks.clone(),
        }),
        spawner,
        initialize,
    )
    .expect("module starts");
    Fixture {
        module,
        mock,
        consent,
        catalog,
        clock,
        dir,
    }
}

/// A manifest for `id` with an events trigger (so approving it creates no
/// schedule), one digest sink, the mock's query and keyed mutation, and
/// `kv`.
pub fn events_manifest(id: &str) -> Value {
    json!({
        "id": id,
        "version": 1,
        "purpose": "A test flow.",
        "trigger": { "events": [ { "module": "plexus", "name": "pull_request_review", "version": 1 } ] },
        "sinks": [ { "agent": "SYNAPSE", "digest_max": "piggyback" } ],
        "ops": [
            { "module": "mock", "op": "echo" },
            { "module": "mock", "op": "send" }
        ]
    })
}

pub fn schedule_manifest(id: &str, schedule: Value) -> Value {
    let mut m = events_manifest(id);
    m["trigger"] = json!({ "schedule": schedule });
    m
}

/// An agent as basal decides it from a route under its core-owned scope.
pub fn agent(name: &str) -> Caller {
    Caller::Agent {
        agent_id: name.to_owned(),
        scope_ref: format!("scope-of-{name}"),
    }
}

/// Installs a version as `caller` and returns the op's reply.
pub fn install(f: &Fixture, caller: &Caller, script: &str, manifest: &Value) -> Value {
    f.module
        .handle(
            caller,
            "flow.install",
            json!({ "script": script, "manifest": manifest.to_string() }),
        )
        .expect("install")
}

/// Installs as `caller` and approves the card as the operator.
pub fn install_approved(f: &Fixture, caller: &Caller, script: &str, manifest: &Value) -> String {
    let reply = install(f, caller, script, manifest);
    let card = reply["card_id"].as_str().expect("card id").to_owned();
    assert!(f.consent.decide(&card, CardDecision::Approve, "operator"));
    assert!(
        f.consent.undelivered().is_empty(),
        "the decision was applied"
    );
    reply["flow_id"].as_str().expect("flow id").to_owned()
}

/// Admits a trigger for an approved flow and returns its run.
pub fn admit(f: &Fixture, flow: &str, trigger_id: &str) -> String {
    f.module
        .rt
        .admit_trigger(flow, trigger_id, basal_proto::JsonText::null())
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

/// A spawner that stops before each spawn until the test lets it go, and
/// tells the test it is waiting. Proves timing claims without timing.
pub struct GatedSpawner {
    inner: ProcessSpawner,
    pub requested: Mutex<Option<Sender<()>>>,
    open: Mutex<u32>,
    cond: Condvar,
    timeout: Duration,
}

impl GatedSpawner {
    pub fn new(pool: &PoolConfig) -> (Arc<Self>, Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Self {
                inner: ProcessSpawner::new(pool),
                requested: Mutex::new(Some(tx)),
                open: Mutex::new(0),
                cond: Condvar::new(),
                timeout: pool.acquire_timeout,
            }),
            rx,
        )
    }

    /// Lets `n` more spawns through.
    pub fn release(&self, n: u32) {
        *self.open.lock().unwrap() += n;
        self.cond.notify_all();
    }
}

impl Spawn for GatedSpawner {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        if let Some(tx) = self.requested.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
        let mut open = self.open.lock().unwrap();
        let deadline = Instant::now() + self.timeout;
        while *open == 0 {
            let now = Instant::now();
            if now >= deadline {
                return Err(SpawnError::Handshake(
                    "timed out waiting for the spawn gate".into(),
                ));
            }
            open = self.cond.wait_timeout(open, deadline - now).unwrap().0;
        }
        *open -= 1;
        drop(open);
        self.inner.spawn()
    }
}
