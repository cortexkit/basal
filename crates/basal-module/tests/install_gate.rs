//! The install gate end to end over the wire: the module asks core's
//! `flow.install_status` (here the wire fake, written from core's handler)
//! before every activation, through the same `RoutingHost` and `CoreHost`
//! production uses. Also that the `flow_install` request carries `placement`
//! only when the manifest states one.

mod common;
mod wire;

use std::sync::Arc;

use basal_core::{InstallGate, NoHooks, RunState};
use basal_host::core_consent::CoreConsent;
use basal_host::core_host::{CoreHost, decode_install_status};
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::{CORE, SubcCatalog};
use basal_host::transport::{Transport, WireError};
use basal_host::{Host, InstallStatus};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::unconfigured::UnconfiguredHost;
use serde_json::{Value, json};
use wire::Fake;

const FLOW: &str = "gate-flow";
const SCRIPT: &str = "return 1;";

fn manifest() -> Value {
    json!({"id":FLOW,"version":1,"purpose":"Report to Synapse","trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"SYNAPSE","digest_max":"piggyback"}],"ops":[{"module":"mock","op":"echo"}]})
}

/// A module whose host reaches the fake core over the wire, with the
/// install gate on, as production runs.
fn gated(fake: &Arc<Fake>, tag: &str) -> (common::Fixture, Arc<CoreConsent>) {
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let catalog = Arc::new(SubcCatalog::new(fake.clone()));
    let hosts = Hosts {
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(fake.clone(), catalog.clone())),
            Arc::new(CoreHost::new(fake.clone())),
            Arc::new(UnconfiguredHost::new()),
        )),
        catalog,
        consent: consent.clone(),
        hooks: Arc::new(NoHooks),
    };
    let f = common::fixture(
        tag,
        common::Options {
            hosts: Some(hosts),
            install_gate: InstallGate::Core,
            ..Default::default()
        },
    );
    (f, consent)
}

/// Installs the flow as the operator and approves its card through core's
/// answers. `in_core` says whether core then holds the install, as it does
/// after a real approval.
fn install_approved(f: &common::Fixture, fake: &Fake, consent: &CoreConsent, in_core: bool) {
    common::install(f, &Caller::Operator, SCRIPT, &manifest());
    let card = fake.calls("elicitation.request")[0]["params"]["flow_install"].clone();
    fake.enqueue(
        "elicitation.answers",
        vec![Ok(
            json!({"records":[{"state":"answered","answered_choice_id":"approve","flow_install":card}],"cursor":1}),
        )],
    );
    consent.poll_once().expect("decision applied");
    if in_core {
        fake.approve_install(FLOW, 1);
    }
}

fn health_entry(f: &common::Fixture) -> Value {
    let h = f
        .module
        .handle(&Caller::Operator, "flow.health", Value::Null)
        .expect("health");
    h["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["flow_id"] == FLOW).cloned())
        .expect("the flow is listed")
}

fn status_calls(fake: &Fake) -> Vec<Value> {
    fake.calls("flow.install_status")
}

/// Asserts the run never activated and was cancelled, and that `flow.health`
/// lists the flow as no longer enabled: `unapproved`, disabled by core.
fn assert_cancelled_and_unapproved(f: &common::Fixture, run_id: &str) {
    let run = f.module.rt.run(run_id).expect("run");
    assert_eq!(run.state, RunState::Cancelled, "{run:#?}");
    assert_eq!(f.module.rt.activation_count(run_id).expect("count"), 0);
    let entry = health_entry(f);
    assert_eq!(entry["state"], "unapproved", "{entry:#}");
    assert!(entry["approved_version"].is_null(), "{entry:#}");
    assert_eq!(entry["disabled"]["by"], "core", "{entry:#}");
    let listed = list_entry(f);
    assert_eq!(listed["state"], "unapproved", "{listed:#}");
    assert_eq!(listed["disabled"]["by"], "core", "{listed:#}");
}

fn list_entry(f: &common::Fixture) -> Value {
    let listed = f
        .module
        .handle(&Caller::Operator, "flow.list", json!({}))
        .expect("list");
    listed["flows"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["flow_id"] == FLOW).cloned())
        .expect("the flow is listed")
}

#[test]
fn active_with_the_runs_code_hash_activates() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-active");
    install_approved(&f, &fake, &consent, true);
    let run_id = common::admit(&f, FLOW, "one");
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(
        f.module.rt.run(&run_id).expect("run").state,
        RunState::Succeeded
    );
    let calls = status_calls(&fake);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["params"], json!({"flow_id": FLOW, "version": 1}));
    assert_eq!(health_entry(&f)["state"], "enabled");
}

#[test]
fn active_with_another_code_hash_is_revoked() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-mismatch");
    install_approved(&f, &fake, &consent, true);
    fake.installs
        .lock()
        .unwrap()
        .insert((FLOW.into(), 1), ("cd".repeat(32), false));
    let run_id = common::admit(&f, FLOW, "one");
    f.module.engine.run_until_idle(20).expect("idle");
    assert_cancelled_and_unapproved(&f, &run_id);
    let detail = f.module.rt.run(&run_id).unwrap().error_detail.unwrap();
    assert!(detail.contains(&"cd".repeat(32)), "{detail}");
}

/// The contract case: the operator revokes the flow in core; its next
/// scheduled run is never activated, and basal reports the flow as no
/// longer enabled.
#[test]
fn revoked_in_core_the_next_scheduled_run_never_activates() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-revoked");
    install_approved(&f, &fake, &consent, true);
    fake.revoke_install(FLOW, 1);
    f.clock.advance(common::HOUR);
    f.module.engine.run_until_idle(20).expect("idle");
    let runs = f.module.rt.store().read(|c| {
        let mut s = c.prepare("SELECT run_id FROM runs WHERE flow_id = ?1")?;
        let ids = s
            .query_map([FLOW], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    });
    let runs = runs.expect("runs");
    assert_eq!(runs.len(), 1, "the schedule fired once");
    assert_cancelled_and_unapproved(&f, &runs[0]);
    // The schedule stopped with the disable: the next due time admits
    // nothing.
    f.clock.advance(common::HOUR);
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(status_calls(&fake).len(), 1);
}

/// A core revoke raises no re-enable card (only the runtime's auto-disable
/// does); the flow comes back when the operator approves a newer version
/// through core's card.
#[test]
fn a_core_revoke_raises_no_reenable_card_and_a_new_approval_brings_the_flow_back() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-back");
    install_approved(&f, &fake, &consent, true);
    fake.revoke_install(FLOW, 1);
    let run_id = common::admit(&f, FLOW, "one");
    for _ in 0..3 {
        f.module.engine.run_until_idle(20).expect("idle");
    }
    assert_cancelled_and_unapproved(&f, &run_id);
    assert!(
        f.module.rt.decision_cards().expect("cards").is_empty(),
        "basal recorded no decision card"
    );
    assert!(
        fake.all_decision_cards().is_empty(),
        "core was sent no decision card"
    );

    let mut v2 = manifest();
    v2["version"] = json!(2);
    common::install(&f, &Caller::Operator, "return 2;", &v2);
    let card = fake.calls("elicitation.request")[1]["params"]["flow_install"].clone();
    assert_eq!(card["version"], 2);
    fake.enqueue(
        "elicitation.answers",
        vec![Ok(
            json!({"records":[{"state":"answered","answered_choice_id":"approve","flow_install":card}],"cursor":2}),
        )],
    );
    consent.poll_once().expect("decision applied");
    fake.approve_install(FLOW, 2);
    let listed = list_entry(&f);
    assert_eq!(listed["state"], "enabled", "{listed:#}");
    assert_eq!(listed["approved_version"], 2);
    assert!(listed["disabled"].is_null(), "{listed:#}");
    let next = common::admit(&f, FLOW, "two");
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(
        f.module.rt.run(&next).expect("run").state,
        RunState::Succeeded
    );
}

#[test]
fn unknown_in_core_cancels_the_run() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-unknown");
    install_approved(&f, &fake, &consent, false);
    let run_id = common::admit(&f, FLOW, "one");
    f.module.engine.run_until_idle(20).expect("idle");
    assert_cancelled_and_unapproved(&f, &run_id);
}

/// Core unreachable: the run stays pending, the engine's passes do not ask
/// again inside the backoff (no loop of questions), and once core answers
/// after the backoff the run activates.
#[test]
fn unreachable_core_keeps_the_run_pending_without_a_hot_loop() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-unreachable");
    install_approved(&f, &fake, &consent, true);
    fake.enqueue(
        "flow.install_status",
        vec![
            Err(WireError::Unknown("timed out".into())),
            Err(WireError::NeverSent("connection refused".into())),
        ],
    );
    let run_id = common::admit(&f, FLOW, "one");
    f.module
        .engine
        .run_until_idle(20)
        .expect("a deferred run is not progress, so the engine goes idle");
    for _ in 0..10 {
        f.module.engine.pass().expect("pass");
        f.module.engine.wait_quiet();
    }
    assert_eq!(
        status_calls(&fake).len(),
        1,
        "asked once inside the backoff"
    );
    let run = f.module.rt.run(&run_id).expect("run");
    assert_eq!(run.state, RunState::Pending, "{run:#?}");
    assert_eq!(run.error_kind, None);

    f.clock.advance(1_000);
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(status_calls(&fake).len(), 2);
    assert_eq!(f.module.rt.run(&run_id).unwrap().state, RunState::Pending);

    f.clock.advance(2_000);
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(status_calls(&fake).len(), 3);
    assert_eq!(f.module.rt.run(&run_id).unwrap().state, RunState::Succeeded);
}

/// A reply basal cannot read is no answer: the run waits, nothing is
/// revoked.
#[test]
fn an_undecodable_reply_defers_the_run() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "gate-wire-undecodable");
    install_approved(&f, &fake, &consent, true);
    fake.enqueue("flow.install_status", vec![Ok(json!({"state": "active"}))]);
    let run_id = common::admit(&f, FLOW, "one");
    f.module.engine.run_until_idle(20).expect("idle");
    assert_eq!(f.module.rt.run(&run_id).unwrap().state, RunState::Pending);
    assert_eq!(health_entry(&f)["state"], "enabled");
}

/// Core's reply decodes only as `{state, code_hash}` with `state` one of
/// `active`, `revoked` (each with a 64-digit lowercase hex hash) or
/// `unknown` (with a null hash); a refusal is no answer.
#[test]
fn install_status_replies_decode_strictly() {
    let hash = "0123456789abcdef".repeat(4);
    assert_eq!(
        decode_install_status(&json!({"state":"active","code_hash":hash})),
        Ok(InstallStatus::Active {
            code_hash: hash.clone(),
            scope: None,
        })
    );
    assert_eq!(
        decode_install_status(&json!({"state":"revoked","code_hash":hash})),
        Ok(InstallStatus::Revoked {
            code_hash: hash.clone()
        })
    );
    assert_eq!(
        decode_install_status(&json!({"state":"unknown","code_hash":null})),
        Ok(InstallStatus::Unknown)
    );
    for bad in [
        json!({"state":"active"}),
        json!({"state":"active","code_hash":null}),
        json!({"state":"active","code_hash":hash.to_uppercase()}),
        json!({"state":"active","code_hash":"abc"}),
        json!({"state":"revoked"}),
        json!({"state":"unknown","code_hash":hash}),
        json!({"state":"superseded","code_hash":hash}),
        json!({"code_hash":hash}),
        json!(null),
    ] {
        assert!(decode_install_status(&bad).is_err(), "{bad}");
    }
    // A refusal from core is no answer either.
    let fake = Fake::new();
    fake.enqueue(
        "flow.install_status",
        vec![Err(WireError::Refused {
            code: "elicitation_requester_refused".into(),
            message: "only basal".into(),
        })],
    );
    assert!(CoreHost::new(fake.clone()).install_status(FLOW, 1).is_err());
}

/// `placement` is left out of the `flow_install` request when the manifest
/// states none (core renders "not stated"), sent when it does, and never
/// sent empty. The fake refuses an empty one as core does.
#[test]
fn placement_is_sent_only_when_the_manifest_states_one() {
    let fake = Fake::new();
    let (f, _consent) = gated(&fake, "gate-wire-placement");
    common::install(&f, &Caller::Operator, SCRIPT, &manifest());
    let sent = fake.calls("elicitation.request")[0]["params"]["flow_install"].clone();
    assert!(sent.get("placement").is_none(), "{sent:#}");

    let mut placed = manifest();
    placed["id"] = json!("placed-flow");
    placed["placement"] = json!("machine:local");
    common::install(&f, &Caller::Operator, SCRIPT, &placed);
    let sent = fake.calls("elicitation.request")[1]["params"]["flow_install"].clone();
    assert_eq!(sent["placement"], "machine:local");

    let mut empty = fake.calls("elicitation.request")[1]["params"].clone();
    empty["flow_install"]["placement"] = json!("");
    let refused = fake.management(CORE, "elicitation.request", empty);
    assert!(
        matches!(&refused, Err(WireError::Refused { code, .. }) if code == "elicitation_invalid_request"),
        "{refused:?}"
    );
}
