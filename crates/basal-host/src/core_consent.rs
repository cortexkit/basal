//! Install cards go to prefrontal-core through `flow.install_request`. Decision
//! cards are held by `consent/v1` providers, which retain answers until basal
//! has durably applied every decision in a page.
use crate::subc_catalog::CORE;
use crate::transport::{Transport, WireError};
use crate::{
    Consent, ConsentError, DecisionAnswer, DecisionCard, DecisionContext, DecisionKind,
    DecisionPath, DecisionSink, InstallCard, InstallOutcome,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Core's refusal of a scope author whose scope it does not own, or that
/// carries no agent.
pub const SCOPE_UNKNOWN: &str = "flow_install_scope_unknown";
/// Core's refusal of a scope author whose scope is no longer live.
pub const SCOPE_ENDED: &str = "flow_install_scope_ended";

pub(crate) fn error(e: WireError) -> ConsentError {
    match e {
        WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. }
            if code == "secret_statement_mismatch" =>
        {
            ConsentError::Secret {
                code,
                retryable: false,
            }
        }
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
/// Install cards are owned by core and identified by flow, version and attempt.
/// Generic elicitation replies cannot confirm an install request.
pub fn decode_install_outcome(reply: &Value) -> Result<InstallOutcome, ConsentError> {
    match reply.get("outcome") {
        Some(Value::String(outcome)) if outcome == "card_raised" => {
            let flow_id = reply["flow_id"].as_str().filter(|id| !id.is_empty());
            let version = reply["version"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0);
            let attempt = reply["attempt"].as_i64().filter(|a| *a > 0);
            match (flow_id, version, attempt) {
                (Some(flow_id), Some(version), Some(attempt)) => Ok(InstallOutcome::CardRaised {
                    flow_id: flow_id.into(),
                    version,
                    attempt,
                }),
                _ => Err(ConsentError::Unavailable(
                    "install card is missing a valid flow, version or attempt".into(),
                )),
            }
        }
        Some(Value::String(outcome)) if outcome == "approved_by_policy" => {
            let rule = reply["rule"].as_str().filter(|rule| !rule.is_empty());
            let hash = reply["code_hash"]
                .as_str()
                .filter(|hash| crate::core_host::is_code_hash(hash));
            match (rule, hash) {
                (Some(rule), Some(code_hash)) => Ok(InstallOutcome::ApprovedByPolicy {
                    rule: rule.into(),
                    code_hash: code_hash.into(),
                }),
                _ => Err(ConsentError::Unavailable(
                    "policy approval is missing a valid rule or code hash".into(),
                )),
            }
        }
        _ => Err(ConsentError::Unavailable("unknown install outcome".into())),
    }
}

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str, ConsentError> {
    v.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ConsentError::Refused(format!("card is missing {name}")))
}
/// The `flow.install_request` for an install card, without the dry run's
/// plain-language text: see [`request_for`].
pub fn request(card: &InstallCard) -> Result<Value, ConsentError> {
    request_for(card, false)
}

/// The capability core lists in the daemon catalog when its install request
/// accepts `flow_install.dry_run_text`.
pub const FLOW_INSTALL_DRY_RUN_TEXT_V1: &str = "flow-install-dry-run-text/v1";

/// Core accepts a stable, basal-minted identity for each install request.
pub const FLOW_INSTALL_ID_V1: &str = "flow-install-id/v1";
pub const FLOW_INSTALL_SECRETS_V1: &str = "flow-install-secrets/v1";

/// The `flow.install_request` for an install card. With `dry_run_text`, and
/// when the card holds one, the body also carries the dry run in plain words
/// (`{summary, would, problems}`) beside the raw `dry_run_summary`; core
/// refuses keys it does not know, so this is set only when core lists
/// [`FLOW_INSTALL_DRY_RUN_TEXT_V1`]. Without it the request is exactly what
/// an older core expects.
pub fn request_for(card: &InstallCard, dry_run_text: bool) -> Result<Value, ConsentError> {
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
        "preview":{"label":"Code","text":script},"expires_in_ms":DECISION_EXPIRES_IN_MS,
        "flow_install":{"flow_id":card.flow_id,"version":card.version,"code_hash":hash,"script":script,"manifest_json":manifest,
            "author":author,"warnings":f["warnings"].as_array().map(|warnings| warnings.iter().map(|w| json!({"code":w["kind"],"detail":w["text"]})).collect::<Vec<_>>()).unwrap_or_default(),"dry_run_summary":f["dry_run_summary"],"token_usage":{"window":f["token_cap"]["window"].as_str().unwrap_or("1d"),"fresh_input":f["token_window"]["input_tokens"].as_u64().unwrap_or(0),"cache_write":f["token_window"]["cache_write_tokens"].as_u64().unwrap_or(0),"output":f["token_window"]["output_tokens"].as_u64().unwrap_or(0),"cache_read":f["token_window"]["cached_input_tokens"].as_u64().unwrap_or(0)}}});
    let mut result = result;
    if let (Some(per_run), Some(usd), Some(window)) = (
        f["model_cost_usd"]["per_run"].as_f64(),
        f["model_cost_usd"]["usd"].as_f64(),
        f["model_cost_usd"]["window"].as_str(),
    ) {
        result["facts"] = json!([{"label":"Classifier dollar budget",
            "value":format!("${per_run} per run; ${usd} per rolling {window}")}]);
    }
    if let Some(placement) = placement {
        result["flow_install"]["placement"] = json!(placement);
    }
    if dry_run_text && f["dry_run_text"].is_object() {
        result["flow_install"]["dry_run_text"] = f["dry_run_text"].clone();
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

// Local caps keep each phone fact row readable. Separately, the consent
// service limits the complete request envelope to 1,310,720 bytes.
const DECISION_FACTS_MAX_COUNT: usize = 4;
const DECISION_FACT_LABEL_MAX_BYTES: usize = 32;
const DECISION_FACT_VALUE_MAX_BYTES: usize = 200;
const CONSENT_REQUEST_ENVELOPE_MAX_BYTES: usize = 1_310_720;

/// Builds the consent request for a basal decision card shown to the operator.
/// prefrontal-core (core) validates the v2 request envelope and typed decision
/// body; its pinned examples define those core-owned fields. Basal derives the
/// phone's plain-text facts from that same body and may add durable
/// `dedup_key` or `args_digest` keys when a production record has them.
///
/// These cards are operator-only, so the request has no `author` or `subject`;
/// core also refuses `session_ref` and `parent_session_ref`, even when null.
/// `scope` names the flow for core's scope check when no `args_digest` exists.
/// The builder refuses invalid options, expiry, facts, or typed decision bodies
/// before sending them.
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
    if card.decision() == DecisionKind::GrantLost
        && (card.options.len() != 2
            || !card
                .options
                .iter()
                .any(|o| o.id == "check_now" && !o.decline)
            || !card
                .options
                .iter()
                .any(|o| o.id == "keep_disabled" && o.decline))
    {
        return refused("a grant-loss card offers check_now and keep_disabled");
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
    let flow_decision = flow_decision_body(card);
    let facts = decision_facts(&flow_decision);
    if let Some(problem) = facts_problem(&facts) {
        return refused(problem);
    }
    let mut request = json!({"kind":"flow_decision","title":card.title,"prompt":card.prompt,
        "options":options,"default":default,"urgency":"normal","on_expiry":"deny",
        "material_damage":false,"late_execution":"notify_only",
        "scope":{"flow_id":card.flow_id},"target":{"kind":"flow","label":card.flow_id},
        "expires_in_ms":card.expires_in_ms,"flow_decision":flow_decision});
    request["facts"] = json!(facts);
    if let Some(key) = &card.dedup_key {
        request["dedup_key"] = json!(key);
    }
    if let Some(digest) = &card.args_digest {
        request["args_digest"] = json!(digest);
    }
    let envelope = json!({"method":"consent.request","params":request});
    if serde_json::to_vec(&envelope)
        .map(|bytes| bytes.len() > CONSENT_REQUEST_ENVELOPE_MAX_BYTES)
        .unwrap_or(true)
    {
        return refused("a consent request fits within the 1,310,720-byte envelope limit");
    }
    Ok(envelope["params"].clone())
}

/// What core would refuse in a card's body, if anything: a blank flow id or
/// one over 256 bytes, a version of 0 for versioned decisions; on a reconcile card a blank or
/// overlong run id or call key, no attempts, or an op that is not two or
/// more dot-separated segments of lowercase letters, digits, `_` and `-`,
/// at most 128 bytes; on a re-enable card a zero limit, window or
/// saturated-window count; on a grant-loss card a blank provider or label,
/// or an empty or over-4-KiB opaque grant.
pub fn body_problem(card: &DecisionCard) -> Option<&'static str> {
    let id_ok = |id: &str| !id.trim().is_empty() && id.len() <= 256;
    if !id_ok(&card.flow_id) || (card.version == 0 && card.decision() != DecisionKind::GrantLost) {
        return Some("a card names a flow; versioned decisions need a version above 0");
    }
    match &card.context {
        DecisionContext::GrantLost {
            provider,
            grant,
            grant_label,
            ..
        } => {
            if provider.trim().is_empty()
                || provider.len() > 256
                || grant.is_empty()
                || grant.len() > 4096
                || grant_label.trim().is_empty()
            {
                Some("a grant-loss card names a provider, a grant within 4 KiB, and a label")
            } else {
                None
            }
        }
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
/// and `saturated_windows`; for grant loss `provider`, `grant`, `grant_label`
/// and `refused_at_ms`, without `version`. Core refuses any other key and any reason
/// outside its closed sets.
pub fn flow_decision_body(card: &DecisionCard) -> Value {
    let mut body = json!({"flow_id":card.flow_id,
        "decision":card.decision().as_str()});
    if card.decision() != DecisionKind::GrantLost {
        body["version"] = json!(card.version);
    }
    match &card.context {
        DecisionContext::GrantLost {
            provider,
            grant,
            grant_label,
            refused_at_ms,
        } => {
            body["provider"] = json!(provider);
            body["grant"] = json!(grant);
            body["grant_label"] = json!(grant_label);
            body["refused_at_ms"] = json!(refused_at_ms);
        }
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

/// Plain-text facts for display on the phone, derived only from the typed body.
fn decision_facts(body: &Value) -> Vec<Value> {
    let mut facts = Vec::with_capacity(DECISION_FACTS_MAX_COUNT);
    match body["decision"].as_str() {
        Some("reconcile") => {
            push_fact(
                &mut facts,
                "Run",
                format_fact_time(body["run_admitted_at_ms"].as_i64()),
            );
            push_fact(&mut facts, "Step", body["step"].as_str().map(str::to_owned));
            let call = body["op"]
                .as_str()
                .zip(body["attempts"].as_u64())
                .map(|(op, attempt)| format!("{op}, attempt {attempt}"));
            push_fact(&mut facts, "Call in doubt", call);
            let why = body["unknown_reason"]
                .as_str()
                .and_then(unknown_reason_fact)
                .map(str::to_owned);
            push_fact(&mut facts, "Why unknown", why);
        }
        Some("reenable") => {
            push_fact(
                &mut facts,
                "Disabled at",
                format_fact_time(body["disabled_at_ms"].as_i64()),
            );
            let why = body["disabled_reason"]
                .as_str()
                .zip(body["limit"].as_u64())
                .zip(body["window_ms"].as_u64())
                .zip(body["saturated_windows"].as_u64())
                .and_then(|(((reason, limit), window_ms), windows)| {
                    let (singular, plural) = match reason {
                        "dispatch_limit_saturated" => ("call", "calls"),
                        "run_limit_saturated" => ("run", "runs"),
                        _ => return None,
                    };
                    let limit_unit = counted_word(limit, singular, plural);
                    let window_unit = counted_word(windows, "window", "windows");
                    Some(format!(
                        "It hit its limit of {limit} {limit_unit} per {} window in {windows} {window_unit} in a row.",
                        format_window(window_ms)
                    ))
                });
            push_fact(&mut facts, "Why disabled", why);
        }
        Some("grant_lost") => {
            push_fact(
                &mut facts,
                "Provider",
                body["provider"].as_str().map(str::to_owned),
            );
            push_fact(
                &mut facts,
                "Grant",
                body["grant_label"].as_str().map(str::to_owned),
            );
            push_fact(
                &mut facts,
                "Refused at",
                format_fact_time(body["refused_at_ms"].as_i64()),
            );
        }
        _ => {}
    }
    facts
}

fn push_fact(facts: &mut Vec<Value>, label: &str, value: Option<String>) {
    if let Some(value) = fact_value(value.as_deref()) {
        facts.push(json!({"label":label,"value":value}));
    }
}

fn fact_value(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("none")
    {
        return None;
    }
    if value.len() <= DECISION_FACT_VALUE_MAX_BYTES {
        return Some(value.to_owned());
    }
    let mut end = DECISION_FACT_VALUE_MAX_BYTES - '…'.len_utf8();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &value[..end]))
}

fn facts_problem(facts: &[Value]) -> Option<&'static str> {
    if facts.len() > DECISION_FACTS_MAX_COUNT {
        return Some("a decision request has at most four phone facts");
    }
    for fact in facts {
        let Some(object) = fact.as_object() else {
            return Some("a phone fact is an object");
        };
        if object.len() != 2 {
            return Some("a phone fact has only label and value");
        }
        let (Some(label), Some(value)) = (fact["label"].as_str(), fact["value"].as_str()) else {
            return Some("a phone fact label and value are strings");
        };
        if label.trim().is_empty()
            || label.len() > DECISION_FACT_LABEL_MAX_BYTES
            || label.ends_with(':')
            || value.trim().is_empty()
            || value.eq_ignore_ascii_case("null")
            || value.eq_ignore_ascii_case("none")
            || value.len() > DECISION_FACT_VALUE_MAX_BYTES
        {
            return Some("a phone fact fits basal's display bounds");
        }
    }
    None
}

fn format_fact_time(ms: Option<i64>) -> Option<String> {
    let timestamp = jiff::Timestamp::from_millisecond(ms?).ok()?;
    Some(
        timestamp
            .to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%Y-%m-%d %H:%M UTC")
            .to_string(),
    )
}

fn format_window(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}-second", ms / 1000)
    } else {
        format!("{ms}-millisecond")
    }
}

fn counted_word(count: u64, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

fn unknown_reason_fact(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "connection_lost" => "The connection closed before a reply came.",
        "reply_timeout" => "No reply came before the call's deadline.",
        "reply_unreadable" => "The reply could not be read.",
        "basal_restarted" => "Basal restarted before the reply was saved.",
        "retries_exhausted" => "The retries ended without a clear answer.",
        "provider_lost_run" => "The provider lost track of the run.",
        _ => return None,
    })
}

/// Reads an answer record of a `flow_decision` card. The provider answers an
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
    let decision = body["decision"]
        .as_str()
        .and_then(DecisionKind::parse)
        .ok_or_else(|| bad("unknown decision kind"))?;
    let version = if decision == DecisionKind::GrantLost {
        0
    } else {
        body["version"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| bad("version is invalid"))?
    };
    let text = |name: &str| body[name].as_str().map(str::to_owned);
    let (run_id, call_key) = (text("run_id"), text("call_key"));
    if decision == DecisionKind::Reconcile && (run_id.is_none() || call_key.is_none()) {
        return Err(bad("a reconcile answer names no run or call"));
    }
    Ok(DecisionAnswer {
        grant_context: if decision == DecisionKind::GrantLost {
            Some(DecisionContext::GrantLost {
                provider: text("provider").ok_or_else(|| bad("no grant provider"))?,
                grant: text("grant").ok_or_else(|| bad("no grant reference"))?,
                grant_label: text("grant_label").ok_or_else(|| bad("no grant label"))?,
                refused_at_ms: body["refused_at_ms"]
                    .as_i64()
                    .ok_or_else(|| bad("no refusal time"))?,
            })
        } else {
            None
        },
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
    poll_started: AtomicBool,
    wake: Mutex<(u64, bool)>,
    cond: Condvar,
    /// Paths whose last poll failed. They are polled again even after their
    /// last card closed, until a page is fully applied and acknowledged.
    retries: Mutex<BTreeSet<DecisionPath>>,
    poll_lock: Mutex<()>,
}

const POLL_MIN: Duration = Duration::from_millis(250);
const POLL_MAX: Duration = Duration::from_secs(5);

fn next_poll_delay(delay: Duration, raised: bool) -> Duration {
    if raised {
        POLL_MIN
    } else {
        delay.saturating_mul(2).min(POLL_MAX)
    }
}

impl State {
    fn raised(&self) {
        let mut wake = self.wake.lock().unwrap_or_else(|p| p.into_inner());
        wake.0 = wake.0.wrapping_add(1);
        self.cond.notify_all();
    }
}

/// Core's install consent (`flow.install_request`) and the answer poller for
/// decision cards held by `consent/v1` providers. Core holds no decision
/// cards, so on its own this adapter raises none: [`crate::cingulate_consent`]
/// adds the provider selection.
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
                poll_started: AtomicBool::new(false),
                wake: Mutex::new((0, false)),
                cond: Condvar::new(),
                retries: Mutex::new(BTreeSet::new()),
                poll_lock: Mutex::new(()),
            }),
            polling: false,
        }
    }
    /// A decision card is about to be sent: start the poller if it is not
    /// running yet, and wake it.
    pub(crate) fn raised(&self) {
        self.start_polling();
        self.state.raised();
    }
    #[cfg(test)]
    pub(crate) fn raised_epoch(&self) -> u64 {
        self.state.wake.lock().unwrap().0
    }
    pub fn with_polling(mut self) -> Self {
        self.polling = true;
        self
    }
    /// Also exposed for deterministic tests, which drive pages without sleeping.
    #[cfg(any(test, feature = "test-support"))]
    pub fn poll_once(&self) -> Result<(), ConsentError> {
        poll(&self.state)
    }
}

fn attached_sink(state: &State) -> Result<Arc<dyn DecisionSink>, ConsentError> {
    state
        .sink
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| ConsentError::Unavailable("decision sink is not attached".into()))
}

/// Whether any provider has a page to read: a path with an open card, or one
/// whose last poll failed. With neither, the poller sends nothing.
fn poll_allowed(state: &State) -> Result<bool, ConsentError> {
    let sink = attached_sink(state)?;
    if !state
        .retries
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty()
    {
        return Ok(true);
    }
    for path in sink.decision_paths().map_err(sink_error)? {
        if sink.has_open_cards_on(&path).map_err(sink_error)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn poll(state: &State) -> Result<(), ConsentError> {
    let _guard = state.poll_lock.lock().unwrap_or_else(|p| p.into_inner());
    let sink = attached_sink(state)?;
    let mut paths: BTreeSet<_> = sink
        .decision_paths()
        .map_err(sink_error)?
        .into_iter()
        .collect();
    paths.extend(
        state
            .retries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned(),
    );
    let mut problem = None;
    for path in paths {
        let retry = state
            .retries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&path);
        if !retry && !sink.has_open_cards_on(&path).map_err(sink_error)? {
            continue;
        }
        // A failed apply or acknowledgement keeps the path polled even if
        // applying the page closed its last card. Only a full success clears
        // the debt.
        state
            .retries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(path.clone());
        let DecisionPath::Consent(provider) = &path;
        match poll_consent_pages(state, sink.as_ref(), &path, provider) {
            Ok(()) => {
                state
                    .retries
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&path);
            }
            Err(error) => {
                problem.get_or_insert(error);
            }
        }
    }
    problem.map_or(Ok(()), Err)
}

/// Applies one answer record from a provider's page. Keep poison records
/// unacknowledged: the caller still applies independent valid decisions later
/// in the page, and redelivery is safe because the sink commits by card
/// identity. Discarding a malformed record could lose consent.
fn apply_record(
    sink: &dyn DecisionSink,
    path: &DecisionPath,
    record: &Value,
) -> Result<(), ConsentError> {
    if record.get("flow_decision").is_none() {
        if record["state"] == "expired" && record["settled_at"].is_i64() {
            return sink
                .expire_on(path, field(record, "elicitation_id")?)
                .map_err(sink_error);
        }
        return Err(ConsentError::Unavailable(
            "consent answer has no decision body".into(),
        ));
    }
    // The page stays unacknowledged until the sink has applied the answer.
    let answer = decision_answer(record)?;
    // Consent requester v1 forbids consent.report_execution for
    // flow_decision cards: these settle policy questions, not execution work.
    if let Err(e) = sink.answer_on(path, &answer) {
        return Err(ConsentError::Unavailable(format!(
            "applying the answer to {}: {e}",
            answer.elicitation_id
        )));
    }
    Ok(())
}

fn sink_error(error: crate::SinkError) -> ConsentError {
    ConsentError::Unavailable(error.to_string())
}

fn ack_consent(state: &State, provider: &str, cursor: i64) -> Result<(), ConsentError> {
    let reply = state
        .transport
        .management(provider, "consent.ack", json!({"cursor":cursor}))
        .map_err(error)?;
    if reply["ok"] != true {
        return Err(ConsentError::Unavailable(
            "consent provider did not acknowledge the page".into(),
        ));
    }
    Ok(())
}

fn poll_consent_pages(
    state: &State,
    sink: &dyn DecisionSink,
    path: &DecisionPath,
    provider: &str,
) -> Result<(), ConsentError> {
    let mut since = sink.answer_cursor(path).map_err(sink_error)?;
    // The durable cursor covers only fully applied pages. Retrying its ack is
    // safe after a crash or a lost reply, including after the last card closed.
    if since > 0 {
        ack_consent(state, provider, since)?;
    }
    loop {
        let reply = state
            .transport
            .management(provider, "consent.answers", json!({"since":since}))
            .map_err(error)?;
        let records = reply["records"]
            .as_array()
            .ok_or_else(|| ConsentError::Unavailable("answer page has no records".into()))?;
        let cursor = reply["cursor"]
            .as_i64()
            .filter(|c| *c >= since)
            .ok_or_else(|| ConsentError::Unavailable("answer cursor is invalid".into()))?;
        let continuation = reply
            .get("continuation")
            .map(|c| {
                c.as_i64()
                    .filter(|c| *c == cursor && *c > since && !records.is_empty())
                    .ok_or_else(|| {
                        ConsentError::Unavailable("answer continuation is invalid".into())
                    })
            })
            .transpose()?;
        let mut problem = None;
        for record in records {
            if let Err(error) = apply_record(sink, path, record) {
                problem.get_or_insert(error);
            }
        }
        if let Some(error) = problem {
            return Err(error);
        }
        sink.save_answer_cursor(path, cursor).map_err(sink_error)?;
        // An empty final page can cover withdrawals and other owners' rows.
        // Only the provider's returned cursor is acknowledged, never a guess.
        ack_consent(state, provider, cursor)?;
        since = cursor;
        if continuation.is_none() {
            return Ok(());
        }
    }
}
impl Consent for CoreConsent {
    fn raise(&self, card: &InstallCard) -> Result<InstallOutcome, ConsentError> {
        let dry_run_text = crate::core_host::core_provides(
            self.state.transport.as_ref(),
            FLOW_INSTALL_DRY_RUN_TEXT_V1,
        );
        // Include the plain-language dry run only if the connected core says it
        // accepts one; an older core refuses the unknown key.
        let mut request = request_for(card, dry_run_text)?;
        if crate::core_host::core_provides(self.state.transport.as_ref(), FLOW_INSTALL_ID_V1) {
            request["flow_install"]["install_id"] = json!(field(&card.fields, "install_id")?);
        }
        let manifest: Value = serde_json::from_str(field(&card.fields["code"], "manifest")?)
            .map_err(|_| ConsentError::Refused("invalid manifest JSON".into()))?;
        if let Some(secrets) = manifest.get("secrets") {
            let secrets = secrets.as_array().ok_or_else(|| ConsentError::Secret {
                code: "invalid_params".into(),
                retryable: false,
            })?;
            if !crate::core_host::core_provides(
                self.state.transport.as_ref(),
                FLOW_INSTALL_SECRETS_V1,
            ) {
                return Err(ConsentError::Secret {
                    code: "install_secrets_unsupported".into(),
                    retryable: false,
                });
            }
            let install_id = field(&card.fields, "install_id")?;
            request["flow_install"]["install_id"] = json!(install_id);
            let flow = json!({"flow_id":card.flow_id,"version":card.version,
                "code_hash":field(&card.fields,"code_hash")?,"install_id":install_id});
            let mut statements = Vec::with_capacity(secrets.len());
            for secret in secrets {
                let mut params =
                    json!({"name":secret["name"],"flow":flow,"target":secret["target"]});
                if let Some(methods) = secret.get("methods") {
                    params["methods"] = methods.clone();
                }
                let reply = self
                    .state
                    .transport
                    .management("claustrum", "secret.request_flow_approval", params)
                    .map_err(|error| {
                        let code = match &error {
                            WireError::Refused { code, .. }
                            | WireError::RefusedDetails { code, .. } => code.as_str(),
                            _ => "secret_vault_unavailable",
                        };
                        ConsentError::Secret {
                            code: code.into(),
                            retryable: !matches!(
                                code,
                                "invalid_params"
                                    | "limit_exceeded"
                                    | "not_found"
                                    | "secret_setup_incomplete"
                            ),
                        }
                    })?;
                if reply["status"] != "approval_required"
                    || !reply["request_id"].is_string()
                    || !reply["statement_b64url"].is_string()
                    || !reply["expires_at_ms"].is_i64()
                {
                    return Err(ConsentError::Secret {
                        code: "secret_vault_unreadable".into(),
                        retryable: true,
                    });
                }
                statements.push(json!({"request_id":reply["request_id"],"statement_b64url":reply["statement_b64url"]}));
            }
            request["flow_install"]["secrets"] = json!(statements);
        }
        let reply = self
            .state
            .transport
            .management(CORE, "flow.install_request", request)
            .map_err(error)?;
        let outcome = decode_install_outcome(&reply)?;
        if let InstallOutcome::CardRaised {
            flow_id, version, ..
        } = &outcome
            && (flow_id != &card.flow_id || *version != card.version)
        {
            return Err(ConsentError::Unavailable(
                "install reply names another flow version".into(),
            ));
        }
        Ok(outcome)
    }
    fn attach(&self, sink: Arc<dyn DecisionSink>) {
        *self.state.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
        // With no open card nothing is started: no thread, no timer and no
        // page read. The first raised card starts the poller. A store error
        // starts it too, so the inventory is read again later.
        if !matches!(poll_allowed(&self.state), Ok(false)) {
            self.start_polling();
        }
        self.state.raised();
    }
}

impl CoreConsent {
    fn start_polling(&self) {
        // Without a sink there is no inventory to read; attaching one starts
        // the poller when it has cards.
        let attached = self
            .state
            .sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        if !self.polling || !attached || self.state.poll_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = self.state.clone();
        std::thread::spawn(move || {
            let mut delay = POLL_MIN;
            loop {
                let (seen, stopped) = *state.wake.lock().unwrap_or_else(|p| p.into_inner());
                if stopped {
                    break;
                }
                let allowed = poll_allowed(&state);
                if matches!(allowed, Ok(false)) {
                    // No timer and no page read while no card is open. A card
                    // raised between the inventory read and this wait changes
                    // the epoch.
                    let wake = state.wake.lock().unwrap_or_else(|p| p.into_inner());
                    drop(state.cond.wait_while(wake, |w| w.0 == seen && !w.1));
                    delay = POLL_MIN;
                    continue;
                }
                if let Err(error) = poll(&state) {
                    tracing::warn!(%error, "consent polling failed; retrying without acknowledging answers");
                }
                let wake = state.wake.lock().unwrap_or_else(|p| p.into_inner());
                let (wake, _) = state
                    .cond
                    .wait_timeout_while(wake, delay, |w| w.0 == seen && !w.1)
                    .unwrap_or_else(|p| p.into_inner());
                delay = next_poll_delay(delay, wake.0 != seen);
            }
        });
    }
}

impl Drop for CoreConsent {
    fn drop(&mut self) {
        self.state.wake.lock().unwrap_or_else(|p| p.into_inner()).1 = true;
        self.state.cond.notify_all();
    }
}

#[cfg(test)]
#[path = "core_consent_tests.rs"]
mod polling_tests;
