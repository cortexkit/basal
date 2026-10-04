//! Refusals that make no remote effect, decoded from codes, not prose.
use cortexkit_resource_busy::ResourceBusy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::ErrorBody;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    ScopeNotCarrier,
    ScopeEnded,
    ScopeNotLive,
    ScopeEpochRequired,
    ScopeNotSynced,
    ScopeChanged,
    TargetFlowUnsupported,
    ResourceBusy,
    ConsentUnavailable,
    NoFlowScope,
    AgentRetired,
}

impl RefusalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScopeNotCarrier => "scope_not_carrier",
            Self::ScopeEnded => "scope_ended",
            Self::ScopeNotLive => "scope_not_live",
            Self::ScopeEpochRequired => "scope_epoch_required",
            Self::ScopeNotSynced => "scope_not_synced",
            Self::ScopeChanged => "scope_changed",
            Self::TargetFlowUnsupported => "target_flow_unsupported",
            Self::ResourceBusy => "resource_busy",
            Self::ConsentUnavailable => "consent_unavailable",
            Self::NoFlowScope => "no_flow_scope",
            Self::AgentRetired => "agent_retired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowRefusal {
    pub reason: RefusalReason,
    pub provider: String,
    pub action: String,
    pub retry_after_ms: Option<u64>,
    pub detail: Option<Value>,
}

impl FlowRefusal {
    pub fn new(reason: RefusalReason, provider: &str, action: &str) -> Self {
        Self { reason, provider: provider.into(), action: action.into(), retry_after_ms: None, detail: None }
    }

    /// `None` means an ordinary refusal. A malformed known refusal is an
    /// error: its code alone cannot prove a resource lease was never taken.
    pub fn decode(body: &ErrorBody) -> Result<Option<Self>, String> {
        let reason = match body.code.as_str() {
            "scope_not_carrier" => RefusalReason::ScopeNotCarrier,
            "scope_ended" => RefusalReason::ScopeEnded,
            "scope_not_live" => RefusalReason::ScopeNotLive,
            "scope_epoch_required" => RefusalReason::ScopeEpochRequired,
            "scope_not_synced" => RefusalReason::ScopeNotSynced,
            "scope_changed" => RefusalReason::ScopeChanged,
            "target_flow_unsupported" => RefusalReason::TargetFlowUnsupported,
            "resource_busy" => RefusalReason::ResourceBusy,
            "consent_unavailable" => RefusalReason::ConsentUnavailable,
            "agent_retired" => RefusalReason::AgentRetired,
            _ => return Ok(None),
        };
        let mut refusal = Self::new(reason, "", "");
        if reason == RefusalReason::ResourceBusy {
            let busy: ResourceBusy = serde_json::from_value(body.detail.clone().ok_or("missing busy detail")?)
                .map_err(|e| e.to_string())?;
            busy.validate().map_err(|e| e.to_string())?;
            refusal.retry_after_ms = Some(busy.retry_after_ms);
        }
        // Provider details are display-only except the validated busy hint.
        refusal.detail = body.detail.clone().filter(|d| d.to_string().len() <= 4096);
        Ok(Some(refusal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_resource_busy::Holder;
    use serde_json::json;

    #[test]
    fn resource_busy_uses_shared_type_and_validates() {
        let busy = ResourceBusy::new("browser_profile", Holder::flow("flow-a"), 10, 250);
        let body = ErrorBody::new("resource_busy", "irrelevant").with_detail(serde_json::to_value(&busy).unwrap());
        let decoded = FlowRefusal::decode(&body).unwrap().unwrap();
        assert_eq!(decoded.reason, RefusalReason::ResourceBusy);
        assert_eq!(decoded.retry_after_ms, Some(250));
        assert_eq!(decoded.detail, body.detail);
    }

    #[test]
    fn malformed_busy_cannot_prove_unsent() {
        for detail in [None, Some(json!({})), Some(json!({"resource":"Bad", "holder":{"kind":"head"}, "since_ms":0, "retry_after_ms":1}))] {
            let mut body = ErrorBody::new("resource_busy", "valid looking prose");
            body.detail = detail;
            assert!(FlowRefusal::decode(&body).is_err());
        }
    }

    #[test]
    fn consent_and_retirement_ignore_detail_and_message() {
        for (code, reason) in [("consent_unavailable", RefusalReason::ConsentUnavailable), ("agent_retired", RefusalReason::AgentRetired)] {
            for detail in [None, Some(json!("not a structure"))] {
                let mut body = ErrorBody::new(code, "resource_busy");
                body.detail = detail.clone();
                let decoded = FlowRefusal::decode(&body).unwrap().unwrap();
                assert_eq!(decoded.reason, reason);
                assert_eq!(decoded.detail, detail);
            }
        }
        assert_eq!(FlowRefusal::decode(&ErrorBody::new("other", "agent_retired")).unwrap(), None);
    }
}
