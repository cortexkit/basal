//! Model calls settle from Broca's bus events: a committed `run.ended` or
//! `run.skipped` wake makes the call due once its body matches the bus
//! digest, and every accepted call keeps a scheduled check, so a lost event
//! costs latency, never an outcome.
use basal_host::broca::{
    BrocaHost, EventRefusal, FIRST_CHECK_MS, MAX_CHECK_MS, RunWake, StateStore, WakeKind,
    fake::{FakeBroca, MemoryStore},
    next_check_at, verify_run_event,
    wire::*,
};
use basal_host::selector::{FakeSelector, ModelSelector, SelectionRequest};
use basal_host::{
    CallRequest, Completion, CompletionAck, CompletionSink, Dispatched, Host, SinkError,
    UnknownOutcome,
};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const START: i64 = 1_800_000_000_000;

fn llm(position: u64) -> CallRequest {
    let key = format!("key:{position}");
    let selection = FakeSelector::default()
        .select(&SelectionRequest {
            caller_class: "flow".into(),
            iq: 40,
            eq: 20,
            flow_id: "f".into(),
            run_id: "r".into(),
            send_id: key.clone(),
        })
        .unwrap();
    CallRequest {
        flow_id: "f".into(),
        run_id: "r".into(),
        position,
        kind: CallKind::Primitive(Primitive::Llm),
        args: JsonText::new(
            json!({"send_id": key, "work_class": "flow:f",
                "session": format!("basal:flow-f:r:{position}"), "op": "llm",
                "max_output": 12, "request": {"prompt": "hello"}, "selection": selection})
            .to_string(),
        )
        .unwrap(),
        idempotency_key: key,
        attempt: 1,
    }
}

#[derive(Default)]
struct Sink {
    completions: Mutex<Vec<Completion>>,
}
impl CompletionSink for Sink {
    fn complete(&self, c: &Completion) -> Result<CompletionAck, SinkError> {
        let mut completions = self.completions.lock().unwrap();
        if completions.iter().any(|old| old == c) {
            return Ok(CompletionAck::Duplicate);
        }
        completions.push(c.clone());
        Ok(CompletionAck::Accepted)
    }
    fn unknown(&self, _: &UnknownOutcome) -> Result<CompletionAck, SinkError> {
        Ok(CompletionAck::Accepted)
    }
}
impl Sink {
    fn texts(&self) -> Vec<Value> {
        self.completions
            .lock()
            .unwrap()
            .iter()
            .map(|c| serde_json::from_str(c.outcome.value.as_str()).unwrap())
            .collect()
    }
}

struct Fixture {
    fake: Arc<FakeBroca>,
    store: Arc<MemoryStore>,
    clock: Arc<AtomicI64>,
    sink: Arc<Sink>,
    host: BrocaHost,
}
impl Fixture {
    fn new() -> Self {
        let fake = Arc::new(FakeBroca::default());
        let store = Arc::new(MemoryStore::default());
        let clock = Arc::new(AtomicI64::new(START));
        let sink = Arc::new(Sink::default());
        let host = Self::host(&fake, &store, &clock);
        host.attach(sink.clone());
        Self {
            fake,
            store,
            clock,
            sink,
            host,
        }
    }
    fn host(fake: &Arc<FakeBroca>, store: &Arc<MemoryStore>, clock: &Arc<AtomicI64>) -> BrocaHost {
        let clock = clock.clone();
        BrocaHost::new(
            fake.clone(),
            store.clone(),
            "/project".into(),
            "basal".into(),
            Arc::new(FakeSelector::default()),
        )
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)))
    }
    /// Dispatches call `position` and commits it, as the runtime does.
    fn accept(&self, position: u64) {
        let request = llm(position);
        assert!(matches!(
            self.host.dispatch_model(&request).unwrap(),
            Dispatched::Accepted { .. }
        ));
        self.host.dispatch_committed(&request);
    }
    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    fn stored(&self, position: u64) -> basal_host::broca::StoredCall {
        self.store.get(&format!("key:{position}")).unwrap().unwrap()
    }
    fn finish(&self, position: u64, text: &str) {
        self.fake
            .finish(
                &format!("key:{position}"),
                text,
                RunFinishReason::Completed,
                Some(Usage {
                    output_tokens: Some(5),
                    ..Usage::default()
                }),
            )
            .unwrap();
    }
}

#[test]
fn a_committed_run_ended_settles_its_call_with_one_read() {
    let f = Fixture::new();
    f.accept(0);
    // The commit reads the call once and schedules its first check.
    assert_eq!(f.fake.calls(), (0, 1));
    assert_eq!(f.stored(0).check_at, Some(START + FIRST_CHECK_MS));
    f.finish(0, "from the event");
    f.store.add_wake(f.fake.publish_ended("key:0").unwrap());
    f.host.wake().unwrap();
    assert_eq!(f.sink.texts(), vec![json!({"text": "from the event"})]);
    // One body read, then one run.result and one run.status.
    assert_eq!(f.fake.event_gets(), 1);
    assert_eq!(f.fake.calls(), (1, 2));
    assert!(f.store.wakes().unwrap().is_empty());
    assert_eq!(
        f.sink.completions.lock().unwrap()[0]
            .outcome
            .usage
            .unwrap()
            .output_tokens,
        Some(5)
    );
    // Settled: nothing is due, and a later pass reads nothing.
    assert_eq!(f.host.next_wake_in(), None);
    f.advance(MAX_CHECK_MS * 4);
    f.host.wake().unwrap();
    assert_eq!(f.fake.calls(), (1, 2));
    assert_eq!(f.fake.event_gets(), 1);
}

#[test]
fn an_idle_host_reads_nothing_and_sleeps_until_woken() {
    let f = Fixture::new();
    f.host.wake().unwrap();
    assert_eq!(f.fake.calls(), (0, 0));
    assert_eq!(f.host.next_wake_in(), None);
}

#[test]
fn a_commit_wakes_the_timer_thread_to_learn_the_new_check() {
    let f = Fixture::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let seen = wakes.clone();
    f.host.set_waker(Arc::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    }));
    f.accept(0);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    f.host.model_events_committed();
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
}

#[test]
fn a_lost_run_ended_settles_at_the_scheduled_check_even_when_archived() {
    for archived in [false, true] {
        let f = Fixture::new();
        f.accept(0);
        f.finish(0, "from run.result");
        if archived {
            f.fake.archive("key:0").unwrap();
        }
        // No event: before the check is due, nothing is read.
        f.advance(FIRST_CHECK_MS - 1);
        f.host.wake().unwrap();
        assert!(f.sink.texts().is_empty());
        assert_eq!(f.fake.calls(), (0, 1));
        f.advance(1);
        f.host.wake().unwrap();
        assert_eq!(
            f.sink.texts(),
            vec![json!({"text": "from run.result"})],
            "archived: {archived}"
        );
        assert_eq!(f.fake.calls(), (1, 2));
        assert_eq!(f.fake.event_gets(), 0);
    }
}

#[test]
fn a_run_still_going_at_its_check_is_read_again_later_never_in_a_loop() {
    let f = Fixture::new();
    f.accept(0);
    f.advance(FIRST_CHECK_MS);
    f.host.wake().unwrap();
    assert_eq!(f.fake.calls(), (0, 2));
    assert_eq!(f.stored(0).check_at, Some(f.now() + 2 * FIRST_CHECK_MS));
    // Repeated passes before the next check read nothing.
    for _ in 0..5 {
        f.host.wake().unwrap();
    }
    assert_eq!(f.fake.calls(), (0, 2));
    f.advance(2 * FIRST_CHECK_MS);
    f.host.wake().unwrap();
    assert_eq!(f.fake.calls(), (0, 3));
    // The wait doubles only up to the cap.
    assert_eq!(f.stored(0).check_at, Some(f.now() + MAX_CHECK_MS));
}

#[test]
fn checks_double_to_a_cap_and_one_falls_before_the_run_deadline() {
    let now = START;
    assert_eq!(next_check_at(now, 0, None), now + FIRST_CHECK_MS);
    assert_eq!(next_check_at(now, 1, None), now + 2 * FIRST_CHECK_MS);
    assert_eq!(next_check_at(now, 2, None), now + MAX_CHECK_MS);
    assert_eq!(next_check_at(now, 40, None), now + MAX_CHECK_MS);
    // A deadline 90 s away pulls the check to 30 s before it.
    assert_eq!(next_check_at(now, 0, Some(now + 90_000)), now + 60_000);
    // A far deadline changes nothing.
    assert_eq!(
        next_check_at(now, 0, Some(now + 3_600_000)),
        now + FIRST_CHECK_MS
    );
    // Once inside the margin, the doubling alone applies: no tight loop.
    assert_eq!(
        next_check_at(now, 0, Some(now + 10_000)),
        now + FIRST_CHECK_MS
    );

    let f = Fixture::new();
    f.store.set_deadline("r", START + 90_000);
    f.accept(0);
    assert_eq!(f.stored(0).check_at, Some(START + 60_000));
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn a_body_that_does_not_match_its_bus_digest_settles_nothing() {
    let f = Fixture::new();
    f.accept(0);
    f.finish(0, "done");
    let mut wake = f.fake.publish_ended("key:0").unwrap();
    wake.digest = sha256("another body");
    f.store.add_wake(wake);
    f.host.wake().unwrap();
    assert!(f.sink.texts().is_empty());
    assert_eq!(f.fake.event_gets(), 1);
    // Only the body was read: no run.result followed the refused wake.
    assert_eq!(f.fake.calls(), (0, 1));
    assert!(f.store.wakes().unwrap().is_empty());
    // The scheduled check still settles the call.
    f.advance(FIRST_CHECK_MS);
    f.host.wake().unwrap();
    assert_eq!(f.sink.texts(), vec![json!({"text": "done"})]);
}

#[test]
fn a_body_naming_another_run_settles_nothing() {
    let f = Fixture::new();
    f.accept(0);
    f.finish(0, "done");
    let call = f.stored(0);
    let jcs = json!({"at_ms": 1, "run_id": "run:someone-else", "seq": 1,
        "structured_output": "absent", "terminal": "completed", "v": 1})
    .to_string();
    f.fake.insert_event(&call.route, "e1", &jcs);
    f.store.add_wake(RunWake {
        send_id: "key:0".into(),
        kind: WakeKind::Ended,
        event_key: "e1".into(),
        digest: sha256(&jcs),
        run_id: call.broca_run_id.clone(),
        session_ref: None,
        seq: None,
    });
    f.host.wake().unwrap();
    assert!(f.sink.texts().is_empty());
    assert_eq!(f.fake.calls(), (0, 1));
}

#[test]
fn an_absent_body_settles_nothing_and_leaves_the_scheduled_check() {
    let f = Fixture::new();
    f.accept(0);
    f.accept(1);
    f.finish(0, "done");
    // The event belongs to another session: on call 0's route Broca
    // answers absent, as it does for a pruned or unknown key.
    let mut wake = f.fake.publish_ended("key:0").unwrap();
    let other = f.stored(1).route;
    let jcs = format!("{{\"stand-in\":\"{}\"}}", wake.event_key);
    f.fake.insert_event(&other, &wake.event_key, &jcs);
    wake.digest = sha256(&jcs);
    f.store.add_wake(wake);
    let reads = f.fake.calls();
    f.host.wake().unwrap();
    assert!(f.sink.texts().is_empty());
    assert_eq!(f.fake.calls(), reads);
    assert!(f.store.wakes().unwrap().is_empty());
    assert_eq!(f.stored(0).check_at, Some(START + FIRST_CHECK_MS));
}

#[test]
fn run_skipped_reads_its_session_call_once() {
    let f = Fixture::new();
    f.accept(0);
    f.accept(1);
    f.finish(0, "ended during the outage");
    f.store.add_wake(f.fake.publish_skipped("key:0").unwrap());
    let reads = f.fake.calls();
    f.host.wake().unwrap();
    assert_eq!(
        f.sink.texts(),
        vec![json!({"text": "ended during the outage"})]
    );
    // Call 1's session was not named: it was not read.
    assert_eq!(f.fake.calls(), (reads.0 + 1, reads.1 + 1));
    assert_eq!(f.stored(1).check_at, Some(START + FIRST_CHECK_MS));
}

#[test]
fn a_skipped_session_no_call_has_learned_rechecks_without_a_body() {
    let f = Fixture::new();
    f.accept(0);
    f.finish(0, "done");
    f.store.add_wake(RunWake {
        send_id: "key:0".into(),
        kind: WakeKind::Recheck,
        event_key: String::new(),
        digest: String::new(),
        run_id: None,
        session_ref: None,
        seq: None,
    });
    f.host.wake().unwrap();
    assert_eq!(f.sink.texts(), vec![json!({"text": "done"})]);
    assert_eq!(f.fake.event_gets(), 0);
}

#[test]
fn a_restart_mid_call_reads_it_once_and_a_stale_wake_reads_nothing() {
    let f = Fixture::new();
    f.accept(0);
    f.finish(0, "done while down");
    f.store.add_wake(f.fake.publish_ended("key:0").unwrap());
    let Fixture {
        fake, store, clock, ..
    } = f;
    let sink = Arc::new(Sink::default());
    let restarted = Fixture::host(&fake, &store, &clock);
    // Attaching the sink reads every pending call once.
    restarted.attach(sink.clone());
    assert_eq!(sink.texts(), vec![json!({"text": "done while down"})]);
    let reads = fake.calls();
    // The wake committed before the restart names a settled call now.
    restarted.wake().unwrap();
    assert_eq!(fake.calls(), reads);
    assert_eq!(fake.event_gets(), 0);
    assert!(store.wakes().unwrap().is_empty());
    assert_eq!(sink.completions.lock().unwrap().len(), 1);
}

// Broca's captured samples, pinned in test-vectors/broca-run-events. The
// event_get files are the exact reply bytes; the bus files hold the subject,
// message id, headers and digest of the same events.
const ENDED_BUS: &str = include_str!("../../../test-vectors/broca-run-events/run_ended.bus.json");
const ENDED_GET: &str =
    include_str!("../../../test-vectors/broca-run-events/run_ended.event_get.json");
const SKIPPED_BUS: &str =
    include_str!("../../../test-vectors/broca-run-events/run_skipped.bus.json");
const SKIPPED_GET: &str =
    include_str!("../../../test-vectors/broca-run-events/run_skipped.event_get.json");
const SUMS: &str = include_str!("../../../test-vectors/broca-run-events/SHA256SUMS");

fn vector(bus: &str, get: &str) -> (RunWake, RunEventReply) {
    let bus: Value = serde_json::from_str(bus).unwrap();
    let header = |name: &str| bus["headers"][name].as_str().map(str::to_owned);
    let kind = match header("kind").as_deref() {
        Some("run.ended") => WakeKind::Ended,
        Some("run.skipped") => WakeKind::Skipped,
        other => panic!("unexpected kind {other:?}"),
    };
    let wake = RunWake {
        send_id: "key:0".into(),
        kind,
        event_key: bus["message_id"].as_str().unwrap().into(),
        digest: bus["digest_sha256_of_jcs"].as_str().unwrap().into(),
        run_id: header("run_id"),
        session_ref: header("session_ref"),
        seq: header("seq"),
    };
    let reply: Value = serde_json::from_str(get).unwrap();
    (
        wake,
        serde_json::from_value(reply["result"].clone()).unwrap(),
    )
}

#[test]
fn the_captured_vectors_are_the_pinned_bytes() {
    for (name, bytes) in [
        ("run_ended.bus.json", ENDED_BUS),
        ("run_ended.event_get.json", ENDED_GET),
        ("run_skipped.bus.json", SKIPPED_BUS),
        ("run_skipped.event_get.json", SKIPPED_GET),
    ] {
        let line = format!("{}  {name}", sha256(bytes));
        assert!(SUMS.lines().any(|l| l == line), "{name} changed");
    }
}

#[test]
fn captured_run_ended_and_run_skipped_bodies_match_their_bus_messages() {
    let (wake, reply) = vector(ENDED_BUS, ENDED_GET);
    assert_eq!(
        wake.digest,
        "a631c4b8aa7f3a6278cb3d63b02777da21579c10e827137244006930c297b83e"
    );
    let body = verify_run_event(&wake, &reply).unwrap();
    assert_eq!(body["terminal"], "completed");
    assert_eq!(body["run_id"], "run-sid-w1Y2RodKlruk2D2XU3jgn6");

    let (wake, reply) = vector(SKIPPED_BUS, SKIPPED_GET);
    // A skipped marker names no run: its headers have no run_id key.
    assert_eq!(wake.run_id, None);
    let body = verify_run_event(&wake, &reply).unwrap();
    assert_eq!(body["to_seq"], 33);
}

#[test]
fn captured_bodies_are_refused_when_anything_disagrees() {
    let (wake, reply) = vector(ENDED_BUS, ENDED_GET);
    let refuse = |wake: &RunWake, reply: &RunEventReply| verify_run_event(wake, reply).unwrap_err();

    let mut tampered = reply.clone();
    tampered.jcs = Some(
        tampered
            .jcs
            .unwrap()
            .replace("\"completed\"", "\"cancelled\""),
    );
    assert_eq!(refuse(&wake, &tampered), EventRefusal::Mismatch("digest"));

    let mut absent = reply.clone();
    absent.state = "absent".into();
    assert_eq!(refuse(&wake, &absent), EventRefusal::Absent);

    let mut other_key = wake.clone();
    other_key.event_key = "0".repeat(64);
    assert_eq!(
        refuse(&other_key, &reply),
        EventRefusal::Mismatch("event key")
    );

    let mut other_run = wake.clone();
    other_run.run_id = Some("run-other".into());
    assert_eq!(refuse(&other_run, &reply), EventRefusal::Mismatch("run id"));

    let mut other_session = wake.clone();
    other_session.session_ref = Some("0".repeat(32));
    assert_eq!(
        refuse(&other_session, &reply),
        EventRefusal::Mismatch("session")
    );

    let mut other_seq = wake.clone();
    other_seq.seq = Some("370".into());
    assert_eq!(
        refuse(&other_seq, &reply),
        EventRefusal::Mismatch("sequence")
    );

    let (wake, reply) = vector(SKIPPED_BUS, SKIPPED_GET);
    let mut other_session = wake.clone();
    other_session.session_ref = Some("0".repeat(32));
    assert_eq!(
        refuse(&other_session, &reply),
        EventRefusal::Mismatch("session")
    );
}
