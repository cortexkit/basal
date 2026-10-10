//! The module assembled: store, runtime, pool, engine, dry runner and the
//! consent plane. Recovery runs right after the runtime exists, before
//! anything can read or drive a run, so no run a previous process left
//! `running` is mistaken for one in progress.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use basal_core::cards::{Decided, Decision};
use basal_core::decisions::Answered;
use basal_core::{Config, Durability, Hooks, InstallError, Runtime, Store};
use basal_host::{
    CardDecision, Catalog, Consent, DecisionAnswer, DecisionEvent, DecisionSink, Host, SinkError,
};

use crate::dryrun::{DryRunConfig, DryRunner};
use crate::engine::{Engine, EngineConfig};
use crate::fatal::{Fatal, install_is_storage, is_storage};
use crate::metrics::Metrics;
use crate::pool::{Pool, PoolConfig, Spawn};

/// Everything the module is configured with, apart from its hosts.
#[derive(Debug, Clone)]
pub struct ModuleConfig {
    pub store_path: PathBuf,
    pub durability: Durability,
    /// The runtime's settings. `auto_resume` (the runtime starting an
    /// activation by itself when a completion makes a run runnable) is
    /// forced off: the engine drives every run, so that a worker is always
    /// acquired before the run is claimed and activations stay bounded.
    pub runtime: Config,
    pub pool: PoolConfig,
    pub engine: EngineConfig,
    pub dry_run: DryRunConfig,
}

/// The systems the module reaches, supplied by whoever starts it: the real
/// adapters, the unconfigured ones, or the mocks.
#[derive(Clone)]
pub struct Hosts {
    pub host: Arc<dyn Host>,
    pub catalog: Arc<dyn Catalog>,
    pub consent: Arc<dyn Consent>,
    pub hooks: Arc<dyn Hooks>,
}

/// Drop stops and joins module-owned engine and runtime threads, and revokes
/// the consent sink's runtime. A stalled shutdown panics after 60 seconds.
/// Caller-owned runtime, engine or store clones must also be dropped before
/// reopening the same storage.
pub struct Module {
    pub rt: Runtime,
    pub pool: Pool,
    pub engine: Engine,
    pub dry: DryRunner,
    pub consent: Arc<dyn Consent>,
    pub catalog: Arc<dyn Catalog>,
    pub metrics: Arc<Metrics>,
    pub fatal: Fatal,
    decisions: Arc<DecisionApplier>,
}

impl Module {
    /// Opens the store, recovers, and builds the rest. Nothing reads or
    /// drives a run before `Runtime::recover` has put every run a previous
    /// process left `running` back in line. The engine's loop is not
    /// started here: the caller starts it (production) or pumps it (tests).
    pub fn start(
        config: ModuleConfig,
        hosts: Hosts,
        spawner: Arc<dyn Spawn>,
    ) -> Result<Self, String> {
        Self::start_with_store(config, hosts, spawner, |_| Ok(()))
    }

    /// Prepare the store, orphan recovery, scheduler and scratch directory
    /// before binding adapters. Bind before attaching completion sinks: a host
    /// can poll saved calls immediately when attached. A preparation failure
    /// therefore cannot retain an adapter's writer lease or start pool workers.
    pub fn start_with_store(
        config: ModuleConfig,
        hosts: Hosts,
        spawner: Arc<dyn Spawn>,
        initialize: impl FnOnce(Arc<Store>) -> Result<(), String>,
    ) -> Result<Self, String> {
        let store = Store::open(&config.store_path, config.durability)
            .map_err(|e| format!("opening the store at {}: {e}", config.store_path.display()))?;
        let store = Arc::new(store);
        let runtime_config = Config {
            auto_resume: false,
            ..config.runtime
        };
        let clock = runtime_config.clock.clone();
        let metrics = Arc::new(Metrics::default());
        let fatal = Fatal::new();
        let pool = Pool::new(config.pool.clone(), spawner, clock, metrics.clone());
        // No runtime or completion sink exists yet. Recovering orphan claims
        // needs only the writer lease, and cannot call an unbound adapter.
        let recovered = store
            .write(basal_core::runs::requeue_orphans)
            .map_err(|e| format!("recovering runs: {e}"))?;
        if !recovered.is_empty() {
            tracing::info!(target: "engine", "recovered {} runs left running", recovered.len());
        }
        let default_deadline_ms = i64::try_from(runtime_config.limits.default_deadline.as_millis())
            .map_err(|_| "building the scheduler: default deadline too long".to_owned())?;
        let scheduler = basal_core::schedule::Scheduler::new(
            store.clone(),
            Arc::new(runtime_config.clock.clone()),
            runtime_config.schedule.clone(),
        )
        .with_limits(basal_core::schedule::AdmitLimits {
            rate: runtime_config.rate,
            default_deadline_ms,
        });
        std::fs::create_dir_all(&config.dry_run.scratch_root).map_err(|e| {
            format!(
                "creating the dry-run directory {}: {e}",
                config.dry_run.scratch_root.display()
            )
        })?;
        initialize(store.clone())?;
        let rt = Runtime::new(
            store,
            hosts.host.clone(),
            hosts.catalog.clone(),
            hosts.hooks,
            None,
            runtime_config.clone(),
        );
        let engine = Engine::with_scheduler(
            rt.clone(),
            pool.clone(),
            hosts.consent.clone(),
            config.engine.clone(),
            metrics.clone(),
            fatal.clone(),
            scheduler,
        );
        let dry = DryRunner::new(
            hosts.catalog.clone(),
            hosts.host,
            pool.clone(),
            config.dry_run.clone(),
            runtime_config,
            metrics.clone(),
        );
        let decisions = Arc::new(DecisionApplier {
            rt: Mutex::new(Some(rt.clone())),
            fatal: fatal.clone(),
        });
        hosts.consent.attach(decisions.clone());
        pool.replenish();
        Ok(Self {
            rt,
            pool,
            engine,
            dry,
            consent: hosts.consent,
            catalog: hosts.catalog,
            metrics,
            fatal,
            decisions,
        })
    }
}

/// Applies card decisions from the consent plane.
struct DecisionApplier {
    // Hosts may retain an attached sink after the module is gone. Clearing
    // its runtime under the callback lock releases that store reference and
    // waits for any callback already using it.
    rt: Mutex<Option<Runtime>>,
    fatal: Fatal,
}

impl DecisionSink for DecisionApplier {
    fn has_open_cards(&self) -> Result<bool, SinkError> {
        let rt = self.rt.lock().unwrap_or_else(|p| p.into_inner());
        rt.as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .has_open_cards()
            .map_err(|e| SinkError(e.to_string()))
    }
    fn decide(&self, event: &DecisionEvent) -> Result<(), SinkError> {
        let rt = self.rt.lock().unwrap_or_else(|p| p.into_inner());
        let rt = rt
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?;
        let decision = match event.decision {
            CardDecision::Approve => Decision::Approve,
            CardDecision::Reject => Decision::Reject,
        };
        match rt.decide_card(&event.card_id, decision, &event.decided_by) {
            Ok(Decided::Applied(state)) => {
                tracing::info!(
                    target: "consent",
                    card = %event.card_id,
                    by = %event.decided_by,
                    "card decided: {}",
                    state.as_str()
                );
                Ok(())
            }
            Ok(Decided::AlreadyDecided(state)) => {
                tracing::info!(
                    target: "consent",
                    card = %event.card_id,
                    "a decision for a card already {} was ignored",
                    state.as_str()
                );
                Ok(())
            }
            Err(e) if install_is_storage(&e) => {
                // Not recorded: the consent plane keeps the decision and
                // delivers it again after the restart.
                self.fatal.raise(format!("recording a card decision: {e}"));
                Err(SinkError(e.to_string()))
            }
            Err(InstallError::Store(e)) if !is_storage(&e) => {
                tracing::error!(target: "consent", card = %event.card_id, "card decision failed: {e}");
                Ok(())
            }
            Err(e) => {
                // A decision for a card basal never raised, or one whose
                // version can no longer be approved: delivering it again
                // would change nothing.
                tracing::warn!(target: "consent", card = %event.card_id, "card decision refused: {e}");
                Ok(())
            }
        }
    }

    fn answer(&self, answer: &DecisionAnswer) -> Result<(), SinkError> {
        let rt = self.rt.lock().unwrap_or_else(|p| p.into_inner());
        let rt = rt
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?;
        match rt.answer_decision(answer) {
            Ok(answered) => {
                let what = match &answered {
                    Answered::Now(state) => state.as_str(),
                    Answered::AlreadyAnswered(_) => "already answered, ignored",
                    Answered::Superseded => "from a replaced card, ignored",
                    Answered::NoSuchCard => "for no card basal raised, ignored",
                };
                tracing::info!(
                    target: "consent",
                    elicitation = %answer.elicitation_id,
                    choice = ?answer.choice,
                    "decision card answer: {what}"
                );
                Ok(())
            }
            Err(e) if is_storage(&e) => {
                // The decision card's answer was not recorded: core keeps
                // it, since its page is not acknowledged, and delivers it
                // again after the restart.
                self.fatal
                    .raise(format!("recording a decision answer: {e}"));
                Err(SinkError(e.to_string()))
            }
            Err(e) => {
                // The answer cannot be applied (an option the card does not
                // have): delivering it again would change nothing.
                tracing::warn!(target: "consent", elicitation = %answer.elicitation_id, "decision answer refused: {e}");
                Ok(())
            }
        }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        let engine = self.engine.clone();
        let pool = self.pool.clone();
        let rt = self.rt.clone();
        let decisions = self.decisions.clone();
        // Waiting for empty run/flow slots is insufficient: an unwinding
        // activation frees its slot before dropping its engine and runtime.
        // A stalled host must fail shutdown loudly, not leave drop hung forever.
        let (done, receiver) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            engine.stop();
            pool.stop();
            engine.join_threads();
            rt.quiesce();
            decisions
                .rt
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            drop((engine, pool, rt, decisions));
            let _ = done.send(());
        });
        receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("module shutdown did not release its threads within 60 seconds");
        shutdown.join().expect("module shutdown panicked");
    }
}
