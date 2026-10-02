//! Management calls over the rig's daemon.
//!
//! Every call is a management request `{method, params}` on a route the
//! daemon opens and stamps. The caller is whoever the daemon says opened the
//! route: this process is a plain local client, so its unscoped routes are
//! `direct`; a route opened under a core scope carries that scope's stamp;
//! calls relayed through the callosum stub arrive as `reserved:callosum`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use subc_client_rs::consumer::{CallError, CallOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::scope::ScopeSelector;
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

    /// One management call on a route admitted under `scope`, which the
    /// daemon stamps on the provider's bind.
    pub async fn call_scoped(
        &self,
        module: &str,
        identity: &BindIdentity,
        scope: &ScopeSelector,
        method: &str,
        params: Value,
    ) -> Reply {
        let handle = self
            .consumer
            .open_route_scoped(
                target(module),
                identity.clone(),
                scope.clone(),
                self.options(),
            )
            .await
            .map_err(refusal)?;
        decode(
            self.consumer
                .request(&handle, body(method, params), self.options())
                .await,
        )
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
