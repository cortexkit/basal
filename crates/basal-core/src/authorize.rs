//! Authorization of one call against the run's approved manifest, in the
//! parent, before anything is dispatched.
//!
//! The worker is not trusted with this: it only issues calls on its run's
//! channel, and the parent decides here whether the approved manifest
//! allows each one. A refused call is still journaled, as a rejection the
//! script can catch, so a replay sees the same refusal at the same
//! position.

use basal_host::{Catalog, HostOutcome};
use basal_proto::{CallKind, Primitive};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kv::rejection;
use crate::manifest::{DigestAction, Manifest, OpRef};

/// Why a call was refused, as the script sees it: `e.data.code` and
/// `e.data.message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: String,
    pub message: String,
    pub module: Option<String>,
}

impl Refusal {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            module: None,
        }
    }

    /// The manifest does not allow the call, or the flow profile never does.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(codes::DENIED, message)
    }

    pub fn outcome(&self) -> HostOutcome {
        let mut outcome = rejection(&self.code, &self.message);
        if self.code == "budget_exhausted" {
            let mut value: Value =
                serde_json::from_str(outcome.value.as_str()).expect("rejection JSON");
            value["budget"] = "model_cost_usd".into();
            outcome.value =
                basal_proto::JsonText::new(value.to_string()).expect("bounded rejection");
        }
        if let Some(module) = &self.module {
            let mut value: Value =
                serde_json::from_str(outcome.value.as_str()).expect("rejection JSON");
            value["module"] = Value::String(module.clone());
            outcome.value =
                basal_proto::JsonText::new(value.to_string()).expect("bounded rejection");
        }
        outcome
    }
}

/// The rejection codes basal journals for calls it refuses.
pub mod codes {
    pub use basal_host::builtins::codes::{DENIED, INVALID_ARGUMENTS, TOO_LARGE};
    /// The flow is disabled: it makes no new calls outside its VM.
    pub const FLOW_DISABLED: &str = "flow_disabled";
    /// The flow's dispatch budget for the current window is spent.
    pub const DISPATCH_BUDGET: &str = "dispatch_budget";
    /// The model call does not fit the flow's token cap for the window.
    pub const TOKEN_CAP: &str = "token_cap";
    /// A `kv` size limit.
    pub const KV_LIMIT: &str = "kv_limit";
    /// The refusal code when prefrontal-core's current catalog does not declare
    /// support for keyed digest writes.
    pub const DIGEST_REPLACE_KEY_UNSUPPORTED: &str = "digest_replace_key_unsupported";
    /// The refusal code when prefrontal-core cannot withdraw a pending digest item.
    pub const DIGEST_WITHDRAW_UNSUPPORTED: &str = "digest_withdraw_unsupported";
    /// The refusal code when a script names a manifest placeholder such as
    /// `$audience` as a call's recipient instead of a concrete agent.
    pub const SINK_TARGET_PLACEHOLDER: &str = "sink_target_placeholder";
    pub const ROOM_POST_UNSUPPORTED: &str = "room_post_unsupported";
    pub const ROOM_NOT_GRANTED: &str = "room_not_granted";
    pub const ROOM_MENTION_NOT_GRANTED: &str = "room_mention_not_granted";
    /// A room post the script built wrong: no room id, no text, text core
    /// would not accept, or mentions that are not a list of agent ids.
    /// Malformed mentions could name a member basal cannot check, which is
    /// why they are refused here rather than sent for core to judge.
    pub const ROOM_POST_INVALID: &str = "sink_item_invalid";
    pub const ROOM_ALL_MENTION_REFUSED: &str = "room_all_mention_refused";
}

/// The most members one post may mention.
pub const MAX_ROOM_MENTIONS: usize = 32;
/// The bytes one room post may carry, as core accepts them.
pub const MAX_ROOM_TEXT_BYTES: usize = 2000;
/// Told to the script when its text is not something core would accept.
pub const ROOM_TEXT_RULE: &str = "a room post is 1 to 2000 UTF-8 bytes without control characters other than newline";

/// Whether core would accept this as post text: at least one byte, at most
/// [`MAX_ROOM_TEXT_BYTES`], and only newline among the control characters.
pub fn valid_room_text(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_ROOM_TEXT_BYTES
        && !text
            .chars()
            .any(|c| c.is_control() && c != '\n')
}

/// Whether the text asks to mention every member with `@all`, which a flow
/// may never do. Core also refuses it, as `room_all_mention_refused`; basal
/// checks first so a live run never posts and a preview says the same.
///
/// The rule is core's room mention token rule, so both sides refuse the same
/// texts. `@` must start the text or follow a character that is neither
/// alphanumeric nor `_`. Then comes `all` in any ASCII case. It must end the
/// text or be followed by a character that is not alphanumeric, `_`, `-` or a
/// combining mark. So `hi @all.` matches; `@allison`, `ops@alltrails.com` and
/// `@all-ops` don't.
pub fn mentions_all_members(text: &str) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let combining = |c: char| {
        matches!(
            get_general_category(c),
            GeneralCategory::NonspacingMark
                | GeneralCategory::SpacingMark
                | GeneralCategory::EnclosingMark
        )
    };
    text.match_indices('@').any(|(at, _)| {
        let boundary_before = text[..at].chars().next_back().is_none_or(|c| !word(c));
        let rest = &text[at + 1..];
        boundary_before
            && rest.len() >= 3
            && rest.is_char_boundary(3)
            && rest[..3].eq_ignore_ascii_case("all")
            && rest[3..]
                .chars()
                .next()
                .is_none_or(|c| !(word(c) || c == '-' || combining(c)))
    })
}

/// Whether an id is a stable agent id: the agent name alphabet agent ids are
/// drawn from, so a display name with a space in it cannot pass as one.
pub fn valid_mention_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= basal_proto::MAX_NAME_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The module ops a flow may never call, even when its manifest lists
/// them: anything that runs commands. Matched without regard to ASCII case,
/// so a module or op name spelt differently cannot slip past.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellDenylist(pub Vec<OpRef>);

impl Default for ShellDenylist {
    /// AFT's `bash` tool (a shell) and basal's own `codemode`, which runs
    /// scripts with a shell.
    fn default() -> Self {
        Self(vec![
            OpRef {
                module: "aft".into(),
                op: "bash".into(),
                for_target: None,
            },
            OpRef {
                module: "basal".into(),
                op: "codemode".into(),
                for_target: None,
            },
        ])
    }
}

impl ShellDenylist {
    pub fn contains(&self, module: &str, op: &str) -> bool {
        self.0
            .iter()
            .any(|d| d.module.eq_ignore_ascii_case(module) && d.op.eq_ignore_ascii_case(op))
    }
}

/// Whether a flow may never call (module, op): on the denylist, marked
/// shell-capable in the catalog, or absent from it (basal never sends an
/// op it cannot classify).
pub fn shell_or_unknown(
    denylist: &ShellDenylist,
    catalog: &dyn Catalog,
    module: &str,
    op: &str,
) -> Option<Refusal> {
    if denylist.contains(module, op) {
        return Some(Refusal::denied(format!(
            "op ({module}, {op}) can run commands and is never available to flows"
        )));
    }
    match catalog.op(module, op) {
        Some(decl) if decl.shell_capable => Some(Refusal::denied(format!(
            "op ({module}, {op}) is marked shell-capable and is never available to flows"
        ))),
        Some(_) => None,
        None => Some(Refusal::denied(format!(
            "op ({module}, {op}) is not in the catalog"
        ))),
    }
}

fn agent_arg(args: &Value, primitive: Primitive) -> Result<&str, Refusal> {
    args.get("agent").and_then(Value::as_str).ok_or_else(|| {
        Refusal::new(
            codes::INVALID_ARGUMENTS,
            format!("{} needs an agent name", primitive.name()),
        )
    })
}

/// Refuses a sink, status, board or facts call whose recipient is a manifest
/// placeholder (any name starting with `$`, such as `$audience`).
///
/// Placeholders belong in the manifest's grants only; a call must name the
/// concrete agent it writes to. Without this check the call would be sent
/// and prefrontal-core would refuse it at delivery, which a capture dry run
/// never reaches, so the mistake would only show up as live run failures.
/// Run it after the driver has replaced `$self` with a package or template
/// owner's id, so only placeholders nothing resolved are refused.
pub fn placeholder_target(
    manifest: &Manifest,
    kind: &CallKind,
    args: &Value,
) -> Result<(), Refusal> {
    if !matches!(
        kind,
        CallKind::Primitive(
            Primitive::SinkDigest
                | Primitive::SinkStatus
                | Primitive::SinkBoard
                | Primitive::SinkWaitResolve
                | Primitive::SinkRoomPost
                | Primitive::Facts
        )
    ) {
        return Ok(());
    }
    let Some(target) = args
        .get("agent")
        .and_then(Value::as_str)
        .filter(|agent| agent.starts_with('$'))
    else {
        return Ok(());
    };
    let mut message =
        format!("\"{target}\" is a manifest placeholder; pass the recipient's agent id");
    if let Some(crate::manifest::Audience::Agent { id }) = &manifest.audience
        && !id.starts_with('$')
    {
        message.push_str(&format!(" ({id} is this flow's approved audience)"));
    }
    Err(Refusal::new(codes::SINK_TARGET_PLACEHOLDER, message))
}

/// Resolve grants against the admission-time owner, never a mutable agent name.
pub fn resolve_self(manifest: &mut Manifest, agent_id: &str) {
    let resolve = |agent: &mut String| {
        if agent == "$self" {
            *agent = agent_id.to_owned();
        }
    };
    for sink in &mut manifest.sinks {
        if let crate::manifest::SinkGrant::Digest(sink) = sink {
            resolve(&mut sink.agent);
        }
    }
    for grant in &mut manifest.board {
        resolve(&mut grant.agent);
    }
    if let Some(crate::manifest::Audience::Agent { id }) = &mut manifest.audience {
        resolve(id);
    }
    for agent in &mut manifest.status {
        resolve(agent);
    }
    for agent in &mut manifest.board_read {
        resolve(agent);
    }
    for claim in &mut manifest.claims {
        resolve(&mut claim.agent);
    }
    if let Some(facts) = &mut manifest.facts {
        for agent in &mut facts.targets {
            resolve(agent);
        }
    }
}

/// The scope the manifest grants a built-in call, after checking the call
/// against it: the arguments' shape, then the scope itself (paths resolved
/// under the roots, the repository approved, the URL's host and method
/// approved). Run in the parent before the call is journaled; the host
/// checks the returned scope again when it acts.
pub fn builtin_grant(
    manifest: &Manifest,
    primitive: Primitive,
    args: &Value,
) -> Result<basal_host::builtins::Grant, Refusal> {
    let Some(grant) = manifest.builtin_grant(primitive) else {
        return Err(Refusal::denied(format!(
            "the manifest grants no {}",
            primitive.name()
        )));
    };
    let call = basal_host::builtins::parse(primitive, args)
        .map_err(|d| Refusal::new(d.code, d.message))?;
    basal_host::builtins::authorize(&call, &grant).map_err(|d| Refusal::new(d.code, d.message))?;
    Ok(grant)
}

/// Checks a call against the manifest. `args` is the call's parsed
/// arguments. Model calls are only checked for a grant here; their token
/// reservation is made with the journal row.
pub fn check(
    manifest: &Manifest,
    denylist: &ShellDenylist,
    catalog: &dyn Catalog,
    kind: &CallKind,
    args: &Value,
) -> Result<(), Refusal> {
    match kind {
        CallKind::Tool { .. } => Err(Refusal::new(
            "profile_violation",
            "catalog tools are not available to flows",
        )),
        CallKind::Op { module, op } | CallKind::OpFor { module, op } => {
            if matches!(kind, CallKind::OpFor { .. }) && manifest.audience.is_none() {
                return Err(Refusal::new(
                    "for_requires_audience",
                    "target calls require an audience",
                ));
            }
            // The shell check comes first, so a shell-capable op is refused
            // the same way whether or not the manifest lists it.
            if let Some(refusal) = shell_or_unknown(denylist, catalog, module, op) {
                return Err(refusal);
            }
            if !manifest.lists_op(module, op) {
                return Err(Refusal::denied(format!(
                    "op ({module}, {op}) is not in the flow's manifest"
                )));
            }
            if matches!(kind, CallKind::OpFor { .. })
                && catalog
                    .op(module, op)
                    .is_none_or(|d| d.kind != Some(basal_host::catalog::OpKind::Query))
                && !manifest.ops.iter().any(|grant| {
                    grant.module == *module
                        && grant.op == *op
                        && grant.for_target == Some(crate::manifest::TargetAuthority::Delegate)
                })
            {
                let mut refusal = Refusal::new(
                    "for_target_requires_delegate",
                    "target mutations require this op's delegating grant",
                );
                refusal.module = Some(module.clone());
                return Err(refusal);
            }
            Ok(())
        }
        CallKind::Primitive(p) => match p {
            Primitive::Now
            | Primitive::Random
            | Primitive::KvGet
            | Primitive::KvSet
            | Primitive::KvDelete => Ok(()),
            Primitive::InstanceEnd => {
                if manifest.params.is_some() {
                    Ok(())
                } else {
                    Err(Refusal::new(
                        "not_a_template_instance",
                        "only template instances may end themselves",
                    ))
                }
            }
            Primitive::Sh => Err(Refusal::denied("sh is not available to flows")),
            Primitive::Sleep => Err(Refusal::new(
                "profile_violation",
                "sleep is not available to flows",
            )),
            Primitive::Facts => {
                let agent = agent_arg(args, *p)?;
                let Some(grant) = &manifest.facts else {
                    return Err(Refusal::denied("the manifest grants no facts"));
                };
                // Only validated global, public-facts grants may use the audience placeholder.
                let audience_facts = matches!(
                    manifest.audience,
                    Some(crate::manifest::Audience::Global {})
                ) && grant.targets == [crate::manifest::AUDIENCE_TARGET]
                    && !grant.text;
                if !audience_facts && !grant.targets.iter().any(|t| t == agent) {
                    return Err(Refusal::denied(format!(
                        "facts of {agent} are not in the manifest's targets"
                    )));
                }
                let include = args.get("options").and_then(|o| o.get("include"));
                if include.is_some_and(|v| !v.is_array()) {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "facts options.include must be an array",
                    ));
                }
                let wants_text = include
                    .and_then(Value::as_array)
                    .is_some_and(|inc| inc.iter().any(|v| v.as_str() == Some("text")));
                if wants_text && !grant.text {
                    return Err(Refusal::denied(
                        "the manifest does not grant private text in facts",
                    ));
                }
                Ok(())
            }
            Primitive::SinkDigest => {
                let agent = agent_arg(args, *p)?;
                let withdrawing = match args.get("withdraw") {
                    None => false,
                    Some(Value::Bool(true)) => true,
                    Some(_) => {
                        return Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            "withdraw must be true when supplied",
                        ));
                    }
                };
                let options = args.get("options");
                if let Some(options) = options {
                    let Some(options) = options.as_object() else {
                        return Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            "digest options must be an object",
                        ));
                    };
                    if let Some(option) = options.keys().find(|key| key.as_str() != "replace_key") {
                        return Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            format!("unknown digest option {option:?}"),
                        ));
                    }
                }
                if withdrawing {
                    if args.get("item").is_some() || args.get("action").is_some() {
                        return Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            "a digest withdrawal must not include an item or action",
                        ));
                    }
                    if options
                        .and_then(|options| options.get("replace_key"))
                        .is_none()
                    {
                        return Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            "a digest withdrawal needs options.replace_key",
                        ));
                    }
                }
                if let Some(replace_key) = options.and_then(|options| options.get("replace_key"))
                    && !replace_key.as_str().is_some_and(|key| {
                        !key.is_empty() && key.len() <= 200 && !key.chars().any(char::is_control)
                    })
                {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "options.replace_key must be a 1 to 200 byte string without control characters",
                    ));
                }
                if manifest.audience.is_some() {
                    return if manifest.digest_sinks().next().is_none() {
                        Err(Refusal::denied("the manifest grants no digest sink"))
                    } else {
                        Ok(())
                    };
                }
                let Some(cap) = manifest.digest_cap(agent) else {
                    return Err(Refusal::denied(format!(
                        "{agent} is not among the manifest's sinks"
                    )));
                };
                match args.get("action") {
                    None | Some(Value::Null) => Ok(()),
                    Some(Value::String(a)) => match DigestAction::parse(a) {
                        Some(action) if action <= cap => Ok(()),
                        Some(_) => Err(Refusal::denied(format!(
                            "action {a} exceeds the manifest's cap for {agent}"
                        ))),
                        None => Err(Refusal::new(
                            codes::INVALID_ARGUMENTS,
                            format!("unknown digest action {a:?}"),
                        )),
                    },
                    Some(_) => Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "a digest action is a string",
                    )),
                }
            }
            Primitive::SinkWaitResolve => {
                if !manifest.wait_sink() {
                    return Err(Refusal::new(
                        "sink_wait_not_granted",
                        "the manifest grants no wait sink",
                    ));
                }
                let valid = args.as_object().is_some_and(|object| {
                    object
                        .keys()
                        .all(|key| matches!(key.as_str(), "outcome" | "note"))
                }) && matches!(args["outcome"].as_str(), Some("met" | "failed"))
                    && args.get("note").is_none_or(|note| {
                        note.as_str().is_some_and(|text| {
                            text.len() <= 500 && !text.chars().any(char::is_control)
                        })
                    });
                if valid {
                    Ok(())
                } else {
                    Err(Refusal::new(
                        "sink_item_invalid",
                        "wait resolution needs met or failed and an optional note of at most 500 UTF-8 bytes without control characters",
                    ))
                }
            }
            Primitive::SinkRoomPost => {
                // The script supplies only these three; identity, receipt and
                // the approved stamp are added after this check, so a payload
                // carrying them is a call basal did not build.
                let valid = args.as_object().is_some_and(|object| {
                    object.keys().all(|key| {
                        matches!(key.as_str(), "room_id" | "text" | "mentions")
                    })
                });
                if !valid {
                    return Err(Refusal::new(
                        codes::ROOM_POST_INVALID,
                        "a room post carries only a room id, text and mentions",
                    ));
                }
                let Some(text) = args["text"].as_str() else {
                    return Err(Refusal::new(
                        codes::ROOM_POST_INVALID,
                        "a room post needs a room id and text",
                    ));
                };
                let Some(allowed) = args["room_id"]
                    .as_str()
                    .and_then(|room_id| manifest.room_mentions(room_id))
                else {
                    return Err(Refusal::new(
                        codes::ROOM_NOT_GRANTED,
                        "the manifest grants no room with that id",
                    ));
                };
                if !valid_room_text(text) {
                    return Err(Refusal::new(codes::ROOM_POST_INVALID, ROOM_TEXT_RULE));
                }
                // The room's members see the mention, so writing every member
                // is never a thing a flow may ask core for.
                if mentions_all_members(text) {
                    return Err(Refusal::new(
                        codes::ROOM_ALL_MENTION_REFUSED,
                        "a flow cannot mention every member of a room",
                    ));
                }
                let Some(mentions) = args.get("mentions") else {
                    return Err(Refusal::new(
                        codes::ROOM_POST_INVALID,
                        "mentions must be a list of agent ids",
                    ));
                };
                if mentions.is_null() {
                    return Ok(());
                }
                let listed = mentions
                    .as_array()
                    .filter(|listed| listed.len() <= MAX_ROOM_MENTIONS)
                    .filter(|listed| {
                        listed
                            .iter()
                            .all(|id| id.as_str().is_some_and(valid_mention_id))
                    });
                let Some(listed) = listed else {
                    return Err(Refusal::new(
                        codes::ROOM_POST_INVALID,
                        "mentions must be a list of agent ids",
                    ));
                };
                for id in listed {
                    if !allowed.iter().any(|approved| Some(approved.as_str()) == id.as_str()) {
                        return Err(Refusal::new(
                            codes::ROOM_MENTION_NOT_GRANTED,
                            format!(
                                "{id} is not an agent id the manifest approves for mentioning in that room"
                            ),
                        ));
                    }
                }
                Ok(())
            }
            Primitive::SinkBoard => {
                let agent = agent_arg(args, *p)?;
                let target = if manifest.audience.is_some() {
                    crate::manifest::AUDIENCE_TARGET
                } else {
                    agent
                };
                if manifest.board.iter().any(|g| g.agent == target) {
                    Ok(())
                } else {
                    Err(Refusal::new(
                        "sink_board_target_not_granted",
                        format!("{agent} is not among the manifest's board targets"),
                    ))
                }
            }
            Primitive::SinkStatus => {
                let agent = agent_arg(args, *p)?;
                if manifest.audience.is_some() {
                    return if manifest.status.is_empty() {
                        Err(Refusal::denied("the manifest grants no status sink"))
                    } else {
                        Ok(())
                    };
                }
                if manifest.status.iter().any(|a| a == agent) {
                    Ok(())
                } else {
                    Err(Refusal::denied(format!(
                        "{agent} is not among the manifest's status targets"
                    )))
                }
            }
            Primitive::Llm | Primitive::Classify => {
                if manifest.llm.is_some() {
                    Ok(())
                } else {
                    Err(Refusal::denied("the manifest grants no model calls"))
                }
            }
            Primitive::FsRead
            | Primitive::FsList
            | Primitive::FsStat
            | Primitive::FsWrite
            | Primitive::GitLog
            | Primitive::GitRevParse
            | Primitive::GitDescribeTags
            | Primitive::GitShow
            | Primitive::GitDiff
            | Primitive::NetFetch => builtin_grant(manifest, *p, args).map(|_| ()),
        },
    }
}

#[cfg(test)]
mod room_mention_tests {
    use super::mentions_all_members;

    /// prefrontal-core's own cases for its room mention token rule
    /// (`all_mention_helper_matches_room_parser_token_boundaries`), so basal
    /// refuses exactly the posts core refuses.
    #[test]
    fn all_mention_matches_cores_token_boundaries() {
        for text in ["@all", "hi @all.", "\n@ALL!", ".@all"] {
            assert!(mentions_all_members(text), "{text:?} should match");
        }
        for text in [
            "@allison",
            "ops@alltrails.com",
            "word@all",
            "_@all",
            "é@all",
            "@all-ops",
            "@all_ops",
            "@allé",
            "@all\u{301}",
        ] {
            assert!(!mentions_all_members(text), "{text:?} should not match");
        }
    }
}
