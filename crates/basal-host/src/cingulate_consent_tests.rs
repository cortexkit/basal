use super::*;
use crate::transport::WireError;
use crate::{DecisionContext, DecisionOption};
use serde_json::Value;

struct LostReply;
impl Transport for LostReply {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("the path is already recorded")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("no tool call")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        Err(WireError::Unknown("accepted but reply lost".into()))
    }
}

#[test]
fn a_lost_request_reply_still_wakes_the_owned_answer_inventory() {
    let consent = CingulateConsent::new(Arc::new(LostReply));
    let path = DecisionPath::Consent("cingulate".into());
    let before = consent.core.raised_epoch();
    assert!(consent.raise_decision_on(&path, &grant_card()).is_err());
    assert_ne!(consent.core.raised_epoch(), before);
}

fn grant_card() -> DecisionCard {
    DecisionCard {
        dedup_key: Some("grant-loss".into()),
        flow_id: "flow".into(),
        version: 1,
        context: DecisionContext::GrantLost {
            provider: "github".into(),
            grant: "opaque".into(),
            grant_label: "Restore access".into(),
            refused_at_ms: 0,
        },
        title: "Decide".into(),
        prompt: "Check the existing grant".into(),
        args_digest: None,
        options: vec![
            DecisionOption {
                id: "check_now".into(),
                label: "Check now".into(),
                decline: false,
            },
            DecisionOption {
                id: "keep_disabled".into(),
                label: "Keep disabled".into(),
                decline: true,
            },
        ],
        expires_in_ms: 60_000,
    }
}

struct NoSend;
impl Transport for NoSend {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("no catalog read")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("no tool call")
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        panic!("core holds no decision cards")
    }
}

#[test]
fn the_core_install_adapter_never_sends_a_decision_card() {
    let consent = CoreConsent::new(Arc::new(NoSend));
    let path = DecisionPath::Consent("cingulate".into());
    assert!(consent.decision_path().is_err());
    assert!(consent.raise_decision_on(&path, &grant_card()).is_err());
    assert!(consent.withdraw_decision_on(&path, "owned-id").is_err());
}

#[derive(Clone)]
struct RecordingTransport {
    catalog: Value,
    calls: Arc<std::sync::Mutex<Vec<(String, String, Value)>>>,
}

impl RecordingTransport {
    fn new(catalog: Value) -> Self {
        Self {
            catalog,
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Transport for RecordingTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(self.catalog.clone())
    }

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("no tool call")
    }

    fn management(&self, provider: &str, method: &str, params: Value) -> Result<Value, WireError> {
        self.calls
            .lock()
            .unwrap()
            .push((provider.into(), method.into(), params));
        match method {
            "consent.request" => Ok(json!({"elicitation_id":"raised-id"})),
            other => panic!("unexpected management call {provider}:{other}"),
        }
    }
}

#[test]
fn missing_consent_provider_is_unavailable_without_calling_core() {
    let transport = RecordingTransport::new(json!({"modules":[]}));
    let consent = CingulateConsent::new(Arc::new(transport.clone()));

    let error = consent.decision_path().unwrap_err();

    assert!(matches!(error, ConsentError::Unavailable(message) if message.contains("consent/v1")));
    assert!(transport.calls().is_empty());
}

#[test]
fn advertised_consent_provider_receives_the_request() {
    let transport = RecordingTransport::new(json!({
        "modules":[{"module_id":"cingulate","capabilities":{"provides":["consent/v1"]}}]
    }));
    let consent = CingulateConsent::new(Arc::new(transport.clone()));

    let path = consent.decision_path().unwrap();
    assert_eq!(path, DecisionPath::Consent("cingulate".into()));
    assert_eq!(
        consent.raise_decision_on(&path, &grant_card()).unwrap(),
        "raised-id"
    );
    assert_eq!(
        transport.calls(),
        [(
            "cingulate".into(),
            "consent.request".into(),
            decision_request(&grant_card()).unwrap()
        )]
    );
}
