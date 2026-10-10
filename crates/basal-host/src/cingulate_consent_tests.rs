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
