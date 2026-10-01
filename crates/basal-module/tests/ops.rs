//! The `flow.*` ops: who may call each (the operator, the flow's owning
//! agent, another agent, core, and any other module), and what each does.

mod common;

use basal_core::{Admission, RunState};
use basal_host::CardDecision;
use basal_host::mock::Fault;
use basal_module::caller::Caller;
use common::{
    Fixture, Options, T0, admit, agent, events_manifest, fixture, install, install_approved,
};
use serde_json::{Value, json};

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

#[test]
fn authorization_matrix() {
    let f = owned_flow("ops-matrix");
    let operator = Caller::Operator;
    let owner = agent(OWNER);
    let other = agent("ALF");
    let core = Caller::Core;
    let module = Caller::Other("reserved:aft".into());

    // (op, params, [operator, owner, another agent, core, another module])
    let install_v2 = json!({ "script": SCRIPT, "manifest": v2().to_string() });
    let cases: Vec<(&str, Value, [bool; 5])> = vec![
        (
            "flow.install",
            install_v2,
            [true, true, false, false, false],
        ),
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "trigger": { "kind": "synthetic" } }),
            [true, true, false, false, false],
        ),
        (
            "flow.dry_run",
            json!({ "flow_id": FLOW, "mode": "live", "trigger": { "kind": "synthetic" } }),
            [true, false, false, false, false],
        ),
        (
            "flow.health",
            Value::Null,
            [true, false, false, true, false],
        ),
        (
            "flow.reconcile",
            json!({ "run_id": "run-none", "position": 0, "resolution": "cancel" }),
            [true, false, false, false, false],
        ),
        (
            "flow.drain",
            json!({ "resume": true }),
            [true, false, false, false, false],
        ),
        (
            "flow.disable",
            json!({ "flow_id": FLOW }),
            [true, true, false, false, false],
        ),
        (
            "flow.enable",
            json!({ "flow_id": FLOW }),
            [true, true, false, false, false],
        ),
    ];
    for (method, params, expected) in cases {
        for (caller, allowed) in [&operator, &owner, &other, &core, &module]
            .into_iter()
            .zip(expected)
        {
            let result = call(&f, caller, method, params.clone());
            assert_eq!(
                !refused(&result),
                allowed,
                "{method} {params} as {}: {result:?}",
                caller.label()
            );
            if method == "flow.disable" && allowed {
                // Put the flow back for the next caller.
                call(&f, &operator, "flow.enable", json!({ "flow_id": FLOW })).expect("enable");
            }
        }
    }
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
    assert_eq!(failing_health["last_run"]["state"], "failed");
    assert_eq!(failing_health["last_run"]["error_kind"], "script");
    let own = flows.iter().find(|x| x["flow_id"] == FLOW).expect("listed");
    assert_eq!(
        own["oldest_overdue_ms"], 5_000,
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
    assert_eq!(post["needs_reconcile"], json!([run.clone()]));

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
