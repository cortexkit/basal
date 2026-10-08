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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use basal_host::{
    CallRequest, Catalog, Completion, CompletionAck, CompletionSink, Dispatched, Host, HostOutcome,
    Sent, SinkError, TransportError, UnknownOutcome, UnknownReason,
};
use basal_proto::{Budgets, CallKind, JsonText};
use serde_json::json;

use crate::admission::{self, Admission, AdmitContext, TriggerSpec};
use crate::authorize::ShellDenylist;
use crate::channel::WorkerSource;
use crate::clock::Clock;
use crate::error::{CoreError, Result};
use crate::hooks::{Boundary, Hooks, Step};
use crate::journal::{self, Source};
use crate::kv::KvLimits;
use crate::model::{CallRow, Run, RunState, StoredClass};
use crate::rate::RateLimits;
use crate::runs::{self, Lease};
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
    pub(crate) catalog: Arc<dyn Catalog>,
    pub(crate) hooks: Arc<dyn Hooks>,
    pub(crate) source: Option<Arc<dyn WorkerSource>>,
    /// Calls being dispatched by a thread of this process. A recovering
    /// activation never sends these again: their thread will record the
    /// outcome.
    pub(crate) inflight: Mutex<HashSet<(String, u64)>>,
    /// Runs whose install gate got no answer from core, with when they may
    /// be checked again. In memory: a restart checks every run at once.
    pub(crate) gate_backoff: Mutex<HashMap<String, crate::gate::Backoff>>,
    pub(crate) signal: Signal,
    threads: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) retention_next: Mutex<Option<i64>>,
    pub(crate) refusal_flush: Mutex<()>,
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
        catalog: Arc<dyn Catalog>,
        hooks: Arc<dyn Hooks>,
        source: Option<Arc<dyn WorkerSource>>,
        config: Config,
    ) -> Self {
        store.set_clock(config.clock.clone());
        let shared = Arc::new(Shared {
            store,
            host,
            catalog,
            hooks,
            source,
            inflight: Mutex::new(HashSet::new()),
            gate_backoff: Mutex::new(HashMap::new()),
            signal: Signal {
                epoch: Mutex::new(0),
                cond: Condvar::new(),
            },
            threads: Mutex::new(Vec::new()),
            retention_next: Mutex::new(None),
            refusal_flush: Mutex::new(()),
        });
        let config = Arc::new(config);
        shared
            .host
            .scope_checks(config.install_gate == crate::gate::InstallGate::Core);
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
        let store_id = self.shared.store.store_id().to_owned();
        let ctx = self.admit_context()?;
        self.shared
            .store
            .write(|tx| admission::admit(tx, &store_id, spec, &ctx))
    }

    /// Admits a trigger under the flow's currently approved version.
    pub fn admit_trigger(
        &self,
        flow_id: &str,
        trigger_id: &str,
        trigger: JsonText,
    ) -> Result<Admission> {
        let store_id = self.shared.store.store_id().to_owned();
        let ctx = self.admit_context()?;
        self.shared
            .store
            .write(|tx| admission::admit_current(tx, &store_id, flow_id, trigger_id, trigger, &ctx))
    }

    /// A scheduler over this runtime's store that reads this runtime's
    /// clock and admits fires under its admission limits.
    pub fn scheduler(&self) -> Result<crate::schedule::Scheduler> {
        let ctx = self.admit_context()?;
        Ok(crate::schedule::Scheduler::new(
            self.shared.store.clone(),
            Arc::new(self.config.clock.clone()),
            self.config.schedule.clone(),
        )
        .with_limits(crate::schedule::AdmitLimits {
            rate: ctx.rate,
            default_deadline_ms: ctx.default_deadline_ms,
        }))
    }

    pub fn retrigger(&self, run_id: &str) -> Result<Admission> {
        let store_id = self.shared.store.store_id().to_owned();
        let ctx = self.admit_context()?;
        self.shared
            .store
            .write(|tx| admission::retrigger(tx, &store_id, run_id, &ctx))
    }

    // ---- Reading -----------------------------------------------------------

    pub fn run(&self, run_id: &str) -> Result<Run> {
        self.shared.store.read(|c| runs::load(c, run_id))
    }

    pub fn calls(&self, run_id: &str) -> Result<Vec<CallRow>> {
        self.shared.store.read(|c| journal::rows(c, run_id))
    }

    /// Why a call was last recorded unknown, if it ever was.
    pub fn unknown_reason(&self, run_id: &str, position: u64) -> Result<Option<UnknownReason>> {
        self.shared
            .store
            .read(|c| journal::unknown_reason(c, run_id, position))
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
        self.flush_refusals()?;
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
        self.flush_refusals()?;
        self.shared.signal.bump();
        if let Ok(run) = self.run(run_id) {
            self.wake_flow(&run.flow_id);
        }
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
            let saved: journal::SavedRefusal =
                serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
            self.shared
                .host
                .refusal_committed(&saved.request()?, &saved.refusal()?);
            self.store().write(|tx| {
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
        self.accept(
            &completion.run_id,
            completion.position,
            Some(&completion.handle),
            &completion.outcome,
            Source::Completion,
        )
    }

    /// Records a host's report that an accepted call's outcome cannot be
    /// established. The same entry point the host's completion sink uses.
    /// `None` means the run's activation is still running and the report
    /// was not recorded: the host must make it again later.
    pub fn unknown(&self, u: &UnknownOutcome) -> Result<Option<CompletionAck>> {
        let ack = self.shared.store.write(|tx| {
            journal::record_host_unknown(tx, &u.run_id, u.position, &u.handle, u.reason, &u.detail)
        })?;
        self.shared.signal.bump();
        Ok(ack)
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
        let capped = self.cap_result(outcome);
        let outcome = capped.as_ref().unwrap_or(outcome);
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
    pub(crate) fn spawn_dispatch(
        &self,
        request: CallRequest,
        class: StoredClass,
        prior_unsent: bool,
    ) {
        let rt = self.clone();
        self.spawn(move || {
            let run_id = request.run_id.clone();
            let position = request.position;
            // Errors here mean the store failed or was cut: the outcome is
            // not recorded, and recovery decides what to do with the call.
            let _ = rt.dispatch(request, class, prior_unsent);
            rt.unregister(&run_id, position);
            rt.shared.signal.bump();
        });
    }

    fn dispatch(
        &self,
        mut request: CallRequest,
        class: StoredClass,
        prior_unsent: bool,
    ) -> Result<()> {
        let run_id = request.run_id.clone();
        let position = request.position;
        let mut retries = 0;
        // Why the latest send that may have reached the host ended without
        // an answer; `None` while every failed send provably never left.
        let mut maybe_sent: Option<UnknownReason> =
            (!prior_unsent && class != StoredClass::Query).then_some(UnknownReason::BasalRestarted);
        let answer = loop {
            let expected_class = match class {
                StoredClass::Query => basal_host::CallClass::Query,
                _ => basal_host::CallClass::Mutation {
                    honours_idempotency_keys: class == StoredClass::KeyedMutation,
                },
            };
            match self
                .shared
                .host
                .dispatch_classified(&request, expected_class)
            {
                Ok(d) => break Ok(d),
                Err(TransportError::Refused(refusal)) => {
                    use basal_host::flow_refusal::RefusalReason;
                    if let Some(reason) = maybe_sent {
                        self.shared
                            .store
                            .write(|tx| journal::record_unknown(tx, &run_id, position, reason))?;
                    } else if refusal.reason == RefusalReason::FlowScopeRequired {
                        tracing::error!(provider=%refusal.provider,action=%refusal.action,code="flow_scope_required","provider refused a flow call without its required scope");
                        self.accept(&run_id, position, None, &refusal.outcome(), Source::Host)?;
                        self.shared.host.refusal_committed(&request, &refusal);
                        return Ok(());
                    } else if refusal.readiness() {
                        self.shared.store.write(|tx| {
                            let accepted = journal::accept_outcome(
                                tx, &run_id, position, None, &refusal.outcome(), Source::Host,
                            )?;
                            if accepted.ack == CompletionAck::Accepted {
                                crate::flow_scope::health(tx, &request.flow_id, &refusal)?;
                                let at: i64 = tx.query_row(
                                    "SELECT at FROM call_audit WHERE run_id=?1 AND position=?2",
                                    rusqlite::params![run_id,position],
                                    |r| r.get(0),
                                )?;
                                crate::rate::refund_dispatch(tx, &request.flow_id, at, &self.config.rate)?;
                                tx.execute(
                                    "UPDATE call_audit SET outcome=?3 WHERE run_id=?1 AND position=?2",
                                    rusqlite::params![run_id,position,refusal.reason.as_str()],
                                )?;
                            }
                            Ok(())
                        })?;
                        self.shared.host.refusal_committed(&request, &refusal);
                    } else if refusal.reason == RefusalReason::AgentRetired {
                        // A retired agent cannot authorize more calls. Fail the run
                        // and disable its flow together so recovery cannot retry it.
                        self.shared.store.write(|tx| {
                            let flow_id = &request.flow_id;
                            crate::install::disable(
                                tx,
                                flow_id,
                                &crate::install::Actor::Core,
                                "agent_retired",
                                self.config.clock.now_ms(),
                            )
                            .map_err(|e| CoreError::Invalid(e.to_string()))?;
                            journal::accept_outcome(
                                tx,
                                &run_id,
                                position,
                                None,
                                &crate::kv::rejection(
                                    "agent_retired",
                                    &format!("{} {}", refusal.provider, refusal.action),
                                ),
                                Source::Host,
                            )?;
                            runs::fail_external(
                                tx,
                                &run_id,
                                "agent_retired",
                                &format!("{} {}", refusal.provider, refusal.action),
                            )?;
                            Ok(())
                        })?;
                        self.flush_refusals()?;
                        self.shared.host.refusal_committed(&request, &refusal);
                        self.wake_flow(&request.flow_id);
                    } else {
                        let now = self.config.clock.now_ms();
                        let backoff = self
                            .config
                            .deferral_retry
                            .saturating_mul(1u32 << (request.attempt.saturating_sub(1)).min(16))
                            .min(self.config.deferral_retry_max);
                        let delay = refusal.retry_after_ms.unwrap_or_else(|| {
                            u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX)
                        });
                        // Missing consent must be restored by an explicit grant.
                        // i64::MAX prevents automatic retries before that grant.
                        let retry = if refusal.reason == RefusalReason::ConsentUnavailable {
                            i64::MAX
                        } else {
                            now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX))
                        };
                        self.shared.store.write(|tx| {
                            let deadline: Option<i64> = tx.query_row(
                                "SELECT deadline_at FROM runs WHERE run_id = ?1",
                                [&run_id],
                                |r| r.get(0),
                            )?;
                            let retry = if refusal.reason != RefusalReason::ConsentUnavailable {
                                deadline.map_or(retry, |d| retry.min(d))
                            } else {
                                retry
                            };
                            journal::defer(tx, &run_id, position, &refusal, retry)
                        })?;
                    }
                    self.shared.signal.bump();
                    return Ok(());
                }
                Err(TransportError::Unavailable { sent, detail }) => {
                    let proven_unsent = sent == Sent::Never;
                    if let Sent::Maybe(reason) = sent {
                        maybe_sent = Some(reason);
                    }
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
        match (answer, maybe_sent) {
            (Ok(Dispatched::Completed(outcome)), _) => {
                self.accept(&run_id, position, None, &outcome, Source::Host)?;
                self.shared.host.dispatch_committed(&request);
            }
            (Ok(Dispatched::Accepted { handle }), _) => {
                self.shared
                    .store
                    .write(|tx| journal::record_accepted(tx, &run_id, position, &handle))?;
                self.at(&run_id, Boundary::AcceptedCommitted { position })?;
                self.shared.host.dispatch_committed(&request);
            }
            (Err(_), Some(last)) if class != StoredClass::Query => {
                // A call that honours idempotency keys was sent again and
                // again; it is unknown because its retries ran out. Any
                // other is sent once, and is unknown for that send's reason.
                let reason = if class.safe_to_resend() {
                    UnknownReason::RetriesExhausted
                } else {
                    last
                };
                self.shared
                    .store
                    .write(|tx| journal::record_unknown(tx, &run_id, position, reason))?;
            }
            (Err(detail), _) => {
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
        if crate::kv::is_kv(kind) {
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

    /// The age of the oldest pending, suspended and `needs_reconcile` run,
    /// on the runtime's clock.
    pub fn run_ages(&self) -> Result<crate::ops::RunAges> {
        let now = self.config.clock.now_ms();
        self.shared.store.read(|c| crate::ops::run_ages(c, now))
    }

    /// Every flow's health, on the runtime's clock.
    pub fn flow_health(&self) -> Result<Vec<crate::ops::FlowHealth>> {
        let now = self.config.clock.now_ms();
        self.shared.store.read(|c| crate::ops::flow_health(c, now))
    }

    /// Pending runs no earlier run of their flow holds back, as (run,
    /// flow), in admission order: what can start now.
    pub fn startable(&self) -> Result<Vec<(String, String)>> {
        self.shared
            .store
            .write(|tx| journal::wake_deferred(tx, self.config.clock.now_ms(), None))?;
        let mut runs = self.shared.store.read(runs::startable)?;
        // A run waiting to ask core again is not offered, so a pass loop
        // cannot ask about it on every pass while core is unreachable.
        runs.retain(|(run_id, _)| self.gate_wait(run_id).is_none());
        Ok(runs)
    }

    pub fn catalog(&self) -> &dyn Catalog {
        self.shared.catalog.as_ref()
    }

    // ---- Slots and deadlines ----------------------------------------------

    /// Fails runs past their deadline, then services durable provider
    /// acknowledgements and retention on the runtime's own cadence.
    pub fn enforce_deadlines(&self) -> Result<Vec<String>> {
        let now = self.config.clock.now_ms();
        let expired = self.shared.store.write(|tx| runs::expire(tx, now, None))?;
        self.flush_refusals()?;
        self.retention_due(now)?;
        self.reap_threads();
        self.shared.signal.bump();
        for (_, flow_id) in &expired {
            self.wake_flow(flow_id);
        }
        Ok(expired.into_iter().map(|(run, _)| run).collect())
    }

    /// Wakes the next pending run of a flow, if configured to resume runs.
    pub(crate) fn wake_flow(&self, flow_id: &str) {
        if !self.config.auto_resume || self.shared.source.is_none() {
            return;
        }
        let next: Result<Option<String>> = self.shared.store.read(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT run_id FROM runs WHERE flow_id = ?1 AND state = 'pending' \
                 ORDER BY admit_seq LIMIT 1",
                [flow_id],
                |r| r.get(0),
            )
            .optional()?)
        });
        if let Ok(Some(run_id)) = next {
            self.wake(&run_id);
        }
    }

    // ---- Tokens ------------------------------------------------------------

    /// Records a model call's usage reported apart from its outcome (Broca
    /// reports usage with the run's end). Deduplicated by `send_id`: only
    /// the first report for a reservation is applied, whichever way it
    /// came. Returns whether this one was applied.
    pub fn report_usage(&self, send_id: &str, usage: crate::tokens::Usage) -> Result<bool> {
        let now = self.config.clock.now_ms();
        self.shared.store.write(|tx| {
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
        self.shared.store.read(|c| {
            let Some(approved) = crate::install::approved(c, flow_id)? else {
                return Ok(None);
            };
            let manifest = crate::manifest::Manifest::parse(&approved.manifest)
                .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            let Some(window_ms) = manifest.token_window_ms() else {
                return Ok(None);
            };
            let start = crate::tokens::window_start(at_ms, window_ms);
            crate::tokens::window_usage(c, flow_id, window_ms, start).map(Some)
        })
    }
}
