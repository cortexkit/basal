//! Core's grant-checked sinks and facts. Replies remain visible to scripts.
use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};
use crate::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, InstallStatus,
    TransportError, UnknownReason,
};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::{Value, json};
use std::sync::Arc;

/// The call's identity and times, fixed when its intent is built.
pub struct IntentContext<'a> {
    pub flow_id: &'a str,
    pub version: u32,
    pub run_id: &'a str,
    pub position: u64,
    pub due_at: i64,
    pub created_at: i64,
}

/// Builds the exact payload before the runtime journals its intent. The host
/// never samples a clock during dispatch, including a crash re-issue.
pub fn intent(kind: Primitive, args: &Value, ctx: IntentContext<'_>) -> Value {
    let IntentContext {
        flow_id,
        version,
        run_id,
        position,
        due_at,
        created_at,
    } = ctx;
    match kind {
        Primitive::SinkDigest => {
            json!({"flow_id":flow_id,"flow_version":version,"run_id":run_id,"call_position":position,
            "agent":args["agent"],"action":args["action"],
            "due_at":due_at,"created_at":created_at,"item":args["item"],"claim":null})
        }
        Primitive::SinkStatus => {
            json!({"flow_id":flow_id,"flow_version":version,"agent":args["agent"],
            "text":args["value"],"ttl_ms":1800000,"revision":created_at})
        }
        Primitive::Facts => json!({"flow_id":flow_id,"flow_version":version,"agent":args["agent"],
            "fields":args["options"].get("fields").cloned().unwrap_or(json!(["identity","residence","activity","attention"])),
            "include":args["options"].get("include").cloned().unwrap_or(json!([])),
            "max_age_ms":args["options"].get("max_age_ms").cloned().unwrap_or(Value::Null)}),
        _ => Value::Null,
    }
}

pub(crate) fn outcome(result: Result<Value, WireError>) -> Result<Dispatched, TransportError> {
    // A reply too large for basal to hold is a reply it cannot read.
    let unreadable = |e: basal_proto::ValueTooLarge| {
        TransportError::maybe_sent(UnknownReason::ReplyUnreadable, e.to_string())
    };
    match result {
        Ok(value) => JsonText::new(value.to_string())
            .map(|v| Dispatched::Completed(HostOutcome::fulfilled(v)))
            .map_err(unreadable),
        Err(WireError::Refused { code, message }) => {
            JsonText::new(json!({"code":code,"message":message}).to_string())
                .map(|v| Dispatched::Completed(HostOutcome::rejected(v)))
                .map_err(unreadable)
        }
        Err(e) => Err(match e.unknown_reason() {
            Some(reason) => TransportError::maybe_sent(reason, format!("{e:?}")),
            None => TransportError::unsent(format!("{e:?}")),
        }),
    }
}

pub struct CoreHost {
    transport: Arc<dyn Transport>,
}
impl CoreHost {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

pub fn validate_reply(kind: Primitive, value: Value) -> Result<Value, WireError> {
    let valid = match kind {
        Primitive::SinkDigest => {
            value["replayed"].is_boolean()
                && match value["disposition"].as_str() {
                    Some("stored") => value["fire_id"].is_string(),
                    Some("claim_in_fallback" | "episode_ended") => {
                        value.get("fire_id").is_some_and(Value::is_null)
                    }
                    _ => false,
                }
        }
        Primitive::SinkStatus => {
            value["replayed"].is_boolean()
                && match value["disposition"].as_str() {
                    Some("published") => {
                        value["accepted_revision"].as_i64().is_some()
                            && value["segment"].is_string()
                            && value["scope"].is_string()
                    }
                    Some("no_live_session") => {
                        value.get("accepted_revision").is_some_and(Value::is_null)
                            && value.get("segment").is_some_and(Value::is_null)
                            && value.get("scope").is_some_and(Value::is_null)
                    }
                    _ => false,
                }
        }
        // Fact leaves are deliberately left intact: unknown and denied are
        // data, not false or idle. Preserve unrecognised fact groups so an
        // older consumer still returns fields added by a newer core.
        Primitive::Facts => {
            value["agent_id"].is_string()
                && value["as_of"].as_i64().is_some()
                && value["home"].is_string()
                && value["core_boot_at"].as_i64().is_some()
        }
        _ => false,
    };
    if valid {
        Ok(value)
    } else {
        Err(WireError::Unreadable("unrecognised core reply".into()))
    }
}
/// Whether `value` is a code hash as core writes it: 64 lowercase hex digits.
fn is_code_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decodes core's `flow.install_status` reply, `{state, code_hash}`: an
/// `active` or `revoked` version carries the hash core approved, an
/// `unknown` one a null hash. Anything else is refused, so a reply basal
/// cannot read never counts as core standing behind a version.
pub fn decode_install_status(value: &Value) -> Result<InstallStatus, String> {
    let hash = value.get("code_hash");
    let approved_hash = || match hash.and_then(Value::as_str) {
        Some(h) if is_code_hash(h) => Ok(h.to_owned()),
        _ => Err(format!(
            "install status carries no valid code hash: {value}"
        )),
    };
    match value.get("state").and_then(Value::as_str) {
        Some("active") => Ok(InstallStatus::Active {
            code_hash: approved_hash()?,
        }),
        Some("revoked") => Ok(InstallStatus::Revoked {
            code_hash: approved_hash()?,
        }),
        Some("unknown") if hash.is_none_or(Value::is_null) => Ok(InstallStatus::Unknown),
        _ => Err(format!("unrecognised install status: {value}")),
    }
}

impl Host for CoreHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        match kind {
            CallKind::Primitive(Primitive::Facts) => CallClass::Query,
            CallKind::Primitive(Primitive::SinkDigest | Primitive::SinkStatus) => {
                CallClass::Mutation {
                    honours_idempotency_keys: true,
                }
            }
            _ => CallClass::Mutation {
                honours_idempotency_keys: false,
            },
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let (p, op) = match request.kind {
            CallKind::Primitive(Primitive::SinkDigest) => (Primitive::SinkDigest, "sink.digest"),
            CallKind::Primitive(Primitive::SinkStatus) => (Primitive::SinkStatus, "sink.status"),
            CallKind::Primitive(Primitive::Facts) => (Primitive::Facts, "agent.facts"),
            _ => return outcome(Err(WireError::NeverSent("not a core call".into()))),
        };
        let params = serde_json::from_str(request.args.as_str())
            .map_err(|e| WireError::NeverSent(e.to_string()));
        outcome(
            params
                .and_then(|v| self.transport.management(CORE, op, v))
                .and_then(|v| validate_reply(p, v)),
        )
    }
    fn now_ms(&self) -> f64 {
        system_now()
    }
    fn random(&self) -> f64 {
        sample()
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        // A refusal is no answer either: whatever core refused (a route
        // that is not `reserved:basal`, a malformed request), it did not say
        // it stands behind the version.
        let reply = self
            .transport
            .management(
                CORE,
                "flow.install_status",
                json!({"flow_id": flow_id, "version": version}),
            )
            .map_err(|e| {
                let detail = format!("flow.install_status: {e:?}");
                match e.unknown_reason() {
                    Some(reason) => TransportError::maybe_sent(reason, detail),
                    None => TransportError::unsent(detail),
                }
            })?;
        decode_install_status(&reply)
            .map_err(|detail| TransportError::maybe_sent(UnknownReason::ReplyUnreadable, detail))
    }
}
pub(crate) fn system_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}
pub(crate) fn sample() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let x = SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(0x9e3779b97f4a7c15);
    (x >> 11) as f64 / ((1u64 << 53) as f64)
}
