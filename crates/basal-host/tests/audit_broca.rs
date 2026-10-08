use basal_host::broca::{
    BrocaError, BrocaHost, StateStore, StoredCall,
    fake::{FakeBroca, MemoryStore},
    wire::GenerationConfig,
};
use basal_host::selector::{FakeSelector, ModelSelector, SelectionRequest};
use basal_host::{CallRequest, Host, Sent, TransportError};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::json;
use std::sync::Arc;

fn request() -> CallRequest {
    let selection = FakeSelector::default()
        .select(&SelectionRequest {
            iq: 1,
            eq: 1,
            flow_id: "f".into(),
            run_id: "r".into(),
            send_id: "k".into(),
        })
        .unwrap();
    CallRequest { flow_id:"f".into(), run_id:"r".into(), position:0, kind:CallKind::Primitive(Primitive::Llm), args:JsonText::new(json!({"send_id":"k", "work_class":"flow:f", "session":"basal:flow-f:r:0", "op":"llm", "max_output":12, "request":{"prompt":"hello"}, "selection":selection}).to_string()).unwrap(), idempotency_key:"k".into(), attempt:1 }
}

struct BrokenStore;
impl StateStore for BrokenStore {
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        Ok(vec![])
    }
    fn save(&self, _: &StoredCall) -> Result<(), BrocaError> {
        Err(BrocaError::Store("disk full".into()))
    }
}

#[test]
fn first_snapshot_failure_is_provably_unsent() {
    let transport = Arc::new(FakeBroca::default());
    let host = BrocaHost::new(
        transport.clone(),
        Arc::new(BrokenStore),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    );
    assert!(matches!(
        host.dispatch(&request()),
        Err(TransportError::Unavailable {
            sent: Sent::Never,
            ..
        })
    ));
    assert!(transport.sends().is_empty());
}

#[test]
fn generation_typos_are_refused() {
    assert!(serde_json::from_value::<GenerationConfig>(json!({"temprature":0.2})).is_err());
}

#[test]
fn saved_params_are_compact_and_read_legacy_byte_arrays() {
    let store = Arc::new(MemoryStore::default());
    BrocaHost::new(
        Arc::new(FakeBroca::default()),
        store.clone(),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    )
    .dispatch_model(&request())
    .unwrap();
    let call = store.load().unwrap().pop().unwrap();
    let mut value = serde_json::to_value(&call).unwrap();
    assert!(value["params"].is_string());
    assert!(!value["envelope"].as_str().unwrap().contains("hello"));
    value["params"] = json!(call.params);
    value["envelope"] = json!(request().args.as_str());
    let legacy: StoredCall = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.params, call.params);
    assert_eq!(legacy.envelope, call.envelope);
}

#[test]
fn broca_errors_have_stable_human_readable_display() {
    assert_eq!(
        BrocaError::Store("disk full".into()).to_string(),
        "Broca store: disk full"
    );
}

#[test]
fn an_unsent_refusal_does_not_penalize_the_selected_model() {
    let store = Arc::new(MemoryStore::default());
    let selector = Arc::new(FakeSelector::default());
    let host = BrocaHost::new(
        Arc::new(FakeBroca::default()),
        store.clone(),
        "/".into(),
        "basal".into(),
        selector.clone(),
    );
    let req = request();
    host.dispatch_model(&req).unwrap();
    let mut call = store.get("k").unwrap().unwrap();
    call.handle = None;
    call.broca_run_id = None;
    call.deferred = true;
    store.save(&call).unwrap();
    host.refusal_committed(
        &req,
        &basal_host::flow_refusal::FlowRefusal::new(
            basal_host::flow_refusal::RefusalReason::AgentRetired,
            "broca",
            "session.send",
        ),
    );
    assert!(selector.reports.lock().unwrap().is_empty());
    assert!(store.get("k").unwrap().unwrap().acknowledged);
}

#[test]
fn model_dispatch_uses_a_keyed_lookup_not_the_full_history() {
    struct Keyed(MemoryStore);
    impl StateStore for Keyed {
        fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
            panic!("model dispatch decoded full history")
        }
        fn get(&self, key: &str) -> Result<Option<StoredCall>, BrocaError> {
            self.0.get(key)
        }
        fn pending_ids(&self) -> Result<Vec<String>, BrocaError> {
            self.0.pending_ids()
        }
        fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
            self.0.save(call)
        }
    }
    let host = BrocaHost::new(
        Arc::new(FakeBroca::default()),
        Arc::new(Keyed(MemoryStore::default())),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    );
    host.dispatch_model(&request()).unwrap();
    host.dispatch_model(&request()).unwrap();
}

#[test]
fn one_corrupt_snapshot_does_not_block_healthy_completions() {
    use basal_host::broca::StoredOutcome;
    use basal_host::{Completion, CompletionAck, CompletionSink, SinkError, UnknownOutcome};
    use std::sync::Mutex;
    struct PartialStore(MemoryStore);
    impl StateStore for PartialStore {
        fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
            self.0.load()
        }
        fn get(&self, id: &str) -> Result<Option<StoredCall>, BrocaError> {
            if id == "bad" {
                Err(BrocaError::Store("corrupt snapshot".into()))
            } else {
                self.0.get(id)
            }
        }
        fn pending_ids(&self) -> Result<Vec<String>, BrocaError> {
            Ok(vec!["bad".into(), "k".into()])
        }
        fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
            self.0.save(call)
        }
    }
    #[derive(Default)]
    struct Sink(Mutex<Vec<Completion>>);
    impl CompletionSink for Sink {
        fn complete(&self, completion: &Completion) -> Result<CompletionAck, SinkError> {
            self.0.lock().unwrap().push(completion.clone());
            Ok(CompletionAck::Accepted)
        }
        fn unknown(&self, _: &UnknownOutcome) -> Result<CompletionAck, SinkError> {
            panic!("fixture expected a completion, not an unknown outcome")
        }
    }
    let store = Arc::new(PartialStore(MemoryStore::default()));
    let host = BrocaHost::new(
        Arc::new(FakeBroca::default()),
        store.clone(),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    );
    host.dispatch_model(&request()).unwrap();
    let mut call = store.get("k").unwrap().unwrap();
    call.state = Some("completed".into());
    call.outcome = Some(StoredOutcome {
        rejected: false,
        value: json!({"text":"done"}).to_string(),
        usage: None,
    });
    store.save(&call).unwrap();
    let sink = Arc::new(Sink::default());
    host.attach(sink.clone());
    assert_eq!(sink.0.lock().unwrap().len(), 1);
    assert!(store.get("k").unwrap().unwrap().acknowledged);
    assert!(
        host.poll().is_err(),
        "the corrupt row must remain visible to the operator"
    );
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn a_stalled_send_does_not_hold_an_unrelated_dispatch() {
    use basal_host::broca::{Route, Transport, wire::*};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;
    struct Blocking {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl Transport for Blocking {
        fn send(&self, _: &Route, params: &[u8]) -> Result<SendResult, BrocaError> {
            let params: SendParams = serde_json::from_slice(params).unwrap();
            if params.send_id.as_deref() == Some("k") {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .expect("test did not release the stalled send");
            }
            Ok(SendResult::Active {
                run_id: format!("run:{}", params.send_id.unwrap()),
            })
        }
        fn watch(&self, _: &Route) -> Result<(), BrocaError> {
            Ok(())
        }
        fn result(&self, _: &Route, _: &RunResultParams) -> Result<RunResultResponse, BrocaError> {
            unreachable!()
        }
        fn status(&self, _: &Route, _: &StatusParams) -> Result<RunStatusResponse, BrocaError> {
            unreachable!()
        }
    }
    let (entered, observed) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let host = Arc::new(BrocaHost::new(
        Arc::new(Blocking {
            entered,
            release: Mutex::new(waiting),
        }),
        Arc::new(MemoryStore::default()),
        "/".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    ));
    let first_host = host.clone();
    let first = std::thread::spawn(move || first_host.dispatch_model(&request()));
    let did_enter = observed.recv_timeout(Duration::from_secs(30));
    let mut other = request();
    other.idempotency_key = "other".into();
    let mut args: serde_json::Value = serde_json::from_str(other.args.as_str()).unwrap();
    args["send_id"] = json!("other");
    other.args = JsonText::new(args.to_string()).unwrap();
    let (done, completed) = mpsc::channel();
    let second = std::thread::spawn(move || {
        let _ = done.send(host.dispatch_model(&other));
    });
    let before_release = completed.recv_timeout(Duration::from_secs(10));
    // Deadlines only stop hangs. Both threads are released and joined before
    // asserting the required order: unrelated completion before stalled send.
    let _ = release.send(());
    first.join().unwrap().unwrap();
    second.join().unwrap();
    did_enter.expect("first dispatch did not reach send");
    assert!(
        before_release
            .expect("unrelated dispatch waited for the stalled send")
            .is_ok()
    );
}
