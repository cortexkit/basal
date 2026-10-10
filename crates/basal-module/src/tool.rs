//! Basal's provider surface. Only the route stamp supplies caller authority;
//! the model supplies a program, a one-line description and lower limits.

use cortexkit_role_tool_provider::catalog::{
    CatalogAnswer, CatalogRequest, CatalogTool, composition_digest, schema_digest,
};
use serde_json::{Value, json};
use subc_protocol::{ErrorBody, Principal, scope::ScopeStamp};

pub const DESCRIPTION: &str = "Run a short JavaScript program that calls your own tools and returns one result.\n\n- Call a tool with `await tools.<name>(input)`: any of your tools except codemode, with the same input you'd pass to it directly. The tool decides allow, ask and deny exactly as for a direct call, and an ask waits for its answer.\n- `tools.model({prompt, system?, max_output?})` asks a model for one completion, and `tools.classify({text, labels})` picks one label; both count against the run's model budget (20 calls, 200,000 tokens).\n- `console.log(...)` output is captured, up to 64 KiB.\n- The program's last `return value` is the result, as JSON, up to 16 KiB.\n- There is no file, network, shell or other global access except through your tools.\n- Limits: 10 s of JavaScript CPU, 30 min wall time (time spent waiting on a person doesn't count, up to 10 min), 200 tool calls.\n\nThe result lists each call with its outcome. A call whose outcome is unknown was never retried, so check before repeating it.";

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "code":{"type":"string","maxLength":1_048_576},
        "description":{"type":"string","maxLength":1_024},
        "limits":{"type":"object","properties":{
            "wall_ms":{"type":"integer","minimum":1,"maximum":1_800_000},
            "tool_calls":{"type":"integer","minimum":1,"maximum":200},
            "output_bytes":{"type":"integer","minimum":1,"maximum":65_536}},"additionalProperties":false}},
        "required":["code"],"additionalProperties":false})
}

pub fn catalog_tool() -> CatalogTool {
    let schema = schema();
    let mut tool = CatalogTool::new(
        "codemode",
        schema_digest(&schema).expect("static schema"),
        1,
        schema,
    );
    tool.description = Some(DESCRIPTION.into());
    tool.reply =
        Some(serde_json::from_value(json!({"max_ms":2_520_000})).expect("static reply deadline"));
    tool
}

pub fn describe() -> Value {
    json!({"majors":[{"version":"tool-provider/v1","ops":["role.describe","tool.catalog","tool.withdraw","late_results","late_results.ack"],"stability":"alpha"}],"implementation_version":env!("CARGO_PKG_VERSION")})
}

pub fn catalog(arguments: Value) -> Result<Value, ErrorBody> {
    let request: CatalogRequest =
        serde_json::from_value(arguments).map_err(|e| invalid("arguments", e.to_string()))?;
    if request.preset.is_some() {
        return Err(invalid("preset", "basal has no named presets"));
    }
    if request.system_text.is_some() {
        return Err(invalid("system_text", "basal supplies no system text"));
    }
    let mut excluded = false;
    for (key, value) in &request.params {
        if key != "exclude" {
            return Err(invalid(&format!("params.{key}"), "unknown catalog axis"));
        }
        let names = value
            .as_array()
            .ok_or_else(|| invalid("params.exclude", "expected tool names"))?;
        for name in names {
            let name = name
                .as_str()
                .ok_or_else(|| invalid("params.exclude", "expected tool names"))?;
            excluded |= name == "codemode";
        }
    }
    let mut answer = CatalogAnswer::new("", "").with_tools(if excluded {
        vec![]
    } else {
        vec![catalog_tool()]
    });
    answer
        .capabilities
        .insert("late_results".into(), true.into());
    if let Some(composition) = &request.composition {
        answer.composition_digest = Some(
            composition_digest(&Value::Object(composition.clone()))
                .map_err(|e| invalid("composition", e.to_string()))?,
        );
    }
    answer.catalog_digest = blake3::hash(serde_json::to_string(&answer).unwrap().as_bytes())
        .to_hex()
        .to_string();
    answer.generation = answer.catalog_digest.clone();
    if request.digest_only == Some(true) {
        return Ok(json!({"generation":answer.generation,"catalog_digest":answer.catalog_digest}));
    }
    serde_json::to_value(answer).map_err(|e| invalid("arguments", e.to_string()))
}

pub fn invalid(field: &str, message: impl Into<String>) -> ErrorBody {
    ErrorBody::new("invalid_request", message).with_detail(json!({"field":field}))
}

pub fn principal(principal: &Principal) -> Result<String, ErrorBody> {
    match principal {
        Principal::Reserved { module_id } => Ok(format!("reserved:{module_id}")),
        Principal::Direct => Ok("direct".into()),
        _ => Err(ErrorBody::new(
            "not_permitted",
            "the route has no verified principal",
        )),
    }
}

#[derive(Clone)]
pub struct Context {
    pub principal: Principal,
    pub scope: Option<ScopeStamp>,
    pub bind_identity: subc_protocol::BindIdentity,
}

impl Context {
    pub fn identity(&self) -> Option<cortexkit_role_tool_provider::scope::ScopeIdentity> {
        self.scope
            .as_ref()
            .map(|scope| cortexkit_role_tool_provider::scope::ScopeIdentity {
                owner: principal(&scope.owner).unwrap_or_default(),
                scope_ref: scope.scope_ref.clone(),
                scope_epoch: scope.scope_epoch,
            })
    }
}
