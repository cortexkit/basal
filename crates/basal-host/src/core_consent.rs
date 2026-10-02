//! Core retains answers until basal has durably applied every decision in a page.
use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};
use crate::{CardDecision, Consent, ConsentError, DecisionEvent, DecisionSink, InstallCard};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Core's refusal of a scope author whose scope it does not own, or that
/// carries no agent.
pub const SCOPE_UNKNOWN: &str = "flow_install_scope_unknown";
/// Core's refusal of a scope author whose scope is no longer live.
pub const SCOPE_ENDED: &str = "flow_install_scope_ended";

fn error(e: WireError) -> ConsentError {
    match e {
        WireError::Refused { code, message } if code == SCOPE_UNKNOWN || code == SCOPE_ENDED => {
            ConsentError::AuthorScope { code, message }
        }
        WireError::Refused { code, message } => ConsentError::Refused(format!("{code}: {message}")),
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
    // Core decodes `placement` as a required string and then renders the
    // card's placement from the manifest itself, so a manifest without one is
    // sent as an empty string rather than null, which core refuses.
    let placement = f["placement"].as_str().unwrap_or("");
    // No form carries `session_ref`: core finds an agent's session in the
    // scope the author names, and a scope author sent with one is refused.
    let result = json!({"kind":"flow_install","title":format!("Install flow {} v{}",card.flow_id,card.version),
        "prompt":field(f,"purpose")?,"options":[{"id":"approve","label":"Approve","effect":"grant"},{"id":"decline","label":"Decline","effect":"decline"}],
        "default":"decline","urgency":"normal","on_expiry":"deny","material_damage":false,"late_execution":"notify_only",
        "args_digest":hash,"dedup_key":format!("flow_install:{}:{}",card.flow_id,card.version),
        "target":{"kind":"flow","label":format!("{} v{}",card.flow_id,card.version)},"facts":[],
        "preview":{"label":"Code","text":script},"expires_in_ms":86400000,
        "flow_install":{"flow_id":card.flow_id,"version":card.version,"code_hash":hash,"script":script,"manifest_json":manifest,
            "author":author,"placement":placement,"warnings":f["warnings"].as_array().map(|warnings| warnings.iter().map(|w| json!({"code":w["kind"],"detail":w["text"]})).collect::<Vec<_>>()).unwrap_or_default(),"dry_run_summary":f["dry_run_summary"],"token_usage":{"window":f["token_cap"]["window"].as_str().unwrap_or("1d"),"fresh_input":f["token_window"]["input_tokens"].as_u64().unwrap_or(0),"cache_write":f["token_window"]["cache_write_tokens"].as_u64().unwrap_or(0),"output":f["token_window"]["output_tokens"].as_u64().unwrap_or(0),"cache_read":f["token_window"]["cached_input_tokens"].as_u64().unwrap_or(0)}}});
    Ok(result)
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
        let record = if record.get("flow_install").is_none() {
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
