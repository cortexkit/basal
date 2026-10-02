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
    /// Core's `flow_decision` cards.
    pub decisions: Mutex<DecisionCards>,
}

/// The `flow_decision` cards the fake's core holds, and the answers it
/// keeps until basal acknowledges them.
#[derive(Default)]
pub struct DecisionCards {
    /// Every card core ever created: (elicitation id, dedup key, the
    /// requests that created or updated it, newest last).
    pub cards: Vec<(String, String, Vec<Value>)>,
    /// Open cards by dedup key, as an index into `cards`.
    pub open: BTreeMap<String, usize>,
    /// Answers not yet acknowledged, with their cursor.
    pub answers: Vec<(i64, Value)>,
    pub cursor: i64,
    /// Requests to accept and then lose the reply of, as if the reply was
    /// cut after core committed the card.
    pub lose_replies: u32,
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
            "elicitation.request" if params["kind"] == "flow_decision" => {
                self.flow_decision_request(params)
            }
            "elicitation.request" => flow_install_request(&self.scopes.lock().unwrap(), params),
            "elicitation.answers" => {
                let decisions = self.decisions.lock().unwrap();
                let records: Vec<Value> =
                    decisions.answers.iter().map(|(_, r)| r.clone()).collect();
                let cursor = decisions.answers.last().map(|(c, _)| *c).unwrap_or(0);
                Ok(json!({"records":records,"cursor":cursor}))
            }
            "elicitation.ack" => {
                let cursor = params["cursor"].as_i64().unwrap_or(0);
                self.decisions
                    .lock()
                    .unwrap()
                    .answers
                    .retain(|(c, _)| *c > cursor);
                Ok(json!({"ok":true}))
            }
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

/// Core's checks of a `flow_decision` request, written from core's rules
/// rather than from basal's encoder: kind `flow_decision`; 2 to 4 options,
/// each exactly an id, a label and an effect, with distinct ids; every
/// action option's effect `choose` and exactly one option's `decline`,
/// which is the default; expiry declines; a dedup key; no session, author
/// or subject, since the card is the operator's alone; and the typed body
/// with a run and call key on reconcile cards only. Anything else is
/// refused with `elicitation_invalid_request`, the fake's choice of code.
fn check_flow_decision(params: &Value) -> Result<(), String> {
    let object = params.as_object().ok_or("the request is not an object")?;
    for key in ["session_ref", "author", "subject", "agent", "flow_install"] {
        if object.contains_key(key) {
            return Err(format!("a flow_decision request carries no {key}"));
        }
    }
    let options = params["options"]
        .as_array()
        .ok_or("options is not a list")?;
    if !(2..=4).contains(&options.len()) {
        return Err("a card has 2 to 4 options".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut declines = Vec::new();
    for option in options {
        let fields = option.as_object().ok_or("an option is not an object")?;
        if fields.len() != 3 {
            return Err("an option is exactly an id, a label and an effect".into());
        }
        let id = option["id"].as_str().filter(|s| !s.is_empty());
        let label = option["label"].as_str().filter(|s| !s.is_empty());
        let (Some(id), Some(_)) = (id, label) else {
            return Err("an option needs a non-empty id and label".into());
        };
        if !ids.insert(id) {
            return Err(format!("option id {id} is repeated"));
        }
        match option["effect"].as_str() {
            Some("choose") => {}
            Some("decline") => declines.push(id),
            other => return Err(format!("option effect {other:?} is not choose or decline")),
        }
    }
    if declines.len() != 1 {
        return Err("exactly one option declines".into());
    }
    if params["default"].as_str() != Some(declines[0]) {
        return Err("the default is the declining option".into());
    }
    if params["on_expiry"] != "deny" {
        return Err("expiry declines".into());
    }
    if !params["dedup_key"].as_str().is_some_and(|k| !k.is_empty()) {
        return Err("a flow_decision card has a dedup key".into());
    }
    let body = params["flow_decision"]
        .as_object()
        .ok_or("flow_decision is not an object")?;
    if !body
        .keys()
        .all(|k| ["flow_id", "version", "decision", "run_id", "call_key"].contains(&k.as_str()))
    {
        return Err("flow_decision has an unknown field".into());
    }
    if !body.get("flow_id").is_some_and(Value::is_string) {
        return Err("flow_decision.flow_id: expected a string".into());
    }
    if !body.get("version").is_some_and(Value::is_u64) {
        return Err("flow_decision.version: expected an integer".into());
    }
    let run = body.get("run_id").is_some_and(Value::is_string);
    let call = body.get("call_key").is_some_and(Value::is_string);
    match body.get("decision").and_then(Value::as_str) {
        Some("reconcile") if run && call => Ok(()),
        Some("reenable") if !body.contains_key("run_id") && !body.contains_key("call_key") => {
            Ok(())
        }
        _ => Err("flow_decision.decision with its run and call key is invalid".into()),
    }
}

impl Fake {
    /// Core taking a `flow_decision` request: refused unless it passes
    /// core's checks; under the key of a card still open, that card is
    /// updated and keeps its id; otherwise a new card is created.
    fn flow_decision_request(&self, params: &Value) -> Result<Value, WireError> {
        if let Err(message) = check_flow_decision(params) {
            return Err(WireError::Refused {
                code: "elicitation_invalid_request".into(),
                message,
            });
        }
        let key = params["dedup_key"].as_str().unwrap_or_default().to_owned();
        let mut d = self.decisions.lock().unwrap();
        let index = match d.open.get(&key) {
            Some(&i) => {
                d.cards[i].2.push(params.clone());
                i
            }
            None => {
                let id = format!("el_decision_{}", d.cards.len() + 1);
                d.cards.push((id, key.clone(), vec![params.clone()]));
                let i = d.cards.len() - 1;
                d.open.insert(key, i);
                i
            }
        };
        if d.lose_replies > 0 {
            d.lose_replies -= 1;
            return Err(WireError::Unknown("the reply was cut".into()));
        }
        Ok(json!({"elicitation_id": d.cards[index].0}))
    }

    /// Every flow_decision card core created under `key`.
    pub fn decision_cards(&self, key: &str) -> Vec<(String, Vec<Value>)> {
        self.decisions
            .lock()
            .unwrap()
            .cards
            .iter()
            .filter(|(_, k, _)| k == key)
            .map(|(id, _, requests)| (id.clone(), requests.clone()))
            .collect()
    }

    /// Every flow_decision card core created.
    pub fn all_decision_cards(&self) -> Vec<(String, String, Vec<Value>)> {
        self.decisions.lock().unwrap().cards.clone()
    }

    /// The operator answers the open card under `key` with `choice`, or the
    /// card expires (`None`). The card closes and the answer waits for
    /// basal. Returns the answer record.
    pub fn answer_decision(&self, key: &str, choice: Option<&str>) -> Value {
        let mut d = self.decisions.lock().unwrap();
        let i = d.open.remove(key).expect("an open card under the key");
        let (id, _, requests) = d.cards[i].clone();
        let last = requests.last().cloned().unwrap_or_default();
        let record = match choice {
            Some(choice) => json!({"elicitation_id":id,"state":"answered",
                "answered_choice_id":choice,"flow_decision":last["flow_decision"]}),
            None => json!({"elicitation_id":id,"state":"expired",
                "flow_decision":last["flow_decision"]}),
        };
        drop(d);
        self.deliver(record.clone());
        record
    }

    /// Puts an answer record on core's answer page (again).
    pub fn deliver(&self, record: Value) {
        let mut d = self.decisions.lock().unwrap();
        d.cursor += 1;
        let cursor = d.cursor;
        d.answers.push((cursor, record));
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
