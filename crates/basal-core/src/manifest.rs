//! The flow manifest: what a flow may do, decoded and checked on its own.
//!
//! A manifest is JSON. Decoding refuses unknown fields everywhere, so a
//! misspelt grant is an error rather than a silently missing permission,
//! and refuses duplicate keys. [`Manifest::parse`] then checks everything
//! that needs no outside knowledge (identifier shapes, limits, exactly one
//! trigger kind, no duplicate entries). Whether the events, ops and agents
//! it names exist is checked against the catalog at install
//! (`crate::install`). `docs/manifest.md` documents every field.

use serde::{Deserialize, Serialize};

use crate::schedule::ScheduleSpec;

/// The manifest format this build reads.
pub const MANIFEST_FORMAT: u32 = 1;
/// The largest manifest accepted, checked before it is parsed.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// The most entries in any list of a manifest.
pub const MAX_ENTRIES: usize = 64;
/// The longest flow id.
pub const MAX_FLOW_ID_BYTES: usize = 63;
/// The longest module, op, event, agent or source-kind name.
pub const MAX_NAME_BYTES: usize = basal_proto::MAX_NAME_BYTES;
/// The longest purpose.
pub const MAX_PURPOSE_BYTES: usize = 1024;
/// The largest token cap per window.
pub const MAX_TOKEN_CAP: u64 = 1_000_000_000;
/// The largest per-call output ceiling.
pub const MAX_OUTPUT_TOKENS: u32 = 200_000;
/// Bounds of the per-run wall-clock deadline.
pub const MIN_DEADLINE_MS: i64 = 1_000;
pub const MAX_DEADLINE_MS: i64 = 24 * 3_600_000;
/// Bounds of a token window.
pub const MIN_TOKEN_WINDOW_MS: i64 = 60_000;
pub const MAX_TOKEN_WINDOW_MS: i64 = 31 * 86_400_000;

fn format_v1() -> u32 {
    MANIFEST_FORMAT
}

fn one() -> u32 {
    1
}

/// A flow's manifest, as approved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The manifest format; only [`MANIFEST_FORMAT`] is read.
    #[serde(default = "format_v1")]
    pub format: u32,
    pub id: String,
    pub version: u32,
    pub purpose: String,
    pub trigger: Trigger,
    #[serde(default)]
    pub sinks: Vec<SinkGrant>,
    /// Agents whose status line the flow may set.
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default)]
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub ops: Vec<OpRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<FactsGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmGrant>,
    /// Roots the `fs` built-ins may read and write under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsGrant>,
    /// Repositories the git built-ins may read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitGrant>,
    /// Hosts `net.fetch` may reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    /// Runs of one flow run one at a time; only 1 is accepted.
    #[serde(default = "one")]
    pub concurrency: u32,
    /// The per-run wall-clock deadline, such as `"10m"`; the runtime's
    /// default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

/// What starts a run: module events or a schedule, never both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<EventRef>>,
    /// The schedule: cron in a named zone or an interval, and the missed
    /// policy. Decoded with unknown fields refused and compiled by
    /// [`crate::schedule::validate`] when the manifest is checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRef {
    pub module: String,
    pub name: String,
    pub version: u32,
}

/// A digest delivery action. Ordered from least to most intrusive, so the
/// cap comparison is an ordinary comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DigestAction {
    Silent,
    Piggyback,
    Wake,
}

impl DigestAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "silent" => Some(Self::Silent),
            "piggyback" => Some(Self::Piggyback),
            "wake" => Some(Self::Wake),
            _ => None,
        }
    }
}

/// An agent whose digest the flow may write, and the most intrusive action
/// it may ask for there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkGrant {
    pub agent: String,
    pub digest_max: DigestAction,
    /// Shown on the card as requested; only the operator grants it, through
    /// policy.
    #[serde(default)]
    pub break_through: bool,
}

/// A source kind the flow takes over from core for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub agent: String,
    pub source_kind: String,
}

/// A module op, as a structured pair: never a dotted string, so no check
/// depends on splitting a name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpRef {
    pub module: String,
    pub op: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsGrant {
    pub targets: Vec<String>,
    /// Whether `private-text` fields may be read.
    #[serde(default)]
    pub text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmGrant {
    /// Minimum intelligence demand passed to routing instead of a model name.
    pub iq: u32,
    #[serde(default)]
    pub eq: u32,
    pub token_cap: TokenCap,
    /// The ceiling on any one call's output tokens.
    pub max_output: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCap {
    pub tokens: u64,
    /// The window length, such as `"1d"`.
    pub window: String,
}

/// The `fs` line: absolute roots (a leading `~` is the user's home), each
/// granting `fs.read`, `fs.list` and `fs.stat` (`read`) or `fs.write`
/// (`write`) on itself and everything under it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsGrant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
}

/// The `git` line: repositories the git reads may run in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitGrant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
}

/// The `net` line: hosts `net.fetch` may reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetGrant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<FetchGrant>,
}

/// One host `net.fetch` may reach, and the methods it may use there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchGrant {
    pub host: String,
    /// `GET` and `HEAD` when absent; any other method must be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
}

impl FetchGrant {
    /// The methods approved for the host.
    pub fn effective_methods(&self) -> Vec<String> {
        match &self.methods {
            Some(m) => m.clone(),
            None => basal_host::builtins::net::DEFAULT_METHODS
                .iter()
                .map(|m| (*m).to_owned())
                .collect(),
        }
    }
}

/// The longest root path.
pub const MAX_ROOT_BYTES: usize = 4096;

/// A root as written in a manifest: absolute, or under `~`, with no `.` or
/// `..` component and no control character. Whether it exists is checked at
/// install.
fn check_root(field: &str, root: &str) -> Result<(), ManifestError> {
    if root.is_empty() || root.len() > MAX_ROOT_BYTES {
        return Err(invalid(
            field,
            format!("a root is 1 to {MAX_ROOT_BYTES} bytes"),
        ));
    }
    if root.chars().any(char::is_control) {
        return Err(invalid(field, format!("{root:?} has a control character")));
    }
    let rest = if root == "~" {
        ""
    } else if let Some(rest) = root.strip_prefix("~/") {
        rest
    } else if let Some(rest) = root.strip_prefix('/') {
        rest
    } else {
        return Err(invalid(
            field,
            format!("{root:?} is not absolute (start it with / or ~/)"),
        ));
    };
    if rest.split('/').any(|c| c == "." || c == "..") {
        return Err(invalid(field, format!("{root:?} has a . or .. component")));
    }
    Ok(())
}

/// Why a manifest was refused before any catalog lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    TooLarge {
        bytes: usize,
        cap: usize,
    },
    /// Not valid JSON for the manifest type: a syntax error, a missing
    /// field, a wrong type, an unknown field or a duplicate key.
    Decode(String),
    UnsupportedFormat(u32),
    /// Both trigger kinds, or neither.
    TriggerKinds {
        events: bool,
        schedule: bool,
    },
    Schedule(String),
    Invalid {
        field: String,
        reason: String,
    },
    Duplicate {
        field: String,
        value: String,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, cap } => {
                write!(f, "manifest of {bytes} bytes exceeds the cap of {cap}")
            }
            Self::Decode(e) => write!(f, "manifest does not decode: {e}"),
            Self::UnsupportedFormat(v) => write!(f, "manifest format {v} is not supported"),
            Self::TriggerKinds { events, schedule } => write!(
                f,
                "a trigger needs exactly one of events and schedule (events: {events}, schedule: {schedule})"
            ),
            Self::Schedule(e) => write!(f, "schedule: {e}"),
            Self::Invalid { field, reason } => write!(f, "{field}: {reason}"),
            Self::Duplicate { field, value } => write!(f, "{field}: {value} is listed twice"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Parses a duration such as `"90s"`, `"10m"`, `"6h"` or `"1d"` into
/// milliseconds. A positive whole number and one unit, nothing else.
pub fn duration_ms(text: &str) -> Option<i64> {
    let (unit_at, _) = text.char_indices().last()?;
    let (digits, unit) = text.split_at(unit_at);
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    let ms = n.checked_mul(scale)?;
    (ms > 0).then_some(ms)
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> ManifestError {
    ManifestError::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

fn check_name(
    field: &str,
    value: &str,
    max: usize,
    allowed: fn(u8) -> bool,
) -> Result<(), ManifestError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > max {
        return Err(invalid(field, format!("longer than {max} bytes")));
    }
    if !value.bytes().all(allowed) {
        return Err(invalid(
            field,
            format!("{value:?} has a character not allowed here"),
        ));
    }
    Ok(())
}

/// Flow ids and module ids: lower-case letters, digits, `-` and `_`.
fn id_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
}

/// Event names and source kinds are one NATS subject token each.
fn token_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

/// Op names may be dotted (`browser.read_page`); the pair, not the dots,
/// identifies the op.
fn op_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

fn agent_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn check_list<T: Ord + Clone + std::fmt::Debug>(
    field: &str,
    items: impl Iterator<Item = T>,
) -> Result<(), ManifestError> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        if seen.len() >= MAX_ENTRIES {
            return Err(invalid(field, format!("more than {MAX_ENTRIES} entries")));
        }
        if !seen.insert(item.clone()) {
            return Err(ManifestError::Duplicate {
                field: field.into(),
                value: format!("{item:?}"),
            });
        }
    }
    Ok(())
}

fn check_agent(field: &'static str, agent: &str) -> Result<(), ManifestError> {
    if agent == "$self" {
        return Ok(());
    }
    check_name(field, agent, MAX_NAME_BYTES, agent_byte)
}

impl Manifest {
    /// Decodes and checks a manifest's exact bytes.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest = Self::decode(text)?;
        manifest.check()?;
        Ok(manifest)
    }

    /// Decodes a manifest's exact bytes without checking it. Package
    /// registration decodes first so it can refuse an over-long package id
    /// with its own code before `check` applies the general flow-id limit.
    pub(crate) fn decode(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                bytes: text.len(),
                cap: MAX_MANIFEST_BYTES,
            });
        }
        serde_json::from_str(text).map_err(|e| ManifestError::Decode(e.to_string()))
    }

    /// Everything that can be checked without the catalog.
    pub fn check(&self) -> Result<(), ManifestError> {
        if self.format != MANIFEST_FORMAT {
            return Err(ManifestError::UnsupportedFormat(self.format));
        }
        check_name("id", &self.id, MAX_FLOW_ID_BYTES, id_byte)?;
        if self.version == 0 {
            return Err(invalid("version", "must be at least 1"));
        }
        if self.purpose.trim().is_empty() {
            return Err(invalid("purpose", "must not be empty"));
        }
        if self.purpose.len() > MAX_PURPOSE_BYTES {
            return Err(invalid(
                "purpose",
                format!("longer than {MAX_PURPOSE_BYTES} bytes"),
            ));
        }
        if self.purpose.chars().any(|c| c.is_control() && c != '\n') {
            return Err(invalid("purpose", "contains a control character"));
        }
        match (&self.trigger.events, &self.trigger.schedule) {
            (Some(events), None) => {
                if events.is_empty() {
                    return Err(invalid("trigger.events", "must name at least one event"));
                }
                for e in events {
                    check_name("trigger.events.module", &e.module, MAX_NAME_BYTES, id_byte)?;
                    check_name("trigger.events.name", &e.name, MAX_NAME_BYTES, token_byte)?;
                    if e.version == 0 {
                        return Err(invalid("trigger.events.version", "must be at least 1"));
                    }
                }
                check_list(
                    "trigger.events",
                    events
                        .iter()
                        .map(|e| (e.module.clone(), e.name.clone(), e.version)),
                )?;
            }
            (None, Some(schedule)) => {
                crate::schedule::validate(schedule)
                    .map_err(|e| ManifestError::Schedule(e.to_string()))?;
            }
            (events, schedule) => {
                return Err(ManifestError::TriggerKinds {
                    events: events.is_some(),
                    schedule: schedule.is_some(),
                });
            }
        }
        for s in &self.sinks {
            check_agent("sinks.agent", &s.agent)?;
        }
        check_list("sinks", self.sinks.iter().map(|s| s.agent.clone()))?;
        for a in &self.status {
            check_agent("status", a)?;
        }
        check_list("status", self.status.iter().cloned())?;
        for c in &self.claims {
            check_agent("claims.agent", &c.agent)?;
            check_name(
                "claims.source_kind",
                &c.source_kind,
                MAX_NAME_BYTES,
                token_byte,
            )?;
        }
        check_list(
            "claims",
            self.claims
                .iter()
                .map(|c| (c.agent.clone(), c.source_kind.clone())),
        )?;
        for o in &self.ops {
            check_name("ops.module", &o.module, MAX_NAME_BYTES, id_byte)?;
            check_name("ops.op", &o.op, MAX_NAME_BYTES, op_byte)?;
        }
        check_list("ops", self.ops.iter().cloned())?;
        if let Some(facts) = &self.facts {
            for a in &facts.targets {
                check_agent("facts.targets", a)?;
            }
            check_list("facts.targets", facts.targets.iter().cloned())?;
        }
        if let Some(llm) = &self.llm {
            if llm.iq > 100 {
                return Err(invalid("llm.iq", "must be between 0 and 100"));
            }
            if llm.eq > 100 {
                return Err(invalid("llm.eq", "must be between 0 and 100"));
            }
            let cap = llm.token_cap.tokens;
            if cap == 0 || cap > MAX_TOKEN_CAP {
                return Err(invalid(
                    "llm.token_cap.tokens",
                    format!("must be between 1 and {MAX_TOKEN_CAP}"),
                ));
            }
            match duration_ms(&llm.token_cap.window) {
                Some(ms) if (MIN_TOKEN_WINDOW_MS..=MAX_TOKEN_WINDOW_MS).contains(&ms) => {}
                _ => {
                    return Err(invalid(
                        "llm.token_cap.window",
                        "must be a duration from 1m to 31d, such as \"1d\"",
                    ));
                }
            }
            if llm.max_output == 0 || llm.max_output > MAX_OUTPUT_TOKENS {
                return Err(invalid(
                    "llm.max_output",
                    format!("must be between 1 and {MAX_OUTPUT_TOKENS}"),
                ));
            }
            if u64::from(llm.max_output) > cap {
                return Err(invalid("llm.max_output", "exceeds the token cap"));
            }
        }
        if let Some(fs) = &self.fs {
            for r in &fs.read {
                check_root("fs.read", r)?;
            }
            check_list("fs.read", fs.read.iter().cloned())?;
            for r in &fs.write {
                check_root("fs.write", r)?;
            }
            check_list("fs.write", fs.write.iter().cloned())?;
        }
        if let Some(git) = &self.git {
            for r in &git.read {
                check_root("git.read", r)?;
            }
            check_list("git.read", git.read.iter().cloned())?;
        }
        if let Some(net) = &self.net {
            for f in &net.fetch {
                if let Some(problem) = basal_host::builtins::net::host_problem(&f.host) {
                    return Err(invalid(
                        "net.fetch.host",
                        format!("{:?}: {problem}", f.host),
                    ));
                }
                if let Some(methods) = &f.methods {
                    if methods.is_empty() {
                        return Err(invalid(
                            "net.fetch.methods",
                            format!(
                                "{} lists no method; leave methods out for GET and HEAD",
                                f.host
                            ),
                        ));
                    }
                    for m in methods {
                        if !basal_host::builtins::net::METHODS.contains(&m.as_str()) {
                            return Err(invalid(
                                "net.fetch.methods",
                                format!(
                                    "{m:?} is not one of {}",
                                    basal_host::builtins::net::METHODS.join(", ")
                                ),
                            ));
                        }
                    }
                    check_list("net.fetch.methods", methods.iter().cloned())?;
                }
            }
            check_list("net.fetch", net.fetch.iter().map(|f| f.host.clone()))?;
        }
        if let Some(p) = &self.placement
            && (p.is_empty() || p.len() > MAX_NAME_BYTES || p.chars().any(char::is_control))
        {
            return Err(invalid(
                "placement",
                format!("must be 1 to {MAX_NAME_BYTES} bytes with no control characters"),
            ));
        }
        if self.concurrency != 1 {
            return Err(invalid(
                "concurrency",
                "only 1 is supported: runs of a flow are serialised",
            ));
        }
        if let Some(d) = &self.deadline {
            match duration_ms(d) {
                Some(ms) if (MIN_DEADLINE_MS..=MAX_DEADLINE_MS).contains(&ms) => {}
                _ => {
                    return Err(invalid(
                        "deadline",
                        "must be a duration from 1s to 24h, such as \"10m\"",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn lists_op(&self, module: &str, op: &str) -> bool {
        self.ops.iter().any(|o| o.module == module && o.op == op)
    }

    /// The most intrusive digest action allowed for `agent`, if any.
    pub fn digest_cap(&self, agent: &str) -> Option<DigestAction> {
        self.sinks
            .iter()
            .find(|s| s.agent == agent)
            .map(|s| s.digest_max)
    }

    /// The scope the manifest grants a built-in call, or `None` when the
    /// line it needs is absent or empty. Roots keep their manifest
    /// spelling; the host expands `~` and resolves them each time it acts.
    pub fn builtin_grant(
        &self,
        primitive: basal_proto::Primitive,
    ) -> Option<basal_host::builtins::Grant> {
        use basal_proto::Primitive as P;
        let roots = |r: &[String]| {
            (!r.is_empty()).then(|| basal_host::builtins::Grant {
                roots: r.to_vec(),
                hosts: Vec::new(),
            })
        };
        match primitive {
            P::FsRead | P::FsList | P::FsStat => roots(&self.fs.as_ref()?.read),
            P::FsWrite => roots(&self.fs.as_ref()?.write),
            P::GitLog | P::GitRevParse | P::GitDescribeTags | P::GitShow | P::GitDiff => {
                roots(&self.git.as_ref()?.read)
            }
            P::NetFetch => {
                let fetch = &self.net.as_ref()?.fetch;
                (!fetch.is_empty()).then(|| basal_host::builtins::Grant {
                    roots: Vec::new(),
                    hosts: fetch
                        .iter()
                        .map(|f| basal_host::builtins::HostRule {
                            host: f.host.clone(),
                            methods: f.effective_methods(),
                        })
                        .collect(),
                })
            }
            _ => None,
        }
    }

    /// Every root the manifest names, with the field naming it.
    pub fn roots(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        if let Some(fs) = &self.fs {
            out.extend(fs.read.iter().map(|r| ("fs.read", r.as_str())));
            out.extend(fs.write.iter().map(|r| ("fs.write", r.as_str())));
        }
        if let Some(git) = &self.git {
            out.extend(git.read.iter().map(|r| ("git.read", r.as_str())));
        }
        out
    }

    /// The token window in milliseconds, when the flow may call models.
    pub fn token_window_ms(&self) -> Option<i64> {
        self.llm
            .as_ref()
            .and_then(|l| duration_ms(&l.token_cap.window))
    }

    /// The per-run deadline, or `default_ms` when the manifest sets none.
    pub fn deadline_ms(&self, default_ms: i64) -> i64 {
        self.deadline
            .as_deref()
            .and_then(duration_ms)
            .unwrap_or(default_ms)
    }

    /// Every agent the manifest names, with the field naming it.
    pub fn agents(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        out.extend(self.sinks.iter().map(|s| ("sinks", s.agent.as_str())));
        out.extend(self.status.iter().map(|a| ("status", a.as_str())));
        out.extend(self.claims.iter().map(|c| ("claims", c.agent.as_str())));
        if let Some(f) = &self.facts {
            out.extend(f.targets.iter().map(|a| ("facts.targets", a.as_str())));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_durations_are_refused_without_panicking() {
        for text in ["é", "10é", "10中", "10🦀", "é1s", "١s", "1sé"] {
            // dry_run uses this same parser for its window.
            assert_eq!(duration_ms(text), None, "window {text:?}");
            for field in ["deadline", "token_window", "schedule_interval"] {
                let mut value = serde_json::json!({
                    "id":"unicode", "version":1, "purpose":"test",
                    "trigger":{"events":[{"module":"echo","name":"tick","version":1}]}
                });
                match field {
                    "deadline" => value["deadline"] = text.into(),
                    "token_window" => {
                        value["llm"] = serde_json::json!({
                            "iq":0,"eq":0,"max_output":1,
                            "token_cap":{"tokens":10,"window":text}
                        })
                    }
                    _ => value["trigger"] = serde_json::json!({"schedule":{"interval":text}}),
                }
                assert!(
                    Manifest::parse(&value.to_string()).is_err(),
                    "{field}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn durations_parse_only_whole_positive_numbers_with_one_unit() {
        assert_eq!(duration_ms("90s"), Some(90_000));
        assert_eq!(duration_ms("10m"), Some(600_000));
        assert_eq!(duration_ms("1d"), Some(86_400_000));
        for bad in [
            "",
            "d",
            "0m",
            "-1m",
            "1.5h",
            "1w",
            "1 d",
            "1dd",
            "9999999999d",
        ] {
            assert_eq!(duration_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn digest_actions_order_from_silent_to_wake() {
        assert!(DigestAction::Silent < DigestAction::Piggyback);
        assert!(DigestAction::Piggyback < DigestAction::Wake);
    }
}
