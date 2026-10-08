use basal_host::core_consent::CoreConsent;
use basal_host::routing::ModuleOpsHost;
use basal_host::selector::SelectionError;
use basal_host::selector::decode_selection;
use basal_host::subc_catalog::{CORE, SubcCatalog};
use basal_host::transport::{Transport, WireError};
use basal_host::{
    CallRequest, Consent, DecisionAnswer, DecisionEvent, DecisionSink, Host, SinkError,
};
use basal_proto::{CallKind, JsonText};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Wire {
    catalogs: Mutex<VecDeque<Result<Value, WireError>>>,
    pages: Mutex<VecDeque<Value>>,
    agents: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<String>>,
}
impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        self.calls.lock().unwrap().push("catalog".into());
        self.catalogs
            .lock()
            .unwrap()
            .pop_front()
            .expect("catalog response")
    }
    fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
        self.calls.lock().unwrap().push(op.into());
        Ok(match op {
            "elicitation.answers" => self.pages.lock().unwrap().pop_front().unwrap(),
            "agent.list" => self.agents.lock().unwrap().pop_front().unwrap(),
            "elicitation.ack" => json!({"ok":true}),
            "echo" | "board.read" => json!({"done":true}),
            _ => panic!("unexpected operation {op}"),
        })
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unexpected tool")
    }
}
fn catalog(module: &str, op: &str) -> Value {
    json!({"modules":[{"module_id":module,"capabilities":{"provides":["flow-scopes/v1"]},
        "roles":[{"role":"management_surface","operations":[{"name":op,"kind":"query"}],
        "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}]})
}
fn request(module: &str, op: &str) -> CallRequest {
    CallRequest {
        flow_id: "flow".into(),
        run_id: "run".into(),
        position: 0,
        kind: CallKind::Op {
            module: module.into(),
            op: op.into(),
        },
        args: JsonText::new("{}").unwrap(),
        idempotency_key: "key".into(),
        attempt: 1,
    }
}
#[test]
fn transient_classification_failure_does_not_finalize_a_query() {
    let wire = Arc::new(Wire::default());
    wire.catalogs.lock().unwrap().extend([
        Err(WireError::Unknown("offline".into())),
        Ok(catalog("mock", "echo")),
    ]);
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire)));
    let request = request("mock", "echo");
    let class = host.classify(&request.kind);
    assert!(
        matches!(host.dispatch_classified(&request, class), Ok(basal_host::Dispatched::Completed(outcome)) if outcome.settlement == basal_proto::Settlement::Fulfilled)
    );
}
#[test]
fn catalog_unavailability_does_not_become_a_final_core_scope_refusal() {
    let wire = Arc::new(Wire::default());
    wire.catalogs.lock().unwrap().extend([
        Err(WireError::Unknown("offline".into())),
        Ok(catalog(CORE, "board.read")),
    ]);
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire)));
    let result = host.dispatch(&request(CORE, "board.read"));
    assert!(
        matches!(
            result,
            Err(basal_host::TransportError::Unavailable {
                sent: basal_host::Sent::Never,
                ..
            })
        ),
        "{result:?}"
    );
}
#[test]
fn temporarily_absent_module_is_retryable() {
    let wire = Arc::new(Wire::default());
    wire.catalogs
        .lock()
        .unwrap()
        .push_back(Ok(json!({"modules":[]})));
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire)));
    let result = host.dispatch(&request("mock", "echo"));
    assert!(
        matches!(
            result,
            Err(basal_host::TransportError::Unavailable {
                sent: basal_host::Sent::Never,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn core_dispatch_reads_scope_capability_and_op_from_one_snapshot() {
    let wire = Arc::new(Wire::default());
    wire.catalogs
        .lock()
        .unwrap()
        .extend((0..3).map(|_| Ok(catalog(CORE, "board.read"))));
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire.clone())));
    let request = request(CORE, "board.read");
    let class = host.classify(&request.kind);
    host.dispatch_classified(&request, class).unwrap();
    assert_eq!(
        wire.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|op| *op == "catalog")
            .count(),
        2,
        "one classification snapshot plus one fresh dispatch snapshot"
    );
}
#[derive(Default)]
struct Sink(Mutex<Vec<DecisionEvent>>);
impl DecisionSink for Sink {
    fn decide(&self, event: &DecisionEvent) -> Result<(), SinkError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
    fn answer(&self, _: &DecisionAnswer) -> Result<(), SinkError> {
        Ok(())
    }
}
#[test]
fn bad_answer_does_not_block_later_valid_decisions_or_acknowledge_the_bad_one() {
    let wire = Arc::new(Wire::default());
    wire.pages.lock().unwrap().push_back(json!({"cursor":2,"records":[
        {"state":"unknown","flow_install":{}},
        {"state":"answered","answered_choice_id":"approve","flow_install":{"flow_id":"flow","version":1,"code_hash":"a".repeat(64)}}
    ]}));
    let sink = Arc::new(Sink::default());
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink.clone());
    assert!(consent.poll_once().is_err());
    assert_eq!(sink.0.lock().unwrap().len(), 1);
    assert!(
        !wire
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|op| op == "elicitation.ack")
    );
}
#[test]
fn idle_consent_page_does_not_send_an_ack() {
    let wire = Arc::new(Wire::default());
    wire.pages
        .lock()
        .unwrap()
        .push_back(json!({"cursor":0,"records":[]}));
    let consent = CoreConsent::new(wire.clone());
    consent.attach(Arc::new(Sink::default()));
    consent.poll_once().unwrap();
    assert_eq!(*wire.calls.lock().unwrap(), ["elicitation.answers"]);
}
#[test]
fn nullable_model_variant_is_the_absent_variant() {
    let result = decode_selection(
        json!({"selected":{"model":{"providerID":"p","modelID":"m","variant":null}},"decisionID":"d","runner":{"provider":"p","model":"m"}}),
    );
    assert_eq!(result.unwrap().variant, None);
}

#[test]
fn selection_errors_have_explicit_script_visible_messages() {
    assert_eq!(
        SelectionError::Refused {
            code: "denied".into(),
            detail: "not allowed".into()
        }
        .to_string(),
        "model selection refused denied: not allowed"
    );
    assert_eq!(
        SelectionError::Unavailable {
            detail: "offline".into()
        }
        .to_string(),
        "model selection unavailable: offline"
    );
}
#[test]
fn stable_agent_id_wins_over_a_display_name_even_on_a_later_page() {
    let wire = Arc::new(Wire::default());
    wire.agents.lock().unwrap().extend([
        json!({"agents":[{"agent_id":"other","name":"agent-1"}],"next_cursor":"next"}),
        json!({"agents":[{"agent_id":"agent-1","name":"A"}]}),
    ]);
    assert_eq!(
        SubcCatalog::new(wire).resolve_agent("agent-1").unwrap(),
        Some("agent-1".into())
    );
}
#[test]
#[should_panic(expected = "an unsent fault cannot apply an effect")]
fn mock_refuses_contradictory_unsent_fault() {
    basal_host::mock::MockHost::new().inject(
        "mock",
        "send",
        &[basal_host::mock::Fault {
            proven_unsent: true,
            effect_applied: true,
        }],
    );
}

#[test]
fn production_random_is_not_the_same_first_draw_after_restart() {
    if std::env::var_os("BASAL_ENTROPY_CHILD").is_some() {
        let host = basal_host::core_host::CoreHost::new(Arc::new(Wire::default()));
        let sample = host.random();
        assert!((0.0..1.0).contains(&sample));
        println!("ENTROPY_BITS={}", sample.to_bits());
        return;
    }
    let draw = || {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "production_random_is_not_the_same_first_draw_after_restart",
                "--nocapture",
            ])
            .env("BASAL_ENTROPY_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // With --nocapture, libtest may print its test-name prefix on the same
        // line as the draw. Locate the marker rather than requiring a new line.
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .lines()
            .find_map(|line| {
                line.split_once("ENTROPY_BITS=")
                    .and_then(|(_, bits)| bits.parse::<u64>().ok())
            })
            .unwrap_or_else(|| {
                panic!("child draw missing from successful child output: {stdout:?}")
            })
    };
    assert_ne!(
        draw(),
        draw(),
        "fresh processes repeated their first production random sample"
    );
}
