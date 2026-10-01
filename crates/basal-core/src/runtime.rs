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

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use basal_host::{
    CallRequest, Completion, CompletionAck, CompletionSink, Dispatched, Host, HostOutcome,
    SinkError, TransportError,
};
use basal_proto::{Budgets, CallKind, JsonText};
use serde_json::json;

use crate::admission::{self, Admission, TriggerSpec};
use crate::channel::WorkerSource;
use crate::error::{CoreError, Result};
use crate::hooks::{Boundary, Hooks, Step};
use crate::journal::{self, Source};
use crate::model::{CallRow, Run, RunState, StoredClass};
use crate::runs::{self, Lease};
use crate::store::Store;

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
    pub retry_backoff: Duration,
    /// After this many activations end with a broken worker, the run fails.
    pub max_broken_activations: u32,
    /// A run made runnable by a completion is resumed at once on a worker
    /// from the source.
    pub auto_resume: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            owner: format!("basal-{}", std::process::id()),
            budgets: Budgets::default(),
            activation_deadline: Duration::from_secs(60),
            unavailable_retries: 3,
            retry_backoff: Duration::from_millis(5),
            max_broken_activations: 3,
            auto_resume: false,
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
    /// worker broke and the run will be replayed on another.
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
}

/// A wake-up counter: bumped whenever anything a blocked activation might
/// wait for is committed.
pub(crate) struct Signal {
    epoch: Mutex<u64>,
    cond: Condvar,
}

impl Signal {
    fn lock(&self) -> MutexGuard<'_, u64> {
        self.epoch.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn current(&self) -> u64 {
        *self.lock()
    }

    pub(crate) fn bump(&self) {
        *self.lock() += 1;
        self.cond.notify_all();
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
    pub(crate) store: Arc<Store>,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) hooks: Arc<dyn Hooks>,
    pub(crate) source: Option<Arc<dyn WorkerSource>>,
    /// Calls being dispatched by a thread of this process. A recovering
    /// activation never sends these again: their thread will record the
    /// outcome.
    pub(crate) inflight: Mutex<HashSet<(String, u64)>>,
    pub(crate) signal: Signal,
    threads: Mutex<Vec<JoinHandle<()>>>,
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
    fn complete(&self, c: &Completion) -> std::result::Result<CompletionAck, SinkError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| SinkError("the runtime is gone".into()))?;
        let rt = Runtime {
            shared,
            config: self.config.clone(),
        };
        rt.accept(
            &c.run_id,
            c.position,
            Some(&c.handle),
            &c.outcome,
            Source::Completion,
        )
        .map_err(|e| SinkError(e.to_string()))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The rejection recorded for a call the host could not be reached for,
/// when the call provably had no effect.
fn unavailable_rejection(detail: &str) -> HostOutcome {
    HostOutcome::rejected(
        JsonText::new(
            json!({"message": "host unavailable", "code": "unavailable", "detail": detail})
                .to_string(),
        )
        .unwrap_or_else(|_| JsonText::null()),
    )
}

impl Runtime {
    pub fn new(
        store: Arc<Store>,
        host: Arc<dyn Host>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
    ) -> Self {
        let shared = Arc::new(Shared {
            store,
            host,
            hooks,
            source,
            inflight: Mutex::new(HashSet::new()),
            signal: Signal {
                epoch: Mutex::new(0),
                cond: Condvar::new(),
            },
            threads: Mutex::new(Vec::new()),
        });
        let config = Arc::new(config);
        shared.host.attach(Arc::new(Sink {
            shared: Arc::downgrade(&shared),
            config: config.clone(),
        }));
        Self { shared, config }
    }

    /// Another handle on the same store and process state with a different
    /// owner identity, standing in for a second owner taking runs over.
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

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Calls `hooks` at a boundary; a crash cuts the store.
    pub(crate) fn at(&self, run_id: &str, boundary: Boundary) -> Result<()> {
        match self.shared.hooks.at(run_id, &boundary) {
            Step::Continue => Ok(()),
            Step::Crash => {
                self.shared.store.cut();
                Err(CoreError::Cut)
            }
        }
    }

    // ---- Admission ---------------------------------------------------------

    pub fn admit(&self, spec: &TriggerSpec) -> Result<Admission> {
        let store_id = self.shared.store.store_id().to_owned();
        self.shared
            .store
            .write(|tx| admission::admit(tx, &store_id, spec))
    }

    pub fn retrigger(&self, run_id: &str) -> Result<Admission> {
        let store_id = self.shared.store.store_id().to_owned();
        self.shared
            .store
            .write(|tx| admission::retrigger(tx, &store_id, run_id))
    }

    // ---- Reading -----------------------------------------------------------

    pub fn run(&self, run_id: &str) -> Result<Run> {
        self.shared.store.read(|c| runs::load(c, run_id))
    }

    pub fn calls(&self, run_id: &str) -> Result<Vec<CallRow>> {
        self.shared.store.read(|c| journal::rows(c, run_id))
    }

    pub fn activation_count(&self, run_id: &str) -> Result<u64> {
        self.shared
            .store
            .read(|c| runs::activation_count(c, run_id))
    }

    // ---- Ownership ---------------------------------------------------------

    /// Moves every `running` run back to `pending`. Called once when the
    /// process starts: the store's lease proves their owners are gone.
    pub fn recover(&self) -> Result<Vec<String>> {
        let ids = self.shared.store.write(runs::requeue_orphans)?;
        self.shared.signal.bump();
        Ok(ids)
    }

    /// Takes a running run from the activation holding `generation`.
    pub fn take_over(&self, run_id: &str, generation: u64) -> Result<Lease> {
        let owner = self.config.owner.clone();
        let lease = self
            .shared
            .store
            .write(|tx| runs::take_over(tx, run_id, &owner, generation))?;
        self.shared.signal.bump();
        Ok(lease)
    }

    pub fn cancel(&self, run_id: &str, actor: &str) -> Result<()> {
        self.shared.store.write(|tx| {
            runs::cancel(tx, run_id, &format!("cancelled by {actor}"))?;
            crate::reconcile::audit(tx, actor, "cancel", Some(run_id), None, "")
        })?;
        self.shared.signal.bump();
        Ok(())
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
            let state = self.shared.store.read(|c| runs::state(c, run_id))?;
            let now = Instant::now();
            if done(state) || now >= deadline {
                return Ok(state);
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

    pub(crate) fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        let handle = thread::spawn(f);
        lock(&self.shared.threads).push(handle);
    }

    // ---- Outcomes ----------------------------------------------------------

    /// Accepts a long-running call's completion. The same entry point the
    /// host's completion sink uses; callers that learn of completions
    /// another way (a subscription replayed after a restart) use it
    /// directly.
    pub fn complete(&self, completion: &Completion) -> Result<CompletionAck> {
        self.accept(
            &completion.run_id,
            completion.position,
            Some(&completion.handle),
            &completion.outcome,
            Source::Completion,
        )
    }

    /// Offers an outcome for a call, from any thread and any activation.
    pub(crate) fn accept(
        &self,
        run_id: &str,
        position: u64,
        handle: Option<&str>,
        outcome: &HostOutcome,
        source: Source,
    ) -> Result<CompletionAck> {
        let accepted = self
            .shared
            .store
            .write(|tx| journal::accept_outcome(tx, run_id, position, handle, outcome, source))?;
        self.at(run_id, Boundary::OutcomeCommitted { position })?;
        self.shared.signal.bump();
        if accepted.woke {
            self.wake(run_id);
        }
        Ok(accepted.ack)
    }

    /// Starts an activation for a run made runnable, if configured to.
    pub(crate) fn wake(&self, run_id: &str) {
        if !self.config.auto_resume || self.shared.source.is_none() {
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
                    self.resume(run_id)?;
                }
                _ => self
                    .shared
                    .signal
                    .wait(seen, (deadline - now).min(Duration::from_millis(50))),
            }
        }
    }

    // ---- Dispatch ----------------------------------------------------------

    /// Registers a call as being dispatched by this process. Done before the
    /// call's row (or its resend authorization) commits, so a recovering
    /// activation never sends a call this process is about to send.
    pub(crate) fn register(&self, run_id: &str, position: u64) {
        lock(&self.shared.inflight).insert((run_id.to_owned(), position));
    }

    pub(crate) fn unregister(&self, run_id: &str, position: u64) {
        lock(&self.shared.inflight).remove(&(run_id.to_owned(), position));
    }

    pub(crate) fn is_inflight(&self, run_id: &str, position: u64) -> bool {
        lock(&self.shared.inflight).contains(&(run_id.to_owned(), position))
    }

    /// Dispatches an authorized call on its own thread.
    pub(crate) fn spawn_dispatch(&self, request: CallRequest, class: StoredClass) {
        let rt = self.clone();
        self.spawn(move || {
            let run_id = request.run_id.clone();
            let position = request.position;
            // Errors here mean the store failed or was cut: the outcome is
            // not recorded, and recovery decides what to do with the call.
            let _ = rt.dispatch(request, class);
            rt.unregister(&run_id, position);
            rt.shared.signal.bump();
        });
    }

    fn dispatch(&self, mut request: CallRequest, class: StoredClass) -> Result<()> {
        let run_id = request.run_id.clone();
        let position = request.position;
        let mut retries = 0;
        let mut maybe_sent = false;
        let answer = loop {
            match self.shared.host.dispatch(&request) {
                Ok(d) => break Ok(d),
                Err(TransportError::Unavailable {
                    proven_unsent,
                    detail,
                }) => {
                    maybe_sent |= !proven_unsent;
                    // A transient failure is retried inside the call only
                    // when a second send cannot cause a second effect.
                    let may_retry = class.safe_to_resend() || proven_unsent;
                    if may_retry && retries < self.config.unavailable_retries {
                        retries += 1;
                        request.attempt = request.attempt.saturating_add(1);
                        thread::sleep(self.config.retry_backoff);
                        continue;
                    }
                    break Err(detail);
                }
            }
        };
        self.at(&run_id, Boundary::HostAnswered { position })?;
        match answer {
            Ok(Dispatched::Completed(outcome)) => {
                self.accept(&run_id, position, None, &outcome, Source::Host)?;
            }
            Ok(Dispatched::Accepted { handle }) => {
                self.shared
                    .store
                    .write(|tx| journal::record_accepted(tx, &run_id, position, &handle))?;
                self.at(&run_id, Boundary::AcceptedCommitted { position })?;
            }
            Err(detail) if class == StoredClass::Query || !maybe_sent => {
                // Nothing can have happened remotely: a query has no effect,
                // and every failed send of this call provably never left.
                self.accept(
                    &run_id,
                    position,
                    None,
                    &unavailable_rejection(&detail),
                    Source::Host,
                )?;
            }
            Err(_) => {
                self.shared
                    .store
                    .write(|tx| journal::record_unknown(tx, &run_id, position))?;
            }
        }
        Ok(())
    }

    /// The request for a journaled call.
    pub(crate) fn request(run: &Run, row: &CallRow, attempt: u32) -> CallRequest {
        CallRequest {
            flow_id: run.flow_id.clone(),
            run_id: run.run_id.clone(),
            position: row.position,
            kind: row.kind.clone(),
            args: row.args.clone(),
            idempotency_key: row.idempotency_key.clone(),
            attempt,
        }
    }

    pub(crate) fn class_of(&self, kind: &CallKind) -> StoredClass {
        if kind.is_synchronous() {
            return StoredClass::Sync;
        }
        if crate::local::is_local(kind) {
            return StoredClass::Local;
        }
        match self.shared.host.classify(kind) {
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
        self.shared
            .store
            .write(|tx| admission::set_draining(tx, true))?;
        let health = self.health()?;
        Ok(health.unfinished)
    }

    pub fn undrain(&self) -> Result<()> {
        self.shared
            .store
            .write(|tx| admission::set_draining(tx, false))
    }

    pub fn health(&self) -> Result<crate::ops::Health> {
        self.shared.store.read(crate::ops::health)
    }
}
