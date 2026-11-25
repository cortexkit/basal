//! Basal's provider surface. Only the principal and scope stamped by the daemon
//! on the inbound route supply caller authority; model arguments never do.

use cortexkit_role_tool_provider::{
    CallMode,
    catalog::{
        CatalogAnswer, CatalogRequest, CatalogTool, SystemTextAnswer, check_modes,
        check_notice_templates_request, composition_digest, schema_digest, serves_call_mode,
        system_text_digest,
    },
    describe::{Major, RoleDescribe},
};
use serde_json::{Value, json};
use subc_protocol::{ErrorBody, Principal, scope::ScopeStamp};

const SYSTEM_TEXT: &str = include_str!("codemode_system_text.md");

/// Catalog requests that carried a capability filter since this process
/// started, and when the last one arrived. The filter is being retired from the
/// tool-provider contract; the contract drops it only after no runner has sent
/// one for a full day, and these counters in health are how basal shows that.
static FILTERED_CATALOG_REQUESTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static LAST_FILTERED_CATALOG_REQUEST_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn count_filtered_request(now_ms: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    FILTERED_CATALOG_REQUESTS.fetch_add(1, Relaxed);
    LAST_FILTERED_CATALOG_REQUEST_MS.fetch_max(now_ms, Relaxed);
}

/// The health metrics entry: `{count, last_at_ms}`, `last_at_ms` null until
/// the first filtered request.
pub fn filtered_catalog_requests() -> Value {
    use std::sync::atomic::Ordering::Relaxed;
    let last = LAST_FILTERED_CATALOG_REQUEST_MS.load(Relaxed);
    json!({
        "count": FILTERED_CATALOG_REQUESTS.load(Relaxed),
        "last_at_ms": (last != 0).then_some(last),
    })
}

pub const DESCRIPTION: &str = "Run a short JavaScript program that calls your own tools and returns one result.\n\nUse a program whenever several calls would each pass through your context or repeat the same input:\n- Near-identical calls: when calls share most of their input (one brief to two workers, the same brief per file, one message to several peers), write the shared text once in the program and change only what differs. Writing it out in each call pays for every word again, and your output tokens cost the most.\n- Lists: reads, checks or launches over a list run in one program, so their arguments and outputs never reach your context.\n- Dependent steps: launch, read the returned id, then use it in the same program, so you never write an id that doesn't exist yet.\nReturn only what you need next (ids, counts, failures), never raw tool output. Check each call's result, so a failure inside a loop is reported, not skipped. For a single call, or one whose full output you must read, call the tool directly.\n\n- Call a tool with `await tools.<name>(input)`, using the same name as a direct call. Inside a program, tools take field names from their catalog schema, which can differ from your direct tool's; a refused call names the fields it expects. A program gets the tools whose providers serve codemode, and `Object.keys(tools)` lists them. Not every tool is available yet; calling a missing one throws an error that names it and lists the available ones. The tool decides allow, ask and deny exactly as for a direct call, and an ask waits for its answer.\n- `tools.llm_complete({demand, prompt, max_output_tokens, max_cost_usd})` asks a model for one completion. `tools.llm_classify({model, questions, item, max_cost_usd})` classifies an item; model tool calls share a $0.50 budget per run, which `limits.model_cost_usd` can lower.\n- `console.log(...)` output is captured, up to 64 KiB.\n- `await sleep(ms)` pauses for 0 to 10,000 ms; it uses wall time only, not CPU time or tool calls.\n- `await waitFor(condition, {timeout, interval})` calls `condition` every `interval` ms (default 500, at least 250) until it returns something truthy, and returns that; after `timeout` ms (default and maximum 30,000) it throws with `error.code` `wait_timeout`.\n- The program's last `return value` is the result, as JSON, up to 16 KiB.\n- There is no file, network, shell or other global access except through your tools.\n- Limits: 10 s of JavaScript CPU, 30 min wall time (time spent waiting on a person doesn't count, up to 10 min), 200 tool calls.\n\nA call that waited for consent may come back not run: it rejects with `error.code` `not_started`, and `error.detail.consent` `approved` means consent was granted but the page may have changed. Catch that, take a fresh snapshot, and act again:\n```js\ntry {\n  await tools.browser_use({action: \"type\", ref, text, snapshot_id});\n} catch (e) {\n  if (e.code !== \"not_started\" || e.detail?.consent !== \"approved\") throw e;\n  const fresh = await tools.browser_use({action: \"snapshot\"});\n  // find the element again in `fresh`, then repeat the call with its ref and snapshot_id\n}\n```\n\nThe result lists each call with its outcome. A call whose outcome is unknown was never retried, so check before repeating it.\n\nTo run many steps as one program instead of one call at a time, see knowhow(id=\"codemode-automation\").";

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "code":{"type":"string","maxLength":1_048_576},
        "description":{"type":"string","maxLength":1_024},
        "limits":{"type":"object","properties":{
            "wall_ms":{"type":"integer","minimum":1,"maximum":1_800_000},
            "tool_calls":{"type":"integer","minimum":1,"maximum":200},
            "output_bytes":{"type":"integer","minimum":1,"maximum":65_536},
            "model_cost_usd":{"type":"number","minimum":0,"maximum":0.50}},"additionalProperties":false}},
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
    tool.result_format = Some(cortexkit_role_tool_provider::result::ResultFormat::ToolResultV1);
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
    if request.params.contains_key("capabilities") {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        count_filtered_request(now_ms);
        return Err(invalid(
            "params.capabilities",
            "basal does not support catalog capability filters",
        ));
    }
    check_preset("preset", request.preset.as_deref())?;
    // Basal serves no session notices, so the contract requires it to refuse
    // even `notice_templates: false` rather than ignore the member. Building
    // the manifest is only worth it when the member is present.
    if request.notice_templates.is_some() {
        let provides = crate::manifest::manifest()
            .capabilities
            .map(|c| c.provides)
            .unwrap_or_default();
        check_notice_templates_request(&request, &provides)?;
    }
    if let Some(item) = &request.system_text {
        check_preset("system_text.preset", item.preset.as_deref())?;
        if let Some(field) = item.params.keys().next() {
            return Err(invalid(
                &format!("system_text.params.{field}"),
                "unknown system text parameter",
            ));
        }
    }
    let full_tools = catalog_tools();
    let generation = blake3::hash(
        &serde_json::to_vec(&(&full_tools, SYSTEM_TEXT))
            .expect("catalog definitions are serializable"),
    )
    .to_hex()
    .to_string();
    let tools = full_tools
        .into_iter()
        .filter(|tool| request.preset.is_none() || tool.name == "codemode")
        .collect();
    let mut answer = CatalogAnswer::new(generation, "").with_tools(tools);
    if request.preset.is_none() {
        answer = answer.with_presets(PRESETS);
    }

    answer
        .capabilities
        .insert("late_results".into(), true.into());
    if let Some(composition) = &request.composition {
        answer.composition_digest = Some(
            composition_digest(&Value::Object(composition.clone()))
                .map_err(|e| invalid("composition", e.to_string()))?,
        );
    }
    if request.system_text.is_some() {
        let digest = system_text_digest(SYSTEM_TEXT);
        let mut text = SystemTextAnswer::new(&digest, &digest)
            .with_text(SYSTEM_TEXT)
            .with_tool_names(answer.tools.iter().map(|tool| tool.name.clone()));
        text.composition_digest = answer.composition_digest.clone();
        answer.system_text = Some(text);
    }
    answer.catalog_digest = catalog_content_digest(&answer);

    if request.digest_only == Some(true) {
        return Ok(json!({"generation":answer.generation,"catalog_digest":answer.catalog_digest}));
    }
    serde_json::to_value(answer).map_err(|e| invalid("arguments", e.to_string()))
}

/// The session presets basal answers. Every session kind gets only codemode,
/// with its system text: a program can call only its own session's tools, so
/// a read-only session's programs stay read-only. `flow` is in the preset-less
/// catalog only, because prefrontal relays it to heads itself and a second
/// copy from a preset would collide with that one.
const PRESETS: [&str; 3] = ["head", "worker", "reader"];

fn check_preset(field: &str, preset: Option<&str>) -> Result<(), ErrorBody> {
    match preset {
        None => Ok(()),
        Some(name) if PRESETS.contains(&name) => Ok(()),
        Some(name) => Err(invalid(field, format!("basal has no preset {name:?}"))),
    }
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

#[cfg(test)]
mod filtered_request_tests {
    use super::*;

    fn count() -> u64 {
        filtered_catalog_requests()["count"].as_u64().unwrap()
    }

    // The counters are process-wide and other tests may send catalog requests
    // in parallel, so assert only on this test's own increments.
    #[test]
    fn filtered_catalog_requests_are_counted_even_when_refused() {
        let before = count();
        // Basal no longer offers the filter, so every filtered request is
        // refused, but the count still shows that a runner sent one.
        let refused =
            catalog(json!({"params": {"capabilities": ["basal:codemode/v1"]}})).unwrap_err();
        assert_eq!(refused.code, "invalid_request");
        let _ = catalog(json!({"preset": "unknown", "params": {"capabilities": []}}));
        assert!(count() >= before + 2);
        let last = filtered_catalog_requests()["last_at_ms"].as_u64().unwrap();
        assert!(
            last > 1_700_000_000_000,
            "last_at_ms is wall-clock milliseconds"
        );
    }

    #[test]
    fn an_older_time_never_moves_the_last_time_backwards() {
        count_filtered_request(u64::MAX - 1);
        count_filtered_request(5);
        assert_eq!(
            filtered_catalog_requests()["last_at_ms"].as_u64(),
            Some(u64::MAX - 1)
        );
    }
}
