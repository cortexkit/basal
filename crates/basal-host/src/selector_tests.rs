use super::*;
use std::collections::VecDeque;

struct Attempt {
    module: String,
    op: String,
    params: Value,
}

struct RoutingTransport {
    catalogs: Mutex<VecDeque<Value>>,
    catalog_reads: Mutex<usize>,
    replies: Mutex<VecDeque<Result<Value, WireError>>>,
    attempts: Mutex<Vec<Attempt>>,
    input_tokens: Mutex<bool>,
    describe_reads: Mutex<usize>,
}

impl RoutingTransport {
    fn new(catalogs: &[&[&str]], replies: Vec<Result<Value, WireError>>) -> Arc<Self> {
        Arc::new(Self {
            catalogs: Mutex::new(catalogs.iter().map(|ids| {
                json!({"modules": ids.iter().map(|id| json!({"module_id": id, "capabilities": {"provides": ["routing/v1"]}})).collect::<Vec<_>>()})
            }).collect()),
            catalog_reads: Mutex::new(0),
            replies: Mutex::new(replies.into()),
            attempts: Mutex::new(Vec::new()),
            input_tokens: Mutex::new(false),
            describe_reads: Mutex::new(0),
        })
    }

    fn modules(&self) -> Vec<String> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.module.clone())
            .collect()
    }
}

impl Transport for RoutingTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        *self.catalog_reads.lock().unwrap() += 1;
        Ok(self
            .catalogs
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected catalog refresh"))
    }

    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        if op == "role.describe" {
            *self.describe_reads.lock().unwrap() += 1;
            return Ok(fake_describe(*self.input_tokens.lock().unwrap()));
        }
        self.attempts.lock().unwrap().push(Attempt {
            module: module.into(),
            op: op.into(),
            params,
        });
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected routing attempt")
    }

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
}

fn request() -> SelectionRequest {
    SelectionRequest {
        input_tokens: 100,
        max_output_tokens: 32,
        caller_class: "flow".into(),
        iq: 70,
        eq: 20,
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        send_id: "send-1".into(),
    }
}

fn answer() -> Value {
    fake_answer("decision-1", ("registry", "model"), ("runner", "model"))
}

#[test]
fn routing_concurrency_full_is_a_typed_wait_with_an_absolute_reset() {
    for (hint, delay) in [
        (json!(2600), 2500),
        (Value::Null, 0),
        (json!("2600"), 0),
        (json!(-1), 0),
    ] {
        let transport = RoutingTransport::new(
            &[&["model-picker"]],
            vec![Err(WireError::RefusedDetails {
                code: "no_route".into(),
                message: "all slots occupied".into(),
                detail: json!({"reason":"concurrency_full","reset_at_ms":hint}),
            })],
        );
        let Err(SelectionError::Wait(wait)) = RoutingSelector::new(transport).select(&request())
        else {
            panic!("concurrency must wait")
        };
        assert!(wait.model_selection_wait() && wait.model_send_wait());
        assert_eq!(wait.retry_delay_ms(100), delay);
        assert_eq!(wait.retry_delay_ms(3000), 0);
        assert!(!wait.terminal() && !wait.readiness());
        let persisted = serde_json::to_value(&wait).unwrap();
        assert_eq!(persisted["reason"], "resource_busy");
        assert_eq!(
            serde_json::from_value::<crate::flow_refusal::FlowRefusal>(persisted).unwrap(),
            *wait
        );
    }
    let refused = error(WireError::RefusedDetails {
        code: "no_route".into(),
        message: "concurrency_full".into(),
        detail: json!({"reason":"all_inadequate","reset_at_ms":2600}),
    });
    assert!(matches!(refused, SelectionError::Refused {code,..} if code=="no_route"));
}

#[test]
fn routing_v1_request_names_consumer_runner_and_preserves_call_identity() {
    assert_eq!(
        select_params(&request()),
        json!({
            "consumer":"basal.flow", "requirements":{"iq":70,"eq":20,"tools":"none","max_output_tokens":32},
            "runners":["broca"], "correlation_id":"send-1", "task_ref":"flow:flow-a:run-a"
        })
    );
    let mut request = request();
    request.caller_class = "codemode".into();
    assert_eq!(select_params(&request)["consumer"], "basal.codemode");
    assert_eq!(select_params(&request)["correlation_id"], "send-1");
    assert_eq!(select_params(&request)["task_ref"], "codemode:run-a");
    let typed: SelectRequest = serde_json::from_value(select_params(&request)).unwrap();
    assert_eq!(typed.requirements.iq, 70);
    assert_eq!(typed.requirements.eq, Some(20));
}

#[test]
fn input_estimate_is_sent_only_when_live_routing_advertises_it() {
    for advertised in [false, true] {
        let transport =
            RoutingTransport::new(&[&["model-picker"]], vec![Ok(answer()), Ok(answer())]);
        *transport.input_tokens.lock().unwrap() = advertised;
        let selector = RoutingSelector::new(transport.clone());
        for _ in 0..2 {
            selector.select(&request()).unwrap();
        }
        let attempts = transport.attempts.lock().unwrap();
        for attempt in attempts.iter() {
            assert_eq!(
                attempt.params["requirements"].get("input_tokens").cloned(),
                advertised.then(|| json!(100))
            );
            assert_eq!(attempt.params["requirements"]["max_output_tokens"], 32);
        }
        assert_eq!(*transport.describe_reads.lock().unwrap(), 1);
    }
}

#[test]
fn routing_v1_answers_decode_published_prices_and_runner_identity() {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../test-vectors/routing-v1/consumers-and-placement.json"
    ))
    .unwrap();
    for answer in vectors["answers"].as_array().unwrap() {
        let mut answer = answer.clone();
        answer["runner"]["module"] = json!("broca");
        let selected = decode_selection(answer.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(selected.price.unwrap()).unwrap(),
            answer["price"]
        );
        assert_eq!(selected.harness.as_deref(), Some("alpha"));
        assert_eq!(selected.runner.provider, "example-provider");
        assert_eq!(selected.runner.model, "model-a");
    }
}

#[test]
fn routing_v1_answer_without_price_is_price_unknown() {
    let mut answer = answer();
    answer.as_object_mut().unwrap().remove("price");
    assert!(
        matches!(decode_selection(answer), Err(SelectionError::Refused {code,..}) if code == "price_unknown")
    );
}

#[test]
fn routing_v1_accepts_decisions_with_and_without_runner_harness() {
    let mut answer = answer();
    answer["runner"]["harness"] = json!("broca");
    assert_eq!(
        decode_selection(answer.clone()).unwrap().harness.as_deref(),
        Some("broca")
    );
    answer["runner"].as_object_mut().unwrap().remove("harness");
    assert_eq!(decode_selection(answer).unwrap().harness, None);
}

fn refused(code: &str) -> Result<Value, WireError> {
    Err(WireError::Refused {
        code: code.into(),
        message: "opaque refusal".into(),
    })
}

#[test]
fn routing_catalog_one_provider_calls_it_once() {
    let transport = RoutingTransport::new(&[&["model-picker"]], vec![Ok(answer())]);
    transport.catalogs.lock().unwrap()[0]["modules"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"module_id":"unrelated-provider","capabilities":{"provides":["other/v1"]}}),
            json!({"module_id":"unqualified-module"}),
        ]);
    let selected = RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(selected.decision_id, "decision-1");
    assert_eq!(transport.modules(), ["model-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_provider_id_is_not_fixed() {
    let transport = RoutingTransport::new(&[&["another-picker"]], vec![Ok(answer())]);
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["another-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_no_provider_refuses_after_one_refresh() {
    let transport = RoutingTransport::new(&[&[], &[]], vec![]);
    for catalog in transport.catalogs.lock().unwrap().iter_mut() {
        catalog["modules"] = json!([{"module_id":"unqualified-module"}]);
    }
    assert!(matches!(
        RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "routing_unavailable"
    ));
    assert!(transport.modules().is_empty());
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_two_providers_refuse_without_dispatch_or_refresh() {
    let transport = RoutingTransport::new(&[&["model-picker", "another-picker"]], vec![]);
    assert!(matches!(
        RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "routing_ambiguous"
    ));
    assert!(transport.modules().is_empty());
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_missing_provider_refreshes_and_finds_provider() {
    let transport = RoutingTransport::new(&[&[], &["model-picker"]], vec![Ok(answer())]);
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["model-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_cached_provider_is_shared_by_selection_and_outcomes() {
    let transport = RoutingTransport::new(
        &[&["model-picker"]],
        vec![Ok(answer()), Ok(json!({"ok":true}))],
    );
    let selector = RoutingSelector::new(transport.clone());
    selector.select(&request()).unwrap();
    selector
        .report_outcome("decision-1", ModelOutcome::Completed)
        .unwrap();
    assert_eq!(transport.modules(), ["model-picker", "model-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_refreshed_provider_is_cached() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["renamed-picker"]],
        vec![refused("unknown_module"), Ok(answer()), Ok(answer())],
    );
    let selector = RoutingSelector::new(transport.clone());
    selector.select(&request()).unwrap();
    selector.select(&request()).unwrap();
    assert_eq!(
        transport.modules(),
        ["model-picker", "renamed-picker", "renamed-picker"]
    );
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_route_miss_refresh_can_refuse_missing_or_ambiguous_provider() {
    for (providers, expected) in [
        (&[][..], "routing_unavailable"),
        (
            &["renamed-picker", "another-picker"][..],
            "routing_ambiguous",
        ),
    ] {
        let transport = RoutingTransport::new(
            &[&["model-picker"], providers],
            vec![refused("unknown_module")],
        );
        assert!(matches!(
            RoutingSelector::new(transport.clone()).select(&request()),
            Err(SelectionError::Refused { code, .. }) if code == expected
        ));
        assert_eq!(transport.modules(), ["model-picker"]);
        assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
    }
}

#[test]
fn routing_catalog_rename_refreshes_on_unknown_module_and_retries_new_provider() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["renamed-picker"]],
        vec![refused("unknown_module"), Ok(answer())],
    );
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["model-picker", "renamed-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
    let attempts = transport.attempts.lock().unwrap();
    assert!(
        attempts
            .iter()
            .all(|a| a.op == "route.select" && a.params == select_params(&request()))
    );
}

#[test]
fn routing_catalog_rename_refreshes_on_target_unavailable_and_retries_new_provider() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["renamed-picker"]],
        vec![
            Err(WireError::RefusedDetails {
                code: "target_unavailable".into(),
                message: "opaque refusal".into(),
                detail: json!({"retry":true}),
            }),
            Ok(answer()),
        ],
    );
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["model-picker", "renamed-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_other_refusals_return_without_refresh_or_message_matching() {
    for code in ["principal_denied", "module_warming", "no_available_model"] {
        let transport = RoutingTransport::new(
            &[&["model-picker"]],
            vec![Err(WireError::Refused {
                code: code.into(),
                message: "unknown_module target_unavailable".into(),
            })],
        );
        assert!(
            matches!(RoutingSelector::new(transport.clone()).select(&request()),
            Err(SelectionError::Refused { code: actual, .. }) if actual == code)
        );
        assert_eq!(transport.modules(), ["model-picker"]);
        assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
    }
}

#[test]
fn routing_catalog_second_miss_returns_without_another_refresh() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["renamed-picker"]],
        vec![refused("unknown_module"), refused("target_unavailable")],
    );
    assert!(
        matches!(RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "target_unavailable")
    );
    assert_eq!(transport.modules(), ["model-picker", "renamed-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_unchanged_id_returns_the_original_miss_without_retry() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["model-picker"]],
        vec![refused("unknown_module")],
    );
    assert!(
        matches!(RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "unknown_module")
    );
    assert_eq!(transport.modules(), ["model-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_outcome_reporting_uses_the_same_resolution_and_refresh() {
    let transport = RoutingTransport::new(
        &[&["model-picker"], &["renamed-picker"]],
        vec![refused("unknown_module"), Ok(json!({"ok":true}))],
    );
    RoutingSelector::new(transport.clone())
        .report_outcome("decision-1", ModelOutcome::Completed)
        .unwrap();
    assert_eq!(transport.modules(), ["model-picker", "renamed-picker"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
    let attempts = transport.attempts.lock().unwrap();
    assert!(attempts.iter().all(|a| a.op == "route.set_decision_outcome"
        && a.params == json!({"decisionID":"decision-1","outcome":"completed"})));
}
