//! Core retains answers until basal has durably applied every decision in a page.
use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};
use crate::{
    CardDecision, Consent, ConsentError, DecisionAnswer, DecisionCard, DecisionContext,
    DecisionEvent, DecisionKind, DecisionSink, InstallCard,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Core's refusal of a scope author whose scope it does not own, or that
/// carries no agent.
pub const SCOPE_UNKNOWN: &str = "flow_install_scope_unknown";
/// Core's refusal of a scope author whose scope is no longer live.
pub const SCOPE_ENDED: &str = "flow_install_scope_ended";

fn error(e: WireError) -> ConsentError {
    match e {
        WireError::Refused { code, message } | WireError::RefusedDetails { code, message, .. }
            if code == SCOPE_UNKNOWN || code == SCOPE_ENDED =>
        {
            ConsentError::AuthorScope { code, message }
        }
        WireError::Refused { code, message } | WireError::RefusedDetails { code, message, .. } => {
            ConsentError::Refused(format!("{code}: {message}"))
        }
        other => ConsentError::Unavailable(format!("{other:?}")),
    }
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str, ConsentError> {
    v.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ConsentError::Refused(format!("card is missing {name}")))
}
pub fn request(card: &InstallCard) -> Result<Value, ConsentError> {
    let f = &card.fields;
    let hash = field(f, "code_hash")?;
    let script = field(&f["code"], "script")?;
    let manifest = field(&f["code"], "manifest")?;
    let author = f
        .get("wire_author")
        .cloned()
        .ok_or_else(|| ConsentError::Refused("card is missing its attested author".into()))?;
    author_kind(&author)?;
    // Core takes `placement` as optional, rendering "not stated" when it is
    // absent, and refuses an empty string. So it is sent only when the
    // manifest states one; the manifest itself never holds an empty one.
    let placement = f["placement"].as_str().filter(|p| !p.is_empty());
    // No form carries `session_ref`: core finds an agent's session in the
    // scope the author names, and a scope author sent with one is refused.
    let result = json!({"kind":"flow_install","title":format!("Install flow {} v{}",card.flow_id,card.version),
        "prompt":field(f,"purpose")?,"options":[{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}],
        "default":"decline","urgency":"normal","on_expiry":"deny","material_damage":false,"late_execution":"notify_only",
        "args_digest":hash,"dedup_key":format!("flow_install:{}:{}",card.flow_id,card.version),
        "target":{"kind":"flow","label":format!("{} v{}",card.flow_id,card.version)},"facts":[],
        "preview":{"label":"Code","text":script},"expires_in_ms":86400000,
        "flow_install":{"flow_id":card.flow_id,"version":card.version,"code_hash":hash,"script":script,"manifest_json":manifest,
            "author":author,"warnings":f["warnings"].as_array().map(|warnings| warnings.iter().map(|w| json!({"code":w["kind"],"detail":w["text"]})).collect::<Vec<_>>()).unwrap_or_default(),"dry_run_summary":f["dry_run_summary"],"token_usage":{"window":f["token_cap"]["window"].as_str().unwrap_or("1d"),"fresh_input":f["token_window"]["input_tokens"].as_u64().unwrap_or(0),"cache_write":f["token_window"]["cache_write_tokens"].as_u64().unwrap_or(0),"output":f["token_window"]["output_tokens"].as_u64().unwrap_or(0),"cache_read":f["token_window"]["cached_input_tokens"].as_u64().unwrap_or(0)}}});
    let mut result = result;
    if let Some(placement) = placement {
        result["flow_install"]["placement"] = json!(placement);
    }
    Ok(result)
}

/// The `effect` value basal sends on every action option of a
/// `flow_decision` card, as core's contract names it. The contract may
/// rename it to `grant`, so the name is held in this one constant.
pub const DECISION_ACTION_EFFECT: &str = "choose";
/// The `effect` value of a card's declining option, the one that does
/// nothing and is the default.
pub const DECISION_DECLINE_EFFECT: &str = "decline";
/// How long core keeps a decision card open before it expires to its
/// default: core's maximum, a day.
pub const DECISION_EXPIRES_IN_MS: u64 = 86_400_000;

/// The `elicitation.request` for an operator decision card, built only
/// here. Given the inputs of core's test vectors
/// (`test-vectors/flow-decision-card-v2/` at prefrontal tag
/// `flow-decision-card-v2`) it produces their bytes exactly; basal's own
/// cards differ only in values and in two optional fields core accepts,
/// `dedup_key` and `args_digest`.
///
/// The card goes to the operator alone, so the request carries no
/// `session_ref` or `parent_session_ref` key at all (core refuses either,
/// even as null) and no author or subject. `scope` names the flow, which
/// core requires when there is no `args_digest`. The typed `flow_decision`
/// body (see [`flow_decision_body`]) states what the card is about; core
/// writes the card's facts from it and never authorizes anything with it,
/// so basal sends no facts of its own. A card that breaks core's rules is
/// refused here rather than sent: 2 to 4 options with distinct, non-empty
/// ids and labels, exactly one declining option, a positive expiry of at
/// most a day, and a valid body (see [`body_problem`]).
pub fn decision_request(card: &DecisionCard) -> Result<Value, ConsentError> {
    let refused = |why: &str| {
        Err(ConsentError::Refused(format!(
            "decision card for flow {}: {why}",
            card.flow_id
        )))
    };
    if !(2..=4).contains(&card.options.len()) {
        return refused("a card has 2 to 4 options");
    }
    let mut ids: Vec<&str> = card.options.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != card.options.len()
        || card
            .options
            .iter()
            .any(|o| o.id.trim().is_empty() || o.label.trim().is_empty())
    {
        return refused("option ids are distinct and not empty, and labels not empty");
    }
    let declines: Vec<&str> = card
        .options
        .iter()
        .filter(|o| o.decline)
        .map(|o| o.id.as_str())
        .collect();
    let [default] = declines.as_slice() else {
        return refused("exactly one option declines");
    };
    if let Some(problem) = body_problem(card) {
        return refused(problem);
    }
    if card.expires_in_ms == 0 || card.expires_in_ms > DECISION_EXPIRES_IN_MS {
        return refused("a card expires within a day");
    }
    let options: Vec<Value> = card
        .options
        .iter()
        .map(|o| {
            let effect = if o.decline {
                DECISION_DECLINE_EFFECT
            } else {
                DECISION_ACTION_EFFECT
            };
            json!({"id":o.id,"label":o.label,"effect":effect})
        })
        .collect();
    let mut request = json!({"kind":"flow_decision","title":card.title,"prompt":card.prompt,
        "options":options,"default":default,"urgency":"normal","on_expiry":"deny",
        "material_damage":false,"late_execution":"notify_only",
        "scope":{"flow_id":card.flow_id},"target":{"kind":"flow","label":card.flow_id},
        "expires_in_ms":card.expires_in_ms,"flow_decision":flow_decision_body(card)});
    if let Some(key) = &card.dedup_key {
        request["dedup_key"] = json!(key);
    }
    if let Some(digest) = &card.args_digest {
        request["args_digest"] = json!(digest);
    }
    Ok(request)
}

/// What core would refuse in a card's body, if anything: a blank flow id or
/// one over 256 bytes, a version of 0; on a reconcile card a blank or
/// overlong run id or call key, no attempts, or an op that is not two or
/// more dot-separated segments of lowercase letters, digits, `_` and `-`,
/// at most 128 bytes; on a re-enable card a zero limit, window or
/// saturated-window count.
pub fn body_problem(card: &DecisionCard) -> Option<&'static str> {
    let id_ok = |id: &str| !id.trim().is_empty() && id.len() <= 256;
    if !id_ok(&card.flow_id) || card.version == 0 {
        return Some("a card names a flow and a version above 0");
    }
    match &card.context {
        DecisionContext::Reconcile {
            run_id,
            call_key,
            op,
            attempts,
            ..
        } => {
            let segment_ok = |s: &str| {
                !s.is_empty()
                    && s.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                    })
            };
            let op_ok =
                op.len() <= 128 && op.split('.').count() >= 2 && op.split('.').all(segment_ok);
            if !id_ok(run_id) || !id_ok(call_key) {
                Some("a reconcile card names its run and call")
            } else if !op_ok {
                Some("a reconcile card's op is `module.op` in lowercase segments")
            } else if *attempts == 0 {
                Some("a reconcile card's call was sent at least once")
            } else {
                None
            }
        }
        DecisionContext::Reenable {
            limit,
            window_ms,
            saturated_windows,
            ..
        } => (*limit == 0 || *window_ms == 0 || *saturated_windows == 0)
            .then_some("a re-enable card's limit, window and saturated windows are above 0"),
    }
}

/// The typed `flow_decision` body of a card, tagged by `decision`: for a
/// reconcile card `run_id`, `run_admitted_at_ms`, `step` (only when known),
/// `call_key`, `op` (`module.op`), `attempts` and `unknown_reason`; for a
/// re-enable card `disabled_at_ms`, `disabled_reason`, `limit`, `window_ms`
/// and `saturated_windows`. Core refuses any other key and any reason
/// outside its closed sets.
pub fn flow_decision_body(card: &DecisionCard) -> Value {
    let mut body = json!({"flow_id":card.flow_id,"version":card.version,
        "decision":card.decision().as_str()});
    match &card.context {
        DecisionContext::Reconcile {
            run_id,
            run_admitted_at_ms,
            step,
            call_key,
            op,
            attempts,
            unknown_reason,
        } => {
            body["run_id"] = json!(run_id);
            body["run_admitted_at_ms"] = json!(run_admitted_at_ms);
            if let Some(step) = step {
                body["step"] = json!(step);
            }
            body["call_key"] = json!(call_key);
            body["op"] = json!(op);
            body["attempts"] = json!(attempts);
            body["unknown_reason"] = json!(unknown_reason.as_str());
        }
        DecisionContext::Reenable {
            disabled_at_ms,
            disabled_reason,
            limit,
            window_ms,
            saturated_windows,
        } => {
            body["disabled_at_ms"] = json!(disabled_at_ms);
            body["disabled_reason"] = json!(disabled_reason.as_str());
            body["limit"] = json!(limit);
            body["window_ms"] = json!(window_ms);
            body["saturated_windows"] = json!(saturated_windows);
        }
    }
    body
}

/// Reads an answer record of a `flow_decision` card. Core answers an
/// expired card with state `expired` and the card's default (decline) id as
/// its choice; basal reads every expiry as no choice at all, whatever id it
/// names, so an expiry can never take an action.
pub fn decision_answer(record: &Value) -> Result<DecisionAnswer, ConsentError> {
    let bad = |why: &str| ConsentError::Unavailable(format!("decision answer: {why}"));
    let elicitation_id = record["elicitation_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| bad("no elicitation id"))?;
    let choice = match (
        record["state"].as_str(),
        record["answered_choice_id"].as_str(),
    ) {
        (Some("answered"), Some(choice)) if !choice.is_empty() => Some(choice.to_owned()),
        (Some("expired"), _) => None,
        _ => return Err(bad("unrecognised state")),
    };
    let body = &record["flow_decision"];
    let flow_id = body["flow_id"].as_str().ok_or_else(|| bad("no flow id"))?;
    let version = body["version"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| bad("version is invalid"))?;
    let decision = body["decision"]
        .as_str()
        .and_then(DecisionKind::parse)
        .ok_or_else(|| bad("decision is not reconcile or reenable"))?;
    let text = |name: &str| body[name].as_str().map(str::to_owned);
    let (run_id, call_key) = (text("run_id"), text("call_key"));
    if decision == DecisionKind::Reconcile && (run_id.is_none() || call_key.is_none()) {
        return Err(bad("a reconcile answer names no run or call"));
    }
    Ok(DecisionAnswer {
        elicitation_id: elicitation_id.to_owned(),
        choice,
        dedup_key: record["dedup_key"].as_str().map(str::to_owned),
        flow_id: flow_id.to_owned(),
        version,
        decision,
        run_id,
        call_key,
    })
}

/// The three author forms core accepts on a `flow_install` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorKind {
    /// `{"operator": true}`: the daemon-attested operator (callosum).
    Operator,
    /// `{"scope": "<scope_ref>"}`: an agent, named by the `scope_ref` the
    /// daemon stamped on the route it installed from. Core looks the scope up
    /// in its own records and takes the agent and its session from there.
    Scope,
    /// `{"local": true}`: a local caller the daemon cannot vouch for, shown
    /// by core as an unverified local caller.
    Local,
}

/// Which of core's author forms `author` is: exactly one key, with the value
/// core expects. Anything else is refused rather than sent, so a card never
/// reaches core with an author it would read differently.
pub fn author_kind(author: &Value) -> Result<AuthorKind, ConsentError> {
    let refused = || {
        ConsentError::Refused(format!(
            "the card's author {author} is not {{\"operator\": true}}, {{\"scope\": <scope_ref>}} or {{\"local\": true}}"
        ))
    };
    let object = author
        .as_object()
        .filter(|o| o.len() == 1)
        .ok_or_else(refused)?;
    match object.iter().next() {
        Some((key, Value::Bool(true))) if key == "operator" => Ok(AuthorKind::Operator),
        Some((key, Value::Bool(true))) if key == "local" => Ok(AuthorKind::Local),
        Some((key, Value::String(scope_ref))) if key == "scope" && !scope_ref.is_empty() => {
            Ok(AuthorKind::Scope)
        }
        _ => Err(refused()),
    }
}
struct State {
    transport: Arc<dyn Transport>,
    sink: Mutex<Option<Arc<dyn DecisionSink>>>,
}
pub struct CoreConsent {
    state: Arc<State>,
    polling: bool,
}
impl CoreConsent {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            state: Arc::new(State {
                transport,
                sink: Mutex::new(None),
            }),
            polling: false,
        }
    }
    pub fn with_polling(mut self) -> Self {
        self.polling = true;
        self
    }
    /// Also exposed for deterministic tests, which drive pages without sleeping.
    pub fn poll_once(&self) -> Result<(), ConsentError> {
        poll(&self.state)
    }
}
fn poll(state: &State) -> Result<(), ConsentError> {
    let sink = state
        .sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| ConsentError::Unavailable("decision sink is not attached".into()))?;
    // Start at zero because core omits acknowledged answers. An in-memory
    // cursor could skip an answer after a failed commit or process restart.
    let reply = state
        .transport
        .management(CORE, "elicitation.answers", json!({"since":0}))
        .map_err(error)?;
    let records = reply["records"]
        .as_array()
        .ok_or_else(|| ConsentError::Unavailable("answer page has no records".into()))?;
    let cursor = reply["cursor"]
        .as_i64()
        .filter(|c| *c >= 0)
        .ok_or_else(|| ConsentError::Unavailable("answer cursor is invalid".into()))?;
    for record in records {
        // Old expired answers omit the install payload. Fetch the owned full
        // record before deciding so the decision keeps its flow identity.
        let full;
        let record =
            if record.get("flow_install").is_none() && record.get("flow_decision").is_none() {
                let id = field(record, "elicitation_id")?;
                full = state
                    .transport
                    .management(
                        CORE,
                        "elicitation.await",
                        json!({"elicitation_id":id,"timeout_ms":0}),
                    )
                    .map_err(error)?;
                &full
            } else {
                record
            };
        if record.get("flow_decision").is_some() {
            // A decision card's answer. The page stays unacknowledged until
            // the sink has applied it, like an install decision.
            let answer = decision_answer(record)?;
            if let Err(e) = sink.answer(&answer) {
                return Err(ConsentError::Unavailable(format!(
                    "applying the answer to {}: {e}",
                    answer.elicitation_id
                )));
            }
            continue;
        }
        let decision = match (
            record["state"].as_str(),
            record["answered_choice_id"].as_str(),
        ) {
            (Some("answered"), Some("approve")) => CardDecision::Approve,
            (Some("answered"), Some("decline")) | (Some("expired"), _) => CardDecision::Reject,
            _ => {
                return Err(ConsentError::Unavailable(
                    "unrecognised card decision".into(),
                ));
            }
        };
        let f = &record["flow_install"];
        let flow = field(f, "flow_id")?;
        let version = f["version"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= u32::MAX as u64)
            .ok_or_else(|| ConsentError::Unavailable("decision version is invalid".into()))?;
        let hash = field(f, "code_hash")?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ConsentError::Unavailable("decision hash is invalid".into()));
        }
        // Card identity is derived from the approved bytes, not an in-memory
        // elicitation-id map that would be lost on process restart.
        sink.decide(&DecisionEvent {
            card_id: format!("card:{flow}:v{version}:{}", &hash[..16]),
            decision,
            decided_by: "core:elicitation".into(),
        })
        .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
    }
    let ack = state
        .transport
        .management(CORE, "elicitation.ack", json!({"cursor":cursor}))
        .map_err(error)?;
    if ack["ok"] != true {
        return Err(ConsentError::Unavailable(
            "core did not acknowledge the page".into(),
        ));
    }
    Ok(())
}
impl Consent for CoreConsent {
    fn raise(&self, card: &InstallCard) -> Result<(), ConsentError> {
        let reply = self
            .state
            .transport
            .management(CORE, "elicitation.request", request(card)?)
            .map_err(error)?;
        if !reply["elicitation_id"].is_string() {
            return Err(ConsentError::Unavailable("missing elicitation id".into()));
        }
        Ok(())
    }
    fn raise_decision(&self, card: &DecisionCard) -> Result<String, ConsentError> {
        let reply = self
            .state
            .transport
            .management(CORE, "elicitation.request", decision_request(card)?)
            .map_err(error)?;
        reply["elicitation_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ConsentError::Unavailable("missing elicitation id".into()))
    }
    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        *self.state.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        if self.polling {
            let weak = Arc::downgrade(&self.state);
            std::thread::spawn(move || {
                loop {
                    let Some(state) = weak.upgrade() else { break };
                    let _ = poll(&state);
                    drop(state);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            });
        }
    }
}
