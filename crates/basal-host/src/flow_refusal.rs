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
    ScopeUnsupported,
    TargetFlowUnsupported,
    ProviderNotCatalogued,
    CatalogUnavailable,
    ResourceBusy,
    Transient,
    AccountSelectionRefused,
    AccountLimitExhausted,
    ConsentUnavailable,
    ModuleGrantAbsent,
    AgentGrantAbsent,
    NoFlowScope,
    AgentRetired,
    FlowScopeRequired,
    FlowDelegationUnsupported,
    SinkTargetNotInAudience,
    FlowNotApproved,
    SinkAudienceUnavailable,
    SinkStoreFailed,
    SecretVaultUnavailable,
    SinkBoardTargetUnavailable,
    FlowHasNoAudience,
    ScopeExpiryTooFar,
    FlowTargetScopePrincipalRefused,
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
            Self::ScopeUnsupported => "scope_unsupported",
            Self::TargetFlowUnsupported => "target_flow_unsupported",
            Self::ProviderNotCatalogued => "provider_not_catalogued",
            Self::CatalogUnavailable => "catalog_unavailable",
            Self::ResourceBusy => "resource_busy",
            Self::Transient => "transient",
            Self::AccountSelectionRefused => "account_selection_refused",
            Self::AccountLimitExhausted => "account_limit_exhausted",
            Self::ConsentUnavailable => "consent_unavailable",
            Self::ModuleGrantAbsent => "module_grant_absent",
            Self::AgentGrantAbsent => "agent_grant_absent",
            Self::NoFlowScope => "no_flow_scope",
            Self::AgentRetired => "agent_retired",
            Self::FlowScopeRequired => "flow_scope_required",
            Self::FlowDelegationUnsupported => "flow_delegation_unsupported",
            Self::SinkTargetNotInAudience => "sink_target_not_in_audience",
            Self::FlowNotApproved => "flow_not_approved",
            Self::SinkAudienceUnavailable => "sink_audience_unavailable",
            Self::SinkStoreFailed => "sink_store_failed",
            Self::SecretVaultUnavailable => "secret_vault_unavailable",
            Self::SinkBoardTargetUnavailable => "sink_board_target_unavailable",
            Self::FlowHasNoAudience => "flow_has_no_audience",
            Self::ScopeExpiryTooFar => "scope_expiry_too_far",
            Self::FlowTargetScopePrincipalRefused => "flow_target_scope_principal_refused",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PersistedRefusal", into = "PersistedRefusal")]
pub struct FlowRefusal {
    pub reason: RefusalReason,
    pub provider: String,
    pub action: String,
    pub retry_after_ms: Option<u64>,
    pub detail: Option<Value>,
    /// The message the provider sent with the refusal, when it sent one.
    /// [`Self::message`] is basal's own wording for the reason, which a flow
    /// run reads; a codemode program, which has no flow, reads this instead.
    /// Boxed so the refusal, carried inside every `WireError`, stays small.
    pub message: Option<Box<str>>,
}

// Older builds decode persisted deferral causes with a closed reason enum.
// ResourceBusy has the same unsent retry/refund behavior as model-send waits. Keep
// that legacy reason on disk and carry the exact provider code in a field
// older builds ignore, so rolling back never makes a run log unreadable.
#[derive(Serialize, Deserialize)]
struct PersistedRefusal {
    reason: RefusalReason,
    provider: String,
    action: String,
    retry_after_ms: Option<u64>,
    detail: Option<Value>,
    message: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_code: Option<String>,
}

impl From<FlowRefusal> for PersistedRefusal {
    fn from(refusal: FlowRefusal) -> Self {
        let model_wait = refusal.model_send_wait();
        let provider_code = model_wait.then(|| refusal.reason.as_str().into());
        Self {
            reason: if model_wait {
                RefusalReason::ResourceBusy
            } else {
                refusal.reason
            },
            provider: refusal.provider,
            action: refusal.action,
            retry_after_ms: refusal.retry_after_ms,
            detail: refusal.detail,
            message: refusal.message,
            provider_code,
        }
    }
}

impl From<PersistedRefusal> for FlowRefusal {
    fn from(refusal: PersistedRefusal) -> Self {
        Self {
            reason: if refusal.reason == RefusalReason::ResourceBusy {
                match refusal.provider_code.as_deref() {
                    Some("transient") => RefusalReason::Transient,
                    Some("account_selection_refused") => RefusalReason::AccountSelectionRefused,
                    Some("account_limit_exhausted") => RefusalReason::AccountLimitExhausted,
                    _ => refusal.reason,
                }
            } else {
                refusal.reason
            },
            provider: refusal.provider,
            action: refusal.action,
            retry_after_ms: refusal.retry_after_ms,
            detail: refusal.detail,
            message: refusal.message,
        }
    }
}

impl FlowRefusal {
    pub fn model_send_wait(&self) -> bool {
        matches!(
            self.reason,
            RefusalReason::Transient
                | RefusalReason::AccountSelectionRefused
                | RefusalReason::AccountLimitExhausted
        )
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self.reason,
            RefusalReason::FlowDelegationUnsupported
                | RefusalReason::SinkTargetNotInAudience
                | RefusalReason::FlowNotApproved
                | RefusalReason::FlowHasNoAudience
                | RefusalReason::ScopeExpiryTooFar
                | RefusalReason::FlowTargetScopePrincipalRefused
        )
    }
    pub fn grant_absent(&self) -> bool {
        matches!(
            self.reason,
            RefusalReason::ModuleGrantAbsent | RefusalReason::AgentGrantAbsent
        )
    }

    pub fn lost_grant(&self) -> Result<(&Value, &str), String> {
        let detail = self.detail.as_ref().ok_or("missing grant detail")?;
        let grant = detail
            .get("grant")
            .filter(|v| !v.is_null())
            .ok_or("missing grant reference")?;
        if grant.as_str().is_some_and(|s| s.is_empty()) {
            return Err("empty grant reference".into());
        }
        let label = detail
            .get("grant_label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("missing grant label")?;
        Ok((grant, label))
    }
    pub fn readiness(&self) -> bool {
        matches!(
            self.reason,
            RefusalReason::NoFlowScope
                | RefusalReason::TargetFlowUnsupported
                | RefusalReason::ProviderNotCatalogued
                | RefusalReason::CatalogUnavailable
        )
    }
    pub fn message(&self) -> &'static str {
        match self.reason {
            RefusalReason::NoFlowScope => "waiting for core to register the flow scope",
            RefusalReason::ProviderNotCatalogued => {
                "the provider is not currently catalogued; waiting for it to register"
            }
            RefusalReason::CatalogUnavailable => "the daemon catalog is temporarily unavailable",
            // The daemon refuses this both for a module that never declared
            // flow-scopes/v1 and for one that has not finished registering
            // after a restart, so the text must not assert which.
            RefusalReason::TargetFlowUnsupported => {
                "the daemon does not accept flow calls to this module yet: it is still registering, or it does not declare flow-scopes/v1; retry later"
            }
            RefusalReason::ResourceBusy => "the provider's exclusive resource is busy",
            RefusalReason::Transient => "the model provider is temporarily unavailable",
            RefusalReason::AccountSelectionRefused | RefusalReason::AccountLimitExhausted => {
                "waiting for an available model account"
            }
            RefusalReason::ConsentUnavailable => {
                "the provider requires a standing grant for this action"
            }
            RefusalReason::ModuleGrantAbsent | RefusalReason::AgentGrantAbsent => {
                "a standing grant for this flow was lost"
            }
            RefusalReason::AgentRetired => "the target agent retired",
            RefusalReason::FlowScopeRequired => {
                "basal sent a provider call without its required flow scope"
            }
            RefusalReason::SinkAudienceUnavailable => {
                "core cannot currently read the install audience"
            }
            RefusalReason::SinkStoreFailed => "core could not store the board write",
            RefusalReason::SecretVaultUnavailable => "the secret vault is temporarily unavailable",
            RefusalReason::SinkBoardTargetUnavailable => "core's agent registry is unavailable",
            RefusalReason::FlowHasNoAudience => "the approved flow has no audience",
            RefusalReason::ScopeExpiryTooFar => {
                "the requested scope lifetime exceeds the approved run bound"
            }
            RefusalReason::FlowTargetScopePrincipalRefused => {
                "basal used the wrong carrier principal for flow.target_scope"
            }
            _ => "the flow scope is not currently admitted",
        }
    }
    pub fn outcome(&self) -> crate::HostOutcome {
        let mut outcome=crate::HostOutcome::rejected(basal_proto::JsonText::new(serde_json::json!({"code":self.reason.as_str(),"module":self.provider,"action":self.action,"message":self.message(),"detail":self.detail}).to_string()).expect("bounded refusal payload"));
        // A typed unsent refusal proves no inference spent any tokens.
        outcome.usage = Some(crate::TokenUsage {
            input_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(0),
            cached_input_tokens: Some(0),
            ..Default::default()
        });
        outcome
    }
    pub fn new(reason: RefusalReason, provider: &str, action: &str) -> Self {
        Self {
            reason,
            provider: provider.into(),
            action: action.into(),
            retry_after_ms: None,
            detail: None,
            message: None,
        }
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
            "scope_unsupported" => RefusalReason::ScopeUnsupported,
            "target_flow_unsupported" => RefusalReason::TargetFlowUnsupported,
            "resource_busy" => RefusalReason::ResourceBusy,
            "consent_unavailable" => RefusalReason::ConsentUnavailable,
            "module_grant_absent" => RefusalReason::ModuleGrantAbsent,
            "agent_grant_absent" => RefusalReason::AgentGrantAbsent,
            "agent_retired" => RefusalReason::AgentRetired,
            "flow_scope_required" => RefusalReason::FlowScopeRequired,
            "flow_delegation_unsupported" => RefusalReason::FlowDelegationUnsupported,
            _ => return Ok(None),
        };
        let mut refusal = Self::new(reason, "", "");
        if reason == RefusalReason::ResourceBusy {
            let busy: ResourceBusy =
                serde_json::from_value(body.detail.clone().ok_or("missing busy detail")?)
                    .map_err(|e| e.to_string())?;
            busy.validate().map_err(|e| e.to_string())?;
            refusal.retry_after_ms = Some(busy.retry_after_ms);
        }
        refusal.message = Some(body.message.as_str().into());
        // Grant references must reach the store intact: it hashes oversized
        // references instead of truncating authority-bearing bytes.
        refusal.detail = body
            .detail
            .clone()
            .filter(|d| refusal.grant_absent() || d.to_string().len() <= 4096);
        if refusal.grant_absent() {
            refusal.lost_grant()?;
        }
        Ok(Some(refusal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_resource_busy::Holder;
    use serde_json::json;

    #[test]
    fn model_wait_cause_is_readable_by_the_legacy_refusal_shape() {
        // The legacy refusal types below reproduce the decoder from before
        // model-send waits: its reason enum is closed, but the containing
        // struct ignores unknown fields.
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        #[serde(rename_all = "snake_case")]
        enum LegacyReason {
            ScopeNotCarrier,
            ScopeEnded,
            ScopeNotLive,
            ScopeEpochRequired,
            ScopeNotSynced,
            ScopeChanged,
            ScopeUnsupported,
            TargetFlowUnsupported,
            ProviderNotCatalogued,
            CatalogUnavailable,
            ResourceBusy,
            ConsentUnavailable,
            ModuleGrantAbsent,
            AgentGrantAbsent,
            NoFlowScope,
            AgentRetired,
            FlowScopeRequired,
            FlowDelegationUnsupported,
            SinkTargetNotInAudience,
            FlowNotApproved,
            SinkAudienceUnavailable,
            SinkStoreFailed,
            SecretVaultUnavailable,
            SinkBoardTargetUnavailable,
            FlowHasNoAudience,
            ScopeExpiryTooFar,
            FlowTargetScopePrincipalRefused,
        }
        #[derive(Debug, Deserialize, Serialize)]
        struct LegacyRefusal {
            reason: LegacyReason,
            provider: String,
            action: String,
            retry_after_ms: Option<u64>,
            detail: Option<Value>,
            message: Option<Box<str>>,
        }
        for reason in [
            RefusalReason::Transient,
            RefusalReason::AccountSelectionRefused,
            RefusalReason::AccountLimitExhausted,
        ] {
            let mut refusal = FlowRefusal::new(reason, "broca", "session.send");
            refusal.message = Some("credential listing unavailable".into());
            refusal.retry_after_ms = Some(2500);
            let cause = serde_json::to_string(&refusal).unwrap();
            let legacy: LegacyRefusal = serde_json::from_str(&cause).unwrap();
            assert_eq!(legacy.reason, LegacyReason::ResourceBusy);
            assert_eq!(legacy.provider, "broca");
            assert_eq!(legacy.action, "session.send");
            assert_eq!(legacy.message, refusal.message);
            assert_eq!(
                serde_json::from_str::<FlowRefusal>(&cause).unwrap(),
                refusal
            );
            assert!(!refusal.terminal() && !refusal.readiness() && !refusal.grant_absent());
            let value: Value = serde_json::from_str(refusal.outcome().value.as_str()).unwrap();
            assert_eq!(value["code"], reason.as_str());
            let old_cause = serde_json::to_string(&legacy).unwrap();
            assert_eq!(
                serde_json::from_str::<FlowRefusal>(&old_cause)
                    .unwrap()
                    .reason,
                RefusalReason::ResourceBusy
            );
        }
    }

    #[test]
    fn resource_busy_uses_shared_type_and_validates() {
        let busy = ResourceBusy::new("browser_profile", Holder::flow("flow-a"), 10, 250);
        let body = ErrorBody::new("resource_busy", "irrelevant")
            .with_detail(serde_json::to_value(&busy).unwrap());
        let decoded = FlowRefusal::decode(&body).unwrap().unwrap();
        assert_eq!(decoded.reason, RefusalReason::ResourceBusy);
        assert_eq!(decoded.retry_after_ms, Some(250));
        assert_eq!(decoded.detail, body.detail);
    }

    #[test]
    fn grant_absence_is_typed_and_preserves_oversized_references() {
        for code in ["module_grant_absent", "agent_grant_absent"] {
            let body = ErrorBody::new(code, "not authority")
                .with_detail(json!({"grant":"x".repeat(4097),"grant_label":"restore this grant"}));
            let refusal = FlowRefusal::decode(&body).unwrap().unwrap();
            assert!(refusal.grant_absent());
            assert_eq!(refusal.lost_grant().unwrap().0, &json!("x".repeat(4097)));
            for detail in [
                json!({}),
                json!({"grant":"g","grant_label":""}),
                json!({"grant":null,"grant_label":"label"}),
            ] {
                assert!(
                    FlowRefusal::decode(&ErrorBody::new(code, "ignored").with_detail(detail))
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn malformed_busy_cannot_prove_unsent() {
        for detail in [
            None,
            Some(json!({})),
            Some(
                json!({"resource":"Bad", "holder":{"kind":"head"}, "since_ms":0, "retry_after_ms":1}),
            ),
        ] {
            let mut body = ErrorBody::new("resource_busy", "valid looking prose");
            body.detail = detail;
            assert!(FlowRefusal::decode(&body).is_err());
        }
    }

    #[test]
    fn consent_and_retirement_ignore_detail_and_message() {
        for (code, reason) in [
            ("consent_unavailable", RefusalReason::ConsentUnavailable),
            ("agent_retired", RefusalReason::AgentRetired),
        ] {
            for detail in [None, Some(json!("not a structure"))] {
                let mut body = ErrorBody::new(code, "resource_busy");
                body.detail = detail.clone();
                let decoded = FlowRefusal::decode(&body).unwrap().unwrap();
                assert_eq!(decoded.reason, reason);
                assert_eq!(decoded.detail, detail);
            }
        }
        assert_eq!(
            FlowRefusal::decode(&ErrorBody::new("other", "agent_retired")).unwrap(),
            None
        );
        let large =
            ErrorBody::new("consent_unavailable", "ignored").with_detail(json!("x".repeat(4097)));
        assert_eq!(FlowRefusal::decode(&large).unwrap().unwrap().detail, None);
    }
}
