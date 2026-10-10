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
    /// Core's `flow_install` rows by (flow, version): the approved code
    /// hash and whether the operator revoked it.
    pub installs: Mutex<BTreeMap<(String, i64), (String, bool)>>,
    /// Core's `flow_decision` cards.
    pub decisions: Mutex<DecisionCards>,
    /// The principal the daemon attests for the caller. The real transport
    /// carries it on the route; here a test sets it.
    pub requester: Mutex<String>,
    pub post_barrier: Mutex<Option<Arc<std::sync::Barrier>>>,
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
        *f.requester.lock().unwrap() = BASAL.into();
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
    /// Records core's install row for `flow_id` `version`, as core writes it
    /// when the operator approves the `flow_install` card basal raised for
    /// that version, with the code hash the card carried.
    pub fn approve_install(&self, flow_id: &str, version: i64) {
        let hash = self
            .calls("elicitation.request")
            .iter()
            .rev()
            .map(|c| &c["params"]["flow_install"])
            .find(|i| i["flow_id"] == flow_id && i["version"] == version)
            .and_then(|i| i["code_hash"].as_str().map(str::to_owned))
            .expect("basal raised a card for this version");
        self.installs
            .lock()
            .unwrap()
            .insert((flow_id.into(), version), (hash, false));
    }

    /// Revokes an install, as the operator's `flow.revoke` does in core.
    pub fn revoke_install(&self, flow_id: &str, version: i64) {
        self.installs
            .lock()
            .unwrap()
            .get_mut(&(flow_id.to_owned(), version))
            .expect("core holds the install")
            .1 = true;
    }

    pub fn enqueue(&self, op: &str, replies: Vec<Result<Value, WireError>>) {
        self.replies
            .lock()
            .unwrap()
            .insert(op.into(), replies.into());
    }
    fn reply(&self, op: &str, params: &Value) -> Result<Value, WireError> {
        if op == "post" {
            let barrier = self.post_barrier.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.wait();
            }
        }
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
            // Core takes `choose` only on flow_decision cards.
            "elicitation.request"
                if params["options"]
                    .as_array()
                    .is_some_and(|o| o.iter().any(|o| o["effect"] == "choose")) =>
            {
                Err(WireError::Refused {
                    code: "elicitation_invalid_request".into(),
                    message: "choose is only valid for flow_decision".into(),
                })
            }
            "elicitation.request" => flow_install_request(&self.scopes.lock().unwrap(), params),
            "flow.install_status" => install_status(&self.installs.lock().unwrap(), params),
            "elicitation.report_execution" => self.report_execution(params),
            "elicitation.withdraw" => {
                let mut decisions = self.decisions.lock().unwrap();
                let index = decisions
                    .cards
                    .iter()
                    .position(|(id, _, _)| params["elicitation_id"] == *id);
                if let Some(index) = index {
                    decisions.open.retain(|_, open| *open != index);
                }
                Ok(json!({"state":"withdrawn","elicitation_id":params["elicitation_id"]}))
            }
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

/// Core's `flow.install_status`, written from core's handler: `flow_id` a
/// non-empty string and `version` a positive integer, or the request is
/// refused; a version core never approved is `unknown` with a null hash,
/// and every approved version is `active` until it is revoked.
fn install_status(
    installs: &BTreeMap<(String, i64), (String, bool)>,
    params: &Value,
) -> Result<Value, WireError> {
    let invalid = |message: &str| {
        Err(WireError::Refused {
            code: "elicitation_invalid_request".into(),
            message: message.into(),
        })
    };
    let Some(flow_id) = params["flow_id"].as_str().filter(|s| !s.is_empty()) else {
        return invalid("missing flow_id");
    };
    let Some(version) = params["version"].as_i64().filter(|v| *v > 0) else {
        return invalid("invalid version");
    };
    Ok(match installs.get(&(flow_id.to_owned(), version)) {
        Some((hash, revoked)) => json!({
            "state": if *revoked { "revoked" } else { "active" },
            "code_hash": hash,
        }),
        None => json!({"state": "unknown", "code_hash": null}),
    })
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
    // `FlowInstallCard`): these fields must be strings and the version an
    // integer. `placement` is optional: absent (or null) is "not stated",
    // and an empty string is refused.
    let install = &params["flow_install"];
    for field in ["flow_id", "code_hash", "script", "manifest_json"] {
        if !install[field].is_string() {
            return refuse(
                invalid,
                &format!("flow_install.{field}: invalid type, expected a string"),
            );
        }
    }
    match install.get("placement") {
        None | Some(Value::Null) => {}
        Some(Value::String(p)) if !p.is_empty() => {}
        Some(Value::String(_)) => {
            return refuse(invalid, "flow_install.placement must not be empty");
        }
        Some(_) => {
            return refuse(
                invalid,
                "flow_install.placement: invalid type, expected a string",
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

/// The only requester core lets raise `flow_decision` cards.
pub const BASAL: &str = "reserved:basal";

/// The keys core's `ConsentRequest` decodes (prefrontal-core-store
/// `elicitation.rs` at tag `flow-decision-card-v2`); it refuses any other.
const REQUEST_KEYS: &[&str] = &[
    "flow_install",
    "flow_decision",
    "kind",
    "title",
    "prompt",
    "options",
    "default",
    "urgency",
    "dedup_key",
    "on_expiry",
    "material_damage",
    "scope",
    "duration",
    "args_digest",
    "late_execution",
    "session_ref",
    "parent_session_ref",
    "target",
    "facts",
    "preview",
    "expires_in_ms",
    "requestedSchema",
];

/// The keys of core's `flow_decision` body, tagged by `decision`
/// (prefrontal-core-store `FlowDecisionCard` at tag `flow-decision-card-v2`):
/// every key but `step` is required, and any other key is refused, so a v1
/// body, which lacks the typed context, is refused too.
const RECONCILE_KEYS: &[&str] = &[
    "decision",
    "flow_id",
    "version",
    "run_id",
    "run_admitted_at_ms",
    "step",
    "call_key",
    "op",
    "attempts",
    "unknown_reason",
];
const REENABLE_KEYS: &[&str] = &[
    "decision",
    "flow_id",
    "version",
    "disabled_at_ms",
    "disabled_reason",
    "limit",
    "window_ms",
    "saturated_windows",
];
const UNKNOWN_REASONS: &[&str] = &[
    "basal_restarted",
    "connection_lost",
    "reply_timeout",
    "reply_unreadable",
    "retries_exhausted",
    "provider_lost_run",
];
const DISABLED_REASONS: &[&str] = &["run_limit_saturated", "dispatch_limit_saturated"];

/// Checks a `flow_decision` body's keys, required fields and value limits
/// as core's decoder and `FlowDecisionCard::valid` do.
fn check_body(body: &Value) -> Result<(), String> {
    let id_ok = |name: &str| {
        body[name]
            .as_str()
            .is_some_and(|v| !v.trim().is_empty() && v.len() <= 256)
    };
    let positive = |name: &str| body[name].as_u64().is_some_and(|n| n > 0);
    let required = |keys: &[&str]| {
        keys.iter()
            .filter(|k| **k != "step")
            .all(|k| body.get(*k).is_some_and(|v| !v.is_null()))
    };
    let (keys, valid) = match body["decision"].as_str() {
        Some("grant_lost") => (
            &[
                "decision",
                "flow_id",
                "provider",
                "grant",
                "grant_label",
                "refused_at_ms",
            ][..],
            id_ok("provider")
                && body["grant"]
                    .as_str()
                    .is_some_and(|g| !g.is_empty() && g.len() <= 4096)
                && body["grant_label"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty())
                && body["refused_at_ms"].is_i64(),
        ),
        Some("reconcile") => {
            let op = body["op"].as_str().unwrap_or("");
            let segments: Vec<&str> = op.split('.').collect();
            let op_ok = op.len() <= 128
                && segments.len() >= 2
                && segments.iter().all(|s| {
                    !s.is_empty()
                        && s.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                        })
                });
            (
                RECONCILE_KEYS,
                id_ok("run_id")
                    && id_ok("call_key")
                    && op_ok
                    && positive("attempts")
                    && body["run_admitted_at_ms"].is_i64()
                    && body.get("step").is_none_or(Value::is_string)
                    && body["unknown_reason"]
                        .as_str()
                        .is_some_and(|r| UNKNOWN_REASONS.contains(&r)),
            )
        }
        Some("reenable") => (
            REENABLE_KEYS,
            positive("limit")
                && positive("window_ms")
                && positive("saturated_windows")
                && body["disabled_at_ms"].is_i64()
                && body["disabled_reason"]
                    .as_str()
                    .is_some_and(|r| DISABLED_REASONS.contains(&r)),
        ),
        _ => return Err("unknown flow_decision.decision".into()),
    };
    keys_within(body, keys, "flow_decision")?;
    if !required(keys) {
        return Err("flow_decision: a required field is missing".into());
    }
    if !(valid
        && id_ok("flow_id")
        && (body["decision"] == "grant_lost" || body["version"].as_i64().is_some_and(|v| v > 0)))
    {
        return Err("invalid flow_decision body".into());
    }
    Ok(())
}

fn keys_within(value: &Value, allowed: &[&str], what: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{what} is not an object"))?;
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("{what}: unknown field {k}")),
        None => Ok(()),
    }
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value[name]
        .as_str()
        .ok_or_else(|| format!("{name}: expected a string"))
}

/// Core's checks of a `flow_decision` request, written from core's rules
/// (`ConsentRequest::decode` and `validate`) rather than from basal's
/// encoder. A refusal is `(code, message)`.
fn check_flow_decision(params: &Value) -> Result<(), (&'static str, String)> {
    let invalid = |m: String| ("elicitation_invalid_request", m);
    // Refused even as null: the card is the operator's alone.
    for key in ["session_ref", "parent_session_ref"] {
        if params.get(key).is_some() {
            return Err(invalid(format!("flow_decision cannot carry {key}")));
        }
    }
    keys_within(params, REQUEST_KEYS, "request").map_err(invalid)?;
    if params.get("flow_install").is_some() {
        return Err(invalid("flow_decision cannot carry flow_install".into()));
    }
    for name in [
        "title",
        "prompt",
        "default",
        "urgency",
        "on_expiry",
        "late_execution",
    ] {
        text(params, name).map_err(invalid)?;
    }
    if !params["material_damage"].is_boolean() || !params["expires_in_ms"].is_i64() {
        return Err(invalid(
            "material_damage and expires_in_ms are required".into(),
        ));
    }
    let options = params["options"]
        .as_array()
        .ok_or_else(|| invalid("options is not a list".into()))?;
    let mut ids = std::collections::BTreeSet::new();
    let mut declines = 0;
    for option in options {
        keys_within(option, &["id", "label", "effect", "detail"], "option").map_err(invalid)?;
        let (id, label, effect) = (
            text(option, "id").map_err(invalid)?,
            text(option, "label").map_err(invalid)?,
            text(option, "effect").map_err(invalid)?,
        );
        if id.trim().is_empty() || label.trim().is_empty() || !ids.insert(id) {
            return Err(invalid(
                "option ids must be unique and nonempty; labels must be nonempty".into(),
            ));
        }
        match effect {
            "decline" => declines += 1,
            "choose" => {}
            _ => {
                return Err(invalid(
                    "flow_decision needs 2 to 4 options: exactly one decline and otherwise choose"
                        .into(),
                ));
            }
        }
    }
    if !(2..=4).contains(&options.len()) || declines != 1 {
        return Err(invalid(
            "flow_decision needs 2 to 4 options: exactly one decline and otherwise choose".into(),
        ));
    }
    let body = &params["flow_decision"];
    if body.is_null() {
        return Err(invalid("flow_decision body is required".into()));
    }
    check_body(body).map_err(invalid)?;
    if body["decision"] == "grant_lost"
        && (options.len() != 2
            || !options
                .iter()
                .any(|o| o["id"] == "check_now" && o["effect"] == "choose")
            || !options
                .iter()
                .any(|o| o["id"] == "keep_disabled" && o["effect"] == "decline"))
    {
        return Err(invalid(
            "grant_lost requires check_now and keep_disabled".into(),
        ));
    }
    if params["title"].as_str().unwrap_or("").trim().is_empty()
        || params["prompt"].as_str().unwrap_or("").trim().is_empty()
    {
        return Err(invalid(
            "title, prompt and nonempty options are required".into(),
        ));
    }
    let default = options
        .iter()
        .find(|o| o["id"] == params["default"])
        .ok_or_else(|| invalid("default must name an option".into()))?;
    if default["effect"] != "decline" || params["on_expiry"] != "deny" {
        return Err((
            "elicitation_fail_closed_required",
            "consent requires a decline default and on_expiry deny".into(),
        ));
    }
    if !matches!(params["urgency"].as_str(), Some("low" | "normal" | "high"))
        || !matches!(
            params["late_execution"].as_str(),
            Some("execute" | "notify_only")
        )
    {
        return Err(invalid("invalid urgency or late_execution".into()));
    }
    let expires = params["expires_in_ms"].as_i64().unwrap_or(0);
    if expires <= 0 || expires > 86_400_000 {
        return Err(invalid(
            "expires_in_ms must be positive and at most 24 hours".into(),
        ));
    }
    if params["args_digest"]
        .as_str()
        .is_none_or(|d| d.trim().is_empty())
        && params.get("scope").is_none()
    {
        return Err(invalid("provide args_digest or scope".into()));
    }
    keys_within(&params["target"], &["kind", "label"], "target").map_err(invalid)?;
    if text(&params["target"], "kind")
        .map_err(invalid)?
        .trim()
        .is_empty()
        || text(&params["target"], "label")
            .map_err(invalid)?
            .trim()
            .is_empty()
    {
        return Err(invalid("target kind and label must be nonempty".into()));
    }
    if let Some(facts) = params.get("facts") {
        for fact in facts
            .as_array()
            .ok_or_else(|| invalid("facts is not a list".into()))?
        {
            keys_within(fact, &["label", "value"], "fact").map_err(invalid)?;
            text(fact, "label").map_err(invalid)?;
            text(fact, "value").map_err(invalid)?;
        }
    }
    Ok(())
}

impl Fake {
    /// Core taking a `flow_decision` request: refused unless the requester
    /// is basal and the request passes core's checks; under the key of a
    /// card still open, that card is updated and keeps its id; otherwise a
    /// new card is created.
    fn flow_decision_request(&self, params: &Value) -> Result<Value, WireError> {
        if *self.requester.lock().unwrap() != BASAL {
            return Err(WireError::Refused {
                code: "elicitation_requester_refused".into(),
                message: "only reserved:basal may raise flow_decision".into(),
            });
        }
        if let Err((code, message)) = check_flow_decision(params) {
            return Err(WireError::Refused {
                code: code.into(),
                message,
            });
        }
        let key = params["dedup_key"].as_str().map(str::to_owned);
        let mut d = self.decisions.lock().unwrap();
        let open = key.as_ref().and_then(|k| d.open.get(k).copied());
        let index = match open {
            Some(i) => {
                d.cards[i].2.push(params.clone());
                i
            }
            None => {
                let id = format!("el_decision_{}", d.cards.len() + 1);
                d.cards
                    .push((id, key.clone().unwrap_or_default(), vec![params.clone()]));
                let i = d.cards.len() - 1;
                if let Some(key) = key {
                    d.open.insert(key, i);
                }
                i
            }
        };
        if d.lose_replies > 0 {
            d.lose_replies -= 1;
            return Err(WireError::Unknown("the reply was cut".into()));
        }
        Ok(json!({"elicitation_id": d.cards[index].0}))
    }

    /// Core refuses an execution report for a `flow_decision` card: core
    /// executes nothing for these cards, so there is nothing to report.
    fn report_execution(&self, params: &Value) -> Result<Value, WireError> {
        let id = params["elicitation_id"].as_str().unwrap_or_default();
        let decision = self
            .decisions
            .lock()
            .unwrap()
            .cards
            .iter()
            .any(|(card, _, _)| card == id);
        if decision {
            return Err(WireError::Refused {
                code: "elicitation_invalid_execution".into(),
                message: "flow_decision cards take no execution report".into(),
            });
        }
        Ok(json!({"elicitation_id": id}))
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
    /// card expires (`None`), and the card closes. As core does, an expiry
    /// is written into the answer log with state `expired` and the card's
    /// default (decline) option as its choice. The answer waits for basal;
    /// it is returned.
    pub fn answer_decision(&self, key: &str, choice: Option<&str>) -> Value {
        let mut d = self.decisions.lock().unwrap();
        let i = d.open.remove(key).expect("an open card under the key");
        let (id, _, requests) = d.cards[i].clone();
        let last = requests.last().cloned().unwrap_or_default();
        let (state, choice) = match choice {
            Some(choice) => ("answered", json!(choice)),
            None => ("expired", last["default"].clone()),
        };
        let record = json!({"elicitation_id":id,"state":state,"answered_choice_id":choice,
            "dedup_key":key,"flow_decision":last["flow_decision"]});
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
