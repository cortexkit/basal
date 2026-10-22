//! Policy installs use production consent/status JSON encoders and decoders
//! against the fake core transport.

mod common;
mod wire;

use std::sync::{Arc, Mutex};

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

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl tracing::Subscriber for CapturedLogs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), fields.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn head() -> Caller {
    Caller::Agent {
        agent_id: "ag_synapse".into(),
        scope_ref: wire::LIVE_SYNAPSE_SCOPES[0].into(),
    }
}

fn manifest(id: &str, version: u32) -> String {
    json!({"id":id,"version":version,"purpose":"Report to Synapse","trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"SYNAPSE","digest_max":"piggyback"}],"ops":[{"module":"mock","op":"echo"}]}).to_string()
}

fn audience_manifest(audience: Value) -> String {
    let mut value: Value = serde_json::from_str(&manifest(FLOW, 1)).unwrap();
    value["audience"] = audience;
    value["sinks"][0]["agent"] = json!("$audience");
    value.to_string()
}

fn install_audience(
    f: &common::Fixture,
    caller: &Caller,
    manifest: &str,
) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        caller,
        "flow.install",
        json!({"script":SCRIPT,"manifest":manifest}),
    )
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
fn card_raised_without_elicitation_id_is_keyed_on_the_triple() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-card");
    fake.enqueue(
        "flow.install_request",
        vec![Ok(
            json!({"outcome":"card_raised","flow_id":FLOW,"version":1,"attempt":7}),
        )],
    );
    let first = install_version(&f, 1).unwrap();
    assert_eq!(first["state"], "pending");
    assert!(first["card_id"].is_string());
    assert!(first.get("approval").is_none());
    assert!(fake.calls("flow.install_status").is_empty());
    assert_eq!(first["attempt"], 7);
    assert_eq!(f.module.rt.cards(FLOW).unwrap()[0].attempt, Some(7));
    // Repeating core's same card key changes only the `new` field.
    fake.enqueue(
        "flow.install_request",
        vec![Ok(
            json!({"outcome":"card_raised","flow_id":FLOW,"version":1,"attempt":7}),
        )],
    );
    let mut repeated = install_version(&f, 1).unwrap();
    assert_eq!(repeated["new"], false);
    repeated["new"] = json!(true);
    assert_eq!(first, repeated);
    assert!(fake.calls("flow.install_status").is_empty());
    assert!(decode_install_outcome(&json!({"elicitation_id":"retired"})).is_err());
}

fn raised_fixture(tag: &str) -> (Arc<Fake>, common::Fixture) {
    let fake = Fake::new();
    let f = fixture(&fake, tag);
    fake.enqueue(
        "flow.install_request",
        vec![Ok(
            json!({"outcome":"card_raised","flow_id":FLOW,"version":1,"attempt":7}),
        )],
    );
    install_version(&f, 1).unwrap();
    (fake, f)
}

fn hint(
    f: &common::Fixture,
    caller: &Caller,
    attempt: i64,
) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        caller,
        "flow.install_decided",
        json!({"flow_id":FLOW,"version":1,"attempt":attempt}),
    )
}

#[test]
fn agent_global_install_waits_for_core_card_answer() {
    let fake = Fake::new();
    let f = fixture(&fake, "agent-global-card");
    let manifest = audience_manifest(json!({"kind":"global"}));
    let hash = basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest));
    let reply = install_audience(&f, &head(), &manifest).unwrap();
    assert_eq!(reply["state"], "pending");
    assert!(reply["card_id"].is_string());
    let requests = fake.calls("flow.install_request");
    assert_eq!(requests.len(), 1);
    let sent_manifest: Value = serde_json::from_str(
        requests[0]["params"]["flow_install"]["manifest_json"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sent_manifest["audience"], json!({"kind":"global"}));
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
    assert!(f.module.rt.outbox().unwrap().is_empty());
    let attempt = reply["attempt"].as_i64().unwrap();
    fake.enqueue(
        "flow.install_status",
        vec![Ok(
            json!({"state":"pending","code_hash":hash,"attempt":attempt}),
        )],
    );
    hint(&f, &Caller::Core, attempt).unwrap();
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
    fake.enqueue(
        "flow.install_status",
        vec![Ok(
            json!({"state":"active","code_hash":hash,"approval":{"kind":"card"}}),
        )],
    );
    hint(&f, &Caller::Core, attempt).unwrap();
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        Some(1)
    );
    assert_eq!(
        entry(&f, "flow.health", FLOW)["approval"],
        json!({"kind":"card"})
    );
    assert_eq!(fake.calls("flow.install_status").len(), 2);
}

fn assert_global_policy_refused(f: &common::Fixture) {
    assert_pending(f);
    let health = entry(f, "flow.health", FLOW);
    let error = health["install_checks"][0]["error"].as_str().unwrap();
    for text in [
        "global audience requires a card approval",
        RULE,
        FLOW,
        "version 1",
    ] {
        assert!(error.contains(text), "{error}");
    }
    assert!(f.module.rt.outbox().unwrap().is_empty());
    assert!(
        f.module
            .rt
            .store()
            .read(|c| basal_core::schedule::table::load(c, FLOW))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.module.rt.cards(FLOW).unwrap()[0].state,
        basal_core::cards::CardState::Pending
    );
}

#[test]
fn global_policy_approval_is_refused_live() {
    for caller in [Caller::Operator, Caller::Local, head()] {
        for policy_reply_first in [true, false] {
            let fake = Fake::new();
            let f = fixture(&fake, "global-policy-live");
            let manifest = audience_manifest(json!({"kind":"global"}));
            let hash = basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest));
            if policy_reply_first {
                fake.enqueue("flow.install_request", vec![Ok(policy_reply(&hash))]);
                // Even a card status cannot legitimize the policy reply's global reach.
                fake.enqueue(
                    "flow.install_status",
                    vec![Ok(
                        json!({"state":"active","code_hash":hash,"approval":{"kind":"card"}}),
                    )],
                );
                assert_eq!(
                    install_audience(&f, &caller, &manifest).unwrap_err().code,
                    "install_policy_unconfirmed"
                );
            } else {
                let reply = install_audience(&f, &caller, &manifest).unwrap();
                fake.enqueue("flow.install_status", vec![Ok(active(&hash))]);
                hint(&f, &Caller::Core, reply["attempt"].as_i64().unwrap()).unwrap();
            }
            assert_global_policy_refused(&f);
            assert_eq!(fake.calls("flow.install_status").len(), 1);
        }
    }
}

#[test]
fn self_and_workspace_audiences_accept_policy_approval() {
    for audience in [
        json!({"kind":"agent","id":"ag_synapse"}),
        json!({"kind":"workspace","id":"team"}),
    ] {
        let fake = Fake::new();
        fake.catalog.lock().unwrap()["modules"].as_array_mut().unwrap().push(json!({"module_id":"entorhinal","roles":[{"role":"management_surface","operations":[{"name":"enumerate","kind":"query"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}));
        fake.enqueue(
            "enumerate",
            vec![Ok(json!({"workspaces":[{"workspaceId":"team"}]}))],
        );
        let f = fixture(&fake, "audience-policy");
        let manifest = audience_manifest(audience);
        let hash = basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest));
        fake.enqueue("flow.install_request", vec![Ok(policy_reply(&hash))]);
        fake.enqueue("flow.install_status", vec![Ok(active(&hash))]);
        let reply = install_audience(&f, &head(), &manifest).unwrap();
        assert_eq!(reply["state"], "approved");
        assert_eq!(reply["approval"], json!({"kind":"policy","rule":RULE}));
        assert_eq!(entry(&f, "flow.health", FLOW)["install_checks"], json!([]));
    }
}

#[test]
fn decided_hint_requires_active_status_with_the_local_hash() {
    for status in [
        json!({"state":"unknown","code_hash":null}),
        json!({"state":"pending","code_hash":hash(FLOW,1),"attempt":7}),
        json!({"state":"revoked","code_hash":hash(FLOW,1)}),
        active(&"ab".repeat(32)),
        json!({"state":"active","code_hash":"bad"}),
    ] {
        let (fake, f) = raised_fixture("hint-not-approved");
        fake.enqueue("flow.install_status", vec![Ok(status)]);
        assert_eq!(hint(&f, &Caller::Core, 7).unwrap(), json!({}));
        assert_eq!(
            f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
            None
        );
        assert_eq!(
            entry(&f, "flow.list", FLOW)["install_cards"][0]["state"],
            "pending"
        );
        assert_eq!(fake.calls("flow.install_status").len(), 1);
        assert!(f.module.rt.outbox().unwrap().is_empty());
    }
}

#[test]
fn decided_hint_confirms_card_or_policy_provenance_in_the_shared_transaction() {
    for approval in [json!({"kind":"card"}), json!({"kind":"policy","rule":RULE})] {
        let (fake, f) = raised_fixture("hint-approved");
        fake.enqueue(
            "flow.install_status",
            vec![Ok(
                json!({"state":"active","code_hash":hash(FLOW,1),"approval":approval}),
            )],
        );
        assert_eq!(hint(&f, &Caller::Core, 7).unwrap(), json!({}));
        assert_eq!(
            f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
            Some(1)
        );
        for op in ["flow.list", "flow.health"] {
            let row = entry(&f, op, FLOW);
            assert_eq!(row["approval"], approval);
            assert_eq!(
                row["install_cards"],
                json!([{"version":1,"attempt":7,"state":"approved","rejected_reason":null,"disposition":null}])
            );
        }
        let schedules: i64 = f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM schedules WHERE flow_id=?1",
                    [FLOW],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(schedules, 1);
        assert!(f.module.rt.outbox().unwrap().is_empty());
    }
}

#[test]
fn declined_and_expired_hints_settle_unapproved_cards_and_are_visible() {
    for state in ["declined", "expired"] {
        let (fake, f) = raised_fixture("hint-terminal");
        fake.enqueue(
            "flow.install_status",
            vec![Ok(
                json!({"state":state,"code_hash":hash(FLOW,1),"attempt":7}),
            )],
        );
        hint(&f, &Caller::Core, 7).unwrap();
        assert_eq!(
            f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
            None
        );
        for op in ["flow.list", "flow.health"] {
            let row = entry(&f, op, FLOW);
            assert_eq!(row["state"], "unapproved");
            assert_eq!(
                row["install_cards"],
                json!([{"version":1,"attempt":7,"state":state,"rejected_reason":null,"disposition":null}])
            );
        }
        assert!(f.module.rt.outbox().unwrap().is_empty());
    }
}

#[test]
fn install_decided_refuses_every_non_core_caller() {
    let (fake, f) = raised_fixture("hint-caller");
    for caller in [
        Caller::Operator,
        Caller::Local,
        common::agent("SYNAPSE"),
        Caller::Module {
            module_id: "cingulate".into(),
        },
        Caller::Other("unknown".into()),
    ] {
        assert_eq!(hint(&f, &caller, 7).unwrap_err().code, "not_permitted");
    }
    assert!(fake.calls("flow.install_status").is_empty());
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
}

#[test]
fn duplicate_and_unknown_hints_are_harmless() {
    for state in ["active", "declined", "expired"] {
        let (fake, f) = raised_fixture("hint-duplicate");
        assert_eq!(hint(&f, &Caller::Core, 6).unwrap(), json!({}));
        assert_eq!(
            f.module
                .handle(
                    &Caller::Core,
                    "flow.install_decided",
                    json!({"flow_id":"absent","version":1,"attempt":7})
                )
                .unwrap(),
            json!({})
        );
        assert!(fake.calls("flow.install_status").is_empty());
        let mut status = json!({"state":state,"code_hash":hash(FLOW,1)});
        if state == "active" {
            status["approval"] = json!({"kind":"card"});
        }
        fake.enqueue("flow.install_status", vec![Ok(status)]);
        hint(&f, &Caller::Core, 7).unwrap();
        let before = f.module.rt.outbox().unwrap().len();
        for _ in 0..3 {
            assert_eq!(hint(&f, &Caller::Core, 7).unwrap(), json!({}));
        }
        assert_eq!(fake.calls("flow.install_status").len(), 1);
        assert_eq!(f.module.rt.outbox().unwrap().len(), before);
        assert_eq!(before, 0);
    }
}

#[test]
fn install_cards_only_use_core_install_request_and_never_wake_answer_polling() {
    for catalog_consent in [false, true] {
        let fake = Fake::new();
        fake.catalog.lock().unwrap()["modules"]
            .as_array_mut()
            .unwrap()
            .push(json!({"module_id":"cingulate","capabilities":{"provides":["consent/v1"]}}));
        let f = common::fixture(
            "install-route",
            common::Options {
                hosts: Some(hosts(&fake, catalog_consent)),
                ..common::Options::default()
            },
        );
        install_version(&f, 1).unwrap();
        assert_eq!(fake.calls("flow.install_request").len(), 1);
        assert_eq!(
            fake.calls_to("prefrontal-core", "flow.install_request")
                .len(),
            1
        );
        assert!(fake.calls("consent.request").is_empty());
        assert!(!f.module.rt.has_open_cards().unwrap());
        assert!(
            !f.module
                .rt
                .has_open_cards_on(&basal_host::DecisionPath::Consent("cingulate".into()))
                .unwrap()
        );
    }
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
            "flow.install_request",
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
        assert!(f.module.rt.outbox().unwrap().is_empty());
    }
}

#[test]
fn mismatched_policy_reply_hash_never_asks_status_or_approves() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-reply-mismatch");
    fake.enqueue(
        "flow.install_request",
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
            "flow.install_request",
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
    fake.enqueue("flow.install_request", vec![Ok(reply.clone())]);
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
    seed_manifest(store, id, &manifest(id, 1), card, confirmed);
}

fn seed_manifest(store: &Store, id: &str, manifest: &str, card: bool, confirmed: bool) {
    let request = InstallRequest {
        script: SCRIPT.into(),
        manifest: manifest.into(),
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
                "UPDATE install_cards SET attempt=?2 WHERE flow_id=?1",
                params![id, if confirmed { Some(7) } else { None }],
            )?;
            Ok(())
        })
        .unwrap();
}

fn record_v3(store: &Store, approve: bool) {
    let manifest = manifest(FLOW, 3);
    let request = InstallRequest {
        script: SCRIPT.into(),
        manifest: manifest.clone(),
        author: "operator".into(),
        loop_override: false,
    };
    let parsed = basal_core::manifest::Manifest::parse(&manifest).unwrap();
    store
        .write(|tx| {
            let installed = install::record(tx, &parsed, &request, vec![], common::T0).unwrap();
            if approve {
                install::approve(
                    tx,
                    FLOW,
                    3,
                    &installed.code_hash,
                    "operator",
                    common::T0,
                    &Default::default(),
                )
                .unwrap();
            } else {
                // This is the on-disk state left by older basal versions, before
                // approving a newer version also closed older pending cards.
                tx.execute(
                    "UPDATE installs SET state='approved', approval_ref='operator', approved_at=?2, approval_kind='card' WHERE flow_id=?1 AND version=3",
                    params![FLOW, common::T0],
                )?;
                tx.execute(
                    "UPDATE flows SET approved_version=3, owner='operator', first_approved_at=?2 WHERE flow_id=?1",
                    params![FLOW, common::T0],
                )?;
                tx.execute(
                    "UPDATE installs SET install_check_error='stale approval remains pending' WHERE flow_id=?1 AND version=1",
                    [FLOW],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn approving_v3_supersedes_an_older_pending_install_card() {
    let fake = Fake::new();
    let f = common::fixture_with_store(
        "approve-supersedes-install-card",
        common::Options {
            hosts: Some(hosts(&fake, false)),
            install_gate: InstallGate::Core,
            ..common::Options::default()
        },
        |store| {
            seed(store.as_ref(), FLOW, true, true);
            record_v3(store.as_ref(), true);
            let states = store
                .read(|conn| {
                    let card: String = conn.query_row(
                        "SELECT state FROM install_cards WHERE flow_id=?1 AND version=1",
                        [FLOW],
                        |row| row.get(0),
                    )?;
                    let install: String = conn.query_row(
                        "SELECT state FROM installs WHERE flow_id=?1 AND version=1",
                        [FLOW],
                        |row| row.get(0),
                    )?;
                    Ok((card, install))
                })
                .unwrap();
            assert_eq!(states, ("stale".into(), "superseded".into()));
            Ok(())
        },
    );

    let cards = f.module.rt.cards(FLOW).unwrap();
    assert_eq!(cards[0].state, basal_core::cards::CardState::Stale);
    assert_eq!(cards[0].decided_by.as_deref(), Some("basal"));
    let states = f
        .module
        .rt
        .store()
        .read(|conn| {
            let v1: String = conn.query_row(
                "SELECT state FROM installs WHERE flow_id=?1 AND version=1",
                [FLOW],
                |row| row.get(0),
            )?;
            let v3: String = conn.query_row(
                "SELECT state FROM installs WHERE flow_id=?1 AND version=3",
                [FLOW],
                |row| row.get(0),
            )?;
            Ok((v1, v3))
        })
        .unwrap();
    assert_eq!(states, ("superseded".into(), "approved".into()));
    assert!(fake.calls("flow.install_status").is_empty());
}

#[test]
fn startup_supersedes_stale_pending_card_without_checking_core_or_warning() {
    let fake = Fake::new();
    let logs = CapturedLogs::default();
    let f = tracing::subscriber::with_default(logs.clone(), || {
        common::fixture_with_store(
            "recover-stale-install-card",
            common::Options {
                hosts: Some(hosts(&fake, false)),
                install_gate: InstallGate::Core,
                ..common::Options::default()
            },
            |store| {
                seed(store.as_ref(), FLOW, true, true);
                record_v3(store.as_ref(), false);
                Ok(())
            },
        )
    });

    let card = f.module.rt.cards(FLOW).unwrap().remove(0);
    assert_eq!(card.state, basal_core::cards::CardState::Stale);
    assert!(fake.calls("flow.install_status").is_empty());
    let state_and_error = f
        .module
        .rt
        .store()
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT state, install_check_error FROM installs WHERE flow_id=?1 AND version=1",
                [FLOW],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(state_and_error, ("superseded".into(), None));
    assert!(!logs.0.lock().unwrap().iter().any(|(level, fields)| {
        *level == tracing::Level::WARN && fields.contains("install approval remains pending")
    }));
}

#[test]
fn global_policy_approval_is_refused_at_startup() {
    for (card, confirmed) in [(false, false), (true, false), (true, true)] {
        let fake = Fake::new();
        let manifest = audience_manifest(json!({"kind":"global"}));
        let hash = basal_core::ids::hex(&basal_core::ids::code_hash(SCRIPT, &manifest));
        let f = common::fixture_with_store(
            "global-policy-startup",
            common::Options {
                hosts: Some(hosts(&fake, false)),
                install_gate: InstallGate::Core,
                ..common::Options::default()
            },
            |store| {
                seed_manifest(&store, FLOW, &manifest, card, confirmed);
                fake.enqueue("flow.install_status", vec![Ok(active(&hash))]);
                Ok(())
            },
        );
        assert_pending(&f);
        let health = entry(&f, "flow.health", FLOW);
        let error = health["install_checks"][0]["error"].as_str().unwrap();
        assert!(
            error.contains("global audience requires a card approval"),
            "{error}"
        );
        assert!(error.contains(RULE), "{error}");
        assert!(f.module.rt.outbox().unwrap().is_empty());
        assert!(
            f.module
                .rt
                .store()
                .read(|c| basal_core::schedule::table::load(c, FLOW))
                .unwrap()
                .is_none()
        );
        if card {
            assert_eq!(
                f.module.rt.cards(FLOW).unwrap()[0].state,
                basal_core::cards::CardState::Pending
            );
        }
        assert_eq!(fake.calls("flow.install_status").len(), 1);
        for _ in 0..3 {
            f.module.engine.run_until_idle(10).unwrap();
        }
        assert_eq!(fake.calls("flow.install_status").len(), 1);
    }
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
fn startup_checks_every_undecided_card_and_pending_policy_once() {
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
        "confirmed-card",
        "approved",
    ] {
        assert_eq!(
            f.module.rt.flow(id).unwrap().unwrap().approved_version,
            Some(1),
            "{id}"
        );
    }
    for id in ["unknown", "revoked", "wrong-hash", "rejected"] {
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
            "confirmed-card",
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
    assert_eq!(fake.calls("flow.install_status").len(), 8);
}

#[test]
fn startup_recovers_declined_expired_and_pending_cards_once() {
    let fake = Fake::new();
    for state in ["declined", "expired", "pending"] {
        let f = common::fixture_with_store(
            "startup-card",
            common::Options {
                hosts: Some(hosts(&fake, false)),
                ..common::Options::default()
            },
            |store| {
                seed(&store, FLOW, true, true);
                fake.enqueue(
                    "flow.install_status",
                    vec![Ok(
                        json!({"state":state,"code_hash":hash(FLOW,1),"attempt":7}),
                    )],
                );
                Ok(())
            },
        );
        assert_eq!(
            f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
            None
        );
        for op in ["flow.list", "flow.health"] {
            assert_eq!(
                entry(&f, op, FLOW)["install_cards"],
                json!([{"version":1,"attempt":7,"state":state,"rejected_reason":null,"disposition":null}])
            );
        }
        let before = fake.calls("flow.install_status").len();
        for _ in 0..5 {
            f.module.engine.run_until_idle(10).unwrap();
        }
        assert_eq!(fake.calls("flow.install_status").len(), before);
    }
    assert_eq!(fake.calls("flow.install_status").len(), 3);
}

#[test]
fn startup_rejected_install_status_settles_once_and_stays_settled() {
    let fake = Fake::new();
    let reason = "store_decode";
    let disposition = "approved_not_executed";
    let f = common::fixture_with_store(
        "startup-rejected-card",
        common::Options {
            hosts: Some(hosts(&fake, false)),
            ..common::Options::default()
        },
        |store| {
            seed(&store, FLOW, true, true);
            fake.enqueue(
                "flow.install_status",
                vec![Ok(json!({
                    "state":"rejected",
                    "code_hash":hash(FLOW,1),
                    "attempt":7,
                    "rejected_reason":reason,
                    "disposition":disposition
                }))],
            );
            Ok(())
        },
    );
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
    let card = f.module.rt.cards(FLOW).unwrap().remove(0);
    assert_eq!(card.state, basal_core::cards::CardState::Rejected);
    assert_eq!(card.rejected_reason.as_deref(), Some(reason));
    assert_eq!(card.disposition.as_deref(), Some(disposition));
    let expected = json!([{
        "version":1,
        "attempt":7,
        "state":"rejected",
        "rejected_reason":reason,
        "disposition":disposition
    }]);
    for op in ["flow.list", "flow.health"] {
        let row = entry(&f, op, FLOW);
        assert_eq!(row["state"], "unapproved");
        assert_eq!(row["install_cards"], expected);
    }
    assert!(f.module.rt.outbox().unwrap().is_empty());
    assert_eq!(fake.calls("flow.install_status").len(), 1);

    // Startup recovery only asks about pending cards, even on another restart.
    f.module.rt.recover_cardless_installs().unwrap();
    f.module.engine.run_until_idle(10).unwrap();
    assert_eq!(fake.calls("flow.install_status").len(), 1);
    assert!(f.module.rt.outbox().unwrap().is_empty());
}

#[test]
fn unknown_install_status_keeps_card_pending() {
    let fake = Fake::new();
    let f = common::fixture_with_store(
        "startup-unknown-install-status",
        common::Options {
            hosts: Some(hosts(&fake, false)),
            ..common::Options::default()
        },
        |store| {
            seed(&store, FLOW, true, true);
            fake.enqueue(
                "flow.install_status",
                vec![Ok(json!({
                    "state":"future_state",
                    "code_hash":hash(FLOW,1),
                    "attempt":7
                }))],
            );
            Ok(())
        },
    );
    assert_eq!(
        f.module.rt.flow(FLOW).unwrap().unwrap().approved_version,
        None
    );
    assert_eq!(
        f.module.rt.cards(FLOW).unwrap()[0].state,
        basal_core::cards::CardState::Pending
    );
    for op in ["flow.list", "flow.health"] {
        assert_eq!(
            entry(&f, op, FLOW)["install_cards"],
            json!([{
                "version":1,
                "attempt":7,
                "state":"pending",
                "rejected_reason":null,
                "disposition":null
            }])
        );
        assert!(
            entry(&f, op, FLOW)["install_checks"][0]["error"]
                .as_str()
                .unwrap()
                .contains("unrecognised install status")
        );
    }
    assert!(f.module.rt.outbox().unwrap().is_empty());
    assert_eq!(fake.calls("flow.install_status").len(), 1);
}

#[test]
fn idle_policy_install_has_zero_status_calls_after_confirmation() {
    let fake = Fake::new();
    let f = fixture(&fake, "policy-idle");
    fake.enqueue(
        "flow.install_request",
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
        "flow.install_request",
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
        "flow.install_request",
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
        "flow.install_request",
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
        "flow.install_request",
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
