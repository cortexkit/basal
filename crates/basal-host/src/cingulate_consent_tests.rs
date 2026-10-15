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
    let card = grant_card();
    for path in [
        DecisionPath::Legacy,
        DecisionPath::Consent("cingulate".into()),
    ] {
        let before = consent.core.raised_epoch();
        assert!(consent.raise_decision_on(&path, &card).is_err());
        assert_ne!(consent.core.raised_epoch(), before);
    }
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
        panic!("a card cannot move to legacy")
    }
}

#[test]
fn a_legacy_only_adapter_cannot_move_an_owned_consent_card() {
    let consent = CoreConsent::new(Arc::new(NoSend));
    let path = DecisionPath::Consent("cingulate".into());
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
            "consent.request" | "elicitation.request" => Ok(json!({"elicitation_id":"raised-id"})),
            "elicitation.answers" => Ok(json!({
                "cursor": 1,
                "records": [{
                    "elicitation_id":"legacy-id",
                    "state":"answered",
                    "answered_choice_id":"reenable",
                    "flow_decision":{"decision":"reenable","flow_id":"flow","version":1}
                }]
            })),
            "elicitation.ack" => Ok(json!({"ok":true})),
            "elicitation.withdraw" => Ok(json!({"state":"withdrawn"})),
            other => panic!("unexpected management call {provider}:{other}"),
        }
    }
}

#[test]
fn missing_consent_provider_is_unavailable_without_calling_core() {
    let transport = RecordingTransport::new(json!({"modules":[]}));
    let consent = CingulateConsent::new(Arc::new(transport.clone()));

    let error = consent.raise_decision(&grant_card()).unwrap_err();

    assert!(matches!(error, ConsentError::Unavailable(message) if message.contains("consent/v1")));
    assert!(transport.calls().is_empty());
}

#[test]
fn advertised_consent_provider_receives_the_request() {
    let transport = RecordingTransport::new(json!({
        "modules":[{"module_id":"cingulate","capabilities":{"provides":["consent/v1"]}}]
    }));
    let consent = CingulateConsent::new(Arc::new(transport.clone()));

    assert_eq!(consent.raise_decision(&grant_card()).unwrap(), "raised-id");
    assert_eq!(
        transport.calls(),
        [(
            "cingulate".into(),
            "consent.request".into(),
            decision_request(&grant_card()).unwrap()
        )]
    );
}

#[derive(Default)]
struct AppliedAnswers(std::sync::Mutex<Vec<(DecisionPath, String, Option<String>)>>);

impl DecisionSink for AppliedAnswers {
    fn decide(&self, _: &crate::DecisionEvent) -> Result<(), crate::SinkError> {
        panic!("decision answer feed does not deliver install decisions")
    }

    fn answer(&self, _: &crate::DecisionAnswer) -> Result<(), crate::SinkError> {
        panic!("the adapter must preserve the stored answer path")
    }

    fn answer_on(
        &self,
        path: &DecisionPath,
        answer: &crate::DecisionAnswer,
    ) -> Result<(), crate::SinkError> {
        self.0.lock().unwrap().push((
            path.clone(),
            answer.elicitation_id.clone(),
            answer.choice.clone(),
        ));
        Ok(())
    }
}

#[test]
fn existing_legacy_cards_keep_answers_and_withdrawals_on_core() {
    let transport = RecordingTransport::new(json!({"modules":[]}));
    let consent = CingulateConsent::new(Arc::new(transport.clone()));
    let sink = Arc::new(AppliedAnswers::default());
    consent.attach(sink.clone());

    consent.poll_once().unwrap();
    consent
        .withdraw_decision_on(&DecisionPath::Legacy, "legacy-id")
        .unwrap();

    assert_eq!(
        *sink.0.lock().unwrap(),
        [(
            DecisionPath::Legacy,
            "legacy-id".into(),
            Some("reenable".into())
        )]
    );
    assert_eq!(
        transport
            .calls()
            .iter()
            .map(|(provider, method, _)| (provider.as_str(), method.as_str()))
            .collect::<Vec<_>>(),
        [
            (CORE, "elicitation.answers"),
            (CORE, "elicitation.ack"),
            (CORE, "elicitation.withdraw"),
        ]
    );
}
