//! Who is calling an op, decided from what the daemon stamped on the route
//! and never from the request's arguments.
//!
//! A route's principal is stamped by the daemon when the route is bound:
//! `direct` for a key-holder with no consumer identity (the operator's `ck`
//! faces), `reserved:<module>` for a daemon-spawned module that proved its
//! launch nonce. An agent is identified by the scope its route was admitted
//! under: prefrontal-core owns every agent's session scope and sets the
//! scope's `agent_id`, and the daemon stamps the scope (with whether its
//! owner is on the daemon's scope-authority list) on the route. Nothing the
//! caller writes in a request body can change any of this.

use subc_protocol::Principal;
use subc_protocol::scope::ScopeStamp;

/// The executive. It owns agents' session scopes and reads `flow.health`
/// on its own cadence to decide whether a flow's claim holds.
pub const CORE_MODULE: &str = "prefrontal-core";

/// Who called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The operator: a direct key-holder on a route with no scope.
    Operator,
    /// Core itself, on its own route (not under an agent's scope).
    Core,
    /// An agent, by the `agent_id` its session scope carries.
    Agent(String),
    /// Anyone else: another module, an unverified route, or a route whose
    /// principal the daemon did not record. Refused by every op that names
    /// a caller.
    Other(String),
}

impl Caller {
    /// A short label for logs and refusals.
    pub fn label(&self) -> String {
        match self {
            Self::Operator => "operator".to_owned(),
            Self::Core => format!("reserved:{CORE_MODULE}"),
            Self::Agent(a) => format!("agent:{a}"),
            Self::Other(o) => o.clone(),
        }
    }
}

fn principal_label(principal: Option<&Principal>) -> String {
    match principal {
        Some(Principal::Direct) => "direct".to_owned(),
        Some(Principal::Reserved { module_id }) => format!("reserved:{module_id}"),
        Some(Principal::Unverified) => "unverified".to_owned(),
        None => "an unrecorded route".to_owned(),
    }
}

/// The caller of a route, from its stamped principal and scope.
///
/// A scope names an agent only when its owner is core and the daemon says
/// that owner may set authority-bearing attributes (`owner_authorized`);
/// otherwise the `agent_id` is a claim nobody vouched for and is ignored. A
/// route under an agent's scope is that agent's even when its principal is
/// `direct` (an agent's harness is a direct key-holder too); only an
/// unscoped direct route is the operator.
pub fn from_route(principal: Option<&Principal>, scope: Option<&ScopeStamp>) -> Caller {
    if let Some(scope) = scope {
        let core_owned = matches!(
            &scope.owner,
            Principal::Reserved { module_id } if module_id == CORE_MODULE
        );
        return match (
            &scope.attributes.agent_id,
            core_owned && scope.owner_authorized,
        ) {
            (Some(agent), true) if !agent.is_empty() => Caller::Agent(agent.clone()),
            _ => Caller::Other(format!(
                "a scope of {} without a vouched agent",
                principal_label(Some(&scope.owner))
            )),
        };
    }
    match principal {
        Some(Principal::Direct) => Caller::Operator,
        Some(Principal::Reserved { module_id }) if module_id == CORE_MODULE => Caller::Core,
        other => Caller::Other(principal_label(other)),
    }
}

#[cfg(test)]
mod tests {
    use subc_protocol::scope::{ScopeAttributes, ScopeKind};

    use super::*;

    fn scope(owner: &str, agent: Option<&str>, authorized: bool) -> ScopeStamp {
        ScopeStamp {
            owner: Principal::Reserved {
                module_id: owner.to_owned(),
            },
            scope_ref: "s-1".into(),
            scope_epoch: 1,
            kind: ScopeKind::Head,
            parent: None,
            parent_state: None,
            attributes: ScopeAttributes {
                agent_id: agent.map(str::to_owned),
                delegates: false,
            },
            owner_authorized: authorized,
        }
    }

    #[test]
    fn identity_comes_only_from_the_stamp() {
        assert_eq!(from_route(Some(&Principal::Direct), None), Caller::Operator);
        assert_eq!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: CORE_MODULE.into()
                }),
                None
            ),
            Caller::Core
        );
        assert_eq!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope(CORE_MODULE, Some("SYNAPSE"), true))
            ),
            Caller::Agent("SYNAPSE".into())
        );
        // An agent id nobody vouched for is not an identity.
        assert!(matches!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope(CORE_MODULE, Some("SYNAPSE"), false))
            ),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope("aft", Some("SYNAPSE"), true))
            ),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(Some(&Principal::Unverified), None),
            Caller::Other(_)
        ));
        assert!(matches!(from_route(None, None), Caller::Other(_)));
        assert!(matches!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: "aft".into()
                }),
                None
            ),
            Caller::Other(_)
        ));
    }
}
