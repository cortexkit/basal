//! Blocking consumer boundary. The runtime dispatches off its activation thread.
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_client_rs::consumer::{
    CallError, CallOptions, ConnectionState, ConsumerOptions, OutcomeUnknownCause, SubcConsumer,
    SubscribeOptions, Subscription,
};
use subc_protocol::{BindIdentity, RouteTarget};

/// How a call over the daemon failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Typed(crate::flow_refusal::FlowRefusal),
    /// Provably never reached the target.
    NeverSent(String),
    /// Sent, and then the route closed or the connection failed before a
    /// reply: its outcome is unknown (`connection_lost`).
    Unknown(String),
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
            Self::Unknown(_) => Some(UnknownReason::ConnectionLost),
            Self::TimedOut(_) => Some(UnknownReason::ReplyTimeout),
            Self::Unreadable(_) => Some(UnknownReason::ReplyUnreadable),
        }
    }
}

/// Returns decoded provider payloads, not the management response envelope.
pub trait Transport: Send + Sync {
    fn provider_ready(
        &self,
        _flow: &str,
        _module: &str,
        _action: &str,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        Ok(())
    }
    fn configure_flow(
        &self,
        _flow: &str,
        _agent_owned: bool,
        _scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
    }
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
    fn catalog(&self) -> Result<Value, WireError>;
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError>;
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError>;
}

/// Maps a subc-client-rs 0.26.1 call error. Citations are to that release's
/// `src/consumer.rs`.
pub fn map_error(error: CallError) -> WireError {
    if let Some(body) = error.route_open_refusal() {
        return match crate::flow_refusal::FlowRefusal::decode(body) {
            Ok(Some(decoded)) => WireError::Typed(decoded),
            Ok(None) if !body.code.starts_with("scope_") => {
                WireError::NeverSent(body.message.clone())
            }
            _ => WireError::Unknown("unrecognised scoped route refusal".into()),
        };
    }
    // Read before the match moves the error apart; `None` for every variant
    // other than OutcomeUnknown.
    let cause = error.outcome_cause();
    match error {
        CallError::NotSent(e) => WireError::NeverSent(e.to_string()),
        CallError::StaleRouteHandle(_) => WireError::NeverSent("stale local route handle".into()),
        CallError::Module(e) => refusal(e),
        // The capability resolver's errors are raised before any request
        // frame for a call is written: `resolve_provider` (lines 1351-1363)
        // after reading the catalog, and `validate_capability_for_resolution`
        // (lines 5351-5359) before even that. The client itself classifies
        // all three as not sent (lines 4566-4572).
        e @ (CallError::CapabilityUnprovided { .. }
        | CallError::CapabilityAmbiguous { .. }
        | CallError::InvalidCapabilityIdentifier { .. }) => WireError::NeverSent(e.to_string()),
        // A request the writer accepted and that got no reply; the client
        // names why (`OutcomeUnknownCause`, line 1926).
        CallError::OutcomeUnknown(e) => outcome_unknown(cause, e.to_string()),
        // SubscriptionBackpressure ends a subscription whose request was
        // already sent (line 3634), and the client counts it as outcome
        // unknown (lines 4552-4560).
        other => WireError::Unknown(other.to_string()),
    }
}

pub fn refusal(body: subc_protocol::ErrorBody) -> WireError {
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
        Err(detail) => WireError::Unknown(detail),
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
        ) => WireError::Unknown(message),
        // A completion inside the client was dropped or did not match its
        // request, so basal never received a usable reply.
        Some(OutcomeUnknownCause::CompletionFailed) => WireError::Unreadable(message),
        // The client attaches a cause to every OutcomeUnknown it builds, so
        // `None` is an error built elsewhere. It and any cause added in a
        // later release are recorded as a lost connection: the request may
        // have been sent, and nothing more is known.
        None => WireError::Unknown(message),
        Some(other) => {
            tracing::warn!(cause = ?other, %message, "unrecognised subc outcome-unknown cause; recording a lost connection");
            WireError::Unknown(message)
        }
    }
}

pub struct SubcTransport {
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
    consumer: SubcConsumer,
    identity: BindIdentity,
    timeout: Duration,
    routes: Arc<Mutex<crate::flow_scope::ScopedRoutes<subc_client_rs::RouteHandle>>>,
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
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .route(flow, target, |scope| {
                self.handle
                    .block_on(self.consumer.open_route_scoped(
                        target.clone(),
                        BindIdentity::new("/", "basal", format!("basal:flow:{flow}")),
                        scope.selector(),
                        CallOptions {
                            timeout: self.timeout,
                            ..Default::default()
                        },
                    ))
                    .map_err(map_error)
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
                if let Err(CallError::StaleRouteHandle(_)) = &result {
                    // The client never reopens a handle request. A later activation
                    // obtains authority again before opening a replacement route.
                    self.routes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .closed(handle, true);
                }
                result
                    .map_err(map_error)
                    .and_then(|bytes| decode_response(&bytes, management))
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
                drop(routes);
                if let Some(on_close) = &on_close {
                    on_close();
                }
            }
        });
        Ok(Arc::new(Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            consumer,
            // Core's answer ownership is principal plus session. This label must
            // remain stable across process restarts, not include a process id.
            identity: BindIdentity::new("/", "basal", "basal:flows"),
            timeout,
            routes,
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
impl SubcTransport {
    fn model_handle(
        &self,
        flow: &str,
        target: &RouteTarget,
        identity: BindIdentity,
    ) -> Result<subc_client_rs::RouteHandle, WireError> {
        let bind = identity;
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .route_for_identity(flow, target, &bind, |scope| {
                self.handle
                    .block_on(self.consumer.open_route_scoped(
                        target.clone(),
                        bind.clone(),
                        scope.selector(),
                        CallOptions {
                            timeout: self.timeout,
                            ..Default::default()
                        },
                    ))
                    .map_err(map_error)
            })?
            .ok_or_else(|| WireError::NeverSent("model route has no handle".into()))
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
        let handle = self
            .model_handle(flow, &target, identity)
            .map_err(|e| contextual(e, module, op))?;
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
                .closed(handle, true);
        }
        result
            .map_err(map_error)
            .map_err(|e| contextual(e, module, op))
            .and_then(|bytes| decode_response(&bytes, true))
    }
    pub(crate) fn subscribe_for_model(
        &self,
        flow: &str,
        identity: BindIdentity,
        module: &str,
        op: &str,
        params: &[u8],
    ) -> Result<Subscription, WireError> {
        let target = RouteTarget::ManagementSurface {
            module_id: module.into(),
        };
        let bytes = management_bytes(op, params)?;
        let opts = SubscribeOptions {
            route_open_timeout: self.timeout,
            route_retry_deadline: self.timeout,
            ..Default::default()
        };
        let handle = self
            .model_handle(flow, &target, identity)
            .map_err(|e| contextual(e, module, op))?;
        self.handle
            .block_on(self.consumer.subscribe_route(&handle, bytes, opts))
            .map_err(map_error)
            .map_err(|e| contextual(e, module, op))
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
    pub(crate) fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        self.handle.spawn(task)
    }
    pub(crate) fn on_connection_state(&self, callback: impl Fn(ConnectionState) + Send + 'static) {
        self.consumer.on_connection_state(callback);
    }
}
impl Transport for SubcTransport {
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
    fn configure_flow(
        &self,
        flow: &str,
        agent_owned: bool,
        scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
        let dropped = self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .configure(flow, agent_owned, scope);
        for handle in dropped {
            let _ = self
                .handle
                .block_on(self.consumer.close_handle(&handle, Default::default()));
        }
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
    fn catalog(&self) -> Result<Value, WireError> {
        let catalog = self
            .handle
            .block_on(self.consumer.catalog_list())
            .map_err(map_error)?;
        Ok(
            json!({"generation": catalog.generation, "modules": catalog.modules, "subc_ops": catalog.subc_ops}),
        )
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

fn decode_response(response: &[u8], management: bool) -> Result<Value, WireError> {
    let value: Value =
        serde_json::from_slice(response).map_err(|e| WireError::Unreadable(e.to_string()))?;
    if !management {
        return Ok(value);
    }
    if let Some(error) = value.get("error") {
        let body: subc_protocol::ErrorBody = serde_json::from_value(error.clone())
            .map_err(|e| WireError::Unknown(format!("invalid error envelope: {e}")))?;
        return Err(refusal(body));
    }
    value
        .get("result")
        .cloned()
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

/// Embed frozen parameters without parsing and reserializing their bytes.
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
mod tests {
    use super::*;
    use crate::UnknownReason;

    fn with_scoped_transport(
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
                let end_scope = body["method"] == "end_scope";
                let reply = if body["op"] == "route.open" {
                    let mut records = records.lock().unwrap();
                    records.push(body);
                    json!({"op":"route.open","route_channel":10+records.len(),"route_epoch":1})
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
                            &json!({"op":"route.closed","channels":[11],"reason":"scope_ended"}),
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
            Duration::from_secs(10),
            Some(&file),
            Some(Arc::new(move || {
                closed.send(()).unwrap();
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
            transport.configure_flow("flow-a", true, Some(registered));
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
                    iq: 70,
                    eq: 20,
                    flow_id: "flow-a".into(),
                    run_id: "run-a".into(),
                    send_id: "send-1".into(),
                })
                .unwrap();
            let broca = crate::broca::subc::SubcBrocaTransport::new(
                transport.clone(),
                "broca".into(),
                Arc::new(|| {}),
            );
            let model_store = Arc::new(crate::broca::fake::MemoryStore::default());
            let model_host = crate::broca::BrocaHost::new(
                broca.clone(),
                model_store.clone(),
                "/".into(),
                "basal".into(),
                selector_client.clone(),
            );
            let request=crate::CallRequest {flow_id:"flow-a".into(),run_id:"run-a".into(),position:0,kind:basal_proto::CallKind::Primitive(basal_proto::Primitive::Llm),args:basal_proto::JsonText::new(json!({"send_id":"send-1","work_class":"flow:flow-a","session":"basal:flow-flow-a:run-a:0","op":"llm","max_output":16,"request":{"prompt":"hello"},"selection":selection}).to_string()).unwrap(),idempotency_key:"send-1".into(),attempt:1};
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
            transport.configure_flow(
                "flow-a",
                true,
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
                true,
                Some(RegisteredScope {
                    selector: next.clone(),
                    targets: ["mock".into(), "broca".into()].into_iter().collect(),
                }),
            );
            transport
                .management_for_flow("flow-a", "mock", "echo", json!({}))
                .unwrap();
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
                true,
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
            let broca = crate::broca::subc::SubcBrocaTransport::new(
                transport,
                "broca".into(),
                Arc::new(|| {}),
            );
            broca.set_unscoped_send_hook(Arc::new(|route| route.session == "unscoped"));
            for session in ["unscoped", "scoped"] {
                let route = Route {
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

    #[test]
    fn scoped_model_calls_keep_per_call_sessions_across_runs_and_recovery() {
        use crate::broca::{BrocaHost, Transport as _};
        use crate::flow_scope::{FlowScope, RegisteredScope};
        use crate::selector::{FakeSelector, ModelSelector, SelectionRequest};
        use basal_proto::{CallKind, JsonText, Primitive};
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
            transport.configure_flow("flow-a", true, Some(register(selector.clone())));
            let broca = crate::broca::subc::SubcBrocaTransport::new(
                transport.clone(),
                "broca".into(),
                Arc::new(|| {}),
            );
            let store = Arc::new(crate::broca::fake::MemoryStore::default());
            let selection = Arc::new(FakeSelector::default());
            let host = BrocaHost::new(
                broca.clone(),
                store.clone(),
                "/".into(),
                "basal".into(),
                selection.clone(),
            );
            let mut requests = Vec::new();
            for (run, position, primitive, session) in [
                ("run-a", 0, Primitive::Llm, "basal:flow-flow-a:run-a:0"),
                ("run-a", 1, Primitive::Classify, "basal:flow-flow-a:run-a:1"),
                ("run-b", 0, Primitive::Llm, "basal:flow-flow-a:run-b:0"),
            ] {
                let send = format!("send-{}", requests.len());
                let chosen = selection
                    .select(&SelectionRequest {
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
                requests.push(crate::CallRequest {
                    flow_id: "flow-a".into(), run_id: run.into(), position,
                    kind: CallKind::Primitive(primitive),
                    args: JsonText::new(json!({"send_id":send,"work_class":"flow:flow-a","session":session,"op":primitive.name(),"max_output":16,"request":args,"selection":chosen}).to_string()).unwrap(),
                    idempotency_key: send, attempt: 1,
                });
            }
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
            for open in &send_opens {
                assert_eq!(open["identity"]["project_root"], "/");
                assert_eq!(open["identity"]["harness"], "basal");
            }
            drop(host);
            drop(broca);

            // A new selector drops the old channels. Recovery must open new
            // scoped channels without changing any call's conversation name.
            let mut current = selector;
            current.epoch += 1;
            transport.configure_flow("flow-a", true, Some(register(current.clone())));
            let broca = crate::broca::subc::SubcBrocaTransport::new(
                transport.clone(),
                "broca".into(),
                Arc::new(|| {}),
            );
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
                        .filter(|(_, body)| {
                            body["method"] == method && body["params"]["run_id"] == run
                        })
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
                assert!(
                    recorded_calls
                        .iter()
                        .any(|(channel, body)| body["method"] == "session.subscribe"
                            && session_for_channel(*channel) == session)
                );
            }
            let read = recorded_calls
                .iter()
                .find(|(_, body)| body["method"] == "session.read")
                .unwrap();
            assert_eq!(session_for_channel(read.0), expected[0]);
            broca.release(&crate::broca::Route {
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
    fn malformed_typed_busy_is_connection_lost_not_provably_unsent() {
        let error = map_error(CallError::Module(subc_protocol::ErrorBody::new(
            "resource_busy",
            "retry safely",
        )));
        assert_eq!(error.unknown_reason(), Some(UnknownReason::ConnectionLost));
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
}
