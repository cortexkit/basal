//! Blocking consumer boundary. The runtime dispatches off its activation thread.
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use subc_client_rs::consumer::{
    CallError, CallOptions, ConnectionState, ConsumerOptions, SubcConsumer, SubscribeOptions,
    Subscription,
};
use subc_protocol::{BindIdentity, RouteTarget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    NeverSent(String),
    Unknown(String),
    Refused { code: String, message: String },
}

/// Returns decoded provider payloads, not the management response envelope.
pub trait Transport: Send + Sync {
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

pub fn map_error(error: CallError) -> WireError {
    match error {
        CallError::NotSent(e) => WireError::NeverSent(e.to_string()),
        CallError::StaleRouteHandle(_) => WireError::NeverSent("stale local route handle".into()),
        CallError::Module(e) => WireError::Refused {
            code: e.code,
            message: e.message,
        },
        other => WireError::Unknown(other.to_string()),
    }
}

pub struct SubcTransport {
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
    consumer: SubcConsumer,
    identity: BindIdentity,
    timeout: Duration,
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
    /// Must be constructed and used on blocking threads, not inside a Tokio task.
    pub fn connect(timeout: Duration) -> Result<Arc<Self>, WireError> {
        let runtime =
            tokio::runtime::Runtime::new().map_err(|e| WireError::NeverSent(e.to_string()))?;
        let consumer = runtime
            .block_on(SubcConsumer::connect_default(ConsumerOptions {
                call_timeout: timeout,
                ..Default::default()
            }))
            .map_err(|e| WireError::NeverSent(e.to_string()))?;
        Ok(Arc::new(Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            consumer,
            // Core's answer ownership is principal plus session. This label must
            // remain stable across process restarts, not include a process id.
            identity: BindIdentity::new("/", "basal", "basal:flows"),
            timeout,
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
        let value: Value =
            serde_json::from_slice(&response).map_err(|e| WireError::Unknown(e.to_string()))?;
        if management {
            if let Some(error) = value.get("error") {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WireError::Unknown("invalid error envelope".into()))?;
                return Err(WireError::Refused {
                    code: code.into(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            value
                .get("result")
                .cloned()
                .ok_or_else(|| WireError::Unknown("missing management result".into()))
        } else {
            Ok(value)
        }
    }
}
impl SubcTransport {
    /// Shares the consumer connection but binds the supplied identity. Broca
    /// callers supply a separate session for each flow/run/call position.
    pub(crate) fn management_as(
        &self,
        identity: BindIdentity,
        module: &str,
        op: &str,
        params: &[u8],
    ) -> Result<Value, WireError> {
        self.call_as(
            RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
            identity,
            management_bytes(op, params)?,
            true,
        )
    }
    pub(crate) fn subscribe_as(
        &self,
        identity: BindIdentity,
        module: &str,
        op: &str,
        params: &[u8],
    ) -> Result<Subscription, WireError> {
        self.handle
            .block_on(self.consumer.subscribe(
                RouteTarget::ManagementSurface {
                    module_id: module.into(),
                },
                identity,
                management_bytes(op, params)?,
                SubscribeOptions {
                    route_open_timeout: self.timeout,
                    route_retry_deadline: self.timeout,
                    ..Default::default()
                },
            ))
            .map_err(map_error)
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
