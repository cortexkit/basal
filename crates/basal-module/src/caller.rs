//! Who is calling an op, decided from what the daemon stamped on the route
//! and never from the request's arguments.
//!
//! A route's principal is stamped by the daemon when the route is bound:
//! `reserved:<module>` for a daemon-spawned module that proved its launch
//! nonce, `direct` for any process that holds the daemon's connection file
//! and has no consumer identity. The operator is the attested
//! `reserved:callosum` (the module that relays the operator's own actions),
//! never `direct`: every local process can be `direct`, so treating it as the
//! operator would hand any of them basal's operator powers. prefrontal-core
//! draws the same line (only a bind attested as callosum is the operator).
//! An unscoped `direct` caller is a [`Caller::Local`]: it may ask to install
//! a flow, since the install card is what authorizes the install, and is
//! refused everything else the operator alone may do.
//!
//! An agent is identified by the scope its route was admitted under:
//! prefrontal-core owns every agent's session scope and sets the scope's
//! `agent_id`, and the daemon stamps the scope (with whether its owner is on
//! the daemon's scope-authority list) on the route. The stamp reaches basal
//! in `on_bind` as `RouteBindRequest::scope` (subc-client-rs 0.24 and
//! later). Nothing the caller writes in a request body can change any of
//! this.
//!
//! Of the `ScopeStamp`, basal reads three fields:
//!
//! - `owner`: the scope must be core's (`reserved:prefrontal-core`), the
//!   module that owns agents' session scopes. Anyone else's scope is their
//!   own business and names no agent here.
//! - `owner_authorized`: the daemon's word that the owner is on its
//!   scope-authority list, so the attributes it set carry authority.
//! - `attributes.agent_id`: the agent itself.
//!
//! The rest is not identity. `scope_ref` (`ref` on the wire) is an opaque
//! id core mints for the scope (not the agent's session), `scope_epoch`,
//! `kind`, `parent` and `parent_state` describe the scope's lifetime and
//! lineage, and `attributes.delegates` lets a provider act as the agent,
//! which basal never does. The agent's session is the route's bind identity
//! session, accepted only on a route admitted under that agent's vouched
//! scope.

use subc_protocol::Principal;
use subc_protocol::scope::ScopeStamp;

/// The executive. It owns agents' session scopes and reads `flow.health`
/// on its own cadence to decide whether a flow's claim holds.
pub const CORE_MODULE: &str = "prefrontal-core";

/// The module the daemon attests for the operator's own actions. Only a
/// route it stamps `reserved:callosum` is the operator.
pub const OPERATOR_MODULE: &str = "callosum";

/// Who called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The operator: the daemon-attested `reserved:callosum`, on a route
    /// with no scope.
    Operator,
    /// A local process the daemon cannot vouch for: a `direct` key-holder on
    /// a route with no scope. It may install a flow (the install card is
    /// what authorizes it) and nothing else the operator may do.
    Local,
    /// Core itself, on its own route (not under an agent's scope).
    Core,
    /// An agent, by the `agent_id` its session scope carries.
    Agent { agent_id: String, session: String },
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
            Self::Local => "local".to_owned(),
            Self::Core => format!("reserved:{CORE_MODULE}"),
            Self::Agent { agent_id, .. } => format!("agent:{agent_id}"),
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
/// `direct` (an agent's harness is a direct key-holder too). Without a
/// scope, only the attested `reserved:callosum` is the operator; a `direct`
/// route is a local caller.
pub fn from_route(
    principal: Option<&Principal>,
    scope: Option<&ScopeStamp>,
    session: &str,
) -> Caller {
    if let Some(scope) = scope {
        let core_owned = matches!(
            &scope.owner,
            Principal::Reserved { module_id } if module_id == CORE_MODULE
        );
        return match (
            &scope.attributes.agent_id,
            core_owned && scope.owner_authorized,
        ) {
            (Some(agent), true) if !agent.is_empty() => Caller::Agent {
                agent_id: agent.clone(),
                session: session.to_owned(),
            },
            _ => Caller::Other(format!(
                "a scope of {} without a vouched agent",
                principal_label(Some(&scope.owner))
            )),
        };
    }
    match principal {
        Some(Principal::Reserved { module_id }) if module_id == OPERATOR_MODULE => Caller::Operator,
        Some(Principal::Direct) => Caller::Local,
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
        assert_eq!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: OPERATOR_MODULE.into()
                }),
                None,
                "ses-author"
            ),
            Caller::Operator
        );
        // Any local process can hold the connection file and be `direct`.
        assert_eq!(
            from_route(Some(&Principal::Direct), None, "ses-author"),
            Caller::Local
        );
        assert_eq!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: CORE_MODULE.into()
                }),
                None,
                "ses-author"
            ),
            Caller::Core
        );
        assert_eq!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope(CORE_MODULE, Some("SYNAPSE"), true)),
                "ses-author"
            ),
            Caller::Agent {
                agent_id: "SYNAPSE".into(),
                session: "ses-author".into()
            }
        );
        // An agent id nobody vouched for is not an identity.
        assert!(matches!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope(CORE_MODULE, Some("SYNAPSE"), false)),
                "ses-author"
            ),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(
                Some(&Principal::Direct),
                Some(&scope("aft", Some("SYNAPSE"), true)),
                "ses-author"
            ),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(Some(&Principal::Unverified), None, "ses-author"),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(None, None, "ses-author"),
            Caller::Other(_)
        ));
        assert!(matches!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: "aft".into()
                }),
                None,
                "ses-author"
            ),
            Caller::Other(_)
        ));
    }
}
