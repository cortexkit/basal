//! The `flow.*` ops: who may call each (the operator, a local caller, the
//! flow's owning agent, another agent, core, and any other module), and
//! what each does.

mod common;

use basal_core::{Actor, Admission, RunState};
use basal_host::CardDecision;
use basal_host::mock::Fault;
use basal_module::caller::{self, CORE_MODULE, Caller, OPERATOR_MODULE};
use basal_module::ops::LOCAL_AUTHOR;
use common::{
    Fixture, Options, T0, admit, agent, events_manifest, fixture, install, install_approved,
};
use serde_json::{Value, json};
use subc_protocol::Principal;

const SCRIPT: &str = "const r = await ops.call('mock', 'echo', { n: 1 }); return r.n;";
const OWNER: &str = "SYNAPSE";
const FLOW: &str = "flow-own";

fn call(f: &Fixture, caller: &Caller, method: &str, params: Value) -> Result<Value, String> {
    f.module
        .handle(caller, method, params)
        .map_err(|e| format!("{}: {}", e.code, e.message))
}

fn refused(r: &Result<Value, String>) -> bool {
    matches!(r, Err(e) if e.starts_with("not_permitted"))
}

/// A flow owned by SYNAPSE, approved, with a second version installed by
/// the owner and pending.
fn owned_flow(tag: &str) -> Fixture {
    let f = fixture(tag, Options::default());
    install_approved(&f, &agent(OWNER), SCRIPT, &events_manifest(FLOW));
    f
}

fn v2() -> Value {
    let mut m = events_manifest(FLOW);
    m["version"] = json!(2);
    m
}

/// What an op did for one caller: ran (or failed for a reason other than
/// who called), refused it, or refused it as needing the attested operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Ran,
    Refused,
    NeedsOperator,
}
use Seen::{NeedsOperator as Attest, Ran as Yes, Refused as No};

fn seen(r: &Result<Value, String>) -> Seen {
    match r {
        Err(e) if e.starts_with("not_permitted") => Seen::Refused,
        Err(e) if e.starts_with("operator_attestation_required") => Seen::NeedsOperator,
        _ => Seen::Ran,
    }
}

/// The caller basal decides for a route stamped with this principal and no
/// scope.
fn unscoped(principal: Principal) -> Caller {
    caller::from_route(Some(&principal), None)
}

fn reserved(module_id: &str) -> Principal {
    Principal::Reserved {
        module_id: module_id.into(),
    }
}

#[test]
fn authorization_matrix() {
    let f = owned_flow("ops-matrix");
    // The operator and the local caller come from their routes' stamps: only
    // the attested callosum is the operator, and a direct key-holder (any
    // local process) is a local caller.
    let operator = unscoped(reserved(OPERATOR_MODULE));
    let local = unscoped(Principal::Direct);
    assert_eq!(operator, Caller::Operator);
    assert_eq!(local, Caller::Local);
    let owner = agent(OWNER);
    let other = agent("ALF");
    let core = unscoped(reserved(CORE_MODULE));
    let module = unscoped(reserved("aft"));

    // Each row: op, params, and what happens for [operator, local, owner,
    // another agent, core, another module]: Yes ran, No refused
    // (not_permitted), Attest refused as needing the attested operator
    // (operator_attestation_required).
    let install_v2 = json!({ "script": SCRIPT, "manifest": v2().to_string() });
    let cases: Vec<(&str, Value, [Seen; 6])> = vec![
        // A local caller may install, but not replace a flow someone else
        // wrote: that is the operator's (see
        // a_local_caller_installs_only_its_own_flows_with_a_digest_sink).
        ("flow.install", install_v2, [Yes, Attest, Yes, No, No, No]),
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "trigger": { "kind": "synthetic" } }),
            [Yes, Attest, Yes, No, No, No],
        ),
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "mode": "live", "trigger": { "kind": "synthetic" } }),
            [Yes, Attest, No, No, No, No],
        ),
        ("flow.health", Value::Null, [Yes, Attest, No, No, Yes, No]),
        (
            "flow.reconcile",
            json!({ "run_id": "run-none", "position": 0, "resolution": "cancel" }),
            [Yes, Attest, No, No, No, No],
        ),
        (
            "flow.drain",
            json!({ "resume": true }),
            [Yes, Attest, No, No, No, No],
        ),
        (
            "flow.disable",
            json!({ "flow_id": FLOW }),
            [Yes, Attest, Yes, No, No, No],
        ),
        (
            "flow.enable",
            json!({ "flow_id": FLOW }),
            [Yes, Attest, Yes, No, No, No],
        ),
    ];
    for (method, params, expected) in cases {
        for (caller, want) in [&operator, &local, &owner, &other, &core, &module]
            .into_iter()
            .zip(expected)
        {
            let result = call(&f, caller, method, params.clone());
            assert_eq!(
                seen(&result),
                want,
                "{method} {params} as {}: {result:?}",
                caller.label()
            );
            if method == "flow.disable" && want == Yes {
                // Put the flow back for the next caller.
                call(&f, &operator, "flow.enable", json!({ "flow_id": FLOW })).expect("enable");
            }
        }
    }

    // Enabling depends on who disabled the flow. The owner undoes a disable
    // it made itself...
    let flow = json!({ "flow_id": FLOW });
    call(
        &f,
        &owner,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "mine" }),
    )
    .expect("the owner disables");
    // A local caller can neither disable the flow again over the owner's
    // disable (an operator's disable would replace the owner's record) nor
    // enable it.
    let r = call(
        &f,
        &local,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "local stop" }),
    );
    assert_eq!(seen(&r), Attest, "local over the owner's disable: {r:?}");
    let r = call(&f, &local, "flow.enable", flow.clone());
    assert_eq!(seen(&r), Attest, "local after the owner's disable: {r:?}");
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert_eq!(record.disabled_reason.as_deref(), Some("mine"));
    let r = call(&f, &owner, "flow.enable", flow.clone());
    assert_eq!(
        r.as_ref().map(|v| v["changed"].clone()),
        Ok(json!(true)),
        "the owner re-enables its own disable: {r:?}"
    );
    // ...but not the operator's stop...
    call(
        &f,
        &operator,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "stop" }),
    )
    .expect("the operator disables");
    let r = call(&f, &owner, "flow.enable", flow.clone());
    assert!(refused(&r), "owner after an operator disable: {r:?}");
    let r = call(&f, &local, "flow.enable", flow.clone());
    assert_eq!(seen(&r), Attest, "local after an operator disable: {r:?}");
    assert!(
        !f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .enabled
    );
    call(&f, &operator, "flow.enable", flow.clone()).expect("the operator enables");
    // ...nor the runtime's auto-disable, which is the flow's loop protection.
    f.module
        .rt
        .disable_flow(FLOW, &Actor::Runtime, "saturated")
        .expect("auto-disable");
    let health = call(&f, &operator, "flow.health", Value::Null).expect("health");
    let own = health["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["flow_id"] == FLOW).cloned())
        .expect("listed");
    assert_eq!(own["disabled"]["by"], "auto", "{own:#}");
    assert_eq!(own["disabled"]["reason"], "saturated");
    let r = call(&f, &owner, "flow.enable", flow.clone());
    assert!(refused(&r), "owner after an auto-disable: {r:?}");
    let r = call(&f, &local, "flow.enable", flow.clone());
    assert_eq!(seen(&r), Attest, "local after an auto-disable: {r:?}");
    assert!(
        !f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .enabled
    );
    call(&f, &operator, "flow.enable", flow).expect("the operator enables");
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert!(record.enabled);
    assert_eq!(record.disabled_by, None, "enabling clears who disabled it");
    assert_eq!(record.disabled_reason, None);
}

#[test]
fn a_local_caller_installs_only_its_own_flows_with_a_digest_sink() {
    let f = owned_flow("ops-local-install");
    let local = unscoped(Principal::Direct);
    let install_as = |caller: &Caller, extra: Value, manifest: &Value| {
        let mut params = json!({ "script": SCRIPT, "manifest": manifest.to_string() });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        call(&f, caller, "flow.install", params)
    };

    // Its own new flow installs under the local author. The card says so to
    // core as {"local": true}, with no session: core shows it as from an
    // unverified local caller and routes it through the first digest sink.
    let reply = install_as(&local, json!({}), &events_manifest("flow-local")).expect("installs");
    let card = f
        .consent
        .card(reply["card_id"].as_str().expect("card id"))
        .expect("raised");
    assert_eq!(card.fields["wire_author"], json!({ "local": true }));
    assert!(
        card.fields.get("session_ref").is_none(),
        "{:#}",
        card.fields
    );
    assert_eq!(card.fields["author"], LOCAL_AUTHOR);
    let mut v2_local = events_manifest("flow-local");
    v2_local["version"] = json!(2);
    install_as(&local, json!({}), &v2_local).expect("a second version of its own flow");
    // An agent cannot install a version of the local caller's flow.
    let r = install_as(&agent(OWNER), json!({}), &v2_local);
    assert_eq!(seen(&r), No, "an agent replacing the local flow: {r:?}");

    // Installing in another author's name, with the loop override, or over a
    // flow someone else wrote is the operator's alone.
    for (what, extra, manifest) in [
        (
            "in another author's name",
            json!({ "author": OWNER }),
            events_manifest("flow-named"),
        ),
        (
            "with the loop override",
            json!({ "loop_override": true }),
            events_manifest("flow-loop"),
        ),
        ("replacing SYNAPSE's flow", json!({}), v2()),
    ] {
        let r = install_as(&local, extra, &manifest);
        assert_eq!(seen(&r), Attest, "local install {what}: {r:?}");
    }
    for flow in ["flow-named", "flow-loop"] {
        assert!(f.module.rt.flow(flow).expect("flow").is_none(), "{flow}");
    }

    // Without a digest sink core has nowhere to show a local caller's card,
    // so basal refuses before installing or raising anything.
    let mut bare = events_manifest("flow-bare");
    bare["sinks"] = json!([]);
    let r = install_as(&local, json!({}), &bare);
    assert!(
        matches!(&r, Err(e) if e.starts_with("local_install_needs_digest_sink")),
        "{r:?}"
    );
    assert!(f.module.rt.flow("flow-bare").expect("flow").is_none());
    assert!(f.module.rt.cards("flow-bare").expect("cards").is_empty());
    assert!(
        f.consent.cards().iter().all(|c| c.flow_id != "flow-bare"),
        "no card raised"
    );
    // The digest-sink requirement is for local callers only: core routes the
    // operator's card without one.
    install_as(&Caller::Operator, json!({}), &bare).expect("the operator installs it");
}

#[test]
fn install_raises_one_card_per_version_and_its_decision_approves_or_rejects() {
    let f = fixture("ops-install", Options::default());
    let owner = agent(OWNER);
    let reply = install(&f, &owner, SCRIPT, &events_manifest(FLOW));
    assert_eq!(reply["state"], "pending");
    assert_eq!(reply["version"], 1);
    let card_id = reply["card_id"].as_str().expect("card").to_owned();
    let card = f.consent.card(&card_id).expect("raised");
    let fields = &card.fields;
    assert_eq!(fields["flow_id"], FLOW);
    assert_eq!(fields["author"], OWNER);
    assert_eq!(fields["purpose"], "A test flow.");
    assert_eq!(fields["sinks"][0]["agent"], "SYNAPSE");
    assert_eq!(fields["sinks"][0]["digest_max"], "piggyback");
    assert_eq!(fields["ops"][0]["kind"], "query");
    assert_eq!(fields["ops"][1]["kind"], "mutate");
    assert_eq!(fields["trigger"]["events"][0]["origin"], "external");
    assert_eq!(fields["code_hash"], reply["code_hash"]);
    assert_eq!(fields["code"]["script"], SCRIPT);
    assert_eq!(fields["dry_run_summary"]["window"]["kind"], "events");

    // Installing the same version again raises the same card again (a raise
    // the consent plane lost is retried this way) and shows one card.
    let again = install(&f, &owner, SCRIPT, &events_manifest(FLOW));
    assert_eq!(again["card_id"], reply["card_id"]);
    assert_eq!(f.consent.raise_count(&card_id), 2);
    assert_eq!(f.consent.cards().len(), 1);
    assert!(
        f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .approved_version
            .is_none()
    );

    // The decision arrives through the consent interface and approves.
    assert!(
        f.consent
            .decide(&card_id, CardDecision::Approve, "operator")
    );
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert_eq!(record.approved_version, Some(1));
    assert_eq!(record.owner.as_deref(), Some(OWNER));
    let decided = install(&f, &owner, SCRIPT, &events_manifest(FLOW));
    assert_eq!(decided["state"], "approved");
    // A second delivery of a decision changes nothing.
    assert!(f.consent.decide(&card_id, CardDecision::Reject, "operator"));
    assert_eq!(
        f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .approved_version,
        Some(1)
    );
    // The author is told.
    let outbox = f.module.rt.outbox().expect("outbox");
    assert!(
        outbox
            .iter()
            .any(|n| n.kind == "flow.install_approved" && n.recipient.as_deref() == Some(OWNER))
    );

    // A rejected version is not approved, and installing it again does not
    // raise it again.
    let second = install(&f, &owner, SCRIPT, &v2());
    let second_card = second["card_id"].as_str().expect("card").to_owned();
    assert!(
        f.consent
            .decide(&second_card, CardDecision::Reject, "operator")
    );
    assert_eq!(
        f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .approved_version,
        Some(1)
    );
    let again = install(&f, &owner, SCRIPT, &v2());
    assert_eq!(again["state"], "rejected");
    assert_eq!(f.consent.raise_count(&second_card), 1);
}

#[test]
fn install_refuses_what_an_agent_may_not_do() {
    let f = owned_flow("ops-install-refusals");
    // Another agent cannot replace a flow it does not own.
    let r = call(
        &f,
        &agent("ALF"),
        "flow.install",
        json!({ "script": SCRIPT, "manifest": v2().to_string() }),
    );
    assert!(refused(&r), "{r:?}");
    // An agent installs for itself only.
    let r = call(
        &f,
        &agent(OWNER),
        "flow.install",
        json!({ "script": SCRIPT, "manifest": v2().to_string(), "author": "ALF" }),
    );
    assert!(refused(&r), "{r:?}");
    // Only the operator overrides the loop install rule.
    let r = call(
        &f,
        &agent(OWNER),
        "flow.install",
        json!({ "script": SCRIPT, "manifest": v2().to_string(), "loop_override": true }),
    );
    assert!(refused(&r), "{r:?}");
    // The operator installs for anyone.
    let r = call(
        &f,
        &Caller::Operator,
        "flow.install",
        json!({ "script": SCRIPT, "manifest": events_manifest("flow-alf").to_string(), "author": "ALF" }),
    )
    .expect("operator installs for ALF");
    let card = f
        .consent
        .card(r["card_id"].as_str().expect("card"))
        .expect("raised");
    assert_eq!(card.fields["author"], "ALF");
    // An invalid manifest is refused before anything is recorded.
    let r = call(
        &f,
        &agent(OWNER),
        "flow.install",
        json!({ "script": SCRIPT, "manifest": "{\"id\": \"x\"}" }),
    );
    assert!(
        matches!(&r, Err(e) if e.starts_with("install_refused")),
        "{r:?}"
    );
}

#[test]
fn install_keeps_the_version_when_the_consent_plane_is_down_and_raises_on_retry() {
    let f = fixture("ops-consent-down", Options::default());
    f.consent.set_unavailable(true);
    let r = call(
        &f,
        &agent(OWNER),
        "flow.install",
        json!({ "script": SCRIPT, "manifest": events_manifest(FLOW).to_string() }),
    );
    assert!(
        matches!(&r, Err(e) if e.starts_with("consent_unavailable")),
        "{r:?}"
    );
    assert!(f.consent.cards().is_empty());
    f.consent.set_unavailable(false);
    let reply = install(&f, &agent(OWNER), SCRIPT, &events_manifest(FLOW));
    assert_eq!(reply["state"], "pending");
    assert_eq!(f.consent.cards().len(), 1);
}

#[test]
fn health_reports_flows_runs_and_the_module() {
    let f = owned_flow("ops-health");
    let failing = install_approved(
        &f,
        &agent(OWNER),
        "throw new Error('broken');",
        &events_manifest("flow-failing"),
    );
    for t in ["f-1", "f-2"] {
        admit(&f, &failing, t);
        f.module.engine.run_until_idle(50).expect("idle");
    }
    admit(&f, FLOW, "waiting");
    f.clock.set(T0 + 5_000);
    let h = call(&f, &Caller::Core, "flow.health", Value::Null).expect("health");
    let flows = h["flows"].as_array().expect("flows");
    let failing_health = flows
        .iter()
        .find(|x| x["flow_id"] == "flow-failing")
        .expect("listed");
    assert_eq!(failing_health["state"], "enabled");
    assert_eq!(failing_health["consecutive_failures"], 2);
    assert_eq!(failing_health["last_run"]["outcome"], "failed");
    assert_eq!(failing_health["last_run"]["error_kind"], "script");
    let own = flows.iter().find(|x| x["flow_id"] == FLOW).expect("listed");
    assert_eq!(
        own["oldest_overdue_age_ms"], 5_000,
        "the admitted run not yet started: {h:#}"
    );
    assert_eq!(own["owner"], OWNER);
    assert_eq!(h["runs"]["oldest_pending_ms"], 5_000);
    assert!(h["runs"]["oldest_suspended_ms"].is_null());
    assert!(h["metrics"]["workers"]["spawned"].as_u64().is_some());
    assert!(h["metrics"]["replay"]["activations"].as_u64().unwrap_or(0) >= 2);
    assert!(h["pool"]["live"].as_u64().is_some());

    f.module
        .handle(
            &Caller::Operator,
            "flow.disable",
            json!({ "flow_id": FLOW }),
        )
        .expect("disable");
    let h = call(&f, &Caller::Operator, "flow.health", Value::Null).expect("health");
    let own = h["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["flow_id"] == FLOW).cloned())
        .expect("listed");
    assert_eq!(own["state"], "disabled");
    assert_eq!(own["auto_disabled"], false);
    assert_eq!(own["disabled"]["by"], "operator");
    assert_eq!(own["disabled"]["actor"], "operator:operator");
    assert_eq!(own["disabled"]["reason"], "disabled by request");
}

/// A flow no one approved is listed as `unapproved`, never `enabled`: core
/// decides from `state` whether a flow may hold a claim.
#[test]
fn health_lists_a_flow_with_no_approved_version_as_unapproved() {
    let f = fixture("ops-health-unapproved", Options::default());
    let owner = agent(OWNER);
    let state = |f: &Fixture| {
        let h = call(f, &Caller::Operator, "flow.health", Value::Null).expect("health");
        h["flows"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["flow_id"] == FLOW).cloned())
            .expect("a flow whose card is open is still listed")
    };
    let reply = install(&f, &owner, SCRIPT, &events_manifest(FLOW));
    let pending = state(&f);
    assert_eq!(pending["state"], "unapproved", "card pending: {pending:#}");
    assert!(pending["approved_version"].is_null());
    assert!(pending["disabled"].is_null());

    let card_id = reply["card_id"].as_str().expect("card").to_owned();
    assert!(f.consent.decide(&card_id, CardDecision::Reject, "operator"));
    let declined = state(&f);
    assert_eq!(
        declined["state"], "unapproved",
        "card declined: {declined:#}"
    );
    assert!(declined["approved_version"].is_null());

    let second = install(&f, &owner, SCRIPT, &v2());
    let second_card = second["card_id"].as_str().expect("card").to_owned();
    assert!(
        f.consent
            .decide(&second_card, CardDecision::Approve, "operator")
    );
    let approved = state(&f);
    assert_eq!(approved["state"], "enabled");
    assert_eq!(approved["approved_version"], 2);
}

#[test]
fn reconcile_resolves_an_unknown_call_for_the_operator() {
    let f = fixture("ops-reconcile", Options::default());
    let mut manifest = events_manifest("flow-post");
    manifest["ops"] = json!([{ "module": "mock", "op": "post" }]);
    let flow = install_approved(
        &f,
        &agent(OWNER),
        "const r = await ops.call('mock', 'post', { n: 1 }); return r;",
        &manifest,
    );
    // The mutation does not deduplicate and its reply is lost: its outcome
    // cannot be proven, so the run stops for the operator.
    f.mock.inject(
        "mock",
        "post",
        &[Fault {
            proven_unsent: false,
            effect_applied: true,
        }],
    );
    let run = admit(&f, &flow, "p-1");
    f.module.engine.run_until_idle(50).expect("idle");
    assert_eq!(
        f.module.rt.run(&run).expect("run").state,
        RunState::NeedsReconcile
    );
    let h = call(&f, &Caller::Operator, "flow.health", Value::Null).expect("health");
    assert_eq!(h["runs"]["oldest_needs_reconcile_ms"], 0);
    let post = h["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["flow_id"] == "flow-post").cloned())
        .expect("listed");
    assert_eq!(post["needs_reconcile"], true);
    assert_eq!(post["needs_reconcile_runs"], json!([run.clone()]));

    let reply = call(
        &f,
        &Caller::Operator,
        "flow.reconcile",
        json!({
            "run_id": run,
            "position": 0,
            "resolution": { "observed_result": { "settlement": "fulfilled", "value": { "applied": true } } },
        }),
    )
    .expect("reconcile");
    assert_ne!(reply["state"], "needs_reconcile");
    f.module.engine.run_until_idle(50).expect("idle");
    let done = f.module.rt.run(&run).expect("run");
    assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
    assert_eq!(f.mock.effects().len(), 1, "the mutation was not sent again");
}

#[test]
fn drain_stops_admission_until_resumed() {
    let f = owned_flow("ops-drain");
    let reply = call(&f, &Caller::Operator, "flow.drain", Value::Null).expect("drain");
    assert_eq!(reply["draining"], true);
    assert_eq!(
        f.module
            .rt
            .admit_trigger(FLOW, "d-1", basal_proto::JsonText::null())
            .expect("admit"),
        Admission::Draining
    );
    call(
        &f,
        &Caller::Operator,
        "flow.drain",
        json!({ "resume": true }),
    )
    .expect("resume");
    admit(&f, FLOW, "d-1");
}

#[test]
fn an_operator_disable_takes_over_the_owners_disable() {
    let f = owned_flow("ops-takeover");
    let owner = agent(OWNER);
    let flow = json!({ "flow_id": FLOW });
    call(
        &f,
        &owner,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "pause" }),
    )
    .expect("the owner disables");
    let notified = f.module.rt.outbox().expect("outbox").len();
    // The flow is already disabled, but the operator's disable still lands:
    // it now stands as the operator's.
    let reply = call(
        &f,
        &Caller::Operator,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "stop" }),
    )
    .expect("the operator disables");
    assert_eq!(reply["changed"], true);
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert_eq!(record.disabled_by.as_deref(), Some("operator:operator"));
    assert_eq!(record.disabled_reason.as_deref(), Some("stop"));
    assert_eq!(
        f.module.rt.outbox().expect("outbox").len(),
        notified + 1,
        "the owner is told"
    );
    // So the owner can no longer undo it.
    let r = call(&f, &owner, "flow.enable", flow.clone());
    assert!(refused(&r), "{r:?}");
    assert!(
        !f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .enabled
    );
    // An agent's disable of a disabled flow changes nothing.
    let again = call(
        &f,
        &owner,
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "mine" }),
    )
    .expect("no-op");
    assert_eq!(again["changed"], false);
    assert_eq!(
        f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .disabled_by
            .as_deref(),
        Some("operator:operator")
    );
    call(&f, &Caller::Operator, "flow.enable", flow).expect("the operator enables");
    assert!(
        f.module
            .rt
            .flow(FLOW)
            .expect("flow")
            .expect("exists")
            .enabled
    );
}

#[test]
fn the_owner_disables_and_enables_its_flow() {
    let f = owned_flow("ops-disable");
    let reply = call(
        &f,
        &agent(OWNER),
        "flow.disable",
        json!({ "flow_id": FLOW, "reason": "noisy" }),
    )
    .expect("disable");
    assert_eq!(reply["changed"], true);
    assert_eq!(
        f.module
            .rt
            .admit_trigger(FLOW, "x-1", basal_proto::JsonText::null())
            .expect("admit"),
        Admission::Disabled
    );
    let record = f.module.rt.flow(FLOW).expect("flow").expect("exists");
    assert_eq!(record.disabled_by.as_deref(), Some("agent:SYNAPSE"));
    call(&f, &agent(OWNER), "flow.enable", json!({ "flow_id": FLOW })).expect("enable");
    admit(&f, FLOW, "x-1");
    let unknown = call(&f, &Caller::Operator, "flow.nothing", Value::Null);
    assert!(matches!(&unknown, Err(e) if e.starts_with("unknown_method")));
}
