//! Target scopes are shared across runs, but each run pins its first usable ref.

use crate::{flow_scope::FlowScope, transport::WireError};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use subc_protocol::Principal;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetCall {
    pub agent: String,
    pub version: u32,
    pub expires_at_ms: i64,
    pub args: serde_json::Value,
    #[serde(default)]
    pub replay: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetScope {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
    pub delegates: bool,
    pub expires_at_ms: i64,
}

impl TargetScope {
    pub fn selector(&self) -> FlowScope {
        FlowScope {
            owner: Principal::Reserved {
                module_id: crate::subc_catalog::CORE.into(),
            },
            scope_ref: self.scope_ref.clone(),
            epoch: self.epoch,
        }
    }
    pub fn route_key(&self) -> String {
        format!("target:{}", self.scope_ref)
    }
}

#[derive(Default)]
pub struct TargetScopes {
    scopes: HashMap<(String, String, u32), Vec<TargetScope>>,
    pins: HashMap<(String, String), TargetScope>,
    invalid: HashMap<(String, u64), i64>,
    retired: Vec<String>,
    refresh_required: HashSet<(String, String, u32)>,
}

impl TargetScopes {
    pub fn take_retired(&mut self) -> Vec<String> {
        std::mem::take(&mut self.retired)
    }
    pub fn resolve(
        &mut self,
        flow: &str,
        run: &str,
        call: &TargetCall,
        now_ms: i64,
        ended: impl Fn(&TargetScope) -> bool,
        ask: impl FnOnce() -> Result<serde_json::Value, WireError>,
    ) -> Result<TargetScope, WireError> {
        self.invalid.retain(|_, expiry| *expiry > now_ms);
        self.scopes.retain(|_, scopes| {
            self.retired.extend(
                scopes
                    .iter()
                    .filter(|s| s.expires_at_ms <= now_ms)
                    .map(TargetScope::route_key),
            );
            scopes.retain(|s| s.expires_at_ms > now_ms);
            !scopes.is_empty()
        });
        self.pins.retain(|_, s| s.expires_at_ms > now_ms);
        self.refresh_required
            .retain(|key| self.scopes.contains_key(key));
        for scope in self.scopes.values().flatten().chain(self.pins.values()) {
            if ended(scope) {
                self.invalid
                    .insert((scope.scope_ref.clone(), scope.epoch), scope.expires_at_ms);
            }
        }
        let usable = |s: &&TargetScope| {
            s.expires_at_ms >= call.expires_at_ms
                && !self.invalid.contains_key(&(s.scope_ref.clone(), s.epoch))
        };
        let pin_key = (run.into(), call.agent.clone());
        let cache_key = (flow.into(), call.agent.clone(), call.version);
        if !call.replay {
            if let Some(scope) = self.pins.get(&pin_key).filter(usable) {
                return Ok(scope.clone());
            }
            if let Some(scope) = self
                .scopes
                .get(&cache_key)
                .filter(|_| !self.refresh_required.contains(&cache_key))
                .and_then(|scopes| scopes.iter().filter(usable).max_by_key(|s| s.expires_at_ms))
            {
                let scope = scope.clone();
                self.pins.insert(pin_key, scope.clone());
                return Ok(scope);
            }
        }
        // Asking core again re-checks the target against the current audience.
        // Drop this run's earlier scope first: if core refuses and the script
        // catches the refusal, a later call must not quietly reuse the old scope.
        self.pins.remove(&pin_key);
        if self.scopes.contains_key(&cache_key) {
            self.refresh_required.insert(cache_key.clone());
        }
        let scope: TargetScope = serde_json::from_value(ask().map_err(target_error)?)
            .map_err(|e| WireError::NeverSent(format!("invalid target scope reply: {e}")))?;
        if scope.scope_ref.trim().is_empty()
            || scope.expires_at_ms < call.expires_at_ms
            || scope.expires_at_ms <= now_ms
        {
            return Err(WireError::NeverSent(
                "target scope does not cover the run deadline".into(),
            ));
        }
        if self
            .invalid
            .contains_key(&(scope.scope_ref.clone(), scope.epoch))
        {
            return Err(WireError::Refused {
                code: "scope_ended".into(),
                message: "core returned an ended target scope".into(),
            });
        }
        // An epoch change invalidates only that ref. When core issues a new ref
        // for a later deadline, the older refs stay valid until they expire, so
        // runs already using one keep it.
        for old in self.scopes.values().flatten() {
            if old.scope_ref == scope.scope_ref && old.epoch != scope.epoch {
                self.invalid
                    .insert((old.scope_ref.clone(), old.epoch), old.expires_at_ms);
            }
        }
        self.refresh_required.remove(&cache_key);
        let scopes = self.scopes.entry(cache_key).or_default();
        scopes.retain(|s| s.scope_ref != scope.scope_ref);
        scopes.push(scope.clone());
        self.pins.insert(pin_key, scope.clone());
        Ok(scope)
    }
}

fn target_error(error: WireError) -> WireError {
    use crate::flow_refusal::{FlowRefusal, RefusalReason};
    let (code, detail) = match &error {
        WireError::Refused { code, .. } => (code, None),
        WireError::RefusedDetails { code, detail, .. } => (code, Some(detail.clone())),
        _ => return error,
    };
    let reason = match code.as_str() {
        "sink_target_not_in_audience" => RefusalReason::SinkTargetNotInAudience,
        "flow_not_approved" => RefusalReason::FlowNotApproved,
        "sink_audience_unavailable" => RefusalReason::SinkAudienceUnavailable,
        "flow_has_no_audience" => RefusalReason::FlowHasNoAudience,
        "scope_expiry_too_far" => RefusalReason::ScopeExpiryTooFar,
        "flow_target_scope_principal_refused" => RefusalReason::FlowTargetScopePrincipalRefused,
        _ => return error,
    };
    let mut refusal = FlowRefusal::new(reason, crate::subc_catalog::CORE, "flow.target_scope");
    refusal.detail = detail.filter(|value| value.to_string().len() <= 4096);
    WireError::Typed(refusal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expired_refs_are_removed_from_the_cache() {
        let mut cache = TargetScopes::default();
        let mut call = TargetCall {
            agent: "a".into(),
            version: 1,
            expires_at_ms: 100,
            args: json!({}),
            replay: false,
        };
        cache
            .resolve(
                "f",
                "run-a",
                &call,
                0,
                |_| false,
                || Ok(json!({"ref":"old","epoch":1,"delegates":false,"expires_at_ms":200})),
            )
            .unwrap();
        call.expires_at_ms = 300;
        cache
            .resolve(
                "f",
                "run-b",
                &call,
                201,
                |_| false,
                || Ok(json!({"ref":"new","epoch":1,"delegates":false,"expires_at_ms":400})),
            )
            .unwrap();
        assert_eq!(
            cache.scopes[&("f".into(), "a".into(), 1)]
                .iter()
                .map(|s| s.scope_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["new"]
        );
        assert!(!cache.pins.contains_key(&("run-a".into(), "a".into())));
        assert_eq!(cache.take_retired(), vec!["target:old"]);
    }
}
