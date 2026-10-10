//! Core supplies selectors; basal only carries them, never creates authority.
use crate::flow_refusal::{FlowRefusal, RefusalReason};
use crate::transport::WireError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::collections::HashMap;
use subc_protocol::{BindIdentity, Principal, scope::ScopeSelector};

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
    let owner = serde_json::Value::deserialize(deserializer)?;
    // Use the protocol's strict scope-principal decoder, not Principal's
    // forward-compatible attribution decoder, for authority-bearing input.
    let selector: ScopeSelector = serde_json::from_value(
        serde_json::json!({"owner":owner,"ref":"owner-decoder","scope_epoch":0}),
    )
    .map_err(serde::de::Error::custom)?;
    Ok(selector.owner)
}

pub struct ScopedRoutes<H> {
    flows: HashMap<String, (RegisteredScope, ScopeKey)>,
    scope_users: HashMap<ScopeKey, usize>,
    routes: HashMap<RouteKey, (FlowScope, Option<H>, String)>,
    connection_generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RouteKey {
    scope: ScopeKey,
    module: String,
    tool: bool,
    identity: Option<(std::path::PathBuf, String, String, Option<String>)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScopeKey {
    owner: (u8, String),
    reference: String,
    epoch: u64,
}
impl From<&FlowScope> for ScopeKey {
    fn from(scope: &FlowScope) -> Self {
        Self {
            owner: match &scope.owner {
                Principal::Reserved { module_id } => (0, module_id.clone()),
                Principal::Direct => (1, String::new()),
                Principal::Unverified => (2, String::new()),
            },
            reference: scope.scope_ref.clone(),
            epoch: scope.epoch,
        }
    }
}
pub(crate) struct RoutePlan<H> {
    pub key: RouteKey,
    pub scope: FlowScope,
    pub handle: Option<H>,
    generation: u64,
}
impl<H> Default for ScopedRoutes<H> {
    fn default() -> Self {
        Self {
            flows: HashMap::new(),
            scope_users: HashMap::new(),
            routes: HashMap::new(),
            connection_generation: 0,
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
        self.connection_generation = self.connection_generation.wrapping_add(1);
    }
    pub fn configure(
        &mut self,
        flow: &str,
        _agent_owned: bool,
        scope: Option<RegisteredScope>,
    ) -> Vec<H> {
        if let Some((_, key)) = self.flows.remove(flow) {
            let users = self
                .scope_users
                .get_mut(&key)
                .expect("registered scope count");
            *users -= 1;
        }
        if let Some(scope) = scope {
            let key = ScopeKey::from(&scope.selector);
            *self.scope_users.entry(key.clone()).or_default() += 1;
            self.flows.insert(flow.into(), (scope, key));
        }
        let mut dropped = Vec::new();
        self.routes.retain(|key, (_, handle, _)| {
            let live = self.scope_users.get(&key.scope).is_some_and(|n| *n > 0);
            if !live && let Some(handle) = handle {
                dropped.push(*handle);
            }
            live
        });
        self.scope_users.retain(|_, users| *users > 0);
        dropped
    }

    /// The caller serializes opening per route identity. When the daemon
    /// closes a route because its scope ended, the route is kept as a
    /// tombstone that refuses every use with `scope_ended` instead of
    /// reopening under the same selector. It stays until prefrontal-core
    /// re-registers the flow's scope with a different selector (a new epoch,
    /// for example), which `configure` records.
    pub fn route(
        &mut self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        open: impl FnOnce(&FlowScope) -> Result<H, WireError>,
    ) -> Result<Option<H>, WireError> {
        self.route_with_identity(flow, target, None, open)
    }

    /// Broca keeps conversation history under the bind identity. Only reads
    /// and subscriptions belonging to that same model call may reuse its route.
    pub fn route_for_identity(
        &mut self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        identity: &BindIdentity,
        open: impl FnOnce(&FlowScope) -> Result<H, WireError>,
    ) -> Result<Option<H>, WireError> {
        self.route_with_identity(flow, target, Some(identity), open)
    }

    fn route_with_identity(
        &mut self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        identity: Option<&BindIdentity>,
        open: impl FnOnce(&FlowScope) -> Result<H, WireError>,
    ) -> Result<Option<H>, WireError> {
        let plan = self.prepare(flow, target, identity)?;
        if let Some(handle) = plan.handle {
            return Ok(Some(handle));
        }
        let handle = open(&plan.scope)?;
        self.install(flow, plan, handle).map(Some)
    }

    pub(crate) fn prepare(
        &self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        identity: Option<&BindIdentity>,
    ) -> Result<RoutePlan<H>, WireError> {
        let (module, tool) = match target {
            subc_protocol::RouteTarget::ManagementSurface { module_id } => {
                (module_id.as_str(), false)
            }
            subc_protocol::RouteTarget::ToolProvider { module_id } => (module_id.as_str(), true),
            _ => return Err(WireError::NeverSent("unsupported flow target".into())),
        };
        self.ready(flow, module, "route.open")
            .map_err(WireError::Typed)?;
        let (registered, scope_key) = self.flows.get(flow).unwrap();
        let scope = registered.selector.clone();
        let key = RouteKey {
            scope: scope_key.clone(),
            module: module.into(),
            tool,
            identity: identity.map(|id| {
                (
                    id.project_root.clone(),
                    id.harness.clone(),
                    id.session.clone(),
                    id.project_id.clone(),
                )
            }),
        };
        if let Some((_, handle, _)) = self.routes.get(&key) {
            return handle
                .map(|handle| RoutePlan {
                    key,
                    scope,
                    handle: Some(handle),
                    generation: self.connection_generation,
                })
                .ok_or_else(|| {
                    WireError::Typed(FlowRefusal::new(
                        RefusalReason::ScopeEnded,
                        module,
                        "route.open",
                    ))
                });
        }
        Ok(RoutePlan {
            key,
            scope,
            handle: None,
            generation: self.connection_generation,
        })
    }

    pub(crate) fn install(
        &mut self,
        flow: &str,
        plan: RoutePlan<H>,
        handle: H,
    ) -> Result<H, WireError> {
        // The route is opened outside the table lock, so two things may have
        // changed while the daemon was answering the open: the flow's scope
        // registration (prefrontal-core re-registered or removed it, and
        // `configure` recorded that) and the connection generation (the
        // daemon connection dropped and was restored, so handles from the old
        // connection are dead). Install the handle only if both still match
        // what the open was planned against.
        self.ready(flow, &plan.key.module, "route.open")
            .map_err(WireError::Typed)?;
        if self.connection_generation != plan.generation
            || self
                .flows
                .get(flow)
                .is_none_or(|(_, key)| key != &plan.key.scope)
        {
            return Err(WireError::NeverSent(
                "scope or connection changed during route open".into(),
            ));
        }
        if let Some((_, existing, _)) = self.routes.get(&plan.key) {
            return existing.ok_or_else(|| {
                WireError::Typed(FlowRefusal::new(
                    RefusalReason::ScopeEnded,
                    &plan.key.module,
                    "route.open",
                ))
            });
        }
        let module = plan.key.module.clone();
        self.routes
            .insert(plan.key, (plan.scope, Some(handle), module));
        Ok(handle)
    }

    /// A settled model call no longer needs a channel or a subscription.
    pub fn release_identity(
        &mut self,
        flow: &str,
        target: &subc_protocol::RouteTarget,
        identity: &BindIdentity,
    ) -> Option<H> {
        let key = self.prepare(flow, target, Some(identity)).ok()?.key;
        self.routes.remove(&key).and_then(|(_, handle, _)| handle)
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

    pub fn ready(&self, flow: &str, module: &str, action: &str) -> Result<(), FlowRefusal> {
        let Some((scope, _)) = self.flows.get(flow) else {
            return Err(FlowRefusal::new(RefusalReason::NoFlowScope, module, action));
        };
        if !scope.targets.contains(module) {
            return Err(FlowRefusal::new(
                RefusalReason::TargetFlowUnsupported,
                module,
                action,
            ));
        }
        Ok(())
    }

    pub fn ended(&self, flow: &str) -> bool {
        self.flows.get(flow).is_some_and(|(_, scope)| {
            self.routes
                .iter()
                .any(|(key, (_, handle, _))| &key.scope == scope && handle.is_none())
        })
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
    if !matches!(selector.owner, Principal::Reserved { .. }) || selector.scope_ref.trim().is_empty()
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
    fn registered(selector: FlowScope) -> RegisteredScope {
        RegisteredScope {
            selector,
            targets: ["broca".into()].into_iter().collect(),
        }
    }
    #[test]
    fn install_scope_fields_are_absent_or_a_complete_registered_selector() {
        assert_eq!(decode(&json!({})).unwrap(), None);
        let decoded = decode(&fixture()).unwrap().unwrap();
        assert_eq!(decoded.selector.epoch, 7);
        assert_eq!(decoded.selector.scope_ref, "flow:a");
        let mut zero = fixture();
        zero["scope"]["epoch"] = json!(0);
        assert_eq!(decode(&zero).unwrap().unwrap().selector.epoch, 0);
        let mut unknown_owner = fixture();
        unknown_owner["scope"]["owner"]["unknown_constraint"] = json!(true);
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
        routes.configure("a", true, Some(registered(scope.clone())));
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        assert_eq!(routes.route("a", &target, |_| Ok(1)).unwrap(), Some(1));
        routes.closed(1, true);
        assert!(matches!(
            routes.route("a", &target, |_| panic!("scope close reopened")),
            Err(WireError::Typed(_))
        ));
        routes.configure("a", true, Some(registered(scope.clone())));
        assert!(
            routes
                .route("a", &target, |_| panic!(
                    "same selector reopened after install status"
                ))
                .is_err()
        );
        let mut next = scope;
        next.epoch += 1;
        routes.configure("a", true, Some(registered(next)));
        assert_eq!(routes.route("a", &target, |_| Ok(2)).unwrap(), Some(2));
    }
    #[test]
    fn epoch_change_drops_the_old_handle_and_reuses_the_new_one() {
        let mut routes = ScopedRoutes::<u64>::default();
        let mut scope = decode(&fixture()).unwrap().unwrap().selector;
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        routes.configure("a", true, Some(registered(scope.clone())));
        routes.route("a", &target, |_| Ok(1)).unwrap();
        assert_eq!(
            routes
                .route("a", &target, |_| panic!("opened twice"))
                .unwrap(),
            Some(1)
        );
        scope.epoch += 1;
        assert_eq!(
            routes.configure("a", true, Some(registered(scope))),
            vec![1]
        );
        assert_eq!(routes.route("a", &target, |_| Ok(2)).unwrap(), Some(2));
    }

    #[test]
    fn scope_bookkeeping_forgets_removed_flows_but_preserves_shared_authority() {
        let mut routes = ScopedRoutes::<u64>::default();
        let scope = decode(&fixture()).unwrap().unwrap();
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        routes.configure("a", true, Some(scope.clone()));
        routes.configure("b", true, Some(scope.clone()));
        routes.route("a", &target, |_| Ok(1)).unwrap();
        assert!(routes.configure("a", true, None).is_empty());
        assert_eq!(
            routes
                .route("b", &target, |_| panic!("shared handle dropped"))
                .unwrap(),
            Some(1)
        );
        assert_eq!(routes.configure("b", true, None), [1]);
        assert!(routes.flows.is_empty());
        assert!(routes.scope_users.is_empty());
        assert!(routes.routes.is_empty());
    }

    #[test]
    fn an_open_reply_cannot_install_after_scope_or_connection_changes() {
        let mut routes = ScopedRoutes::<u64>::default();
        let scope = decode(&fixture()).unwrap().unwrap();
        let target = subc_protocol::RouteTarget::ManagementSurface {
            module_id: "broca".into(),
        };
        routes.configure("a", true, Some(scope.clone()));
        let plan = routes.prepare("a", &target, None).unwrap();
        routes.connection_restored();
        assert!(matches!(
            routes.install("a", plan, 1),
            Err(WireError::NeverSent(_))
        ));
        let plan = routes.prepare("a", &target, None).unwrap();
        let mut next = scope;
        next.selector.epoch += 1;
        routes.configure("a", true, Some(next));
        assert!(matches!(
            routes.install("a", plan, 2),
            Err(WireError::NeverSent(_))
        ));
        assert_eq!(routes.route("a", &target, |_| Ok(3)).unwrap(), Some(3));
    }
}
