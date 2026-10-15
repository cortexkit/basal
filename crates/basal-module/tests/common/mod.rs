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
    // Fields drop in declaration order, so Module shutdown must precede this
    // guard's directory removal.
    _cleanup: DirectoryCleanup,
}

struct DirectoryCleanup {
    dir: PathBuf,
    store: std::sync::Weak<basal_core::Store>,
}

impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Do not turn the test's original failure into a double-panic abort.
            let _ = std::fs::remove_dir_all(&self.dir);
            return;
        }
        assert!(
            self.store.upgrade().is_none(),
            "fixture cleanup still has an open store owner"
        );
        std::fs::remove_dir_all(&self.dir).expect("remove fixture after module shutdown");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.module.pool.stop();
    }
}

pub fn scratch(tag: &str) -> PathBuf {
    basal_testkit::harness::scratch(&format!("module-{tag}"))
}

pub fn call(f: &Fixture, caller: &Caller, method: &str, params: Value) -> Result<Value, String> {
    f.module
        .handle(caller, method, params)
        .map_err(|e| format!("{}: {}", e.code, e.message))
}

pub fn refused(r: &Result<Value, String>) -> bool {
    matches!(r, Err(e) if e.starts_with("not_permitted"))
}

pub fn events_v2(flow: &str) -> Value {
    let mut manifest = events_manifest(flow);
    manifest["version"] = json!(2);
    manifest
}

/// The real path of an existing `path`, spelled as a manifest root. On
/// Windows `std::fs::canonicalize` adds the extended-length `\\?\` prefix,
/// which manifest roots and the `fs` and `git` built-ins refuse, so it is
/// removed here.
pub fn canonical(path: &std::path::Path) -> PathBuf {
    let real = std::fs::canonicalize(path).expect("canonical path");
    #[cfg(windows)]
    if let Some(plain) = real.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        return PathBuf::from(plain);
    }
    real
}

pub struct Options {
    pub hosts: Option<Hosts>,
    pub worker_binary: Option<PathBuf>,
    pub selector: Arc<dyn basal_host::selector::ModelSelector>,
    pub warm_spares: usize,
    pub max_concurrent: usize,
    pub engine_pass_interval: Duration,
    pub spawner: Option<Arc<dyn Spawn>>,
    pub hooks: Arc<dyn Hooks>,
    pub default_deadline: Duration,
    pub max_activations: u32,
    pub idle_retire: Duration,
    pub activation_deadline: Duration,
    pub budgets: basal_proto::Budgets,
    pub pool_wait_timeout: Option<Duration>,
    /// Off unless a test is about the install gate: the mock consent plane
    /// is not core, so no core approved these tests' flows.
    pub install_gate: InstallGate,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            hosts: None,
            worker_binary: None,
            selector: Arc::new(basal_host::selector::FakeSelector::default()),
            warm_spares: 0,
            max_concurrent: 4,
            engine_pass_interval: EngineConfig::default().pass_interval,
            spawner: None,
            hooks: Arc::new(NoHooks),
            default_deadline: Duration::from_secs(600),
            max_activations: 256,
            idle_retire: Duration::from_secs(600),
            activation_deadline: Duration::from_secs(60),
            budgets: basal_proto::Budgets::default(),
            pool_wait_timeout: None,
            install_gate: InstallGate::Off,
        }
    }
}

impl Options {
    /// Options with one worker already started and handshaken, for tests that
    /// run flows or exercise the pool. The default starts no worker, because
    /// tests that only install or inspect flows never need one.
    pub fn with_worker() -> Self {
        Self {
            warm_spares: 1,
            ..Self::default()
        }
    }
}

pub fn pool_config(o: &Options) -> PoolConfig {
    let mut pool = PoolConfig::new(
        o.worker_binary.clone().unwrap_or_else(worker_binary),
        WorkerLaunch::Plain,
    );
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
        budgets: o.budgets,
        install_gate: o.install_gate,
        ..Config::default()
    };
    runtime.limits.default_deadline = o.default_deadline;
    let config = ModuleConfig {
        store_path: dir.join("basal.db"),
        durability: Durability::throwaway(),
        runtime,
        pool,
        engine: EngineConfig {
            max_concurrent_activations: o.max_concurrent,
            pass_interval: o.engine_pass_interval,
        },
        dry_run: DryRunConfig::new(dir.join("dry-run")),
    };
    let mut store = None;
    let module = Module::start_with_store(
        config,
        o.hosts.unwrap_or_else(|| Hosts {
            transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
            host: Arc::new(mock.clone()),
            catalog: Arc::new(catalog.clone()),
            consent: Arc::new(consent.clone()),
            hooks: o.hooks.clone(),
        }),
        spawner,
        |opened| {
            store = Some(Arc::downgrade(&opened));
            initialize(opened)
        },
    )
    .expect("module starts");
    Fixture {
        module,
        mock,
        consent,
        catalog,
        clock,
        _cleanup: DirectoryCleanup {
            dir: dir.clone(),
            store: store.expect("module initialized its store"),
        },
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

/// Checks a dry run's plain-language text against its committed fixture in
/// `test-vectors/dry-run-text/`, which core and the phone client read as
/// examples of real output. On a mismatch the message holds the whole new
/// file, so an intended change is copied into the fixture by hand.
pub fn assert_dry_run_text_fixture(name: &str, text: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/dry-run-text")
        .join(name);
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    // Through the struct, so the file keeps the fields in their documented
    // order and holds nothing else.
    let text: basal_core::dry_run_text::DryRunText = serde_json::from_value(text.clone()).unwrap();
    let actual = serde_json::to_string_pretty(&text).unwrap() + "\n";
    assert!(
        expected == actual,
        "{} differs from this run's dry_run_text; the new contents are:\n{actual}",
        path.display()
    );
}
