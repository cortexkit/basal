//! Each decision card belongs to the `consent/v1` provider it was raised on.
//! Tests drive the real engine and answer sink directly, without worker
//! execution or polling timers.
mod common;

use basal_core::decisions::{self, CardState};
use basal_host::cingulate_consent::CingulateConsent;
use basal_host::core_consent::decision_request;
use basal_host::mock::MockHost;
use basal_host::transport::{Transport, WireError};
use basal_host::{
    Consent, DecisionCard, DecisionContext, DecisionKind, DecisionOption, DecisionPath,
    DisabledReason, MockCatalog, UnknownReason,
};
use basal_module::module::Hosts;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const PROVIDER: &str = "alternate-consent";
/// A second consent/v1 provider the catalog does not list: cards a test
/// selects onto it stay there, whatever the catalog offers.
const OTHER: &str = "second-consent";

#[derive(Default)]
struct Plane {
    next: u64,
    cursor: i64,
    ack: i64,
    cards: BTreeMap<String, Value>,
    answers: Vec<(i64, Value)>,
}

#[derive(Default)]
struct State {
    listed: bool,
    calls: Vec<(String, String, Value)>,
    planes: BTreeMap<String, Plane>,
    fail_request: bool,
    fail_ack: bool,
    page_size: usize,
}

#[derive(Default)]
struct Wire(Mutex<State>);

impl Wire {
    fn listed(&self, listed: bool) {
        self.0.lock().unwrap().listed = listed;
    }
    fn calls(&self, method: &str) -> Vec<(String, Value)> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(_, m, _)| m == method)
            .map(|(p, _, v)| (p.clone(), v.clone()))
            .collect()
    }
    fn calls_to(&self, provider: &str, method: &str) -> Vec<Value> {
        self.calls(method)
            .into_iter()
            .filter(|(p, _)| p == provider)
            .map(|(_, v)| v)
            .collect()
    }
    fn acked(&self, provider: &str) -> i64 {
        self.0.lock().unwrap().planes[provider].ack
    }
    fn settle(&self, provider: &str, id: &str, choice: Option<&str>) {
        let mut state = self.0.lock().unwrap();
        let plane = state.planes.get_mut(provider).unwrap();
        let card = plane.cards.get_mut(id).unwrap();
        card["state"] = json!(if choice.is_some() {
            "answered"
        } else {
            "expired"
        });
        if let Some(choice) = choice {
            card["answered_choice_id"] = json!(choice);
        }
        plane.cursor += 1;
        plane.answers.push((plane.cursor, card.clone()));
    }
}

impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(if self.0.lock().unwrap().listed {
            json!({"modules":[{"module_id":PROVIDER,"capabilities":{"provides":["consent/v1"]}}]})
        } else {
            json!({"modules":[]})
        })
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("no worker or tool execution is needed")
    }
    fn management(&self, provider: &str, method: &str, params: Value) -> Result<Value, WireError> {
        let mut state = self.0.lock().unwrap();
        state
            .calls
            .push((provider.into(), method.into(), params.clone()));
        let fail_request = std::mem::take(&mut state.fail_request);
        let fail_ack = if method.ends_with(".ack") {
            std::mem::take(&mut state.fail_ack)
        } else {
            false
        };
        let page_size = state.page_size;
        let plane = state.planes.entry(provider.into()).or_default();
        match method {
            "consent.request" => {
                let existing = plane
                    .cards
                    .iter()
                    .find(|(_, c)| {
                        c["state"] == "pending"
                            && params.get("dedup_key").is_some()
                            && c["dedup_key"] == params["dedup_key"]
                    })
                    .map(|(id, c)| (id.clone(), c.clone()));
                let id = if let Some((id, mut old)) = existing {
                    old.as_object_mut().unwrap().remove("elicitation_id");
                    old.as_object_mut().unwrap().remove("state");
                    if old == params {
                        id
                    } else {
                        plane.cards.get_mut(&id).unwrap()["state"] = json!("withdrawn");
                        plane.cursor += 1;
                        plane.next += 1;
                        format!("el_{}", plane.next)
                    }
                } else {
                    plane.next += 1;
                    format!("el_{}", plane.next)
                };
                let mut card = params;
                card["elicitation_id"] = json!(id);
                card["state"] = json!("pending");
                plane.cards.insert(id.clone(), card);
                if fail_request {
                    Err(WireError::Unknown("accepted but reply lost".into()))
                } else {
                    Ok(json!({"elicitation_id":id}))
                }
            }
            "consent.answers" => {
                let since = params["since"].as_i64().unwrap();
                let pending: Vec<_> = plane
                    .answers
                    .iter()
                    .filter(|(c, _)| *c > since && *c > plane.ack)
                    .collect();
                let count = if page_size > 0 {
                    page_size.min(pending.len())
                } else {
                    pending.len()
                };
                let partial = count < pending.len();
                let cursor = if partial {
                    pending[count - 1].0
                } else {
                    plane.cursor
                };
                let records: Vec<_> = pending[..count].iter().map(|(_, v)| v.clone()).collect();
                let mut page = json!({"cursor":cursor,"records":records});
                if partial {
                    page["continuation"] = json!(cursor);
                }
                Ok(page)
            }
            "consent.ack" => {
                if fail_ack {
                    return Err(WireError::Unknown("ack unavailable".into()));
                }
                plane.ack = plane
                    .ack
                    .max(params["cursor"].as_i64().unwrap().min(plane.cursor));
                Ok(json!({"ok":true}))
            }
            "consent.withdraw" => {
                let card = plane
                    .cards
                    .get_mut(params["elicitation_id"].as_str().unwrap())
                    .unwrap();
                if card["state"] == "pending" {
                    card["state"] = json!("withdrawn");
                    plane.cursor += 1;
                }
                Ok(card.clone())
            }
            _ => panic!("unexpected management call {provider}:{method}"),
        }
    }
}

struct Rig {
    module: Option<basal_module::module::Module>,
    dir: std::path::PathBuf,
    config: basal_module::module::ModuleConfig,
    wire: Arc<Wire>,
    consent: Arc<CingulateConsent>,
}

impl Rig {
    fn new() -> Self {
        let dir = common::scratch("consent-paths");
        let config = basal_module::module::ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: basal_core::Durability::throwaway(),
            runtime: basal_core::Config {
                clock: basal_core::Clock::manual(common::T0),
                install_gate: basal_core::InstallGate::Off,
                ..Default::default()
            },
            pool: common::pool_config(&common::Options::default()),
            engine: basal_module::engine::EngineConfig::default(),
            dry_run: basal_module::dryrun::DryRunConfig::new(dir.join("dry")),
        };
        let wire = Arc::new(Wire::default());
        let consent = Arc::new(CingulateConsent::new(wire.clone()));
        let mut rig = Self {
            module: None,
            dir,
            config,
            wire,
            consent,
        };
        rig.start();
        rig
    }
    fn start(&mut self) {
        self.module = Some(
            basal_module::module::Module::start(
                self.config.clone(),
                Hosts {
                    transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                    host: Arc::new(MockHost::new()),
                    catalog: Arc::new(MockCatalog::standard()),
                    consent: self.consent.clone(),
                    hooks: Arc::new(basal_core::NoHooks),
                },
                Arc::new(basal_module::pool::ProcessSpawner::new(&self.config.pool)),
            )
            .unwrap(),
        );
    }
    fn restart(&mut self) {
        drop(self.module.take());
        self.consent = Arc::new(CingulateConsent::new(self.wire.clone()));
        self.start();
    }
    fn m(&self) -> &basal_module::module::Module {
        self.module.as_ref().unwrap()
    }
    fn pass(&self) {
        self.m().engine.pass().unwrap();
    }
    fn record(&self, flow: &str) -> decisions::DecisionRecord {
        self.m()
            .rt
            .decision_cards()
            .unwrap()
            .into_iter()
            .find(|c| c.flow_id == flow)
            .unwrap()
    }
    fn select_path(&self, flow: &str, path: DecisionPath) {
        let seq = self.record(flow).seq;
        assert_eq!(self.m().rt.select_decision_path(seq, &path).unwrap(), path);
    }
    fn install(&self, flow: &str) {
        let installed = self.m().rt.install(&basal_core::InstallRequest {
            script: "return 1;".into(), manifest: json!({"id":flow,"version":1,"purpose":"Consent path test","trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1}]}}).to_string(),
            author: "SYNAPSE".into(), loop_override: false,
        }).unwrap();
        self.m()
            .rt
            .approve(flow, 1, &installed.code_hash, "operator")
            .unwrap();
    }
    fn reenable(&self, flow: &str) {
        self.install(flow);
        self.m()
            .rt
            .store()
            .write(|tx| {
                basal_core::install::disable(
                    tx,
                    flow,
                    &basal_core::Actor::Runtime,
                    "run_limit_saturated",
                    common::T0,
                )
                .map_err(|e| basal_core::CoreError::Invalid(e.to_string()))?;
                decisions::record_auto_disable(
                    tx,
                    flow,
                    &DecisionContext::Reenable {
                        disabled_at_ms: common::T0,
                        disabled_reason: DisabledReason::RunLimitSaturated,
                        limit: 10,
                        window_ms: 60_000,
                        saturated_windows: 3,
                    },
                    common::T0,
                )
            })
            .unwrap();
    }
    fn grant(&self, flow: &str) {
        self.install(flow);
        let context = DecisionContext::GrantLost {
            provider: "github".into(),
            grant: "opaque".into(),
            grant_label: "original".into(),
            refused_at_ms: common::T0,
        };
        self.m().rt.store().write(|tx| {
            basal_core::install::disable(tx, flow, &basal_core::Actor::Core, "grant_lost", common::T0)
                .map_err(|e| basal_core::CoreError::Invalid(e.to_string()))?;
            tx.execute("INSERT INTO flow_grant_losses(flow_id,provider,grant_key,grant_label,echoable,run_id,version,state,next_poll_at,lost_at) VALUES (?1,'github','opaque','original',1,'lost-run',1,'polling',?2,?3)", rusqlite::params![flow, common::T0+1_000_000, common::T0])?;
            tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,instance,card,state,created_at) VALUES (?1,'grant_lost',?2,1,'lost-run',1,?3,'open',?4)", rusqlite::params![decisions::grant_lost_key(flow,"github","opaque"),flow,decisions::context_json(&context).to_string(),common::T0])?;
            Ok(())
        }).unwrap();
    }
    fn repeat_grant(&self, flow: &str) {
        self.m().rt.store().write(|tx| {
            tx.execute("UPDATE flow_grant_losses SET revision=revision+1,grant_label='replacement',lost_at=lost_at+1 WHERE flow_id=?1", [flow])?;
            tx.execute("UPDATE decision_cards SET revision=revision+1,instance=instance+1,card=json_set(card,'$.grant_label','replacement','$.refused_at_ms',(SELECT lost_at FROM flow_grant_losses WHERE flow_id=?1)) WHERE flow_id=?1", [flow])?;
            Ok(())
        }).unwrap();
    }
    fn enable(&self, flow: &str) {
        self.m()
            .rt
            .store()
            .write(|tx| {
                basal_core::install::enable(tx, flow, common::T0)
                    .map_err(|e| basal_core::CoreError::Invalid(e.to_string()))
            })
            .unwrap();
    }
    fn audit(&self, action: &str) -> i64 {
        self.m()
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM audit WHERE action=?1",
                    [action],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        drop(self.module.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn vector_card(kind: DecisionKind) -> DecisionCard {
    let (context, prompt, options): (DecisionContext, &str, &[(&str, &str, bool)]) = match kind {
        DecisionKind::Reconcile => (
            DecisionContext::Reconcile {
                run_id: "run-x".into(),
                run_admitted_at_ms: 1_780_408_800_000,
                step: Some("writer/send".into()),
                call_key: "call-x".into(),
                op: "prefrontal-core.sink.digest".into(),
                attempts: 2,
                unknown_reason: UnknownReason::ConnectionLost,
            },
            "Run 14:00 of flow-x may not have finished: it reached writer/send and lost track of whether the send went out.",
            &[
                ("send_again", "Send again", false),
                ("continue", "Continue: it ran", false),
                ("cancel", "Cancel the run", false),
                ("unresolved", "Leave it unresolved", true),
            ],
        ),
        DecisionKind::Reenable => (
            DecisionContext::Reenable {
                disabled_at_ms: 1_780_409_100_000,
                disabled_reason: DisabledReason::RunLimitSaturated,
                limit: 10,
                window_ms: 60_000,
                saturated_windows: 3,
            },
            "flow-x was turned off because it kept hitting its run limit.",
            &[
                ("reenable", "Re-enable", false),
                ("disabled", "Keep disabled", true),
            ],
        ),
        DecisionKind::GrantLost => (
            DecisionContext::GrantLost {
                provider: "github".into(),
                grant: "opaque:owner/cortexkit/basal:create_issue".into(),
                grant_label: "github: create_issue on cortexkit/basal (owner grant for BASAL)"
                    .into(),
                refused_at_ms: 1_780_409_100_000,
            },
            "flow-x was disabled after github refused its grant at 2026-06-02 14:05 UTC. Check now re-checks the existing grant; keep disabled stops polling. No answer keeps background polling without applying a choice.",
            &[
                ("check_now", "Check now", false),
                ("keep_disabled", "Keep disabled", true),
            ],
        ),
    };
    DecisionCard {
        dedup_key: None,
        flow_id: "flow-x".into(),
        version: 3,
        context,
        title: "Decide about flow-x".into(),
        prompt: prompt.into(),
        args_digest: None,
        options: options
            .iter()
            .map(|(id, label, decline)| DecisionOption {
                id: (*id).into(),
                label: (*label).into(),
                decline: *decline,
            })
            .collect(),
        expires_in_ms: 60_000,
    }
}

#[test]
fn requester_vectors_are_pinned_and_all_three_requests_match_bytes() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/consent-requester-v1");
    let manifest = std::fs::read(dir.join("SHA256SUMS")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&manifest)),
        "debc4ca40807fae5ad26cf1448f2ff4b062af7ebd0914244b08c9d3efb015642"
    );
    let entries: Vec<_> = String::from_utf8(manifest)
        .unwrap()
        .lines()
        .map(|line| {
            let (hash, file) = line.split_once("  ").unwrap();
            (hash.to_owned(), file.to_owned())
        })
        .collect();
    assert_eq!(entries.len(), 45);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), entries.len() + 1);
    for (hash, file) in entries {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(dir.join(&file)).unwrap())
            ),
            hash,
            "{file}"
        );
    }
    let wire = Arc::new(Wire::default());
    wire.listed(true);
    let consent = CingulateConsent::new(wire.clone());
    for kind in [
        DecisionKind::Reconcile,
        DecisionKind::Reenable,
        DecisionKind::GrantLost,
    ] {
        let card = vector_card(kind);
        let path = consent.decision_path().unwrap();
        consent.raise_decision_on(&path, &card).unwrap();
        let calls = wire.calls("consent.request");
        let (provider, params) = calls.last().unwrap();
        assert_eq!(provider, PROVIDER);
        let bytes =
            serde_json::to_vec(&json!({"method":"consent.request","params":params})).unwrap();
        assert_eq!(
            bytes,
            std::fs::read(dir.join(format!("{}-request.json", kind.as_str()))).unwrap()
        );
        let mut core_body = params.clone();
        core_body.as_object_mut().unwrap().remove("facts");
        let body = serde_json::to_string_pretty(&core_body).unwrap() + "\n";
        assert_eq!(
            body.as_bytes(),
            std::fs::read(dir.join(format!("{}-body.json", kind.as_str()))).unwrap()
        );
    }
}

#[test]
fn new_cards_require_an_advertised_consent_capability() {
    let rig = Rig::new();
    rig.wire.listed(true);
    rig.reenable("modern");
    rig.pass();
    assert_eq!(
        rig.record("modern").consent_path,
        Some(DecisionPath::Consent(PROVIDER.into()))
    );
    assert_eq!(rig.wire.calls("consent.request").len(), 1);
    rig.wire.listed(false);
    rig.reenable("unavailable");
    rig.pass();
    let unavailable = rig.record("unavailable");
    assert_eq!(unavailable.consent_path, None);
    assert_eq!(unavailable.raised_revision, None);
    assert_eq!(rig.wire.calls("consent.request").len(), 1);
}

#[test]
fn answers_on_two_providers_apply_once_and_ack_their_own_owner() {
    let rig = Rig::new();
    rig.reenable("other");
    rig.select_path("other", DecisionPath::Consent(OTHER.into()));
    rig.pass();
    rig.wire.listed(true);
    rig.reenable("modern");
    rig.pass();
    // Independent stores can issue identical ids. Path identity must participate
    // in lookup rather than trusting an elicitation id as globally unique.
    assert_eq!(
        rig.record("other").elicitation_id,
        rig.record("modern").elicitation_id
    );
    rig.wire.settle(OTHER, "el_1", Some("reenable"));
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.wire.0.lock().unwrap().fail_ack = true;
    assert!(rig.consent.poll_once().is_err());
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.record("other").state, CardState::Applied);
    assert_eq!(rig.record("modern").state, CardState::Applied);
    assert_eq!(rig.audit("flow.reenable"), 2);
    assert_eq!(rig.audit("decision.stale"), 0);
    assert_eq!(
        rig.wire.calls_to(OTHER, "consent.ack"),
        [json!({"cursor":1})]
    );
    assert!(
        rig.wire
            .calls_to(PROVIDER, "consent.ack")
            .iter()
            .all(|v| *v == json!({"cursor":2}))
    );
    assert_eq!((rig.wire.acked(OTHER), rig.wire.acked(PROVIDER)), (1, 2));
    assert!(rig.wire.calls("consent.report_execution").is_empty());
}

#[test]
fn updates_and_withdrawals_use_the_raising_path_and_replacement_id() {
    // One card is raised on the listed provider, the other is selected onto an
    // unlisted one. Each keeps its provider after the catalog changes.
    for (listed, owner, stranger) in [(true, PROVIDER, OTHER), (false, OTHER, PROVIDER)] {
        let mut rig = Rig::new();
        rig.wire.listed(listed);
        rig.grant("grant-flow");
        if !listed {
            rig.select_path("grant-flow", DecisionPath::Consent(OTHER.into()));
        }
        rig.pass();
        let original = rig.record("grant-flow");
        let path = DecisionPath::Consent(owner.into());
        assert_eq!(original.consent_path, Some(path.clone()));
        rig.wire.listed(!listed);
        rig.repeat_grant("grant-flow");
        rig.pass();
        let updated = rig.record("grant-flow");
        assert_eq!(updated.consent_path, Some(path));
        assert_eq!(updated.raised_revision, Some(2));
        let calls = rig.wire.calls_to(owner, "consent.request");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["dedup_key"], calls[1]["dedup_key"]);
        assert_eq!(calls[1]["flow_decision"]["grant_label"], "replacement");
        assert_ne!(updated.elicitation_id, original.elicitation_id);
        rig.enable("grant-flow");
        rig.restart();
        rig.pass();
        assert_eq!(
            rig.wire.calls("consent.withdraw"),
            [(
                owner.into(),
                json!({"elicitation_id":updated.elicitation_id.unwrap()})
            )]
        );
        assert!(rig.m().rt.decision_withdrawals_on().unwrap().is_empty());
        assert!(rig.wire.calls_to(stranger, "consent.request").is_empty());
    }
}

#[test]
fn provider_disappearance_keeps_old_answers_and_leaves_new_cards_pending() {
    let rig = Rig::new();
    rig.wire.listed(true);
    rig.reenable("before");
    rig.pass();
    rig.wire.listed(false);
    rig.reenable("after");
    rig.pass();
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.record("before").state, CardState::Applied);
    assert_eq!(rig.record("after").state, CardState::Open);
    assert_eq!(rig.record("after").consent_path, None);
    assert_eq!(rig.record("after").elicitation_id, None);
    assert_eq!(
        rig.wire.calls("consent.answers"),
        [(PROVIDER.into(), json!({"since":0}))]
    );
    assert_eq!(rig.wire.calls("consent.request").len(), 1);
}

#[test]
fn lost_request_reply_cannot_move_a_card_after_restart() {
    let mut rig = Rig::new();
    rig.wire.listed(true);
    rig.reenable("lost");
    rig.wire.0.lock().unwrap().fail_request = true;
    rig.pass();
    assert!(rig.record("lost").elicitation_id.is_none());
    assert_eq!(
        rig.record("lost").consent_path,
        Some(DecisionPath::Consent(PROVIDER.into()))
    );
    rig.wire.listed(false);
    rig.restart();
    rig.pass();
    assert_eq!(rig.wire.calls("consent.request").len(), 2);
    assert_eq!(rig.wire.0.lock().unwrap().planes[PROVIDER].cards.len(), 1);
    assert_eq!(rig.record("lost").elicitation_id.as_deref(), Some("el_1"));
}

#[test]
fn paged_cursor_is_durable_and_empty_withdrawal_pages_are_acked() {
    let mut rig = Rig::new();
    rig.wire.listed(true);
    for flow in ["one", "two", "three"] {
        rig.reenable(flow);
        rig.pass();
    }
    rig.wire.0.lock().unwrap().page_size = 1;
    for id in ["el_1", "el_2", "el_3"] {
        rig.wire.settle(PROVIDER, id, Some("reenable"));
    }
    rig.consent.poll_once().unwrap();
    assert_eq!(
        rig.wire.calls("consent.answers"),
        [
            (PROVIDER.into(), json!({"since":0})),
            (PROVIDER.into(), json!({"since":1})),
            (PROVIDER.into(), json!({"since":2})),
        ]
    );
    assert_eq!(
        rig.wire.calls("consent.ack"),
        [
            (PROVIDER.into(), json!({"cursor":1})),
            (PROVIDER.into(), json!({"cursor":2})),
            (PROVIDER.into(), json!({"cursor":3})),
        ]
    );
    rig.restart();
    rig.reenable("four");
    rig.pass();
    // Withdrawals advance the provider's settlement cursor, but consent.answers
    // omits their records. An empty final page must still preserve that cursor.
    rig.wire
        .0
        .lock()
        .unwrap()
        .planes
        .get_mut(PROVIDER)
        .unwrap()
        .cursor = 8;
    rig.consent.poll_once().unwrap();
    assert_eq!(
        rig.wire.calls("consent.answers").last().unwrap().1,
        json!({"since":3})
    );
    assert_eq!(
        rig.wire.calls("consent.ack").last().unwrap().1,
        json!({"cursor":8})
    );
    assert_eq!(
        rig.m()
            .rt
            .decision_answer_cursor(&DecisionPath::Consent(PROVIDER.into()))
            .unwrap(),
        8
    );
}

#[test]
fn poison_page_is_not_acked_and_other_path_still_progresses() {
    let rig = Rig::new();
    rig.reenable("other");
    rig.select_path("other", DecisionPath::Consent(OTHER.into()));
    rig.pass();
    rig.wire.listed(true);
    rig.reenable("poison");
    rig.pass();
    rig.reenable("valid");
    rig.pass();
    rig.wire.settle(OTHER, "el_1", Some("reenable"));
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.wire.settle(PROVIDER, "el_2", Some("reenable"));
    rig.wire
        .0
        .lock()
        .unwrap()
        .planes
        .get_mut(PROVIDER)
        .unwrap()
        .answers[0]
        .1["flow_decision"] = json!({});
    assert!(rig.consent.poll_once().is_err());
    assert_eq!(rig.record("other").state, CardState::Applied);
    assert_eq!(rig.record("valid").state, CardState::Applied);
    assert_eq!(rig.record("poison").state, CardState::Open);
    assert!(rig.wire.calls_to(PROVIDER, "consent.ack").is_empty());
    assert_eq!(rig.wire.acked(OTHER), 1);
    assert_eq!(
        rig.m()
            .rt
            .decision_answer_cursor(&DecisionPath::Consent(PROVIDER.into()))
            .unwrap(),
        0
    );
    let mut state = rig.wire.0.lock().unwrap();
    let plane = state.planes.get_mut(PROVIDER).unwrap();
    plane.answers[0].1 = plane.cards["el_1"].clone();
    drop(state);
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.audit("flow.reenable"), 3);
}

#[test]
fn retention_tombstone_closes_only_its_owned_card_without_action() {
    let rig = Rig::new();
    rig.reenable("other");
    rig.select_path("other", DecisionPath::Consent(OTHER.into()));
    rig.pass();
    rig.wire.listed(true);
    rig.reenable("retained");
    rig.pass();
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.wire
        .0
        .lock()
        .unwrap()
        .planes
        .get_mut(PROVIDER)
        .unwrap()
        .answers[0]
        .1 = json!({"elicitation_id":"el_1","state":"expired","settled_at":common::T0});
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.record("retained").state, CardState::Expired);
    assert_eq!(rig.record("other").state, CardState::Open);
    assert_eq!(rig.audit("flow.reenable"), 0);
    assert_eq!(rig.wire.calls_to(PROVIDER, "consent.ack").len(), 1);
}

#[test]
fn consent_endpoint_cannot_weaken_v2_body_validation() {
    let wire = Arc::new(Wire::default());
    wire.listed(true);
    let consent = CingulateConsent::new(wire.clone());
    for kind in [
        DecisionKind::Reconcile,
        DecisionKind::Reenable,
        DecisionKind::GrantLost,
    ] {
        let mut card = vector_card(kind);
        card.flow_id = " ".into();
        let path = consent.decision_path().unwrap();
        assert!(consent.raise_decision_on(&path, &card).is_err());
    }
    assert!(wire.calls("consent.request").is_empty());
    assert!(decision_request(&vector_card(DecisionKind::Reconcile)).is_ok());
}

#[test]
fn lost_replacement_reply_recovers_only_the_current_cingulate_grant_answer() {
    let rig = Rig::new();
    rig.wire.listed(true);
    rig.grant("grant-flow");
    rig.pass();
    rig.repeat_grant("grant-flow");
    rig.wire.0.lock().unwrap().fail_request = true;
    rig.pass();
    assert_eq!(
        rig.record("grant-flow").elicitation_id.as_deref(),
        Some("el_1")
    );
    assert_eq!(rig.record("grant-flow").raised_revision, Some(1));
    rig.wire.settle(PROVIDER, "el_2", Some("check_now"));
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.record("grant-flow").state, CardState::Applied);
    assert_eq!(
        rig.record("grant-flow").elicitation_id.as_deref(),
        Some("el_2")
    );
    assert_eq!(rig.audit("grant.check_now"), 1);
    let applied = rig.record("grant-flow");
    rig.m().rt.decision_raised(applied.seq, 1, "el_1").unwrap();
    assert_eq!(
        rig.record("grant-flow").elicitation_id.as_deref(),
        Some("el_2")
    );
}

fn grant_answer(record: &decisions::DecisionRecord, id: &str) -> basal_host::DecisionAnswer {
    let mut body = decision_request(&record.to_card().unwrap()).unwrap();
    body["elicitation_id"] = json!(id);
    body["state"] = json!("answered");
    body["answered_choice_id"] = json!("check_now");
    basal_host::core_consent::decision_answer(&body).unwrap()
}

#[test]
fn old_context_and_recorded_superseded_ids_cannot_answer_an_outstanding_revision() {
    let rig = Rig::new();
    rig.wire.listed(true);
    rig.grant("grant-flow");
    rig.pass();
    let original = rig.record("grant-flow");
    rig.repeat_grant("grant-flow");
    rig.pass();
    let accepted = rig.record("grant-flow");
    let path = DecisionPath::Consent(PROVIDER.into());
    assert_eq!(
        rig.m()
            .rt
            .answer_decision_on(&path, &grant_answer(&accepted, "unrecognized-id"))
            .unwrap(),
        decisions::Answered::Superseded
    );
    rig.repeat_grant("grant-flow");
    rig.wire.0.lock().unwrap().fail_request = true;
    rig.pass();
    let latest = rig.record("grant-flow");
    let path = DecisionPath::Consent(PROVIDER.into());
    for answer in [
        grant_answer(&original, "el_1"),
        grant_answer(&accepted, "el_2"),
        grant_answer(&latest, "el_1"),
    ] {
        assert_eq!(
            rig.m().rt.answer_decision_on(&path, &answer).unwrap(),
            decisions::Answered::Superseded
        );
        assert_eq!(rig.record("grant-flow").state, CardState::Open);
    }
    assert_eq!(rig.audit("grant.check_now"), 0);
}

#[test]
fn failed_final_ack_is_retried_from_the_durable_cursor_when_a_card_opens() {
    let mut rig = Rig::new();
    rig.wire.listed(true);
    rig.reenable("modern");
    rig.pass();
    rig.wire.settle(PROVIDER, "el_1", Some("reenable"));
    rig.wire.0.lock().unwrap().fail_ack = true;
    assert!(rig.consent.poll_once().is_err());
    assert_eq!(rig.record("modern").state, CardState::Applied);
    assert_eq!(
        rig.m()
            .rt
            .decision_answer_cursor(&DecisionPath::Consent(PROVIDER.into()))
            .unwrap(),
        1
    );
    // After a restart with no open card nothing is read or acknowledged: the
    // provider keeps the page, and the durable cursor covers it.
    rig.restart();
    let sent = rig.wire.0.lock().unwrap().calls.len();
    rig.consent.poll_once().unwrap();
    assert_eq!(rig.wire.0.lock().unwrap().calls.len(), sent);
    assert_eq!(rig.wire.acked(PROVIDER), 0);
    // The next card on that provider acknowledges the durable cursor before
    // reading, so the applied answer is never read again.
    rig.reenable("next");
    rig.pass();
    rig.consent.poll_once().unwrap();
    assert_eq!(
        rig.wire.calls_to(PROVIDER, "consent.ack")[1..],
        [json!({"cursor":1}), json!({"cursor":1})]
    );
    assert_eq!(
        rig.wire.calls("consent.answers").last().unwrap().1,
        json!({"since":1})
    );
    assert_eq!(rig.wire.acked(PROVIDER), 1);
    assert_eq!(rig.audit("flow.reenable"), 1);
}
