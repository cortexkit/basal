//! Management calls over the rig's daemon.
//!
//! Every call is a management request `{method, params}` on a route the
//! daemon opens and stamps. The caller is whoever the daemon says opened the
//! route: this process is a plain local client, so its routes are `direct`;
//! calls relayed through the callosum stub arrive as `reserved:callosum`.
//!
//! The suite acts as its test agent the way a head does in production: it
//! calls core's `flow.relay` on a route bound with the head's harness and
//! session, and core sends the call on to basal on a route the daemon stamps
//! with the head's scope. The suite never opens a scoped route itself.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use subc_client_rs::consumer::{CallError, CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

/// A refusal: the provider's error code and message, or a transport failure
/// (code `transport`, or the daemon's route-open refusal code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub type Reply = Result<Value, Refusal>;

/// How core's `flow.relay` answered one call.
#[derive(Debug, Clone, PartialEq)]
pub enum Relayed {
    /// basal answered: its result, as core passed it on (`outcome: reply`).
    Reply(Value),
    /// basal refused: its code and message, as core passed them on
    /// (`outcome: refused`).
    Refused(Refusal),
    /// The call failed before basal answered: core's own refusal (an Error
    /// frame such as `flow_caller_not_registered` or `invalid_request`), a
    /// transport failure, or a result that is neither outcome.
    NotRelayed(Refusal),
}

impl Relayed {
    /// The relay's answer as JSON evidence, naming which of the three it was.
    pub fn evidence(&self) -> Value {
        match self {
            Self::Reply(value) => json!({ "reply": value }),
            Self::Refused(r) => json!({ "refused": { "code": r.code, "message": r.message } }),
            Self::NotRelayed(r) => {
                json!({ "not_relayed": { "code": r.code, "message": r.message } })
            }
        }
    }

    /// basal's result, if basal answered.
    pub fn reply(&self) -> Option<&Value> {
        match self {
            Self::Reply(value) => Some(value),
            _ => None,
        }
    }

    /// The answer as a plain [`Reply`]: basal's result, or whichever refusal
    /// came back, basal's or core's.
    pub fn into_reply(self) -> Reply {
        match self {
            Self::Reply(value) => Ok(value),
            Self::Refused(r) | Self::NotRelayed(r) => Err(r),
        }
    }
}

/// The reply as JSON evidence: `{"ok": result}` or `{"refused": {code, message}}`.
pub fn evidence(reply: &Reply) -> Value {
    match reply {
        Ok(value) => json!({ "ok": value }),
        Err(r) => json!({ "refused": { "code": r.code, "message": r.message } }),
    }
}

pub struct Client {
    consumer: SubcConsumer,
    timeout: Duration,
}

/// How long the suite waits for one `flow.relay` answer. Core bounds a relay
/// call by its own route-open and call timeouts, and resends once on a lost
/// route, so its answer can take longer than a direct call's.
const RELAY_TIMEOUT: Duration = Duration::from_secs(150);

impl Client {
    pub async fn connect(connection_file: &Path, timeout: Duration) -> Result<Self, String> {
        let consumer = SubcConsumer::connect(
            connection_file,
            ConsumerOptions {
                call_timeout: timeout,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("cannot connect through {}: {e}", connection_file.display()))?;
        Ok(Self { consumer, timeout })
    }

    fn options(&self) -> CallOptions {
        CallOptions {
            timeout: self.timeout,
            route_retry_deadline: self.timeout,
            ..Default::default()
        }
    }

    fn relay_options(&self) -> CallOptions {
        CallOptions {
            timeout: RELAY_TIMEOUT,
            route_retry_deadline: self.timeout,
            ..Default::default()
        }
    }

    /// One management call on an unscoped route bound with `identity`.
    pub async fn call(
        &self,
        module: &str,
        identity: &BindIdentity,
        method: &str,
        params: Value,
    ) -> Reply {
        let reply = self
            .consumer
            .call(
                target(module),
                identity.clone(),
                body(method, params),
                self.options(),
            )
            .await;
        decode(reply)
    }

    /// One `flow` tool call as a head makes it: core's `flow.relay` on a
    /// route bound with `head`, the head's own harness and session. Core
    /// takes the caller from that bind identity, so no caller goes in the
    /// params; `arguments` are basal's own params for `action`.
    pub async fn relay(&self, head: &BindIdentity, action: &str, arguments: Value) -> Relayed {
        let reply = self
            .consumer
            .call(
                target(RELAY_MODULE),
                head.clone(),
                body(
                    "flow.relay",
                    json!({ "action": action, "arguments": arguments }),
                ),
                self.relay_options(),
            )
            .await;
        let result = match decode(reply) {
            Ok(result) => result,
            Err(refusal) => return Relayed::NotRelayed(refusal),
        };
        match result["outcome"].as_str() {
            Some("reply") if result.get("reply").is_some() => {
                Relayed::Reply(result["reply"].clone())
            }
            Some("refused") => Relayed::Refused(Refusal {
                code: result["error"]["code"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned(),
                message: result["error"]["message"].as_str().unwrap_or("").to_owned(),
            }),
            _ => Relayed::NotRelayed(Refusal {
                code: "unknown_relay_outcome".into(),
                message: result.to_string(),
            }),
        }
    }

    /// A call relayed through the callosum stub: the stub's own route to its
    /// target carries the daemon-attested principal `reserved:callosum`.
    pub async fn operator(&self, op: &str, params: Value) -> Reply {
        self.call(STUB_MODULE, &stub_identity(), op, params).await
    }

    /// A basal op called as the operator, through the stub.
    pub async fn basal_as_operator(&self, method: &str, params: Value) -> Reply {
        self.operator("basal.call", json!({ "method": method, "params": params }))
            .await
    }

    pub async fn close(&self) {
        self.consumer.close().await;
    }
}

/// The callosum stub's module id.
pub const STUB_MODULE: &str = "callosum";

/// The module that serves heads their `flow` tool.
const RELAY_MODULE: &str = "prefrontal-core";

/// The bind identity the suite uses for its own unscoped routes. It is the
/// opener's word about itself and grants nothing: basal and core decide the
/// caller from the daemon's stamp.
pub fn suite_identity() -> BindIdentity {
    BindIdentity::new("/", "basal-rig", "basal-rig:contract")
}

fn stub_identity() -> BindIdentity {
    BindIdentity::new("/", "basal-rig", "basal-rig:operator")
}

fn target(module: &str) -> RouteTarget {
    RouteTarget::ManagementSurface {
        module_id: module.to_owned(),
    }
}

fn body(method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "method": method, "params": params }))
        .expect("a JSON value always serialises")
}

fn refusal(error: CallError) -> Refusal {
    if let Some(body) = error.route_open_refusal() {
        return Refusal {
            code: body.code.clone(),
            message: body.message.clone(),
        };
    }
    Refusal {
        code: error.code().unwrap_or("transport").to_owned(),
        message: error.to_string(),
    }
}

fn decode(reply: Result<Vec<u8>, CallError>) -> Reply {
    let bytes = reply.map_err(refusal)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| Refusal {
        code: "undecodable_reply".into(),
        message: format!("{e}: {}", String::from_utf8_lossy(&bytes)),
    })?;
    if let Some(error) = value.get("error") {
        return Err(Refusal {
            code: error["code"].as_str().unwrap_or("unknown").to_owned(),
            message: error["message"].as_str().unwrap_or("").to_owned(),
        });
    }
    match value.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(Refusal {
            code: "no_result_envelope".into(),
            message: value.to_string(),
        }),
    }
}
