//! Blocking consumer boundary. The runtime dispatches off its activation thread.
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use subc_client_rs::consumer::{
    CallError, CallOptions, ConnectionState, ConsumerOptions, OutcomeUnknownCause, SubcConsumer,
    SubscribeOptions, Subscription,
};
use subc_protocol::{BindIdentity, RouteTarget};

/// How a call over the daemon failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
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
}

impl WireError {
    /// Why the call's outcome is unknown, or `None` when it provably never
    /// left or was answered with a refusal.
    pub fn unknown_reason(&self) -> Option<crate::UnknownReason> {
        use crate::UnknownReason;
        match self {
            Self::NeverSent(_) | Self::Refused { .. } => None,
            Self::Unknown(_) => Some(UnknownReason::ConnectionLost),
            Self::TimedOut(_) => Some(UnknownReason::ReplyTimeout),
            Self::Unreadable(_) => Some(UnknownReason::ReplyUnreadable),
        }
    }
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

/// Maps a subc-client-rs 0.26.1 call error. Citations are to that release's
/// `src/consumer.rs`.
pub fn map_error(error: CallError) -> WireError {
    // Read before the match moves the error apart; `None` for every variant
    // other than OutcomeUnknown.
    let cause = error.outcome_cause();
    match error {
        CallError::NotSent(e) => WireError::NeverSent(e.to_string()),
        CallError::StaleRouteHandle(_) => WireError::NeverSent("stale local route handle".into()),
        CallError::Module(e) => WireError::Refused {
            code: e.code,
            message: e.message,
        },
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
            serde_json::from_slice(&response).map_err(|e| WireError::Unreadable(e.to_string()))?;
        if management {
            if let Some(error) = value.get("error") {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WireError::Unreadable("invalid error envelope".into()))?;
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
                .ok_or_else(|| WireError::Unreadable("missing management result".into()))
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
    request.preset = None;
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
