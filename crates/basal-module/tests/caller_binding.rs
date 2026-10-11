//! Which caller the production handler assigns to a route. Each test binds a
//! route through `BasalHandler::on_bind` with the principal and scope stamp
//! a real daemon would put in the bind request, reads back the caller the
//! handler will use for every request on that route, and runs `flow.*` ops
//! as that caller.

mod common;

use basal_module::caller::{CORE_MODULE, Caller, OPERATOR_MODULE};
use basal_module::serve::BasalHandler;
use common::{Fixture, Options, events_manifest, fixture, install, install_approved};
use serde_json::{Value, json};
use subc_client_rs::{ModuleHandler, RouteBindRequest, RouteHandle};
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};
use subc_protocol::{BindIdentity, Principal, RouteTarget};

const SCRIPT: &str = "const r = await ops.call('mock', 'echo', { n: 1 }); return r.n;";
const OWNER: &str = "SYNAPSE";
const FLOW: &str = "flow-own";
/// The bind identity's session, which basal must not use for anything.
const SESSION: &str = "ses-synapse-1";

/// A handler that never reaches HELLO_ACK: binding routes and deciding
/// callers need no store.
fn handler() -> BasalHandler {
    BasalHandler::new(
        Box::new(|_| unreachable!("these tests never start the module's store")),
        Box::new(|| unreachable!("these tests never start the module's hosts")),
    )
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// A bind of `handle` to basal's management surface, from a harness whose
/// bind identity names `session`, with no principal and no scope yet.
fn bind_request(handle: RouteHandle, session: &str) -> RouteBindRequest {
    RouteBindRequest::new(
        handle,
        RouteTarget::ManagementSurface {
            module_id: "basal".into(),
        },
        BindIdentity::new("/work/project", "opencode", session),
    )
}

/// The `scope_ref` core would mint for the scope a route on `channel` is
/// admitted under: a different ref for every route in these tests.
fn scope_ref_of(channel: u16) -> String {
    format!("0f3c9a7d2b1e4c6a8d5f7e9b1a3c{channel:04x}")
}

/// A session scope owned by `reserved:<owner>`, naming `agent`.
fn scope(owner: &str, agent: &str, owner_authorized: bool, scope_ref: String) -> ScopeStamp {
    ScopeStamp {
        owner: Principal::Reserved {
            module_id: owner.to_owned(),
        },
        scope_ref,
        scope_epoch: 1,
        kind: ScopeKind::Head,
        parent: None,
        parent_state: None,
        attributes: ScopeAttributes::new().with_agent_id(Some(agent.to_owned())),
        owner_authorized,
    }
}

/// Binds the route through the handler and returns the caller it decides.
fn caller_of(handler: &BasalHandler, request: RouteBindRequest) -> Caller {
    let handle = request.handle;
    block_on(handler.on_bind(&request));
    handler.caller(&handle)
}

/// Binds a route the way an agent's harness gets one: principal `direct`
/// (the harness holds a daemon key), under a session scope owned by core,
/// marked owner-authorized by the daemon and naming `agent`, whose ref is
/// [`scope_ref_of`]`(channel)`. The bind identity names `session`.
fn agent_route(handler: &BasalHandler, channel: u16, agent: &str, session: &str) -> Caller {
    caller_of(
        handler,
        bind_request(RouteHandle::detached(channel, 1), session)
            .with_principal(Principal::Direct)
            .with_scope(scope(CORE_MODULE, agent, true, scope_ref_of(channel))),
    )
}

fn call(f: &Fixture, caller: &Caller, method: &str, params: Value) -> Result<Value, String> {
    f.module
        .handle(caller, method, params)
        .map_err(|e| format!("{}: {}", e.code, e.message))
}

fn refused(r: &Result<Value, String>) -> bool {
    matches!(r, Err(e) if e.starts_with("not_permitted"))
}

fn v2() -> Value {
    let mut m = events_manifest(FLOW);
    m["version"] = json!(2);
    m
}

/// Every op a flow's owning agent may call on it, refused for `caller`.
fn assert_refused_on_the_flow(f: &Fixture, caller: &Caller) {
    let cases = [
        (
            "flow.install",
            json!({ "script": SCRIPT, "manifest": v2().to_string() }),
        ),
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "trigger": { "kind": "synthetic" } }),
        ),
        ("flow.disable", json!({ "flow_id": FLOW })),
        ("flow.enable", json!({ "flow_id": FLOW })),
    ];
    for (method, params) in cases {
        let r = call(f, caller, method, params.clone());
        assert!(
            refused(&r),
            "{method} {params} as {}: {r:?}",
            caller.label()
        );
    }
}

/// subc-protocol 0.29 added `flow_id` to a scope's attributes: core sets it
/// when a route is opened on behalf of a flow rather than the agent itself.
/// The daemon delivers such a route only to a module that declares the
/// `flow-scopes/v1` capability, and basal does not, so no real bind to basal
/// carries it. This test proves only that a stamp carrying `flow_id` still
/// decodes and binds. Treating such a caller as a flow rather than its owner
/// agent is not implemented, and must be before basal declares the capability.
#[test]
fn a_flow_id_in_the_bind_stamp_decodes_without_changing_agent_identity() {
    let stamp: ScopeStamp = serde_json::from_value(json!({
        "owner": { "kind": "reserved", "module_id": CORE_MODULE },
        "ref": "scope-with-flow",
        "scope_epoch": 1,
        "kind": "head",
        "attributes": { "agent_id": OWNER, "flow_id": "flow-123" },
        "owner_authorized": true,
    }))
    .expect("the 0.29 scope decoder accepts flow_id");
    let handler = handler();
    let caller = caller_of(
        &handler,
        bind_request(RouteHandle::detached(12, 1), SESSION)
            .with_principal(Principal::Direct)
            .with_scope(stamp),
    );

    assert_eq!(
        caller,
        Caller::Agent {
            agent_id: OWNER.into(),
            scope_ref: "scope-with-flow".into(),
        }
    );
}

#[test]
fn an_owner_authorized_core_scope_names_the_agent_and_its_scope() {
    let handler = handler();
    let caller = agent_route(&handler, 1, OWNER, SESSION);
    assert_eq!(
        caller,
        Caller::Agent {
            agent_id: OWNER.into(),
            scope_ref: scope_ref_of(1),
        }
    );

    // The agent installs as itself. Its card names it to core by the scope
    // ref the daemon stamped on its route, and carries no session: the bind
    // identity's session is the opener's own claim.
    let f = fixture("caller-agent", Options::default());
    let reply = install(&f, &caller, SCRIPT, &events_manifest(FLOW));
    let card_id = reply["card_id"].as_str().expect("card id");
    let card = f.consent.card(card_id).expect("raised");
    assert_eq!(
        card.fields["wire_author"],
        json!({ "scope": scope_ref_of(1) }),
        "{:#}",
        card.fields
    );
    assert!(
        card.fields.get("session_ref").is_none(),
        "{:#}",
        card.fields
    );
    assert!(
        !card.fields.to_string().contains(SESSION),
        "the bind session reaches the card: {:#}",
        card.fields
    );

    // Once approved, the flow is the agent's: it may disable and enable it.
    assert!(
        f.consent
            .decide(card_id, basal_host::CardDecision::Approve, "operator")
    );
    call(&f, &caller, "flow.disable", json!({ "flow_id": FLOW })).expect("the owner disables");
    call(&f, &caller, "flow.enable", json!({ "flow_id": FLOW })).expect("the owner enables");
}

#[test]
fn a_scope_owned_by_another_module_names_no_agent() {
    let handler = handler();
    // Any module may register scopes of its own, and the daemon may even
    // mark it owner-authorized, but only a scope core owns can name an
    // agent: core is the module that owns agents' sessions.
    let caller = caller_of(
        &handler,
        bind_request(RouteHandle::detached(1, 1), SESSION)
            .with_principal(Principal::Direct)
            .with_scope(scope("aft", OWNER, true, scope_ref_of(1))),
    );
    assert!(matches!(caller, Caller::Other(_)), "{caller:?}");

    let f = fixture("caller-foreign-scope", Options::default());
    install_approved(
        &f,
        &agent_route(&handler, 2, OWNER, SESSION),
        SCRIPT,
        &events_manifest(FLOW),
    );
    assert_refused_on_the_flow(&f, &caller);
}

#[test]
fn a_scope_without_owner_authorization_names_no_agent() {
    let handler = handler();
    // Core's scope, but the daemon has not marked its owner authorized to
    // set attributes such as `agent_id`, so the agent id in it is not
    // accepted as an identity.
    let caller = caller_of(
        &handler,
        bind_request(RouteHandle::detached(1, 1), SESSION)
            .with_principal(Principal::Direct)
            .with_scope(scope(CORE_MODULE, OWNER, false, scope_ref_of(1))),
    );
    assert!(matches!(caller, Caller::Other(_)), "{caller:?}");

    let f = fixture("caller-unvouched-scope", Options::default());
    install_approved(
        &f,
        &agent_route(&handler, 2, OWNER, SESSION),
        SCRIPT,
        &events_manifest(FLOW),
    );
    assert_refused_on_the_flow(&f, &caller);
}

#[test]
fn a_route_without_a_scope_is_never_an_agent() {
    let handler = handler();
    let unscoped = |channel: u16, principal: Option<Principal>| {
        let request = bind_request(RouteHandle::detached(channel, 1), SESSION);
        caller_of(
            &handler,
            match principal {
                Some(p) => request.with_principal(p),
                None => request,
            },
        )
    };

    // Only the route the daemon attests as callosum is the operator. A
    // direct key-holder with no scope is a local caller, whatever session
    // its bind identity claims, and core's own route is core.
    assert_eq!(
        unscoped(
            9,
            Some(Principal::Reserved {
                module_id: OPERATOR_MODULE.into()
            })
        ),
        Caller::Operator
    );
    let local = unscoped(1, Some(Principal::Direct));
    assert_eq!(local, Caller::Local);
    assert_eq!(
        unscoped(
            2,
            Some(Principal::Reserved {
                module_id: CORE_MODULE.into()
            })
        ),
        Caller::Core
    );

    // These callers cannot manage agents' flows: another module, an unverified route, a
    // route whose principal the daemon did not record, a route never bound,
    // and a route that has gone, even one an agent's scope admitted.
    let others = [
        unscoped(
            3,
            Some(Principal::Reserved {
                module_id: "aft".into(),
            }),
        ),
        unscoped(4, Some(Principal::Unverified)),
        unscoped(5, None),
        handler.caller(&RouteHandle::detached(6, 1)),
        {
            let gone = RouteHandle::detached(7, 1);
            agent_route(&handler, 7, OWNER, SESSION);
            block_on(handler.on_route_gone(&gone));
            handler.caller(&gone)
        },
    ];

    let f = fixture("caller-unscoped", Options::default());
    install_approved(
        &f,
        &agent_route(&handler, 8, OWNER, SESSION),
        SCRIPT,
        &events_manifest(FLOW),
    );
    // The local caller is not the flow's owner either: dry-running,
    // disabling and enabling the flow are refused as needing the attested
    // operator.
    for (method, params) in [
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "trigger": { "kind": "synthetic" } }),
        ),
        ("flow.disable", json!({ "flow_id": FLOW })),
        ("flow.enable", json!({ "flow_id": FLOW })),
    ] {
        let r = call(&f, &local, method, params);
        assert!(
            matches!(&r, Err(e) if e.starts_with("operator_attestation_required")),
            "{method} as local: {r:?}"
        );
    }
    for caller in &others {
        assert!(
            matches!(caller, Caller::Other(_) | Caller::Module { .. }),
            "{caller:?}"
        );
        assert_refused_on_the_flow(&f, caller);
        let r = call(&f, caller, "flow.health", Value::Null);
        assert!(refused(&r), "flow.health as {}: {r:?}", caller.label());
    }
}

#[test]
fn an_agent_cannot_act_on_a_flow_it_does_not_own() {
    let handler = handler();
    let owner = agent_route(&handler, 1, OWNER, SESSION);
    let other = agent_route(&handler, 2, "ALF", "ses-alf-1");
    assert_eq!(
        other,
        Caller::Agent {
            agent_id: "ALF".into(),
            scope_ref: scope_ref_of(2),
        }
    );

    let f = fixture("caller-not-owner", Options::default());
    install_approved(&f, &owner, SCRIPT, &events_manifest(FLOW));

    // A vouched agent is still only itself: SYNAPSE's flow is not ALF's.
    assert_refused_on_the_flow(&f, &other);
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert!(record.enabled, "ALF's refused disable changed nothing");

    // The same ops as the owner, on its own route, are allowed.
    call(
        &f,
        &owner,
        "flow.dry_run",
        json!({ "flow_id": FLOW, "trigger": { "kind": "synthetic" } }),
    )
    .expect("the owner dry-runs its flow");
    call(&f, &owner, "flow.disable", json!({ "flow_id": FLOW })).expect("the owner disables");
    let r = call(&f, &other, "flow.enable", json!({ "flow_id": FLOW }));
    assert!(refused(&r), "ALF enabling SYNAPSE's disabled flow: {r:?}");
    call(&f, &owner, "flow.enable", json!({ "flow_id": FLOW })).expect("the owner enables");
}
