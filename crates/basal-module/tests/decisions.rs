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
use basal_host::core_consent::{
    CoreConsent, FLOW_DECISION_BODY, FlowDecisionBody, decision_request, flow_decision_body,
};
use basal_host::core_host::CoreHost;
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_host::{
    Consent, DecisionAnswer, DecisionCard, DecisionContext, DecisionEvent, DecisionKind,
    DecisionOption, DecisionSink, DisabledReason, SinkError, UnknownReason,
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
            {"id":"not_applied","label":"Send the call again","effect":"choose"},
            {"id":"applied","label":"Continue as applied","effect":"choose"},
            {"id":"cancel","label":"Cancel the run","effect":"choose"},
            {"id":"leave","label":"Leave it unresolved","effect":"decline"}
        ])
    );
    assert_eq!(request["default"], "leave");
    assert_eq!(request["dedup_key"], key);
    assert_eq!(request["scope"], json!({"flow_id":FLOW}));
    assert_eq!(request["target"], json!({"kind":"flow","label":FLOW}));
    // The v1 body core decodes today.
    assert_eq!(
        request["flow_decision"],
        json!({"flow_id":FLOW,"version":1,"decision":"reconcile","run_id":run,"call_key":call.idempotency_key})
    );
    for absent in [
        "session_ref",
        "parent_session_ref",
        "author",
        "subject",
        "preview",
    ] {
        assert!(request.get(absent).is_none(), "{absent} in {request}");
    }
    assert_eq!(
        request["prompt"],
        format!(
            "Run 00:00 UTC of {FLOW} may not have finished: its call to mock.post was sent \
             and then the connection closed before a reply came."
        )
    );
    assert_eq!(fact(request, "Run"), run);
    assert_eq!(fact(request, "Run admitted"), "2026-05-01 00:00:00 UTC");
    assert_eq!(fact(request, "Call"), "mock.post");
    assert_eq!(fact(request, "Sends"), "1");
    assert_eq!(fact(request, "Why unknown"), "connection_lost");
    assert_eq!(request["args_digest"], hex(&call.args_digest.0));
    // The typed context the card is built from, the v2 body to come.
    let record = rig.card(&key);
    assert_eq!(
        record.context().unwrap(),
        DecisionContext::Reconcile {
            run_id: run.clone(),
            run_admitted_at_ms: common::T0,
            call_key: call.idempotency_key.clone(),
            op: "mock.post".into(),
            attempts: 1,
            unknown_reason: UnknownReason::ConnectionLost,
        }
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
        // Core executes nothing for these cards, so basal reports nothing.
        assert!(rig.fake.calls("elicitation.report_execution").is_empty());
    }
}

/// Each place an outcome becomes unknown records its one reason, which
/// the card then shows.
#[test]
fn each_unknown_site_records_its_reason_and_the_card_shows_it() {
    // An op without idempotency keys is sent once; the transport's failure
    // names the reason.
    for (failure, reason) in [
        (
            WireError::Unknown("consumer closed while request was pending".into()),
            UnknownReason::ConnectionLost,
        ),
        (
            WireError::TimedOut("request on channel 2 timed out at its deadline".into()),
            UnknownReason::ReplyTimeout,
        ),
        (
            WireError::Unreadable("expected value at line 1 column 1".into()),
            UnknownReason::ReplyUnreadable,
        ),
    ] {
        let rig = Rig::new("decision-reason", None);
        rig.install(POST);
        rig.fake.enqueue("post", vec![Err(failure)]);
        let run = rig.admit("one").unwrap();
        rig.pass();
        assert_eq!(rig.state(&run), RunState::NeedsReconcile);
        assert_eq!(rig.m().rt.unknown_reason(&run, 0).unwrap(), Some(reason));
        let key = decisions::reconcile_key(FLOW, &run, 0);
        let request = &rig.fake.decision_cards(&key)[0].1[0];
        assert_eq!(fact(request, "Why unknown"), reason.as_str());
    }

    // A digest write honours idempotency keys: it is sent again on every
    // ambiguous failure, and unknown once the retries run out.
    let rig = Rig::new("decision-reason-retries", None);
    rig.install("await sink.digest('SYNAPSE',{title:'x',body:'y'},'piggyback'); return 1;");
    let retries = rig.m().rt.config().unavailable_retries as usize;
    rig.fake.enqueue(
        "sink.digest",
        (0..=retries)
            .map(|_| Err(WireError::Unknown("lost".into())))
            .collect(),
    );
    let run = rig.admit("one").unwrap();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::NeedsReconcile);
    assert_eq!(rig.fake.calls("sink.digest").len(), retries + 1);
    assert_eq!(
        rig.m().rt.unknown_reason(&run, 0).unwrap(),
        Some(UnknownReason::RetriesExhausted)
    );
    let request = &rig
        .fake
        .decision_cards(&decisions::reconcile_key(FLOW, &run, 0))[0]
        .1[0];
    assert_eq!(fact(request, "Call"), "prefrontal-core.sink.digest");

    // basal stops after sending and before saving the reply: the restart
    // finds the call sent with nothing recorded.
    let mut rig = Rig::new("decision-reason-restart", None);
    rig.install(POST);
    rig.crash();
    let probe = Arc::new(Probe::crash_at(
        Point::parse("HostAnswered { position: 0 }#1").unwrap(),
    ));
    rig.start(probe.clone());
    let run = rig.admit("one").unwrap();
    assert!(rig.m().engine.run_until_idle(50).is_err());
    assert!(probe.fired());
    rig.crash();
    rig.start(Arc::new(NoHooks));
    rig.pass();
    assert_eq!(rig.state(&run), RunState::NeedsReconcile);
    assert_eq!(
        rig.m().rt.unknown_reason(&run, 0).unwrap(),
        Some(UnknownReason::BasalRestarted)
    );
    let request = &rig
        .fake
        .decision_cards(&decisions::reconcile_key(FLOW, &run, 0))[0]
        .1[0];
    assert_eq!(
        request["prompt"],
        format!(
            "Run 00:00 UTC of {FLOW} may not have finished: its call to mock.post was sent \
             and then basal restarted before the reply was saved."
        )
    );
}

/// Core answers an expired card with state `expired` and the card's default
/// id as its choice. basal reads every expiry as the decline, whatever id
/// it names: nothing happens and the decision stays visible.
#[test]
fn an_expiry_is_the_decline_whatever_choice_it_names() {
    let rig = Rig::new("decision-expiry", None);
    let run = rig.lost_post(POST);
    let key = decisions::reconcile_key(FLOW, &run, 0);
    let mut expired = rig.fake.answer_decision(&key, None);
    assert_eq!(expired["state"], "expired");
    assert_eq!(
        expired["answered_choice_id"], "leave",
        "core names the default"
    );
    // Even an expiry that named an action would change nothing.
    rig.fake.decisions.lock().unwrap().answers.clear();
    expired["answered_choice_id"] = json!("applied");
    rig.fake.deliver(expired);
    rig.poll();
    rig.pass();
    assert_eq!(rig.state(&run), RunState::NeedsReconcile);
    assert_eq!(rig.posts(), 1);
    let card = rig.card(&key);
    assert_eq!((card.state, card.choice), (CardState::Expired, None));
    let health = rig.m().rt.flow_health().unwrap();
    assert!(
        health
            .iter()
            .any(|f| f.flow_id == FLOW && f.needs_reconcile.contains(&run)),
        "the decision stays visible"
    );
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
    rig.fake.enqueue(
        "post",
        vec![Err(WireError::TimedOut("no reply this time".into()))],
    );
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
    assert_eq!(fact(&requests[1], "Sends"), "2");
    assert_eq!(fact(&requests[1], "Why unknown"), "reply_timeout");
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

/// Rate limits allowing one admission per 60-second window, so a flow's
/// second admission in a window is refused, and that one saturated window
/// disables the flow.
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
            {"id":"reenable","label":"Re-enable the flow","effect":"choose"},
            {"id":"keep","label":"Keep disabled","effect":"decline"}
        ])
    );
    assert_eq!(request["default"], "keep");
    assert_eq!(
        request["flow_decision"],
        json!({"flow_id":FLOW,"version":1,"decision":"reenable"})
    );
    assert_eq!(
        request["prompt"],
        format!(
            "basal disabled {FLOW} at 00:00 UTC: it reached its limit of 1 runs per \
             60-second window in 1 windows in a row."
        )
    );
    assert_eq!(fact(request, "Disabled at"), "2026-05-01 00:00:00 UTC");
    assert_eq!(fact(request, "Why disabled"), "run_limit_saturated");
    assert_eq!(fact(request, "Limit"), "1 runs per 60-second window");
    assert_eq!(fact(request, "Saturated windows in a row"), "1");
    // The rule's numbers as they were when it tripped, kept with the
    // disable.
    assert_eq!(
        rig.card(&key).context().unwrap(),
        DecisionContext::Reenable {
            disabled_at_ms: common::T0,
            disabled_reason: DisabledReason::RunLimitSaturated,
            limit: 1,
            window_ms: 60_000,
            saturated_windows: 1,
        }
    );
    assert_eq!(
        rig.m()
            .rt
            .flow(FLOW)
            .unwrap()
            .unwrap()
            .disabled_by
            .as_deref(),
        Some("runtime")
    );
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

fn reconcile_context(run_id: &str, call_key: &str) -> DecisionContext {
    DecisionContext::Reconcile {
        run_id: run_id.into(),
        run_admitted_at_ms: 1_000,
        call_key: call_key.into(),
        op: "prefrontal-core.sink.digest".into(),
        attempts: 1,
        unknown_reason: UnknownReason::BasalRestarted,
    }
}

fn reenable_context() -> DecisionContext {
    DecisionContext::Reenable {
        disabled_at_ms: 1_000,
        disabled_reason: DisabledReason::DispatchLimitSaturated,
        limit: 600,
        window_ms: 60_000,
        saturated_windows: 3,
    }
}

fn options(list: &[(&str, &str, bool)]) -> Vec<DecisionOption> {
    list.iter()
        .map(|(id, label, decline)| DecisionOption {
            id: (*id).into(),
            label: (*label).into(),
            decline: *decline,
        })
        .collect()
}

/// A card with the inputs of core's v1 test vectors: no dedup key, digest
/// or facts, and core's test title, prompt, options and expiry.
fn vector_card(context: DecisionContext) -> DecisionCard {
    let list: &[(&str, &str, bool)] = match context.kind() {
        DecisionKind::Reconcile => &[
            ("send_again", "Send again", false),
            ("continue", "Continue: it ran", false),
            ("cancel", "Cancel the run", false),
            ("unresolved", "Leave it unresolved", true),
        ],
        DecisionKind::Reenable => &[
            ("reenable", "Re-enable", false),
            ("disabled", "Keep disabled", true),
        ],
    };
    DecisionCard {
        dedup_key: None,
        flow_id: "flow-x".into(),
        version: 1,
        context,
        title: "Decide about flow-x".into(),
        prompt: "How should this flow proceed?".into(),
        args_digest: None,
        facts: Vec::new(),
        options: options(list),
        expires_in_ms: 60_000,
    }
}

const VECTORS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/vectors/flow-decision-card-v1"
);

/// Core's v1 request vectors, copied from prefrontal tag
/// `flow-decision-card-v1` (commit 610b1463f) with the SHA-256 its owner
/// published, and basal's builder producing each byte for byte from the
/// vector's inputs. The vectors fix the shape; basal's own cards carry its
/// own title, prompt, labels and expiry, plus `dedup_key`, `args_digest`
/// and `facts`, which core's decoder accepts.
#[test]
fn the_request_builder_reproduces_core_v1_vectors_byte_for_byte() {
    use sha2::{Digest, Sha256};
    for (file, sha256, card) in [
        (
            "reconcile-request.json",
            "7582f41e2a7264e04a1dc3faf7e974dbe05332e23d910585962f37623a9aab94",
            vector_card(reconcile_context("run-x", "call-x")),
        ),
        (
            "reenable-request.json",
            "d3bde1ef76e580cbdbd55905bee63ad70c7aba6756ed08405a46d4ab4225124d",
            vector_card(reenable_context()),
        ),
    ] {
        let bytes = std::fs::read(format!("{VECTORS}/{file}")).unwrap();
        assert_eq!(
            hex(&Sha256::digest(&bytes)),
            sha256,
            "{file} is the pinned vector"
        );
        let built = decision_request(&card).unwrap();
        let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
        assert_eq!(text, String::from_utf8(bytes).unwrap(), "{file}");
        // And the fake core takes it, as core's own test does.
        let fake = Fake::new();
        assert!(
            fake.management("prefrontal-core", "elicitation.request", built)
                .is_ok(),
            "{file}"
        );
    }
}

/// The v2 body, built now and sent once core publishes v2's vectors: the
/// card's context, typed and required, field for field.
#[test]
fn the_v2_body_carries_the_typed_context_field_for_field() {
    let reconcile = vector_card(reconcile_context("run-x", "call-x"));
    assert_eq!(
        flow_decision_body(&reconcile, FlowDecisionBody::V2),
        json!({"flow_id":"flow-x","version":1,"decision":"reconcile","run_id":"run-x",
            "run_admitted_at_ms":1000,"call_key":"call-x","op":"prefrontal-core.sink.digest",
            "attempts":1,"unknown_reason":"basal_restarted"})
    );
    let reenable = vector_card(reenable_context());
    assert_eq!(
        flow_decision_body(&reenable, FlowDecisionBody::V2),
        json!({"flow_id":"flow-x","version":1,"decision":"reenable","disabled_at_ms":1000,
            "disabled_reason":"dispatch_limit_saturated","limit":600,"window_ms":60000,
            "saturated_windows":3})
    );
    // v1 stays on the wire until core's v2 tag: today's core refuses the v2
    // body's extra keys.
    assert_eq!(FLOW_DECISION_BODY, FlowDecisionBody::V1);
    let mut v2 = decision_request(&reconcile).unwrap();
    v2["flow_decision"] = flow_decision_body(&reconcile, FlowDecisionBody::V2);
    assert!(matches!(
        Fake::new().management("prefrontal-core", "elicitation.request", v2),
        Err(WireError::Refused { .. })
    ));
}

#[test]
fn decision_requests_follow_core_rules_and_the_fake_core_refuses_any_other() {
    let mut good = vector_card(reconcile_context("run-1", "key-1"));
    good.dedup_key = Some("flow_decision:test".into());
    good.args_digest = Some("00".repeat(32));
    good.facts = vec![("Run".into(), "run-1".into())];
    let sent = decision_request(&good).unwrap();
    assert_eq!(sent["dedup_key"], "flow_decision:test");
    assert_eq!(sent["args_digest"], "00".repeat(32));
    assert_eq!(sent["facts"], json!([{"label":"Run","value":"run-1"}]));
    // basal refuses to send a card that breaks core's rules.
    let with = |list: &[(&str, &str, bool)]| DecisionCard {
        options: options(list),
        ..good.clone()
    };
    for bad in [
        with(&[("a", "A", true)]),
        with(&[
            ("a", "A", true),
            ("b", "B", false),
            ("c", "C", false),
            ("d", "D", false),
            ("e", "E", false),
        ]),
        with(&[("a", "A", true), ("b", "B", true)]),
        with(&[("a", "A", false), ("b", "B", false)]),
        with(&[("a", "A", true), ("a", "B", false)]),
        with(&[("a", "A", true), ("b", " ", false)]),
        DecisionCard {
            version: 0,
            ..good.clone()
        },
        DecisionCard {
            expires_in_ms: 0,
            ..good.clone()
        },
        DecisionCard {
            expires_in_ms: 86_400_001,
            ..good.clone()
        },
    ] {
        assert!(decision_request(&bad).is_err(), "{bad:?}");
    }
    // The fake core takes the good request and refuses every breach of
    // core's rules, with core's code.
    let fake = Fake::new();
    let core = |request: Value| fake.management("prefrontal-core", "elicitation.request", request);
    assert!(core(sent.clone()).is_ok());
    let invalid = "elicitation_invalid_request";
    let fail_closed = "elicitation_fail_closed_required";
    let breach = |change: &dyn Fn(&mut Value), code: &'static str| {
        let mut v = sent.clone();
        change(&mut v);
        (v, code)
    };
    for (request, code) in [
        breach(&|v| v["options"][0]["effect"] = json!("grant"), invalid),
        breach(&|v| v["options"][3]["effect"] = json!("choose"), invalid),
        breach(&|v| v["options"] = json!([v["options"][3]]), invalid),
        breach(&|v| v["default"] = json!("send_again"), fail_closed),
        breach(&|v| v["on_expiry"] = json!("allow"), fail_closed),
        breach(&|v| v["session_ref"] = json!("ses-1"), invalid),
        breach(&|v| v["session_ref"] = Value::Null, invalid),
        breach(&|v| v["parent_session_ref"] = Value::Null, invalid),
        breach(&|v| v["subject"] = json!({"label":"x"}), invalid),
        breach(&|v| v["flow_install"] = json!({}), invalid),
        breach(&|v| v["flow_decision"]["attempts"] = json!(1), invalid),
        breach(&|v| v["flow_decision"]["version"] = json!(0), invalid),
        breach(&|v| v["expires_in_ms"] = json!(86_400_001), invalid),
        breach(
            &|v| {
                let o = v.as_object_mut().unwrap();
                o.remove("scope");
                o.remove("args_digest");
            },
            invalid,
        ),
    ] {
        match core(request.clone()) {
            Err(WireError::Refused { code: got, .. }) => assert_eq!(got, code, "{request}"),
            other => panic!("accepted {request}: {other:?}"),
        }
    }
    // Only basal may raise the card.
    *fake.requester.lock().unwrap() = "reserved:plexus".into();
    assert!(matches!(
        core(sent.clone()),
        Err(WireError::Refused { code, .. }) if code == "elicitation_requester_refused"
    ));
    *fake.requester.lock().unwrap() = wire::BASAL.into();
    // `choose` belongs to flow_decision cards only.
    let mut install = sent.clone();
    install["kind"] = json!("flow_install");
    assert!(matches!(core(install), Err(WireError::Refused { .. })));
    // And a flow_decision card takes no execution report.
    let id = core(sent).unwrap()["elicitation_id"].clone();
    assert!(matches!(
        fake.management(
            "prefrontal-core",
            "elicitation.report_execution",
            json!({"elicitation_id":id,"execution":"executed"})
        ),
        Err(WireError::Refused { code, .. }) if code == "elicitation_invalid_execution"
    ));
}
