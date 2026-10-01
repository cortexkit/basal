//! The loops: tick the scheduler, enforce run deadlines, and drive runnable
//! runs on pool workers.
//!
//! All decisions about time read the runtime's clock, which a test sets; the
//! production loop only uses wall time to decide how often to look. Each
//! pass:
//! 1. ticks the scheduler, which plans due fires and admits them as runs;
//! 2. fails runs past their wall-clock deadline;
//! 3. lets the pool retire idle workers and top up its spares;
//! 4. starts an activation, each on its own thread, for every run that can
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
use basal_core::{ActivationEnd, CoreError, Runtime};

use crate::fatal::Fatal;
use crate::metrics::Metrics;
use crate::pool::{Binding, Pool};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Activations running at once, across all flows.
    pub max_concurrent_activations: usize,
    /// How often the production loop runs a pass.
    pub pass_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_activations: 4,
            pass_interval: Duration::from_millis(200),
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
    scheduler: Scheduler,
    config: EngineConfig,
    metrics: Arc<Metrics>,
    fatal: Fatal,
    active: Mutex<Active>,
    cond: Condvar,
    /// Bumped whenever an activation ends in a way that changed something
    /// (anything but finding the run not runnable or its slot taken).
    progress: AtomicU64,
}

/// The engine. Cloning shares it.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(
        rt: Runtime,
        pool: Pool,
        config: EngineConfig,
        metrics: Arc<Metrics>,
        fatal: Fatal,
    ) -> Result<Self, CoreError> {
        let scheduler = rt.scheduler()?;
        Ok(Self {
            inner: Arc::new(Inner {
                rt,
                pool,
                scheduler,
                config,
                metrics,
                fatal,
                active: Mutex::new(Active::default()),
                cond: Condvar::new(),
                progress: AtomicU64::new(0),
            }),
        })
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
            // Every core error here comes from the store: these calls take
            // no input but the store's own contents.
            let why = e.to_string();
            inner.fatal.raise(why.clone());
            why
        };
        let tick = inner.scheduler.tick().map_err(fatal)?;
        let admitted: Vec<String> = tick.new_runs().into_iter().map(str::to_owned).collect();
        let expired = inner.rt.enforce_deadlines().map_err(fatal)?;
        inner.pool.maintain();
        let mut started = Vec::new();
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
            let engine = self.clone();
            thread::spawn(move || engine.activate(run_id, flow_id));
        }
        inner.cond.notify_all();
        Ok(PassReport {
            admitted,
            expired,
            started,
        })
    }

    /// One activation of `run_id`, on its own thread.
    fn activate(&self, run_id: String, flow_id: String) {
        let inner = &self.inner;
        let replayed = journal_rows(&inner.rt, &run_id);
        let started = Instant::now();
        let changed = match self.drive_on_pool(&run_id, &flow_id) {
            None => false,
            Some(Err(e)) => {
                // The claim failed in the store.
                inner.fatal.raise(format!("activating {run_id}: {e}"));
                false
            }
            Some(Ok(end)) => {
                let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                if !matches!(
                    end,
                    ActivationEnd::Waiting { .. } | ActivationEnd::NotRunnable { .. }
                ) {
                    inner.metrics.activation(replayed, micros);
                }
                self.after(&run_id, end)
            }
        };
        if changed {
            inner.progress.fetch_add(1, Ordering::SeqCst);
        }
        {
            let mut active = self.active();
            active.runs.remove(&run_id);
            active.flows.remove(&flow_id);
        }
        inner.cond.notify_all();
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
            // The driver turns every error inside an activation other than a
            // lost ownership into this ending, and they are all store
            // failures: the run stays `running` until recovery.
            ActivationEnd::Failed { kind, detail } if kind == "store" => {
                self.inner
                    .fatal
                    .raise(format!("activation of {run_id}: {detail}"));
                false
            }
            // The store was cut under the activation.
            ActivationEnd::Crashed => {
                self.inner
                    .fatal
                    .raise(format!("activation of {run_id}: the store was cut"));
                false
            }
            ActivationEnd::Waiting { .. } | ActivationEnd::NotRunnable { .. } => false,
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

    /// The production loop: a pass every `pass_interval` until a fatal error.
    pub fn spawn_loop(&self) -> thread::JoinHandle<()> {
        let engine = self.clone();
        thread::spawn(move || {
            loop {
                if let Err(why) = engine.pass() {
                    tracing::error!(target: "engine", "the engine stopped: {why}");
                    return;
                }
                thread::sleep(engine.inner.config.pass_interval);
            }
        })
    }
}

/// How many calls the run has journaled: what its next activation replays.
fn journal_rows(rt: &Runtime, run_id: &str) -> u64 {
    rt.store()
        .read(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM journal WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
        .unwrap_or(0)
}
