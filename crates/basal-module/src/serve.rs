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

enum Phase {
    Starting,
    Ready(Arc<Module>),
    Failed(String),
}

type RouteKey = (u16, u32);

/// What the daemon stamped on one route when it was bound.
#[derive(Debug, Clone)]
struct RouteStamp {
    principal: Option<Principal>,
    scope: Option<ScopeStamp>,
    /// The bind identity's session: the opener's own claim, which names the
    /// agent's session only when the route is under that agent's vouched
    /// scope (see [`caller::from_route`]).
    session: String,
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
    /// Records the principal, scope and session of a route the daemon has
    /// just bound.
    pub fn bind(&self, request: &RouteBindRequest) {
        lock(&self.stamps).insert(
            (request.handle.channel, request.handle.epoch),
            RouteStamp {
                principal: request.principal.clone(),
                scope: request.scope.clone(),
                session: request.identity.session.clone(),
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
            Some(s) => caller::from_route(s.principal.as_ref(), s.scope.as_ref(), &s.session),
            None => caller::from_route(None, None, ""),
        }
    }
}

pub struct BasalHandler {
    phase: Arc<Mutex<Phase>>,
    routes: Routes,
    configure: Arc<Configure>,
    hosts: Arc<MakeHosts>,
    initialize: Arc<InitializeHosts>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BasalHandler {
    pub fn new(configure: Configure, hosts: MakeHosts) -> Self {
        Self {
            phase: Arc::new(Mutex::new(Phase::Starting)),
            routes: Routes::default(),
            configure: Arc::new(configure),
            hosts: Arc::new(hosts),
            initialize: Arc::new(Box::new(|_| Ok(()))),
        }
    }
    pub fn with_store_initializer(mut self, initialize: InitializeHosts) -> Self {
        self.initialize = Arc::new(initialize);
        self
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
    phase: Arc<Mutex<Phase>>,
) {
    std::thread::spawn(move || {
        let dependencies = hosts();
        let config = configure(path);
        let deadline = std::time::Instant::now() + OPEN_RETRY;
        let started = loop {
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

#[async_trait]
impl ModuleHandler for BasalHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
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
            Ok(Err(e)) => HandlerOutcome::Error {
                code: e.code,
                message: e.message,
            },
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
        match store_path(ack) {
            Ok(path) => start(
                path,
                self.configure.clone(),
                self.hosts.clone(),
                self.initialize.clone(),
                self.phase.clone(),
            ),
            Err(e) => {
                tracing::error!(target: "store", "{e}");
                *lock(&self.phase) = Phase::Failed(e);
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
                None => HealthReport {
                    status: HealthStatus::Ok,
                    detail: None,
                    metrics: Some(module.metrics.to_json()),
                },
            },
        }
    }
}
