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
