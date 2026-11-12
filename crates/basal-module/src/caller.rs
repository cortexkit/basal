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
//! Of the `ScopeStamp`, basal reads four fields:
//!
//! - `owner`: the scope must be core's (`reserved:prefrontal-core`), the
//!   module that owns agents' session scopes. Anyone else's scope is their
//!   own business and names no agent here.
//! - `owner_authorized`: the daemon's word that the owner is on its
//!   scope-authority list, so the attributes it set carry authority.
//! - `attributes.agent_id`: the agent itself.
//! - `scope_ref` (`ref` on the wire): the id core minted for the scope. An
//!   agent's install card names its author by this ref alone, and core looks
//!   the scope up in its own records to find the agent and its session, so
//!   core relies on nothing basal says about either.
//!
//! The rest is not identity: `scope_epoch`, `kind`, `parent` and
//! `parent_state` describe the scope's lifetime and lineage, and
//! `attributes.delegates` lets a provider act as the agent, which basal
//! never does.
//!
//! The session in the route's bind identity is not used for anything. It is
//! what the process that opened the route declared about itself; the daemon
//! passes it on without checking it, and nothing in the stamp ties it to the
//! agent's scope, so any process admitted under the scope could name any
//! session there. Where an agent's session matters (the install card), core
//! reads it from the scope it owns.

use subc_protocol::BindIdentity;
use subc_protocol::Principal;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

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
    /// An admitted dev operator app: a `direct`, unscoped, non-federated
    /// route that is not the OpenCode plugin's shared channel (see
    /// [`dev_operator_app_admission`]). The admission matches nearly every
    /// local route, so it is a [`Caller::Local`] first: it keeps every right a
    /// local caller has, and gains only `flow.overview`, `flow.details` and
    /// `flow.uninstall`. Every other operator-only op refuses it as local.
    DevOperatorApp,
    /// Core itself, on its own route (not under an agent's scope).
    Core,
    /// Another daemon-attested module on an unscoped route.
    Module { module_id: String },
    /// An agent, by the `agent_id` its session scope carries, and that
    /// scope's `scope_ref` exactly as the daemon stamped it.
    Agent { agent_id: String, scope_ref: String },
    /// Anyone else: an unverified route, or a route whose
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
            // The desktop app's actor label, matching core's `operator:local-direct`.
            Self::DevOperatorApp => "operator:local-direct".to_owned(),
            Self::Core => format!("reserved:{CORE_MODULE}"),
            Self::Module { module_id } => format!("reserved:{module_id}"),
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
            (Some(agent), true) if !agent.is_empty() => Caller::Agent {
                agent_id: agent.clone(),
                scope_ref: scope.scope_ref.clone(),
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
        Some(Principal::Reserved { module_id }) => Caller::Module {
            module_id: module_id.clone(),
        },
        other => Caller::Other(principal_label(other)),
    }
}

/// Whether a route is an admitted dev operator app, mirroring core's
/// `dev_operator_app_admission`: a `direct` carrier with no scope stamp,
/// whose harness does not start with `fed-`, and whose bind identity is not
/// the OpenCode plugin's shared channel (`harness == "alfonso"` and
/// `session == "prefrontal-core"`). `Reserved` and `Unverified` binds are
/// never admitted: the principal check admits only `Direct`. Core's other
/// exclusion, a federated operator, is a `reserved:callosum` bind, which
/// that same check already refuses.
///
/// Release plan: replace this function's body with a check of SUBC's
/// `operator_app` stamp (subc-protocol 0.33.5, capability `operator-app/v1`).
pub fn dev_operator_app_admission(
    principal: Option<&Principal>,
    scope: Option<&ScopeStamp>,
    identity: &BindIdentity,
) -> bool {
    if !matches!(principal, Some(Principal::Direct)) {
        return false;
    }
    if scope.is_some() {
        return false;
    }
    if identity.harness.starts_with("fed-") {
        return false;
    }
    // The OpenCode plugin's shared channel: an exact harness and session
    // pair, no prefix match and no other field.
    if identity.harness == "alfonso" && identity.session == "prefrontal-core" {
        return false;
    }
    true
}

/// The caller of a route, including the admitted dev operator app.
///
/// A route admitted by [`dev_operator_app_admission`] is a
/// [`Caller::DevOperatorApp`]; every other route is decided by [`from_route`]
/// alone. Every op that accepts [`Caller::Local`] also accepts the admitted
/// caller, so admission only adds `flow.overview`, `flow.details` and
/// `flow.uninstall` and never takes a local right away.
pub fn caller_for_route(
    principal: Option<&Principal>,
    scope: Option<&ScopeStamp>,
    identity: &BindIdentity,
) -> Caller {
    if dev_operator_app_admission(principal, scope, identity) {
        return Caller::DevOperatorApp;
    }
    from_route(principal, scope)
}

/// A worker session's own scope, as codemode reads it: a session that works
/// for a head without that head's authority, such as a mason or a sidekick
/// session. Only codemode accepts one; every other op still sees such a route
/// as [`Caller::Other`], since none of them has a meaning for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerScope {
    pub scope_ref: String,
    pub scope_epoch: u64,
}

/// The worker scope a route was admitted under, if it is one: core owns it,
/// the daemon vouches that core may set its attributes, its kind is `worker`,
/// and it carries no `agent_id`, `run_id`, `flow_id` or `delegates`. Core
/// also names the head the worker works for in `on_behalf_of_agent`; that is
/// attribution only, so it is allowed and never read here.
///
/// Core uses this shape for more than masons and sidekick sessions, and basal
/// does not try to tell them apart: core decides, when basal asks it to open
/// the run, whether this scope may have one.
pub fn worker_scope(scope: &ScopeStamp) -> Option<WorkerScope> {
    let core_owned = matches!(
        &scope.owner,
        Principal::Reserved { module_id } if module_id == CORE_MODULE
    );
    let attributes = &scope.attributes;
    (core_owned
        && scope.owner_authorized
        && scope.kind == ScopeKind::Worker
        && attributes.agent_id.is_none()
        && attributes.run_id.is_none()
        && attributes.flow_id.is_none()
        && !attributes.delegates)
        .then(|| WorkerScope {
            scope_ref: scope.scope_ref.clone(),
            scope_epoch: scope.scope_epoch,
        })
}

/// Admission for the read-only diagnostics query, using only the daemon's
/// route stamp. Core owns registered flow scopes; basal is their carrier,
/// not their owner. Worker attribution never confers head authority.
pub fn diagnostics_admitted(principal: Option<&Principal>, scope: Option<&ScopeStamp>) -> bool {
    let Some(scope) = scope else {
        return matches!(principal, Some(Principal::Reserved { module_id })
            if module_id == OPERATOR_MODULE || module_id == CORE_MODULE);
    };
    if !scope.owner_authorized
        || scope.kind == ScopeKind::Worker
        || !matches!(&scope.owner, Principal::Reserved { module_id } if module_id == CORE_MODULE)
    {
        return false;
    }
    let attributes = &scope.attributes;
    if attributes
        .flow_id
        .as_ref()
        .is_some_and(|id| !id.trim().is_empty())
    {
        return true;
    }
    scope.kind == ScopeKind::Head
        && attributes
            .agent_id
            .as_ref()
            .is_some_and(|id| !id.trim().is_empty())
        && attributes.run_id.is_none()
        && attributes.flow_id.is_none()
}

#[cfg(test)]
mod tests {
    use subc_protocol::scope::ScopeAttributes;

    use super::*;

    fn scope(owner: &str, agent: Option<&str>, authorized: bool) -> ScopeStamp {
        ScopeStamp::new(
            Principal::Reserved {
                module_id: owner.to_owned(),
            },
            "s-1",
            1,
            ScopeKind::Head,
        )
        .with_attributes(ScopeAttributes::new().with_agent_id(agent.map(str::to_owned)))
        .with_owner_authorized(authorized)
    }

    #[test]
    fn diagnostics_admission_uses_only_authorized_route_stamps() {
        let reserved = |id: &str| Principal::Reserved {
            module_id: id.into(),
        };
        for id in [OPERATOR_MODULE, CORE_MODULE] {
            assert!(diagnostics_admitted(Some(&reserved(id)), None));
        }
        let head = scope(CORE_MODULE, Some("agent"), true);
        assert!(diagnostics_admitted(Some(&Principal::Direct), Some(&head)));
        for fixture in [
            include_str!("../tests/vectors/flow-scope-v1/registered-scope.json"),
            include_str!("../tests/vectors/flow-scope-v1/registered-global-scope.json"),
        ] {
            let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
            let flow = ScopeStamp::new(
                serde_json::from_value(fixture["owner"].clone()).unwrap(),
                "flow-ref",
                1,
                ScopeKind::Head,
            )
            .with_attributes(
                serde_json::from_value(fixture["record"]["attributes"].clone()).unwrap(),
            )
            .with_owner_authorized(true);
            assert!(diagnostics_admitted(None, Some(&flow)));
            let mut worker = flow.clone();
            worker.kind = ScopeKind::Worker;
            assert!(!diagnostics_admitted(
                Some(&reserved(OPERATOR_MODULE)),
                Some(&worker)
            ));
            let mut foreign = flow.clone();
            foreign.owner = reserved("basal");
            assert!(!diagnostics_admitted(None, Some(&foreign)));
            let mut unauthorized = flow;
            unauthorized.owner_authorized = false;
            assert!(!diagnostics_admitted(None, Some(&unauthorized)));
        }
        for mut rejected in [head.clone(), head.clone(), head.clone(), head]
            .into_iter()
            .enumerate()
        {
            match rejected.0 {
                0 => rejected.1.kind = ScopeKind::Worker,
                1 => rejected.1.owner_authorized = false,
                2 => rejected.1.owner = reserved("foreign"),
                _ => rejected.1.attributes.run_id = Some("worker-run".into()),
            }
            assert!(!diagnostics_admitted(None, Some(&rejected.1)));
        }
        assert!(!diagnostics_admitted(Some(&Principal::Direct), None));
        assert!(!diagnostics_admitted(Some(&reserved("foreign")), None));
        assert!(!diagnostics_admitted(None, None));
    }

    #[test]
    fn identity_comes_only_from_the_stamp() {
        assert_eq!(
            from_route(
                Some(&Principal::Reserved {
                    module_id: OPERATOR_MODULE.into()
                }),
                None
            ),
            Caller::Operator
        );
        // Any local process can hold the connection file and be `direct`.
        assert_eq!(from_route(Some(&Principal::Direct), None), Caller::Local);
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
            Caller::Agent {
                agent_id: "SYNAPSE".into(),
                scope_ref: "s-1".into()
            }
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
            Caller::Module { .. }
        ));
    }

    fn worker(owner: &str, authorized: bool, attributes: ScopeAttributes) -> ScopeStamp {
        ScopeStamp::new(
            Principal::Reserved {
                module_id: owner.to_owned(),
            },
            "task-1",
            4,
            ScopeKind::Worker,
        )
        .with_attributes(attributes)
        .with_owner_authorized(authorized)
    }

    fn identity(harness: &str, session: &str) -> BindIdentity {
        BindIdentity::new("/work/project", harness, session)
    }

    #[test]
    fn dev_operator_app_admits_only_a_direct_unscoped_non_fed_route() {
        let direct = Some(Principal::Direct);
        let admitted = || identity("ckdesk", "desktop-session");
        // The desktop app's own shape is admitted.
        assert!(dev_operator_app_admission(
            direct.as_ref(),
            None,
            &admitted()
        ));
        assert_eq!(
            caller_for_route(direct.as_ref(), None, &admitted()),
            Caller::DevOperatorApp
        );
        assert_eq!(
            caller_for_route(direct.as_ref(), None, &admitted()).label(),
            "operator:local-direct"
        );
        // `from_route` alone still says local: only the bind-aware caller
        // upgrades the admitted route.
        assert_eq!(from_route(direct.as_ref(), None), Caller::Local);
    }

    #[test]
    fn dev_operator_app_refuses_every_exclusion() {
        let direct = Some(Principal::Direct);
        let scoped = scope(CORE_MODULE, Some("SYNAPSE"), true);
        let cases: Vec<(Option<Principal>, Option<ScopeStamp>, BindIdentity)> = vec![
            // A scope keeps its own authority, even head, worker and flow sessions.
            (
                Some(Principal::Direct),
                Some(scoped),
                identity("ckdesk", "s"),
            ),
            // Federated harnesses keep their own authority.
            (direct.clone(), None, identity("fed-phone", "s")),
            (direct.clone(), None, identity("fed-anything", "s")),
            // The OpenCode plugin's shared channel is exactly this pair.
            (direct.clone(), None, identity("alfonso", "prefrontal-core")),
            // Reserved (other than callosum) and unverified binds are never admitted.
            (
                Some(Principal::Reserved {
                    module_id: "aft".into(),
                }),
                None,
                identity("ckdesk", "s"),
            ),
            (
                Some(Principal::Reserved {
                    module_id: OPERATOR_MODULE.into(),
                }),
                None,
                identity("ckdesk", "s"),
            ),
            (
                Some(Principal::Reserved {
                    module_id: CORE_MODULE.into(),
                }),
                None,
                identity("ckdesk", "s"),
            ),
            (Some(Principal::Unverified), None, identity("ckdesk", "s")),
            (None, None, identity("ckdesk", "s")),
        ];
        for (principal, scope, id) in cases {
            assert!(
                !dev_operator_app_admission(principal.as_ref(), scope.as_ref(), &id),
                "admitted {principal:?} {scope:?} {} {}",
                id.harness,
                id.session
            );
            assert_ne!(
                caller_for_route(principal.as_ref(), scope.as_ref(), &id),
                Caller::DevOperatorApp,
                "admitted {principal:?} {scope:?} {} {}",
                id.harness,
                id.session
            );
        }
        // Near misses of the shared channel are still admitted: only the
        // exact harness and session pair is excluded.
        for (harness, session) in [
            ("alfonso", "other"),
            ("alfonso-desktop", "prefrontal-core"),
            ("ckdesk", "prefrontal-core"),
        ] {
            let id = identity(harness, session);
            assert!(
                dev_operator_app_admission(direct.as_ref(), None, &id),
                "refused near miss {harness:?} {session:?}"
            );
        }
    }

    #[test]
    fn a_worker_scope_is_core_owned_authorized_and_carries_no_agent() {
        let found = Some(WorkerScope {
            scope_ref: "task-1".into(),
            scope_epoch: 4,
        });
        let head = || ScopeAttributes::new().with_on_behalf_of_agent(Some("SYNAPSE".into()));
        // With or without the head it works for.
        assert_eq!(worker_scope(&worker(CORE_MODULE, true, head())), found);
        assert_eq!(
            worker_scope(&worker(CORE_MODULE, true, ScopeAttributes::new())),
            found
        );
        // It is still no agent to every other op.
        assert!(matches!(
            from_route(
                Some(&Principal::Direct),
                Some(&worker(CORE_MODULE, true, head()))
            ),
            Caller::Other(_)
        ));
        for refused in [
            worker(CORE_MODULE, false, head()),
            worker("aft", true, head()),
            worker(
                CORE_MODULE,
                true,
                ScopeAttributes::new().with_agent_id(Some("SYNAPSE".into())),
            ),
            // A run scope: codemode never runs a program from inside a run.
            worker(CORE_MODULE, true, head().with_run_id(Some("r".into()))),
            worker(CORE_MODULE, true, head().with_flow_id(Some("f".into()))),
            worker(
                CORE_MODULE,
                true,
                ScopeAttributes::new().with_delegates(true),
            ),
            // A head's session scope is not a worker's, even when it has no `agent_id`.
            ScopeStamp::new(
                Principal::Reserved {
                    module_id: CORE_MODULE.into(),
                },
                "task-1",
                4,
                ScopeKind::Head,
            )
            .with_attributes(head())
            .with_owner_authorized(true),
        ] {
            assert_eq!(worker_scope(&refused), None, "{refused:?}");
        }
    }
}
