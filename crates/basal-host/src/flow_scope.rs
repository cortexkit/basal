//! Core supplies selectors; basal only carries them, never creates authority.
use crate::flow_refusal::{FlowRefusal, RefusalReason};
use crate::transport::WireError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::collections::HashMap;
use subc_protocol::{Principal, scope::ScopeSelector};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowScope {
    #[serde(deserialize_with = "decode_owner")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
}

fn decode_owner<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Principal, D::Error> {
    let owner=serde_json::Value::deserialize(deserializer)?;
    // Use the protocol's strict scope-principal decoder, not Principal's
    // forward-compatible attribution decoder, for authority-bearing input.
    let selector:ScopeSelector=serde_json::from_value(serde_json::json!({"owner":owner,"ref":"owner-decoder","scope_epoch":0})).map_err(serde::de::Error::custom)?;
    Ok(selector.owner)
}

pub struct ScopedRoutes<H> {
    flows: HashMap<String, (bool, Option<FlowScope>)>,
    routes: HashMap<String, (FlowScope, Option<H>, String)>,
}
impl<H> Default for ScopedRoutes<H> {
    fn default() -> Self {
        Self {
            flows: HashMap::new(),
            routes: HashMap::new(),
        }
    }
}

impl<H: Copy + Eq> ScopedRoutes<H> {
    /// Daemon control pushes name a channel list, or a module on older
    /// daemons. They do not carry a route epoch or a single route_channel.
    pub fn control_push(
        &mut self,
        push: &subc_client_rs::consumer::ControlPush,
        channel: impl Fn(H) -> u16,
    ) {
        if push.op != "route.closed" {
            return;
        }
        let scope_close = push
            .body
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| reason.starts_with("scope_"));
        let channels = push
            .body
            .get("channels")
            .and_then(serde_json::Value::as_array);
        let module = push
            .body
            .get("module_id")
            .and_then(serde_json::Value::as_str);
        let handles: Vec<_> = self
            .routes
            .values()
            .filter_map(|(_, handle, target)| {
                let handle = (*handle)?;
                let matches = match channels {
                    Some(channels) => channels
                        .iter()
                        .any(|c| c.as_u64() == Some(u64::from(channel(handle)))),
                    None if push.body.get("channels").is_none() => module == Some(target.as_str()),
                    None => false,
                };
                matches.then_some(handle)
            })
            .collect();
        for handle in handles {
            self.closed(handle, scope_close);
        }
    }
    pub fn connection_restored(&mut self) {
        // Closed-scope tombstones survive a connection restart. Live handles
        // do not: their next use opens a scoped route on the new connection.
        self.routes.retain(|_, (_, handle, _)| handle.is_none());
    }
    pub fn configure(&mut self, flow: &str, agent_owned: bool, scope: Option<FlowScope>) -> Vec<H> {
        self.flows.insert(flow.into(), (agent_owned, scope));
        let mut dropped = Vec::new();
        self.routes.retain(|_, (selector, handle, _)| {
            let live = self
                .flows
                .values()
                .any(|(_, s)| s.as_ref() == Some(selector));
            if !live && let Some(handle) = handle {
                dropped.push(*handle);
            }
            live
        });
        dropped
    }

    /// Opening and lookup share a lock in the caller, so concurrent runs cannot
    /// mint two routes. A closed scope keeps a tombstone until core changes it.
    pub fn route(
        &mut self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        open: impl FnOnce(&FlowScope) -> Result<H, WireError>,
    ) -> Result<Option<H>, WireError> {
        let module = match target {
            subc_protocol::RouteTarget::ManagementSurface { module_id }
            | subc_protocol::RouteTarget::ToolProvider { module_id } => module_id.as_str(),
            _ => return Err(WireError::NeverSent("unsupported flow target".into())),
        };
        let (owned, scope) = self.flows.get(flow).cloned().unwrap_or((true, None));
        let Some(scope) = scope else {
            if owned {
                return Err(WireError::Typed(FlowRefusal::new(
                    RefusalReason::NoFlowScope,
                    module,
                    "route.open",
                )));
            }
            if module == crate::subc_catalog::CORE {
                return Err(WireError::Refused {
                    code: "basal_scope_bug".into(),
                    message: "a flow core op has no scoped route".into(),
                });
            }
            return Ok(None);
        };
        let key = format!(
            "{}:{}",
            serde_json::to_string(&scope).unwrap(),
            serde_json::to_string(target).unwrap()
        );
        if let Some((_, handle, _)) = self.routes.get(&key) {
            return handle.map(Some).ok_or_else(|| {
                WireError::Typed(FlowRefusal::new(
                    RefusalReason::ScopeEnded,
                    module,
                    "route.open",
                ))
            });
        }
        let handle = open(&scope)?;
        self.routes
            .insert(key, (scope, Some(handle), module.into()));
        Ok(Some(handle))
    }

    pub fn closed(&mut self, handle: H, scope_close: bool) {
        self.routes.retain(|_, (_, current, _)| {
            if *current != Some(handle) {
                return true;
            }
            if scope_close {
                *current = None;
                true
            } else {
                false
            }
        });
    }
}

impl FlowScope {
    pub fn selector(&self) -> ScopeSelector {
        ScopeSelector {
            owner: self.owner.clone(),
            scope_ref: self.scope_ref.clone(),
            scope_epoch: Some(self.epoch),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredScope {
    pub selector: FlowScope,
    pub targets: BTreeSet<String>,
}

/// The fields are absent together before registration. Null is not absence:
/// accepting it would conceal a broken authority-bearing reply from core.
pub fn decode(value: &serde_json::Value) -> Result<Option<RegisteredScope>, String> {
    let (scope, targets) = match (value.get("scope"), value.get("flow_scope_targets")) {
        (None, None) => return Ok(None),
        (Some(scope), Some(targets)) => (scope, targets),
        _ => return Err("scope and flow_scope_targets must be present together".into()),
    };
    let selector: FlowScope = serde_json::from_value(scope.clone()).map_err(|e| e.to_string())?;
    if !matches!(selector.owner, Principal::Reserved { .. })
        || selector.scope_ref.trim().is_empty()
    {
        return Err("invalid flow scope selector".into());
    }
    let modules: Vec<String> =
        serde_json::from_value(targets.clone()).map_err(|e| e.to_string())?;
    if modules.iter().any(|m| m.trim().is_empty())
        || modules.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("flow_scope_targets must be sorted unique module ids".into());
    }
    Ok(Some(RegisteredScope {
        selector,
        targets: modules.into_iter().collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> serde_json::Value {
        json!({"scope":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"flow:a","epoch":7},"flow_scope_targets":["broca","cerebellum"]})
    }
    #[test]
    fn install_scope_fields_are_absent_or_a_complete_registered_selector() {
        assert_eq!(decode(&json!({})).unwrap(), None);
        let decoded = decode(&fixture()).unwrap().unwrap();
        assert_eq!(decoded.selector.epoch, 7);
        assert_eq!(decoded.selector.scope_ref, "flow:a");
        let mut zero=fixture();zero["scope"]["epoch"]=json!(0);
        assert_eq!(decode(&zero).unwrap().unwrap().selector.epoch,0);
        let mut unknown_owner=fixture();unknown_owner["scope"]["owner"]["unknown_constraint"]=json!(true);
        assert!(decode(&unknown_owner).is_err());
        assert_eq!(
            decoded.targets,
            ["broca".into(), "cerebellum".into()].into_iter().collect()
        );
        for bad in [
            json!({"scope":null}),
            json!({"scope":null,"flow_scope_targets":null}),
            json!({"scope":fixture()["scope"],"flow_scope_targets":null}),
            json!({"scope":fixture()["scope"],"flow_scope_targets":["cerebellum","broca"]}),
            json!({"scope":fixture()["scope"],"flow_scope_targets":["broca","broca"]}),
        ] {
            assert!(decode(&bad).is_err(), "{bad}");
        }
        for field in ["owner", "ref", "epoch"] {
            let mut bad = fixture();
            bad["scope"].as_object_mut().unwrap().remove(field);
            assert!(decode(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn a_scope_closed_route_is_not_reopened_under_the_same_selector() {
        let mut routes = ScopedRoutes::<u64>::default();
        let scope = decode(&fixture()).unwrap().unwrap().selector;
        routes.configure("a", true, Some(scope.clone()));
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        assert_eq!(routes.route("a", &target, |_| Ok(1)).unwrap(), Some(1));
        routes.closed(1, true);
        assert!(matches!(
            routes.route("a", &target, |_| panic!("scope close reopened")),
            Err(WireError::Typed(_))
        ));
        routes.configure("a", true, Some(scope.clone()));
        assert!(
            routes
                .route("a", &target, |_| panic!(
                    "same selector reopened after install status"
                ))
                .is_err()
        );
        let mut next = scope;
        next.epoch += 1;
        routes.configure("a", true, Some(next));
        assert_eq!(routes.route("a", &target, |_| Ok(2)).unwrap(), Some(2));
    }
    #[test]
    fn epoch_change_drops_the_old_handle_and_reuses_the_new_one() {
        let mut routes = ScopedRoutes::<u64>::default();
        let mut scope = decode(&fixture()).unwrap().unwrap().selector;
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        routes.configure("a", true, Some(scope.clone()));
        routes.route("a", &target, |_| Ok(1)).unwrap();
        assert_eq!(
            routes
                .route("a", &target, |_| panic!("opened twice"))
                .unwrap(),
            Some(1)
        );
        scope.epoch += 1;
        assert_eq!(routes.configure("a", true, Some(scope)), vec![1]);
        assert_eq!(routes.route("a", &target, |_| Ok(2)).unwrap(), Some(2));
    }
}
