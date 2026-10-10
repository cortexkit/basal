//! `ck-callosum-stub`: a stand-in for the operator's callosum, for basal's
//! isolated ckdev-flows rig only (`script/flows-rig.sh`).
//!
//! Approval cards ask the operator to grant a flow's or package's capabilities.
//! The isolated daemon has no user's phone, so this module answers those cards
//! on the rig only. Core and cingulate require the daemon's reserved:callosum
//! stamp on the answering route; a caller cannot grant itself that stamp in JSON.
//! Basal also treats that principal as the operator. The rig runs the stub as
//! module callosum, authenticated by the supervisor's launch nonce. `script/flows-rig.sh` signs it as `ckdev-callosum` and
//! names it only in the rig's own daemon config; no production config ever
//! carries it, and it must never be deployed: anything that can reach it acts
//! as the operator.
//!
//! The rig's contract suite calls these management ops on it, and the stub
//! makes each call on its own route, so the target sees the operator:
//!
//! - `consent.list_pending` → cingulate `consent.list_pending` `{}`
//! - `consent.get` `{elicitationId}` → cingulate `consent.get`
//! - `consent.answer` `{elicitationId, choiceId}` → cingulate `consent.answer`
//! - `elicitation.list_pending`, `elicitation.get`, `elicitation.answer`
//!   → core's legacy card API, still used by basal's install owner
//! - `flow.revoke` `{flow_id, version?}` → core `flow.revoke`
//! - `basal.call` `{method, params}` → that basal op
//!
//! A successful reply is the target's result; a refusal keeps the target's
//! code and message.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use subc_client_rs::consumer::{CallError, CallOptions, ConsumerOptions, SubcConsumer};
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::manifest::{
    Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
};
use subc_protocol::{BindIdentity, PROTOCOL_VERSION, RouteTarget};
use tokio::sync::OnceCell;

/// The reserved module id whose routes prefrontal-core and basal both treat
/// as the operator's.
const MODULE_ID: &str = "callosum";
const CORE: &str = "prefrontal-core";
const BASAL: &str = "basal";
const CINGULATE: &str = "cingulate";
const TIMEOUT: Duration = Duration::from_secs(30);

/// The stub's management operations: public name, operation kind, and
/// manifest description.
const OPERATIONS: &[(&str, ManagementOperationKind, &str)] = &[
    (
        "elicitation.list_pending",
        ManagementOperationKind::Query,
        "List basal's legacy install cards in core.",
    ),
    (
        "elicitation.get",
        ManagementOperationKind::Query,
        "Read one legacy core card.",
    ),
    (
        "elicitation.answer",
        ManagementOperationKind::Mutate,
        "Answer one legacy core card as the operator.",
    ),
    (
        "consent.list_pending",
        ManagementOperationKind::Query,
        "List cingulate's pending consent cards, as the operator sees them.",
    ),
    (
        "consent.get",
        ManagementOperationKind::Query,
        "Read one consent card from cingulate, as the operator sees it.",
    ),
    (
        "consent.answer",
        ManagementOperationKind::Mutate,
        "Answer one consent card in cingulate as the operator.",
    ),
    (
        "flow.revoke",
        ManagementOperationKind::Mutate,
        "Revoke a flow's approved install in core as the operator.",
    ),
    (
        "basal.call",
        ManagementOperationKind::Mutate,
        "Call one basal op as the operator.",
    ),
];

fn manifest() -> ModuleManifest {
    ModuleManifest::builder(MODULE_ID, env!("CARGO_PKG_VERSION"))
        .protocol_ver(PROTOCOL_VERSION)
        .provides(vec![ProviderRole::ManagementSurface {
            operations: OPERATIONS
                .iter()
                .map(|(name, kind, description)| ManagementOperation {
                    name: (*name).to_owned(),
                    kind: kind.clone(),
                    description: Some((*description).to_owned()),
                })
                .collect(),
            config_schema: json!({"type": "object"}),
            observability: Vec::new(),
            identity_scope: Vec::new(),
            concurrency: Concurrency::ModuleManaged,
        }])
        .capabilities(None)
        .self_signals(None)
        .provenance(None)
        .build()
}

/// The connection file the daemon passes as `--subc <file>` or `--subc=<file>`.
fn connection_file() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text == "--subc" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = text.strip_prefix("--subc=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

struct Stub {
    connection_file: PathBuf,
    /// The stub's own consumer connection. Opened on first use, inside the
    /// supervised process, so its route opens carry the module id and launch
    /// nonce the daemon gave it and are attested `reserved:callosum`.
    consumer: OnceCell<SubcConsumer>,
}

fn error(code: &str, message: impl Into<String>) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: code.into(),
        message: message.into(),
    }
}

/// Cingulate permits card answers only on reserved:callosum routes with a
/// fed-phone: harness. Use that phone-class bind there, preserving core's bind.
fn operator_bind(module: &str) -> BindIdentity {
    let harness = if module == CINGULATE {
        "fed-phone:ckdev-flows"
    } else {
        MODULE_ID
    };
    BindIdentity::new("/", harness, "callosum:rig-stub")
}

impl Stub {
    async fn relay(&self, module: &str, method: &str, params: Value) -> HandlerOutcome {
        let consumer = match self
            .consumer
            .get_or_try_init(|| {
                SubcConsumer::connect(
                    &self.connection_file,
                    ConsumerOptions {
                        call_timeout: TIMEOUT,
                        ..Default::default()
                    },
                )
            })
            .await
        {
            Ok(consumer) => consumer,
            Err(e) => return error("stub_unconnected", e.to_string()),
        };
        let body = json!({ "method": method, "params": params });
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: module.to_owned(),
                },
                operator_bind(module),
                serde_json::to_vec(&body).expect("a JSON value always serialises"),
                CallOptions {
                    timeout: TIMEOUT,
                    ..Default::default()
                },
            )
            .await;
        match reply {
            // The target's envelope, `{result}` or `{error}`, passes through
            // unchanged.
            Ok(bytes) => HandlerOutcome::Response(bytes),
            Err(CallError::Module(body)) => error(&body.code, body.message),
            Err(e) => error(
                e.route_open_refusal()
                    .map(|body| body.code.as_str())
                    .unwrap_or("stub_transport"),
                e.to_string(),
            ),
        }
    }
}

#[async_trait]
impl ModuleHandler for Stub {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return error("invalid_request", e.to_string()),
        };
        let method = request["method"].as_str().unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "elicitation.list_pending" => {
                self.relay(CORE, "elicitation.list_pending_for_user", json!({}))
                    .await
            }
            "elicitation.get" | "elicitation.answer" => self.relay(CORE, method, params).await,
            "consent.list_pending" => {
                self.relay(CINGULATE, "consent.list_pending", json!({}))
                    .await
            }
            // The operator surface uses camelCase card and choice IDs.
            "consent.get" | "consent.answer" => self.relay(CINGULATE, method, params).await,
            "flow.revoke" => self.relay(CORE, method, params).await,
            "basal.call" => match params["method"].as_str() {
                Some(op) => {
                    let op_params = params.get("params").cloned().unwrap_or(Value::Null);
                    self.relay(BASAL, op, op_params).await
                }
                None => error("invalid_request", "basal.call needs a method"),
            },
            other => error("unknown_method", other),
        }
    }
}

fn main() -> std::process::ExitCode {
    let Some(connection_file) = connection_file() else {
        eprintln!("usage: ck-callosum-stub --subc <connection file> (rig only)");
        return std::process::ExitCode::from(64);
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("ck-callosum-stub: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let stub = Stub {
        connection_file,
        consumer: OnceCell::new(),
    };
    match runtime.block_on(subc_client_rs::serve(manifest(), stub)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ck-callosum-stub: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cingulate_operator_bind_uses_phone_harness_without_changing_core_bind() {
        let phone = serde_json::to_value(operator_bind(CINGULATE)).unwrap();
        let legacy = serde_json::to_value(operator_bind(CORE)).unwrap();
        assert_eq!(phone["harness"], "fed-phone:ckdev-flows");
        assert_eq!(legacy["harness"], "callosum");
        assert_eq!(phone["session"], legacy["session"]);
    }
}
