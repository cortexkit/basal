//! basal on subc: the handler `subc_client_rs::serve` drives.
//!
//! subc-client-rs owns HELLO, route binding, health and the frame lifecycle.
//! This handler records the principal and the scope the daemon stamps on
//! each route when it is bound ([`Routes`]), opens the store the daemon
//! names in HELLO_ACK, starts the module (recovery first) and its loop, and
//! runs each op on a blocking thread, because the core and the pool are
//! blocking code.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor, sqlite_store_path};
use serde::Deserialize;
use serde_json::{Value, json};
use subc_client_rs::{
    BindDecision, HandlerOutcome, HealthReport, HealthStatus, ModuleHandler, RequestCtx,
    RouteBindRequest, RouteHandle,
};
use subc_protocol::scope::ScopeStamp;
use subc_protocol::{ModuleHelloAckBody, Principal};

use crate::caller::{self, Caller};
use crate::manifest::MODULE_ID;
use crate::module::{Hosts, Module, ModuleConfig};
use crate::pool::{ProcessSpawner, Spawn};

/// Builds the module's configuration once the store path is known.
pub type Configure = Box<dyn Fn(PathBuf) -> ModuleConfig + Send + Sync>;
/// Builds the hosts the module runs with.
pub type MakeHosts = Box<dyn Fn() -> Hosts + Send + Sync>;
pub type InitializeHosts = Box<dyn Fn(Arc<basal_core::Store>) -> Result<(), String> + Send + Sync>;
/// Starts adapter polling only after module preparation has succeeded.
pub type ReadyHosts = Box<dyn Fn() + Send + Sync>;

enum Phase {
    Starting,
    Ready(Arc<Module>),
    Failed(String),
}

type RouteKey = (u16, u32);

/// What the daemon stamped on one route when it was bound. The bind
/// identity is kept only as provider configuration (workspace and session),
/// never as caller authority (see [`crate::caller`]).
#[derive(Debug, Clone)]
struct RouteStamp {
    principal: Option<Principal>,
    scope: Option<ScopeStamp>,
    tool_surface: bool,
    bind_identity: subc_protocol::BindIdentity,
}

/// The stamp of every bound route, and the caller each one names.
///
/// The daemon stamps a route once, at `route.bind`, never per request, and
/// a scope's stamp is fixed for the route's life (a change that revokes
/// authority closes the route), so the stamp kept here is what every
/// request on the route is decided by.
#[derive(Debug, Default)]
pub struct Routes {
    stamps: Mutex<HashMap<RouteKey, RouteStamp>>,
}

impl Routes {
    /// Records the principal and scope of a route the daemon has just bound.
    pub fn bind(&self, request: &RouteBindRequest) {
        lock(&self.stamps).insert(
            (request.handle.channel, request.handle.epoch),
            RouteStamp {
                principal: request.principal.clone(),
                scope: request.scope.clone(),
                bind_identity: request.identity.clone(),
                tool_surface: matches!(
                    request.target,
                    subc_protocol::RouteTarget::ToolProvider { .. }
                ),
            },
        );
    }

    /// Forgets a route that has gone.
    pub fn forget(&self, handle: &RouteHandle) {
        lock(&self.stamps).remove(&(handle.channel, handle.epoch));
    }

    /// Who calls on a route, from its stamp. A route with no recorded stamp
    /// has no principal and no scope, so it is refused by every op.
    pub fn caller(&self, handle: &RouteHandle) -> Caller {
        let stamp = lock(&self.stamps)
            .get(&(handle.channel, handle.epoch))
            .cloned();
        match stamp {
            Some(s) => caller::from_route(s.principal.as_ref(), s.scope.as_ref()),
            None => caller::from_route(None, None),
        }
    }

    fn tool_context(&self, handle: &RouteHandle) -> Option<crate::tool::Context> {
        let stamps = lock(&self.stamps);
        let stamp = stamps.get(&(handle.channel, handle.epoch))?;
        stamp.tool_surface.then(|| crate::tool::Context {
            principal: stamp.principal.clone().unwrap_or(Principal::Unverified),
            scope: stamp.scope.clone(),
            bind_identity: stamp.bind_identity.clone(),
        })
    }
}

#[derive(Clone)]
pub struct BasalHandler {
    phase: Arc<Mutex<Phase>>,
    routes: Arc<Routes>,
    configure: Arc<Configure>,
    hosts: Arc<MakeHosts>,
    initialize: Arc<InitializeHosts>,
    ready: Arc<ReadyHosts>,
    initialized: Arc<std::sync::atomic::AtomicBool>,
    tool_foreground: Duration,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BasalHandler {
    pub fn new(configure: Configure, hosts: MakeHosts) -> Self {
        Self {
            phase: Arc::new(Mutex::new(Phase::Starting)),
            routes: Arc::new(Routes::default()),
            configure: Arc::new(configure),
            hosts: Arc::new(hosts),
            initialize: Arc::new(Box::new(|_| Ok(()))),
            ready: Arc::new(Box::new(|| {})),
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_foreground: Duration::from_secs(25),
        }
    }
    pub fn with_store_initializer(mut self, initialize: InitializeHosts) -> Self {
        self.initialize = Arc::new(initialize);
        self
    }
    pub fn with_ready_hosts(mut self, ready: ReadyHosts) -> Self {
        self.ready = Arc::new(ready);
        self
    }

    /// Lower only the foreground wait; keyed runs continue under their own
    /// budgets and retain their terminal result for the custodian to pull.
    pub fn with_tool_foreground(mut self, wait: Duration) -> Self {
        self.tool_foreground = wait.min(self.tool_foreground);
        self
    }

    async fn tool_request(
        &self,
        ctx: RequestCtx,
        context: crate::tool::Context,
        body: Vec<u8>,
    ) -> HandlerOutcome {
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => return tool_error(crate::tool::invalid("request", error.to_string())),
        };
        for field in ["call_key", "schema_pin"] {
            if let Some(value) = value.get(field)
                && !value.is_null()
                && !value.is_string()
            {
                return tool_error(crate::tool::invalid(field, "expected an opaque string"));
            }
        }
        if value["name"] == "tool.withdraw" && value.get("call_key").is_some() {
            return tool_error(crate::tool::invalid(
                "call_key",
                "withdraw has no top-level call key",
            ));
        }
        let request: cortexkit_role_tool_provider::call::ToolCallRequest =
            match serde_json::from_value(value) {
                Ok(request) => request,
                Err(error) => {
                    return tool_error(crate::tool::invalid("request", error.to_string()));
                }
            };
        let module = match &*lock(&self.phase) {
            Phase::Ready(module) => module.clone(),
            _ => {
                return tool_error(subc_protocol::ErrorBody::new(
                    "module_warming",
                    "basal is opening its store",
                ));
            }
        };
        if request.name != "codemode" {
            let outcome =
                tokio::task::spawn_blocking(move || module.codemode.role(&context, &request)).await;
            return tool_outcome(outcome);
        }
        let keyed = request.call_key.is_some();
        let cancellation = ctx.cancellation_token();
        let codemode = module.codemode.clone();
        let start = tokio::task::spawn_blocking(move || {
            codemode.begin_guarded(&context, &request, || cancellation.is_cancelled())
        })
        .await;
        let id = match start {
            Ok(Ok(id)) => id,
            Ok(Err(error)) => return tool_error(error),
            Err(error) => {
                return tool_error(subc_protocol::ErrorBody::new("internal", error.to_string()));
            }
        };
        let codemode = module.codemode.clone();
        let waiting_id = id.clone();
        let timeout = if keyed {
            self.tool_foreground
        } else {
            Duration::from_secs(42 * 60)
        };
        let waiting = tokio::task::spawn_blocking(move || codemode.wait(&waiting_id, timeout));
        tokio::select! {
            outcome = waiting => match outcome {
                Ok(Ok(Some(result))) => tool_response(result),
                Ok(Ok(None)) if keyed => tool_response(json!({"status":"running","run_id":id,
                    "text":"running: the final result will be retained in late_results."})),
                Ok(Ok(None)) => tool_error(subc_protocol::ErrorBody::new("outcome_unknown","keyless codemode reply was lost")),
                Ok(Err(error)) => tool_error(error),
                Err(error) => tool_error(subc_protocol::ErrorBody::new("internal",error.to_string())),
            },
            _ = ctx.cancelled() => {
                let codemode = module.codemode.clone();
                let outcome = tokio::task::spawn_blocking(move || codemode.cancel(&id)).await;
                match outcome {
                    Ok(Ok(())) => tool_error(subc_protocol::ErrorBody::new("cancelled","codemode cancelled")),
                    Ok(Err(error)) => tool_error(error),
                    Err(error) => tool_error(subc_protocol::ErrorBody::new("internal",error.to_string())),
                }
            }
        }
    }

    /// Who calls on a route this handler has seen bound: what `handle`
    /// decides each request by.
    pub fn caller(&self, handle: &RouteHandle) -> Caller {
        self.routes.caller(handle)
    }
}

/// Where the store lives when the daemon names none: the fleet's data home,
/// as entorhinal falls back.
fn fallback_descriptor() -> StorageDescriptor {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("ck-basal-data"));
    StorageDescriptor {
        module_id: MODULE_ID.to_owned(),
        storage_namespace: "core".to_owned(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: sqlite_store_path(&data_home.to_string_lossy(), "ck-basal"),
        },
    }
}

fn store_path(ack: &ModuleHelloAckBody) -> Result<PathBuf, String> {
    let descriptor = match &ack.storage {
        Some(value) => serde_json::from_value::<StorageDescriptor>(value.clone())
            .map_err(|e| format!("HELLO_ACK's storage descriptor is invalid: {e}"))?,
        None => fallback_descriptor(),
    };
    match descriptor.backend {
        StorageBackend::Sqlite { path } => Ok(PathBuf::from(path)),
        #[allow(unreachable_patterns)]
        other => Err(format!(
            "basal needs a SQLite store, the daemon offered {other:?}"
        )),
    }
}

/// How long a new process retries opening a store whose single-writer
/// lease a predecessor still holds while it exits.
const OPEN_RETRY: Duration = Duration::from_secs(10);

/// Opens the store and starts the module on its own thread: opening can wait
/// for an exiting predecessor's lease, and HELLO_ACK must not wait with it.
fn start(
    path: PathBuf,
    configure: Arc<Configure>,
    hosts: Arc<MakeHosts>,
    initialize: Arc<InitializeHosts>,
    ready: Arc<ReadyHosts>,
    phase: Arc<Mutex<Phase>>,
) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + OPEN_RETRY;
        let started = loop {
            // Every attempt owns fresh adapters. Retaining a failed attempt's
            // bound Broca store would hold the old write lease across retries.
            let dependencies = hosts();
            let config = configure(path.clone());
            let spawner: Arc<dyn Spawn> = Arc::new(ProcessSpawner::new(&config.pool));
            match Module::start_with_store(config.clone(), dependencies.clone(), spawner, |store| {
                initialize(store)
            }) {
                Ok(module) => break Ok(module),
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::warn!(target: "store", "starting: {e}; retrying");
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => break Err(e),
            }
        };
        match started {
            Ok(module) => {
                ready();
                let module = Arc::new(module);
                module.fatal.exit_when_raised();
                module.engine.spawn_loop();
                tracing::info!(target: "engine", "basal is serving");
                *lock(&phase) = Phase::Ready(module);
            }
            Err(e) => {
                tracing::error!(target: "store", "basal could not start: {e}");
                *lock(&phase) = Phase::Failed(e);
                // Supervision restarts the process; serving on without a
                // store would only answer every op with an error.
                std::process::exit(crate::fatal::EXIT_STORE_FAILURE);
            }
        }
    });
}

#[derive(Deserialize)]
struct WireRequest {
    method: String,
    #[serde(default)]
    params: Value,
}

fn op_error_outcome(e: crate::ops::OpError) -> HandlerOutcome {
    match e.detail {
        Some(detail) => HandlerOutcome::ErrorWithDetail {
            code: e.code,
            message: e.message,
            detail,
        },
        None => HandlerOutcome::Error {
            code: e.code,
            message: e.message,
        },
    }
}

#[async_trait]
impl ModuleHandler for BasalHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if let Some(context) = self.routes.tool_context(&ctx.route_handle()) {
            return self.tool_request(ctx, context, body).await;
        }
        let request = match serde_json::from_slice::<WireRequest>(&body) {
            Ok(r) => r,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "invalid_request".into(),
                    message: format!("a request is JSON with method and params: {e}"),
                };
            }
        };
        let caller = self.caller(&ctx.route_handle());
        let module = match &*lock(&self.phase) {
            Phase::Ready(m) => m.clone(),
            Phase::Starting => {
                return HandlerOutcome::Error {
                    code: "module_warming".into(),
                    message: "basal is opening its store".into(),
                };
            }
            Phase::Failed(e) => {
                return HandlerOutcome::Error {
                    code: "storage_unavailable".into(),
                    message: e.clone(),
                };
            }
        };
        let method = request.method;
        let params = request.params;
        let outcome =
            tokio::task::spawn_blocking(move || module.handle(&caller, &method, params)).await;
        match outcome {
            Ok(Ok(result)) => match serde_json::to_vec(&json!({ "result": result })) {
                Ok(bytes) => HandlerOutcome::Response(bytes),
                Err(e) => HandlerOutcome::Error {
                    code: "encode_failed".into(),
                    message: e.to_string(),
                },
            },
            Ok(Err(e)) => op_error_outcome(e),
            Err(e) => HandlerOutcome::Error {
                code: "internal".into(),
                message: format!("the op's thread ended: {e}"),
            },
        }
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.routes.bind(request);
        BindDecision::accept()
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.routes.forget(handle);
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        // A replacement SDK connection changes the describer, not the module's
        // store, workers or runtime. Reopening would contend with our own lease.
        if self
            .initialized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match store_path(ack) {
            Ok(path) => start(
                path,
                self.configure.clone(),
                self.hosts.clone(),
                self.initialize.clone(),
                self.ready.clone(),
                self.phase.clone(),
            ),
            Err(e) => {
                tracing::error!(target: "store", "{e}");
                *lock(&self.phase) = Phase::Failed(e);
                // A descriptor that cannot name a store cannot become healthy
                // by serving more requests. Let supervision retry startup.
                std::process::exit(crate::fatal::EXIT_STORE_FAILURE);
            }
        }
    }

    async fn health(&self) -> HealthReport {
        // Read from memory only: the daemon's health path must answer even
        // while an op holds the store.
        match &*lock(&self.phase) {
            Phase::Starting => HealthReport {
                status: HealthStatus::Degraded,
                detail: Some("opening the store".into()),
                metrics: None,
            },
            Phase::Failed(e) => HealthReport {
                status: HealthStatus::Failing,
                detail: Some(e.clone()),
                metrics: None,
            },
            Phase::Ready(module) => match module.fatal.get() {
                Some(why) => HealthReport {
                    status: HealthStatus::Failing,
                    detail: Some(why),
                    metrics: None,
                },
                None => {
                    let pool = module.pool.stats();
                    HealthReport {
                        status: if pool.spawn_error.is_some() {
                            HealthStatus::Degraded
                        } else {
                            HealthStatus::Ok
                        },
                        detail: pool.spawn_error,
                        metrics: Some(module.metrics.to_json()),
                    }
                }
            },
        }
    }
}

fn tool_error(error: subc_protocol::ErrorBody) -> HandlerOutcome {
    match error.detail {
        Some(detail) => HandlerOutcome::ErrorWithDetail {
            code: error.code,
            message: error.message,
            detail,
        },
        None => HandlerOutcome::Error {
            code: error.code,
            message: error.message,
        },
    }
}

fn tool_response(value: Value) -> HandlerOutcome {
    match serde_json::to_vec(&value) {
        Ok(bytes) => HandlerOutcome::Response(bytes),
        Err(error) => tool_error(subc_protocol::ErrorBody::new(
            "encode_failed",
            error.to_string(),
        )),
    }
}

fn tool_outcome(
    outcome: Result<Result<Value, subc_protocol::ErrorBody>, tokio::task::JoinError>,
) -> HandlerOutcome {
    match outcome {
        Ok(Ok(value)) => tool_response(value),
        Ok(Err(error)) => tool_error(error),
        Err(error) => tool_error(subc_protocol::ErrorBody::new("internal", error.to_string())),
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    #[test]
    fn refusal_without_detail_keeps_the_plain_error_frame() {
        let outcome = op_error_outcome(crate::ops::OpError {
            code: "not_permitted".into(),
            message: "refused".into(),
            detail: None,
        });
        assert!(
            matches!(outcome,HandlerOutcome::Error {code,message} if code == "not_permitted" && message == "refused")
        );
    }
    #[test]
    fn stale_generation_detail_uses_a_machine_readable_error_frame() {
        let outcome = op_error_outcome(crate::ops::OpError {
            code: "generation_stale".into(),
            message: "stale".into(),
            detail: Some(json!({"current":9})),
        });
        assert!(
            matches!(outcome,HandlerOutcome::ErrorWithDetail {code,message,detail} if code == "generation_stale" && message == "stale" && detail == json!({"current":9}))
        );
    }
}
