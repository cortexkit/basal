//! Operator decision cards end to end: the module raises them on the fake
//! core's consent plane through `CoreConsent`, and core's answers come back
//! through `elicitation.answers`, as for install cards.

mod common;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use basal_core::decisions::{self, CardState};
use basal_core::{Clock, Config, Durability, Hooks, NoHooks, RateLimits, RunState};
use basal_host::core_consent::{CoreConsent, decision_request};
use basal_host::core_host::CoreHost;
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_host::{
    Consent, DecisionAnswer, DecisionCard, DecisionEvent, DecisionKind, DecisionOption,
    DecisionSink, SinkError,
};
use basal_module::caller::Caller;
use basal_module::dryrun::DryRunConfig;
use basal_module::engine::EngineConfig;
use basal_module::module::{Hosts, Module, ModuleConfig};
use basal_module::pool::{PoolConfig, ProcessSpawner};
use basal_module::unconfigured::UnconfiguredHost;
use basal_proto::{JsonText, Settlement};
use basal_testkit::harness::{Point, Probe};
use serde_json::{Value, json};
use wire::Fake;

const FLOW: &str = "host-flow";

/// Calls `mock.post` (a mutation without idempotency keys) and reports how
/// it ended: `ran` when it returned, otherwise the rejection's code.
const POST: &str = "try { await ops.call('mock','post',{n:1}); return 'ran'; } \
                    catch (e) { return e.data.code; }";

fn manifest() -> Value {
    json!({"id":FLOW,"version":1,"purpose":"Post for Synapse","trigger":{"schedule":{"interval":"1h"}},"sinks":[{"agent":"SYNAPSE","digest_max":"piggyback"}],"ops":[{"module":"mock","op":"echo"},{"module":"mock","op":"post"}]})
}

/// A sink that takes nothing, attached before a module is dropped so the
/// consent adapter stops holding the dropped module's runtime (and with it
/// the store's lease).
struct Detached;
impl DecisionSink for Detached {
    fn decide(&self, _: &DecisionEvent) -> Result<(), SinkError> {
        Err(SinkError("no module is running".into()))
    }
    fn answer(&self, _: &DecisionAnswer) -> Result<(), SinkError> {
        Err(SinkError("no module is running".into()))
    }
}

/// A module over one store that can be crashed and started again, against
/// the fake core.
struct Rig {
    dir: PathBuf,
    fake: Arc<Fake>,
    consent: Arc<CoreConsent>,
    config: ModuleConfig,
    pool: PoolConfig,
    module: Option<Module>,
}

impl Rig {
    fn new(tag: &str, rate: Option<RateLimits>) -> Self {
        let dir = common::scratch(tag);
        let pool = PoolConfig::new(basal_testkit::channel::worker_binary());
        let mut runtime = Config {
            clock: Clock::manual(common::T0),
            ..Default::default()
        };
        if let Some(rate) = rate {
            runtime.rate = rate;
        }
        let config = ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability { fullfsync: false },
            runtime,
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry")),
        };
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let mut rig = Self {
            dir,
            fake,
            consent,
            config,
            pool,
            module: None,
        };
        rig.start(Arc::new(NoHooks));
        rig
    }

    fn start(&mut self, hooks: Arc<dyn Hooks>) {
        let catalog = Arc::new(SubcCatalog::new(self.fake.clone()));
        let hosts = Hosts {
            host: Arc::new(RoutingHost::new(
                Arc::new(ModuleOpsHost::new(self.fake.clone(), catalog.clone())),
                Arc::new(CoreHost::new(self.fake.clone())),
                Arc::new(UnconfiguredHost::new()),
            )),
            catalog,
            consent: self.consent.clone(),
            hooks,
        };
        let module = Module::start(
            self.config.clone(),
            hosts,
            Arc::new(ProcessSpawner::new(&self.pool)),
        )
        .expect("module starts");
        self.module = Some(module);
    }

    /// Stops the module the way a crash would: nothing more is written,
    /// and the next start recovers from the store alone.
    fn crash(&mut self) {
        if let Some(module) = self.module.take() {
            module.rt.quiesce();
            module.pool.stop();
            self.consent.attach(Arc::new(Detached));
        }
    }

    fn m(&self) -> &Module {
        self.module.as_ref().expect("a running module")
    }

    fn pass(&self) {
        self.m().engine.run_until_idle(50).expect("passes");
    }

    /// Installs `script` as the operator and approves its install card.
    fn install(&self, script: &str) {
        self.m()
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"script":script,"manifest":manifest().to_string()}),
            )
            .expect("install");
        let flow = self.fake.calls("elicitation.request")[0]["params"]["flow_install"].clone();
        self.fake.enqueue(
            "elicitation.answers",
            vec![Ok(json!({"records":[{"state":"answered","answered_choice_id":"approve","flow_install":flow}],"cursor":0}))],
        );
        self.consent.poll_once().expect("approval applied");
    }

    fn admit(&self, trigger: &str) -> Option<String> {
        self.m()
            .rt
            .admit_trigger(FLOW, trigger, JsonText::null())
            .expect("admission")
            .run_id()
            .map(str::to_owned)
    }

    /// Installs `script`, makes its first `mock.post` lose its reply, and
    /// runs it into `needs_reconcile`. Returns the run.
    fn lost_post(&self, script: &str) -> String {
        self.install(script);
        self.lose_post()
    }

    /// Runs an installed flow whose first `mock.post` loses its reply into
    /// `needs_reconcile`. Returns the run.
    fn lose_post(&self) -> String {
        self.fake.enqueue(
            "post",
            vec![Err(WireError::Unknown("the reply was lost".into()))],
        );
        let run = self.admit("one").expect("admitted");
        self.pass();
        assert_eq!(self.state(&run), RunState::NeedsReconcile);
        run
    }

    fn state(&self, run: &str) -> RunState {
        self.m().rt.run(run).unwrap().state
    }

    fn poll(&self) {
        self.consent.poll_once().expect("answers applied");
    }

    fn card(&self, key: &str) -> decisions::DecisionRecord {
        let cards: Vec<_> = self
            .m()
            .rt
            .decision_cards()
            .unwrap()
            .into_iter()
            .filter(|c| c.dedup_key == key)
            .collect();
        cards.last().cloned().expect("a card under the key")
    }

    /// The audit rows written through the card with this elicitation id,
    /// as (action, run, position).
    fn audit(&self, elicitation_id: &str) -> Vec<(String, Option<String>, Option<i64>)> {
        self.m()
            .rt
            .store()
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT action, run_id, position FROM audit WHERE elicitation_id = ?1 \
                     ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map([elicitation_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn posts(&self) -> usize {
        self.fake.calls("post").len()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.crash();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fact<'a>(request: &'a Value, label: &str) -> &'a str {
    request["facts"]
        .as_array()
        .and_then(|facts| facts.iter().find(|f| f["label"] == label))
        .and_then(|f| f["value"].as_str())
        .unwrap_or_else(|| panic!("no fact {label} in {request}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_run_in_needs_reconcile_raises_one_card_per_unknown_call_to_the_operator_only() {
    let rig = Rig::new("decision-reconcile-card", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    let cards = rig.fake.decision_cards(&key);
    assert_eq!(cards.len(), 1, "one card for the unknown call");
    let request = &cards[0].1[0];
    let call = rig.m().rt.calls(&run).unwrap()[0].clone();
    assert_eq!(request["kind"], "flow_decision");
    assert_eq!(
        request["options"],
        json!([
            {"id":"leave","label":"Leave it unresolved","effect":"decline"},
            {"id":"not_applied","label":"It never ran: send it again","effect":"choose"},
            {"id":"applied","label":"It ran: continue","effect":"choose"},
            {"id":"cancel","label":"Cancel the run","effect":"choose"}
        ])
    );
    assert_eq!(request["default"], "leave");
    assert_eq!(request["dedup_key"], key);
    assert_eq!(
        request["flow_decision"],
        json!({"flow_id":FLOW,"version":1,"decision":"reconcile","run_id":run,"call_key":call.idempotency_key})
    );
    for absent in ["session_ref", "author", "subject"] {
        assert!(request.get(absent).is_none(), "{absent} in {request}");
    }
    assert_eq!(fact(request, "flow"), FLOW);
    assert_eq!(fact(request, "version"), "1");
    assert_eq!(fact(request, "run"), run);
    assert!(fact(request, "op").contains("post"), "{request}");
    assert_eq!(fact(request, "arguments digest"), hex(&call.args_digest.0));
    assert_eq!(request["args_digest"], hex(&call.args_digest.0));
    assert_eq!(fact(request, "sent at"), "2026-05-01T00:00:00Z");
    assert!(
        fact(request, "error").contains("the reply was lost"),
        "{request}"
    );
    let card = rig.card(&key);
    assert_eq!(card.elicitation_id.as_deref(), Some(cards[0].0.as_str()));
    assert_eq!(card.state, CardState::Open);
    // Later passes find the card raised and send nothing more.
    rig.pass();
    rig.pass();
    assert_eq!(rig.fake.decision_cards(&key)[0].1.len(), 1);
    assert_eq!(rig.fake.all_decision_cards().len(), 1);
}

#[test]
fn each_reconcile_option_and_the_default_on_expiry_apply_through_the_journaled_path() {
    for (choice, state, result, posts, card_state, action) in [
        (
            Some("leave"),
            RunState::NeedsReconcile,
            None,
            1,
            CardState::Declined,
            "decision.declined",
        ),
        (
            None,
            RunState::NeedsReconcile,
            None,
            1,
            CardState::Expired,
            "decision.expired",
        ),
        (
            Some("not_applied"),
            RunState::Succeeded,
            Some("\"ran\""),
            2,
            CardState::Applied,
            "reconcile.not_applied",
        ),
        (
            Some("applied"),
            RunState::Succeeded,
            Some("\"reconciled_as_applied\""),
            1,
            CardState::Applied,
            "reconcile.applied",
        ),
        (
            Some("cancel"),
            RunState::Cancelled,
            None,
            1,
            CardState::Applied,
            "reconcile.cancel",
        ),
    ] {
        let rig = Rig::new("decision-reconcile-option", None);
        let run = rig.lost_post(POST);
        let key = decisions::reconcile_key(FLOW, &run, 0);
        let answer = rig.fake.answer_decision(&key, choice);
        rig.poll();
        rig.pass();
        let done = rig.m().rt.run(&run).unwrap();
        assert_eq!(done.state, state, "{choice:?}: {done:?}");
        assert_eq!(done.result.as_deref(), result, "{choice:?}");
        assert_eq!(rig.posts(), posts, "{choice:?}");
        let card = rig.card(&key);
        assert_eq!(card.state, card_state, "{choice:?}");
        assert_eq!(card.choice.as_deref(), choice);
        let elicitation = answer["elicitation_id"].as_str().unwrap();
        assert_eq!(
            rig.audit(elicitation),
            vec![(action.to_owned(), Some(run.clone()), Some(0))],
            "{choice:?}"
        );
        // The default leaves the decision where it was, and no second card
        // is raised for it.
        rig.pass();
        assert_eq!(rig.fake.all_decision_cards().len(), 1, "{choice:?}");
    }
}

#[test]
fn a_duplicate_answer_changes_nothing() {
    for choice in ["not_applied", "leave"] {
        let rig = Rig::new("decision-duplicate", None);
        let run = rig.lost_post(POST);
        let key = decisions::reconcile_key(FLOW, &run, 0);
        let answer = rig.fake.answer_decision(&key, Some(choice));
        rig.poll();
        rig.pass();
        let posts = rig.posts();
        let before = rig.m().rt.run(&run).unwrap();
        rig.fake.deliver(answer.clone());
        rig.fake.deliver(answer.clone());
        rig.poll();
        rig.pass();
        assert_eq!(rig.posts(), posts, "{choice}");
        assert_eq!(rig.m().rt.run(&run).unwrap().state, before.state);
        let elicitation = answer["elicitation_id"].as_str().unwrap();
        assert_eq!(rig.audit(elicitation).len(), 1, "{choice}: applied once");
        assert!(rig.fake.calls("elicitation.ack").len() >= 2);
    }
}

#[test]
fn an_answer_after_the_decision_was_settled_another_way_is_stale() {
    // The operator reconciled the call with flow.reconcile first.
    let rig = Rig::new("decision-stale-op", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    rig.m()
        .handle(
            &Caller::Operator,
            "flow.reconcile",
            json!({"run_id":run,"position":0,"resolution":"not_applied"}),
        )
        .unwrap();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::Succeeded);
    let answer = rig.fake.answer_decision(&key, Some("applied"));
    rig.poll();
    rig.pass();
    let done = rig.m().rt.run(&run).unwrap();
    assert_eq!(done.result.as_deref(), Some("\"ran\""));
    assert_eq!(rig.posts(), 2);
    assert_eq!(rig.card(&key).state, CardState::Stale);
    let elicitation = answer["elicitation_id"].as_str().unwrap();
    assert_eq!(
        rig.audit(elicitation),
        vec![("decision.stale".to_owned(), Some(run.clone()), Some(0))]
    );
    assert_eq!(rig.fake.all_decision_cards().len(), 1);

    // The run was cancelled.
    let rig = Rig::new("decision-stale-cancel", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    rig.m().rt.cancel(&run, "operator").unwrap();
    rig.fake.answer_decision(&key, Some("not_applied"));
    rig.poll();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::Cancelled);
    assert_eq!(rig.posts(), 1);
    assert_eq!(rig.card(&key).state, CardState::Stale);
}

#[test]
fn a_changed_decision_updates_its_card_and_another_unknown_call_gets_its_own() {
    let rig = Rig::new("decision-update", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    // The operator sends the call again with flow.reconcile and it is lost
    // again: the same call needs a new decision, and the card still open
    // in core is updated to show it.
    rig.fake
        .enqueue("post", vec![Err(WireError::Unknown("lost again".into()))]);
    rig.m()
        .handle(
            &Caller::Operator,
            "flow.reconcile",
            json!({"run_id":run,"position":0,"resolution":"not_applied"}),
        )
        .unwrap();
    rig.pass();
    rig.m().rt.quiesce();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::NeedsReconcile);
    let cards = rig.fake.decision_cards(&key);
    assert_eq!(cards.len(), 1, "one card, updated");
    let requests = &cards[0].1;
    assert_eq!(requests.len(), 2, "raised again under its key");
    assert_eq!(fact(&requests[1], "attempt"), "2");
    assert!(fact(&requests[1], "error").contains("lost again"));
    let card = rig.card(&key);
    assert_eq!((card.revision, card.raised_revision), (2, Some(2)));
    // The answer decides the call as it is now.
    rig.fake.answer_decision(&key, Some("applied"));
    rig.poll();
    rig.pass();
    let done = rig.m().rt.run(&run).unwrap();
    assert_eq!(done.result.as_deref(), Some("\"reconciled_as_applied\""));

    // Two calls lost in one run: each is decided on its own card.
    let rig = Rig::new("decision-two-calls", None);
    rig.install(
        "const r = await Promise.allSettled([ops.call('mock','post',{n:1}), \
         ops.call('mock','post',{n:2})]); return r.map(x => x.status);",
    );
    rig.fake.enqueue(
        "post",
        vec![
            Err(WireError::Unknown("lost".into())),
            Err(WireError::Unknown("lost".into())),
        ],
    );
    let run = rig.admit("two").unwrap();
    rig.pass();
    rig.m().rt.quiesce();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::NeedsReconcile);
    for position in [0, 1] {
        assert_eq!(
            rig.fake
                .decision_cards(&decisions::reconcile_key(FLOW, &run, position))
                .len(),
            1,
            "position {position}"
        );
    }
    assert_eq!(rig.fake.all_decision_cards().len(), 2);
}

#[test]
fn a_crash_between_the_intent_and_core_accepting_still_shows_exactly_one_card() {
    // Core never received the request.
    let mut rig = Rig::new("decision-crash-unsent", None);
    rig.install(POST);
    rig.fake.enqueue(
        "elicitation.request",
        vec![Err(WireError::NeverSent("the socket closed".into()))],
    );
    let run = rig.lose_post();
    let key = decisions::reconcile_key(FLOW, &run, 0);
    assert!(rig.fake.decision_cards(&key).is_empty());
    let intent = rig.card(&key);
    assert_eq!(intent.raised_revision, None, "the intent is durable");
    rig.crash();
    rig.start(Arc::new(NoHooks));
    rig.pass();
    let cards = rig.fake.decision_cards(&key);
    assert_eq!(cards.len(), 1);
    assert_eq!(
        rig.card(&key).elicitation_id.as_deref(),
        Some(cards[0].0.as_str())
    );

    // Core accepted the card and the reply was lost.
    let mut rig = Rig::new("decision-crash-accepted", None);
    rig.fake.decisions.lock().unwrap().lose_replies = 1;
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    assert_eq!(rig.card(&key).raised_revision, None);
    rig.crash();
    rig.start(Arc::new(NoHooks));
    rig.pass();
    let cards = rig.fake.decision_cards(&key);
    assert_eq!(cards.len(), 1, "core's dedup key shows one card");
    assert_eq!(cards[0].1.len(), 2, "raised once more after the restart");
    assert_eq!(
        rig.card(&key).elicitation_id.as_deref(),
        Some(cards[0].0.as_str())
    );
    assert_eq!(rig.fake.all_decision_cards().len(), 1);
}

#[test]
fn a_crash_between_receiving_an_answer_and_applying_it_applies_it_exactly_once() {
    let mut rig = Rig::new("decision-crash-answer", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    let answer = rig.fake.answer_decision(&key, Some("not_applied"));
    let acks = rig.fake.calls("elicitation.ack").len();
    // The store fails under the answer: nothing is recorded, and the page
    // is not acknowledged, so core keeps the answer.
    rig.m().rt.store().cut();
    assert!(rig.consent.poll_once().is_err());
    assert_eq!(rig.fake.calls("elicitation.ack").len(), acks);
    rig.crash();
    rig.start(Arc::new(NoHooks));
    assert_eq!(rig.card(&key).state, CardState::Open);
    rig.poll();
    rig.pass();
    // Core delivering the same page again changes nothing.
    rig.fake.deliver(answer.clone());
    rig.poll();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::Succeeded);
    assert_eq!(rig.posts(), 2, "sent again exactly once");
    let elicitation = answer["elicitation_id"].as_str().unwrap();
    assert_eq!(
        rig.audit(elicitation),
        vec![("reconcile.not_applied".to_owned(), Some(run), Some(0))]
    );
    assert_eq!(rig.card(&key).state, CardState::Applied);
}

#[test]
fn reconciled_as_applied_is_journaled_and_replays_the_same_rejection() {
    let script = "let r; try { await ops.call('mock','post',{n:1}); r = 'ran'; } \
                  catch (e) { r = e.data.code; } \
                  const t = await ops.call('mock','echo',{r}); return [r, t.r];";
    let expected = r#"{"code":"reconciled_as_applied","message":"the operator reconciled this call as applied; its result was not observed"}"#;
    let mut rig = Rig::new("decision-replay", None);
    let run = rig.lost_post(script);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    // The activation that continues the run crashes once the echo after
    // the released rejection is answered, so the next start replays the
    // run from the top over the recorded rejection.
    rig.crash();
    let probe = Arc::new(Probe::crash_at(
        Point::parse("HostAnswered { position: 1 }#1").unwrap(),
    ));
    rig.start(probe.clone());
    rig.fake.answer_decision(&key, Some("applied"));
    rig.poll();
    assert!(rig.m().engine.run_until_idle(50).is_err());
    assert!(probe.fired());
    rig.crash();
    rig.start(Arc::new(NoHooks));
    let recorded = rig.m().rt.calls(&run).unwrap()[0].outcome.clone();
    assert!(recorded.is_some(), "released before the crash");
    rig.pass();
    let done = rig.m().rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded, "{done:?}");
    assert_eq!(
        done.result.as_deref(),
        Some("[\"reconciled_as_applied\",\"reconciled_as_applied\"]")
    );
    assert_eq!(rig.posts(), 1, "the call itself is never sent again");
    assert_eq!(rig.fake.calls("echo").len(), 2, "the run was replayed");
    let outcome = rig.m().rt.calls(&run).unwrap()[0].outcome.clone().unwrap();
    assert_eq!(
        Some(outcome.clone()),
        recorded,
        "the same release, replayed"
    );
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(outcome.value.as_str(), expected);
    assert_eq!(outcome.delivery_order, 0);
}

/// Rate limits that refuse a flow's second admission within one 60-second
/// window and disable the flow on that first saturated window.
fn tight() -> RateLimits {
    RateLimits {
        window: Duration::from_secs(60),
        max_runs: 1,
        max_dispatches: 600,
        saturated_windows_to_disable: 1,
    }
}

fn auto_disabled(tag: &str) -> Rig {
    let rig = Rig::new(tag, Some(tight()));
    rig.install("return 1;");
    rig.admit("first").expect("the first run is admitted");
    assert!(rig.admit("second").is_none(), "the second is refused");
    assert!(!rig.m().rt.flow(FLOW).unwrap().unwrap().enabled);
    rig.pass();
    rig
}

#[test]
fn an_auto_disabled_flow_raises_one_reenable_card_and_each_option_applies() {
    let key = decisions::reenable_key(FLOW, 1);
    let rig = auto_disabled("decision-reenable-card");
    let cards = rig.fake.decision_cards(&key);
    assert_eq!(cards.len(), 1);
    let request = &cards[0].1[0];
    assert_eq!(
        request["options"],
        json!([
            {"id":"keep","label":"Keep disabled","effect":"decline"},
            {"id":"reenable","label":"Re-enable","effect":"choose"}
        ])
    );
    assert_eq!(request["default"], "keep");
    assert_eq!(
        request["flow_decision"],
        json!({"flow_id":FLOW,"version":1,"decision":"reenable"})
    );
    assert_eq!(fact(request, "rule"), "rate_saturation");
    assert_eq!(fact(request, "limit that refused"), "runs");
    assert_eq!(fact(request, "consecutive saturated windows"), "1");
    assert_eq!(fact(request, "runs allowed per window"), "1");
    assert_eq!(fact(request, "window"), "60000 ms");
    rig.pass();
    assert_eq!(rig.fake.all_decision_cards().len(), 1);

    for (choice, enabled, action) in [
        (Some("keep"), false, "decision.declined"),
        (None, false, "decision.expired"),
        (Some("reenable"), true, "flow.reenable"),
    ] {
        let rig = auto_disabled("decision-reenable-option");
        let answer = rig.fake.answer_decision(&key, choice);
        rig.poll();
        rig.pass();
        assert_eq!(
            rig.m().rt.flow(FLOW).unwrap().unwrap().enabled,
            enabled,
            "{choice:?}"
        );
        let elicitation = answer["elicitation_id"].as_str().unwrap();
        assert_eq!(
            rig.audit(elicitation),
            vec![(action.to_owned(), None, None)],
            "{choice:?}"
        );
        assert_eq!(rig.fake.all_decision_cards().len(), 1, "{choice:?}");
        if enabled {
            // A second auto-disable is a new episode with its own card.
            assert!(rig.admit("third").is_none());
            rig.pass();
            assert_eq!(
                rig.fake
                    .decision_cards(&decisions::reenable_key(FLOW, 2))
                    .len(),
                1
            );
            assert_eq!(rig.fake.all_decision_cards().len(), 2);
        }
    }

    // The operator re-enabled the flow directly: the card's answer is stale.
    let rig = auto_disabled("decision-reenable-stale");
    rig.m()
        .handle(&Caller::Operator, "flow.enable", json!({"flow_id":FLOW}))
        .unwrap();
    rig.m()
        .handle(&Caller::Operator, "flow.disable", json!({"flow_id":FLOW}))
        .unwrap();
    let answer = rig.fake.answer_decision(&key, Some("reenable"));
    rig.poll();
    assert!(
        !rig.m().rt.flow(FLOW).unwrap().unwrap().enabled,
        "the operator's own disable stands"
    );
    assert_eq!(rig.card(&key).state, CardState::Stale);
    let elicitation = answer["elicitation_id"].as_str().unwrap();
    assert_eq!(
        rig.audit(elicitation),
        vec![("decision.stale".to_owned(), None, None)]
    );
}

fn card(kind: DecisionKind, options: &[(&str, bool)]) -> DecisionCard {
    let reconcile = kind == DecisionKind::Reconcile;
    DecisionCard {
        dedup_key: "flow_decision:test".into(),
        flow_id: "flow-x".into(),
        version: 2,
        decision: kind,
        run_id: reconcile.then(|| "run-1".into()),
        call_key: reconcile.then(|| "key-1".into()),
        title: "Title".into(),
        prompt: "Prompt".into(),
        args_digest: "00".repeat(32),
        facts: vec![("flow".into(), "flow-x".into())],
        options: options
            .iter()
            .map(|(id, decline)| DecisionOption {
                id: (*id).into(),
                label: format!("Label {id}"),
                decline: *decline,
            })
            .collect(),
    }
}

#[test]
fn decision_requests_follow_core_rules_and_the_fake_core_refuses_any_other() {
    let reconcile = DecisionKind::Reconcile;
    let good = card(
        reconcile,
        &[("leave", true), ("not_applied", false), ("applied", false)],
    );
    let sent = decision_request(&good).unwrap();
    assert_eq!(
        sent,
        json!({"kind":"flow_decision","title":"Title","prompt":"Prompt",
            "options":[{"id":"leave","label":"Label leave","effect":"decline"},
                {"id":"not_applied","label":"Label not_applied","effect":"choose"},
                {"id":"applied","label":"Label applied","effect":"choose"}],
            "default":"leave","urgency":"normal","on_expiry":"deny","material_damage":false,
            "late_execution":"notify_only","args_digest":"00".repeat(32),
            "dedup_key":"flow_decision:test","target":{"kind":"flow","label":"flow-x v2"},
            "facts":[{"label":"flow","value":"flow-x"}],"expires_in_ms":86400000,
            "flow_decision":{"flow_id":"flow-x","version":2,"decision":"reconcile","run_id":"run-1","call_key":"key-1"}})
    );
    // basal refuses to send a card that breaks core's rules.
    for bad in [
        card(reconcile, &[("leave", true)]),
        card(
            reconcile,
            &[
                ("a", true),
                ("b", false),
                ("c", false),
                ("d", false),
                ("e", false),
            ],
        ),
        card(reconcile, &[("a", true), ("b", true)]),
        card(reconcile, &[("a", false), ("b", false)]),
        card(reconcile, &[("a", true), ("a", false)]),
        DecisionCard {
            run_id: None,
            ..good.clone()
        },
        DecisionCard {
            decision: DecisionKind::Reenable,
            ..good.clone()
        },
    ] {
        assert!(decision_request(&bad).is_err(), "{bad:?}");
    }
    // The fake core takes the good request and refuses every breach.
    let fake = Fake::new();
    let core = |request: Value| fake.management("prefrontal-core", "elicitation.request", request);
    assert!(core(sent.clone()).is_ok());
    let mut breaches = Vec::new();
    let mut v = sent.clone();
    v["options"][1]["effect"] = json!("grant");
    breaches.push(v);
    let mut v = sent.clone();
    v["default"] = json!("not_applied");
    breaches.push(v);
    let mut v = sent.clone();
    v["session_ref"] = json!("ses-1");
    breaches.push(v);
    let mut v = sent.clone();
    v["on_expiry"] = json!("allow");
    breaches.push(v);
    let mut v = sent.clone();
    v.as_object_mut().unwrap().remove("dedup_key");
    breaches.push(v);
    let mut v = sent.clone();
    v["options"][0]["effect"] = json!("choose");
    breaches.push(v);
    let mut v = sent.clone();
    v["flow_decision"]["decision"] = json!("reenable");
    breaches.push(v);
    let mut v = sent.clone();
    v["options"] = json!([sent["options"][0]]);
    breaches.push(v);
    for breach in breaches {
        match core(breach.clone()) {
            Err(WireError::Refused { code, .. }) => {
                assert_eq!(code, "elicitation_invalid_request", "{breach}")
            }
            other => panic!("accepted {breach}: {other:?}"),
        }
    }
}
