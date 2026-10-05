//! The fleet's routing module chooses each model before basal journals a
//! request that can be sent. A replay uses that saved choice, not a new one.
use crate::broca::wire::ModelParams;
use crate::transport::{Transport, WireError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRequest {
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
        write!(f, "{self:?}")
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
}
impl fmt::Debug for RoutingSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoutingSelector(prefrontal-routing)")
    }
}
impl RoutingSelector {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}
pub fn select_params(request: &SelectionRequest) -> Value {
    json!({"targetAgent":"flow","requirements":{"iq":request.iq,"eq":request.eq},"excludeRouteKeys":[],"sendID":request.send_id,"taskId":format!("flow:{}:{}",request.flow_id,request.run_id),"substrate":"broca"})
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
            self.transport
                .management("prefrontal-routing", "route.select", select_params(request))
                .map_err(error)?,
        )
    }
    fn report_outcome(
        &self,
        decision_id: &str,
        outcome: ModelOutcome,
    ) -> Result<(), SelectionError> {
        let reply = self
            .transport
            .management(
                "prefrontal-routing",
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
pub struct FakeSelector {
    pub requests: Mutex<Vec<SelectionRequest>>,
    pub reports: Mutex<Vec<(String, ModelOutcome)>>,
    pub replies: Mutex<std::collections::VecDeque<Result<ModelSelection, SelectionError>>>,
    pub report_error: Mutex<Option<SelectionError>>,
}
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
