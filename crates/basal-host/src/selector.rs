//! The fleet's routing module chooses each model before basal journals a
//! request that can be sent. A replay uses that saved choice, not a new one.
use crate::broca::wire::ModelParams;
use crate::transport::{Transport, WireError};
use cortexkit_role_routing::{
    price::Price,
    select::{Requirements, SelectAnswer, SelectRequest, ToolsRequirement},
};
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

#[cfg(any(test, feature = "test-support"))]
pub fn fake_price() -> Price {
    serde_json::from_value(json!({"basis":"metered","rates_per_mtok":{"input":1,"output":1,"cache_read":0,"cache_write":1}})).unwrap()
}

#[cfg(any(test, feature = "test-support"))]
pub fn fake_answer(decision: &str, route: (&str, &str), runner: (&str, &str)) -> Value {
    use cortexkit_role_routing::select::{RouteRef, RunnerRef};
    serde_json::to_value(
        SelectAnswer::new(
            decision,
            RouteRef::new(route.0, route.1),
            RunnerRef::new("broca", RouteRef::new(runner.0, runner.1)).with_harness("basal"),
            200_000,
            32_000,
            1_800_000_000_000,
            "The fake route fits.",
        )
        .with_price(fake_price()),
    )
    .unwrap()
}

#[cfg(any(test, feature = "test-support"))]
pub fn fake_describe(input_tokens: bool) -> Value {
    let mut describe = cortexkit_role_routing::describe::RoleDescribe::new(vec![], false);
    if input_tokens {
        describe.requirement_fields.push("input_tokens".into());
    }
    serde_json::to_value(describe).unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRequest {
    pub input_tokens: u64,
    pub max_output_tokens: u64,
    /// Internal call class choosing basal's declared consumer, never an agent ID.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
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
    Wait(Box<crate::flow_refusal::FlowRefusal>),
    Refused { code: String, detail: String },
    Unavailable { detail: String },
}
impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wait(refusal) => {
                write!(f, "model selection waiting: {}", refusal.reason.as_str())
            }
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
        WireError::RefusedDetails {
            code,
            message,
            detail,
        } => {
            use cortexkit_role_routing::errors::{NoRouteReason, Refusal, RefusalCode};
            let refusal = Refusal::with(RefusalCode::parse(&code), &message, &detail);
            if refusal.code == RefusalCode::NoRoute
                && serde_json::from_value::<NoRouteReason>(detail["reason"].clone()).ok()
                    == Some(NoRouteReason::ConcurrencyFull)
            {
                use crate::flow_refusal::{FlowRefusal, RefusalReason};
                let mut wait =
                    FlowRefusal::new(RefusalReason::ResourceBusy, "routing", "route.select");
                let mut detail = detail;
                // Use the contract's timestamp classifier; missing or malformed
                // hints still wait on basal's normal bounded retry schedule.
                detail["reset_at_ms"] = json!(refusal.wait_until_ms());
                wait.detail = Some(detail);
                wait.message = Some(message.into());
                return SelectionError::Wait(Box::new(wait));
            }
            SelectionError::Refused {
                code,
                detail: message,
            }
        }
        WireError::Refused { code, message } => SelectionError::Refused {
            code,
            detail: message,
        },
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
pub struct RoutingSelector {
    transport: Arc<dyn Transport>,
    module: Mutex<Option<String>>,
    input_tokens: Mutex<Option<(String, bool)>>,
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
            input_tokens: Mutex::new(None),
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

    fn params_for(&self, module: &str, request: &SelectionRequest) -> Value {
        let mut cached = self.input_tokens.lock().unwrap();
        if cached.as_ref().is_none_or(|(id, _)| id != module) {
            let advertised = self
                .transport
                .management_with_route_refusals(module, "role.describe", json!({}))
                .ok()
                .and_then(|value| {
                    serde_json::from_value::<cortexkit_role_routing::describe::RoleDescribe>(value)
                        .ok()
                })
                .is_some_and(|description| {
                    description.role == "routing"
                        && description.version == 1
                        && description
                            .requirement_fields
                            .iter()
                            .any(|field| field == "input_tokens")
                });
            *cached = Some((module.to_owned(), advertised));
        }
        let mut params = select_params(request);
        if cached.as_ref().is_some_and(|(_, advertised)| *advertised) {
            params["requirements"]["input_tokens"] = request.input_tokens.into();
        }
        params
    }

    fn management(&self, op: &str, params: impl Fn(&str) -> Value) -> Result<Value, WireError> {
        let (module, result) = match self.routing_module(false) {
            Ok(module) => {
                let result =
                    self.transport
                        .management_with_route_refusals(&module, op, params(&module));
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
                return self.transport.management_with_route_refusals(
                    &refreshed,
                    op,
                    params(&refreshed),
                );
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
    let consumer = if request.caller_class == "codemode" {
        CODEMODE_CONSUMER
    } else {
        FLOW_CONSUMER
    };
    let requirements = Requirements {
        iq: u8::try_from(request.iq).unwrap_or(u8::MAX),
        eq: Some(u8::try_from(request.eq).unwrap_or(u8::MAX)),
        max_output_tokens: Some(request.max_output_tokens),
        tools: Some(ToolsRequirement::None),
        ..Requirements::default()
    };
    serde_json::to_value(
        SelectRequest::new(consumer, requirements)
            .with_runners(vec!["broca".into()])
            .with_correlation_id(&request.send_id)
            .with_task_ref(task),
    )
    .expect("routing request serializes")
}
/// Checks structural answer and price validity only, not runner admission.
/// `RoutingSelector::select` admits the runner before returning a choice.
pub fn decode_selection(reply: Value) -> Result<ModelSelection, SelectionError> {
    if reply.get("runner").is_none_or(Value::is_null) {
        return Err(SelectionError::Refused {
            code: "route_runner_missing".into(),
            detail: "routing did not supply a Broca runner".into(),
        });
    }
    let answer: SelectAnswer =
        serde_json::from_value(reply).map_err(|e| invalid(&e.to_string()))?;
    if answer.route.provider.is_empty()
        || answer.route.model.is_empty()
        || answer.decision_id.is_empty()
        || answer.runner.model.provider.is_empty()
        || answer.runner.model.model.is_empty()
    {
        return Err(invalid("empty selection identity"));
    }
    let price = answer.price.ok_or_else(crate::model_cost::price_unknown)?;
    price.check().map_err(invalid)?;
    Ok(ModelSelection {
        provider_id: answer.route.provider,
        model_id: answer.route.model,
        variant: answer.route.variant,
        decision_id: answer.decision_id,
        runner: ModelParams {
            provider: answer.runner.model.provider,
            model: answer.runner.model.model,
            variant: answer.runner.model.variant,
        },
        harness: answer.runner.harness,
        price: Some(price),
    })
}
impl ModelSelector for RoutingSelector {
    fn select(&self, request: &SelectionRequest) -> Result<ModelSelection, SelectionError> {
        let reply = self
            .management("route.select", |module| self.params_for(module, request))
            .map_err(error)?;
        let selection = decode_selection(reply.clone())?;
        if reply["runner"]["module"] != "broca" {
            return Err(SelectionError::Refused {
                code: "route_runner_missing".into(),
                detail: "routing did not supply a Broca runner".into(),
            });
        }
        Ok(selection)
    }
    fn report_outcome(
        &self,
        decision_id: &str,
        outcome: ModelOutcome,
    ) -> Result<(), SelectionError> {
        let reply = self
            .management(
                "route.set_decision_outcome",
                |_| json!({"decisionID":decision_id,"outcome":outcome}),
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
            harness: Some("basal".into()),
            price: Some(fake_price()),
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
