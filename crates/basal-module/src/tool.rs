//! Basal's provider surface. Only the principal and scope stamped by the daemon
//! on the inbound route supply caller authority; model arguments never do.

use cortexkit_role_tool_provider::{
    CATALOG_CAPABILITY_FILTER, CallMode,
    catalog::{
        CatalogAnswer, CatalogRequest, CatalogTool, check_modes, composition_digest, schema_digest,
        select_by_capabilities, serves_call_mode,
    },
    describe::{Major, RoleDescribe},
};
use serde_json::{Value, json};
use subc_protocol::{ErrorBody, Principal, scope::ScopeStamp};

pub const DESCRIPTION: &str = "Run a short JavaScript program that calls your own tools and returns one result.\n\n- Call a tool with `await tools.<name>(input)`, using the same name as a direct call. Inside a program, tools take field names from their catalog schema, which can differ from your direct tool's; a refused call names the fields it expects. A program gets the tools whose providers serve codemode, and `Object.keys(tools)` lists them. Not every tool is available yet; calling a missing one throws an error that names it and lists the available ones. The tool decides allow, ask and deny exactly as for a direct call, and an ask waits for its answer.\n- `tools.model({prompt, system?, max_output?})` asks a model for one completion, and `tools.classify({text, labels})` picks one label; both count against the run's model budget (20 calls, 200,000 tokens).\n- `console.log(...)` output is captured, up to 64 KiB.\n- `await sleep(ms)` pauses for 0 to 10,000 ms; it uses wall time only, not CPU time or tool calls.\n- `await waitFor(condition, {timeout, interval})` calls `condition` every `interval` ms (default 500, at least 250) until it returns something truthy, and returns that; after `timeout` ms (default and maximum 30,000) it throws with `error.code` `wait_timeout`.\n- The program's last `return value` is the result, as JSON, up to 16 KiB.\n- There is no file, network, shell or other global access except through your tools.\n- Limits: 10 s of JavaScript CPU, 30 min wall time (time spent waiting on a person doesn't count, up to 10 min), 200 tool calls.\n\nA call that waited for consent may come back not run: it rejects with `error.code` `not_started`, and `error.detail.consent` `approved` means consent was granted but the page may have changed. Catch that, take a fresh snapshot, and act again:\n```js\ntry {\n  await tools.browser_use({action: \"type\", ref, text, snapshot_id});\n} catch (e) {\n  if (e.code !== \"not_started\" || e.detail?.consent !== \"approved\") throw e;\n  const fresh = await tools.browser_use({action: \"snapshot\"});\n  // find the element again in `fresh`, then repeat the call with its ref and snapshot_id\n}\n```\n\nThe result lists each call with its outcome. A call whose outcome is unknown was never retried, so check before repeating it.";

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
    tool.result_format = Some(cortexkit_role_tool_provider::result::ResultFormat::ContentV1);
    tool.reply =
        Some(serde_json::from_value(json!({"max_ms":2_520_000})).expect("static reply deadline"));
    // Core offers a head only the tools whose tags its allowlist names, so this
    // tag is the switch that grants codemode. It exposes nothing on its own.
    let tool = tool
        .with_capabilities(vec!["basal:codemode/v1".into()])
        .with_modes(vec![CallMode::Classic]);
    check_modes(&tool).expect("static codemode call modes");
    tool
}

pub fn check_call_mode(name: &str, mode: &CallMode) -> Result<(), ErrorBody> {
    // Role operations are metadata and custody operations, not catalog tools.
    // Unknown bind modes still fail closed, even on those operations.
    if matches!(mode, CallMode::Unknown(_))
        || (name == "codemode" && !serves_call_mode(&catalog_tool(), mode))
        || (name == "flow" && !serves_call_mode(&crate::flow_tool::catalog_tool(), mode))
    {
        let mode = serde_json::to_value(mode).expect("call mode wire spelling");
        let spelling = mode.as_str().expect("call mode is a string");
        return Err(ErrorBody::new(
            cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED,
            format!("tool {name:?} does not serve mode {spelling}"),
        )
        .with_detail(json!({"tool":name,"mode":mode})));
    }
    Ok(())
}

pub fn describe() -> Value {
    let tools = catalog_tools();
    let describe = RoleDescribe::new(
        vec![Major {
            version: "tool-provider/v1".into(),
            ops: vec![
                "role.describe".into(),
                "tool.catalog".into(),
                "tool.withdraw".into(),
                "late_results".into(),
                "late_results.ack".into(),
            ],
            stability: "alpha".into(),
        }],
        env!("CARGO_PKG_VERSION"),
    )
    .with_capabilities(vec![CATALOG_CAPABILITY_FILTER.into()])
    .with_tool_capabilities(
        tools
            .iter()
            .flat_map(|tool| tool.capabilities.iter().cloned()),
    );
    serde_json::to_value(describe).expect("role description is serializable")
}

fn catalog_tools() -> Vec<CatalogTool> {
    vec![catalog_tool(), crate::flow_tool::catalog_tool()]
}

fn catalog_content_digest(answer: &CatalogAnswer) -> String {
    let mut content = answer.clone();
    content.catalog_digest.clear();
    blake3::hash(&serde_json::to_vec(&content).expect("catalog answer is serializable"))
        .to_hex()
        .to_string()
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
    let full_tools = catalog_tools();
    let generation =
        blake3::hash(&serde_json::to_vec(&full_tools).expect("catalog tools are serializable"))
            .to_hex()
            .to_string();
    let tools = select_by_capabilities(&request.params, &full_tools)?
        .into_iter()
        .cloned()
        .collect();
    let mut answer = CatalogAnswer::new(generation, "").with_tools(tools);

    answer
        .capabilities
        .insert("late_results".into(), true.into());
    if let Some(composition) = &request.composition {
        answer.composition_digest = Some(
            composition_digest(&Value::Object(composition.clone()))
                .map_err(|e| invalid("composition", e.to_string()))?,
        );
    }
    answer.catalog_digest = catalog_content_digest(&answer);

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
    pub call_mode: CallMode,
    pub consumer_capabilities: Option<Vec<String>>,
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
