//! The loops: tick the scheduler, enforce run deadlines, and drive runnable
//! runs on pool workers.
//!
//! All decisions about time read the runtime's clock, which a test sets; the
//! production loop waits for a timer or a coalesced work signal. Each
//! pass:
//! 1. ticks the scheduler, which plans due fires and admits them as runs;
//! 2. fails runs past their wall-clock deadline;
//! 3. lets the pool retire idle workers and top up its spares;
//! 4. raises the operator decision cards not yet accepted by the consent
//!    plane;
//! 5. starts an activation, each on its own thread, for every run that can
//!    start now (at most one per flow, since a flow's runs hold its slot one
//!    at a time), up to the configured number of activations at once.
//!
//! A run resumed by a completion, or requeued after its worker was killed,
//! is simply pending again and is picked up by the next pass.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use basal_core::schedule::Scheduler;
use basal_core::{ActivationEnd, CoreError, RevokeCause, Runtime};
use basal_host::Consent;

use crate::fatal::Fatal;
use crate::metrics::Metrics;
use crate::pool::{Binding, Pool, PoolError};

/// A missed notification must not strand work indefinitely. This is a safety
/// net, not the normal scheduler cadence: idle stores need no frequent writes.
pub const FALLBACK_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Wake {
    state: Mutex<(u64, bool)>,
    cond: Condvar,
}

impl Wake {
    fn notify(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.0 = state.0.wrapping_add(1);
        self.cond.notify_all();
    }

    fn snapshot(&self) -> (u64, bool) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wait(&self, seen: u64, timeout: Duration) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .cond
            .wait_timeout_while(state, timeout, |s| s.0 == seen && !s.1);
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Activations running at once, across all flows.
    pub max_concurrent_activations: usize,
    /// Maximum silence between passes. Timers and work signals wake earlier.
    pub pass_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_activations: 4,
            pass_interval: FALLBACK_INTERVAL,
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Runs the scheduler admitted.
    pub admitted: Vec<String>,
    /// Runs failed for passing their deadline.
    pub expired: Vec<String>,
    /// Runs an activation was started for.
    pub started: Vec<String>,
}

#[derive(Default)]
struct Active {
    runs: HashSet<String>,
    flows: HashSet<String>,
}

struct Inner {
    rt: Runtime,
    pool: Pool,
    consent: Arc<dyn Consent>,
    scheduler: Scheduler,
    config: EngineConfig,
    metrics: Arc<Metrics>,
    fatal: Fatal,
    active: Mutex<Active>,
    cond: Condvar,
    /// Bumped whenever an activation ran: any ending except finding the run
    /// not pending or its flow's slot held by an earlier run, the two that
    /// leave everything as it was.
    progress: AtomicU64,
    wake: Arc<Wake>,
    // None prevents new engine threads. Register under this lock so shutdown
    // cannot miss a thread between its spawn and its handle being saved.
    threads: Mutex<Option<Vec<thread::JoinHandle<()>>>>,
    #[cfg(test)]
    activation_exit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    joining: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Release both reservations even if a host, hook or spawner unwinds. A panic
/// may have left a durable claim behind, so supervision must run recovery.
struct ActivationSlot<'a> {
    engine: &'a Engine,
    run_id: &'a str,
    flow_id: &'a str,
    notify: bool,
}

impl Drop for ActivationSlot<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.engine
                .inner
                .fatal
                .raise(format!("activation of {} panicked", self.run_id));
        }
        let mut active = self.engine.active();
        active.runs.remove(self.run_id);
        active.flows.remove(self.flow_id);
        self.engine.inner.cond.notify_all();
        if self.notify {
            self.engine.inner.wake.notify();
        }
    }
}

/// The engine. Cloning shares it.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

/// A loop observer. The engine owns the actual thread handle so module
/// shutdown can join it even when its caller discards this observer.
pub struct EngineLoop {
    done: std::sync::mpsc::Receiver<thread::Result<()>>,
}

#[cfg(test)]
struct ActivationExit<'a>(&'a Engine);

#[cfg(test)]
impl Drop for ActivationExit<'_> {
    fn drop(&mut self) {
        if let Some(exit) = self
            .0
            .inner
            .activation_exit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            exit();
        }
    }
}

impl EngineLoop {
    pub fn join(self) -> thread::Result<()> {
        self.done.recv().expect("engine loop completion was lost")
    }
}

impl Engine {
    pub fn new(
        rt: Runtime,
        pool: Pool,
        consent: Arc<dyn Consent>,
        config: EngineConfig,
        metrics: Arc<Metrics>,
        fatal: Fatal,
    ) -> Result<Self, CoreError> {
        let scheduler = rt.scheduler()?;
        Ok(Self::with_scheduler(
            rt, pool, consent, config, metrics, fatal, scheduler,
        ))
    }

    /// Startup can validate the scheduler before binding adapters, then attach
    /// the real runtime without leaving any fallible preparation afterward.
    pub(crate) fn with_scheduler(
        rt: Runtime,
        pool: Pool,
        consent: Arc<dyn Consent>,
        config: EngineConfig,
        metrics: Arc<Metrics>,
        fatal: Fatal,
        scheduler: Scheduler,
    ) -> Self {
        let wake = Arc::new(Wake::default());
        let weak = Arc::downgrade(&wake);
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(wake) = weak.upgrade() {
                wake.notify();
            }
        });
        rt.on_work(notify.clone());
        pool.on_change(notify);
        Self {
            inner: Arc::new(Inner {
                rt,
                pool,
                consent,
                scheduler,
                config,
                metrics,
                fatal,
                active: Mutex::new(Active::default()),
                cond: Condvar::new(),
                progress: AtomicU64::new(0),
                wake,
                threads: Mutex::new(Some(Vec::new())),
                #[cfg(test)]
                activation_exit: Mutex::new(None),
                #[cfg(test)]
                joining: Mutex::new(None),
            }),
        }
    }

    fn active(&self) -> MutexGuard<'_, Active> {
        self.inner.active.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Activations in progress.
    pub fn active_count(&self) -> usize {
        self.active().runs.len()
    }

    /// One pass. A storage error raises the fatal latch and is returned.
    pub fn pass(&self) -> Result<PassReport, String> {
        let inner = &self.inner;
        if let Some(why) = inner.fatal.get() {
            return Err(why);
        }
        let fatal = |e: CoreError| {
            // These calls take no input but the store's own contents, so an
            // error means the store failed or holds something this build
            // cannot read. Serving on cannot fix either; a restart runs
            // recovery.
            let why = e.to_string();
            inner.fatal.raise(why.clone());
            why
        };
        inner.rt.poll_lost_grants().map_err(fatal)?;
        inner.rt.drain_event_backlog().map_err(fatal)?;
        let tick = inner.scheduler.tick().map_err(fatal)?;
        let admitted: Vec<String> = tick.new_runs().into_iter().map(str::to_owned).collect();
        let expired = inner.rt.enforce_deadlines().map_err(fatal)?;
        inner.pool.maintain();
        self.raise_decisions().map_err(fatal)?;
        let mut started = Vec::new();
        let mut threads = inner.threads.lock().unwrap_or_else(|p| p.into_inner());
        let Some(threads) = threads.as_mut() else {
            return Ok(PassReport {
                admitted,
                expired,
                started,
            });
        };
        for index in (0..threads.len()).rev() {
            if threads[index].is_finished() {
                threads
                    .swap_remove(index)
                    .join()
                    .expect("engine thread failed outside its panic boundary");
            }
        }
        for (run_id, flow_id) in inner.rt.startable().map_err(fatal)? {
            {
                let mut active = self.active();
                if active.runs.len() >= inner.config.max_concurrent_activations {
                    break;
                }
                if active.flows.contains(&flow_id) || active.runs.contains(&run_id) {
                    continue;
                }
                active.runs.insert(run_id.clone());
                active.flows.insert(flow_id.clone());
            }
            started.push(run_id.clone());
            threads.push(
                self.spawn_task(move |engine| {
                    // The slot is released inside activate, before the captured
                    // engine is dropped. Empty run/flow slots are not thread exit.
                    #[cfg(test)]
                    let _exit = ActivationExit(engine);
                    engine.activate(run_id, flow_id);
                })
                .0,
            );
        }
        inner.cond.notify_all();
        Ok(PassReport {
            admitted,
            expired,
            started,
        })
    }

    /// Raises every decision card whose latest revision the consent plane
    /// has not accepted. The card's row was committed before this, so a
    /// crash or an unreachable consent plane only delays it: the next pass
    /// raises it again under the same deduplication key on the recorded path.
    /// A store error is returned; a consent error is logged and retried next pass.
    fn raise_decisions(&self) -> Result<(), CoreError> {
        let inner = &self.inner;
        for (path, id) in inner.rt.decision_withdrawals_on()? {
            match inner.consent.withdraw_decision_on(&path, &id) {
                Ok(()) => inner.rt.decision_withdrawn_on(&path, &id)?,
                Err(e) => {
                    tracing::warn!(target:"consent", elicitation_id=%id, "withdrawing a decision card: {e}")
                }
            }
        }
        for record in inner.rt.decisions_due()? {
            let card = record.to_card()?;
            let path = match &record.consent_path {
                Some(path) => path.clone(),
                None => match inner.consent.decision_path() {
                    Ok(path) => inner.rt.select_decision_path(record.seq, &path)?,
                    Err(e) => {
                        tracing::warn!(target: "consent", key = %record.dedup_key, "selecting a decision provider: {e}");
                        continue;
                    }
                },
            };
            match inner.consent.raise_decision_on(&path, &card) {
                Ok(elicitation_id) => {
                    inner
                        .rt
                        .decision_raised(record.seq, record.revision, &elicitation_id)?;
                }
                Err(e) => {
                    tracing::warn!(target: "consent", key = %record.dedup_key, "raising a decision card: {e}");
                }
            }
        }
        Ok(())
    }

    /// One activation of `run_id`, on its own thread.
    fn activate(&self, run_id: String, flow_id: String) {
        let mut slot = ActivationSlot {
            engine: self,
            run_id: &run_id,
            flow_id: &flow_id,
            notify: true,
        };
        let inner = &self.inner;
        let replayed = match journal_rows(&inner.rt, &run_id) {
            Ok(n) => n,
            Err(e) => {
                inner
                    .fatal
                    .raise(format!("reading replay size for {run_id}: {e}"));
                return;
            }
        };
        let started = Instant::now();
        let changed = match self.drive_on_pool(&run_id, &flow_id) {
            None => {
                // No worker was available. A failed spawn has already signalled
                // its retry time, and a pool that is backing off has no new
                // capacity, so waking the loop now would only spin.
                slot.notify = false;
                false
            }
            Some(Err(e)) => {
                // `Runtime::activate` returns an error only when claiming
                // the run failed in the store; everything after the claim
                // is reported as an ending.
                inner.fatal.raise(format!("activating {run_id}: {e}"));
                false
            }
            Some(Ok(end)) => {
                let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                if !matches!(
                    end,
                    ActivationEnd::Waiting { .. }
                        | ActivationEnd::NotRunnable { .. }
                        | ActivationEnd::Deferred { .. }
                        | ActivationEnd::Revoked { .. }
                ) {
                    inner.metrics.activation(replayed, micros);
                }
                self.after(&run_id, end)
            }
        };
        if changed {
            inner.progress.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Drives the run on a worker bound to its flow. The worker is acquired
    /// first and the run claimed after, so the run's wall-clock deadline,
    /// fixed at its first claim, never counts the time a spawn and its
    /// handshake took (a launch can stall for over a minute on macOS).
    /// `None` when no worker could be had; the run stays pending.
    fn drive_on_pool(
        &self,
        run_id: &str,
        flow_id: &str,
    ) -> Option<Result<ActivationEnd, CoreError>> {
        let mut lease = match self.inner.pool.acquire(Binding::Flow(flow_id.to_owned())) {
            Ok(lease) => lease,
            Err(PoolError::Backoff { .. }) => return None,
            Err(e) => {
                tracing::warn!(target: "engine", run = %run_id, "no worker for the run: {e}");
                return None;
            }
        };
        lease.serve_run(run_id);
        Some(self.inner.rt.activate(run_id, &mut lease))
    }

    /// Handles an activation's end; returns whether anything changed.
    fn after(&self, run_id: &str, end: ActivationEnd) -> bool {
        match end {
            // The driver turns every error inside an activation, other than
            // a lost ownership, into this ending. Each one left the run
            // `running` with no activation driving it, and only recovery at
            // the next start puts it back in line.
            ActivationEnd::Failed { kind, detail } if kind == "store" => {
                self.inner
                    .fatal
                    .raise(format!("activation of {run_id}: {detail}"));
                false
            }
            // The store was cut under the activation (`Store::cut`, a
            // simulated storage failure): every later write fails.
            ActivationEnd::Crashed => {
                self.inner
                    .fatal
                    .raise(format!("activation of {run_id}: the store was cut"));
                false
            }
            ActivationEnd::Waiting { .. } | ActivationEnd::NotRunnable { .. } => false,
            // Core gave the install gate no answer: the run is pending as it
            // was and waits out a backoff, during which `Runtime::startable`
            // does not offer it, so nothing changed.
            ActivationEnd::Deferred { detail, retry_in } => {
                tracing::warn!(
                    target: "engine",
                    run = %run_id,
                    "not activated, asking core again in {retry_in:?}: {detail}"
                );
                false
            }
            ActivationEnd::Revoked {
                version,
                cause: RevokeCause::HashMismatch { core, run },
                detail,
            } => {
                // basal and core disagree about what was approved: never
                // expected, so it is reported as loudly as the engine can.
                tracing::error!(
                    target: "engine",
                    run = %run_id,
                    version,
                    core_code_hash = %core,
                    run_code_hash = %run,
                    "core approved other code for this version; the version is revoked and the run cancelled: {detail}"
                );
                true
            }
            ActivationEnd::Revoked {
                version, detail, ..
            } => {
                tracing::warn!(target: "engine", run = %run_id, version, "{detail}");
                true
            }
            ActivationEnd::Failed { kind, detail } => {
                tracing::info!(target: "engine", run = %run_id, kind = %kind, "run failed: {detail}");
                true
            }
            other => {
                tracing::debug!(target: "engine", run = %run_id, "activation ended: {other:?}");
                true
            }
        }
    }

    /// Runs passes until nothing more happens: no run admitted, expired or
    /// changed by an activation, and no activation in progress. For tests
    /// and the harness; production uses [`Engine::spawn_loop`].
    pub fn run_until_idle(&self, max_passes: usize) -> Result<(), String> {
        for _ in 0..max_passes {
            let before = self.inner.progress.load(Ordering::SeqCst);
            let report = self.pass()?;
            self.wait_quiet();
            if let Some(why) = self.inner.fatal.get() {
                return Err(why);
            }
            let after = self.inner.progress.load(Ordering::SeqCst);
            if report.admitted.is_empty() && report.expired.is_empty() && before == after {
                return Ok(());
            }
        }
        Err(format!("still busy after {max_passes} passes"))
    }

    /// Waits until no activation is in progress.
    pub fn wait_quiet(&self) {
        let mut active = self.active();
        while !active.runs.is_empty() {
            active = self
                .inner
                .cond
                .wait(active)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Waits until an activation is in progress for `run_id`, up to
    /// `timeout`. For tests that act while a run is being driven.
    pub fn wait_active(&self, run_id: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut active = self.active();
        loop {
            if active.runs.contains(run_id) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            active = self
                .inner
                .cond
                .wait_timeout(active, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }

    /// Stops the loop even when no timer or store work is due.
    pub fn stop(&self) {
        let mut state = self
            .inner
            .wake
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        state.1 = true;
        self.inner.wake.cond.notify_all();
    }

    pub(crate) fn join_threads(&self) {
        self.stop();
        let handles = self
            .inner
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        #[cfg(test)]
        if let Some(joining) = self
            .inner
            .joining
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            joining();
        }
        for handle in handles.into_iter().flatten() {
            handle
                .join()
                .expect("engine thread failed outside its panic boundary");
        }
    }

    fn spawn_task(
        &self,
        f: impl FnOnce(&Engine) + Send + 'static,
    ) -> (thread::JoinHandle<()>, EngineLoop) {
        let engine = self.clone();
        let (done, receiver) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&engine)));
            if result.is_err() {
                engine.inner.fatal.raise("engine thread panicked");
            }
            drop(engine);
            let _ = done.send(result);
        });
        (handle, EngineLoop { done: receiver })
    }

    fn next_wait(&self) -> Result<Duration, CoreError> {
        let next = self
            .inner
            .rt
            .next_wake_at(self.inner.pool.next_maintenance_at())?;
        // Querying can wait behind a writer; do not add that wait to the
        // remaining time of a deadline that was already armed.
        let now = self.inner.rt.config().clock.now_ms();
        Ok(next.map_or(self.inner.config.pass_interval, |at| {
            Duration::from_millis(at.saturating_sub(now).max(0) as u64)
                .min(self.inner.config.pass_interval)
        }))
    }

    /// The production loop: work signals and the earliest timer, with a slow
    /// fallback. Capture the epoch before the pass so a commit between the
    /// scan and the wait cannot be lost.
    pub fn spawn_loop(&self) -> EngineLoop {
        let mut threads = self.inner.threads.lock().unwrap_or_else(|p| p.into_inner());
        let threads = threads.as_mut().expect("cannot start a stopped engine");
        let (handle, observer) = self.spawn_task(|engine| {
            engine.run_loop(|seen, wait| engine.inner.wake.wait(seen, wait));
        });
        threads.push(handle);
        observer
    }

    fn run_loop(&self, mut wait: impl FnMut(u64, Duration)) {
        let engine = self;
        loop {
            let (seen, stopped) = engine.inner.wake.snapshot();
            if stopped {
                return;
            }
            if let Err(why) = engine.pass() {
                tracing::error!(target: "engine", "the engine stopped: {why}");
                return;
            }
            match engine.next_wait() {
                Ok(timeout) => wait(seen, timeout),
                Err(e) => {
                    engine
                        .inner
                        .fatal
                        .raise(format!("reading the next engine wake: {e}"));
                    return;
                }
            }
        }
    }
}

/// How many calls the run has journaled: what its next activation replays.
fn journal_rows(rt: &Runtime, run_id: &str) -> Result<u64, CoreError> {
    rt.store().read(|c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM journal WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        u64::try_from(n).map_err(|_| CoreError::Corrupt("negative journal count".into()))
    })
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod lifecycle_tests;
