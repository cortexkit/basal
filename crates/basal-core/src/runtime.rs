//! The runtime: the store, the host, and everything that runs beside an
//! activation (dispatches in flight, completions arriving, wakeups).
//!
//! Concurrency model: plain threads. An activation runs on the caller's
//! thread and talks to its worker over blocking frames; every remote call is
//! dispatched on its own thread, so several calls of one run are in flight
//! at once and finish in any order; completions of long-running calls arrive
//! on whatever thread the host uses. All of them meet in the store, whose
//! single connection serialises transactions, and in one condition variable
//! that wakes a blocked activation when anything arrives. Nothing here needs
//! an async runtime: SQLite and the worker pipes are blocking, and a module
//! that embeds this next to an async client calls it from blocking threads.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use basal_host::CallRequest;
use basal_host::{
    Catalog, Completion, CompletionAck, CompletionSink, Host, HostOutcome, SinkError,
    UnknownOutcome, UnknownReason,
};
use basal_proto::{Budgets, CallKind, JsonText};
use serde_json::json;

use crate::admission::{self, Admission, AdmitContext, TriggerSpec};
use crate::authorize::ShellDenylist;
use crate::channel::WorkerSource;
use crate::clock::Clock;
use crate::error::{CoreError, Result};
use crate::hooks::Hooks;

use crate::kv::KvLimits;
use crate::model::{CallRow, Run, RunState, StoredClass};
use crate::rate::RateLimits;
use crate::runs::Lease;
use crate::store::Store;

#[cfg(test)]
#[path = "runtime_regressions.rs"]
mod regressions;

/// Per-run limits, besides the per-activation budgets the worker enforces.
/// The JS heap limit does not cover the parent's buffers or the store, so
/// every argument and result is capped in bytes in the parent before
/// anything parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// Host calls one run may issue; the next one fails the run.
    pub max_calls: u64,
    /// Bytes one run's journal may hold (arguments, requests sent and
    /// outcomes); a call that would pass it fails the run. Kept well under
    /// the IPC frame cap, so a run's replay prefix always fits one frame.
    pub max_journal_bytes: u64,
    /// The largest argument of one call; a larger one is refused as a
    /// journaled rejection before it is parsed.
    pub max_arg_bytes: usize,
    /// The largest result of one call; a larger one is recorded as a
    /// rejection with code `result_too_large` before anything parses it.
    pub max_result_bytes: usize,
    /// The wall-clock deadline of a run whose manifest sets none.
    pub default_deadline: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_calls: 10_000,
            max_journal_bytes: 16 * 1024 * 1024,
            max_arg_bytes: 256 * 1024,
            max_result_bytes: 512 * 1024,
            default_deadline: Duration::from_secs(600),
        }
    }
}

/// Runtime settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// This process's owner identity, written into every run it drives.
    pub owner: String,
    pub budgets: Budgets,
    /// Wall-clock limit for one activation, from the `Activate` frame to the
    /// worker's ending. Waiting on the host counts.
    pub activation_deadline: Duration,
    /// How many times a transient `Unavailable` is retried inside one call,
    /// when retrying is safe.
    pub unavailable_retries: u32,
    pub selector: Arc<dyn basal_host::selector::ModelSelector>,
    pub retry_backoff: Duration,
    /// Backoff for a provider's known-unsent refusal when it supplies no hint.
    /// These retries replay an activation, rather than retrying a transport send.
    pub deferral_retry: Duration,
    pub deferral_retry_max: Duration,
    /// After this many activations end with a broken worker, the run fails.
    pub max_broken_activations: u32,
    /// A run made runnable by a completion is resumed at once on a worker
    /// from the source.
    pub auto_resume: bool,
    /// Route authorized self-ending calls to a capture host instead of applying
    /// the local instance lifecycle effect. Only disposable dry-run runtimes
    /// enable this; ordinary-flow authorization still refuses the primitive.
    pub capture_instance_end: bool,
    /// The clock for token windows, rate windows and run deadlines.
    pub clock: Clock,
    /// Module ops no flow may call, even when its manifest lists them.
    pub shell_denylist: ShellDenylist,
    pub limits: RunLimits,
    pub kv: KvLimits,
    pub rate: RateLimits,
    /// The scheduler's settings, used when an approval plans the fires a
    /// replaced schedule owed, and by [`Runtime::scheduler`].
    pub schedule: crate::schedule::SchedulerConfig,
    /// Whether every activation first asks core whether it still stands
    /// behind the run's flow version (see [`crate::gate`]).
    pub install_gate: crate::gate::InstallGate,
    /// How long a run waits to be checked again after core gave its gate no
    /// answer. Each further unanswered check doubles it, up to
    /// `install_gate_retry_max`.
    pub install_gate_retry: Duration,
    pub install_gate_retry_max: Duration,
    pub retention: crate::retention::RetentionConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            owner: format!("basal-{}", std::process::id()),
            budgets: Budgets::default(),
            activation_deadline: Duration::from_secs(60),
            unavailable_retries: 3,
            selector: Arc::new(basal_host::selector::UnconfiguredSelector),
            retry_backoff: Duration::from_millis(5),
            deferral_retry: Duration::from_secs(1),
            deferral_retry_max: Duration::from_secs(60),
            max_broken_activations: 3,
            auto_resume: false,
            capture_instance_end: false,
            clock: Clock::system(),
            shell_denylist: ShellDenylist::default(),
            limits: RunLimits::default(),
            kv: KvLimits::default(),
            rate: RateLimits::default(),
            schedule: crate::schedule::SchedulerConfig::default(),
            install_gate: crate::gate::InstallGate::Core,
            install_gate_retry: Duration::from_secs(1),
            install_gate_retry_max: Duration::from_secs(60),
            retention: crate::retention::RetentionConfig::default(),
        }
    }
}

/// How an activation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationEnd {
    Succeeded {
        value: String,
    },
    Failed {
        kind: String,
        detail: String,
    },
    Suspended {
        awaited: Vec<u64>,
    },
    /// The run is runnable again: something arrived as it suspended, or the
    /// worker broke and the run will be replayed on another worker.
    Requeued,
    NeedsReconcile {
        positions: Vec<u64>,
    },
    EngineMismatch {
        detail: String,
    },
    /// Another activation took the run, or it was cancelled. Nothing more
    /// was written for this one and its worker was killed.
    OwnerLost,
    /// A hook simulated a crash.
    Crashed,
    /// The run was not pending, so no activation started.
    NotRunnable {
        state: RunState,
    },
    /// An earlier run of the same flow holds the flow's slot; this one
    /// waits until that run reaches a terminal state.
    Waiting {
        holder: String,
    },
    /// The install gate got no answer from core (unreachable, timed out, or
    /// a reply basal cannot read), so nothing was activated. The run stays
    /// pending and is not offered again for `retry_in`.
    Deferred {
        detail: String,
        retry_in: Duration,
    },
    /// Core no longer stands behind the run's flow version: the version is
    /// recorded as revoked and the run was cancelled before activating.
    Revoked {
        version: u32,
        cause: crate::gate::RevokeCause,
        detail: String,
    },
}

/// A wake-up counter: bumped whenever anything a blocked activation might
/// wait for is committed.
pub(crate) struct Signal {
    epoch: Mutex<u64>,
    cond: Condvar,
    listener: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Signal {
    fn lock(&self) -> MutexGuard<'_, u64> {
        lock(&self.epoch)
    }

    pub(crate) fn current(&self) -> u64 {
        *self.lock()
    }

    pub(crate) fn bump(&self) {
        *self.lock() += 1;
        self.cond.notify_all();
        let listener = lock(&self.listener).clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    /// Waits until the counter moves past `seen` or `timeout` passes.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) {
        let guard = self.lock();
        let _ = self
            .cond
            .wait_timeout_while(guard, timeout, |epoch| *epoch == seen);
    }
}

/// State shared by every runtime handle over one store in one process.
pub(crate) struct Shared {
    pub(crate) staged_hosts: Option<Arc<crate::run_host::RecoveredHosts>>,
    pub(crate) staged_owners: Mutex<HashMap<String, (u64, String)>>,
    pub(crate) staged_workers: Mutex<HashMap<(String, u64), std::thread::ThreadId>>,
    pub(crate) staged_stopped: Condvar,
    pub(crate) ready: AtomicBool,
    pub(crate) removal_drain: Mutex<()>,
    stopping: AtomicBool,
    pub(crate) store: Arc<Store>,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) catalog: Arc<dyn Catalog>,
    pub(crate) hooks: Arc<dyn Hooks>,
    pub(crate) source: Option<Arc<dyn WorkerSource>>,

    pub(crate) flow_routes: Mutex<()>,
    /// Runs whose install gate got no answer from core, with when they may
    /// be checked again. In memory: a restart checks every run at once.
    pub(crate) gate_backoff: Mutex<HashMap<String, crate::gate::Backoff>>,
    event_scope_warnings: Mutex<EventScopeWarnings>,

    pub(crate) signal: Signal,
    threads: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) retention_next: Mutex<Option<i64>>,
    pub(crate) refusal_flush: Mutex<()>,
    pub(crate) owner_notice_drain: Mutex<()>,
    /// The `fs.write` temporary files this process is writing now.
    pub(crate) fs_temps: crate::retention::FsTemps,
}

const EVENT_SCOPE_WARNING_HISTORY: usize = 4096;

/// Warning state is process-local, so one repeat after restart is expected.
/// Ended runs are removed and FIFO eviction bounds the remaining live IDs.
#[derive(Default)]
struct EventScopeWarnings {
    run_ids: HashSet<String>,
    order: VecDeque<String>,
}

impl EventScopeWarnings {
    fn insert_once(&mut self, run_id: &str) -> bool {
        if !self.run_ids.insert(run_id.to_owned()) {
            return false;
        }
        self.order.push_back(run_id.to_owned());
        while self.order.len() > EVENT_SCOPE_WARNING_HISTORY {
            if let Some(expired) = self.order.pop_front() {
                self.run_ids.remove(&expired);
            }
        }
        true
    }

    fn remove(&mut self, run_id: &str) {
        if self.run_ids.remove(run_id) {
            self.order.retain(|id| id != run_id);
        }
    }
}

/// A handle on the runtime. Cheap to clone.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) shared: Arc<Shared>,
    pub(crate) config: Arc<Config>,
}

/// The completion sink handed to the host. It holds the runtime weakly, so
/// a host outliving the runtime does not keep its store open.
struct Sink {
    shared: Weak<Shared>,
    config: Arc<Config>,
}

impl CompletionSink for Sink {
    fn route_released(&self, run_id: &str) {
        if let Some(shared) = self.shared.upgrade() {
            let rt = Runtime {
                shared,
                config: self.config.clone(),
            };
            if let Ok(run) = rt.run(run_id)
                && run.state.is_terminal()
            {
                let _ = rt.release_unused_flow_routes_for(&run.flow_id);
            }
            rt.shared.signal.bump();
        }
    }
    fn complete(&self, c: &Completion) -> std::result::Result<CompletionAck, SinkError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| SinkError("the runtime is gone".into()))?;
        let rt = Runtime {
            shared,
            config: self.config.clone(),
        };
        rt.complete(c).map_err(|e| SinkError(e.to_string()))
    }

    fn unknown(&self, u: &UnknownOutcome) -> std::result::Result<CompletionAck, SinkError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| SinkError("the runtime is gone".into()))?;
        Runtime {
            shared,
            config: self.config.clone(),
        }
        .unknown(u)
        .map_err(|e| SinkError(e.to_string()))?
        .ok_or_else(|| SinkError("the run is running; report the unknown outcome again".into()))
    }
}

use basal_proto::util::lock;

/// The rejection recorded for a call the host could not be reached for,
/// when the call provably had no effect.
pub(crate) fn unavailable_rejection(detail: &str) -> HostOutcome {
    HostOutcome::rejected(
        JsonText::new(
            json!({"message": "host unavailable", "code": "unavailable", "detail": detail})
                .to_string(),
        )
        .unwrap_or_else(|_| JsonText::null()),
    )
}

impl Runtime {
    /// Migrate and recover the log before constructing or attaching a runtime.
    pub fn new(
        store: Arc<Store>,
        host: Arc<dyn Host>,
        catalog: Arc<dyn Catalog>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
    ) -> Result<Self> {
        let recovered = Self::recover_hosts(store, hooks.clone(), &config)?;
        let rt = Self::new_staged(recovered, host, catalog, hooks, source, config)?;
        rt.recover()?;
        Ok(rt)
    }

    /// Startup preparation has no host, runtime or worker side effects. The
    /// returned registry is complete before decision adapters or sinks attach.
    pub fn recover_hosts(
        store: Arc<Store>,
        hooks: Arc<dyn Hooks>,
        config: &Config,
    ) -> Result<Arc<crate::run_host::RecoveredHosts>> {
        let path: String = store.read(|c| {
            Ok(c.query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )?)
        })?;
        let directory = std::path::Path::new(&path)
            .parent()
            .ok_or_else(|| CoreError::Invalid("store needs a directory".into()))?
            .join("run-log");
        crate::run_cutover::Migration::new(store.clone(), &directory, hooks.clone()).run()?;
        let meta = store.read(crate::run_store_sql::meta)?;
        let (writer, recovered) =
            crate::run_log::RunLog::<crate::run_records::Record>::open_restoring(
                crate::run_log::Config {
                    directory,
                    identity: crate::run_log::Identity {
                        store_id: meta.store_id.as_bytes().try_into().map_err(|_| {
                            CoreError::Corrupt("store identity must contain 16 bytes".into())
                        })?,
                        log_epoch: meta.log_epoch,
                    },
                    lease_epoch: store.lease_epoch(),
                    target_size: crate::run_log::Config::TARGET_SIZE,
                    hooks: hooks.clone(),
                },
                store.clone(),
            )
            .map_err(|e| CoreError::Store(e.to_string()))?;
        let log = Arc::new(writer);
        let budgets = crate::run_driver::Budgets {
            dispatch_window_ms: i64::try_from(config.rate.window.as_millis())
                .map_err(|_| CoreError::Invalid("invalid rate window".into()))?,
            token_window_ms: 86_400_000,
            dispatches: u64::from(config.rate.max_dispatches),
            tokens: u64::MAX,
            cost_micros: u64::MAX,
            cost_window: None,
        };
        Ok(Arc::new(crate::run_host::RecoveredHosts::recover(
            store,
            log.clone(),
            Arc::new(log.watermark()),
            &recovered.records,
            budgets,
            hooks,
            config.clock.clone(),
        )?))
    }

    /// Staged construction requires completed registry recovery before attach
    /// can poll saved host work. The production host bundle is built between
    /// recovery and this constructor so its adapters see the complete registry.
    pub fn new_staged(
        hosts: Arc<crate::run_host::RecoveredHosts>,
        host: Arc<dyn Host>,
        catalog: Arc<dyn Catalog>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
    ) -> Result<Self> {
        let rt = Self::new_inner(
            hosts.store.clone(),
            host,
            catalog,
            hooks,
            source,
            config,
            Some(hosts),
        );
        rt.configure_staged_decisions()?;
        rt.capture_existing_removals()?;
        rt.resume_run_removals()?;
        if !rt.removals_complete()? {
            return Err(CoreError::Unavailable(
                "removed runs are still closing during startup".into(),
            ));
        }
        rt.attach_host();
        rt.flush_staged_live_refusals()?;
        rt.flush_refusals()?;
        rt.shared.ready.store(true, Ordering::Release);
        Ok(rt)
    }

    fn new_inner(
        store: Arc<Store>,
        host: Arc<dyn Host>,
        catalog: Arc<dyn Catalog>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
        staged_hosts: Option<Arc<crate::run_host::RecoveredHosts>>,
    ) -> Self {
        store.set_clock(config.clock.clone());
        let shared = Arc::new(Shared {
            staged_hosts,
            staged_owners: Mutex::new(HashMap::new()),
            staged_workers: Mutex::new(HashMap::new()),
            staged_stopped: Condvar::new(),
            ready: AtomicBool::new(false),
            removal_drain: Mutex::new(()),
            stopping: AtomicBool::new(false),
            store,
            host,
            catalog,
            hooks,
            source,

            flow_routes: Mutex::new(()),
            gate_backoff: Mutex::new(HashMap::new()),
            event_scope_warnings: Mutex::new(EventScopeWarnings::default()),

            signal: Signal {
                epoch: Mutex::new(0),
                cond: Condvar::new(),
                listener: Mutex::new(None),
            },
            threads: Mutex::new(Vec::new()),
            retention_next: Mutex::new(None),
            refusal_flush: Mutex::new(()),
            owner_notice_drain: Mutex::new(()),
            fs_temps: crate::retention::FsTemps::default(),
        });
        let config = Arc::new(config);
        let work = Arc::downgrade(&shared);
        shared.host.on_catalog_change(Arc::new(move || {
            if let Some(shared) = work.upgrade() {
                shared.signal.bump();
            }
        }));
        shared
            .host
            .scope_checks(config.install_gate == crate::gate::InstallGate::Core);
        let rt = Self { shared, config };
        {}
        rt
    }

    fn attach_host(&self) {
        self.shared.host.attach(Arc::new(Sink {
            shared: Arc::downgrade(&self.shared),
            config: self.config.clone(),
        }));
        self.shared
            .host
            .bind_fs_temps(crate::retention::fs_temp_ledger(&self.shared));
    }

    /// Another handle on the same store and process state with a different
    /// owner identity, standing in for a second owner taking runs over.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_owner(&self, owner: &str) -> Self {
        let mut config = (*self.config).clone();
        config.owner = owner.to_owned();
        Self {
            shared: self.shared.clone(),
            config: Arc::new(config),
        }
    }

    pub fn store(&self) -> &Store {
        &self.shared.store
    }

    /// All runtime construction is log-backed.
    pub fn uses_run_log(&self) -> bool {
        true
    }

    /// Stop this runtime's execution writes before closing hosts or workers.
    /// Durable running claims and unfinished calls are left for a new runtime's
    /// recovery. A transaction already executing is allowed to finish normally.
    pub fn stop_for_recovery(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.signal.bump();
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::Acquire)
    }

    fn write_execution<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.shared.store.write(|tx| {
            // Check after acquiring the writer: a queued dispatch/completion may
            // wake only after shutdown has closed the transport that served it.
            if self.is_stopping() {
                return Err(CoreError::Unavailable(
                    "runtime stopped for recovery".into(),
                ));
            }
            write(tx)
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Attaches the engine's coalescing wake signal. The callback must not
    /// read the store: commits may notify while holding other runtime locks.
    pub fn on_work(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.signal.listener) = Some(notify);
    }

    // ---- Admission ---------------------------------------------------------

    pub(crate) fn admit_context(&self) -> Result<AdmitContext> {
        Ok(AdmitContext {
            now_ms: self.config.clock.now_ms(),
            rate: self.config.rate,
            default_deadline_ms: i64::try_from(self.config.limits.default_deadline.as_millis())
                .map_err(|_| CoreError::Invalid("default deadline too long".into()))?,
        })
    }

    /// Admits a trigger whose spec carries the flow's approved script and
    /// manifest. Anything else is refused as [`Admission::NotApproved`].
    pub fn admit(&self, spec: &TriggerSpec) -> Result<Admission> {
        self.staged_admit(spec)
    }

    /// Admits a trigger under the flow's currently approved version.
    pub fn admit_trigger(
        &self,
        flow_id: &str,
        trigger_id: &str,
        trigger: JsonText,
    ) -> Result<Admission> {
        self.staged_admit_trigger(flow_id, trigger_id, trigger)
    }

    /// A scheduler sharing this runtime's clock and admission limits.
    pub fn scheduler(&self) -> Result<crate::schedule::Scheduler> {
        let ctx = self.admit_context()?;
        Ok(crate::schedule::Scheduler::new(
            self.clone(),
            Arc::new(self.config.clock.clone()),
            self.config.schedule.clone(),
        )
        .with_limits(crate::schedule::AdmitLimits {
            rate: ctx.rate,
            default_deadline_ms: ctx.default_deadline_ms,
        }))
    }

    pub fn retrigger(&self, run_id: &str) -> Result<Admission> {
        self.staged_retrigger(run_id)
    }

    // ---- Reading -----------------------------------------------------------

    pub fn run(&self, run_id: &str) -> Result<Run> {
        self.staged_run(run_id)
    }

    pub fn calls(&self, run_id: &str) -> Result<Vec<CallRow>> {
        self.staged_calls(run_id)
    }

    /// Why a call was last recorded unknown, if it ever was.
    pub fn unknown_reason(&self, run_id: &str, position: u64) -> Result<Option<UnknownReason>> {
        if let Some(s) = self.staged_snapshot(run_id) {
            return Ok(s
                .calls
                .get(&position)
                .and_then(|c| c.unknown.as_deref())
                .and_then(crate::run_records::parse_unknown_reason));
        }
        self.store().read(|c| {
            let call = crate::run_store_sql::call(c, run_id, position)?;
            Ok(call.and_then(|c| {
                c.detail["unknown"]
                    .as_str()
                    .and_then(crate::run_records::parse_unknown_reason)
            }))
        })
    }

    pub fn activation_count(&self, run_id: &str) -> Result<u64> {
        if let Some(s) = self.staged_snapshot(run_id) {
            return Ok(s.activation_count);
        }
        self.store().read(|c| {
            crate::run_store_sql::history(c, run_id)?
                .map(|h| h.activation_count)
                .ok_or_else(|| CoreError::NoSuchRun(run_id.into()))
        })
    }

    // ---- Ownership ---------------------------------------------------------

    /// Moves every `running` run back to `pending`. Called once when the
    /// process starts: the store's lease proves their owners are gone.
    pub fn recover(&self) -> Result<Vec<String>> {
        self.staged_recover()
    }

    /// Takes a running run from the activation holding `generation`.
    pub fn take_over(&self, run_id: &str, generation: u64) -> Result<Lease> {
        self.staged_take_over(run_id, generation)
    }

    pub fn cancel(&self, run_id: &str, actor: &str) -> Result<()> {
        self.staged_cancel(run_id, actor)
    }

    // ---- Waiting -----------------------------------------------------------

    /// Waits until the run's state satisfies `done`, or `timeout` passes.
    pub fn wait_for(
        &self,
        run_id: &str,
        timeout: Duration,
        done: impl Fn(RunState) -> bool,
    ) -> Result<RunState> {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = self.shared.signal.current();
            let state = { self.run(run_id)?.state };
            let now = Instant::now();
            if done(state) {
                return Ok(state);
            }
            if now >= deadline {
                return Err(CoreError::Timeout {
                    run_id: run_id.to_owned(),
                    state,
                });
            }
            self.shared
                .signal
                .wait(seen, (deadline - now).min(Duration::from_millis(50)));
        }
    }

    /// Joins every dispatch and wake thread this runtime started.
    pub fn quiesce(&self) {
        loop {
            let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.shared.threads));
            if handles.is_empty() {
                return;
            }
            for h in handles {
                let _ = h.join();
            }
        }
    }

    /// Includes finished threads while their handles remain in the runtime's thread list.
    #[cfg(test)]
    pub(crate) fn thread_handle_count(&self) -> usize {
        lock(&self.shared.threads).len()
    }

    pub(crate) fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        let handle = thread::spawn(f);
        let mut threads = lock(&self.shared.threads);
        threads.retain(|h| !h.is_finished());
        threads.push(handle);
    }

    fn reap_threads(&self) {
        lock(&self.shared.threads).retain(|h| !h.is_finished());
    }

    /// Deliver only committed refusal intents. A flush lock prevents two
    /// activations acknowledging the same intent concurrently. A crash may
    /// redeliver an intent, as the host's acknowledgement API allows.
    pub(crate) fn flush_refusals(&self) -> Result<()> {
        if self.is_stopping() {
            return Ok(());
        }
        let _flush = lock(&self.shared.refusal_flush);
        let rows = self.store().read(|c| {
            let mut stmt = c.prepare(
                "SELECT seq,body FROM outbox WHERE kind='refusal_committed' ORDER BY seq LIMIT 128",
            )?;
            Ok(stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (seq, body) in rows {
            let saved: crate::refusal_receipt::SavedRefusal =
                serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
            self.shared
                .host
                .refusal_committed(&saved.request()?, &saved.refusal()?);
            self.write_execution(|tx| {
                tx.execute(
                    "DELETE FROM outbox WHERE seq=?1 AND kind='refusal_committed'",
                    [seq],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    // ---- Outcomes ----------------------------------------------------------

    /// Accepts a long-running call's completion. The same entry point the
    /// host's completion sink uses; callers that learn of completions
    /// another way (a subscription replayed after a restart) use it
    /// directly.
    pub fn complete(&self, completion: &Completion) -> Result<CompletionAck> {
        let hosts = self.shared.staged_hosts.as_ref().expect("recovered hosts");
        let capped = self
            .cap_result(&completion.outcome)
            .map(|outcome| Completion {
                outcome,
                ..completion.clone()
            });
        let ack = hosts
            .complete(capped.as_ref().unwrap_or(completion))
            .map_err(|e| CoreError::Unavailable(e.to_string()))?;
        self.shared.signal.bump();
        Ok(ack)
    }

    /// Records a host's report that an accepted call's outcome cannot be
    /// established. The same entry point the host's completion sink uses.
    /// `None` means the run's activation is still running and the report
    /// was not recorded: the host must make it again later.
    pub fn unknown(&self, u: &UnknownOutcome) -> Result<Option<CompletionAck>> {
        let hosts = self.shared.staged_hosts.as_ref().expect("recovered hosts");
        let ack = hosts
            .unknown(u)
            .map_err(|e| CoreError::Unavailable(e.to_string()))?;
        self.shared.signal.bump();
        Ok(Some(ack))
    }

    /// A result over the byte cap, replaced before anything parses it by a
    /// rejection that names its size. The replacement depends only on the
    /// size, so a redelivery of the same oversized result is recognised as
    /// a duplicate.
    pub(crate) fn cap_result(&self, outcome: &HostOutcome) -> Option<HostOutcome> {
        let cap = self.config.limits.max_result_bytes;
        let bytes = outcome.value.len();
        (bytes > cap).then(|| {
            let mut rejected = crate::kv::rejection(
                "result_too_large",
                &format!("the host's result of {bytes} bytes exceeds the cap of {cap}"),
            );
            rejected.usage = outcome.usage;
            rejected
        })
    }

    /// Starts an activation for a run made runnable, if configured to.
    pub(crate) fn wake(&self, run_id: &str) {
        if !self.shared.ready.load(Ordering::Acquire)
            || self.is_stopping()
            || !self.config.auto_resume
            || self.shared.source.is_none()
        {
            return;
        }
        let rt = self.clone();
        let run_id = run_id.to_owned();
        self.spawn(move || {
            // An activation that ends requeued wakes the run again itself.
            let _ = rt.resume(&run_id);
        });
    }

    /// Drives a run until it rests: terminal, `needs_reconcile`, or
    /// `suspended` (nothing more happens until a completion arrives), or
    /// until `timeout` passes.
    pub fn run_to_rest(&self, run_id: &str, timeout: Duration) -> Result<Run> {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = self.shared.signal.current();
            let run = self.run(run_id)?;
            let now = Instant::now();
            let resting = run.state.is_terminal()
                || matches!(run.state, RunState::NeedsReconcile | RunState::Suspended);
            if resting || now >= deadline {
                return Ok(run);
            }
            match run.state {
                RunState::Pending => {
                    if let ActivationEnd::Waiting { .. } | ActivationEnd::Deferred { .. } =
                        self.resume(run_id)?
                    {
                        // An earlier run of the flow holds the slot (the
                        // signal is bumped when that run ends), or core gave
                        // the install gate no answer and the run waits to
                        // ask again.
                        self.shared
                            .signal
                            .wait(seen, (deadline - now).min(Duration::from_millis(50)));
                    }
                }
                _ => self
                    .shared
                    .signal
                    .wait(seen, (deadline - now).min(Duration::from_millis(50))),
            }
        }
    }

    // ---- Dispatch ----------------------------------------------------------

    pub(crate) fn mark_event_scope_warning(&self, run_id: &str) -> bool {
        lock(&self.shared.event_scope_warnings).insert_once(run_id)
    }

    pub(crate) fn clear_event_scope_warning(&self, run_id: &str) {
        lock(&self.shared.event_scope_warnings).remove(run_id);
    }

    /// The request for a journaled call.
    #[cfg(test)]
    pub(crate) fn request(run: &Run, row: &CallRow, attempt: u32) -> CallRequest {
        CallRequest {
            flow_version: run.flow_version,
            flow_id: run.flow_id.clone(),
            run_id: run.run_id.clone(),
            position: row.position,
            kind: row.kind.clone(),
            args: row.dispatch_args(),
            idempotency_key: row.idempotency_key.clone(),
            attempt,
        }
    }

    pub(crate) fn class_of(
        &self,
        kind: &CallKind,
        args: Option<&serde_json::Value>,
    ) -> StoredClass {
        if kind.is_synchronous() {
            return StoredClass::Sync;
        }
        if crate::kv::is_kv(kind)
            || (!self.config.capture_instance_end
                && matches!(
                    kind,
                    basal_proto::CallKind::Primitive(basal_proto::Primitive::InstanceEnd)
                ))
        {
            return StoredClass::Local;
        }
        // A built-in's class is basal's own rule, not the host's, and for
        // `net.fetch` it depends on the method in the arguments. Arguments
        // over the cap are refused before dispatch, so they are not parsed.
        let class = args.and_then(|parsed| basal_host::builtins::class(kind, parsed));
        match class.unwrap_or_else(|| self.shared.host.classify(kind)) {
            basal_host::CallClass::Query => StoredClass::Query,
            basal_host::CallClass::Mutation {
                honours_idempotency_keys: true,
            } => StoredClass::KeyedMutation,
            basal_host::CallClass::Mutation {
                honours_idempotency_keys: false,
            } => StoredClass::Mutation,
        }
    }

    // ---- Operations --------------------------------------------------------

    /// Stops admitting runs. Returns the runs not yet finished, by state, so
    /// the caller can wait for suspended ones before an engine upgrade.
    pub fn drain(&self) -> Result<BTreeMap<String, Vec<String>>> {
        self.write_execution(|tx| admission::set_draining(tx, true))?;
        let health = self.health()?;
        Ok(health.unfinished)
    }

    pub fn undrain(&self) -> Result<()> {
        self.write_execution(|tx| admission::set_draining(tx, false))
    }

    pub fn health(&self) -> Result<crate::ops::Health> {
        self.log_health()
    }

    /// The age of the oldest pending, suspended and `needs_reconcile` run,
    /// on the runtime's clock.
    pub fn run_ages(&self) -> Result<crate::ops::RunAges> {
        let now = self.config.clock.now_ms();

        let runs = self.log_runs(None)?;
        let oldest = |state| {
            runs.iter()
                .filter(|r| r.state == state)
                .map(|r| now.saturating_sub(r.admitted_at).max(0))
                .max()
        };
        Ok(crate::ops::RunAges {
            oldest_pending_ms: oldest(RunState::Pending),
            oldest_suspended_ms: oldest(RunState::Suspended),
            oldest_needs_reconcile_ms: oldest(RunState::NeedsReconcile),
        })
    }

    /// Every flow's health, on the runtime's clock.
    pub fn flow_health(&self) -> Result<Vec<crate::ops::FlowHealth>> {
        let now = self.config.clock.now_ms();

        let runs = self.log_runs(None)?;
        let states = self.log_states();
        self.store()
            .read(|c| crate::ops::flow_health_staged(c, now, &runs, &states))
    }

    /// Pending runs no earlier run of their flow holds back, as (run,
    /// flow), in admission order: what can start now.
    pub fn startable(&self) -> Result<Vec<(String, String)>> {
        self.resume_run_removals()?;
        self.flush_refusals()?;
        self.staged_startable()
    }

    pub fn catalog(&self) -> &dyn Catalog {
        self.shared.catalog.as_ref()
    }

    // ---- Slots and deadlines ----------------------------------------------

    /// Fails runs past their deadline, then services durable provider
    /// acknowledgements and retention on the runtime's own cadence.
    pub fn enforce_deadlines(&self) -> Result<Vec<String>> {
        self.resume_run_removals()?;
        self.flush_refusals()?;
        let now = self.config.clock.now_ms();
        let mut expired = Vec::new();
        for run in self.log_runs(None)? {
            if matches!(
                run.state,
                RunState::Pending | RunState::Running | RunState::Suspended
            ) && run.deadline_at.is_some_and(|at| at <= now)
            {
                let (kind, detail) = self
                    .event_body_deadline_failure(&run.run_id)?
                    .unwrap_or_else(|| {
                        (
                            "deadline".into(),
                            "the run passed its wall-clock deadline".into(),
                        )
                    });
                match self.staged_finish(&run.run_id, RunState::Failed, Some(&kind), Some(&detail))
                {
                    Ok(()) => {
                        expired.push(run.run_id);
                        self.wake_flow(&run.flow_id);
                    }
                    Err(CoreError::Unavailable(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.flush_refusals()?;
        self.retention_due(now)?;
        self.reap_threads();
        Ok(expired)
    }

    /// Wakes the next startable run while respecting the flow slot.
    pub(crate) fn wake_flow(&self, flow_id: &str) {
        if !self.shared.ready.load(Ordering::Acquire)
            || !self.config.auto_resume
            || self.shared.source.is_none()
        {
            return;
        }
        if let Ok(runs) = self.staged_startable()
            && let Some((id, _)) = runs.into_iter().find(|(_, flow)| flow == flow_id)
        {
            self.wake(&id);
        }
    }

    // ---- Tokens ------------------------------------------------------------

    /// Records a model call's usage reported apart from its outcome (Broca
    /// reports usage with the run's end). Deduplicated by `send_id`: only
    /// the first report for a reservation is applied, whichever way it
    /// came. Returns whether this one was applied.
    #[cfg(any(test, feature = "test-support"))]
    pub fn report_usage(&self, send_id: &str, usage: crate::tokens::Usage) -> Result<bool> {
        let hosts = self
            .shared
            .staged_hosts
            .as_ref()
            .expect("log-backed runtime");
        for actor in hosts.registry().snapshot() {
            let (id, _, _, state) = actor.reader_snapshot();
            let Some(position) = state.calls.values().find_map(|call| {
                call.intent
                    .as_ref()
                    .filter(|i| i.idempotency_key.as_deref() == Some(send_id))
                    .map(|_| call.identity.position)
            }) else {
                continue;
            };
            let Ok(entrant) = actor.enter() else {
                break;
            };
            let budgets = hosts.budgets_for(&id)?;
            return entrant.account(|records| {
                records.check_healthy()?;
                let state = records.state();
                let call = &state.calls[&position];
                if call.arrival.is_some()
                    || call.usage_report.is_some()
                    || call
                        .intent
                        .as_ref()
                        .and_then(|i| i.reservation.as_ref())
                        .is_none_or(|r| r.input_tokens.saturating_add(r.output_tokens) == 0)
                {
                    return Ok((false, crate::run_driver::accounting::rows(state, &budgets)?));
                }
                let lsn = records.append(
                    state.generation,
                    crate::run_records::Body::MeasuredUsage {
                        position,
                        report: crate::run_records::UsageReport {
                            input_tokens: Some(usage.input_tokens),
                            cache_write_tokens: Some(usage.cache_write_tokens),
                            output_tokens: Some(usage.output_tokens),
                            cached_input_tokens: Some(usage.cached_input_tokens),
                        },
                    },
                )?;
                crate::run_store::RunRecords::sync_through(records, lsn)?;
                Ok((
                    true,
                    crate::run_driver::accounting::rows(records.state(), &budgets)?,
                ))
            });
        }
        let now = self.config.clock.now_ms();
        self.write_execution(|tx| {
            crate::tokens::settle(
                tx,
                crate::tokens::Key::SendId(send_id),
                crate::tokens::Report::Usage(usage),
                now,
            )
        })
    }

    /// What a flow's token window counts. `at_ms` picks the window; the
    /// window length comes from the flow's approved manifest.
    pub fn token_usage(
        &self,
        flow_id: &str,
        at_ms: i64,
    ) -> Result<Option<crate::tokens::WindowUsage>> {
        if self.uses_run_log() {
            return self.log_token_usage(flow_id, at_ms);
        }
        self.shared.store.read(|c| {
            let Some(approved) = crate::install::approved(c, flow_id)? else {
                return Ok(None);
            };
            let manifest = crate::manifest::Manifest::parse_stored(&approved.manifest)
                .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            let Some(window_ms) = manifest.token_window_ms() else {
                return Ok(None);
            };
            let start = crate::tokens::window_start(at_ms, window_ms);
            crate::tokens::window_usage(c, flow_id, window_ms, start).map(Some)
        })
    }

    /// Current token windows for approved flows, read in one SQL statement with
    /// live accounting merged under sorted flow locks. Unknown flows and flows
    /// without token grants are omitted.
    pub fn token_usage_many(
        &self,
        flow_ids: &[&str],
        at_ms: i64,
    ) -> Result<std::collections::BTreeMap<String, crate::tokens::WindowUsage>> {
        self.log_token_usage_many(flow_ids, at_ms)
    }
}

#[cfg(test)]
#[path = "grant_loss_tests.rs"]
mod grant_loss_tests;
