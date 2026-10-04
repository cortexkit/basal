//! Daemon declarations are read on demand so revocations do not linger in a cache.
use crate::transport::{Transport, WireError};
use crate::{Catalog, EventDecl, OpDecl, OpKind};
use serde_json::{Value, json};
use std::sync::Arc;
use subc_protocol::manifest::{ExecutionMode, ManagementOperationKind, ProviderRole};

pub const CORE: &str = "prefrontal-core";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Management,
    Tool,
}
#[derive(Debug, Clone)]
pub struct CatalogOp {
    pub declaration: OpDecl,
    pub surface: Surface,
    pub execution_mode: Option<ExecutionMode>,
}

pub struct SubcCatalog {
    transport: Arc<dyn Transport>,
    denylist: Vec<(String, String)>,
}
impl SubcCatalog {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            denylist: vec![
                ("aft".into(), "bash".into()),
                ("basal".into(), "codemode".into()),
            ],
        }
    }
    pub fn with_denylist(mut self, denylist: Vec<(String, String)>) -> Self {
        self.denylist = denylist;
        self
    }
    pub fn resolve(&self, module: &str, op: &str) -> Result<Option<CatalogOp>, WireError> {
        let catalog = self.transport.catalog()?;
        let modules = catalog
            .get("modules")
            .and_then(Value::as_array)
            .ok_or_else(|| WireError::Unknown("catalog modules missing".into()))?;
        let Some(entry) = modules
            .iter()
            .find(|e| e.get("module_id").and_then(Value::as_str) == Some(module))
        else {
            return Ok(None);
        };
        let roles: Vec<ProviderRole> = serde_json::from_value(
            entry
                .get("roles")
                .cloned()
                .ok_or_else(|| WireError::Unknown("catalog roles missing".into()))?,
        )
        .map_err(|e| WireError::Unknown(format!("catalog roles: {e}")))?;
        let mut found = None;
        for role in roles {
            let candidate = match role {
                ProviderRole::ManagementSurface { operations, .. } => {
                    operations.into_iter().find(|o| o.name == op).map(|o| {
                        (
                            Surface::Management,
                            match o.kind {
                                ManagementOperationKind::Query => OpKind::Query,
                                ManagementOperationKind::Mutate => OpKind::Mutate,
                            },
                            None,
                        )
                    })
                }
                ProviderRole::ToolProvider { tools, .. } => {
                    tools.into_iter().find(|t| t.name == op).map(|t| {
                        (
                            Surface::Tool,
                            if t.execution_mode == ExecutionMode::Pure {
                                OpKind::Query
                            } else {
                                OpKind::Mutate
                            },
                            Some(t.execution_mode),
                        )
                    })
                }
                _ => None,
            };
            if let Some((surface, kind, execution_mode)) = candidate {
                if found.is_some() {
                    return Err(WireError::Unknown(
                        "ambiguous op on management and tool surfaces".into(),
                    ));
                }
                // The protocol describes replay safety but does not declare
                // whether an operation runs a shell. The operator's denylist
                // supplies that missing declaration for known shell operations.
                let shell_capable = self
                    .denylist
                    .iter()
                    .any(|(m, o)| m.eq_ignore_ascii_case(module) && o.eq_ignore_ascii_case(op));
                found = Some(CatalogOp {
                    declaration: OpDecl {
                        kind: Some(kind),
                        cause_echo: false,
                        shell_capable,
                    },
                    surface,
                    execution_mode,
                });
            }
        }
        Ok(found)
    }
    pub fn known_agent(&self, agent: &str) -> Result<bool, WireError> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = Value::String(c.clone());
            }
            let reply = self.transport.management(CORE, "agent.list", params)?;
            let agents = reply
                .get("agents")
                .and_then(Value::as_array)
                .ok_or_else(|| WireError::Unknown("agent.list agents missing".into()))?;
            if agents.iter().any(|a| {
                a.get("agent_id").and_then(Value::as_str) == Some(agent)
                    || a.get("name").and_then(Value::as_str) == Some(agent)
            }) {
                return Ok(true);
            }
            match reply.get("next_cursor") {
                None | Some(Value::Null) => return Ok(false),
                Some(Value::String(c)) if seen.insert(c.clone()) => cursor = Some(c.clone()),
                _ => {
                    return Err(WireError::Unknown(
                        "invalid or repeated agent cursor".into(),
                    ));
                }
            }
        }
    }
}
impl Catalog for SubcCatalog {
    fn supports_flow_scopes(&self, module: &str) -> bool {
        self.transport
            .catalog()
            .ok()
            .and_then(|c| {
                c["modules"].as_array().and_then(|modules| {
                    modules.iter().find(|m| m["module_id"] == module).map(|m| {
                        m["capabilities"]["provides"]
                            .as_array()
                            .is_some_and(|caps| caps.iter().any(|c| c == "flow-scopes/v1"))
                    })
                })
            })
            .unwrap_or(false)
    }
    fn event(&self, _: &str, _: &str, _: u32) -> Option<EventDecl> {
        None
    }
    fn op(&self, module: &str, op: &str) -> Option<OpDecl> {
        self.resolve(module, op)
            .ok()
            .flatten()
            .map(|o| o.declaration)
    }
    fn agent_known(&self, agent: &str) -> bool {
        self.known_agent(agent).unwrap_or(false)
    }
}
