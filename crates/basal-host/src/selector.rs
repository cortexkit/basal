//! The fleet's routing module chooses each model before basal journals a
//! request that can be sent. A replay uses that saved choice, not a new one.
use crate::broca::wire::ModelParams;
use crate::transport::{Transport, WireError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[path = "selector_tests.rs"]
mod tests;

pub const FLOW_CONSUMER: &str = "basal.flow";
pub const CODEMODE_CONSUMER: &str = "basal.codemode";
pub const FLOW_CLASSIFY_CONSUMER: &str = "basal.flow.classify";
pub const CODEMODE_CLASSIFY_CONSUMER: &str = "basal.codemode.classify";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRequest {
    /// A shared routing category such as "flow" or "codemode", not an agent ID.
    pub caller_class: String,
    pub iq: u32,
    pub eq: u32,
    pub flow_id: String,
    pub run_id: String,
    pub send_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(rename = "decisionID")]
    pub decision_id: String,
    pub runner: ModelParams,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOutcome {
    Completed,
    Error,
    Cancelled,
    Interrupted,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    Refused { code: String, detail: String },
    Unavailable { detail: String },
}
impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { code, detail } => write!(f, "model selection refused {code}: {detail}"),
            Self::Unavailable { detail } => write!(f, "model selection unavailable: {detail}"),
        }
    }
}
impl std::error::Error for SelectionError {}
pub trait ModelSelector: fmt::Debug + Send + Sync {
    fn select(&self, request: &SelectionRequest) -> Result<ModelSelection, SelectionError>;
    fn report_outcome(
        &self,
        decision_id: &str,
        outcome: ModelOutcome,
    ) -> Result<(), SelectionError>;
}
#[derive(Debug, Default)]
pub struct UnconfiguredSelector;
impl ModelSelector for UnconfiguredSelector {
    fn select(&self, _: &SelectionRequest) -> Result<ModelSelection, SelectionError> {
        Err(SelectionError::Refused {
            code: "route_not_configured".into(),
            detail: "the fleet model selector is not configured".into(),
        })
    }
    fn report_outcome(&self, _: &str, _: ModelOutcome) -> Result<(), SelectionError> {
        Err(SelectionError::Refused {
            code: "route_not_configured".into(),
            detail: "the fleet model selector is not configured".into(),
        })
    }
}
fn error(e: WireError) -> SelectionError {
    match e {
        WireError::Refused { code, message } | WireError::RefusedDetails { code, message, .. } => {
            SelectionError::Refused {
                code,
                detail: message,
            }
        }
        other => SelectionError::Unavailable {
            detail: format!("{other:?}"),
        },
    }
}
fn invalid(detail: &str) -> SelectionError {
    SelectionError::Refused {
        code: "route_reply_invalid".into(),
        detail: detail.into(),
    }
}
fn string(value: &Value, name: &str) -> Result<String, SelectionError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid(name))
}
pub struct RoutingSelector {
    transport: Arc<dyn Transport>,
    module: Mutex<Option<String>>,
}
impl fmt::Debug for RoutingSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutingSelector(routing/v1)")
    }
}
impl RoutingSelector {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            module: Mutex::new(None),
        }
    }

    fn routing_module(&self, refresh: bool) -> Result<String, WireError> {
        let mut cached = self.module.lock().unwrap();
        if !refresh && let Some(module) = cached.as_ref() {
            return Ok(module.clone());
        }
        *cached = None;
        let catalog = self.transport.catalog()?;
        let modules = catalog["modules"]
            .as_array()
            .ok_or_else(|| WireError::Unreadable("catalog modules missing".into()))?;
        // The daemon reserves routing/v1 at HELLO admission, so its catalog's
        // capability claims can be trusted without a separate nonce check here.
        let mut providers = modules.iter().filter(|module| {
            module["capabilities"]["provides"]
                .as_array()
                .is_some_and(|caps| caps.iter().any(|cap| cap == "routing/v1"))
        });
        let provider = providers.next().ok_or_else(|| WireError::Refused {
            code: "routing_unavailable".into(),
            message: "no module provides routing/v1".into(),
        })?;
        if providers.next().is_some() {
            return Err(WireError::Refused {
                code: "routing_ambiguous".into(),
                message: "more than one module provides routing/v1".into(),
            });
        }
        let module = provider["module_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| WireError::Unreadable("routing provider module_id missing".into()))?
            .to_owned();
        *cached = Some(module.clone());
        Ok(module)
    }

    fn management(&self, op: &str, params: Value) -> Result<Value, WireError> {
        let (module, result) = match self.routing_module(false) {
            Ok(module) => {
                let result =
                    self.transport
                        .management_with_route_refusals(&module, op, params.clone());
                (Some(module), result)
            }
            Err(error) => (None, Err(error)),
        };
        // A provider can appear or be renamed after the catalog read. Refresh
        // once on a missing provider or route, retrying only if the id changed
        // or no provider was found: at most two management calls and two catalog
        // reads, each with its normal timeout.
        if result.as_ref().is_err_and(|error| {
            matches!(
                error.refusal_code(),
                Some("unknown_module" | "target_unavailable" | "routing_unavailable")
            )
        }) {
            let refreshed = self.routing_module(true)?;
            if module.as_deref() != Some(&refreshed) {
                return self
                    .transport
                    .management_with_route_refusals(&refreshed, op, params);
            }
        }
        result
    }
}
pub fn select_params(request: &SelectionRequest) -> Value {
    let task = if request.caller_class == "codemode" {
        format!("codemode:{}", request.run_id)
    } else {
        format!("flow:{}:{}", request.flow_id, request.run_id)
    };
    json!({"targetAgent":request.caller_class,"requirements":{"iq":request.iq,"eq":request.eq},"excludeRouteKeys":[],"sendID":request.send_id,"taskId":task,"substrate":"broca"})
}
pub fn decode_selection(reply: Value) -> Result<ModelSelection, SelectionError> {
    let raw = reply.get("runner").ok_or_else(|| SelectionError::Refused {
        code: "route_runner_missing".into(),
        detail: "routing did not supply a Broca runner".into(),
    })?;
    let runner: ModelParams =
        serde_json::from_value(raw.clone()).map_err(|e| invalid(&e.to_string()))?;
    if runner.provider.is_empty() || runner.model.is_empty() {
        return Err(invalid("empty runner identity"));
    }
    let selected = &reply["selected"]["model"];
    let variant = selected
        .get("variant")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("variant"))
        })
        .transpose()?;
    Ok(ModelSelection {
        provider_id: string(selected, "providerID")?,
        model_id: string(selected, "modelID")?,
        variant,
        decision_id: string(&reply, "decisionID")?,
        runner,
    })
}
impl ModelSelector for RoutingSelector {
    fn select(&self, request: &SelectionRequest) -> Result<ModelSelection, SelectionError> {
        decode_selection(
            self.management("route.select", select_params(request))
                .map_err(error)?,
        )
    }
    fn report_outcome(
        &self,
        decision_id: &str,
        outcome: ModelOutcome,
    ) -> Result<(), SelectionError> {
        let reply = self
            .management(
                "route.set_decision_outcome",
                json!({"decisionID":decision_id,"outcome":outcome}),
            )
            .map_err(error)?;
        if reply["ok"] != true {
            return Err(invalid("outcome acknowledgment"));
        }
        Ok(())
    }
}

/// Tests supply this fake explicitly. The runtime defaults to a refusing
/// selector rather than choosing a model without fleet routing.
#[derive(Debug, Default)]
#[cfg(any(test, feature = "test-support"))]
pub struct FakeSelector {
    pub requests: Mutex<Vec<SelectionRequest>>,
    pub reports: Mutex<Vec<(String, ModelOutcome)>>,
    pub replies: Mutex<std::collections::VecDeque<Result<ModelSelection, SelectionError>>>,
    pub report_error: Mutex<Option<SelectionError>>,
}
#[cfg(any(test, feature = "test-support"))]
impl ModelSelector for FakeSelector {
    fn select(&self, request: &SelectionRequest) -> Result<ModelSelection, SelectionError> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.clone());
        if let Some(reply) = self
            .replies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
        {
            return reply;
        }
        Ok(ModelSelection {
            provider_id: "registry-provider".into(),
            model_id: "registry-model".into(),
            variant: None,
            decision_id: format!("decision:{}", request.send_id),
            runner: ModelParams {
                provider: "fake".into(),
                model: "test".into(),
                variant: None,
            },
        })
    }
    fn report_outcome(
        &self,
        decision_id: &str,
        outcome: ModelOutcome,
    ) -> Result<(), SelectionError> {
        self.reports
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((decision_id.into(), outcome));
        match self
            .report_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
