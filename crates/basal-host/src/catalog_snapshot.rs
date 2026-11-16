//! Immutable daemon declarations for one request. Acquisition is separate from
//! lookup so a future source observing daemon catalog changes can supply the
//! same snapshot handle without changing callers.
use std::collections::BTreeMap;
use std::sync::Arc;
use subc_client_rs::consumer::CatalogList;
use subc_protocol::manifest::{EventDeclaration, ProviderRole};

#[derive(Debug)]
pub struct Module {
    pub roles: Vec<ProviderRole>,
    pub provides: Vec<String>,
    pub events: Option<Vec<EventDeclaration>>,
    pub flow_operations: Option<Vec<String>>,
    tools: BTreeMap<String, Option<(usize, usize)>>,
}

impl Module {
    pub fn provides(&self, capability: &str) -> bool {
        self.provides.iter().any(|c| c == capability)
    }
    pub fn tool_schema(
        &self,
        name: &str,
    ) -> Result<Option<&serde_json::Value>, crate::transport::WireError> {
        match self.tools.get(name) {
            None => Ok(None),
            Some(None) => Err(crate::transport::WireError::Unreadable(
                "ambiguous tool declaration".into(),
            )),
            Some(Some((role, tool))) => match &self.roles[*role] {
                ProviderRole::ToolProvider { tools, .. } => Ok(Some(&tools[*tool].schema)),
                _ => unreachable!("tool index points to a tool provider"),
            },
        }
    }
}

#[derive(Debug)]
pub struct CatalogSnapshot {
    pub generation: u64,
    modules: BTreeMap<String, Module>,
}

impl CatalogSnapshot {
    /// Adapter for Value-based test transports. Production starts with the
    /// daemon client's typed list and never serializes it back to JSON.
    pub fn from_value(
        mut value: serde_json::Value,
    ) -> Result<Arc<Self>, crate::transport::WireError> {
        use crate::transport::WireError;
        value
            .as_object_mut()
            .ok_or_else(|| WireError::Unreadable("catalog is not an object".into()))?
            .entry("generation")
            .or_insert(serde_json::json!(0));
        let modules = value["modules"]
            .as_array_mut()
            .ok_or_else(|| WireError::Unreadable("catalog modules missing".into()))?;
        for module in modules {
            if let Some(module) = module.as_object_mut() {
                module.entry("control_ops").or_insert(serde_json::json!([]));
            }
        }
        serde_json::from_value(value)
            .map(Self::new)
            .map_err(|e| WireError::Unreadable(format!("catalog: {e}")))
    }
    pub fn new(catalog: CatalogList) -> Arc<Self> {
        Arc::new(Self {
            generation: catalog.generation,
            modules: catalog
                .modules
                .into_iter()
                .map(|m| {
                    let mut tools = BTreeMap::new();
                    for (r, role) in m.roles.iter().enumerate() {
                        if let ProviderRole::ToolProvider {
                            tools: declared, ..
                        } = role
                        {
                            for (t, tool) in declared.iter().enumerate() {
                                tools
                                    .entry(tool.name.clone())
                                    .and_modify(|position| *position = None)
                                    .or_insert(Some((r, t)));
                            }
                        }
                    }
                    (
                        m.module_id,
                        Module {
                            roles: m.roles,
                            provides: m.capabilities.map(|c| c.provides).unwrap_or_default(),
                            events: m.events,
                            flow_operations: m.flow_operations,
                            tools,
                        },
                    )
                })
                .collect(),
        })
    }

    pub fn module(&self, id: &str) -> Option<&Module> {
        self.modules.get(id)
    }
}
