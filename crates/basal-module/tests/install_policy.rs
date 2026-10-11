//! Policy installs use production consent/status JSON encoders and decoders
//! against the fake core transport.

mod common;
mod wire;

use std::sync::Arc;

use basal_core::{InstallGate, InstallRequest, NoHooks, Store, install};
use basal_host::cingulate_consent::CingulateConsent;
use basal_host::core_consent::{CoreConsent, decode_install_outcome};
use basal_host::core_host::CoreHost;
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_host::{Consent, InstallOutcome};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::unconfigured::UnconfiguredHost;
use rusqlite::params;
use serde_json::{Value, json};
use wire::Fake;

const FLOW: &str = "policy-flow";
const RULE: &str = "head-self-within-limits";
const SCRIPT: &str = "return 1;";

fn manifest(id: &str, version: u32) -> String {
    json!({"id":id,"version":version,"purpose":"Report to Synapse","trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"SYNAPSE","digest_max":"piggyback"}],"ops":[{"module":"mock","op":"echo"}]}).to_string()
}

fn hash(id: &str, version: u32) -> String {
    basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest(id, version)))
}

fn hosts(fake: &Arc<Fake>, catalog_consent: bool) -> Hosts {
    hosts_transport(fake.clone(), catalog_consent)
}

fn hosts_transport(transport: Arc<dyn Transport>, catalog_consent: bool) -> Hosts {
    let catalog = Arc::new(SubcCatalog::new(transport.clone()));
    Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(transport.clone(), catalog.clone())),
            Arc::new(CoreHost::new(transport.clone())),
            Arc::new(UnconfiguredHost::new()),
        )),
        catalog,
        consent: if catalog_consent {
            Arc::new(CingulateConsent::new(transport)) as Arc<dyn Consent>
        } else {
            Arc::new(CoreConsent::new(transport))
        },
        hooks: Arc::new(NoHooks),
    }
}

fn fixture(fake: &Arc<Fake>, tag: &str) -> common::Fixture {
    common::fixture(
        tag,
        common::Options {
            hosts: Some(hosts(fake, false)),
            install_gate: InstallGate::Core,
            ..common::Options::default()
        },
    )
}

fn install_version(f: &common::Fixture, version: u32) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        "flow.install",
        json!({"script":SCRIPT,"manifest":manifest(FLOW,version)}),
    )
}

fn policy_reply(hash: &str) -> Value {
    json!({"outcome":"approved_by_policy","rule":RULE,"code_hash":hash})
}

fn active(hash: &str) -> Value {
    json!({"state":"active","code_hash":hash,"approval":{"kind":"policy","rule":RULE}})
}

fn entry(f: &common::Fixture, op: &str, id: &str) -> Value {
    let reply = f.module.handle(&Caller::Operator, op, json!({})).unwrap();
    reply["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["flow_id"] == id)
        .unwrap()
        .clone()
}

fn assert_pending(f: &common::Fixture) {
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
    let health = entry(f, "flow.health", FLOW);
    assert_eq!(health["state"], "unapproved");
    assert_eq!(health["install_checks"].as_array().unwrap().len(), 1);
    assert!(
        !health["install_checks"][0]["error"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(entry(f, "flow.list", FLOW)["pending_version"], 1);
}

#[test]
fn declares_flow_install_policy_capability() {
    let manifest = basal_module::manifest::manifest();
    assert!(
        manifest
            .capabilities
            .unwrap()
            .provides
            .iter()
            .any(|cap| cap == "flow-install-policy/v1")
    );
}

#[test]
fn card_raised_preserves_the_legacy_reply_and_card_decision() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-card");
    fake.enqueue(
        "elicitation.request",
        vec![Ok(
            json!({"outcome":"card_raised","elicitation_id":"el-policy"}),
        )],
    );
    let first = install_version(&f, 1).unwrap();
    assert_eq!(first["state"], "pending");
    assert!(first["card_id"].is_string());
    assert!(first.get("approval").is_none());
    assert!(fake.calls("flow.install_status").is_empty());
    // Repeating the install with a reply containing only `elicitation_id`
    // changes only the `new` field.
    fake.enqueue(
        "elicitation.request",
        vec![Ok(json!({"elicitation_id":"el-policy"}))],
    );
    let mut repeated = install_version(&f, 1).unwrap();
    assert_eq!(repeated["new"], false);
    repeated["new"] = json!(true);
    assert_eq!(first, repeated);
    let card_id = first["card_id"].as_str().unwrap();
    f.module
        .rt
        .decide_card(card_id, basal_core::cards::Decision::Approve, "operator")
        .unwrap();
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        Some(1)
    );
    assert_eq!(
        entry(&f, "flow.list", FLOW)["approval"],
        json!({"kind":"card"})
    );
    assert!(fake.calls("flow.install_status").is_empty());
}

#[test]
fn matching_policy_reply_and_active_status_approve_on_both_consent_paths() {
    for catalog_consent in [false, true] {
        let fake = Fake::new();
        let f = common::fixture(
            "policy-active",
            common::Options {
                hosts: Some(hosts(&fake, catalog_consent)),
                install_gate: InstallGate::Core,
                ..common::Options::default()
            },
        );
        fake.enqueue(
            "elicitation.request",
            vec![Ok(policy_reply(&hash(FLOW, 1)))],
        );
        fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 1)))]);
        let reply = install_version(&f, 1).unwrap();
        assert_eq!(reply["state"], "approved");
        assert!(reply["card_id"].is_null());
        assert_eq!(reply["approval"], json!({"kind":"policy","rule":RULE}));
        assert_eq!(
            f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
            Some(1)
        );
        assert!(!f.module.rt.has_open_cards().unwrap());
        assert_eq!(fake.calls("flow.install_status").len(), 1);
        assert_eq!(
            fake.calls("flow.install_status")[0]["params"],
            json!({"flow_id":FLOW,"version":1})
        );
        for op in ["flow.list", "flow.health"] {
            let row = entry(&f, op, FLOW);
            assert_eq!(row["approval"], json!({"kind":"policy","rule":RULE}));
            assert_eq!(row["install_checks"], json!([]));
        }
        assert_eq!(
            f.module.rt.outbox().unwrap().last().unwrap().kind,
            "flow.install_approved"
        );
    }
}

#[test]
fn mismatched_policy_reply_hash_never_asks_status_or_approves() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-reply-mismatch");
    fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&"ab".repeat(32)))],
    );
    fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 1)))]);
    assert_eq!(
        install_version(&f, 1).unwrap_err().code,
        "install_policy_unconfirmed"
    );
    assert_pending(&f);
    assert!(fake.calls("flow.install_status").is_empty());
}

#[test]
fn policy_status_must_be_active_with_the_local_hash() {
    for reply in [
        json!({"state":"unknown","code_hash":null}),
        json!({"state":"revoked","code_hash":hash(FLOW,1)}),
        active(&"ab".repeat(32)),
        json!({"state":"active","code_hash":"malformed"}),
    ] {
        let fake = Fake::new();
        let f = fixture(&fake, "policy-status-refused");
        fake.enqueue(
            "elicitation.request",
            vec![Ok(policy_reply(&hash(FLOW, 1)))],
        );
        fake.enqueue("flow.install_status", vec![Ok(reply)]);
        assert_eq!(
            install_version(&f, 1).unwrap_err().code,
            "install_policy_unconfirmed"
        );
        assert_pending(&f);
        assert_eq!(fake.calls("flow.install_status").len(), 1);
    }
}

#[test]
fn unknown_install_outcome_is_an_error_even_with_a_valid_hash_or_card_id() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-unknown");
    let reply = json!({"outcome":"approved_maybe","elicitation_id":"el-unknown","rule":RULE,"code_hash":hash(FLOW,1)});
    fake.enqueue("elicitation.request", vec![Ok(reply.clone())]);
    assert!(decode_install_outcome(&reply).is_err());
    assert_eq!(
        install_version(&f, 1).unwrap_err().code,
        "consent_unavailable"
    );
    assert_pending(&f);
    assert!(fake.calls("flow.install_status").is_empty());
    for reply in [
        json!({"outcome":"approved_by_policy","rule":RULE}),
        json!({"outcome":"approved_by_policy","code_hash":hash(FLOW,1)}),
        json!({"outcome":"card_raised"}),
        json!({"outcome":null,"elicitation_id":"el"}),
    ] {
        assert!(decode_install_outcome(&reply).is_err());
    }
    assert_eq!(
        decode_install_outcome(&policy_reply(&hash(FLOW, 1))).unwrap(),
        InstallOutcome::ApprovedByPolicy {
            rule: RULE.into(),
            code_hash: hash(FLOW, 1)
        }
    );
}

fn seed(store: &Store, id: &str, card: bool, confirmed: bool) {
    let request = InstallRequest {
        script: SCRIPT.into(),
        manifest: manifest(id, 1),
        author: "operator".into(),
        loop_override: false,
    };
    let parsed = basal_core::manifest::Manifest::parse(&request.manifest).unwrap();
    store
        .write(|tx| {
            let installed = install::record(tx, &parsed, &request, vec![], 0).unwrap();
            if card {
                basal_core::cards::record(tx, id, 1, &installed.code_hash, "operator", "{}", 0)?;
            }
            tx.execute(
                "UPDATE installs SET card_confirmed=?2 WHERE flow_id=?1",
                params![id, confirmed],
            )?;
            Ok(())
        })
        .unwrap();
}

struct StartupTransport {
    fake: Arc<Fake>,
}

impl Transport for StartupTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        self.fake.catalog()
    }
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError> {
        self.fake.tool(module, name, arguments, call_key)
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        let reply = self.fake.management(module, op, params.clone());
        if op != "flow.install_status" {
            return reply;
        }
        let id = params["flow_id"].as_str().unwrap();
        // Reply by flow identity, not query order: an extra query must expose
        // the wrong eligibility decision, not shift another flow's fixture.
        Ok(match id {
            "unknown" => json!({"state":"unknown","code_hash":null}),
            "revoked" => json!({"state":"revoked","code_hash":hash(id,1)}),
            "wrong-hash" => active(&"ab".repeat(32)),
            "legacy" => json!({"state":"active","code_hash":hash(id,1)}),
            "card-provenance" => {
                json!({"state":"active","code_hash":hash(id,1),"approval":{"kind":"card"}})
            }
            _ => active(&hash(id, 1)),
        })
    }
}

#[test]
fn startup_checks_only_pending_versions_without_confirmed_cards_once() {
    let fake = Fake::new();
    let ids = [
        "active",
        "saved-request",
        "legacy",
        "card-provenance",
        "unknown",
        "revoked",
        "wrong-hash",
        "confirmed-card",
        "rejected",
        "approved",
    ];
    let f = common::fixture_with_store(
        "policy-startup",
        common::Options {
            hosts: Some(hosts_transport(
                Arc::new(StartupTransport { fake: fake.clone() }),
                false,
            )),
            install_gate: InstallGate::Core,
            ..common::Options::default()
        },
        |store| {
            for id in ids {
                seed(
                    &store,
                    id,
                    id != "active" && id != "legacy" && id != "card-provenance",
                    id == "confirmed-card",
                );
            }
            store
                .write(|tx| {
                    tx.execute(
                        "UPDATE install_cards SET state='rejected',decided_by='operator',decided_at=0 WHERE flow_id='rejected'",
                        [],
                    )?;
                    install::approve(
                        tx,
                        "approved",
                        1,
                        &basal_core::ids::code_hash(SCRIPT, &manifest("approved", 1)),
                        "old",
                        common::T0,
                        &Default::default(),
                    )
                    .unwrap();
                    Ok(())
                })
                .unwrap();
            Ok(())
        },
    );
    for id in [
        "active",
        "saved-request",
        "legacy",
        "card-provenance",
        "approved",
    ] {
        assert_eq!(
            f.module.rt.flow(id).unwrap().unwrap().approved_version,
            Some(1),
            "{id}"
        );
    }
    for id in [
        "unknown",
        "revoked",
        "wrong-hash",
        "confirmed-card",
        "rejected",
    ] {
        assert_eq!(
            f.module.rt.flow(id).unwrap().unwrap().approved_version,
            None,
            "{id}"
        );
    }
    assert_eq!(
        entry(&f, "flow.list", "active")["approval"],
        json!({"kind":"policy","rule":RULE})
    );
    assert_eq!(
        entry(&f, "flow.health", "saved-request")["approval"],
        json!({"kind":"policy","rule":RULE})
    );
    assert_eq!(
        entry(&f, "flow.list", "legacy")["approval"],
        json!({"kind":"unknown"})
    );
    assert_eq!(
        entry(&f, "flow.list", "card-provenance")["approval"],
        json!({"kind":"card"})
    );
    let checked: Vec<_> = fake
        .calls("flow.install_status")
        .iter()
        .map(|call| call["params"]["flow_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        checked,
        [
            "active",
            "card-provenance",
            "legacy",
            "revoked",
            "saved-request",
            "unknown",
            "wrong-hash"
        ]
    );
    for _ in 0..5 {
        f.module.engine.run_until_idle(10).unwrap();
    }
    assert_eq!(fake.calls("flow.install_status").len(), 7);
}

#[test]
fn idle_policy_install_has_zero_status_calls_after_confirmation() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-idle");
    fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&hash(FLOW, 1)))],
    );
    fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 1)))]);
    install_version(&f, 1).unwrap();
    let before = fake.calls("flow.install_status").len();
    assert_eq!(before, 1);
    for _ in 0..5 {
        f.module.engine.run_until_idle(10).unwrap();
        entry(&f, "flow.list", FLOW);
        entry(&f, "flow.health", FLOW);
    }
    assert_eq!(fake.calls("flow.install_status").len(), before);

    // A pending install must not cause idle retries of install_status.
    let pending_fake = Fake::new();
    let pending = fixture(&pending_fake, "policy-idle-pending");
    pending_fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&hash(FLOW, 1)))],
    );
    pending_fake.enqueue(
        "flow.install_status",
        vec![Ok(json!({"state":"unknown","code_hash":null}))],
    );
    assert!(install_version(&pending, 1).is_err());
    let before = pending_fake.calls("flow.install_status").len();
    assert_eq!(before, 1);
    for _ in 0..5 {
        pending.module.engine.run_until_idle(10).unwrap();
    }
    assert_eq!(pending_fake.calls("flow.install_status").len(), before);
}

#[test]
fn policy_approval_uses_card_version_schedule_and_core_disable_rules() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-version-rules");
    let first = install_version(&f, 1).unwrap();
    f.module
        .rt
        .decide_card(
            first["card_id"].as_str().unwrap(),
            basal_core::cards::Decision::Approve,
            "operator",
        )
        .unwrap();
    let old_run = common::admit(&f, FLOW, "old");
    f.module
        .rt
        .disable_flow(FLOW, &basal_core::Actor::Core, "install_revoked")
        .unwrap();
    fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&hash(FLOW, 2)))],
    );
    fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 2)))]);
    install_version(&f, 2).unwrap();
    let flow = f.module.rt.flow(FLOW).unwrap().unwrap();
    assert_eq!(flow.approved_version, Some(2));
    assert!(flow.enabled);
    assert!(flow.disabled_by.is_none());
    let new_run = common::admit(&f, FLOW, "new");
    assert_eq!(f.module.rt.run(&old_run).unwrap().flow_version, Some(1));
    assert_eq!(f.module.rt.run(&new_run).unwrap().flow_version, Some(2));
    f.module
        .rt
        .store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT state FROM installs WHERE flow_id=?1 AND version=1",
                    [FLOW],
                    |r| r.get::<_, String>(0)
                )?,
                "superseded"
            );
            assert_eq!(
                basal_core::schedule::table::load(c, FLOW)?.unwrap().version,
                2
            );
            Ok(())
        })
        .unwrap();
    f.module
        .rt
        .disable_flow(
            FLOW,
            &basal_core::Actor::Operator("operator".into()),
            "operator_stop",
        )
        .unwrap();
    fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&hash(FLOW, 3)))],
    );
    fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 3)))]);
    install_version(&f, 3).unwrap();
    assert!(!f.module.rt.flow(FLOW).unwrap().unwrap().enabled);
}

#[test]
fn policy_provenance_and_approval_roll_back_in_one_transaction() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-atomic");
    let first = install_version(&f, 1).unwrap();
    f.module
        .rt
        .decide_card(
            first["card_id"].as_str().unwrap(),
            basal_core::cards::Decision::Approve,
            "operator",
        )
        .unwrap();
    f.module
        .rt
        .disable_flow(FLOW, &basal_core::Actor::Core, "install_revoked")
        .unwrap();
    f.module.rt.store().write(|tx| {
        tx.execute_batch("CREATE TRIGGER refuse_policy_provenance BEFORE UPDATE OF approval_rule ON installs WHEN NEW.approval_rule IS NOT NULL BEGIN SELECT RAISE(ABORT,'provenance unavailable'); END;")?;
        Ok(())
    }).unwrap();
    fake.enqueue(
        "elicitation.request",
        vec![Ok(policy_reply(&hash(FLOW, 2)))],
    );
    fake.enqueue("flow.install_status", vec![Ok(active(&hash(FLOW, 2)))]);
    assert!(install_version(&f, 2).is_err());
    let flow = f.module.rt.flow(FLOW).unwrap().unwrap();
    assert_eq!(flow.approved_version, Some(1));
    assert!(!flow.enabled);
    assert_eq!(
        f.module.rt.cards(FLOW).unwrap().last().unwrap().state,
        basal_core::cards::CardState::Pending
    );
    f.module.rt.store().read(|c| {
        let row: (String,Option<String>,Option<String>) = c.query_row("SELECT state,approval_ref,approval_rule FROM installs WHERE flow_id=?1 AND version=2", [FLOW], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(row,("validated".into(),None,None));
        assert_eq!(c.query_row("SELECT state FROM installs WHERE flow_id=?1 AND version=1",[FLOW],|r|r.get::<_,String>(0))?,"approved");
        assert_eq!(basal_core::schedule::table::load(c,FLOW)?.unwrap().version,1);
        Ok(())
    }).unwrap();
}
