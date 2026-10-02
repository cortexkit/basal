#![allow(dead_code)]
use basal_host::transport::{Transport, WireError, management_body, tool_body};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Fake {
    pub records: Mutex<Vec<(String, Value)>>,
    pub replies: Mutex<BTreeMap<String, VecDeque<Result<Value, WireError>>>>,
    pub catalog: Mutex<Value>,
    /// Core's own records of agent session scopes, by scope ref.
    pub scopes: Mutex<BTreeMap<String, CoreScope>>,
}
impl Fake {
    pub fn new() -> Arc<Self> {
        let f = Arc::new(Self::default());
        *f.catalog.lock().unwrap() = json!({"generation":1,"subc_ops":["catalog.list"],"modules":[{"module_id":"mock","roles":[
            {"role":"management_surface","operations":[{"name":"echo","kind":"query"},{"name":"post","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"},
            {"role":"tool_provider","tools":[{"name":"send","execution_mode":"mutating","schema":{}},{"name":"read","execution_mode":"pure","schema":{}},{"name":"shell","execution_mode":"unfenceable","schema":{}}],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
        ],"control_ops":[]}]});
        for scope_ref in LIVE_SYNAPSE_SCOPES {
            f.scopes.lock().unwrap().insert(
                (*scope_ref).into(),
                CoreScope {
                    agent: Some("ag_synapse".into()),
                    live: true,
                },
            );
        }
        f
    }
    pub fn enqueue(&self, op: &str, replies: Vec<Result<Value, WireError>>) {
        self.replies
            .lock()
            .unwrap()
            .insert(op.into(), replies.into());
    }
    fn reply(&self, op: &str, params: &Value) -> Result<Value, WireError> {
        if let Some(r) = self
            .replies
            .lock()
            .unwrap()
            .get_mut(op)
            .and_then(VecDeque::pop_front)
        {
            return r;
        }
        match op {
            // A head's entry as core lists it: its project and workspace, and a
            // residence whose address is an object and whose state is `live`.
            "agent.list" => Ok(
                json!({"agents":[{"agent_id":"ag_synapse","name":"SYNAPSE","name_version":1,"tag":"synapse","labels":[],"role":"head","project_id":"pj-synapse","workspace_id":"synapse","sleep":false,"wake_policy_version":1,"reachability":{"state":"unknown"},"created_at":0,"residence":{"machine_id":"local","harness":"opencode","address_json":{"version":1,"server":"local","session":"ses-author"},"residence_epoch":1,"residence_state":"live"}}]}),
            ),
            "route.select" => Ok(
                json!({"selected":{"model":{"providerID":"registry-provider","modelID":"registry-model"}},"decisionID":format!("decision:{}",params["sendID"].as_str().unwrap_or("")),"runner":{"provider":"fake","model":"test"}}),
            ),
            "route.set_decision_outcome" => Ok(json!({"ok":true})),
            "elicitation.request" => flow_install_request(&self.scopes.lock().unwrap(), params),
            "elicitation.answers" => Ok(json!({"records":[],"cursor":0})),
            "elicitation.ack" => Ok(json!({"ok":true})),
            "sink.digest" => Ok(json!({"disposition":"stored","fire_id":"wf_1","replayed":false})),
            "sink.status" => Ok(
                json!({"disposition":"published","accepted_revision":params["revision"],"segment":"flow:host-flow","scope":"session:ses-author","replayed":false}),
            ),
            // Core stamps every fact with its own `as_of` as well as the reply's.
            "agent.facts" => Ok(
                json!({"agent_id":"ag_synapse","as_of":1,"home":"local","core_boot_at":0,"activity":{"state":{"value":null,"status":"unknown","as_of":1,"observed_at":null,"privacy":"public","source":{"kind":"memory","ref":"session_activity_states","survives_restart":false}}}}),
            ),
            _ => Ok(params.clone()),
        }
    }
}

/// A session scope as core records it: the agent it carries, if any, and
/// whether it is still live.
#[derive(Clone, Debug)]
pub struct CoreScope {
    pub agent: Option<String>,
    pub live: bool,
}

/// Scope refs the fake's core holds as live sessions of SYNAPSE from the
/// start: the ref `common::agent` stamps and the refs the host tests bind.
pub const LIVE_SYNAPSE_SCOPES: &[&str] = &[
    "scope-of-SYNAPSE",
    "5e1f0c3a9b7d4e2f8a6c1b3d5f7e9a0c",
    "scope-live",
];

/// Core's checks of a `flow_install` request's author, written from core's
/// rules rather than from basal's encoder. Exactly three forms are accepted:
///
/// - `{"operator": true}`;
/// - `{"local": true}`, which core routes through the first digest sink's
///   agent, so its manifest must declare one;
/// - `{"scope": <scope_ref>}`, with no `session_ref`: core looks the scope
///   up in its own records, refusing `flow_install_scope_unknown` for a ref
///   it never minted or one that carries no agent, and
///   `flow_install_scope_ended` for a scope no longer live.
///
/// Every other form, the retired `{"agent": ...}` included, is refused with
/// `elicitation_invalid_request`; that code is the fake's choice.
fn flow_install_request(
    scopes: &BTreeMap<String, CoreScope>,
    params: &Value,
) -> Result<Value, WireError> {
    let refuse = |code: &str, message: &str| {
        Err(WireError::Refused {
            code: code.into(),
            message: message.into(),
        })
    };
    let invalid = "elicitation_invalid_request";
    // Core decodes the nested payload strictly (prefrontal-core-store
    // `FlowInstallCard`): these fields must be strings, `placement` included
    // even when the manifest names none, and the version an integer.
    let install = &params["flow_install"];
    for field in [
        "flow_id",
        "code_hash",
        "script",
        "manifest_json",
        "placement",
    ] {
        if !install[field].is_string() {
            return refuse(
                invalid,
                &format!("flow_install.{field}: invalid type, expected a string"),
            );
        }
    }
    if !install["version"].is_i64() {
        return refuse(invalid, "flow_install.version: expected an integer");
    }
    let author = &params["flow_install"]["author"];
    let sinks = params["flow_install"]["manifest_json"]
        .as_str()
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|m| m["sinks"].as_array().map(Vec::len))
        .unwrap_or(0);
    if *author == json!({ "operator": true }) {
        return Ok(json!({"elicitation_id":"el_1"}));
    }
    if *author == json!({ "local": true }) {
        if sinks == 0 {
            return refuse(
                invalid,
                "a local caller's flow has no digest sink to route its card through",
            );
        }
        return Ok(json!({"elicitation_id":"el_1"}));
    }
    let scope_ref = author
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("scope"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let Some(scope_ref) = scope_ref else {
        return refuse(invalid, "author is not operator, scope or local");
    };
    if params.get("session_ref").is_some() {
        return refuse(
            invalid,
            "a scope author's session is core's to find; session_ref is refused",
        );
    }
    match scopes.get(scope_ref) {
        Some(CoreScope {
            agent: Some(_),
            live: true,
        }) => Ok(json!({"elicitation_id":"el_1"})),
        Some(CoreScope {
            agent: Some(_),
            live: false,
        }) => refuse("flow_install_scope_ended", "the scope has ended"),
        _ => refuse(
            "flow_install_scope_unknown",
            "core holds no agent session scope under this ref",
        ),
    }
}

impl Fake {
    pub fn calls(&self, op: &str) -> Vec<Value> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, v)| v["method"] == op || v["name"] == op)
            .map(|(_, v)| v.clone())
            .collect()
    }
}
impl Transport for Fake {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(self.catalog.lock().unwrap().clone())
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.records
            .lock()
            .unwrap()
            .push((module.into(), management_body(op, params.clone())));
        self.reply(op, &params)
    }
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        key: &str,
    ) -> Result<Value, WireError> {
        self.records
            .lock()
            .unwrap()
            .push((module.into(), tool_body(name, arguments.clone(), key)?));
        self.reply(name, &arguments)
    }
}
