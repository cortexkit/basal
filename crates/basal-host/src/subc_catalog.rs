//! Daemon declarations are read on demand so revocations do not linger in a cache.
use crate::transport::{Transport, WireError};
use crate::{Catalog, EventBody, EventDecl, EventOrigin, OpDecl, OpKind};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use subc_protocol::manifest::{
    CAP_CATALOG_EVENTS_V1, EventDeclaration, ExecutionMode, ManagementOperationKind, ProviderRole,
};

/// Daemon-wide capabilities from basal's latest registration acknowledgement.
/// Until an acknowledgement arrives, catalog event fields are not authoritative.
#[derive(Default)]
pub struct DaemonCapabilities {
    catalog_events: AtomicBool,
}

impl DaemonCapabilities {
    pub fn supports_catalog_events(&self) -> bool {
        self.catalog_events.load(Ordering::SeqCst)
    }

    pub fn observe_hello_ack(&self, ack: &subc_protocol::ModuleHelloAckBody) {
        self.catalog_events.store(
            ack.subc_capabilities
                .iter()
                .any(|cap| cap == CAP_CATALOG_EVENTS_V1),
            Ordering::SeqCst,
        );
    }
}

/// Convert the publisher's manifest declarations, not inferred subject names.
/// Notices contain no event body; the read-only `events_get` tool supplies
/// that body under the receiving flow's own daemon scope, before script startup.
pub fn declared_event(events: &[EventDeclaration], name: &str, version: u32) -> Option<EventDecl> {
    events
        .iter()
        .find(|e| e.name == name && e.version == version)
        .map(|event| EventDecl {
            headers: event.headers.clone(),
            // The declaration has no provenance marker. Treat all publisher text
            // as external rather than granting trust the publisher did not declare.
            origin: EventOrigin::External,
            body: EventBody::Resolved {
                resolve_op: crate::catalog::EVENT_BODY_OP.into(),
            },
        })
}

/// Only explicit declarations count; roles and subject names do not declare events.
fn catalog_event_declarations(entry: &Value) -> Option<Vec<EventDeclaration>> {
    entry
        .get("events")?
        .as_array()?
        .iter()
        .map(|event| {
            let mut event = event.clone();
            // Missing filter hints do not change whether the event is declared.
            event.as_object_mut()?.entry("headers").or_insert(json!([]));
            serde_json::from_value(event).ok()
        })
        .collect()
}

/// A fresh call key for a read-only role op (`role.describe`,
/// `tool.catalog`). Such a read changes nothing, so the key only has to be
/// well formed and not collide with an earlier call.
fn role_call_key() -> String {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "basal-catalog-read-{nanos}-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Reads `module`'s own `tool.catalog` answer: `role.describe` first, then
/// `tool.catalog` asking for every capability tag the provider serves. The
/// answer carries per-tool declarations the daemon catalog does not, such as
/// `emits_events` and `modes`. A failed call, a describe that fails the
/// role's checks, or an answer that does not decode is an error.
///
/// Each of the two requests is bounded by the transport's call timeout.
pub fn read_tool_catalog(
    transport: &dyn Transport,
    module: &str,
) -> Result<cortexkit_role_tool_provider::catalog::CatalogAnswer, WireError> {
    read_tool_catalog_with(&mut |name, arguments| {
        transport.tool(module, name, arguments, &role_call_key())
    })
}

/// `read_tool_catalog` for a codemode run: both requests go on a route
/// opened under the run's own scope `scope` and bound with `identity`, so the
/// provider sees the codemode call mode its run binds will carry, and they
/// carry no envelope call key. Providers such as AFT refuse a bind whose
/// harness is not one they recognise, and providers such as cerebellum admit
/// basal only on a codemode run scope.
///
/// Neither request carries the run's catalog preset, in the `tool.catalog`
/// arguments or in the request envelope, even when core chose one for the
/// run. The tool-provider role lets a provider that defines no presets refuse
/// `CatalogRequest.preset`, and such providers do refuse it, which would drop
/// all of their tools from the run. The preset-less answer is also the one
/// place a provider lists the presets it serves, and only that list decides
/// which providers get the run's preset on their tool calls (see
/// `run_scope::provider_serves_preset`). A provider that does define presets,
/// such as AFT, answers a preset-less catalog read with its `head` catalog and
/// enforces the run's preset when a tool is called, from the preset on the
/// tool call's own request.
pub fn read_tool_catalog_on_run(
    transport: &dyn Transport,
    scope: &subc_protocol::scope::ScopeSelector,
    identity: &subc_protocol::BindIdentity,
    module: &str,
) -> Result<cortexkit_role_tool_provider::catalog::CatalogAnswer, WireError> {
    read_tool_catalog_with(&mut |name, arguments| {
        transport.role_on_run_scope(scope, identity, module, name, arguments)
    })
}

fn read_tool_catalog_with(
    send: &mut dyn FnMut(&str, Value) -> Result<Value, WireError>,
) -> Result<cortexkit_role_tool_provider::catalog::CatalogAnswer, WireError> {
    use cortexkit_role_tool_provider::catalog::{
        CatalogAnswer, CatalogRequest, params_for_capabilities,
    };
    use cortexkit_role_tool_provider::describe::{check_describe, select_tool_capabilities};
    let describe = send("role.describe", json!({}))?;
    let describe = check_describe(&describe)
        .map_err(|problems| WireError::Unreadable(format!("role.describe: {problems:?}")))?;
    // A provider that filters its catalog by capability tag answers an
    // empty request with no tools. Ask such a provider for every tag it
    // serves, so the answer holds every tool that carries any tag.
    let params = match select_tool_capabilities(&describe, &describe.tool_capabilities) {
        Some(tags) => params_for_capabilities(tags),
        None => serde_json::Map::new(),
    };
    let request = serde_json::to_value(CatalogRequest::new(params))
        .map_err(|e| WireError::NeverSent(e.to_string()))?;
    let answer = send("tool.catalog", request)?;
    let answer: CatalogAnswer = serde_json::from_value(answer)
        .map_err(|e| WireError::Unreadable(format!("tool.catalog answer: {e}")))?;
    Ok(answer)
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
    daemon_capabilities: Arc<DaemonCapabilities>,
}
impl SubcCatalog {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            daemon_capabilities: Arc::new(DaemonCapabilities::default()),
            denylist: vec![
                ("aft".into(), "bash".into()),
                ("basal".into(), "codemode".into()),
            ],
        }
    }
    pub fn with_daemon_capabilities(mut self, capabilities: Arc<DaemonCapabilities>) -> Self {
        self.daemon_capabilities = capabilities;
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
    /// Reads `module`'s own declaration of whether its tool `op` can make it
    /// emit an event: the typed `emits_events` of the tool's entry in the
    /// module's `tool.catalog` answer. A management op, or a tool the answer
    /// does not list, declares nothing (`Ok(None)`). A failed call or an
    /// answer that does not decode is an error, never a declaration.
    ///
    /// Each request is bounded by the transport's call timeout.
    pub fn tool_emits_events(&self, module: &str, op: &str) -> Result<Option<bool>, WireError> {
        // Only tools carry the declaration; a management op has none to read.
        match self.resolve(module, op)? {
            Some(found) if found.surface == Surface::Tool => {}
            _ => return Ok(None),
        }
        let answer = read_tool_catalog(self.transport.as_ref(), module)?;
        Ok(answer
            .tools
            .into_iter()
            .find(|tool| tool.name == op)
            .and_then(|tool| tool.emits_events))
    }
    pub fn known_agent(&self, agent: &str) -> Result<bool, WireError> {
        self.resolve_agent(agent).map(|id| id.is_some())
    }

    pub fn known_workspace(&self, id: &str) -> Result<bool, WireError> {
        Ok(self.workspace_entry(id)?.is_some())
    }

    fn workspace_entry(&self, id: &str) -> Result<Option<Value>, WireError> {
        let Some(op) = self.resolve("entorhinal", "enumerate")? else {
            return Err(WireError::Unreadable("entorhinal.enumerate missing".into()));
        };
        if op.surface != Surface::Management || op.declaration.kind != Some(OpKind::Query) {
            return Err(WireError::Unreadable(
                "entorhinal.enumerate is not a management query".into(),
            ));
        }
        let reply =
            self.transport
                .management("entorhinal", "enumerate", json!({"workspaceId": id}))?;
        let workspaces = reply["workspaces"].as_array().ok_or_else(|| {
            WireError::Unreadable("entorhinal.enumerate workspaces missing".into())
        })?;
        let mut found = None;
        for workspace in workspaces {
            let workspace_id = workspace["workspaceId"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    WireError::Unreadable("entorhinal.enumerate workspaceId missing".into())
                })?;
            if workspace_id == id {
                found = Some(workspace.clone());
            }
        }
        Ok(found)
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
    fn workspace_name(&self, id: &str) -> Result<Option<String>, WireError> {
        Ok(self
            .workspace_entry(id)?
            .and_then(|entry| entry["name"].as_str().map(str::to_owned)))
    }
    fn workspaces_of(&self, agent_id: &str) -> Result<Vec<String>, WireError> {
        let Some(op) = self.resolve("entorhinal", "agent.fleet_identity")? else {
            return Err(WireError::Unreadable(
                "entorhinal.agent.fleet_identity missing".into(),
            ));
        };
        if op.surface != Surface::Management || op.declaration.kind != Some(OpKind::Query) {
            return Err(WireError::Unreadable(
                "entorhinal.agent.fleet_identity is not a management query".into(),
            ));
        }
        // Transport already decodes the management result envelope. Decoding it
        // again would hide a malformed double envelope or discard a valid reply.
        let reply = self
            .transport
            .management("entorhinal", "agent.fleet_identity", json!({}))?;
        let agents = reply["agents"]
            .as_array()
            .ok_or_else(|| WireError::Unreadable("agent.fleet_identity agents missing".into()))?;
        let mut membership = Vec::new();
        for entry in agents {
            let id = entry["agentId"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    WireError::Unreadable("agent.fleet_identity agentId missing".into())
                })?;
            let workspace = match entry.get("workspace") {
                None => None,
                Some(value) => Some(
                    value["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            WireError::Unreadable(
                                "agent.fleet_identity workspace id missing".into(),
                            )
                        })?,
                ),
            };
            if id == agent_id {
                membership.extend(workspace.map(str::to_owned));
            }
        }
        if membership.len() > 1 {
            return Err(WireError::Unreadable(
                "duplicate agent workspace placement".into(),
            ));
        }
        Ok(membership)
    }
    fn workspace_known(&self, id: &str) -> Result<bool, WireError> {
        self.known_workspace(id)
    }
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
        if !self.daemon_capabilities.supports_catalog_events() {
            return None;
        }
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
    fn emits_events(&self, module: &str, op: &str) -> Result<Option<bool>, WireError> {
        self.tool_emits_events(module, op)
    }
    fn agent_known(&self, agent: &str) -> Result<bool, WireError> {
        self.known_agent(agent)
    }
    fn agent_id(&self, agent: &str) -> Result<Option<String>, WireError> {
        self.resolve_agent(agent)
    }
    fn resolve_agents(&self, agents: &[&str]) -> Result<Vec<Option<String>>, WireError> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut entries = Vec::new();
        loop {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let reply = self.transport.management(CORE, "agent.list", params)?;
            for entry in reply["agents"]
                .as_array()
                .ok_or_else(|| WireError::Unreadable("agent.list agents missing".into()))?
            {
                let id = entry["agent_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| WireError::Unreadable("agent.list agent_id missing".into()))?;
                entries.push((id.to_owned(), entry["name"].as_str().map(str::to_owned)));
            }
            match reply.get("next_cursor") {
                None | Some(Value::Null) => break,
                Some(Value::String(next)) if seen.insert(next.clone()) => {
                    cursor = Some(next.clone())
                }
                _ => {
                    return Err(WireError::Unreadable(
                        "invalid or repeated agent cursor".into(),
                    ));
                }
            }
        }
        agents
            .iter()
            .map(|agent| {
                if let Some((id, _)) = entries.iter().find(|(id, _)| id == agent) {
                    return Ok(Some(id.clone()));
                }
                let named: std::collections::BTreeSet<_> = entries
                    .iter()
                    .filter(|(_, name)| name.as_deref() == Some(*agent))
                    .map(|(id, _)| id.clone())
                    .collect();
                match named.len() {
                    0 => Ok(None),
                    1 => Ok(named.into_iter().next()),
                    _ => Err(WireError::Unreadable("ambiguous agent display name".into())),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn catalog_event_mapping_preserves_names_versions_and_filter_headers() {
        let entry = json!({"events":[
            {"name":"github_pr_changed","version":1,"headers":["repo","project","pr"]},
            {"name":"digest_delivered","version":2}
        ]});
        let events = catalog_event_declarations(&entry).unwrap();
        assert_eq!(
            events,
            vec![
                EventDeclaration::new("github_pr_changed", 1).with_headers(vec![
                    "repo".into(),
                    "project".into(),
                    "pr".into()
                ]),
                EventDeclaration::new("digest_delivered", 2),
            ]
        );
        assert_eq!(
            catalog_event_declarations(&json!({"events":[]})),
            Some(vec![])
        );
        assert!(catalog_event_declarations(&json!({})).is_none());
        assert!(catalog_event_declarations(&json!({"events":[{"name":"broken"}]})).is_none());
    }

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

#[cfg(test)]
mod emits_events_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A daemon whose catalog lists plexus with a management mutation
    /// `label` and a mutating tool `react`, and whose plexus answers each
    /// role op with what the test set for it.
    #[derive(Default)]
    struct Plexus {
        answers: Mutex<BTreeMap<String, Result<Value, WireError>>>,
        /// Each role op call: its name, arguments and call key.
        calls: Mutex<Vec<(String, Value, String)>>,
    }

    impl Plexus {
        fn answering(describe: Value, catalog: Result<Value, WireError>) -> Arc<Self> {
            let plexus = Arc::new(Self::default());
            let mut answers = plexus.answers.lock().unwrap();
            answers.insert("role.describe".into(), Ok(describe));
            answers.insert("tool.catalog".into(), catalog);
            drop(answers);
            plexus
        }
        fn calls(&self) -> Vec<(String, Value, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Transport for Plexus {
        fn catalog(&self) -> Result<Value, WireError> {
            Ok(
                json!({"modules":[{"module_id":"plexus","capabilities":{"provides":[]},"roles":[
                    {"role":"management_surface","operations":[{"name":"label","kind":"mutate"}],
                     "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"},
                    {"role":"tool_provider","tools":[{"name":"react","execution_mode":"mutating","schema":{}}],
                     "identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
                ]}]}),
            )
        }
        fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
            panic!("unexpected management op {op}")
        }
        fn tool(
            &self,
            module: &str,
            name: &str,
            arguments: Value,
            call_key: &str,
        ) -> Result<Value, WireError> {
            assert_eq!(module, "plexus");
            self.calls
                .lock()
                .unwrap()
                .push((name.into(), arguments, call_key.into()));
            self.answers
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .unwrap_or_else(|| panic!("unexpected role op {name}"))
        }
    }

    fn describe() -> Value {
        json!({"majors":[{"version":"tool-provider/v1","ops":["role.describe","tool.catalog"],
            "stability":"alpha"}],"implementation_version":"1"})
    }

    /// A `tool.catalog` answer listing `react` with `emits_events` set to
    /// `declared`, or without the member when `declared` is `None`.
    fn answer(declared: Option<Value>) -> Value {
        let mut react = json!({"name":"react","schema_digest":"0".repeat(64),"semantics":1,
            "capabilities":[],"input_schema":{"type":"object"}});
        if let Some(declared) = declared {
            react["emits_events"] = declared;
        }
        json!({"generation":"g","catalog_digest":"d","tools":[react]})
    }

    fn read(plexus: &Arc<Plexus>, op: &str) -> Result<Option<bool>, WireError> {
        SubcCatalog::new(plexus.clone()).emits_events("plexus", op)
    }

    #[test]
    fn reads_the_tools_typed_declaration_from_its_modules_catalog() {
        for (declared, expected) in [
            (Some(json!(false)), Some(false)),
            (Some(json!(true)), Some(true)),
            (None, None),
            // An explicit null decodes as absent, as the contract says.
            (Some(Value::Null), None),
        ] {
            let plexus = Plexus::answering(describe(), Ok(answer(declared.clone())));
            assert_eq!(read(&plexus, "react"), Ok(expected), "{declared:?}");
            let calls = plexus.calls();
            let names: Vec<&str> = calls.iter().map(|(name, ..)| name.as_str()).collect();
            assert_eq!(names, ["role.describe", "tool.catalog"]);
            // This describe does not advertise
            // tool-catalog-capability-filter/v1, so the request's params
            // are empty.
            assert_eq!(calls[1].1, json!({"params":{}}));
            assert_ne!(calls[0].2, calls[1].2, "each call has its own key");
        }
    }

    #[test]
    fn a_management_op_or_an_unlisted_tool_declares_nothing() {
        let plexus = Plexus::answering(describe(), Ok(answer(Some(json!(false)))));
        assert_eq!(read(&plexus, "label"), Ok(None));
        assert!(plexus.calls().is_empty(), "management ops are not fetched");
        let other = Plexus::answering(
            describe(),
            Ok(json!({"generation":"g","catalog_digest":"d","tools":[]})),
        );
        assert_eq!(read(&other, "react"), Ok(None));
    }

    #[test]
    fn a_malformed_or_unreadable_answer_is_an_error_not_a_declaration() {
        let malformed = Plexus::answering(describe(), Ok(answer(Some(json!("false")))));
        assert!(
            matches!(read(&malformed, "react"), Err(WireError::Unreadable(e)) if e.contains("tool.catalog")),
            "a non-boolean emits_events does not decode"
        );
        let timed_out = Plexus::answering(describe(), Err(WireError::TimedOut("no reply".into())));
        assert_eq!(
            read(&timed_out, "react"),
            Err(WireError::TimedOut("no reply".into()))
        );
        let bad_describe = Plexus::answering(json!({"majors":[]}), Ok(answer(Some(json!(false)))));
        assert!(matches!(
            read(&bad_describe, "react"),
            Err(WireError::Unreadable(e)) if e.contains("role.describe")
        ));
        assert_eq!(
            bad_describe.calls().len(),
            1,
            "no catalog fetch after a bad describe"
        );
    }

    #[test]
    fn a_capability_filtering_provider_is_asked_for_every_tag_it_serves() {
        let mut filtering = describe();
        filtering["capabilities"] =
            json!([cortexkit_role_tool_provider::CATALOG_CAPABILITY_FILTER]);
        filtering["tool_capabilities"] = json!(["plexus:github.react/v1", "plexus:github.read/v1"]);
        let plexus = Plexus::answering(filtering, Ok(answer(Some(json!(false)))));
        assert_eq!(read(&plexus, "react"), Ok(Some(false)));
        assert_eq!(
            plexus.calls()[1].1,
            json!({"params":{"capabilities":["plexus:github.react/v1","plexus:github.read/v1"]}})
        );
    }
}
