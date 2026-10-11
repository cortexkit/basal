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
    /// Not allowed by the approved manifest or by the flow profile.
    pub const DENIED: &str = "denied";
    /// Arguments that are not JSON or do not have the call's shape.
    pub const INVALID_ARGUMENTS: &str = "invalid_arguments";
    /// An argument over the byte cap, refused before it was parsed.
    pub const TOO_LARGE: &str = "too_large";
    /// The flow is disabled: it makes no new calls outside its VM.
    pub const FLOW_DISABLED: &str = "flow_disabled";
    /// The flow's dispatch budget for the current window is spent.
    pub const DISPATCH_BUDGET: &str = "dispatch_budget";
    /// The model call does not fit the flow's token cap for the window.
    pub const TOKEN_CAP: &str = "token_cap";
    /// A `kv` size limit.
    pub const KV_LIMIT: &str = "kv_limit";
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
            },
            OpRef {
                module: "basal".into(),
                op: "codemode".into(),
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

/// Resolve grants against the admission-time owner, never a mutable agent name.
pub fn resolve_self(manifest: &mut Manifest, agent_id: &str) {
    let resolve = |agent: &mut String| {
        if agent == "$self" {
            *agent = agent_id.to_owned();
        }
    };
    for sink in &mut manifest.sinks {
        resolve(&mut sink.agent);
    }
    if let Some(crate::manifest::Audience::Agent { id }) = &mut manifest.audience {
        resolve(id);
    }
    for agent in &mut manifest.status {
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
        CallKind::Op { module, op } => {
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
            Ok(())
        }
        CallKind::Primitive(p) => match p {
            Primitive::Now | Primitive::Random => Ok(()),
            Primitive::KvGet | Primitive::KvSet | Primitive::KvDelete => Ok(()),
            Primitive::Sh => Err(Refusal::denied("sh is not available to flows")),
            Primitive::Facts => {
                let agent = agent_arg(args, *p)?;
                let Some(grant) = &manifest.facts else {
                    return Err(Refusal::denied("the manifest grants no facts"));
                };
                if !grant.targets.iter().any(|t| t == agent) {
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
                if manifest.audience.is_some() {
                    return if manifest.sinks.is_empty() {
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
