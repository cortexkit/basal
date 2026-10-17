//! Core's grant-checked sinks and facts. Replies remain visible to scripts.
use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};
use crate::{
    CallClass, CallRequest, Dispatched, Host, HostOutcome, InstallApproval, InstallStatus,
    TransportError, UnknownReason,
};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::{Value, json};
use std::sync::Arc;

pub const FLOW_DIGEST_REPLACE_KEY_V1: &str = "flow-digest-replace-key/v1";
pub const FLOW_OWNER_NOTICE_V1: &str = "flow-owner-notice/v1";

/// Manifest-declared ops that core accepts from `reserved:basal`, checking a
/// `{flow_id, flow_version, code_hash}` stamp against the approved install.
/// These use basal's own management route, not a provider route bound to the
/// flow's scope selector, so installing them does not need core to advertise
/// `flow-scopes/v1`. `board.read` pages live boards; `work.unsettled` lists
/// delivered work items whose owning head has not settled them yet.
pub const CORE_CARRIER_OPS: &[&str] = &["board.read", "work.unsettled"];

pub fn is_core_carrier_op(module: &str, op: &str) -> bool {
    module == CORE && CORE_CARRIER_OPS.contains(&op)
}

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
            let mut request = json!({"flow_id":flow_id,"flow_version":version,"run_id":run_id,"call_position":position,
            "agent":args["agent"],"action":args["action"],
            "due_at":due_at,"created_at":created_at,"item":args["item"],"claim":null});
            if let Some(tag) = args.get("options").and_then(|o| o.get("status_tag")) {
                request["status_tag"] = tag.clone();
            }
            if let Some(replace_key) = args.get("options").and_then(|o| o.get("replace_key")) {
                request["replace_key"] = replace_key.clone();
            }
            request
        }
        Primitive::SinkStatus => {
            json!({"flow_id":flow_id,"flow_version":version,"agent":args["agent"],
            "text":args["value"],"ttl_ms":STATUS_TTL_MS,"revision":created_at})
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
        Err(WireError::ScopeCut(detail)) => Err(TransportError::ScopeCut { detail }),
        Err(WireError::Typed(refusal)) => Err(TransportError::Refused(refusal)),
        Err(WireError::RefusedDetails {
            code,
            message,
            detail,
        }) => JsonText::new(json!({"code":code,"message":message,"detail":detail}).to_string())
            .map(|v| Dispatched::Completed(HostOutcome::rejected(v)))
            .map_err(unreadable),
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

    /// Read the current catalog for each gated write; a stale catalog cannot
    /// authorize a field that an older core may refuse.
    pub fn supports_capability(&self, capability: &str) -> bool {
        core_provides(self.transport.as_ref(), capability)
    }
}

/// Whether core's entry in the daemon's current catalog lists `capability`
/// among what it provides. A catalog that cannot be read provides nothing,
/// so a field an older core would refuse is never sent on a guess.
pub fn core_provides(transport: &dyn Transport, capability: &str) -> bool {
    transport
        .catalog()
        .ok()
        .and_then(|catalog| {
            catalog["modules"].as_array().map(|modules| {
                modules.iter().any(|module| {
                    module["module_id"] == CORE
                        && module["capabilities"]["provides"]
                            .as_array()
                            .is_some_and(|provided| provided.iter().any(|item| item == capability))
                })
            })
        })
        .unwrap_or(false)
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
pub(crate) fn is_code_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decodes core's `flow.install_status` reply. Unknown states and malformed
/// replies are errors, so basal leaves the install card pending without
/// approval.
const MAX_INSTALL_STATUS_TEXT_BYTES: usize = 256;

fn optional_install_status_text(value: &Value, field: &str) -> Result<Option<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= MAX_INSTALL_STATUS_TEXT_BYTES => {
            Ok(Some(text.clone()))
        }
        Some(Value::String(_)) => Err(format!(
            "install status {field} exceeds {MAX_INSTALL_STATUS_TEXT_BYTES} bytes"
        )),
        Some(_) => Err(format!("install status {field} is not a string")),
    }
}

pub fn decode_install_status(value: &Value) -> Result<InstallStatus, String> {
    let scope = crate::flow_scope::decode(value)?;
    // Scope metadata is validated on every status reply, but only an active
    // install may use it to authorize a routed call.
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
            scope,
        }),
        Some("revoked") => Ok(InstallStatus::Revoked {
            code_hash: approved_hash()?,
        }),
        Some("pending") => Ok(InstallStatus::Pending {
            code_hash: approved_hash()?,
        }),
        Some("executed") => Ok(InstallStatus::Pending {
            code_hash: approved_hash()?,
        }),
        Some("declined") => Ok(InstallStatus::Declined {
            code_hash: approved_hash()?,
        }),
        Some("expired") => Ok(InstallStatus::Expired {
            code_hash: approved_hash()?,
        }),
        Some("rejected") => {
            let code_hash = match hash {
                None | Some(Value::Null) => None,
                Some(Value::String(hash)) if is_code_hash(hash) => Some(hash.clone()),
                _ => {
                    return Err(format!(
                        "rejected install status carries an invalid code hash: {value}"
                    ));
                }
            };
            let attempt = match value.get("attempt") {
                None | Some(Value::Null) => None,
                Some(attempt) => match attempt.as_i64() {
                    Some(attempt) if attempt > 0 => Some(attempt),
                    _ => {
                        return Err(format!(
                            "rejected install status carries an invalid attempt: {value}"
                        ));
                    }
                },
            };
            Ok(InstallStatus::Rejected {
                code_hash,
                attempt,
                rejected_reason: optional_install_status_text(value, "rejected_reason")?,
                disposition: optional_install_status_text(value, "disposition")?,
            })
        }
        Some("unknown") if hash.is_none_or(Value::is_null) => Ok(InstallStatus::Unknown),
        _ => Err(format!("unrecognised install status: {value}")),
    }
}

impl Host for CoreHost {
    fn on_catalog_change(&self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.transport.on_catalog_change(notify);
    }
    fn owner_notice(&self, request: Value) -> Result<Value, WireError> {
        self.transport
            .management(CORE, "flow.owner_notice", request)
    }
    fn supports_core_capability(&self, capability: &str) -> bool {
        self.supports_capability(capability)
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        match kind {
            CallKind::Primitive(Primitive::Facts) => CallClass::Query,
            CallKind::Primitive(
                Primitive::SinkDigest | Primitive::SinkStatus | Primitive::SinkBoard,
            ) => CallClass::Mutation {
                honours_idempotency_keys: true,
            },
            _ => CallClass::Mutation {
                honours_idempotency_keys: false,
            },
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        let (p, op) = match request.kind {
            CallKind::Primitive(Primitive::SinkDigest) => (Primitive::SinkDigest, "sink.digest"),
            CallKind::Primitive(Primitive::SinkStatus) => (Primitive::SinkStatus, "sink.status"),
            CallKind::Primitive(Primitive::SinkBoard) => (Primitive::SinkBoard, "sink.board"),
            CallKind::Primitive(Primitive::Facts) => (Primitive::Facts, "agent.facts"),
            _ => return outcome(Err(WireError::NeverSent("not a core call".into()))),
        };
        let params = serde_json::from_str(request.args.as_str())
            .map_err(|e| WireError::NeverSent(e.to_string()));
        outcome(params.and_then(|mut v: Value| {
            if p == Primitive::SinkBoard {
                v["receipt_key"] = json!(request.idempotency_key);
            }
            let operation = v["operation"]["op"].as_str().map(str::to_owned);
            self.transport
                .management(CORE, op, v)
                .map_err(|e| crate::transport::contextual(e, CORE, op))
                .map_err(|e| {
                    if p == Primitive::SinkBoard {
                        board_refusal(e)
                    } else {
                        e
                    }
                })
                .and_then(|v| {
                    if p == Primitive::SinkBoard {
                        validate_board_reply(operation.as_deref(), v)
                    } else {
                        validate_reply(p, v)
                    }
                })
        }))
    }
    crate::live_host_services!();
    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        self.install_status_with_approval(flow_id, version)
            .map(|(status, _)| status)
    }
    fn install_status_with_approval(
        &self,
        flow_id: &str,
        version: u32,
    ) -> Result<(InstallStatus, Option<InstallApproval>), TransportError> {
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
        let decode = || -> Result<(InstallStatus, Option<InstallApproval>), String> {
            let status = decode_install_status(&reply)?;
            let approval = match reply.get("approval") {
                None => None,
                Some(value) if matches!(status, InstallStatus::Active { .. }) => {
                    let approval: InstallApproval = serde_json::from_value(value.clone())
                        .map_err(|e| format!("unreadable install approval: {e}"))?;
                    if matches!(&approval, InstallApproval::Policy { rule } if rule.is_empty()) {
                        return Err("empty policy rule".into());
                    }
                    Some(approval)
                }
                _ => return Err("only an active install may carry approval provenance".into()),
            };
            Ok((status, approval))
        };
        decode()
            .map_err(|detail| TransportError::maybe_sent(UnknownReason::ReplyUnreadable, detail))
    }
}

fn validate_board_reply(operation: Option<&str>, value: Value) -> Result<Value, WireError> {
    let valid = value.is_object()
        && match operation {
            Some("append") => {
                value.as_object().is_some_and(|o| o.len() == 2)
                    && value["lane_id"].as_str().is_some_and(|s| !s.is_empty())
                    && value["item_id"].as_str().is_some_and(|s| !s.is_empty())
            }
            Some("set_state" | "retire") => true,
            _ => false,
        };
    if valid {
        Ok(value)
    } else {
        Err(WireError::Unreadable(
            "unrecognised sink.board reply".into(),
        ))
    }
}

fn board_refusal(error: WireError) -> WireError {
    use crate::flow_refusal::{FlowRefusal, RefusalReason};
    let code = match &error {
        WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. } => code.as_str(),
        _ => return error,
    };
    let reason = match code {
        "sink_store_failed" => RefusalReason::SinkStoreFailed,
        "sink_audience_unavailable" => RefusalReason::SinkAudienceUnavailable,
        "sink_audience_check_required" => {
            tracing::error!(
                code,
                "core could not check the audience on basal's sink.board carrier route"
            );
            return error;
        }
        _ => return error,
    };
    WireError::Typed(FlowRefusal::new(reason, CORE, "sink.board"))
}
pub(crate) fn system_now() -> f64 {
    basal_proto::util::unix_millis() as f64
}
pub(crate) fn sample() -> f64 {
    let mut x = 0u64;
    // Live draws use OS entropy, not shared predictable process state. The
    // runtime journals each draw and replay serves that saved value instead.
    // There is deliberately no fallback when the OS cannot supply entropy.
    fill_entropy(&mut x);
    (x >> 11) as f64 / ((1u64 << 53) as f64)
}

#[cfg(unix)]
fn fill_entropy(x: &mut u64) {
    let rc = unsafe { libc::getentropy((x as *mut u64).cast(), std::mem::size_of::<u64>()) };
    assert_eq!(
        rc,
        0,
        "OS entropy unavailable: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(windows)]
fn fill_entropy(x: &mut u64) {
    // The system-preferred RNG needs no algorithm handle; flag value 2 is
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG.
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut core::ffi::c_void,
            buffer: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            (x as *mut u64).cast(),
            std::mem::size_of::<u64>() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    assert_eq!(status, 0, "OS entropy unavailable: NTSTATUS {status:#010x}");
}

/// A published status lasts half an hour unless core supersedes its revision.
const STATUS_TTL_MS: u64 = 30 * 60 * 1_000;
