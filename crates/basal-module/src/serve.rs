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
use std::sync::{Arc, Mutex};
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
/// What the serving process does with the first store failure the module
/// latches. Production exits so supervision restarts it and recovery runs.
pub type OnStoreFailure = Box<dyn Fn(String) + Send + Sync>;

enum Phase {
    Starting,
    Ready(Arc<Module>),
    Failed(String),
}

type RouteKey = (u16, u32);

/// The daemon records the route's principal and scope when binding it. Keep
/// those values to identify the caller. Bind identity configures the provider's
/// workspace and session; it establishes who the caller is only for the
/// admitted dev operator app (see [`crate::caller::dev_operator_app_admission`]).
#[derive(Debug, Clone)]
struct RouteStamp {
    principal: Option<Principal>,
    scope: Option<ScopeStamp>,
    tool_surface: bool,
    bind_identity: subc_protocol::BindIdentity,
    call_mode: subc_client_rs::CallMode,
    consumer_capabilities: Option<Vec<String>>,
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
                call_mode: request.call_mode.clone(),
                consumer_capabilities: request.consumer_capabilities.clone(),
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
            Some(s) => {
                caller::caller_for_route(s.principal.as_ref(), s.scope.as_ref(), &s.bind_identity)
            }
            None => caller::from_route(None, None),
        }
    }

    fn diagnostics_admitted(&self, handle: &RouteHandle) -> bool {
        lock(&self.stamps)
            .get(&(handle.channel, handle.epoch))
            .is_some_and(|stamp| {
                caller::diagnostics_admitted(stamp.principal.as_ref(), stamp.scope.as_ref())
            })
    }

    /// Whether the route was opened under a flow's scope. The daemon admits
    /// such routes because basal declares `flow-scopes/v1`, which it does
    /// only so flows can read `diagnostics.findings`.
    fn flow_scoped(&self, handle: &RouteHandle) -> bool {
        lock(&self.stamps)
            .get(&(handle.channel, handle.epoch))
            .and_then(|stamp| stamp.scope.as_ref())
            .and_then(|scope| scope.attributes.flow_id.as_deref())
            .is_some_and(|id| !id.is_empty())
    }

    fn tool_context(&self, handle: &RouteHandle) -> Option<crate::tool::Context> {
        let stamps = lock(&self.stamps);
        let stamp = stamps.get(&(handle.channel, handle.epoch))?;
        stamp.tool_surface.then(|| crate::tool::Context {
            principal: stamp.principal.clone().unwrap_or(Principal::Unverified),
            scope: stamp.scope.clone(),
            bind_identity: stamp.bind_identity.clone(),
            call_mode: stamp.call_mode.clone(),
            consumer_capabilities: stamp.consumer_capabilities.clone(),
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
    event_transport: Arc<Option<Arc<dyn basal_host::transport::Transport>>>,
    initialized: Arc<std::sync::atomic::AtomicBool>,
    daemon_capabilities: Arc<basal_host::subc_catalog::DaemonCapabilities>,
    tool_foreground: Duration,
    on_store_failure: Arc<OnStoreFailure>,
}

use basal_proto::util::lock;

impl BasalHandler {
    pub fn new(configure: Configure, hosts: MakeHosts) -> Self {
        Self {
            phase: Arc::new(Mutex::new(Phase::Starting)),
            routes: Arc::new(Routes::default()),
            configure: Arc::new(configure),
            hosts: Arc::new(hosts),
            initialize: Arc::new(Box::new(|_| Ok(()))),
            ready: Arc::new(Box::new(|| {})),
            event_transport: Arc::new(None),
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            daemon_capabilities: Arc::new(basal_host::subc_catalog::DaemonCapabilities::default()),
            tool_foreground: Duration::from_secs(25),
            on_store_failure: Arc::new(Box::new(crate::fatal::exit_after_store_failure)),
        }
    }
    pub fn with_daemon_capabilities(
        mut self,
        capabilities: Arc<basal_host::subc_catalog::DaemonCapabilities>,
    ) -> Self {
        self.daemon_capabilities = capabilities;
        self
    }
    pub fn with_store_initializer(mut self, initialize: InitializeHosts) -> Self {
        self.initialize = Arc::new(initialize);
        self
    }
    pub fn with_ready_hosts(mut self, ready: ReadyHosts) -> Self {
        self.ready = Arc::new(ready);
        self
    }

    /// Replace exiting the process on a latched store failure. Only an
    /// in-process test harness should use this: exiting there would end the
    /// whole test binary before it reports which test hit the failure. The
    /// module still latches the failure and its health reports `Failing`.
    pub fn with_store_failure_action(mut self, action: OnStoreFailure) -> Self {
        self.on_store_failure = Arc::new(action);
        self
    }

    /// Start consuming events after recovery, so new admissions cannot race
    /// with requeuing runs that the previous module process left running.
    pub fn with_event_reader(
        mut self,
        transport: Arc<dyn basal_host::transport::Transport>,
    ) -> Self {
        self.event_transport = Arc::new(Some(transport));
        self
    }

    /// Shorten only the initial wait for a reply. Runs with a call key continue
    /// within their budgets and keep their result available for later retrieval.
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
        if let Err(error) = crate::tool::check_call_mode(&request.name, &context.call_mode) {
            return tool_error(error);
        }
        let accepts_tool_result_v1 = cortexkit_role_tool_provider::consumer_reads_tool_result_v1(
            context.consumer_capabilities.as_deref(),
        );
        let module = match &*lock(&self.phase) {
            Phase::Ready(module) => module.clone(),
            _ => {
                return tool_error(subc_protocol::ErrorBody::new(
                    "module_warming",
                    "basal is opening its store",
                ));
            }
        };
        if request.name == "flow" {
            return tool_result_outcome(
                tokio::task::spawn_blocking(move || module.flow_tool(&context, &request)).await,
                accepts_tool_result_v1,
            );
        }
        if request.name != "codemode" {
            let outcome =
                tokio::task::spawn_blocking(move || module.codemode.role(&context, &request)).await;
            return tool_outcome(outcome);
        }
        let keyed = request.call_key.is_some();
        let consumer_capabilities = context.consumer_capabilities.clone();
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
                Ok(Ok(Some(result))) => tool_result_response(result, accepts_tool_result_v1),
                Ok(Ok(None)) if keyed => tool_result_response(module.codemode.running(&id, consumer_capabilities.as_deref()), accepts_tool_result_v1),
                Ok(Ok(None)) => tool_error(subc_protocol::ErrorBody::new("outcome_unknown","keyless codemode reply was lost")),
                Ok(Err(error)) => tool_error(error),
                Err(error) => tool_error(subc_protocol::ErrorBody::new("internal",error.to_string())),
            },
            _ = ctx.cancelled() => {
                let codemode = module.codemode.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    codemode.cancel(&id)?;
                    codemode.wait(&id, Duration::ZERO)
                }).await;
                match outcome {
                    Ok(Ok(Some(result))) => tool_result_response(result, accepts_tool_result_v1),
                    Ok(Ok(None)) => tool_error(subc_protocol::ErrorBody::new("cancelled","codemode cancelled")),
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
fn start(path: PathBuf, handler: &BasalHandler) {
    let BasalHandler {
        configure,
        hosts,
        initialize,
        ready,
        phase,
        event_transport,
        on_store_failure,
        ..
    } = handler.clone();
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
                if let Some(transport) = event_transport.as_ref() {
                    *lock(&module.event_reader) = Some(crate::events::Reader::start(
                        module.rt.clone(),
                        transport.clone(),
                    ));
                }
                let module = Arc::new(module);
                module.fatal.when_raised(move |why| on_store_failure(why));
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

/// The final refusal for a flow-scoped route reaching any management op but
/// `diagnostics.findings`, with core's code for the same
/// case. Basal's tools need no gate here: neither serves the `flow` call
/// mode, so the tool-provider contract already refuses a flow-scoped bind
/// with `call_mode_unsupported`.
fn flow_scope_gate(flow_scoped: bool, method: &str) -> Option<HandlerOutcome> {
    if !flow_scoped || method == "diagnostics.findings" {
        return None;
    }
    Some(HandlerOutcome::Error {
        code: "flow_scope_not_supported".into(),
        message: format!("flows may call only diagnostics.findings on basal, not {method}"),
    })
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
        // A flow reaches basal's management surface only for
        // `diagnostics.findings`. Its scope can carry its author's agent id,
        // so any other op would let a flow act as that agent here.
        let flow_scoped = self.routes.flow_scoped(&ctx.route_handle());
        if let Some(refusal) = flow_scope_gate(flow_scoped, &request.method) {
            return refusal;
        }
        let caller = self.caller(&ctx.route_handle());
        let diagnostics_admitted = self.routes.diagnostics_admitted(&ctx.route_handle());
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
        let outcome = tokio::task::spawn_blocking(move || {
            if method == "diagnostics.findings" {
                if !diagnostics_admitted {
                    return Err(crate::ops::OpError::not_permitted(&method, &caller));
                }
                module.diagnostics_findings(params)
            } else {
                module.handle(&caller, &method, params)
            }
        })
        .await;
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
        // Reconnects can change daemon capabilities without reopening the store.
        self.daemon_capabilities.observe_hello_ack(ack);
        // A replacement SDK connection changes the describer, not the module's
        // store, workers or runtime. Reopening would contend with our own lease.
        if self
            .initialized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match store_path(ack) {
            Ok(path) => start(path, self),
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
                    let events = lock(&module.event_reader)
                        .as_ref()
                        .map(crate::events::Reader::health);
                    let event_error = events.as_ref().and_then(|h| {
                        h.error
                            .clone()
                            .or_else(|| (!h.connected).then(|| "event reader connecting".into()))
                    });
                    let mut metrics = module.metrics.to_json();
                    metrics["events"] = serde_json::to_value(events).unwrap_or(Value::Null);
                    HealthReport {
                        status: if pool.spawn_error.is_some() || event_error.is_some() {
                            HealthStatus::Degraded
                        } else {
                            HealthStatus::Ok
                        },
                        detail: pool.spawn_error.or(event_error),
                        metrics: Some(metrics),
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

fn tool_result_response(value: Value, accepts_tool_result_v1: bool) -> HandlerOutcome {
    if accepts_tool_result_v1 {
        tool_response(value)
    } else {
        tool_response(legacy_tool_result(value))
    }
}

fn legacy_tool_result(value: Value) -> Value {
    let Some(text) = value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
    else {
        return value;
    };
    let Some(fields) = value.get("structuredContent").and_then(Value::as_object) else {
        return value;
    };
    // Legacy callers receive the same text and machine fields in one JSON object.
    let mut legacy = fields.clone();
    legacy.insert("text".into(), Value::String(text.to_owned()));
    Value::Object(legacy)
}

fn tool_result_outcome(
    outcome: Result<Result<Value, subc_protocol::ErrorBody>, tokio::task::JoinError>,
    accepts_tool_result_v1: bool,
) -> HandlerOutcome {
    match outcome {
        Ok(Ok(value)) => tool_result_response(value, accepts_tool_result_v1),
        Ok(Err(error)) => tool_error(error),
        Err(error) => tool_error(subc_protocol::ErrorBody::new("internal", error.to_string())),
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

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[tokio::test]
    async fn hello_ack_refreshes_catalog_events_on_reconnect() {
        let capabilities = Arc::new(basal_host::subc_catalog::DaemonCapabilities::default());
        let handler = BasalHandler::new(
            Box::new(|_| panic!("reconnect must not configure the store")),
            Box::new(|| panic!("reconnect must not rebuild hosts")),
        )
        .with_daemon_capabilities(capabilities.clone());
        assert!(!capabilities.supports_catalog_events());
        // Exercise the reconnect callback without starting a second store owner.
        handler
            .initialized
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for present in [false, true, false, true] {
            let ack = serde_json::from_value(json!({
                "negotiated_ver": subc_protocol::PROTOCOL_VERSION,
                "subc_ops": [],
                "subc_capabilities": if present {
                    vec![subc_protocol::manifest::CAP_CATALOG_EVENTS_V1]
                } else {
                    vec!["flow-scopes/v1"]
                },
                "storage": null,
                "machine_id": null,
            }))
            .unwrap();
            handler.on_hello_ack(&ack).await;
            assert_eq!(capabilities.supports_catalog_events(), present);
        }
    }
}

#[cfg(test)]
mod diagnostics_route_tests {
    use super::*;
    use subc_protocol::{BindIdentity, RouteTarget};

    #[test]
    fn diagnostics_requires_a_bound_stamp_and_rejects_the_dev_app() {
        let routes = Routes::default();
        let handle = RouteHandle::detached(12, 1);
        assert!(!routes.diagnostics_admitted(&handle));
        let request = RouteBindRequest::new(
            handle,
            RouteTarget::ManagementSurface {
                module_id: "basal".into(),
            },
            BindIdentity::new("/work/project", "desktop", "desktop-session"),
        )
        .with_principal(Principal::Direct);
        routes.bind(&request);
        assert_eq!(routes.caller(&handle), Caller::DevOperatorApp);
        assert!(!routes.diagnostics_admitted(&handle));
        let request = request.with_principal(Principal::Reserved {
            module_id: caller::OPERATOR_MODULE.into(),
        });
        routes.bind(&request);
        assert!(routes.diagnostics_admitted(&handle));
        routes.forget(&handle);
        assert!(!routes.diagnostics_admitted(&handle));
    }

    /// A flow's scope may carry its author's agent id, so a flow-scoped route
    /// must reach nothing on basal but `diagnostics.findings`.
    #[test]
    fn a_flow_scoped_route_reaches_only_diagnostics() {
        use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
        let routes = Routes::default();
        let handle = RouteHandle::detached(13, 1);
        let flow_scope = ScopeStamp::new(
            Principal::Reserved {
                module_id: caller::CORE_MODULE.into(),
            },
            "flow-ref",
            1,
            ScopeKind::Head,
        )
        .with_attributes(
            ScopeAttributes::new()
                .with_agent_id(Some("agent_author".into()))
                .with_flow_id(Some("alf-self-diagnosis".into())),
        )
        .with_owner_authorized(true);
        routes.bind(
            &RouteBindRequest::new(
                handle,
                RouteTarget::ManagementSurface {
                    module_id: "basal".into(),
                },
                BindIdentity::new("/", "basal", "flow"),
            )
            .with_principal(Principal::Reserved {
                module_id: "basal".into(),
            })
            .with_scope(flow_scope),
        );
        assert!(routes.flow_scoped(&handle));
        assert!(routes.diagnostics_admitted(&handle));
        // Without the gate this route would be its author agent to every op.
        assert!(matches!(routes.caller(&handle), Caller::Agent { .. }));
        assert!(flow_scope_gate(true, "diagnostics.findings").is_none());
        for method in ["flow.list", "flow.install", "flow.disable"] {
            match flow_scope_gate(true, method) {
                Some(HandlerOutcome::Error { code, .. }) => {
                    assert_eq!(code, "flow_scope_not_supported", "{method}")
                }
                _ => panic!("{method} reached basal from a flow scope"),
            }
        }
        // An agent's own head route is untouched.
        assert!(flow_scope_gate(false, "flow.list").is_none());
    }
}

#[cfg(test)]
mod reader_health_tests {
    use super::*;
    use basal_host::transport::{Transport, WireError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PanickingCredentials(AtomicUsize);

    impl Transport for PanickingCredentials {
        fn catalog(&self) -> Result<Value, WireError> {
            unreachable!()
        }
        fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
            unreachable!()
        }
        fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("injected reader credential panic");
            }
            std::thread::sleep(Duration::from_millis(500));
            Err(WireError::NeverSent("credential retry reached".into()))
        }
    }

    #[tokio::test]
    async fn dead_reader_degrades_top_level_health_and_retries() {
        let dir = std::env::temp_dir().join(format!("basal-reader-health-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut pool =
            crate::pool::PoolConfig::new("unused-worker", crate::process::WorkerLaunch::Plain);
        pool.warm_spares = 0;
        let config = ModuleConfig {
            store_path: dir.join("store.db"),
            durability: basal_core::Durability::throwaway(),
            runtime: basal_core::Config::default(),
            pool: pool.clone(),
            engine: Default::default(),
            dry_run: crate::dryrun::DryRunConfig::new(dir.join("dry-run")),
        };
        let transport = Arc::new(PanickingCredentials(AtomicUsize::new(0)));
        let hosts = Hosts {
            transport: transport.clone(),
            host: Arc::new(basal_host::mock::MockHost::new()),
            catalog: Arc::new(basal_host::MockCatalog::standard()),
            consent: Arc::new(basal_host::MockConsent::new()),
            hooks: Arc::new(basal_core::NoHooks),
        };
        let module =
            Arc::new(Module::start(config, hosts, Arc::new(ProcessSpawner::new(&pool))).unwrap());
        let handler = BasalHandler::new(Box::new(|_| unreachable!()), Box::new(|| unreachable!()));
        *lock(&handler.phase) = Phase::Ready(module.clone());
        assert_eq!(handler.health().await.status, HealthStatus::Ok);
        *lock(&module.event_reader) = Some(crate::events::Reader::start(
            module.rt.clone(),
            transport.clone(),
        ));
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let report = loop {
            let report = handler.health().await;
            if report
                .detail
                .as_ref()
                .is_some_and(|e| e.contains("injected reader credential panic"))
            {
                break report;
            }
            assert!(
                std::time::Instant::now() < until,
                "panic missing from top-level health"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(report.status, HealthStatus::Degraded);
        assert!(report.detail.unwrap().starts_with("reader stopped:"));
        let events = &report.metrics.unwrap()["events"];
        assert_eq!(events["connected"], false);
        assert!(events["retry_in_ms"].as_u64().unwrap() >= 250);
        while transport.0.load(Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < until,
                "reader never retried after panic"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        *lock(&handler.phase) = Phase::Starting;
        drop(module);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
