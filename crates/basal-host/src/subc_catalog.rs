//! Daemon declarations are read on demand so revocations do not linger in a cache.
use crate::transport::{Transport, WireError};
use crate::{Catalog, EventBody, EventDecl, EventOrigin, OpDecl, OpKind};
use serde_json::{Value, json};
use std::sync::Arc;
use subc_protocol::manifest::{
    EventDeclaration, ExecutionMode, ManagementOperationKind, ProviderRole,
};

/// Convert the publisher's manifest declarations, not inferred subject names.
/// Notices contain no event body; the read-only `events_get` tool supplies
/// that body under the receiving flow's own daemon scope, before script startup.
pub fn declared_event(events: &[EventDeclaration], name: &str, version: u32) -> Option<EventDecl> {
    events
        .iter()
        .find(|e| e.name == name && e.version == version)
        .map(|_| EventDecl {
            // The declaration has no provenance marker. Treat all publisher text
            // as external rather than granting trust the publisher did not declare.
            origin: EventOrigin::External,
            body: EventBody::Resolved {
                resolve_op: crate::catalog::EVENT_BODY_OP.into(),
            },
        })
}

/// Adapter pending the daemon catalog release that carries EventDeclaration.
/// The current catalog has no authoritative event field: fail closed until
/// that field ships, rather than treating roles or subject names as declarations.
fn catalog_event_declarations(_entry: &Value) -> Option<Vec<EventDeclaration>> {
    None
}

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
        self.resolve_for_dispatch(module, op).map(|(decl, _)| decl)
    }
    /// Scope capability and operation kind come from the same fresh snapshot.
    /// A missing provider is not evidence that an operation was removed: the
    /// provider may simply be disconnected while the daemon rebuilds its catalog.
    pub fn resolve_for_dispatch(
        &self,
        module: &str,
        op: &str,
    ) -> Result<(Option<CatalogOp>, bool), WireError> {
        let catalog = self.transport.catalog()?;
        let modules = catalog
            .get("modules")
            .and_then(Value::as_array)
            .ok_or_else(|| WireError::Unknown("catalog modules missing".into()))?;
        let Some(entry) = modules
            .iter()
            .find(|e| e.get("module_id").and_then(Value::as_str) == Some(module))
        else {
            return Err(WireError::NeverSent(format!(
                "provider {module} is not currently catalogued"
            )));
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
        let flow_capable = entry["capabilities"]["provides"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|c| c == "flow-scopes/v1"));
        Ok((found, flow_capable))
    }
    pub fn known_agent(&self, agent: &str) -> Result<bool, WireError> {
        self.resolve_agent(agent).map(|id| id.is_some())
    }

    /// Resolves either public agent identifier through core's agent registry.
    pub fn resolve_agent(&self, agent: &str) -> Result<Option<String>, WireError> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut named = std::collections::BTreeSet::new();
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
            for entry in agents {
                let id = entry
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| WireError::Unreadable("agent.list agent_id missing".into()))?;
                if id == agent {
                    return Ok(Some(id.to_owned()));
                }
                if entry.get("name").and_then(Value::as_str) == Some(agent) {
                    named.insert(id.to_owned());
                }
            }
            match reply.get("next_cursor") {
                None | Some(Value::Null) => {
                    return match named.len() {
                        0 => Ok(None),
                        1 => Ok(named.into_iter().next()),
                        _ => Err(WireError::Unreadable("ambiguous agent display name".into())),
                    };
                }
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
    fn event(&self, module: &str, name: &str, version: u32) -> Option<EventDecl> {
        let catalog = self.transport.catalog().ok()?;
        let entry = catalog["modules"]
            .as_array()?
            .iter()
            .find(|e| e["module_id"] == module)?;
        declared_event(&catalog_event_declarations(entry)?, name, version)
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
    fn agent_id(&self, agent: &str) -> Option<String> {
        self.resolve_agent(agent).ok().flatten()
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn catalog_event_lookup_requires_the_declared_name_and_version() {
        let events =
            vec![EventDeclaration::new("github_pr_changed", 1).with_headers(vec!["repo".into()])];
        let declaration = declared_event(&events, "github_pr_changed", 1).expect("declared event");
        assert_eq!(
            declaration.body,
            EventBody::Resolved {
                resolve_op: crate::catalog::EVENT_BODY_OP.into()
            }
        );
        assert_eq!(declaration.origin, EventOrigin::External);
        assert!(declared_event(&events, "github_issue_changed", 1).is_none());
        assert!(declared_event(&events, "github_pr_changed", 2).is_none());
        assert!(declared_event(&[], "github_pr_changed", 1).is_none());
    }
}
