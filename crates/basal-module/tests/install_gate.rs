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
    gated_with_options(fake, tag, common::Options::default())
}

fn gated_with_worker(fake: &Arc<Fake>, tag: &str) -> (common::Fixture, Arc<CoreConsent>) {
    gated_with_options(fake, tag, common::Options::with_worker())
}

fn gated_with_options(
    fake: &Arc<Fake>,
    tag: &str,
    options: common::Options,
) -> (common::Fixture, Arc<CoreConsent>) {
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let catalog = Arc::new(SubcCatalog::new(fake.clone()));
    let hosts = Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
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
            ..options
        },
    );
    (f, consent)
}

/// Seeds local approval so these tests isolate activation-time status checks.
/// `in_core` says whether core also holds the approved install.
fn install_approved(f: &common::Fixture, fake: &Fake, _consent: &CoreConsent, in_core: bool) {
    let reply = common::install(f, &Caller::Operator, SCRIPT, &manifest());
    f.module
        .rt
        .decide_card(
            reply["card_id"].as_str().unwrap(),
            basal_core::cards::Decision::Approve,
            "fixture",
        )
        .unwrap();
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

fn revoke_hint(
    f: &common::Fixture,
    caller: &Caller,
    flow: &str,
) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(caller, "flow.install_revoked", json!({
        "flow_id":flow, "version":1,
        "code_hash":basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest().to_string()))
    }))
}

#[test]
fn revocation_hint_checks_status_and_ignores_active() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "revoke-active");
    install_approved(&f, &fake, &consent, true);
    assert_eq!(revoke_hint(&f, &Caller::Core, FLOW).unwrap(), json!({}));
    assert_eq!(status_calls(&fake).len(), 1);
    assert_eq!(health_entry(&f)["state"], "enabled");
    assert!(
        f.module
            .rt
            .store()
            .read(|c| basal_core::install::revocation(c, FLOW, 1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn revocation_hint_is_core_only_and_strictly_decoded() {
    let fake = Fake::new();
    let (f, _) = gated(&fake, "revoke-caller");
    assert_eq!(
        revoke_hint(&f, &Caller::Operator, FLOW).unwrap_err().code,
        "not_permitted"
    );
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.install_revoked",
                json!({"flow_id":FLOW,"version":0,"code_hash":"bad"})
            )
            .unwrap_err()
            .code,
        "invalid_params"
    );
    assert!(status_calls(&fake).is_empty());
}

#[test]
fn revocation_hint_unknown_and_repeated_are_noops() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "revoke-repeat");
    assert_eq!(
        revoke_hint(&f, &Caller::Core, "missing").unwrap(),
        json!({})
    );
    assert!(status_calls(&fake).is_empty());
    install_approved(&f, &fake, &consent, true);
    fake.revoke_install(FLOW, 1);
    assert_eq!(revoke_hint(&f, &Caller::Core, FLOW).unwrap(), json!({}));
    assert_eq!(revoke_hint(&f, &Caller::Core, FLOW).unwrap(), json!({}));
    assert_eq!(status_calls(&fake).len(), 1);
    assert_eq!(health_entry(&f)["state"], "unapproved");
}

#[test]
fn revocation_hint_never_approved_is_a_noop() {
    let fake = Fake::new();
    let (f, _) = gated(&fake, "revoke-unapproved");
    common::install(&f, &Caller::Operator, SCRIPT, &manifest());
    assert_eq!(revoke_hint(&f, &Caller::Core, FLOW).unwrap(), json!({}));
    assert!(status_calls(&fake).is_empty());
    assert!(
        f.module
            .rt
            .store()
            .read(|c| basal_core::install::revocation(c, FLOW, 1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn revocation_hint_unknown_status_and_mismatched_core_hash_revoke() {
    for status in [
        json!({"state":"unknown","code_hash":null}),
        json!({"state":"active","code_hash":"cd".repeat(32)}),
    ] {
        let fake = Fake::new();
        let (f, consent) = gated(&fake, "revoke-status");
        install_approved(&f, &fake, &consent, true);
        fake.enqueue("flow.install_status", vec![Ok(status)]);
        assert_eq!(revoke_hint(&f, &Caller::Core, FLOW).unwrap(), json!({}));
        assert_eq!(status_calls(&fake).len(), 1);
        assert_eq!(health_entry(&f)["state"], "unapproved");
    }
}

#[test]
fn revocation_hint_unreachable_is_retryable_and_changes_nothing() {
    let fake = Fake::new();
    let (f, consent) = gated(&fake, "revoke-offline");
    install_approved(&f, &fake, &consent, true);
    fake.enqueue(
        "flow.install_status",
        vec![Err(WireError::NeverSent("offline".into()))],
    );
    let error = revoke_hint(&f, &Caller::Core, FLOW).unwrap_err();
    assert_eq!(error.code, "core_unavailable");
    assert_eq!(error.detail, Some(json!({"retryable":true})));
    assert_eq!(status_calls(&fake).len(), 1);
    assert_eq!(health_entry(&f)["state"], "enabled");
    assert!(
        f.module
            .rt
            .store()
            .read(|c| basal_core::install::revocation(c, FLOW, 1))
            .unwrap()
            .is_none()
    );
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
    let (f, consent) = gated_with_worker(&fake, "gate-wire-active");
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
    let (f, consent) = gated_with_worker(&fake, "gate-wire-revoked");
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
    let (f, consent) = gated_with_worker(&fake, "gate-wire-back");
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
    let reply = common::install(&f, &Caller::Operator, "return 2;", &v2);
    let card = fake.calls("flow.install_request")[1]["params"]["flow_install"].clone();
    assert_eq!(card["version"], 2);
    f.module
        .rt
        .decide_card(
            reply["card_id"].as_str().unwrap(),
            basal_core::cards::Decision::Approve,
            "fixture",
        )
        .unwrap();
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

#[test]
fn unapproved_core_states_close_the_activation_gate_like_unknown() {
    for state in ["pending", "declined", "expired", "rejected"] {
        let fake = Fake::new();
        let (f, consent) = gated(&fake, "gate-not-approved");
        install_approved(&f, &fake, &consent, true);
        fake.enqueue("flow.install_status", vec![Ok(json!({"state":state,"code_hash":basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest().to_string()))}))]);
        let run = common::admit(&f, FLOW, "one");
        f.module.engine.run_until_idle(20).unwrap();
        assert_cancelled_and_unapproved(&f, &run);
        assert_eq!(status_calls(&fake).len(), 1);
    }
}

/// Core unreachable: the run stays pending, the engine's passes do not ask
/// again inside the backoff (no loop of questions), and once core answers
/// after the backoff the run activates.
#[test]
fn unreachable_core_keeps_the_run_pending_without_a_hot_loop() {
    let fake = Fake::new();
    let (f, consent) = gated_with_worker(&fake, "gate-wire-unreachable");
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

/// `flow.install_status` accepts only these `state` values. Active, revoked,
/// pending, executed, declined and expired states require a valid hash; rejected
/// may omit the hash, while unknown must have no hash.
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
    for (state, expected) in [
        (
            "pending",
            InstallStatus::Pending {
                code_hash: hash.clone(),
            },
        ),
        (
            "declined",
            InstallStatus::Declined {
                code_hash: hash.clone(),
            },
        ),
        (
            "expired",
            InstallStatus::Expired {
                code_hash: hash.clone(),
            },
        ),
    ] {
        assert_eq!(
            decode_install_status(&json!({"state":state,"code_hash":hash,"attempt":1})),
            Ok(expected)
        );
        for bad in [
            json!({"state":state}),
            json!({"state":state,"code_hash":null}),
            json!({"state":state,"code_hash":"bad"}),
        ] {
            assert!(decode_install_status(&bad).is_err());
        }
    }
    assert_eq!(
        decode_install_status(&json!({
            "state":"rejected",
            "code_hash":hash,
            "attempt":7,
            "rejected_reason":"store_decode",
            "disposition":"approved_not_executed",
            "scope":{
                "owner":{"kind":"reserved","module_id":"prefrontal-core"},
                "ref":"0123456789abcdef0123456789abcdef",
                "epoch":1
            },
            "flow_scope_targets":["broca"]
        })),
        Ok(InstallStatus::Rejected {
            code_hash: Some(hash.clone()),
            attempt: Some(7),
            rejected_reason: Some("store_decode".into()),
            disposition: Some("approved_not_executed".into()),
        })
    );
    assert_eq!(
        decode_install_status(&json!({"state":"rejected","code_hash":null})),
        Ok(InstallStatus::Rejected {
            code_hash: None,
            attempt: None,
            rejected_reason: None,
            disposition: None,
        })
    );
    for bad in [
        json!({"state":"rejected","code_hash":"bad"}),
        json!({"state":"rejected","attempt":0}),
        json!({"state":"rejected","rejected_reason":false}),
        json!({"state":"rejected","disposition":"x".repeat(257)}),
    ] {
        assert!(decode_install_status(&bad).is_err(), "{bad}");
    }
    assert_eq!(
        decode_install_status(&json!({"state":"executed","code_hash":hash})),
        Ok(InstallStatus::Pending {
            code_hash: hash.clone(),
        })
    );
    for bad in [
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
    let sent = fake.calls("flow.install_request")[0]["params"]["flow_install"].clone();
    assert!(sent.get("placement").is_none(), "{sent:#}");

    let mut placed = manifest();
    placed["id"] = json!("placed-flow");
    placed["placement"] = json!("machine:local");
    common::install(&f, &Caller::Operator, SCRIPT, &placed);
    let sent = fake.calls("flow.install_request")[1]["params"]["flow_install"].clone();
    assert_eq!(sent["placement"], "machine:local");

    let mut empty = fake.calls("flow.install_request")[1]["params"].clone();
    empty["flow_install"]["placement"] = json!("");
    let refused = fake.management(CORE, "flow.install_request", empty);
    assert!(
        matches!(&refused, Err(WireError::Refused { code, .. }) if code == "elicitation_invalid_request"),
        "{refused:?}"
    );
}
