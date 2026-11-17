use super::*;
use basal_host::{
    Consent, ConsentError, DecisionAnswer, DecisionEvent, DecisionPath, DecisionSink, InstallCard,
    InstallOutcome, SinkError,
    cingulate_consent::CingulateConsent,
    transport::{Transport, WireError},
};
use basal_module::events::{commit_batch_with_consent, decode, test_control_payload};
use serde_json::Value;
use std::sync::{Condvar, Mutex, atomic::AtomicBool, mpsc};

const SAMPLE_SUBJECT: &str =
    include_str!("../../../../test-vectors/card-settled-v1/basal.basal.subject.txt");
const SAMPLE_BODY: &[u8] =
    include_bytes!("../../../../test-vectors/card-settled-v1/basal.basal.body.json");
const SAMPLE_HEADERS: &str =
    include_str!("../../../../test-vectors/card-settled-v1/basal.basal.headers.json");

struct Sink {
    open: AtomicBool,
    cursor: std::sync::atomic::AtomicI64,
    idle: mpsc::Sender<()>,
}

impl DecisionSink for Sink {
    fn decision_paths(&self) -> Result<Vec<DecisionPath>, SinkError> {
        Ok(vec![DecisionPath::Consent("cingulate".into())])
    }
    fn has_open_cards_on(&self, _: &DecisionPath) -> Result<bool, SinkError> {
        let open = self.open.load(Ordering::Acquire);
        if !open {
            let _ = self.idle.send(());
        }
        Ok(open)
    }
    fn answer_cursor(&self, _: &DecisionPath) -> Result<i64, SinkError> {
        Ok(self.cursor.load(Ordering::Acquire))
    }
    fn save_answer_cursor(&self, _: &DecisionPath, cursor: i64) -> Result<(), SinkError> {
        self.cursor.store(cursor, Ordering::Release);
        Ok(())
    }
    fn decide(&self, _: &DecisionEvent) -> Result<(), SinkError> {
        panic!("install decisions do not use this feed")
    }
    fn answer_on(&self, _: &DecisionPath, answer: &DecisionAnswer) -> Result<(), SinkError> {
        assert_eq!(answer.elicitation_id, "el_3");
        self.open.store(false, Ordering::Release);
        Ok(())
    }
}

struct Wire {
    reads: Mutex<Vec<Value>>,
    initial: mpsc::Sender<()>,
    released: Mutex<bool>,
    gate: Condvar,
}

impl Wire {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.gate.notify_all();
    }
}

impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("the pointer must not select a provider")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
    fn management(&self, module: &str, op: &str, args: Value) -> Result<Value, WireError> {
        assert_eq!(module, "cingulate");
        match op {
            "consent.answers" => {
                let count = {
                    let mut reads = self.reads.lock().unwrap();
                    reads.push(args);
                    reads.len()
                };
                if count == 1 {
                    self.initial.send(()).unwrap();
                    let guard = self.released.lock().unwrap();
                    let (_released, timeout) = self
                        .gate
                        .wait_timeout_while(guard, Duration::from_secs(60), |released| !*released)
                        .unwrap();
                    assert!(
                        !timeout.timed_out(),
                        "initial answer read was never released"
                    );
                    Ok(json!({"cursor":7,"records":[]}))
                } else {
                    Ok(json!({"cursor":8,"records":[{
                        "elicitation_id":"el_3", "state":"answered",
                        "answered_choice_id":"reenable",
                        "flow_decision":{"decision":"reenable","flow_id":"flow","version":1}
                    }]}))
                }
            }
            "consent.ack" => Ok(json!({"ok":true})),
            _ => panic!("unexpected management call {op}"),
        }
    }
}

struct CountingConsent {
    inner: CingulateConsent,
    wakes: AtomicUsize,
}

impl Consent for CountingConsent {
    fn raise(&self, _: &InstallCard) -> Result<InstallOutcome, ConsentError> {
        unreachable!()
    }
    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        self.inner.attach(sink);
    }
    fn card_settled(&self) {
        self.wakes.fetch_add(1, Ordering::AcqRel);
        self.inner.card_settled();
    }
}

async fn publish(server: &Server, body: &[u8], id: Option<&str>) {
    let source: std::collections::BTreeMap<String, String> =
        serde_json::from_str(SAMPLE_HEADERS).unwrap();
    let mut headers = async_nats::HeaderMap::new();
    for (key, value) in source {
        headers.insert(key.as_str(), value.as_str());
    }
    if let Some(id) = id {
        headers.insert("Nats-Msg-Id", id);
    }
    async_nats::jetstream::new(server.bus.client())
        .publish_with_headers(SAMPLE_SUBJECT, headers, body.to_vec().into())
        .await
        .unwrap()
        .await
        .unwrap();
}

struct ConsentFixture {
    consent: Arc<CountingConsent>,
    wire: Arc<Wire>,
    sink: Arc<Sink>,
    started: mpsc::Receiver<()>,
    parked: mpsc::Receiver<()>,
}

fn consent_fixture() -> ConsentFixture {
    let (initial, started) = mpsc::channel();
    let (idle, parked) = mpsc::channel();
    let wire = Arc::new(Wire {
        reads: Mutex::new(Vec::new()),
        initial,
        released: Mutex::new(false),
        gate: Condvar::new(),
    });
    let consent = Arc::new(CountingConsent {
        inner: CingulateConsent::new(wire.clone()).with_polling(),
        wakes: AtomicUsize::new(0),
    });
    let sink = Arc::new(Sink {
        open: AtomicBool::new(true),
        cursor: std::sync::atomic::AtomicI64::new(7),
        idle,
    });
    consent.attach(sink.clone());
    ConsentFixture {
        consent,
        wire,
        sink,
        started,
        parked,
    }
}

#[tokio::test]
async fn card_settled_pinned_sample_coalesces_into_exactly_one_reread() {
    let server = Server::with_subject(Duration::from_secs(30), "ck.box_example.event.>").await;
    let rt = runtime(&server.dir.join("cards.db"));
    let ConsentFixture {
        consent,
        wire,
        sink,
        started,
        parked,
    } = consent_fixture();
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    publish(&server, SAMPLE_BODY, None).await;
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload.as_ref(), SAMPLE_BODY);
    assert_eq!(messages[0].subject.as_str(), SAMPLE_SUBJECT);
    assert_eq!(
        test_control_payload(&messages[0], rt.catalog()).unwrap(),
        json!({"elicitation_id":"el_3","feed":"answers","seq":3})
    );
    // Redelivery and several settlements in a batch need only one wake.
    let batch = vec![messages[0].clone(); 3];
    let writes = rt.store().write_transactions_for_test();
    let (_, malformed) = commit_batch_with_consent(rt.clone(), consent.clone(), &batch, true)
        .await
        .unwrap();
    wire.release();
    assert_eq!(rt.store().write_transactions_for_test(), writes);
    assert_eq!(malformed, 0);
    assert_eq!(consent.wakes.load(Ordering::Acquire), 1);
    parked.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(
        *wire.reads.lock().unwrap(),
        [json!({"since":7}), json!({"since":7})]
    );
    assert_eq!(sink.cursor.load(Ordering::Acquire), 8);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
}

#[tokio::test]
async fn card_settled_malformed_bodies_are_acked_counted_and_do_not_reread() {
    let subject = SAMPLE_SUBJECT;
    let warnings = super::reader::warning_count_for(subject);
    let server = Server::with_subject(Duration::from_secs(30), "ck.box_example.event.>").await;
    let rt = runtime(&server.dir.join("malformed-cards.db"));
    let ConsentFixture {
        consent,
        wire,
        sink,
        started,
        parked,
    } = consent_fixture();
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    let bodies = [
        json!({"elicitation_id":"el_3","feed":"outcomes","seq":3}),
        json!({"elicitation_id":"el_3","feed":"answers"}),
        json!({"elicitation_id":"el_3","feed":"answers","seq":0}),
        json!({"elicitation_id":"el_3","feed":"answers","seq":-1}),
        json!({"elicitation_id":"el_3","feed":"answers","seq":1.5}),
        json!({"elicitation_id":3,"feed":"answers","seq":3}),
        json!({"elicitation_id":"el_3","feed":"answers","seq":3,"answer":"reenable"}),
    ];
    for (n, body) in bodies.iter().enumerate() {
        publish(
            &server,
            body.to_string().as_bytes(),
            Some(&format!("bad:{n}")),
        )
        .await;
    }
    let mut consumer = bind(&server.bus, STREAM).await.unwrap();
    let messages = pull(&consumer).await.unwrap();
    assert_eq!(messages.len(), bodies.len());
    let (_, malformed) = commit_batch_with_consent(rt, consent.clone(), &messages, true)
        .await
        .unwrap();
    sink.open.store(false, Ordering::Release);
    wire.release();
    parked.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(malformed, bodies.len());
    assert_eq!(consent.wakes.load(Ordering::Acquire), 0);
    assert_eq!(wire.reads.lock().unwrap().len(), 1);
    assert_eq!(consumer.info().await.unwrap().num_ack_pending, 0);
    assert_eq!(
        super::reader::warning_count_for(subject) - warnings,
        bodies.len()
    );
}

#[tokio::test]
async fn card_settled_control_digest_and_flow_body_boundaries_stay_strict() {
    let server = Server::with_subject(Duration::from_secs(30), "ck.box_example.event.>").await;
    let rt = runtime(&server.dir.join("digest.db"));
    let js = async_nats::jetstream::new(server.bus.client());
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", "wrong-digest");
    headers.insert(
        "Ck-Content-Digest",
        ContentDigest::of_bytes(b"different").to_string().as_str(),
    );
    js.publish_with_headers(
        "ck.box_example.event.aft.late_result_ready.v1.basal",
        headers,
        br#"{"call_key":"call:3","outcome":"ok"}"#.to_vec().into(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", "empty-claim-check");
    js.publish_with_headers(
        "ck.box_example.event.aft.late_result_ready.v1.basal",
        headers,
        Default::default(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", "inline-flow-trigger");
    headers.insert(
        "Ck-Content-Digest",
        ContentDigest::of_bytes(SAMPLE_BODY).to_string().as_str(),
    );
    js.publish_with_headers(
        "ck.box_example.event.plexus.pull_request_review.v1",
        headers,
        SAMPLE_BODY.to_vec().into(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    let messages = pull(&bind(&server.bus, STREAM).await.unwrap())
        .await
        .unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        test_control_payload(&messages[0], rt.catalog()).unwrap_err(),
        "control notice digest mismatch"
    );
    assert_eq!(
        test_control_payload(&messages[1], rt.catalog()).unwrap_err(),
        "notice Ck-Content-Digest absent"
    );
    let consent = Arc::new(CountingConsent {
        inner: CingulateConsent::new(Arc::new(basal_module::unconfigured::UnconfiguredTransport)),
        wakes: AtomicUsize::new(0),
    });
    for message in &messages[..2] {
        let (_, malformed) = commit_batch_with_consent(
            rt.clone(),
            consent.clone(),
            std::slice::from_ref(message),
            false,
        )
        .await
        .unwrap();
        assert_eq!(malformed, 1);
    }
    assert_eq!(consent.wakes.load(Ordering::Acquire), 0);
    assert_eq!(
        decode(&messages[2], rt.catalog()).unwrap_err(),
        "event notice carries a body"
    );
}
