//! Blocking consumer boundary. The runtime dispatches off its activation thread.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_client_rs::consumer::{
    CallError, CallOptions, ConnectionState, ConsumerOptions, OutcomeUnknownCause, SubcConsumer,
};
use subc_protocol::{BindIdentity, RouteTarget};

/// Core's catalog capability for the package.approved_versions management read.
pub const PACKAGE_APPROVED_VERSIONS_CAPABILITY: &str = "package-approved-versions/v1";

/// How a call over the daemon failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Typed(crate::flow_refusal::FlowRefusal),
    /// Provably never reached the target.
    NeverSent(String),
    /// Sent, and then the route closed or the connection failed before a
    /// reply: its outcome is unknown (`connection_lost`).
    Unknown(String),
    /// A scope close cut an in-flight request; even keyed mutations must reconcile.
    ScopeCut(String),
    /// Sent, and no reply came within the call's deadline: its outcome is
    /// unknown (`reply_timeout`).
    TimedOut(String),
    /// A reply came that could not be decoded, or named a disposition basal
    /// does not know: its outcome is unknown (`reply_unreadable`).
    Unreadable(String),
    Refused {
        code: String,
        message: String,
    },
    RefusedDetails {
        code: String,
        message: String,
        detail: Value,
    },
}

impl WireError {
    /// Why the call's outcome is unknown, or `None` when it provably never
    /// left or was answered with a refusal.
    pub fn unknown_reason(&self) -> Option<crate::UnknownReason> {
        use crate::UnknownReason;
        match self {
            Self::NeverSent(_)
            | Self::Refused { .. }
            | Self::RefusedDetails { .. }
            | Self::Typed(_) => None,
            Self::Unknown(_) | Self::ScopeCut(_) => Some(UnknownReason::ConnectionLost),
            Self::TimedOut(_) => Some(UnknownReason::ReplyTimeout),
            Self::Unreadable(_) => Some(UnknownReason::ReplyUnreadable),
        }
    }
}

/// Returns decoded provider payloads, not the management response envelope.
pub trait Transport: Send + Sync {
    /// Wake durable work when a catalog read observes a different generation.
    fn on_catalog_change(&self, _notify: Arc<dyn Fn() + Send + Sync>) {}

    /// Read a caller's file under the short-lived scope core registered for this
    /// flow-tool call, never with basal's own file grants.
    fn flow_read_exact(
        &self,
        _scope: &subc_protocol::scope::ScopeSelector,
        _identity: &BindIdentity,
        _path: &str,
    ) -> Result<Value, WireError> {
        Err(WireError::NeverSent("scoped file reads unavailable".into()))
    }

    fn newest_approved_template_version(&self, package: &str) -> Result<Option<u32>, WireError> {
        let catalog = self.catalog()?;
        let supported = catalog["modules"].as_array().is_some_and(|modules| {
            modules.iter().any(|module| {
                module["module_id"] == "prefrontal-core"
                    && module["capabilities"]["provides"]
                        .as_array()
                        .is_some_and(|tags| {
                            tags.iter()
                                .any(|tag| tag == PACKAGE_APPROVED_VERSIONS_CAPABILITY)
                        })
            })
        });
        if !supported {
            return Err(WireError::Refused {
                code: "version_required".into(),
                message: "Pass version; this core can't report approved template versions yet"
                    .into(),
            });
        }
        let reply = self.management(
            "prefrontal-core",
            "package.approved_versions",
            json!({"package":package}),
        )?;
        if reply["package"] != package {
            return Err(WireError::Unreadable(
                "approved versions package mismatch".into(),
            ));
        }
        match reply.get("newest_approved") {
            Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
                .map(Some)
                .ok_or_else(|| WireError::Unreadable("invalid approved version".into())),
            None => Err(WireError::Unreadable("missing approved version".into())),
        }
    }
    /// Returns a route-table key for a core-owned target scope, never an unscoped fallback.
    fn target_flow(
        &self,
        _request: &crate::CallRequest,
        _call: &crate::target_scope::TargetCall,
    ) -> Result<String, WireError> {
        Err(WireError::NeverSent("target scopes unavailable".into()))
    }
    fn watch_run_scope(&self, _run: &str, _ended: Arc<RunScopeEnded>) {}
    /// Unlike the compatibility flow-call defaults, this read may never fall
    /// back to an unscoped management call.
    fn grant_would_ask(
        &self,
        _flow: &str,
        _version: u32,
        _agent: Option<&str>,
        _module: &str,
        _grant: &str,
    ) -> Result<bool, WireError> {
        Err(WireError::NeverSent(
            "scoped grant diagnostics unavailable".into(),
        ))
    }
    fn provider_ready(
        &self,
        _flow: &str,
        _module: &str,
        _action: &str,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        Ok(())
    }
    fn configure_flow(&self, _flow: &str, _scope: Option<crate::flow_scope::RegisteredScope>) {}
    fn retain_flow_routes(&self, _flow_id: &str, _live: &std::collections::HashSet<String>) {}
    fn management_for_flow(
        &self,
        _flow: &str,
        module: &str,
        op: &str,
        params: Value,
    ) -> Result<Value, WireError> {
        self.management(module, op, params)
    }
    fn tool_for_flow(
        &self,
        _flow: &str,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError> {
        self.tool(module, name, arguments, call_key)
    }
    fn tool_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
        options: &RunToolOptions,
    ) -> Result<Value, WireError> {
        let _ = (run, module, name, arguments, call_key, options);
        Err(WireError::NeverSent(
            "run-scoped tool dispatch unavailable".into(),
        ))
    }
    /// Custody operations may reopen the run's tool route after reconnect, but
    /// never resend the tool call or fall back to an unscoped management route.
    /// They carry no top-level call key.
    fn role_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        arguments: Value,
        options: &RunToolOptions,
    ) -> Result<Value, WireError> {
        let _ = (run, module, name, arguments, options);
        Err(WireError::ScopeCut("run custody route unavailable".into()))
    }
    fn catalog(&self) -> Result<Value, WireError>;
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError>;
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError>;
    /// Sends one tool-provider role request (`role.describe`, `tool.catalog`)
    /// to `module` on a route opened under a codemode run's own scope
    /// `scope` and bound with `identity`, then closes that route.
    ///
    /// The daemon derives the call mode from the scope it stamps on the bind:
    /// a run scope (one carrying `run_id`) makes the call `codemode`. Opening
    /// under the run scope is therefore what makes a provider see a codemode
    /// call; an unscoped route reaches it as `classic`. Role requests carry no
    /// envelope `call_key`: providers refuse one there as `invalid_request`.
    /// The identity is bind configuration (project root, harness label,
    /// session), never authority.
    ///
    /// The default refuses without sending, so a transport that cannot open
    /// run-scoped routes never falls back to an unscoped read.
    fn role_on_run_scope(
        &self,
        scope: &subc_protocol::scope::ScopeSelector,
        identity: &BindIdentity,
        module: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, WireError> {
        let _ = (scope, identity, module, name, arguments);
        Err(WireError::NeverSent(
            "run-scoped role requests unavailable".into(),
        ))
    }
}

pub type RunScopeEnded = dyn Fn(&str) + Send + Sync;

/// Configuration copied from the invoking bind; it doesn't establish who the caller is.
#[derive(Clone)]
pub struct RunToolOptions {
    pub identity: BindIdentity,
    pub reply_timeout: Duration,
    /// Core's catalog preset for the run. Only `tool_for_run` sends it, in
    /// the tool call's `ToolCallRequest.preset`.
    pub preset: Option<String>,
}

/// Maps the client's typed send disposition without guessing from prose.
pub fn map_error(error: CallError) -> WireError {
    if let Some(body) = error.route_open_refusal() {
        if body.code == "scope_expired" || body.code == "flow_delegation_unsupported" {
            return refusal(body.clone());
        }
        return match crate::flow_refusal::FlowRefusal::decode(body) {
            Ok(Some(decoded)) => WireError::Typed(decoded),
            Ok(None) if !body.code.starts_with("scope_") => WireError::NeverSent(format!(
                "{}: {} (detail: {:?})",
                body.code, body.message, body.detail
            )),
            _ => WireError::Unreadable(format!("unrecognised scoped route refusal: {body:?}")),
        };
    }
    // Read before the match moves the error apart; `None` for every variant
    // other than OutcomeUnknown.
    let cause = error.outcome_cause();
    match error {
        CallError::NotSent(e) => WireError::NeverSent(e.to_string()),
        CallError::Module(e) => refusal(e),
        // Stale handles and capability resolution errors are rejected before
        // any request frame is written, unlike failures of accepted requests.
        e @ (CallError::StaleRouteHandle(_)
        | CallError::CapabilityUnprovided { .. }
        | CallError::CapabilityAmbiguous { .. }
        | CallError::InvalidCapabilityIdentifier { .. }) => WireError::NeverSent(e.to_string()),
        // A request the writer accepted and that got no reply; the client
        // names why with OutcomeUnknownCause.
        CallError::OutcomeUnknown(e) => outcome_unknown(cause, e.to_string()),
        // SubscriptionBackpressure ends a subscription whose request was
        // already sent, and the client counts it as outcome unknown.
        other => WireError::Unknown(other.to_string()),
    }
}

/// Maps a failure on a codemode run's own route, for a program that has no
/// flow scope to be refused one. A provider refusal keeps the provider's own
/// `code`, `message` and `detail`. A flow refusal (`flow_scope_required`,
/// `no_flow_scope`, ...) names a scope the program never had, so reporting
/// basal's wording for it would both describe the wrong thing and drop what
/// the provider said. Flow runs keep their flow-refusal handling by mapping
/// through [`refusal`] instead.
fn run_scope_error(error: CallError) -> WireError {
    match error {
        // Scope-state refusals (`scope_ended`, `scope_changed`, ...) describe
        // the run's own scope, and the supervisor reads them as typed reasons
        // to end the run or retry a withdrawal, so they keep `refusal`'s
        // mapping.
        CallError::Module(body) if body.code.starts_with("scope_") => refusal(body),
        CallError::Module(body) => provider_refusal(body),
        // Everything else keeps `map_error`'s meaning: a request that never
        // left or whose outcome is unknown is not the provider's refusal. A
        // route open on a run's own scope is mapped where it happens
        // ([`bind_error`]), so a flow-coded bind refusal keeps the provider's
        // words too.
        error => map_error(error),
    }
}

/// A failed route open as the caller that opens it should read it. A codemode
/// run's route belongs to a program with no flow scope, so a refusal there
/// keeps the provider's own code and message; a flow route keeps `map_error`'s
/// flow refusal, which the flow's deferral and health read.
fn bind_error(open: WireError, codemode: bool) -> WireError {
    if !codemode {
        return open;
    }
    match open {
        WireError::Refused { code, message } => {
            provider_refusal(subc_protocol::ErrorBody::new(code, message))
        }
        WireError::RefusedDetails {
            code,
            message,
            detail,
        } => provider_refusal(subc_protocol::ErrorBody::new(code, message).with_detail(detail)),
        // The daemon's bind refusal is decoded before this route is opened, so
        // a flow code arrives as a flow refusal. The program reads it under the
        // code the provider sent, with the provider's own message and detail.
        WireError::Typed(refusal) => {
            let message = refusal
                .message
                .as_deref()
                .unwrap_or(refusal.reason.as_str())
                .to_owned();
            let body = subc_protocol::ErrorBody::new(refusal.reason.as_str(), message);
            match &refusal.detail {
                Some(detail) => provider_refusal(body.with_detail(detail.clone())),
                None => provider_refusal(body),
            }
        }
        error => error,
    }
}

/// The refusal as the provider sent it, for a codemode program: its own
/// `code` and `message`, with `detail` when it sent one.
fn provider_refusal(body: subc_protocol::ErrorBody) -> WireError {
    match body.detail {
        Some(detail) => WireError::RefusedDetails {
            code: body.code,
            message: body.message,
            detail,
        },
        None => WireError::Refused {
            code: body.code,
            message: body.message,
        },
    }
}

pub fn refusal(body: subc_protocol::ErrorBody) -> WireError {
    if body.code == "scope_expired" {
        return match body.detail {
            Some(detail) => WireError::RefusedDetails {
                code: body.code,
                message: body.message,
                detail,
            },
            None => WireError::Refused {
                code: body.code,
                message: body.message,
            },
        };
    }
    match crate::flow_refusal::FlowRefusal::decode(&body) {
        Ok(Some(decoded)) => WireError::Typed(decoded),
        Ok(None) => match body.detail {
            Some(detail) => WireError::RefusedDetails {
                code: body.code,
                message: body.message,
                detail,
            },
            None => WireError::Refused {
                code: body.code,
                message: body.message,
            },
        },
        Err(detail) => WireError::Unreadable(detail),
    }
}

/// Records why a sent call's outcome is unknown, from the cause the client
/// attached to `CallError::OutcomeUnknown`.
fn outcome_unknown(cause: Option<OutcomeUnknownCause>, message: String) -> WireError {
    match cause {
        Some(OutcomeUnknownCause::Deadline) => WireError::TimedOut(message),
        Some(
            OutcomeUnknownCause::WriterClosed
            | OutcomeUnknownCause::ConsumerClosed
            | OutcomeUnknownCause::ConnectionFailed
            | OutcomeUnknownCause::RouteEnded,
        )
        | None => WireError::Unknown(message),
        // A completion inside the client was dropped or did not match its
        // request, so basal never received a usable reply.
        Some(OutcomeUnknownCause::CompletionFailed) => WireError::Unreadable(message),
        // The client attaches a cause to its OutcomeUnknown errors. A missing
        // cause still means a request may have left, so it uses Unknown rather
        // than NeverSent. New causes are logged with that same uncertainty.
        Some(other) => {
            tracing::warn!(cause = ?other, %message, "unrecognised subc outcome-unknown cause; recording a lost connection");
            WireError::Unknown(message)
        }
    }
}

pub struct SubcTransport {
    catalog_changes: CatalogChanges,
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
    consumer: SubcConsumer,
    identity: BindIdentity,
    timeout: Duration,
    routes: Arc<Mutex<crate::flow_scope::ScopedRoutes<subc_client_rs::RouteHandle>>>,
    run_watchers: Arc<Mutex<std::collections::HashMap<String, Arc<RunScopeEnded>>>>,
    target_scopes: Mutex<crate::target_scope::TargetScopes>,
    open_gates: Mutex<
        std::collections::HashMap<
            crate::flow_scope::RouteKey,
            std::sync::Weak<tokio::sync::Mutex<()>>,
        >,
    >,
}

#[derive(Default)]
struct CatalogChanges {
    generation: Mutex<Option<Value>>,
    notify: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
impl CatalogChanges {
    fn observe(&self, generation: Value) {
        let changed = {
            let mut previous = self.generation.lock().unwrap_or_else(|p| p.into_inner());
            let changed = previous.as_ref() != Some(&generation);
            *previous = Some(generation);
            changed
        };
        if changed {
            let notify = self
                .notify
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(notify) = notify {
                notify();
            }
        }
    }
}
impl Drop for SubcTransport {
    fn drop(&mut self) {
        // The serving handler can be dropped inside Tokio. Background shutdown
        // cancels held subscriptions without blocking or nesting runtimes.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
impl SubcTransport {
    fn scoped_handle(
        &self,
        flow: &str,
        target: &RouteTarget,
    ) -> Result<Option<subc_client_rs::RouteHandle>, WireError> {
        self.open_scoped_handle(
            flow,
            target,
            None,
            BindIdentity::new("/", "basal", format!("basal:flow:{flow}")),
        )
        .map(Some)
    }

    fn open_scoped_handle(
        &self,
        flow: &str,
        target: &RouteTarget,
        identity: Option<&BindIdentity>,
        bind: BindIdentity,
    ) -> Result<subc_client_rs::RouteHandle, WireError> {
        self.open_scoped_handle_with_budget(flow, target, identity, bind, None)
    }

    /// Custody bounds the entire open attempt, including a wait behind another
    /// opener. Ordinary tool and model opens keep their existing timeout policy.
    fn open_scoped_handle_with_budget(
        &self,
        flow: &str,
        target: &RouteTarget,
        identity: Option<&BindIdentity>,
        bind: BindIdentity,
        budget: Option<Duration>,
    ) -> Result<subc_client_rs::RouteHandle, WireError> {
        self.handle.block_on(async {
            let opening = async {
                let key = {
                    let routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
                    let plan = routes.prepare(flow, target, identity)?;
                    if let Some(handle) = plan.handle {
                        return Ok(handle);
                    }
                    plan.key
                };
                let gate = {
                    let mut gates = self.open_gates.lock().unwrap_or_else(|p| p.into_inner());
                    gates.retain(|_, gate| gate.strong_count() > 0);
                    let gate = gates
                        .get(&key)
                        .and_then(std::sync::Weak::upgrade)
                        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
                    gates.insert(key, Arc::downgrade(&gate));
                    gate
                };
                // Only opens for this route identity wait for one another. Control
                // pushes and other providers never wait on a network round trip.
                let _opening = gate.lock().await;
                let plan = self
                    .routes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .prepare(flow, target, identity)?;
                if let Some(handle) = plan.handle {
                    return Ok(handle);
                }
                let mut options = CallOptions {
                    timeout: budget.unwrap_or(self.timeout),
                    ..Default::default()
                };
                if flow.starts_with("codemode:")
                    && matches!(target, RouteTarget::ToolProvider { .. })
                {
                    options = run_tool_route_options(options);
                }
                let handle = self
                    .consumer
                    .open_route_scoped(target.clone(), bind, plan.scope.selector(), options)
                    .await
                    .map_err(|e| bind_error(map_error(e), flow.starts_with("codemode:")))?;
                let result = self
                    .routes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .install(flow, plan, handle);
                if !result.as_ref().is_ok_and(|installed| *installed == handle) {
                    let _ = self
                        .consumer
                        .close_handle(&handle, Default::default())
                        .await;
                }
                result
            };
            if let Some(budget) = budget {
                tokio::time::timeout(budget, opening)
                    .await
                    .map_err(|_| WireError::TimedOut("custody route open timed out".into()))?
            } else {
                opening.await
            }
        })
    }

    fn call_for_flow(
        &self,
        flow: &str,
        target: RouteTarget,
        bytes: Vec<u8>,
        management: bool,
        module: &str,
        action: &str,
    ) -> Result<Value, WireError> {
        let result = match self
            .scoped_handle(flow, &target)
            .map_err(|e| contextual(e, module, action))?
        {
            Some(handle) => {
                let result = self.handle.block_on(self.consumer.request(
                    &handle,
                    bytes,
                    CallOptions {
                        timeout: self.timeout,
                        ..Default::default()
                    },
                ));
                if result
                    .as_ref()
                    .is_err_and(|e| target_route_ended(flow, e.outcome_cause()))
                {
                    // The client can report a route end before the control-push reader
                    // installs its tombstone. Do not resend a target mutation in that gap.
                    self.routes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .closed(handle, true);
                    return Err(WireError::ScopeCut("target route ended during call".into()));
                }
                if let Err(CallError::StaleRouteHandle(_)) = &result {
                    // A stale connection handle says nothing about scope expiry.
                    // Only a daemon scope-close push may leave a tombstone.
                    self.routes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .closed(handle, false);
                }
                let mapped = result.map_err(map_error);
                if mapped.as_ref().is_err_and(|e| e.unknown_reason().is_some())
                    && flow.starts_with("target:")
                    && self
                        .routes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .ended(flow)
                {
                    return Err(WireError::ScopeCut("scope closed during call".into()));
                }
                mapped.and_then(|bytes| decode_response(&bytes, management))
            }
            None => Err(WireError::Typed(crate::flow_refusal::FlowRefusal::new(
                crate::flow_refusal::RefusalReason::NoFlowScope,
                module,
                action,
            ))),
        };
        result.map_err(|e| contextual(e, module, action))
    }

    /// Must be constructed and used on blocking threads, not inside a Tokio task.
    pub fn connect(timeout: Duration) -> Result<Arc<Self>, WireError> {
        Self::connect_with_file(timeout, None, None)
    }
    fn connect_with_file(
        timeout: Duration,
        file: Option<&std::path::Path>,
        on_close: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Arc<Self>, WireError> {
        let runtime =
            tokio::runtime::Runtime::new().map_err(|e| WireError::NeverSent(e.to_string()))?;
        let opts = ConsumerOptions {
            call_timeout: timeout,
            ..Default::default()
        };
        let consumer = match file {
            Some(file) => runtime.block_on(SubcConsumer::connect(file, opts)),
            None => runtime.block_on(SubcConsumer::connect_default(opts)),
        }
        .map_err(|e| WireError::NeverSent(e.to_string()))?;
        let routes = Arc::new(Mutex::new(crate::flow_scope::ScopedRoutes::<
            subc_client_rs::RouteHandle,
        >::default()));
        let route_events = routes.clone();
        let run_watchers: Arc<Mutex<std::collections::HashMap<String, Arc<RunScopeEnded>>>> =
            Arc::default();
        let scope_events = run_watchers.clone();
        let restored_routes = routes.clone();
        consumer.on_connection_state(move |state| {
            if matches!(state, ConnectionState::Restored { .. }) {
                restored_routes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .connection_restored();
            }
        });
        let mut pushes = consumer.control_pushes(256);
        runtime.spawn(async move {
            while let Some(push) = pushes.recv().await {
                if push.op != "route.closed" {
                    continue;
                }
                let mut routes = route_events.lock().unwrap_or_else(|p| p.into_inner());
                routes.control_push(&push, |handle| handle.channel);
                let ended = scope_events
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .filter(|(run, _)| routes.ended(run))
                    .map(|(_, ended)| ended.clone())
                    .collect::<Vec<_>>();
                drop(routes);
                let reason = push
                    .body
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("scope_ended");
                for ended in ended {
                    ended(reason);
                }
                if let Some(on_close) = &on_close {
                    on_close();
                }
            }
        });
        Ok(Arc::new(Self {
            catalog_changes: CatalogChanges::default(),
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            consumer,
            // Core's answer ownership is principal plus session. This label must
            // remain stable across process restarts, not include a process id.
            identity: BindIdentity::new("/", "basal", "basal:flows"),
            timeout,
            routes,
            run_watchers,
            open_gates: Mutex::new(std::collections::HashMap::new()),
            target_scopes: Mutex::new(crate::target_scope::TargetScopes::default()),
        }))
    }
    fn call(&self, target: RouteTarget, body: Value, management: bool) -> Result<Value, WireError> {
        let bytes = serde_json::to_vec(&body).map_err(|e| WireError::NeverSent(e.to_string()))?;
        self.call_as(target, self.identity.clone(), bytes, management)
    }
    fn call_as(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        bytes: Vec<u8>,
        management: bool,
    ) -> Result<Value, WireError> {
        // The client caches routes by target and identity and only retries a
        // dead channel when the daemon proves it was not forwarded.
        let response = self
            .handle
            .block_on(self.consumer.call(
                target,
                identity,
                bytes,
                CallOptions {
                    timeout: self.timeout,
                    ..Default::default()
                },
            ))
            .map_err(map_error)?;
        decode_response(&response, management)
    }
}

fn target_route_ended(flow: &str, cause: Option<OutcomeUnknownCause>) -> bool {
    flow.starts_with("target:") && matches!(cause, Some(OutcomeUnknownCause::RouteEnded))
}
impl SubcTransport {
    fn model_handle(
        &self,
        flow: &str,
        target: &RouteTarget,
        identity: BindIdentity,
    ) -> Result<subc_client_rs::RouteHandle, WireError> {
        self.open_scoped_handle(flow, target, Some(&identity), identity.clone())
    }

    /// Sends to Broca as basal without the flow's scope. Only the test rig's
    /// build has this, to prove that Broca refuses such a send; production
    /// basal never sends a flow's model call unscoped.
    #[cfg(feature = "rig-kill-hook")]
    pub(crate) fn unscoped_model_send(
        &self,
        identity: BindIdentity,
        module: &str,
        params: &[u8],
    ) -> Result<Value, WireError> {
        self.call_as(
            RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
            identity,
            management_bytes("session.send", params)?,
            true,
        )
    }

    pub(crate) fn management_for_model(
        &self,
        flow: &str,
        identity: BindIdentity,
        module: &str,
        op: &str,
        params: &[u8],
    ) -> Result<Value, WireError> {
        let target = RouteTarget::ManagementSurface {
            module_id: module.into(),
        };
        let codemode = flow.starts_with("codemode:");
        let handle = self
            .model_handle(flow, &target, identity)
            // `contextual` would turn a plain refusal back into a flow refusal,
            // so the codemode route's mapping is final.
            .map_err(|e| {
                if codemode {
                    bind_error(e, true)
                } else {
                    contextual(e, module, op)
                }
            })?;
        let result = self.handle.block_on(self.consumer.request(
            &handle,
            management_bytes(op, params)?,
            CallOptions {
                timeout: self.timeout,
                ..Default::default()
            },
        ));
        if let Err(CallError::StaleRouteHandle(_)) = &result {
            self.routes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .closed(handle, false);
        }
        result
            .map_err(if codemode { run_scope_error } else { map_error })
            .and_then(|bytes| decode_response(&bytes, true))
            .map_err(|e| {
                if codemode {
                    e
                } else {
                    contextual(e, module, op)
                }
            })
    }

    pub(crate) fn release_model(&self, flow: &str, identity: BindIdentity, module: &str) {
        let handle = self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .release_identity(
                flow,
                &RouteTarget::ManagementSurface {
                    module_id: module.into(),
                },
                &identity,
            );
        if let Some(handle) = handle {
            let _ = self
                .handle
                .block_on(self.consumer.close_handle(&handle, Default::default()));
        }
    }
}
impl Transport for SubcTransport {
    fn flow_read_exact(
        &self,
        scope: &subc_protocol::scope::ScopeSelector,
        identity: &BindIdentity,
        path: &str,
    ) -> Result<Value, WireError> {
        self.handle.block_on(async {
            let handle = self
                .consumer
                // AFT serves file.read_exact on its tool-provider route and
                // refuses its management surface under a flow-read scope, so
                // the child scope can never reach AFT's management operations.
                .open_route_scoped(
                    RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    identity.clone(),
                    scope.clone(),
                    CallOptions {
                        timeout: self.timeout,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| {
                    // Route admission is a typed refusal too; do not flatten its code.
                    if let Some(body) = error.route_open_refusal() {
                        refusal(body.clone())
                    } else {
                        map_error(error)
                    }
                })?;
            let result = self
                .consumer
                .request(
                    &handle,
                    // A flat request AFT intercepts before tool-call parsing.
                    // Success is the bare {bytes_base64, size, sha256}; every
                    // refusal is a protocol Error frame with AFT's code.
                    serde_json::to_vec(&json!({"op":"file.read_exact","path":path}))
                        .expect("file read request"),
                    CallOptions {
                        timeout: self.timeout,
                        ..Default::default()
                    },
                )
                .await
                .map_err(map_error)
                .and_then(|bytes| decode_response(&bytes, false));
            let _ = self
                .consumer
                .close_handle(&handle, Default::default())
                .await;
            result
        })
    }
    fn target_flow(
        &self,
        request: &crate::CallRequest,
        call: &crate::target_scope::TargetCall,
    ) -> Result<String, WireError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| WireError::NeverSent(e.to_string()))?
            .as_millis();
        let mut cache = self.target_scopes.lock().unwrap_or_else(|p| p.into_inner());
        let result = cache.resolve(
            &request.flow_id, &request.run_id, call, i64::try_from(now).unwrap_or(i64::MAX),
            |scope| self.routes.lock().unwrap_or_else(|p| p.into_inner()).ended(&scope.route_key()),
            || self.management(crate::subc_catalog::CORE, "flow.target_scope", json!({"flow_id":request.flow_id,"version":call.version,"agent":call.agent,"expires_at_ms":call.expires_at_ms})),
        );
        let retired = cache.take_retired();
        drop(cache);
        for key in retired {
            self.configure_flow(&key, None);
        }
        let scope = result?;
        let targets = self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .targets(&request.route_key())
            .ok_or_else(|| WireError::NeverSent("flow scope targets unavailable".into()))?;
        let key = scope.route_key();
        self.configure_flow(
            &key,
            Some(crate::flow_scope::RegisteredScope {
                selector: scope.selector(),
                targets,
            }),
        );
        Ok(key)
    }
    fn grant_would_ask(
        &self,
        flow: &str,
        version: u32,
        agent: Option<&str>,
        module: &str,
        grant: &str,
    ) -> Result<bool, WireError> {
        let mut params = json!({"flow_id":flow,"items":[{"grant":grant}]});
        if let Some(agent) = agent {
            params["agent_id"] = json!(agent);
        }
        let reply = self.call_for_flow(
            &crate::flow_scope::flow_route_key(flow, version),
            RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
            serde_json::to_vec(&management_body("grants.would_ask", params))
                .map_err(|e| WireError::NeverSent(e.to_string()))?,
            true,
            module,
            "grants.would_ask",
        )?;
        grant_verdict(&reply, grant)
    }
    fn provider_ready(
        &self,
        flow: &str,
        module: &str,
        action: &str,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ready(flow, module, action)
    }
    fn configure_flow(&self, flow: &str, scope: Option<crate::flow_scope::RegisteredScope>) {
        if scope.is_none() {
            self.run_watchers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(flow);
        }
        let dropped = self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .configure(flow, scope);
        for handle in dropped {
            let _ = self
                .handle
                .block_on(self.consumer.close_handle(&handle, Default::default()));
        }
    }
    fn retain_flow_routes(&self, flow_id: &str, live: &std::collections::HashSet<String>) {
        let dropped = self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain_flow_routes_for(flow_id, live);
        for handle in dropped {
            let _ = self
                .handle
                .block_on(self.consumer.close_handle(&handle, Default::default()));
        }
    }
    fn watch_run_scope(&self, run: &str, ended: Arc<RunScopeEnded>) {
        self.run_watchers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(run.into(), ended);
    }
    fn management_for_flow(
        &self,
        flow: &str,
        module: &str,
        op: &str,
        params: Value,
    ) -> Result<Value, WireError> {
        self.call_for_flow(
            flow,
            RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
            serde_json::to_vec(&management_body(op, params))
                .map_err(|e| WireError::NeverSent(e.to_string()))?,
            true,
            module,
            op,
        )
    }
    fn tool_for_flow(
        &self,
        flow: &str,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError> {
        self.call_for_flow(
            flow,
            RouteTarget::ToolProvider {
                module_id: module.into(),
            },
            serde_json::to_vec(&tool_body(name, arguments, call_key)?)
                .map_err(|e| WireError::NeverSent(e.to_string()))?,
            false,
            module,
            name,
        )
    }
    fn tool_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
        options: &RunToolOptions,
    ) -> Result<Value, WireError> {
        let target = RouteTarget::ToolProvider {
            module_id: module.into(),
        };
        let mut request = tool_body(name, arguments, call_key)?;
        if let Some(preset) = &options.preset {
            request["preset"] = Value::String(preset.clone());
        }
        let body = serde_json::to_vec(&request).map_err(|e| WireError::NeverSent(e.to_string()))?;
        // A re-opened attempt gets only what is left of the call's reply time.
        let deadline = std::time::Instant::now() + options.reply_timeout;
        // The program calling this tool owns no flow scope, so its route's
        // refusals reach it as the provider sent them.
        send_run_request(
            || {
                self.open_scoped_handle(
                    run,
                    &target,
                    Some(&options.identity),
                    options.identity.clone(),
                )
                .map_err(|error| contextual(error, module, name))
            },
            |handle| {
                self.handle.block_on(
                    self.consumer.request(
                        &handle,
                        body.clone(),
                        CallOptions {
                            timeout: deadline
                                .saturating_duration_since(std::time::Instant::now())
                                .max(Duration::from_millis(1)),
                            ..Default::default()
                        },
                    ),
                )
            },
            |handle| {
                self.routes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed(handle, false);
            },
        )
        .and_then(|bytes| decode_response(&bytes, false))
    }
    fn role_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        arguments: Value,
        options: &RunToolOptions,
    ) -> Result<Value, WireError> {
        let target = RouteTarget::ToolProvider {
            module_id: module.into(),
        };
        let custody_deadline =
            std::time::Instant::now() + options.reply_timeout.min(Duration::from_secs(2));
        let handle = self.open_scoped_handle_with_budget(
            run,
            &target,
            Some(&options.identity),
            options.identity.clone(),
            Some(custody_deadline.saturating_duration_since(std::time::Instant::now())),
        )?;
        // Custody requests carry no preset: the tool-provider role gives it
        // no meaning on these ops.
        let body = serde_json::to_vec(&subc_protocol::tool_call::ToolCallRequest::new(
            name, arguments,
        ))
        .map_err(|e| WireError::NeverSent(e.to_string()))?;
        let remaining = custody_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(WireError::TimedOut(
                "custody request deadline passed".into(),
            ));
        }
        let reply = self.handle.block_on(self.consumer.request(
            &handle,
            body,
            CallOptions {
                timeout: remaining,
                ..Default::default()
            },
        ));
        if matches!(&reply, Err(CallError::StaleRouteHandle(_))) {
            self.routes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .closed(handle, false);
        }
        let bytes = reply.map_err(run_scope_error)?;
        decode_response(&bytes, false)
    }
    fn catalog(&self) -> Result<Value, WireError> {
        let catalog = self
            .handle
            .block_on(self.consumer.catalog_list())
            .map_err(map_error)?;
        self.catalog_changes.observe(json!(catalog.generation));
        Ok(
            json!({"generation": catalog.generation, "modules": catalog.modules, "subc_ops": catalog.subc_ops}),
        )
    }
    fn on_catalog_change(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        *self
            .catalog_changes
            .notify
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(notify);
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.call(
            RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
            management_body(op, params),
            true,
        )
    }
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError> {
        let body = tool_body(name, arguments, call_key)?;
        self.call(
            RouteTarget::ToolProvider {
                module_id: module.into(),
            },
            body,
            false,
        )
    }
    fn role_on_run_scope(
        &self,
        scope: &subc_protocol::scope::ScopeSelector,
        identity: &BindIdentity,
        module: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, WireError> {
        // A role request names no call key and no preset in its envelope.
        let body = serde_json::to_vec(&subc_protocol::tool_call::ToolCallRequest::new(
            name, arguments,
        ))
        .map_err(|e| WireError::NeverSent(e.to_string()))?;
        let options = || CallOptions {
            timeout: self.timeout,
            ..Default::default()
        };
        self.handle.block_on(async {
            let handle = self
                .consumer
                // The same route shape the run's own tool calls open: the
                // run's scope and the consumer capabilities a run declares,
                // so the provider answers for the binds the program will use.
                .open_route_scoped(
                    RouteTarget::ToolProvider {
                        module_id: module.into(),
                    },
                    identity.clone(),
                    scope.clone(),
                    run_tool_route_options(options()),
                )
                .await
                .map_err(|error| {
                    // Keep the provider's bind refusal code (for example
                    // `principal_denied`), so the caller can tell a permanent
                    // refusal from a transient one. This route belongs to a
                    // program with no flow scope, so a flow code in it is the
                    // provider's own refusal, not a flow-scope diagnosis.
                    match error.route_open_refusal() {
                        Some(body) => provider_refusal(body.clone()),
                        None => map_error(error),
                    }
                })?;
            let result = self
                .consumer
                .request(&handle, body, options())
                .await
                .map_err(run_scope_error)
                .and_then(|bytes| decode_response(&bytes, false));
            // Callers pass identities that are not reused (one per codemode
            // run), so the route is closed rather than left in the client's
            // route cache.
            let _ = self
                .consumer
                .close_handle(&handle, Default::default())
                .await;
            result
        })
    }
}

/// The consumer capabilities every tool-provider route of a codemode run
/// declares: tool-result/v1 results and held replies.
fn run_tool_route_options(options: CallOptions) -> CallOptions {
    options
        .with_consumer_capability(cortexkit_role_tool_provider::TOOL_RESULT_V1_CAPABILITY)
        .expect("static capability")
        .with_consumer_capability("held-reply/v1")
        .expect("static capability")
}

/// Sends one codemode tool request, opening (or reusing) its route with `open`
/// and sending with `send`. When the request provably never left basal, the
/// route is opened again and the request sent once more; a second such failure
/// is returned. Two failures qualify:
/// - the daemon refused the bind `scope_changed` or `scope_not_synced` (the
///   client has already retried these until its own deadline). Either means
///   no route was bound, so no request was sent; `scope_changed` means the run
///   scope's record changed while the bind was in flight, for example when
///   its owner re-pointed the scope's parent;
/// - the request was refused `StaleRouteHandle`, which the client returns
///   before writing a request frame: the cached route belongs to a closed
///   route or to a connection that has since been restored. `evict` drops it
///   so the next open binds a fresh route.
///
/// Every other failure, including a `scope_changed` error frame the provider
/// sent after receiving the request, is returned without a resend: that
/// request may have reached the provider.
///
/// A refusal of the sent request is returned as [`run_scope_error`] maps it,
/// so the program reads the provider's own code and message; a refused bind is
/// mapped by [`bind_error`] where the route is opened.
fn send_run_request<H: Copy>(
    mut open: impl FnMut() -> Result<H, WireError>,
    mut send: impl FnMut(H) -> Result<Vec<u8>, CallError>,
    mut evict: impl FnMut(H),
) -> Result<Vec<u8>, WireError> {
    use crate::flow_refusal::RefusalReason;
    let mut reopened = false;
    loop {
        let handle = match open() {
            Ok(handle) => handle,
            Err(WireError::Typed(refusal))
                if !reopened
                    && matches!(
                        refusal.reason,
                        RefusalReason::ScopeChanged | RefusalReason::ScopeNotSynced
                    ) =>
            {
                reopened = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        match send(handle) {
            Err(error @ CallError::StaleRouteHandle(_)) => {
                evict(handle);
                if reopened {
                    return Err(run_scope_error(error));
                }
                reopened = true;
            }
            result => return result.map_err(run_scope_error),
        }
    }
}

pub fn contextual(error: WireError, module: &str, action: &str) -> WireError {
    match error {
        WireError::Typed(mut refusal) => {
            refusal.provider = module.into();
            refusal.action = action.into();
            WireError::Typed(refusal)
        }
        WireError::Refused { code, message } => {
            let error = refusal(subc_protocol::ErrorBody::new(code, message));
            if matches!(error, WireError::Typed(_)) {
                contextual(error, module, action)
            } else {
                error
            }
        }
        WireError::RefusedDetails {
            code,
            message,
            detail,
        } => {
            let error = refusal(subc_protocol::ErrorBody::new(code, message).with_detail(detail));
            if matches!(error, WireError::Typed(_)) {
                contextual(error, module, action)
            } else {
                error
            }
        }
        other => other,
    }
}

fn grant_verdict(reply: &Value, grant: &str) -> Result<bool, WireError> {
    let bad = || WireError::Unreadable("invalid grant diagnostic reply".into());
    let items = reply["items"].as_array().ok_or_else(bad)?;
    if items.len() != 1
        || items[0]["grant"].as_str() != Some(grant)
        || items[0]["reason"].as_str().is_none()
    {
        return Err(bad());
    }
    match items[0]["verdict"].as_str() {
        Some("yes") => Ok(true),
        Some("no" | "would_ask") => Ok(false),
        _ => Err(bad()),
    }
}

fn decode_response(response: &[u8], management: bool) -> Result<Value, WireError> {
    let mut value: Value =
        serde_json::from_slice(response).map_err(|e| WireError::Unreadable(e.to_string()))?;
    if !management {
        return Ok(value);
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| WireError::Unreadable("management reply is not an object".into()))?;
    if let Some(error) = object.remove("error") {
        let body: subc_protocol::ErrorBody = serde_json::from_value(error)
            .map_err(|e| WireError::Unreadable(format!("invalid error envelope: {e}")))?;
        return Err(refusal(body));
    }
    object
        .remove("result")
        .ok_or_else(|| WireError::Unreadable("missing management result".into()))
}

pub fn management_body(op: &str, params: Value) -> Value {
    json!({"method":op,"params":params})
}
pub fn tool_body(name: &str, arguments: Value, call_key: &str) -> Result<Value, WireError> {
    let mut request = subc_protocol::tool_call::ToolCallRequest::new(name, arguments);
    request.call_key = Some(call_key.to_owned());
    serde_json::to_value(request).map_err(|e| WireError::NeverSent(e.to_string()))
}

/// Validate frozen parameters as an object, then embed their original bytes
/// without reserializing them (spacing and field order are preserved).
pub fn management_bytes(op: &str, params: &[u8]) -> Result<Vec<u8>, WireError> {
    let value: Value =
        serde_json::from_slice(params).map_err(|e| WireError::NeverSent(e.to_string()))?;
    if !value.is_object() {
        return Err(WireError::NeverSent(
            "management parameters must be an object".into(),
        ));
    }
    let method = serde_json::to_vec(op).map_err(|e| WireError::NeverSent(e.to_string()))?;
    let mut body = b"{\"method\":".to_vec();
    body.extend(method);
    body.extend(b",\"params\":");
    body.extend(params);
    body.push(b'}');
    Ok(body)
}

#[cfg(test)]
#[path = "run_scope_wire_tests.rs"]
mod run_scope_wire_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UnknownReason;

    #[test]
    fn observed_catalog_generation_wakes_once_per_change() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let changes = CatalogChanges::default();
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        *changes.notify.lock().unwrap() = Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        }));
        changes.observe(json!(1));
        changes.observe(json!(1));
        changes.observe(json!(2));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    fn registered() -> crate::flow_scope::RegisteredScope {
        crate::flow_scope::RegisteredScope {
            selector: crate::flow_scope::FlowScope {
                owner: subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                scope_ref: "flow:stale".into(),
                epoch: 1,
            },
            targets: ["broca".into()].into_iter().collect(),
        }
    }
    #[test]
    fn opening_a_route_does_not_hold_the_control_table_lock() {
        let (entered, received) = std::sync::mpsc::sync_channel(1);
        let (release, paused) = tokio::sync::oneshot::channel();
        with_paused_open(Some((entered, paused)), |transport, _, _, _| {
            transport.configure_flow("a", Some(registered()));
            let opening = transport.clone();
            let thread = std::thread::spawn(move || {
                opening.scoped_handle(
                    "a",
                    &RouteTarget::ManagementSurface {
                        module_id: "broca".into(),
                    },
                )
            });
            received.recv_timeout(Duration::from_secs(10)).unwrap();
            // The daemon has not replied yet; control pushes must still be able
            // to acquire the table. Release and join before reporting failure.
            let unlocked = transport.routes.try_lock().is_ok();
            release.send(()).unwrap();
            thread.join().unwrap().unwrap();
            assert!(
                unlocked,
                "route open held the control table while awaiting the daemon"
            );
        });
    }
    #[test]
    fn stale_handle_does_not_leave_a_scope_tombstone() {
        with_scoped_transport(|transport, _, _, _| {
            let scope = registered();
            transport.configure_flow("a", Some(scope.clone()));
            let target = RouteTarget::ManagementSurface {
                module_id: "broca".into(),
            };
            let handle = transport.scoped_handle("a", &target).unwrap().unwrap();
            transport
                .handle
                .block_on(transport.consumer.close_handle(&handle, Default::default()))
                .unwrap();
            assert!(matches!(
                transport.management_for_flow("a", "broca", "echo", json!({})),
                Err(WireError::NeverSent(_))
            ));
            // A reconnect and the same core selector must not preserve a local stale handle.
            transport.routes.lock().unwrap().connection_restored();
            transport.configure_flow("a", Some(scope));
            assert_eq!(
                transport
                    .management_for_flow("a", "broca", "echo", json!({"n":1}))
                    .unwrap(),
                json!({"n":1})
            );
        });
    }
    #[test]
    fn stale_model_request_can_reopen_without_a_scope_epoch_change() {
        with_scoped_transport(|transport, _, _, _| {
            transport.configure_flow("a", Some(registered()));
            let target = RouteTarget::ManagementSurface {
                module_id: "broca".into(),
            };
            let identity = BindIdentity::new("/", "basal", "model");
            let handle = transport
                .model_handle("a", &target, identity.clone())
                .unwrap();
            transport
                .handle
                .block_on(transport.consumer.close_handle(&handle, Default::default()))
                .unwrap();
            assert!(matches!(
                transport.management_for_model("a", identity.clone(), "broca", "echo", b"{}"),
                Err(WireError::NeverSent(_))
            ));
            assert_eq!(
                transport
                    .management_for_model("a", identity, "broca", "echo", b"{}")
                    .unwrap(),
                json!({})
            );
        });
    }
    #[test]
    fn model_refusal_has_provider_and_action_after_envelope_decoding() {
        with_scoped_transport(|transport, _, _, _| {
            transport.configure_flow("a", Some(registered()));
            let result = transport.management_for_model(
                "a",
                BindIdentity::new("/", "basal", "model"),
                "broca",
                "refuse",
                b"{}",
            );
            assert!(
                matches!(result, Err(WireError::Typed(refusal)) if refusal.provider == "broca" && refusal.action == "refuse")
            );
        });
    }

    #[test]
    fn stale_handle_mapping_keeps_the_client_handle_detail() {
        with_scoped_transport(|transport, _, _, _| {
            transport.configure_flow("a", Some(registered()));
            let handle = transport
                .scoped_handle(
                    "a",
                    &RouteTarget::ManagementSurface {
                        module_id: "broca".into(),
                    },
                )
                .unwrap()
                .unwrap();
            assert!(
                matches!(map_error(CallError::StaleRouteHandle(handle)), WireError::NeverSent(detail) if detail.contains(&format!("{handle:?}")))
            );
        });
    }
    #[test]
    fn malformed_management_errors_are_unreadable_not_connection_loss() {
        for bytes in [
            br#"{"error":{}}"#.as_slice(),
            br#"{"error":{"code":"resource_busy","message":"busy"}}"#.as_slice(),
        ] {
            assert_eq!(
                decode_response(bytes, true).unwrap_err().unknown_reason(),
                Some(UnknownReason::ReplyUnreadable)
            );
        }
    }

    fn with_scoped_transport(
        test: impl FnOnce(
            Arc<SubcTransport>,
            Arc<Mutex<Vec<Value>>>,
            Arc<Mutex<Vec<(u16, Value)>>>,
            std::sync::mpsc::Receiver<()>,
        ),
    ) {
        with_paused_open(None, test);
    }

    fn with_paused_open(
        pause: Option<(
            std::sync::mpsc::SyncSender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
        test: impl FnOnce(
            Arc<SubcTransport>,
            Arc<Mutex<Vec<Value>>>,
            Arc<Mutex<Vec<(u16, Value)>>>,
            std::sync::mpsc::Receiver<()>,
        ),
    ) {
        with_paused_open_timeout(pause, Duration::from_secs(10), test);
    }

    /// How long the fixture daemon holds a `slow_echo` reply. A test that
    /// passes a shorter `call_timeout` to `with_paused_open_timeout` gets the
    /// reply only if the call's own `reply_timeout` replaces that timeout.
    const SLOW_ECHO_DELAY: Duration = Duration::from_millis(500);

    /// `with_paused_open` with `call_timeout` as the consumer's default
    /// per-call timeout instead of 10 s.
    fn with_paused_open_timeout(
        mut pause: Option<(
            std::sync::mpsc::SyncSender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
        call_timeout: Duration,
        test: impl FnOnce(
            Arc<SubcTransport>,
            Arc<Mutex<Vec<Value>>>,
            Arc<Mutex<Vec<(u16, Value)>>>,
            std::sync::mpsc::Receiver<()>,
        ),
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        use subc_protocol::{Flags, Frame, FrameType, Priority};
        use subc_transport::connection_file::{
            ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic,
        };
        use subc_transport::{authenticate_server, read_frame, write_frame};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "basal-scoped-wire-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("subc-connection.json");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: address.port(),
            }],
            key: vec![7; 32],
            daemon_id: [9; 16],
            pid: std::process::id(),
            daemon_ver: "scoped-test".into(),
        };
        write_atomic(&file, &info).unwrap();
        let opens = Arc::new(Mutex::new(Vec::<Value>::new()));
        let records = opens.clone();
        let calls = Arc::new(Mutex::new(Vec::<(u16, Value)>::new()));
        let recorded_calls = calls.clone();
        let scope_closed = Arc::new(tokio::sync::Notify::new());
        let close_applied = scope_closed.clone();
        let daemon = runtime.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            authenticate_server(
                &mut stream,
                &info.key,
                &info.daemon_id,
                "scoped-test",
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            while let Some(frame) = read_frame(&mut stream).await.unwrap() {
                if frame.header.ty != FrameType::Request {
                    continue;
                }
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                recorded_calls.lock().unwrap().push((frame.header.channel,body.clone()));
                if body["method"] == "cut_scope" {
                    let push=Frame::build(FrameType::Push,Flags::new(false,Priority::Passive,false),0,0,0,
                        serde_json::to_vec(&json!({"op":"route.closed","channels":[frame.header.channel],"reason":"scope_ended"})).unwrap()).unwrap();
                    write_frame(&mut stream,&push).await.unwrap();
                    scope_closed.notified().await;
                    break;
                }
                let end_scope = body["method"] == "end_scope";
                if body["name"] == "changed_after_send" {
                    // The provider received the request and answers with an
                    // Error frame whose code is scope_changed.
                    let error = Frame::build(
                        FrameType::Error,
                        Flags::new(false, Priority::Passive, false),
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr,
                        serde_json::to_vec(&json!({"code":"scope_changed","message":"changed"}))
                            .unwrap(),
                    )
                    .unwrap();
                    write_frame(&mut stream, &error).await.unwrap();
                    continue;
                }
                if body["op"] == "route.open" && let Some((entered, release)) = pause.take() {
                    entered.send(()).unwrap();
                    release.await.unwrap();
                }
                let reply = if body["op"] == "route.open" {
                    let mut records = records.lock().unwrap();
                    records.push(body);
                    json!({"op":"route.open","route_channel":10+records.len(),"route_epoch":1})
                } else if body["method"] == "grants.would_ask" {
                    json!({"result":{"items":[{"grant":body["params"]["items"][0]["grant"],"verdict":"yes","reason":"allowed"}]}})
                } else if body["method"] == "refuse" {
                    json!({"error":{"code":"scope_ended","message":"ended"}})
                } else if body["method"] == "session.send" {
                    json!({"result":{"state":"active","run_id":format!("broca-run:{}",body["params"]["send_id"].as_str().unwrap())}})
                } else if body["method"]=="route.select" {
                    json!({"result":{"selected":{"model":{"providerID":"registry","modelID":"model"}},"decisionID":"selection-1","runner":{"provider":"runner","model":"model"}}})
                } else if body["method"]=="run.result" {
                    json!({"result":{"run_id":body["params"]["run_id"],"state":"completed","final_message":{"ordinal":0,"mid":"m","text":"answer"}}})
                } else if body["method"]=="run.status" {
                    json!({"result":{"state":"completed"}})
                } else if body["method"]=="route.set_decision_outcome" {
                    json!({"result":{"ok":true}})
                } else if body["op"] == "file.read_exact" {
                    // AFT answers the flat read with a bare object, unwrapped.
                    json!({"path":body["path"]})
                } else if body["name"] == "echo" || body["name"] == "slow_echo" {
                    if body["name"] == "slow_echo" { tokio::time::sleep(SLOW_ECHO_DELAY).await; }
                    body["arguments"].clone()
                } else {
                    json!({"result":body["params"]})
                };
                let response = Frame::build(
                    FrameType::Response,
                    Flags::new(false, Priority::Passive, false),
                    frame.header.channel,
                    frame.header.epoch,
                    frame.header.corr,
                    serde_json::to_vec(&reply).unwrap(),
                )
                .unwrap();
                write_frame(&mut stream, &response).await.unwrap();
                if end_scope {
                    let push = Frame::build(
                        FrameType::Push,
                        Flags::new(false, Priority::Passive, false),
                        0,
                        0,
                        0,
                        serde_json::to_vec(
                            &json!({"op":"route.closed","channels":[11],"reason":"scope_expired"}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    write_frame(&mut stream, &push).await.unwrap();
                }
            }
        });
        let (closed, close_seen) = std::sync::mpsc::sync_channel(1);
        let transport = SubcTransport::connect_with_file(
            call_timeout,
            Some(&file),
            Some(Arc::new(move || {
                closed.send(()).unwrap();
                close_applied.notify_one();
            })),
        )
        .unwrap();
        test(transport.clone(), opens, calls, close_seen);
        daemon.abort();
        drop(transport);
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flow_file_reads_open_aft_on_the_exact_invoking_scope() {
        with_scoped_transport(|transport, opens, calls, _| {
            let selector = subc_protocol::scope::ScopeSelector::new(
                subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                "flow-read/call",
                Some(1),
            );
            let identity = BindIdentity::new("/agent/project", "agent", "session");
            for path in ["flow.js", "flow.json"] {
                assert_eq!(
                    transport
                        .flow_read_exact(&selector, &identity, path)
                        .unwrap(),
                    json!({"path":path})
                );
            }
            let opens = opens.lock().unwrap();
            assert_eq!(opens.len(), 2);
            for open in opens.iter() {
                assert_eq!(
                    open["target"],
                    json!({"kind":"tool_provider","module_id":"aft"})
                );
                assert_eq!(open["scope"], serde_json::to_value(&selector).unwrap());
                assert_eq!(open["identity"], serde_json::to_value(&identity).unwrap());
            }
            let calls = calls.lock().unwrap();
            let reads: Vec<_> = calls
                .iter()
                .filter(|(_, body)| body["op"] == "file.read_exact")
                .collect();
            assert_eq!(reads.len(), 2);
            assert_eq!(reads[0].0, 11);
            assert_eq!(reads[1].0, 12);
        });
    }

    #[test]
    fn role_on_run_scope_binds_the_run_scope_with_no_call_key_and_closes_it() {
        with_scoped_transport(|transport, opens, calls, _| {
            let identity = BindIdentity::new("/agent/project", "runner", "basal:codemode:run-1");
            let scope = subc_protocol::scope::ScopeSelector::new(
                subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                "run-1",
                Some(19),
            );
            assert_eq!(
                transport
                    .role_on_run_scope(&scope, &identity, "aft", "echo", json!({"n":1}))
                    .unwrap(),
                json!({"n":1})
            );
            let opens = opens.lock().unwrap();
            assert_eq!(opens.len(), 1);
            assert_eq!(opens[0]["scope"], serde_json::to_value(&scope).unwrap());
            assert_eq!(
                opens[0]["identity"],
                serde_json::to_value(&identity).unwrap()
            );
            let calls = calls.lock().unwrap();
            let sent = calls
                .iter()
                .find(|(_, body)| body["name"] == "echo")
                .unwrap();
            assert_eq!(sent.0, 11, "sent on the route opened under the run scope");
            assert!(sent.1.get("call_key").is_none(), "{}", sent.1);
        });
    }

    #[test]
    fn grant_polling_never_uses_an_unscoped_route() {
        with_scoped_transport(|transport, opens, calls, _| {
            assert!(
                transport
                    .grant_would_ask("flow-a", 1, Some("owner"), "mock", "g1.ref")
                    .is_err()
            );
            assert!(opens.lock().unwrap().is_empty());
            let mut scope = registered();
            scope.targets.insert("mock".into());
            transport.configure_flow(&crate::flow_scope::flow_route_key("flow-a", 1), Some(scope));
            assert!(
                transport
                    .grant_would_ask("flow-a", 1, Some("owner"), "mock", "g1.ref")
                    .unwrap()
            );
            let opens = opens.lock().unwrap();
            assert_eq!(opens.len(), 1);
            assert!(opens[0].get("scope").is_some());
            let calls = calls.lock().unwrap();
            let poll = calls
                .iter()
                .find(|(_, body)| body["method"] == "grants.would_ask")
                .unwrap();
            assert_eq!(poll.0, 11);
            assert_eq!(
                poll.1["params"],
                json!({"agent_id":"owner","flow_id":"flow-a","items":[{"grant":"g1.ref"}]})
            );
        });
    }

    #[test]
    fn model_reads_and_releases_keep_their_installed_version_route() {
        use crate::broca::{Route, Transport as _};
        with_scoped_transport(|transport, opens, calls, _| {
            let mut v1 = registered();
            v1.targets.insert("broca".into());
            v1.selector.scope_ref = "approved-v1".into();
            let mut v2 = v1.clone();
            v2.selector.scope_ref = "approved-v2".into();
            let broca =
                crate::broca::subc::SubcBrocaTransport::new(transport.clone(), "broca".into());
            let first = Route {
                flow_id: Some("flow-a".into()),
                flow_version: Some(1),
                project_root: "/".into(),
                harness: "basal".into(),
                session: "v1-call".into(),
            };
            let second = Route {
                flow_version: Some(2),
                session: "v2-call".into(),
                ..first.clone()
            };
            transport.configure_flow(&crate::flow_scope::flow_route_key("flow-a", 1), Some(v1));
            broca.send(&first, br#"{"send_id":"v1"}"#).unwrap();
            transport.configure_flow(&crate::flow_scope::flow_route_key("flow-a", 2), Some(v2));
            broca.send(&second, br#"{"send_id":"v2"}"#).unwrap();
            broca.send(&first, br#"{"send_id":"v1-again"}"#).unwrap();
            broca
                .send_status(
                    &first,
                    &crate::broca::wire::SendStatusParams {
                        send_id: "v1".into(),
                    },
                )
                .unwrap();
            let opened = opens.lock().unwrap();
            assert_eq!(opened.len(), 2);
            assert_eq!(opened[0]["scope"]["ref"], "approved-v1");
            assert_eq!(opened[1]["scope"]["ref"], "approved-v2");
            drop(opened);
            let sent: Vec<_> = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, body)| body["method"] == "session.send")
                .map(|(channel, _)| *channel)
                .collect();
            assert_eq!(sent, vec![11, 12, 11]);
            assert!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(channel, body)| *channel == 11 && body["method"] == "send.status")
            );
            broca.release(&first);
            broca.send(&second, br#"{"send_id":"v2-again"}"#).unwrap();
            assert_eq!(opens.lock().unwrap().len(), 2);
        });
    }

    #[test]
    fn grant_reply_must_name_the_requested_reference_and_known_verdict() {
        assert!(
            grant_verdict(
                &json!({"items":[{"grant":"g","verdict":"yes","reason":"allowed"}]}),
                "g"
            )
            .unwrap()
        );
        for verdict in ["no", "would_ask"] {
            assert!(
                !grant_verdict(
                    &json!({"items":[{"grant":"g","verdict":verdict,"reason":"blocked"}]}),
                    "g"
                )
                .unwrap()
            );
        }
        for reply in [
            json!({"items":[]}),
            json!({"items":[{"grant":"foreign","verdict":"yes","reason":"allowed"}]}),
            json!({"items":[{"grant":"g","verdict":"unknown","reason":"allowed"}]}),
        ] {
            assert!(grant_verdict(&reply, "g").is_err());
        }
    }

    #[test]
    fn run_tool_routes_copy_bind_identity_and_outlive_transport_fallback_wait() {
        // The 200 ms consumer call timeout is shorter than SLOW_ECHO_DELAY,
        // so the reply comes back only because the 2 s per-call
        // reply_timeout replaces the consumer's fallback wait.
        let call_timeout = Duration::from_millis(200);
        with_paused_open_timeout(None, call_timeout, |transport, opens, calls, _| {
            let selector = crate::flow_scope::FlowScope {
                owner: subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                scope_ref: "run-scope".into(),
                epoch: 19,
            };
            transport.configure_flow(
                "codemode:run",
                Some(crate::flow_scope::RegisteredScope {
                    selector: selector.clone(),
                    targets: ["mock".into()].into_iter().collect(),
                }),
            );
            let mut identity =
                BindIdentity::new("/agent/project", "agent:other", "invoking-session");
            identity.project_id = Some("pj-registered".into());
            let options = RunToolOptions {
                identity: identity.clone(),
                reply_timeout: Duration::from_secs(2),
                preset: None,
            };
            assert_eq!(
                transport
                    .tool_for_run(
                        "codemode:run",
                        "mock",
                        "slow_echo",
                        json!({"n":7}),
                        "run:call:1",
                        &options
                    )
                    .unwrap(),
                json!({"n":7})
            );
            assert_eq!(
                opens.lock().unwrap()[0]["identity"],
                serde_json::to_value(identity).unwrap()
            );
            assert_eq!(
                opens.lock().unwrap()[0]["scope"],
                serde_json::to_value(selector.selector()).unwrap()
            );
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, body)| body["name"] == "slow_echo")
                    .count(),
                1
            );
            transport.configure_flow("codemode:run", None);
        });
    }

    #[test]
    fn run_custody_reuses_the_declared_route_and_never_resends_the_tool() {
        with_scoped_transport(|transport, opens, calls, _| {
            let options = run_scope(&transport);
            transport
                .tool_for_run(
                    "codemode:run",
                    "mock",
                    "echo",
                    json!(7),
                    "stable-key",
                    &options,
                )
                .unwrap();
            assert_eq!(
                opens.lock().unwrap()[0]["consumer_capabilities"],
                json!(["held-reply/v1", "tool-result/v1"])
            );
            for name in ["late_results", "late_results.ack", "tool.withdraw"] {
                transport
                    .role_for_run(
                        "codemode:run",
                        "mock",
                        name,
                        json!({"call_key":"stable-key"}),
                        &options,
                    )
                    .unwrap();
            }
            let recorded = calls.lock().unwrap();
            let tool = recorded
                .iter()
                .find(|(_, body)| body["name"] == "echo")
                .unwrap();
            for name in ["late_results", "late_results.ack", "tool.withdraw"] {
                let call = recorded
                    .iter()
                    .find(|(_, body)| body["name"] == name)
                    .unwrap();
                assert_eq!(call.0, tool.0);
                assert!(call.1.get("call_key").is_none());
                assert!(call.1.get("method").is_none());
            }
            drop(recorded);
            assert_eq!(opens.lock().unwrap().len(), 1);
            transport.configure_flow("codemode:run", None);
            assert!(
                transport
                    .role_for_run(
                        "codemode:run",
                        "mock",
                        "late_results",
                        json!({"since":null}),
                        &options
                    )
                    .is_err()
            );
            assert_eq!(
                opens.lock().unwrap().len(),
                1,
                "custody never opens a replacement route"
            );
        });
    }

    #[test]
    fn custody_reconnect_reopens_the_same_scope_for_metadata_without_resending_the_tool() {
        with_scoped_transport(|transport, opens, calls, _| {
            let options = run_scope(&transport);
            transport
                .tool_for_run("codemode:run", "mock", "echo", json!(7), "once", &options)
                .unwrap();
            let handle = cached_run_route(&transport, &options);
            transport
                .handle
                .block_on(transport.consumer.close_handle(&handle, Default::default()))
                .unwrap();
            transport.routes.lock().unwrap().connection_restored();
            for name in ["late_results", "late_results.ack", "tool.withdraw"] {
                transport
                    .role_for_run("codemode:run", "mock", name, json!({}), &options)
                    .unwrap();
            }
            let opened = opens.lock().unwrap();
            assert_eq!(opened.len(), 2);
            assert_eq!(
                opened[0]["scope"], opened[1]["scope"],
                "reconnect keeps the same scope epoch"
            );
            assert_eq!(opened[0]["identity"], opened[1]["identity"]);
            assert_eq!(
                requests_named(&calls, "echo"),
                1,
                "the tool action is never resent"
            );
            drop(opened);
            transport.configure_flow("codemode:run", None);
        });
    }

    #[test]
    fn custody_deadline_includes_waiting_for_a_shared_route_open() {
        let (entered, received) = std::sync::mpsc::sync_channel(1);
        let (release, paused) = tokio::sync::oneshot::channel();
        with_paused_open(Some((entered, paused)), |transport, _, calls, _| {
            let options = run_scope(&transport);
            let action_transport = transport.clone();
            let action_options = options.clone();
            let action = std::thread::spawn(move || {
                action_transport.tool_for_run(
                    "codemode:run",
                    "mock",
                    "echo",
                    json!(7),
                    "once",
                    &action_options,
                )
            });
            received.recv_timeout(Duration::from_secs(10)).unwrap();
            let custody_transport = transport.clone();
            let mut custody_options = options;
            custody_options.reply_timeout = Duration::from_millis(50);
            let (finished, done) = std::sync::mpsc::sync_channel(1);
            let custody = std::thread::spawn(move || {
                let reply = custody_transport.role_for_run(
                    "codemode:run",
                    "mock",
                    "late_results",
                    json!({"since":null}),
                    &custody_options,
                );
                finished.send(reply).unwrap();
            });
            let reply = done.recv_timeout(Duration::from_millis(500));
            release.send(()).unwrap();
            action.join().unwrap().unwrap();
            custody.join().unwrap();
            assert!(
                matches!(reply, Ok(Err(WireError::TimedOut(_)))),
                "custody must time out while the unrelated route opener is still waiting"
            );
            assert_eq!(requests_named(&calls, "echo"), 1);
            assert_eq!(requests_named(&calls, "late_results"), 0);
            transport.configure_flow("codemode:run", None);
        });
    }

    /// Registers the run scope `codemode:run` targeting `mock`, and the
    /// options a codemode tool call carries.
    fn run_scope(transport: &SubcTransport) -> RunToolOptions {
        transport.configure_flow(
            "codemode:run",
            Some(crate::flow_scope::RegisteredScope {
                selector: crate::flow_scope::FlowScope {
                    owner: subc_protocol::Principal::Reserved {
                        module_id: "prefrontal-core".into(),
                    },
                    scope_ref: "run-scope".into(),
                    epoch: 19,
                },
                targets: ["mock".into()].into_iter().collect(),
            }),
        );
        RunToolOptions {
            identity: BindIdentity::new("/agent/project", "agent:other", "invoking-session"),
            reply_timeout: Duration::from_secs(5),
            preset: None,
        }
    }

    fn requests_named(calls: &Mutex<Vec<(u16, Value)>>, name: &str) -> usize {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["name"] == name)
            .count()
    }

    /// The route basal has cached for `mock` on the run scope.
    fn cached_run_route(
        transport: &SubcTransport,
        options: &RunToolOptions,
    ) -> subc_client_rs::RouteHandle {
        transport
            .routes
            .lock()
            .unwrap()
            .prepare(
                "codemode:run",
                &RouteTarget::ToolProvider {
                    module_id: "mock".into(),
                },
                Some(&options.identity),
            )
            .unwrap()
            .handle
            .expect("an earlier call's route is cached")
    }

    /// What `send_run_request` did: how many opens and sends it made.
    fn reopen_trace(
        mut opens: Vec<Result<u8, WireError>>,
        mut sends: Vec<Result<Vec<u8>, CallError>>,
    ) -> (Result<Vec<u8>, WireError>, usize, usize) {
        opens.reverse();
        sends.reverse();
        let (mut opened, mut sent) = (0, 0);
        let result = send_run_request(
            || {
                opened += 1;
                opens.pop().expect("no more opens expected")
            },
            |_| {
                sent += 1;
                sends.pop().expect("no more sends expected")
            },
            |_| {},
        );
        (result, opened, sent)
    }

    fn bind_refused(reason: crate::flow_refusal::RefusalReason) -> WireError {
        WireError::Typed(crate::flow_refusal::FlowRefusal::new(
            reason, "mock", "echo",
        ))
    }

    #[test]
    fn run_tool_bind_refused_scope_changed_reopens_once_and_sends_one_request() {
        use crate::flow_refusal::RefusalReason::{ScopeChanged, ScopeNotSynced};
        for reason in [ScopeChanged, ScopeNotSynced] {
            let (result, opened, sent) = reopen_trace(
                vec![Err(bind_refused(reason)), Ok(1)],
                vec![Ok(b"{}".to_vec())],
            );
            assert_eq!(result, Ok(b"{}".to_vec()), "{reason:?}");
            assert_eq!((opened, sent), (2, 1), "{reason:?}");
        }
    }

    #[test]
    fn run_tool_second_bind_refusal_is_returned_without_a_send() {
        use crate::flow_refusal::RefusalReason::ScopeChanged;
        let (result, opened, sent) = reopen_trace(
            vec![
                Err(bind_refused(ScopeChanged)),
                Err(bind_refused(ScopeChanged)),
            ],
            vec![],
        );
        assert!(
            matches!(result, Err(WireError::Typed(ref refusal)) if refusal.reason == ScopeChanged)
        );
        assert_eq!((opened, sent), (2, 0));
    }

    #[test]
    fn run_tool_other_bind_refusals_and_sent_requests_are_never_retried() {
        use crate::flow_refusal::RefusalReason::{ScopeChanged, ScopeEnded};
        // A terminal bind refusal is not re-opened.
        let (result, opened, sent) = reopen_trace(vec![Err(bind_refused(ScopeEnded))], vec![]);
        assert!(matches!(result, Err(WireError::Typed(_))));
        assert_eq!((opened, sent), (1, 0));
        // The provider received the request and answered scope_changed: the
        // answer is returned as the scope reason the supervisor reads, never
        // followed by a second request.
        let (result, opened, sent) = reopen_trace(
            vec![Ok(1)],
            vec![Err(CallError::Module(subc_protocol::ErrorBody::new(
                "scope_changed",
                "changed",
            )))],
        );
        assert!(
            matches!(&result, Err(WireError::Typed(refusal)) if refusal.reason == ScopeChanged),
            "{result:?}"
        );
        assert_eq!((opened, sent), (1, 1));
    }

    #[test]
    fn run_tool_provider_scope_changed_frame_after_send_is_not_resent() {
        with_scoped_transport(|transport, opens, calls, _| {
            let options = run_scope(&transport);
            let result = transport.tool_for_run(
                "codemode:run",
                "mock",
                "changed_after_send",
                json!({}),
                "run:call:1",
                &options,
            );
            // A scope-state refusal keeps its typed reason.
            assert!(
                matches!(&result, Err(WireError::Typed(refusal)) if refusal.reason == crate::flow_refusal::RefusalReason::ScopeChanged),
                "{result:?}"
            );
            assert_eq!(requests_named(&calls, "changed_after_send"), 1);
            assert_eq!(opens.lock().unwrap().len(), 1);
            transport.configure_flow("codemode:run", None);
        });
    }

    /// A cached route that died (closed here; a dropped connection leaves its
    /// handle stale the same way) is refused `StaleRouteHandle` before any
    /// frame is written, so the same call re-opens a fresh route and the
    /// provider still sees the request exactly once.
    #[test]
    fn run_tool_dead_route_reopens_once_and_the_provider_sees_one_request() {
        with_scoped_transport(|transport, opens, calls, _| {
            let options = run_scope(&transport);
            let call = |key: &str| {
                transport.tool_for_run(
                    "codemode:run",
                    "mock",
                    "echo",
                    json!({"k":key}),
                    key,
                    &options,
                )
            };
            assert_eq!(call("run:call:1").unwrap(), json!({"k":"run:call:1"}));
            let handle = cached_run_route(&transport, &options);
            transport
                .handle
                .block_on(transport.consumer.close_handle(&handle, Default::default()))
                .unwrap();
            assert_eq!(call("run:call:2").unwrap(), json!({"k":"run:call:2"}));
            assert_eq!(opens.lock().unwrap().len(), 2, "a fresh route was opened");
            assert_eq!(requests_named(&calls, "echo"), 2, "one request per call");
            transport.configure_flow("codemode:run", None);
        });
    }

    /// After a connection is restored, a later call in the same run opens a
    /// fresh route instead of reusing the handle from the old connection.
    #[test]
    fn run_tool_after_a_reconnect_opens_a_fresh_route() {
        with_scoped_transport(|transport, opens, calls, _| {
            let options = run_scope(&transport);
            let call = |key: &str| {
                transport.tool_for_run(
                    "codemode:run",
                    "mock",
                    "echo",
                    json!({"k":key}),
                    key,
                    &options,
                )
            };
            assert_eq!(call("run:call:1").unwrap(), json!({"k":"run:call:1"}));
            assert_eq!(call("run:call:2").unwrap(), json!({"k":"run:call:2"}));
            assert_eq!(opens.lock().unwrap().len(), 1, "a live route is reused");
            // A dropped connection takes its routes with it; the restore then
            // clears every handle basal cached for the old connection.
            let handle = cached_run_route(&transport, &options);
            transport
                .handle
                .block_on(transport.consumer.close_handle(&handle, Default::default()))
                .unwrap();
            transport.routes.lock().unwrap().connection_restored();
            assert!(
                transport
                    .routes
                    .lock()
                    .unwrap()
                    .prepare(
                        "codemode:run",
                        &RouteTarget::ToolProvider {
                            module_id: "mock".into()
                        },
                        Some(&options.identity)
                    )
                    .unwrap()
                    .handle
                    .is_none(),
                "no handle from the old connection is kept"
            );
            assert_eq!(call("run:call:3").unwrap(), json!({"k":"run:call:3"}));
            assert_eq!(opens.lock().unwrap().len(), 2, "a fresh route was opened");
            assert_eq!(requests_named(&calls, "echo"), 3);
            transport.configure_flow("codemode:run", None);
        });
    }

    #[test]
    fn real_subc_transport_opens_scoped_ops_and_models_and_reuses_routes() {
        use crate::flow_scope::{FlowScope, RegisteredScope};
        with_scoped_transport(|transport, opens, calls, close_seen| {
            let selector = FlowScope {
                owner: subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                scope_ref: "registered-flow".into(),
                epoch: 7,
            };
            let registered = RegisteredScope {
                selector: selector.clone(),
                targets: ["mock".into(), "broca".into()].into_iter().collect(),
            };
            transport.configure_flow("flow-a", Some(registered));
            let (scope_ended, scope_events) = std::sync::mpsc::channel();
            transport.watch_run_scope(
                "flow-a",
                Arc::new(move |code| {
                    scope_ended.send(code.to_owned()).unwrap();
                }),
            );
            for _ in 0..2 {
                assert_eq!(
                    transport
                        .management_for_flow("flow-a", "mock", "echo", json!({"n":7}))
                        .unwrap(),
                    json!({"n":7})
                );
            }
            use crate::selector::ModelSelector;
            let selector_client =
                Arc::new(crate::selector::RoutingSelector::new(transport.clone()));
            let selection = selector_client
                .select(&crate::selector::SelectionRequest {
                    caller_class: "flow".into(),
                    iq: 70,
                    eq: 20,
                    flow_id: "flow-a".into(),
                    run_id: "run-a".into(),
                    send_id: "send-1".into(),
                })
                .unwrap();
            let broca =
                crate::broca::subc::SubcBrocaTransport::new(transport.clone(), "broca".into());
            let model_store = Arc::new(crate::broca::fake::MemoryStore::default());
            let model_host = crate::broca::BrocaHost::new(
                broca.clone(),
                model_store.clone(),
                "/".into(),
                "basal".into(),
                selector_client.clone(),
            );
            let request=crate::CallRequest { flow_version: None, flow_id: "flow-a".into(), run_id:"run-a".into(),position:0,kind:basal_proto::CallKind::Primitive(basal_proto::Primitive::Llm),args:basal_proto::JsonText::new(json!({"send_id":"send-1","work_class":"flow:flow-a","session":"basal:flow-flow-a:run-a:0","op":"llm","max_output":16,"request":{"prompt":"hello"},"selection":selection}).to_string()).unwrap(),idempotency_key:"send-1".into(),attempt:1 };
            model_host.dispatch_model(&request).unwrap();
            drop(model_host);
            let recovered_host = crate::broca::BrocaHost::new(
                broca.clone(),
                model_store,
                "/".into(),
                "basal".into(),
                selector_client,
            );
            recovered_host.poll().unwrap();
            recovered_host.poll().unwrap();
            assert_eq!(
                transport
                    .management(
                        "prefrontal-core",
                        "sink.digest",
                        json!({"flow_id":"flow-a","flow_version":1})
                    )
                    .unwrap(),
                json!({"flow_id":"flow-a","flow_version":1})
            );
            let records = opens.lock().unwrap().clone();
            assert_eq!(
                records.len(),
                4,
                "ops share a route; this model call reuses its own route; carrier core is separate"
            );
            assert_eq!(
                records[0]["scope"],
                serde_json::to_value(selector.selector()).unwrap()
            );
            assert_eq!(records[2]["scope"], records[0]["scope"]);
            assert!(
                records[1].get("scope").is_none(),
                "model-selection plumbing must remain unscoped"
            );
            assert!(
                records[3].get("scope").is_none(),
                "core carrier call must remain unscoped"
            );
            transport
                .management_for_flow("flow-a", "mock", "end_scope", json!({}))
                .unwrap();
            close_seen.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(
                scope_events.recv_timeout(Duration::from_secs(10)).unwrap(),
                "scope_expired"
            );
            transport.configure_flow(
                "flow-a",
                Some(RegisteredScope {
                    selector: selector.clone(),
                    targets: ["mock".into(), "broca".into()].into_iter().collect(),
                }),
            );
            assert!(
                matches!(
                    transport.management_for_flow("flow-a", "mock", "echo", json!({})),
                    Err(WireError::Typed(_))
                ),
                "a daemon scope close must not trigger an automatic reopen"
            );
            assert_eq!(opens.lock().unwrap().len(), 4);
            let mut next = selector;
            next.epoch += 1;
            transport.configure_flow(
                "flow-a",
                Some(RegisteredScope {
                    selector: next.clone(),
                    targets: ["mock".into(), "broca".into()].into_iter().collect(),
                }),
            );
            transport
                .management_for_flow("flow-a", "mock", "echo", json!({}))
                .unwrap();
            transport.configure_flow("flow-a", None);
            assert!(
                !transport
                    .run_watchers
                    .lock()
                    .unwrap()
                    .contains_key("flow-a")
            );
            assert_eq!(
                opens.lock().unwrap()[4]["scope"],
                serde_json::to_value(next.selector()).unwrap()
            );
            let calls = calls.lock().unwrap();
            let reports: Vec<_> = calls
                .iter()
                .filter(|(_, body)| body["method"] == "route.set_decision_outcome")
                .collect();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].1["params"]["decisionID"], "selection-1");
            let selected = calls
                .iter()
                .find(|(_, body)| body["method"] == "route.select")
                .unwrap();
            assert_eq!(selected.0, reports[0].0);
            assert_eq!(
                selected.1["params"],
                json!({"targetAgent":"flow","requirements":{"iq":70,"eq":20},"excludeRouteKeys":[],"sendID":"send-1","taskId":"flow:flow-a:run-a","substrate":"broca"})
            );
            let sent = calls
                .iter()
                .find(|(_, body)| body["method"] == "session.send")
                .unwrap();
            assert_ne!(selected.0, sent.0);
            assert_eq!(sent.1["params"]["send_id"], "send-1");
            for method in ["run.result", "run.status"] {
                let reads: Vec<_> = calls
                    .iter()
                    .filter(|(_, body)| body["method"] == method)
                    .collect();
                assert_eq!(reads.len(), 1);
                assert_eq!(
                    reads[0].0, sent.0,
                    "recovered Broca reads must use the send's scoped route"
                );
            }
            drop(calls);
            drop(broca);
        });
    }

    #[cfg(feature = "rig-kill-hook")]
    #[test]
    fn rig_send_hook_opens_only_the_selected_send_unscoped_and_keeps_other_calls_scoped() {
        use crate::broca::{Route, Transport as _};
        use crate::flow_scope::{FlowScope, RegisteredScope};
        with_scoped_transport(|transport, opens, calls, _| {
            transport.configure_flow(
                "armed",
                Some(RegisteredScope {
                    selector: FlowScope {
                        owner: subc_protocol::Principal::Reserved {
                            module_id: "prefrontal-core".into(),
                        },
                        scope_ref: "registered".into(),
                        epoch: 7,
                    },
                    targets: ["broca".into()].into_iter().collect(),
                }),
            );
            let broca = crate::broca::subc::SubcBrocaTransport::new(transport, "broca".into());
            broca.set_unscoped_send_hook(Arc::new(|route| route.session == "unscoped"));
            for session in ["unscoped", "scoped"] {
                let route = Route {
                    flow_version: None,
                    flow_id: Some("armed".into()),
                    project_root: "/".into(),
                    harness: "basal".into(),
                    session: session.into(),
                };
                broca.send(&route, br#"{"send_id":"s"}"#).unwrap();
            }
            let opens = opens.lock().unwrap();
            assert_eq!(opens.len(), 2);
            assert!(opens[0].get("scope").is_none());
            assert_eq!(opens[0]["identity"]["session"], "unscoped");
            assert_eq!(opens[1]["scope"]["ref"], "registered");
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|(_, body)| body["method"] == "session.send")
                    .count(),
                2
            );
        });
    }

    fn scoped_model_requests(
        selection: &dyn crate::selector::ModelSelector,
    ) -> Vec<crate::CallRequest> {
        use crate::selector::SelectionRequest;
        use basal_proto::{CallKind, JsonText, Primitive};
        let mut requests = Vec::new();
        for (run, position, primitive, session) in [
            ("run-a", 0, Primitive::Llm, "basal:flow-flow-a:run-a:0"),
            ("run-a", 1, Primitive::Classify, "basal:flow-flow-a:run-a:1"),
            ("run-b", 0, Primitive::Llm, "basal:flow-flow-a:run-b:0"),
        ] {
            let send = format!("send-{}", requests.len());
            let chosen = selection
                .select(&SelectionRequest {
                    caller_class: "flow".into(),
                    iq: 70,
                    eq: 20,
                    flow_id: "flow-a".into(),
                    run_id: run.into(),
                    send_id: send.clone(),
                })
                .unwrap();
            let args = if primitive == Primitive::Classify {
                json!({"text":"hello","labels":["answer","other"]})
            } else {
                json!({"prompt":"hello"})
            };
            requests.push(crate::CallRequest { flow_version: None, flow_id: "flow-a".into(), run_id: run.into(), position,
            kind: CallKind::Primitive(primitive),
            args: JsonText::new(json!({"send_id":send,"work_class":"flow:flow-a","session":session,"op":primitive.name(),"max_output":16,"request":args,"selection":chosen}).to_string()).unwrap(),
            idempotency_key: send, attempt: 1, });
        }
        requests
    }

    fn assert_model_opens(
        send_opens: &[Value],
        selector: &crate::flow_scope::FlowScope,
        expected: &[&str],
    ) {
        assert_eq!(send_opens.len(), 3, "one scoped Broca route per call");
        let mut sessions: Vec<_> = send_opens
            .iter()
            .map(|open| {
                assert_eq!(
                    open["scope"],
                    serde_json::to_value(selector.selector()).unwrap()
                );
                open["identity"]["session"]
                    .as_str()
                    .unwrap_or_else(|| panic!("missing bind session: {open}"))
                    .to_owned()
            })
            .collect();
        sessions.sort();
        assert_eq!(sessions, expected);
        for open in send_opens {
            assert_eq!(open["identity"]["project_root"], "/");
            assert_eq!(open["identity"]["harness"], "basal");
        }
    }

    fn assert_recovered_model_calls(
        recorded_opens: &[Value],
        recorded_calls: &[(u16, Value)],
        expected_session: &str,
    ) {
        let session_for_channel = |channel: u16| {
            recorded_opens[usize::from(channel - 11)]["identity"]["session"]
                .as_str()
                .unwrap()
        };
        for (channel, send) in recorded_calls
            .iter()
            .filter(|(_, body)| body["method"] == "session.send")
        {
            let session = session_for_channel(*channel);
            let run = format!("broca-run:{}", send["params"]["send_id"].as_str().unwrap());
            for method in ["run.result", "run.status"] {
                let reads: Vec<_> = recorded_calls
                    .iter()
                    .filter(|(_, body)| body["method"] == method && body["params"]["run_id"] == run)
                    .collect();
                assert_eq!(reads.len(), 1);
                assert_ne!(
                    reads[0].0, *channel,
                    "recovery uses the current selector's new channel"
                );
                assert_eq!(
                    session_for_channel(reads[0].0),
                    session,
                    "recovery must read the send's conversation"
                );
            }
        }
        // Model calls open no session stream: Broca's run events on the bus
        // wake them, and each call's scheduled check covers a lost event.
        assert!(
            !recorded_calls
                .iter()
                .any(|(_, body)| body["method"] == "session.subscribe")
        );
        let read = recorded_calls
            .iter()
            .find(|(_, body)| body["method"] == "session.read")
            .unwrap();
        assert_eq!(session_for_channel(read.0), expected_session);
    }

    #[test]
    fn scoped_model_calls_keep_per_call_sessions_across_runs_and_recovery() {
        use crate::broca::{BrocaHost, Transport as _};
        use crate::flow_scope::{FlowScope, RegisteredScope};
        use crate::selector::FakeSelector;
        with_scoped_transport(|transport, opens, calls, _| {
            let selector = FlowScope {
                owner: subc_protocol::Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                },
                scope_ref: "registered-flow".into(),
                epoch: 7,
            };
            let register = |selector: FlowScope| RegisteredScope {
                selector,
                targets: ["broca".into()].into_iter().collect(),
            };
            transport.configure_flow("flow-a", Some(register(selector.clone())));
            let broca =
                crate::broca::subc::SubcBrocaTransport::new(transport.clone(), "broca".into());
            let store = Arc::new(crate::broca::fake::MemoryStore::default());
            let selection = Arc::new(FakeSelector::default());
            let host = BrocaHost::new(
                broca.clone(),
                store.clone(),
                "/".into(),
                "basal".into(),
                selection.clone(),
            );
            let requests = scoped_model_requests(selection.as_ref());
            // The accepted calls remain in flight together. Broca must not
            // receive their prompts under a shared conversation identity.
            std::thread::scope(|threads| {
                for request in &requests {
                    let host = &host;
                    threads.spawn(move || host.dispatch_model(request).unwrap());
                }
            });
            let expected = [
                "basal:flow-flow-a:run-a:0",
                "basal:flow-flow-a:run-a:1",
                "basal:flow-flow-a:run-b:0",
            ];
            let send_opens = opens.lock().unwrap().clone();
            assert_model_opens(&send_opens, &selector, &expected);
            drop(host);
            drop(broca);

            // A new selector drops the old channels. Recovery must open new
            // scoped channels without changing any call's conversation name.
            let mut current = selector;
            current.epoch += 1;
            transport.configure_flow("flow-a", Some(register(current.clone())));
            let broca =
                crate::broca::subc::SubcBrocaTransport::new(transport.clone(), "broca".into());
            let recovered =
                BrocaHost::new(broca.clone(), store, "/".into(), "basal".into(), selection);
            recovered.poll().unwrap();
            let recorded_opens = opens.lock().unwrap().clone();
            assert_eq!(recorded_opens.len(), 6);
            for open in &recorded_opens[3..] {
                assert_eq!(
                    open["scope"],
                    serde_json::to_value(current.selector()).unwrap()
                );
            }
            // session.read uses the same routing helper as result/status, so
            // inspecting conversation history cannot acquire another identity.
            transport
                .management_for_model(
                    "flow-a",
                    BindIdentity::new("/", "basal", expected[0]),
                    "broca",
                    "session.read",
                    b"{}",
                )
                .unwrap();
            let recorded_calls = calls.lock().unwrap().clone();
            assert_recovered_model_calls(&recorded_opens, &recorded_calls, expected[0]);
            broca.release(&crate::broca::Route {
                flow_version: None,
                flow_id: Some("flow-a".into()),
                project_root: "/".into(),
                harness: "basal".into(),
                session: expected[0].into(),
            });
            transport
                .management_for_model(
                    "flow-a",
                    BindIdentity::new("/", "basal", expected[1]),
                    "broca",
                    "session.read",
                    b"{}",
                )
                .unwrap();
            assert_eq!(
                opens.lock().unwrap().len(),
                6,
                "releasing one call leaves the other call's route cached"
            );
            drop(recovered);
            drop(broca);
        });
    }

    #[test]
    fn malformed_typed_busy_is_unreadable_not_provably_unsent() {
        let error = map_error(CallError::Module(subc_protocol::ErrorBody::new(
            "resource_busy",
            "retry safely",
        )));
        assert_eq!(error.unknown_reason(), Some(UnknownReason::ReplyUnreadable));
    }

    fn boxed(text: &str) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(std::io::Error::other(text.to_owned()))
    }

    /// One case per subc-client-rs 0.26.1 `CallError` variant basal can build
    /// (a `StaleRouteHandle` needs a route handle only the client can make;
    /// it maps to never sent). Each names whether the call can have been
    /// sent and, if so, the unknown reason recorded for it. An
    /// `OutcomeUnknown` built here carries no cause; the client offers no
    /// public way to attach one, so the tests below check each cause on
    /// `outcome_unknown` directly.
    #[test]
    fn every_call_error_variant_maps_to_never_sent_or_one_unknown_reason() {
        let cases: Vec<(CallError, Option<UnknownReason>, bool)> = vec![
            (CallError::NotSent(boxed("never wrote")), None, true),
            (
                CallError::OutcomeUnknown(boxed("no cause attached")),
                Some(UnknownReason::ConnectionLost),
                false,
            ),
            (
                CallError::SubscriptionBackpressure(boxed(
                    "subscription event receiver closed before the stream ended",
                )),
                Some(UnknownReason::ConnectionLost),
                false,
            ),
            (
                CallError::CapabilityUnprovided {
                    capability: "x/v1".into(),
                },
                None,
                true,
            ),
            (
                CallError::CapabilityAmbiguous {
                    capability: "x/v1".into(),
                    claimants: vec!["a".into(), "b".into()],
                },
                None,
                true,
            ),
            (
                CallError::InvalidCapabilityIdentifier {
                    capability: "not a capability".into(),
                },
                None,
                true,
            ),
        ];
        for (error, reason, never_sent) in cases {
            let label = format!("{error}");
            let mapped = map_error(error);
            assert_eq!(mapped.unknown_reason(), reason, "{label}");
            assert_eq!(
                matches!(mapped, WireError::NeverSent(_)),
                never_sent,
                "{label}"
            );
        }
        let refused =
            serde_json::from_value(json!({"code":"provider_denied","message":"no"})).unwrap();
        let mapped = map_error(CallError::Module(refused));
        assert_eq!(mapped.unknown_reason(), None);
        assert!(matches!(mapped, WireError::Refused { .. }));
    }

    /// The reason recorded for an unknown outcome with this cause; the
    /// client's message is kept as the error's text.
    fn reason_for(cause: Option<OutcomeUnknownCause>) -> Option<UnknownReason> {
        let mapped = outcome_unknown(cause, "the client's message".into());
        match &mapped {
            WireError::TimedOut(m) | WireError::Unknown(m) | WireError::Unreadable(m) => {
                assert_eq!(m, "the client's message")
            }
            other => panic!("an unknown outcome mapped to {other:?}"),
        }
        mapped.unknown_reason()
    }

    #[test]
    fn deadline_is_a_reply_timeout() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::Deadline)),
            Some(UnknownReason::ReplyTimeout)
        );
    }

    #[test]
    fn writer_closed_is_a_lost_connection() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::WriterClosed)),
            Some(UnknownReason::ConnectionLost)
        );
    }

    #[test]
    fn consumer_closed_is_a_lost_connection() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::ConsumerClosed)),
            Some(UnknownReason::ConnectionLost)
        );
    }

    #[test]
    fn connection_failed_is_a_lost_connection() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::ConnectionFailed)),
            Some(UnknownReason::ConnectionLost)
        );
    }

    #[test]
    fn route_ended_is_a_lost_connection() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::RouteEnded)),
            Some(UnknownReason::ConnectionLost)
        );
    }
    #[test]
    fn target_route_end_is_a_scope_cut_even_before_the_control_push() {
        assert!(target_route_ended(
            "target:ref",
            Some(OutcomeUnknownCause::RouteEnded)
        ));
        assert!(!target_route_ended(
            "flow-test",
            Some(OutcomeUnknownCause::RouteEnded)
        ));
        assert!(!target_route_ended(
            "target:ref",
            Some(OutcomeUnknownCause::ConnectionFailed)
        ));
        assert!(!target_route_ended("target:ref", None));
    }

    #[test]
    fn real_target_scope_cut_is_unknown_while_ordinary_flow_keeps_connection_loss() {
        for (flow, reference, targeted) in [
            ("flow-a", "ordinary", false),
            ("target:target-ref", "target-ref", true),
        ] {
            with_scoped_transport(|transport, opens, _calls, closed| {
                let mut scope = registered();
                scope.selector.scope_ref = reference.into();
                scope.targets.insert("mock".into());
                transport.configure_flow(flow, Some(scope));
                let error = transport
                    .management_for_flow(flow, "mock", "cut_scope", json!({}))
                    .unwrap_err();
                if targeted {
                    assert!(matches!(error, WireError::ScopeCut(_)), "{error:?}");
                } else {
                    assert!(matches!(error, WireError::Unknown(_)), "{error:?}");
                }
                assert_eq!(
                    error.unknown_reason(),
                    Some(crate::UnknownReason::ConnectionLost)
                );
                closed.recv_timeout(Duration::from_secs(5)).unwrap();
                let opens = opens.lock().unwrap();
                assert_eq!(opens.len(), 1);
                assert_eq!(opens[0]["scope"]["ref"], reference);
            });
        }
    }

    #[test]
    fn completion_failed_is_an_unreadable_reply() {
        assert_eq!(
            reason_for(Some(OutcomeUnknownCause::CompletionFailed)),
            Some(UnknownReason::ReplyUnreadable)
        );
    }

    #[test]
    fn no_cause_is_a_lost_connection() {
        assert_eq!(reason_for(None), Some(UnknownReason::ConnectionLost));
    }

    #[test]
    fn workspace_registry_reads_the_real_decoded_management_payload() {
        struct RegistryReply(Vec<u8>);
        impl Transport for RegistryReply {
            fn catalog(&self) -> Result<Value, WireError> {
                Ok(
                    serde_json::json!({"modules":[{"module_id":"entorhinal","roles":[{
                        "role":"management_surface","operations":[{"name":"enumerate","kind":"query"}],
                        "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"
                    }]}]}),
                )
            }
            fn management(
                &self,
                module: &str,
                op: &str,
                params: Value,
            ) -> Result<Value, WireError> {
                assert_eq!((module, op), ("entorhinal", "enumerate"));
                assert_eq!(params, serde_json::json!({"workspaceId":"team"}));
                // Use the production decoder so a fake cannot retain the wire
                // envelope that SubcTransport removes before catalog reads it.
                decode_response(&self.0, true)
            }
            fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
                panic!("workspace lookup must use management")
            }
        }
        for (raw, known) in [
            (
                r#"{"result":{"workspaces":[{"workspaceId":"team"}],"projects":[],"generation":1,"incarnation":"isolated"}}"#,
                true,
            ),
            (
                r#"{"result":{"workspaces":[{"workspaceId":"other","name":"team"}],"projects":[],"generation":2}}"#,
                false,
            ),
            (
                r#"{"result":{"workspaces":[],"projects":[],"generation":3}}"#,
                false,
            ),
        ] {
            let catalog = crate::subc_catalog::SubcCatalog::new(std::sync::Arc::new(
                RegistryReply(raw.as_bytes().to_vec()),
            ));
            assert_eq!(catalog.known_workspace("team").unwrap(), known);
        }
    }

    #[test]
    fn fleet_membership_reads_the_real_decoded_management_payload() {
        use crate::Catalog;
        struct FleetReply(Value);
        impl Transport for FleetReply {
            fn catalog(&self) -> Result<Value, WireError> {
                Ok(
                    serde_json::json!({"modules":[{"module_id":"entorhinal","roles":[{
                        "role":"management_surface","operations":[{"name":"agent.fleet_identity","kind":"query"}],
                        "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"
                    }]}]}),
                )
            }
            fn management(
                &self,
                module: &str,
                op: &str,
                params: Value,
            ) -> Result<Value, WireError> {
                assert_eq!((module, op), ("entorhinal", "agent.fleet_identity"));
                assert_eq!(params, serde_json::json!({}));
                decode_response(&serde_json::to_vec(&self.0).unwrap(), true)
            }
            fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
                panic!("membership must use management")
            }
        }
        let payload = serde_json::json!({"agents":[
            {"agentId":"agent_placed","displayName":"Placed","role":"engineer","tag":"P","labels":[],
             "project":{"id":"project_synthetic","name":"Synthetic","canonicalRoot":null},
             "workspace":{"id":"workspace_synthetic","name":"Synthetic workspace"}},
            {"agentId":"agent_unplaced","displayName":"Unplaced","role":"engineer","tag":"U","labels":[]}
        ],"generation":1,"incarnation":"synthetic","logPosition":1,"token":"synthetic","unchanged":false});
        let catalog = crate::subc_catalog::SubcCatalog::new(std::sync::Arc::new(FleetReply(
            serde_json::json!({"result":payload}),
        )));
        assert_eq!(
            catalog.workspaces_of("agent_placed").unwrap(),
            ["workspace_synthetic"]
        );
        assert!(catalog.workspaces_of("agent_unplaced").unwrap().is_empty());
        assert!(catalog.workspaces_of("agent_unknown").unwrap().is_empty());
        let double = crate::subc_catalog::SubcCatalog::new(std::sync::Arc::new(FleetReply(
            serde_json::json!({"result":{"result":payload}}),
        )));
        assert!(double.workspaces_of("agent_placed").is_err());
        let failed = crate::subc_catalog::SubcCatalog::new(std::sync::Arc::new(FleetReply(
            serde_json::json!({"error":{"code":"offline","message":"unavailable"}}),
        )));
        assert!(failed.workspaces_of("agent_placed").is_err());
    }
}
