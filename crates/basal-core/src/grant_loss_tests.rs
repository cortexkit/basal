use super::*;
use basal_host::flow_scope::{FlowScope, RegisteredScope};
use basal_host::{
    CallClass, MockCatalog,
    flow_refusal::{FlowRefusal, RefusalReason},
};
use basal_proto::{CallKind, JsonText};
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

type Probe = (String, Option<String>, String, String);

struct ProbeHost {
    refusal: Mutex<FlowRefusal>,
    probes: Mutex<Vec<Probe>>,
    yes: AtomicBool,
    scoped: AtomicBool,
    revoked: AtomicBool,
    configured: Mutex<Option<String>>,
    hash: String,
    on_probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl Host for ProbeHost {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        Err(TransportError::Refused(lock(&self.refusal).clone()))
    }
    fn grant_would_ask(
        &self,
        flow: &str,
        agent: Option<&str>,
        provider: &str,
        grant: &str,
    ) -> std::result::Result<bool, TransportError> {
        assert_eq!(lock(&self.configured).as_deref(), Some(flow));
        lock(&self.probes).push((
            flow.into(),
            agent.map(str::to_owned),
            provider.into(),
            grant.into(),
        ));
        if let Some(callback) = lock(&self.on_probe).take() {
            callback();
        }
        Ok(self.yes.load(Ordering::SeqCst))
    }
    fn configure_flow(&self, flow: &str, _: bool, scope: Option<RegisteredScope>) {
        assert!(scope.is_some());
        *lock(&self.configured) = Some(flow.into());
    }
    fn install_status(
        &self,
        _: &str,
        _: u32,
    ) -> std::result::Result<basal_host::InstallStatus, TransportError> {
        if self.revoked.load(Ordering::SeqCst) {
            return Ok(basal_host::InstallStatus::Revoked {
                code_hash: self.hash.clone(),
            });
        }
        Ok(basal_host::InstallStatus::Active {
            code_hash:self.hash.clone(),
            scope:self.scoped.load(Ordering::SeqCst).then(|| RegisteredScope {
                selector: serde_json::from_value::<FlowScope>(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"flow-scope","epoch":1})).unwrap(),
                targets:["plexus".into()].into_iter().collect(),
            }),
        })
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}
struct Fixture {
    rt: Runtime,
    host: Arc<ProbeHost>,
    dir: std::path::PathBuf,
}
fn manifest(version: u32, targets: Value) -> String {
    json!({"id":"flow","version":version,"purpose":"restoration test","trigger":{"events":[{"module":"mock","name":"changed","version":1}]},"status":targets}).to_string()
}
fn absence(grant: Value) -> FlowRefusal {
    let mut r = FlowRefusal::new(RefusalReason::ModuleGrantAbsent, "plexus", "github");
    r.detail = Some(
        json!({"grant":grant,"grant_label":"github: create_issue on cortexkit/basal","layer":"module","connection":"github","action":"create_issue","cause":"missing"}),
    );
    r
}
impl Fixture {
    fn new(owner: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "basal-grant-loss-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            Arc::new(Store::open(dir.join("store.sqlite"), crate::Durability::default()).unwrap());
        let hash = crate::ids::code_hash("return 1", &manifest(1, json!([])));
        store.write(|tx| {
            tx.execute("INSERT INTO flows(flow_id,owner,approved_version,created_at) VALUES ('flow',?1,1,0)",[owner])?;
            tx.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,approval_ref,state,installed_at) VALUES ('flow',1,?1,?2,'return 1',?3,'approval','approved',0)",params![hash.as_slice(),manifest(1,json!([])),owner])?;
            Ok(())
        }).unwrap();
        let host = Arc::new(ProbeHost {
            refusal: Mutex::new(absence(json!("g1.opaque"))),
            probes: Mutex::new(Vec::new()),
            yes: AtomicBool::new(false),
            scoped: AtomicBool::new(true),
            revoked: AtomicBool::new(false),
            configured: Mutex::new(None),
            hash: crate::ids::hex(&hash),
            on_probe: Mutex::new(None),
        });
        let catalog = Arc::new(MockCatalog::standard());
        catalog.add_agent("old-agent");
        catalog.add_agent("new-agent");
        catalog.add_named_agent("old-agent", "OLD");
        let rt = Runtime::new(
            store,
            host.clone(),
            catalog,
            Arc::new(crate::NoHooks),
            None,
            Config {
                clock: crate::Clock::manual(0),
                ..Config::default()
            },
        );
        *rt.shared.retention_next.lock().unwrap() = Some(3_600_000);
        Self { rt, host, dir }
    }
    fn lose(&self, run: &str, refusal: FlowRefusal, args: Value) {
        self.lose_after(run, refusal, args, true);
    }
    fn lose_after(&self, run: &str, refusal: FlowRefusal, args: Value, prior_unsent: bool) {
        let class = if prior_unsent {
            StoredClass::Query
        } else {
            StoredClass::KeyedMutation
        };
        *lock(&self.host.refusal) = refusal;
        self.rt.store().write(|tx| {
            tx.execute("INSERT INTO runs(run_id,flow_id,flow_version,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,deadline_ms) VALUES (?1,'flow',1,?1,1,'null','return 1',?2,zeroblob(32),'pending',0,600000)",params![run,manifest(1,json!([]))])?;
            let lease = runs::claim(tx,run,"owner",0)?;
            journal::insert_call(tx,&lease,"flow",&journal::NewCall {position:0,kind:&CallKind::Op {module:"plexus".into(),op:"github".into()},args:&JsonText::new(args.to_string()).unwrap(),class,request:None})?;
            Ok(())
        }).unwrap();
        let request = Runtime::request(
            &self.rt.run(run).unwrap(),
            &self.rt.calls(run).unwrap()[0],
            1,
        );
        self.rt.dispatch(request, class, prior_unsent).unwrap();
    }
    fn approve(&self, owner: &str, targets: Value) {
        let m = manifest(2, targets);
        let hash = crate::ids::code_hash("return 2", &m);
        self.rt.store().write(|tx| {
            tx.execute("INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at) VALUES ('flow',2,?1,?2,'return 2',?3,'validated',0)",params![hash.as_slice(),m,owner])?;
            Ok(())
        }).unwrap();
        self.rt.approve("flow", 2, &hash, "approval-2").unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn grant_absence_disables_and_keeps_one_durable_subject() {
    let f = Fixture::new("old-agent");
    let mut refusal = absence(json!("g1.opaque"));
    refusal.reason = RefusalReason::AgentGrantAbsent;
    f.lose("run-1", refusal, json!({}));
    assert_eq!(
        f.rt.flow("flow")
            .unwrap()
            .unwrap()
            .disabled_reason
            .as_deref(),
        Some("grant_lost")
    );
    assert_eq!(
        f.rt.calls("run-1").unwrap()[0].dispatch,
        crate::DispatchState::Deferred
    );
    assert_eq!(f.rt.grant_losses().unwrap().len(), 1);
    f.lose("run-2", absence(json!("g1.opaque")), json!({}));
    let subjects = f.rt.grant_losses().unwrap();
    assert_eq!(subjects.len(), 1);
    assert_eq!(subjects[0].run_id, "run-2");
    assert_eq!(subjects[0].revision, 2);
    assert_eq!(subjects[0].grant, "g1.opaque");
    let cards = f.rt.decision_cards().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].kind, basal_host::DecisionKind::GrantLost);
    assert_eq!(cards[0].run_id.as_deref(), Some("run-2"));
    assert_eq!(cards[0].revision, 2);
}

#[test]
fn structured_grants_have_stable_keys_and_oversized_grants_are_not_echoed() {
    let f = Fixture::new("old-agent");
    f.lose(
        "run-1",
        absence(serde_json::from_str(r#"{"z":3,"a":{"y":2,"x":1}}"#).unwrap()),
        json!({}),
    );
    f.lose(
        "run-2",
        absence(serde_json::from_str(r#"{"a":{"x":1,"y":2},"z":3}"#).unwrap()),
        json!({}),
    );
    let losses = f.rt.grant_losses().unwrap();
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].grant, r#"{"a":{"x":1,"y":2},"z":3}"#);
    assert_eq!(losses[0].revision, 2);
    let oversized = "x".repeat(4097);
    f.lose("run-3", absence(json!(oversized)), json!({}));
    let losses = f.rt.grant_losses().unwrap();
    let hashed = losses.iter().find(|g| !g.echoable).unwrap();
    assert_eq!(
        hashed.grant,
        blake3::hash(oversized.as_bytes()).to_hex().to_string()
    );
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
    assert_eq!(
        f.rt.decision_cards().unwrap().len(),
        1,
        "only the smaller structured grant has a card"
    );
    f.rt.poll_lost_grants().unwrap();
    assert_eq!(
        lock(&f.host.probes).len(),
        1,
        "a hashed fallback is not a provider reference"
    );
}

#[test]
fn grant_polling_uses_injected_backoff_and_requires_registered_scope() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(0));
    f.host.scoped.store(false, Ordering::SeqCst);
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 1);
    assert!(lock(&f.host.probes).is_empty());
    assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(1000));
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 0);
    f.host.scoped.store(true, Ordering::SeqCst);
    f.rt.config.clock.set(1000);
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 1);
    assert_eq!(f.rt.grant_losses().unwrap()[0].next_poll_at_ms, 3000);
    assert_eq!(
        lock(&f.host.probes)[0],
        (
            "flow".into(),
            Some("old-agent".into()),
            "plexus".into(),
            "g1.opaque".into()
        )
    );
    for _ in 0..10 {
        let at = f.rt.grant_losses().unwrap()[0].next_poll_at_ms;
        f.rt.config.clock.set(at);
        f.rt.poll_lost_grants().unwrap();
    }
    let loss = &f.rt.grant_losses().unwrap()[0];
    assert_eq!(loss.next_poll_at_ms - f.rt.config.clock.now_ms(), 60_000);
}

#[test]
fn restored_grant_reenables_only_its_own_disable() {
    for actor in [
        None,
        Some(crate::Actor::Operator("stop".into())),
        Some(crate::Actor::Agent("old-agent".into())),
    ] {
        let f = Fixture::new("old-agent");
        f.lose("run", absence(json!("g1.opaque")), json!({}));
        if let Some(actor) = &actor {
            f.rt.disable_flow("flow", actor, "manual stop").unwrap();
        }
        f.host.yes.store(true, Ordering::SeqCst);
        assert_eq!(f.rt.poll_lost_grants().unwrap(), 1);
        assert_eq!(f.rt.flow("flow").unwrap().unwrap().enabled, actor.is_none());
        assert_eq!(f.rt.grant_losses().unwrap()[0].state, "restored");
        assert_eq!(f.rt.poll_lost_grants().unwrap(), 0);
    }
}

#[test]
fn keeping_disabled_stops_polling_until_explicit_owner_enable() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    assert!(
        f.rt.stop_grant_polling("flow", "plexus", "g1.opaque")
            .unwrap()
    );
    f.host.yes.store(true, Ordering::SeqCst);
    f.rt.config.clock.set(1_000_000);
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 0);
    assert!(lock(&f.host.probes).is_empty());
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
    assert!(!f.rt.check_grant_now("flow", "plexus", "g1.opaque").unwrap());
    assert!(
        f.rt.enable_flow_as("flow", &crate::Actor::Agent("old-agent".into()))
            .unwrap()
    );
    assert_eq!(f.rt.grant_losses().unwrap()[0].state, "cleared");
    assert_eq!(
        f.rt.store()
            .read(|c| journal::deferred_until(c, "run", 0))
            .unwrap(),
        Some(1_000_000)
    );
}

#[test]
fn immediate_check_and_multiple_grants_do_not_clear_other_causes() {
    let f = Fixture::new("old-agent");
    f.lose("run-1", absence(json!("first")), json!({}));
    f.lose("run-2", absence(json!("second")), json!({}));
    f.rt.poll_lost_grants().unwrap();
    f.rt.stop_grant_polling("flow", "plexus", "second").unwrap();
    assert!(f.rt.check_grant_now("flow", "plexus", "first").unwrap());
    f.host.yes.store(true, Ordering::SeqCst);
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 1);
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
}

#[test]
fn core_revocation_stops_grant_polling_without_provider_calls() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    f.host.revoked.store(true, Ordering::SeqCst);
    f.rt.poll_lost_grants().unwrap();
    assert_eq!(f.rt.grant_losses().unwrap()[0].state, "cleared");
    assert!(lock(&f.host.probes).is_empty());
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 0);
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
}

#[test]
fn retirement_is_durable_health_without_a_card_and_enable_is_typed_refusal() {
    let f = Fixture::new("old-agent");
    f.lose(
        "run",
        FlowRefusal::new(
            RefusalReason::AgentRetired,
            "prefrontal-core",
            "sink.status",
        ),
        json!({"agent":"old-agent"}),
    );
    assert_eq!(f.rt.run("run").unwrap().state, crate::RunState::Failed);
    assert_eq!(
        f.rt.flow("flow")
            .unwrap()
            .unwrap()
            .disabled_reason
            .as_deref(),
        Some("agent_retired")
    );
    let health = f.rt.flow_health().unwrap();
    assert_eq!(
        health[0].agent_retirement.as_ref().unwrap()["agent_id"],
        "old-agent"
    );
    assert!(!health[0].auto_disabled);
    assert!(f.rt.decision_cards().unwrap().is_empty());
    for actor in [
        crate::Actor::Operator("op".into()),
        crate::Actor::Agent("old-agent".into()),
        crate::Actor::Runtime,
        crate::Actor::Core,
    ] {
        assert!(matches!(
            f.rt.enable_flow_as("flow", &actor),
            Err(crate::InstallError::AgentRetired { .. })
        ));
    }
    assert!(matches!(
        f.rt.enable_flow("flow"),
        Err(crate::InstallError::AgentRetired { .. })
    ));
    f.approve("old-agent", json!([]));
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
}

#[test]
fn retirement_clears_only_when_reinstall_no_longer_involves_the_agent() {
    for (owner, targets, enabled) in [
        ("old-agent", json!([]), false),
        ("new-agent", json!([]), true),
        ("operator", json!(["old-agent"]), false),
        ("operator", json!(["OLD"]), false),
        ("operator", json!(["new-agent"]), true),
        ("operator", json!([]), true),
    ] {
        let f = Fixture::new("operator");
        f.lose(
            "run",
            FlowRefusal::new(
                RefusalReason::AgentRetired,
                "prefrontal-core",
                "sink.status",
            ),
            json!({"agent":"old-agent"}),
        );
        f.approve(owner, targets);
        assert_eq!(
            f.rt.flow("flow").unwrap().unwrap().enabled,
            enabled,
            "owner={owner}"
        );
    }
}

#[test]
fn removed_flow_stops_restoration_without_provider_reads() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    f.rt.store()
        .write(|tx| {
            tx.execute("UPDATE flows SET removed=1 WHERE flow_id='flow'", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 0);
    assert!(lock(&f.host.probes).is_empty());
    assert_eq!(f.rt.grant_losses().unwrap()[0].state, "cleared");
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
}

#[test]
fn stale_positive_diagnostic_cannot_clear_a_repeat_loss() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    f.host.yes.store(true, Ordering::SeqCst);
    let rt = f.rt.clone();
    *lock(&f.host.on_probe) = Some(Box::new(move || {
        let mut refusal = absence(json!("g1.opaque"));
        refusal.detail.as_mut().unwrap()["grant_label"] = json!("updated grant label");
        rt.store()
            .write(|tx| {
                crate::grant_loss::record(tx, "flow", "run", &refusal, rt.config.clock.now_ms())
            })
            .unwrap();
    }));
    f.rt.poll_lost_grants().unwrap();
    assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
    let loss = &f.rt.grant_losses().unwrap()[0];
    assert_eq!(loss.state, "polling");
    assert_eq!(loss.revision, 2);
    assert_eq!(loss.grant_label, "updated grant label");
}

fn answer_for(f: &Fixture, choice: Option<&str>) -> basal_host::DecisionAnswer {
    let card = &f.rt.decision_cards().unwrap()[0];
    f.rt.decision_raised(card.seq, card.revision, "grant-card")
        .unwrap();
    basal_host::DecisionAnswer {
        grant_context: Some(card.context().unwrap()),
        elicitation_id: "grant-card".into(),
        choice: choice.map(str::to_owned),
        dedup_key: Some(card.dedup_key.clone()),
        flow_id: card.flow_id.clone(),
        version: 0,
        decision: basal_host::DecisionKind::GrantLost,
        run_id: None,
        call_key: None,
    }
}

#[test]
fn grant_card_expiry_keeps_polling_and_explicit_keep_stops_it() {
    for (choice, polling) in [(None, true), (Some("keep_disabled"), false)] {
        let f = Fixture::new("old-agent");
        f.lose("run", absence(json!("g1.card")), json!({}));
        let answer = answer_for(&f, choice);
        f.rt.answer_decision(&answer).unwrap();
        assert_eq!(
            f.rt.grant_losses().unwrap()[0].state,
            if polling { "polling" } else { "stopped" }
        );
        assert_eq!(f.rt.poll_lost_grants().unwrap(), usize::from(polling));
        assert!(!f.rt.flow("flow").unwrap().unwrap().enabled);
        assert!(
            f.rt.decisions_due().unwrap().is_empty(),
            "a settled card is not repeatedly raised"
        );
    }
}

#[test]
fn grant_card_check_now_schedules_an_immediate_scoped_read() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    f.rt.poll_lost_grants().unwrap();
    assert_eq!(f.rt.grant_losses().unwrap()[0].next_poll_at_ms, 1000);
    f.rt.answer_decision(&answer_for(&f, Some("check_now")))
        .unwrap();
    assert_eq!(f.rt.grant_losses().unwrap()[0].next_poll_at_ms, 0);
    f.host.yes.store(true, Ordering::SeqCst);
    assert_eq!(f.rt.poll_lost_grants().unwrap(), 1);
    assert_eq!(lock(&f.host.probes).len(), 2);
    assert_eq!(f.rt.grant_losses().unwrap()[0].state, "restored");
}

#[test]
fn restored_card_withdrawal_is_durable_even_after_a_late_raise_ack() {
    let f = Fixture::new("old-agent");
    f.lose("run", absence(json!("g1.opaque")), json!({}));
    let card = f.rt.decisions_due().unwrap().remove(0);
    f.host.yes.store(true, Ordering::SeqCst);
    f.rt.poll_lost_grants().unwrap();
    assert_eq!(
        f.rt.decision_cards().unwrap()[0].state,
        crate::decisions::CardState::Stale
    );
    f.rt.decision_raised(card.seq, card.revision, "late-card")
        .unwrap();
    assert_eq!(f.rt.decision_withdrawals().unwrap(), ["late-card"]);
    f.rt.decision_withdrawn("late-card").unwrap();
    assert!(f.rt.decision_withdrawals().unwrap().is_empty());
}

#[test]
fn grant_card_answer_to_a_superseded_body_cannot_stop_current_polling() {
    let f = Fixture::new("old-agent");
    f.lose("run-1", absence(json!("g1.opaque")), json!({}));
    let answer = answer_for(&f, Some("keep_disabled"));
    f.rt.config.clock.set(100);
    f.lose("run-2", absence(json!("g1.opaque")), json!({}));
    assert_eq!(
        f.rt.answer_decision(&answer).unwrap(),
        crate::decisions::Answered::Superseded
    );
    assert_eq!(f.rt.grant_losses().unwrap()[0].state, "polling");
}

#[test]
fn older_raise_ack_cannot_replace_the_current_grant_card_identity() {
    let f = Fixture::new("old-agent");
    f.lose("run-1", absence(json!("g1.opaque")), json!({}));
    let first = f.rt.decisions_due().unwrap().remove(0);
    f.rt.config.clock.set(100);
    f.lose("run-2", absence(json!("g1.opaque")), json!({}));
    let latest = f.rt.decisions_due().unwrap().remove(0);
    f.rt.decision_raised(latest.seq, latest.revision, "new-card")
        .unwrap();
    f.rt.decision_raised(first.seq, first.revision, "old-card")
        .unwrap();
    assert_eq!(
        f.rt.decision_cards().unwrap()[0].elicitation_id.as_deref(),
        Some("new-card")
    );
}

#[test]
fn retirement_after_unknown_preserves_reconcile_then_not_applied_fails_once() {
    let f = Fixture::new("old-agent");
    f.lose_after(
        "run",
        FlowRefusal::new(
            RefusalReason::AgentRetired,
            "prefrontal-core",
            "sink.status",
        ),
        json!({"agent":"old-agent"}),
        false,
    );
    assert_eq!(
        f.rt.calls("run").unwrap()[0].dispatch,
        crate::DispatchState::Unknown
    );
    assert_eq!(
        f.rt.flow("flow")
            .unwrap()
            .unwrap()
            .disabled_reason
            .as_deref(),
        Some("agent_retired")
    );
    f.rt.store()
        .write(|tx| {
            runs::exit(
                tx,
                &crate::runs::Lease {
                    run_id: "run".into(),
                    owner: "owner".into(),
                    generation: 1,
                },
                &crate::runs::Exit::NeedsReconcile { positions: vec![0] },
            )?;
            Ok(())
        })
        .unwrap();
    let card = f.rt.decisions_due().unwrap().remove(0);
    assert_eq!(card.kind, basal_host::DecisionKind::Reconcile);
    f.rt.decision_raised(card.seq, card.revision, "reconcile-card")
        .unwrap();
    f.rt.answer_decision(&basal_host::DecisionAnswer {
        grant_context: None,
        elicitation_id: "reconcile-card".into(),
        choice: Some(crate::decisions::NOT_APPLIED.into()),
        dedup_key: Some(card.dedup_key.clone()),
        flow_id: "flow".into(),
        version: 1,
        decision: basal_host::DecisionKind::Reconcile,
        run_id: Some("run".into()),
        call_key: card.call_key.clone(),
    })
    .unwrap();
    assert_eq!(f.rt.run("run").unwrap().state, crate::RunState::Pending);
    let attempt =
        f.rt.store()
            .write(|tx| {
                let lease = runs::claim(tx, "run", "owner", 0)?;
                journal::authorize_resend(tx, &lease, 0)
            })
            .unwrap();
    let request = Runtime::request(
        &f.rt.run("run").unwrap(),
        &f.rt.calls("run").unwrap()[0],
        attempt,
    );
    f.rt.dispatch(request, StoredClass::KeyedMutation, true)
        .unwrap();
    assert_eq!(f.rt.run("run").unwrap().state, crate::RunState::Failed);
    assert_eq!(
        f.rt.run("run").unwrap().error_kind.as_deref(),
        Some("agent_retired")
    );
    assert!(f.rt.decisions_due().unwrap().is_empty());
    assert_eq!(
        f.rt.decision_cards().unwrap().len(),
        1,
        "only the earlier reconciliation card exists"
    );
}

#[test]
fn grant_loss_after_unknown_disables_without_relabelling_the_earlier_call() {
    let f = Fixture::new("old-agent");
    f.lose_after("run", absence(json!("opaque-uncertain")), json!({}), false);
    assert_eq!(
        f.rt.calls("run").unwrap()[0].dispatch,
        crate::DispatchState::Unknown
    );
    assert_eq!(
        f.rt.unknown_reason("run", 0).unwrap(),
        Some(basal_host::UnknownReason::BasalRestarted)
    );
    assert_eq!(
        f.rt.flow("flow")
            .unwrap()
            .unwrap()
            .disabled_reason
            .as_deref(),
        Some("grant_lost")
    );
    assert_eq!(f.rt.grant_losses().unwrap().len(), 1);
}
