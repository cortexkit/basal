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
}
impl Fake {
    pub fn new() -> Arc<Self> {
        let f = Arc::new(Self::default());
        *f.catalog.lock().unwrap() = json!({"generation":1,"subc_ops":["catalog.list"],"modules":[{"module_id":"mock","roles":[
            {"role":"management_surface","operations":[{"name":"echo","kind":"query"},{"name":"post","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"},
            {"role":"tool_provider","tools":[{"name":"send","execution_mode":"mutating","schema":{}},{"name":"read","execution_mode":"pure","schema":{}},{"name":"shell","execution_mode":"unfenceable","schema":{}}],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}
        ],"control_ops":[]}]});
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
            "agent.list" => Ok(
                json!({"agents":[{"agent_id":"ag_synapse","name":"SYNAPSE","name_version":1,"tag":"synapse","labels":[],"role":"head","sleep":false,"wake_policy_version":1,"reachability":{"state":"unknown"},"created_at":0,"residence":{"machine_id":"local","harness":"opencode","address_json":"{\"session\":\"ses-author\"}","residence_epoch":1,"residence_state":"active"}}]}),
            ),
            "route.select" => Ok(
                json!({"selected":{"model":{"providerID":"registry-provider","modelID":"registry-model"}},"decisionID":format!("decision:{}",params["sendID"].as_str().unwrap_or("")),"runner":{"provider":"fake","model":"test"}}),
            ),
            "route.set_decision_outcome" => Ok(json!({"ok":true})),
            "elicitation.request" => flow_install_request(params),
            "elicitation.answers" => Ok(json!({"records":[],"cursor":0})),
            "elicitation.ack" => Ok(json!({"ok":true})),
            "sink.digest" => Ok(json!({"disposition":"stored","fire_id":"wf_1","replayed":false})),
            "sink.status" => Ok(
                json!({"disposition":"published","accepted_revision":params["revision"],"segment":"flow:host-flow","scope":"session:ses-author","replayed":false}),
            ),
            "agent.facts" => Ok(
                json!({"agent_id":"ag_synapse","as_of":1,"home":"local","core_boot_at":0,"activity":{"state":{"value":null,"status":"unknown","observed_at":null,"privacy":"public","source":{"kind":"memory","ref":"session_activity_states","survives_restart":false}}}}),
            ),
            _ => Ok(params.clone()),
        }
    }
}

/// Core's checks of a `flow_install` request's author, written from core's
/// rules rather than from basal's encoder: `{"operator": true}`, `{"agent":
/// <id or name>}` with a non-empty `session_ref`, or `{"local": true}`,
/// which needs no session but is routed through the first digest sink's
/// agent, so its manifest must declare one. Every other form is refused.
/// The refusal code is the fake's choice: core's own code for each case is
/// not pinned here.
fn flow_install_request(params: &Value) -> Result<Value, WireError> {
    let invalid = |message: &str| {
        Err(WireError::Refused {
            code: "elicitation_invalid_request".into(),
            message: message.into(),
        })
    };
    let author = &params["flow_install"]["author"];
    let session = params["session_ref"].as_str().unwrap_or("");
    let sinks = params["flow_install"]["manifest_json"]
        .as_str()
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|m| m["sinks"].as_array().map(Vec::len))
        .unwrap_or(0);
    let accepted = if *author == json!({ "operator": true }) {
        true
    } else if *author == json!({ "local": true }) {
        if sinks == 0 {
            return invalid("a local caller's flow has no digest sink to route its card through");
        }
        true
    } else {
        let agent = author
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get("agent"))
            .and_then(Value::as_str);
        matches!(agent, Some(a) if !a.is_empty()) && !session.is_empty()
    };
    if !accepted {
        return invalid("author is not operator, agent with a session, or local");
    }
    Ok(json!({"elicitation_id":"el_1"}))
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
