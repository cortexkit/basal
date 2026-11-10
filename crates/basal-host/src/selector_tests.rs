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
}

impl RoutingTransport {
    fn new(catalogs: &[&[&str]], replies: Vec<Result<Value, WireError>>) -> Arc<Self> {
        Arc::new(Self {
            catalogs: Mutex::new(catalogs.iter().map(|ids| {
                json!({"modules": ids.iter().map(|id| json!({"module_id": id})).collect::<Vec<_>>()})
            }).collect()),
            catalog_reads: Mutex::new(0),
            replies: Mutex::new(replies.into()),
            attempts: Mutex::new(Vec::new()),
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
        caller_class: "flow".into(),
        iq: 70,
        eq: 20,
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        send_id: "send-1".into(),
    }
}

fn answer() -> Value {
    json!({"selected":{"model":{"providerID":"registry","modelID":"model"}},"decisionID":"decision-1","runner":{"provider":"runner","model":"model"}})
}

fn refused(code: &str) -> Result<Value, WireError> {
    Err(WireError::Refused {
        code: code.into(),
        message: "opaque refusal".into(),
    })
}

#[test]
fn routing_catalog_legacy_registered_calls_legacy_once() {
    let transport = RoutingTransport::new(&[&["prefrontal-routing"]], vec![Ok(answer())]);
    let selected = RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(selected.decision_id, "decision-1");
    assert_eq!(transport.modules(), ["prefrontal-routing"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_only_striatum_registered_calls_striatum() {
    let transport = RoutingTransport::new(&[&["striatum"]], vec![Ok(answer())]);
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["striatum"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
}

#[test]
fn routing_catalog_prefers_legacy_when_both_or_neither_are_registered() {
    for ids in [&["prefrontal-routing", "striatum"][..], &[][..]] {
        let transport = RoutingTransport::new(&[ids], vec![Ok(answer())]);
        RoutingSelector::new(transport.clone())
            .select(&request())
            .unwrap();
        assert_eq!(transport.modules(), ["prefrontal-routing"]);
    }
}

#[test]
fn routing_catalog_stale_legacy_refreshes_on_unknown_module_and_retries_striatum() {
    let transport = RoutingTransport::new(
        &[&["prefrontal-routing"], &["striatum"]],
        vec![refused("unknown_module"), Ok(answer())],
    );
    RoutingSelector::new(transport.clone())
        .select(&request())
        .unwrap();
    assert_eq!(transport.modules(), ["prefrontal-routing", "striatum"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
    let attempts = transport.attempts.lock().unwrap();
    assert!(
        attempts
            .iter()
            .all(|a| a.op == "route.select" && a.params == select_params(&request()))
    );
}

#[test]
fn routing_catalog_stale_striatum_refreshes_on_target_unavailable_and_retries_legacy() {
    let transport = RoutingTransport::new(
        &[&["striatum"], &["prefrontal-routing"]],
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
    assert_eq!(transport.modules(), ["striatum", "prefrontal-routing"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_other_refusals_return_without_refresh_or_message_matching() {
    for code in ["principal_denied", "module_warming", "no_available_model"] {
        let transport = RoutingTransport::new(
            &[&["prefrontal-routing"]],
            vec![Err(WireError::Refused {
                code: code.into(),
                message: "unknown_module target_unavailable".into(),
            })],
        );
        assert!(
            matches!(RoutingSelector::new(transport.clone()).select(&request()),
            Err(SelectionError::Refused { code: actual, .. }) if actual == code)
        );
        assert_eq!(transport.modules(), ["prefrontal-routing"]);
        assert_eq!(*transport.catalog_reads.lock().unwrap(), 1);
    }
}

#[test]
fn routing_catalog_second_miss_returns_without_another_refresh() {
    let transport = RoutingTransport::new(
        &[&["prefrontal-routing"], &["striatum"]],
        vec![refused("unknown_module"), refused("target_unavailable")],
    );
    assert!(
        matches!(RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "target_unavailable")
    );
    assert_eq!(transport.modules(), ["prefrontal-routing", "striatum"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_unchanged_id_returns_the_original_miss_without_retry() {
    let transport = RoutingTransport::new(
        &[&["prefrontal-routing"], &["prefrontal-routing"]],
        vec![refused("unknown_module")],
    );
    assert!(
        matches!(RoutingSelector::new(transport.clone()).select(&request()),
        Err(SelectionError::Refused { code, .. }) if code == "unknown_module")
    );
    assert_eq!(transport.modules(), ["prefrontal-routing"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
}

#[test]
fn routing_catalog_outcome_reporting_uses_the_same_resolution_and_refresh() {
    let transport = RoutingTransport::new(
        &[&["prefrontal-routing"], &["striatum"]],
        vec![refused("unknown_module"), Ok(json!({"ok":true}))],
    );
    RoutingSelector::new(transport.clone())
        .report_outcome("decision-1", ModelOutcome::Completed)
        .unwrap();
    assert_eq!(transport.modules(), ["prefrontal-routing", "striatum"]);
    assert_eq!(*transport.catalog_reads.lock().unwrap(), 2);
    let attempts = transport.attempts.lock().unwrap();
    assert!(attempts.iter().all(|a| a.op == "route.set_decision_outcome"
        && a.params == json!({"decisionID":"decision-1","outcome":"completed"})));
}
