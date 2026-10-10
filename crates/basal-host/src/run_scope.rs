//! prefrontal-core authorizes run scopes. The invoking tool route's daemon stamp
//! records the verified principal, scope owner, reference, epoch and agent_id.
//! Basal sends that agent and scope identity with a fixed expiry to core over its
//! own route stamped reserved:basal, the daemon-verified basal module principal.
//! The confined worker never makes these authority-bearing control requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};

pub const WALL_MS: i64 = 30 * 60 * 1_000;
pub const PERSON_WAIT_MS: i64 = 10 * 60 * 1_000;
pub const SCOPE_LIFETIME_MS: i64 = WALL_MS + PERSON_WAIT_MS + 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(rename = "ref")]
    pub reference: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    pub agent_id: String,
    pub invoking_scope: Scope,
    pub run_id: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub tool: String,
    pub module: String,
    pub op: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub scope: Scope,
    pub expires_at_ms: i64,
    pub catalog: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Close {
    pub run_id: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub closed: bool,
    pub already_closed: bool,
}

pub fn open(transport: &dyn Transport, request: &Open) -> Result<Opened, WireError> {
    call(transport, "codemode.run_scope.open", request)
}

pub fn close(transport: &dyn Transport, request: &Close) -> Result<Closed, WireError> {
    call(transport, "codemode.run_scope.close", request)
}

fn call<T: Serialize, R: for<'de> Deserialize<'de>>(
    transport: &dyn Transport,
    op: &str,
    request: &T,
) -> Result<R, WireError> {
    let params = serde_json::to_value(request).map_err(|e| WireError::NeverSent(e.to_string()))?;
    let value: Value = transport.management(CORE, op, params)?;
    serde_json::from_value(value).map_err(|e| WireError::Unreadable(e.to_string()))
}

/// Resolve only tools declared by the daemon, not schemas carried by core or
/// script input. Basal inserts its own model tools separately.
pub fn catalog(daemon: &Value, entries: &[CatalogEntry]) -> Result<Value, WireError> {
    let modules = daemon["modules"]
        .as_array()
        .ok_or_else(|| WireError::Unreadable("daemon catalog has no modules".into()))?;
    let mut resolved = Vec::new();
    for entry in entries {
        if entry.tool == "codemode" || (entry.module == "basal" && entry.op == "codemode") {
            return Err(WireError::Unreadable(
                "recursive codemode catalog entry".into(),
            ));
        }
        let module = modules
            .iter()
            .find(|module| module["module_id"] == entry.module)
            .ok_or_else(|| WireError::NeverSent(format!("provider {} is absent", entry.module)))?;
        if !module["capabilities"]["provides"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "agent-run-scopes/v1"))
        {
            return Err(WireError::NeverSent(format!(
                "provider {} does not accept run scopes",
                entry.module
            )));
        }
        let roles: Vec<subc_protocol::manifest::ProviderRole> =
            serde_json::from_value(module["roles"].clone())
                .map_err(|e| WireError::Unreadable(e.to_string()))?;
        let mut schemas = roles
            .into_iter()
            .filter_map(|role| match role {
                subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } => Some(tools),
                _ => None,
            })
            .flatten()
            .filter(|tool| tool.name == entry.op)
            .map(|tool| tool.schema);
        let schema = schemas.next().ok_or_else(|| {
            WireError::NeverSent(format!("tool {}.{} is absent", entry.module, entry.op))
        })?;
        if schemas.next().is_some() {
            return Err(WireError::Unreadable("ambiguous tool declaration".into()));
        }
        resolved.push(serde_json::json!({"name":entry.tool,"module":entry.module,"op":entry.op,"input_schema":schema}));
    }
    Ok(Value::Array(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_run_scope_wire_shapes() {
        let request = Open {
            agent_id: "agent-1".into(),
            invoking_scope: Scope {
                reference: "agent:1".into(),
                epoch: 7,
            },
            run_id: "run-1".into(),
            expires_at_ms: 2_460_123,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"agent_id":"agent-1","invoking_scope":{"ref":"agent:1","epoch":7},"run_id":"run-1","expires_at_ms":2_460_123})
        );
        let reply = json!({"scope":{"ref":"run:1","epoch":9},"expires_at_ms":2_460_123,"catalog":[{"tool":"read","module":"aft","op":"read"}]});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Opened>(reply.clone()).unwrap()).unwrap(),
            reply
        );
        assert_eq!(
            serde_json::to_value(Close {
                run_id: "run-1".into(),
                epoch: 9
            })
            .unwrap(),
            json!({"run_id":"run-1","epoch":9})
        );
        let reply = json!({"closed":false,"already_closed":true});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Closed>(reply.clone()).unwrap()).unwrap(),
            reply
        );
    }
}
