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
    /// Sends tools under a run's registered daemon scope. Unlike flow host
    /// dispatch, codemode makes one attempt and never retries a lost reply.
    pub transport: Arc<dyn basal_host::transport::Transport>,
    pub catalog: Arc<dyn Catalog>,
    pub consent: Arc<dyn Consent>,
    pub hooks: Arc<dyn Hooks>,
}

/// Drop stops and joins module-owned engine and runtime threads, and revokes
/// the consent sink's runtime. A stalled shutdown panics after 60 seconds,
/// unless the dropping thread is already panicking: a second panic there
/// would abort the process, so drop logs the stall and returns instead.
/// Caller-owned runtime, engine or store clones must also be dropped before
/// reopening the same storage.
pub struct Module {
    pub rt: Runtime,
    pub pool: Pool,
    pub codemode_pool: Pool,
    pub(crate) codemode: crate::codemode::Codemode,
    pub engine: Engine,
    pub dry: DryRunner,
    pub consent: Arc<dyn Consent>,
    pub catalog: Arc<dyn Catalog>,
    pub metrics: Arc<Metrics>,
    pub fatal: Fatal,
    pub(crate) event_reader: Mutex<Option<crate::events::Reader>>,
    decisions: Arc<DecisionApplier>,
    /// How long drop waits for shutdown to release the module's threads.
    /// Tests shorten it to exercise a stalled shutdown quickly.
    pub(crate) shutdown_grace: Duration,
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(60);

impl Module {
    /// Handle a tool call on an already bound daemon route. The caller must
    /// supply Context using the principal and scope the daemon recorded when
    /// binding that route, never values supplied in model arguments.
    pub fn handle_tool(
        &self,
        context: &crate::tool::Context,
        request: cortexkit_role_tool_provider::call::ToolCallRequest,
        foreground: Duration,
    ) -> Result<serde_json::Value, subc_protocol::ErrorBody> {
        if request.name != "codemode" {
            return self.codemode.role(context, &request);
        }
        let keyed = request.call_key.is_some();
        let id = self.codemode.begin(context, &request)?;
        let timeout = if keyed {
            foreground
        } else {
            Duration::from_secs(42 * 60)
        };
        match self.codemode.wait(&id, timeout)? {
            Some(result) => Ok(result),
            None if keyed => Ok(serde_json::json!({"status":"running","run_id":id})),
            None => Err(subc_protocol::ErrorBody::new(
                "outcome_unknown",
                "keyless reply was lost",
            )),
        }
    }

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
        let pool = Pool::new(config.pool.clone(), spawner, clock.clone(), metrics.clone());
        let codemode_config = PoolConfig {
            warm_spares: 0,
            max_workers: 16,
            max_activations: 1,
            ..config.pool.clone()
        };
        let codemode_pool = Pool::new(
            codemode_config.clone(),
            Arc::new(crate::pool::ProcessSpawner::codemode(&codemode_config)),
            clock,
            Arc::new(Metrics::default()),
        );
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
        let codemode = crate::codemode::Codemode::new(
            store.clone(),
            &hosts,
            codemode_pool.clone(),
            &runtime_config,
        )
        .map_err(|error| format!("recovering codemode runs: {error}"))?;
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
            codemode_pool,
            codemode,
            engine,
            dry,
            consent: hosts.consent,
            catalog: hosts.catalog,
            metrics,
            fatal,
            event_reader: Mutex::new(None),
            decisions,
            shutdown_grace: SHUTDOWN_GRACE,
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
    fn decision_paths(&self) -> Result<Vec<basal_host::DecisionPath>, SinkError> {
        self.rt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .decision_paths()
            .map_err(|e| SinkError(e.to_string()))
    }

    fn has_open_cards_on(&self, path: &basal_host::DecisionPath) -> Result<bool, SinkError> {
        self.rt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .has_open_cards_on(path)
            .map_err(|e| SinkError(e.to_string()))
    }

    fn answer_cursor(&self, path: &basal_host::DecisionPath) -> Result<i64, SinkError> {
        self.rt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .decision_answer_cursor(path)
            .map_err(|e| SinkError(e.to_string()))
    }

    fn save_answer_cursor(
        &self,
        path: &basal_host::DecisionPath,
        cursor: i64,
    ) -> Result<(), SinkError> {
        self.rt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .save_decision_answer_cursor(path, cursor)
            .map_err(|e| SinkError(e.to_string()))
    }

    fn expire_on(&self, path: &basal_host::DecisionPath, id: &str) -> Result<(), SinkError> {
        self.rt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?
            .expire_decision_on(path, id)
            .map_err(|e| SinkError(e.to_string()))
    }
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
        self.answer_on(&basal_host::DecisionPath::Legacy, answer)
    }

    fn answer_on(
        &self,
        path: &basal_host::DecisionPath,
        answer: &DecisionAnswer,
    ) -> Result<(), SinkError> {
        let rt = self.rt.lock().unwrap_or_else(|p| p.into_inner());
        let rt = rt
            .as_ref()
            .ok_or_else(|| SinkError("the module is stopped".into()))?;
        match rt.answer_decision_on(path, answer) {
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
                // The decision card's answer was not recorded: its provider keeps
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
        let event_reader = self
            .event_reader
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let engine = self.engine.clone();
        let pool = self.pool.clone();
        let codemode_pool = self.codemode_pool.clone();
        let codemode = self.codemode.clone();
        let rt = self.rt.clone();
        let decisions = self.decisions.clone();
        // Waiting for empty run/flow slots is insufficient: an unwinding
        // activation frees its slot before dropping its engine and runtime.
        // A stalled host must fail shutdown loudly, not leave drop hung forever.
        let (done, receiver) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(event_reader);
            engine.stop();
            if let Err(error) = codemode.stop() {
                // Cancellation still kills workers and joins their drivers
                // when a simulated process death or storage failure prevents
                // its terminal commit. Startup marks unfinished runs interrupted.
                tracing::error!(%error, "codemode shutdown could not commit");
            }
            codemode_pool.stop();
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
        // Drop runs during unwinding too, for example when a test's assertion
        // fails while it owns a module. A panic raised there aborts the whole
        // process, hiding the original failure, so an unwinding thread only
        // logs a stalled or panicked shutdown and leaves the shutdown thread
        // running.
        let unwinding = std::thread::panicking();
        match receiver.recv_timeout(self.shutdown_grace) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if unwinding => {
                tracing::error!(
                    grace = ?self.shutdown_grace,
                    "module shutdown did not release its threads; abandoning it because the dropping thread is already panicking"
                );
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "module shutdown did not release its threads within {:?}",
                    self.shutdown_grace
                );
            }
            // The shutdown thread ended without signalling, so it panicked;
            // joining it below reports that.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if shutdown.join().is_err() {
            if unwinding {
                tracing::error!(
                    "module shutdown panicked while the dropping thread was already panicking"
                );
            } else {
                panic!("module shutdown panicked");
            }
        }
    }
}
