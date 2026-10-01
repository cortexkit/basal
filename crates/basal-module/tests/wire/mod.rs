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
            "elicitation.request" => Ok(json!({"elicitation_id":"el_1"})),
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
