//! The module assembled: store, runtime, pool, engine, dry runner and the
//! consent plane. Recovery runs right after the runtime exists, before
//! anything can read or drive a run, so no run a previous process left
//! `running` is mistaken for one in progress.

use std::path::PathBuf;
use std::sync::Arc;

use basal_core::cards::{Decided, Decision};
use basal_core::{Config, Durability, Hooks, InstallError, Runtime, Store};
use basal_host::{CardDecision, Catalog, Consent, DecisionEvent, DecisionSink, Host, SinkError};

use crate::dryrun::{DryRunConfig, DryRunner};
use crate::engine::{Engine, EngineConfig};
use crate::fatal::{Fatal, install_is_storage, is_storage};
use crate::metrics::Metrics;
use crate::pool::{Pool, PoolConfig, PoolSource, Spawn};

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

pub struct Module {
    pub rt: Runtime,
    pub pool: Pool,
    pub engine: Engine,
    pub dry: DryRunner,
    pub consent: Arc<dyn Consent>,
    pub catalog: Arc<dyn Catalog>,
    pub metrics: Arc<Metrics>,
    pub fatal: Fatal,
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

    /// Bind adapters to the SQLite Store used by Runtime before attaching
    /// completion sinks or recovering calls. Broca must not save completions
    /// through an unbound store or a second writer.
    pub fn start_with_store(
        config: ModuleConfig,
        hosts: Hosts,
        spawner: Arc<dyn Spawn>,
        initialize: impl FnOnce(Arc<Store>) -> Result<(), String>,
    ) -> Result<Self, String> {
        let store = Store::open(&config.store_path, config.durability)
            .map_err(|e| format!("opening the store at {}: {e}", config.store_path.display()))?;
        let store = Arc::new(store);
        initialize(store.clone())?;
        let runtime_config = Config {
            auto_resume: false,
            ..config.runtime
        };
        let clock = runtime_config.clock.clone();
        let metrics = Arc::new(Metrics::default());
        let fatal = Fatal::new();
        let pool = Pool::new(config.pool.clone(), spawner, clock, metrics.clone());
        let rt = Runtime::new(
            store,
            hosts.host.clone(),
            hosts.catalog.clone(),
            hosts.hooks,
            Some(Arc::new(PoolSource { pool: pool.clone() })),
            runtime_config.clone(),
        );
        let recovered = rt.recover().map_err(|e| format!("recovering runs: {e}"))?;
        if !recovered.is_empty() {
            tracing::info!(target: "engine", "recovered {} runs left running", recovered.len());
        }
        pool.replenish();
        let engine = Engine::new(
            rt.clone(),
            pool.clone(),
            config.engine.clone(),
            metrics.clone(),
            fatal.clone(),
        )
        .map_err(|e| format!("building the scheduler: {e}"))?;
        std::fs::create_dir_all(&config.dry_run.scratch_root).map_err(|e| {
            format!(
                "creating the dry-run directory {}: {e}",
                config.dry_run.scratch_root.display()
            )
        })?;
        let dry = DryRunner::new(
            hosts.catalog.clone(),
            hosts.host,
            pool.clone(),
            config.dry_run.clone(),
            runtime_config,
            metrics.clone(),
        );
        hosts.consent.attach(Arc::new(DecisionApplier {
            rt: rt.clone(),
            fatal: fatal.clone(),
        }));
        Ok(Self {
            rt,
            pool,
            engine,
            dry,
            consent: hosts.consent,
            catalog: hosts.catalog,
            metrics,
            fatal,
        })
    }
}

/// Applies card decisions from the consent plane.
struct DecisionApplier {
    rt: Runtime,
    fatal: Fatal,
}

impl DecisionSink for DecisionApplier {
    fn decide(&self, event: &DecisionEvent) -> Result<(), SinkError> {
        let decision = match event.decision {
            CardDecision::Approve => Decision::Approve,
            CardDecision::Reject => Decision::Reject,
        };
        match self
            .rt
            .decide_card(&event.card_id, decision, &event.decided_by)
        {
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
}
