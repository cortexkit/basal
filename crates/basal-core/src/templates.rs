//! Bounded template parameters and the effective authority of an instance.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::manifest::{Manifest, ManifestError, duration_ms};
use crate::schedule::ScheduleSpec;

// Activation input is parsed by JavaScript, so every accepted integer must
// remain exact when represented as a script number.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Parameter {
    Path { under: Vec<String> },
    Repo { under: Vec<String> },
    Url { hosts: Vec<String> },
    Duration { min: String, max: String },
    Integer { min: i64, max: i64 },
    String { max_bytes: usize },
    Enum { values: Vec<String> },
    Timestamp { max_ahead: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub max_per_agent: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_live: Option<u32>,
    pub max_lifetime: String,
}

fn bound(field: impl Into<String>, reason: impl Into<String>) -> ManifestError {
    ManifestError::TemplateBoundInvalid {
        field: field.into(),
        reason: reason.into(),
    }
}
fn placeholder(field: &str) -> ManifestError {
    ManifestError::TemplatePlaceholderInvalid {
        field: field.into(),
    }
}
pub(crate) fn is_placeholder(text: &str) -> bool {
    text.starts_with("$params.")
}

pub fn declarations(manifest: &Manifest) -> Result<BTreeMap<String, Parameter>, ManifestError> {
    manifest
        .params
        .as_ref()
        .into_iter()
        .flatten()
        .map(|(name, value)| {
            serde_json::from_value(value.clone())
                .map(|p| (name.clone(), p))
                .map_err(|e| bound(format!("params.{name}"), e.to_string()))
        })
        .collect()
}
pub fn policy(manifest: &Manifest) -> Result<Template, ManifestError> {
    serde_json::from_value(
        manifest
            .template
            .clone()
            .ok_or_else(|| bound("template", "required with params"))?,
    )
    .map_err(|e| bound("template", e.to_string()))
}

/// Checks bounds and all placeholder positions before ordinary manifest checks.
pub(crate) fn check(manifest: &Manifest) -> Result<(), ManifestError> {
    if manifest.params.is_none() && manifest.template.is_some() {
        return Err(bound("template", "requires params"));
    }
    let decls = declarations(manifest)?;
    if manifest.params.is_some() {
        let limits = policy(manifest)?;
        if !(1..=100).contains(&limits.max_per_agent) {
            return Err(bound("template.max_per_agent", "must be 1 to 100"));
        }
        if limits.max_live.is_some_and(|n| !(1..=10_000).contains(&n)) {
            return Err(bound("template.max_live", "must be 1 to 10000"));
        }
        if duration_ms(&limits.max_lifetime).is_none() {
            return Err(bound(
                "template.max_lifetime",
                "must be a positive duration",
            ));
        }
        if decls.len() > 16 {
            return Err(bound("params", "at most 16 parameters"));
        }
        for (name, decl) in &decls {
            let field = format!("params.{name}");
            if name.is_empty()
                || name.len() > 32
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(bound(
                    &field,
                    "names must be 1 to 32 bytes of a-z, 0-9 or _",
                ));
            }
            let valid = match decl {
                Parameter::Path { under } | Parameter::Repo { under } => {
                    !under.is_empty()
                        && under.len() <= 64
                        && under.iter().collect::<BTreeSet<_>>().len() == under.len()
                        && under
                            .iter()
                            .all(|root| crate::manifest::check_root(&field, root).is_ok())
                }
                Parameter::Url { hosts } => {
                    !hosts.is_empty()
                        && hosts.len() <= 64
                        && hosts.iter().collect::<BTreeSet<_>>().len() == hosts.len()
                        && hosts
                            .iter()
                            .all(|h| basal_host::builtins::net::host_problem(h).is_none())
                }
                Parameter::Duration { min, max } => match (duration_ms(min), duration_ms(max)) {
                    (Some(min), Some(max)) => {
                        min >= crate::schedule::MIN_INTERVAL_SECS as i64 * 1000 && min <= max
                    }
                    _ => false,
                },
                Parameter::Integer { min, max } => {
                    min <= max && *min >= -MAX_SAFE_INTEGER && *max <= MAX_SAFE_INTEGER
                }
                Parameter::String { max_bytes } => (1..=4096).contains(max_bytes),
                Parameter::Enum { values } => {
                    (1..=32).contains(&values.len())
                        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
                }
                Parameter::Timestamp { max_ahead } => duration_ms(max_ahead).is_some(),
            };
            if !valid {
                return Err(bound(field, "invalid bounds"));
            }
        }
    }
    fn visit(
        value: &Value,
        field: &str,
        decls: &BTreeMap<String, Parameter>,
    ) -> Result<(), ManifestError> {
        match value {
            Value::String(text) if text.contains("$params.") => {
                let name = text
                    .strip_prefix("$params.")
                    .ok_or_else(|| placeholder(field))?;
                let decl = decls.get(name).ok_or_else(|| placeholder(field))?;
                let fits = match decl {
                    Parameter::Path { .. } => {
                        field.starts_with("fs.read[") || field.starts_with("fs.write[")
                    }
                    Parameter::Repo { .. } => field.starts_with("git.read["),
                    Parameter::Url { .. } => {
                        field.starts_with("net.fetch[") && field.ends_with("].host")
                    }
                    Parameter::Duration { max, .. } => {
                        if field == "trigger.schedule.interval"
                            && duration_ms(max).unwrap()
                                > crate::schedule::MAX_INTERVAL_SECS as i64 * 1000
                        {
                            return Err(bound(
                                format!("params.{name}.max"),
                                "interval maximum exceeds 365d",
                            ));
                        }
                        field == "trigger.schedule.interval"
                    }
                    Parameter::Timestamp { .. } => field == "trigger.schedule.at",
                    Parameter::String { .. } | Parameter::Enum { .. } => {
                        field.starts_with("trigger.events[") && field.contains("].match.")
                    }
                    _ => false,
                };
                if !fits {
                    return Err(placeholder(field));
                }
            }
            Value::Object(object) => {
                for (name, value) in object {
                    let child = if field.is_empty() {
                        name.clone()
                    } else {
                        format!("{field}.{name}")
                    };
                    visit(value, &child, decls)?;
                }
            }
            Value::Array(items) => {
                for (index, value) in items.iter().enumerate() {
                    visit(value, &format!("{field}[{index}]"), decls)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(
        &serde_json::to_value(manifest).expect("manifest serializes"),
        "",
        &decls,
    )?;
    if let Some(spec) = &manifest.trigger.schedule
        && let Some(at) = &spec.at
    {
        if !is_placeholder(at) {
            return Err(placeholder("trigger.schedule.at"));
        }
        if spec.tz.is_some() {
            return Err(placeholder("trigger.schedule.tz"));
        }
        if spec.each_cap.is_some() {
            return Err(placeholder("trigger.schedule.each_cap"));
        }
        if spec.missed != crate::schedule::Missed::Once {
            return Err(placeholder("trigger.schedule.missed"));
        }
    }
    Ok(())
}

pub(crate) fn timestamp(text: &str) -> Option<jiff::Timestamp> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
        || ![0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|&index| bytes[index].is_ascii_digit())
    {
        return None;
    }
    let mut suffix = &text[19..];
    if let Some(fraction) = suffix.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=3).contains(&digits) {
            return None;
        }
        suffix = &fraction[digits..];
    }
    if !matches!(suffix, "Z" | "z" | "+00:00") {
        return None;
    }
    text.parse().ok()
}

fn canonical_timestamp(at: jiff::Timestamp) -> String {
    let base = at.strftime("%Y-%m-%dT%H:%M:%S");
    let millis = at.as_millisecond().rem_euclid(1000);
    if millis == 0 {
        format!("{base}Z")
    } else {
        format!("{base}.{millis:03}Z")
    }
}

/// Compile the timing shape with a duration parameter's declared minimum or a
/// fixed UTC timestamp sample. Only the copy changes; approval still hashes the
/// original manifest text with its placeholders.
pub(crate) fn validation_schedule(
    manifest: &Manifest,
    spec: &ScheduleSpec,
) -> Result<ScheduleSpec, ManifestError> {
    let mut spec = spec.clone();
    let decls = declarations(manifest)?;
    if let Some(interval) = &spec.interval
        && let Some(Parameter::Duration { min, .. }) =
            interval.strip_prefix("$params.").and_then(|n| decls.get(n))
    {
        spec.interval = Some(min.clone());
    }
    if spec.at.as_ref().is_some_and(|at| is_placeholder(at)) {
        spec.at = Some("2026-11-01T09:00:00Z".into());
    }
    Ok(spec)
}

/// Stable JSON for the validated scalar parameter values, ordered by UTF-8 keys.
pub fn canonical_params(values: &Value) -> String {
    let ordered: BTreeMap<_, _> = values
        .as_object()
        .expect("validated params are an object")
        .iter()
        .collect();
    serde_json::to_string(&ordered).expect("parameter values serialize")
}

pub fn instance_id(template: &str, agent: &str, values: &Value) -> String {
    let p = canonical_params(values);
    let mut h = blake3::Hasher::new();
    h.update(b"basal-template-instance-v1\0");
    for bytes in [template.as_bytes(), agent.as_bytes(), p.as_bytes()] {
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    format!("{template}_{}", &h.finalize().to_hex()[..16])
}

#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub detail: Value,
}

fn parameter_refusal(code: &'static str, name: &str, bound: Value) -> Refusal {
    Refusal {
        code,
        detail: json!({"param": name, "bound": bound}),
    }
}

/// Canonical path resolution shared with the host's per-access checks.
pub fn resolved_path(text: &str) -> Option<std::path::PathBuf> {
    basal_host::builtins::resolved_path(text)
}

fn is_repository(path: &std::path::Path) -> bool {
    let dot_git = path.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let Some(text) = repository_pointer(&dot_git) else {
            return false;
        };
        let Some(dir) = text.trim().strip_prefix("gitdir: ") else {
            return false;
        };
        path.join(dir)
    } else {
        path.to_owned()
    };
    let common = repository_pointer(&git_dir.join("commondir"))
        .map(|dir| git_dir.join(dir.trim()))
        .unwrap_or_else(|| git_dir.clone());
    git_dir.join("HEAD").is_file()
        && common.join("objects").is_dir()
        && common.join("refs").is_dir()
}

fn repository_pointer(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    // Git layout pointers contain paths, not arbitrary file bodies. Limit the
    // read itself so malformed markers cannot allocate unbounded creation input.
    const LIMIT: u64 = 1024 * 1024;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(LIMIT + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= LIMIT).then_some(text)
}

/// Checks the union of supplied and declared names in byte order, reporting only
/// its first failure. Returned paths, and therefore effective grants, are resolved.
pub fn validate_values(manifest: &Manifest, values: &Value, now: i64) -> Result<Value, Refusal> {
    let decls = declarations(manifest).expect("registered bounds were validated");
    let Some(object) = values.as_object() else {
        return Err(Refusal {
            code: "invalid_params",
            detail: json!({"field":"params"}),
        });
    };
    let names: BTreeSet<_> = decls.keys().chain(object.keys()).collect();
    let mut validated = serde_json::Map::new();
    for name in names {
        let Some(decl) = decls.get(name) else {
            return Err(parameter_refusal("param_unknown", name, Value::Null));
        };
        let bound = serde_json::to_value(decl).expect("bounds serialize");
        let Some(value) = object.get(name) else {
            return Err(parameter_refusal("param_missing", name, bound));
        };
        let mut canonical = value.clone();
        let fits = match decl {
            Parameter::Path { under } | Parameter::Repo { under } => {
                value.as_str().and_then(resolved_path).is_some_and(|real| {
                    let inside = under
                        .iter()
                        .filter_map(|root| basal_host::builtins::canonical_root(root))
                        .any(|root| real.starts_with(root));
                    if let Some(text) = real.to_str() {
                        canonical = Value::String(text.into());
                    } else {
                        return false;
                    }
                    inside && (!matches!(decl, Parameter::Repo { .. }) || is_repository(&real))
                })
            }
            Parameter::Url { hosts } => value
                .as_str()
                .and_then(|s| basal_host::builtins::net::parse_url(s).ok())
                .is_some_and(|url| {
                    hosts
                        .iter()
                        .any(|host| host.eq_ignore_ascii_case(&url.host))
                }),
            Parameter::Duration { min, max } => {
                value.as_str().and_then(duration_ms).is_some_and(|ms| {
                    ms >= duration_ms(min).unwrap() && ms <= duration_ms(max).unwrap()
                })
            }
            Parameter::Integer { min, max } => {
                value.as_i64().is_some_and(|n| n >= *min && n <= *max)
            }
            Parameter::String { max_bytes } => value
                .as_str()
                .is_some_and(|s| s.len() <= *max_bytes && !s.chars().any(char::is_control)),
            Parameter::Enum { values } => value
                .as_str()
                .is_some_and(|s| values.iter().any(|v| v == s)),
            Parameter::Timestamp { max_ahead } => {
                value.as_str().and_then(timestamp).is_some_and(|at| {
                    canonical = Value::String(canonical_timestamp(at));
                    at.as_millisecond() >= now
                        && at
                            .as_millisecond()
                            .checked_sub(now)
                            .is_some_and(|ahead| ahead <= duration_ms(max_ahead).unwrap())
                })
            }
        };
        let used_as_pattern = manifest.trigger.events.iter().flatten().any(|event| {
            event
                .header_match
                .values()
                .any(|pattern| pattern.strip_prefix("$params.") == Some(name.as_str()))
        });
        let fits = if used_as_pattern {
            fits && value
                .as_str()
                .is_some_and(crate::manifest::valid_header_pattern)
        } else {
            fits
        };
        if !fits {
            return Err(parameter_refusal("param_out_of_bounds", name, bound));
        }
        validated.insert(name.clone(), canonical);
    }
    Ok(Value::Object(validated))
}

/// Records path boundaries for validated values, shared by instances and previews.
pub fn grant_guards(
    manifest: &Manifest,
    values: &Value,
) -> Vec<basal_host::builtins::TemplateRoot> {
    let granted: BTreeSet<_> = manifest
        .roots()
        .into_iter()
        .filter_map(|(_, root)| root.strip_prefix("$params.").map(str::to_owned))
        .collect();
    declarations(manifest)
        .expect("validated declarations")
        .into_iter()
        .filter_map(|(name, decl)| {
            if !granted.contains(&name) {
                return None;
            }
            let under = match decl {
                Parameter::Path { under } | Parameter::Repo { under } => under,
                _ => return None,
            };
            let root = values[&name].as_str().expect("validated path").to_owned();
            let under = under
                .iter()
                .filter_map(|r| basal_host::builtins::canonical_root(r))
                .filter_map(|r| r.to_str().map(str::to_owned))
                .collect();
            Some(basal_host::builtins::TemplateRoot {
                param: name,
                existed: std::path::Path::new(&root).exists(),
                root,
                under,
            })
        })
        .collect()
}

/// Substitutes only validated grant, timing and header-filter slots, never arbitrary manifest
/// text. The stored manifest and its hash remain the exact approved bytes.
pub fn effective(manifest: &mut Manifest, values: &Value) {
    let replace = |slot: &mut String, host: bool| {
        if let Some(name) = slot.strip_prefix("$params.") {
            let value = values[name].as_str().expect("validated placeholder type");
            *slot = if host {
                basal_host::builtins::net::parse_url(value)
                    .expect("validated URL")
                    .host
            } else {
                value.into()
            };
        }
    };
    if let Some(fs) = &mut manifest.fs {
        for root in fs.read.iter_mut().chain(fs.write.iter_mut()) {
            replace(root, false);
        }
    }
    if let Some(git) = &mut manifest.git {
        for root in &mut git.read {
            replace(root, false);
        }
    }
    if let Some(net) = &mut manifest.net {
        for grant in &mut net.fetch {
            replace(&mut grant.host, true);
        }
    }
    if let Some(schedule) = &mut manifest.trigger.schedule {
        if let Some(interval) = &mut schedule.interval {
            replace(interval, false);
        }
        if let Some(at) = &mut schedule.at {
            replace(at, false);
        }
    }
    if let Some(events) = &mut manifest.trigger.events {
        for event in events {
            for pattern in event.header_match.values_mut() {
                replace(pattern, false);
            }
        }
    }
}

#[derive(Debug)]
pub enum TemplateError {
    Refused(Refusal),
    Store(crate::CoreError),
}
impl From<crate::CoreError> for TemplateError {
    fn from(error: crate::CoreError) -> Self {
        Self::Store(error)
    }
}
impl From<rusqlite::Error> for TemplateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
impl From<Refusal> for TemplateError {
    fn from(error: Refusal) -> Self {
        Self::Refused(error)
    }
}
fn refused(code: &'static str, detail: Value) -> TemplateError {
    TemplateError::Refused(Refusal { code, detail })
}

impl crate::Runtime {
    pub fn instantiate(
        &self,
        template: &str,
        version: u32,
        agent: &str,
        values: &Value,
        expires_in: &str,
    ) -> Result<Value, TemplateError> {
        use rusqlite::{OptionalExtension, params};
        let package = self
            .store()
            .read(|conn| crate::packages::get(conn, template, version))?
            .ok_or_else(|| refused("package_unknown", Value::Null))?;
        let manifest = Manifest::parse(&package.manifest)
            .map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
        if manifest.params.is_none() {
            return Err(refused("package_not_template", Value::Null));
        }
        let limits = policy(&manifest).map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
        if self.catalog().agent_id(agent).as_deref() != Some(agent) {
            return Err(refused("agent_unknown", Value::Null));
        }
        let now = self.config().clock.now_ms();
        let values = validate_values(&manifest, values, now)?;
        let lifetime = duration_ms(expires_in)
            .ok_or_else(|| refused("invalid_params", json!({"field":"expires_in"})))?;
        if lifetime > duration_ms(&limits.max_lifetime).unwrap() {
            return Err(refused("lifetime_too_long", Value::Null));
        }
        let expires_at = now
            .checked_add(lifetime)
            .ok_or_else(|| refused("invalid_params", json!({"field":"expires_in"})))?;
        let id = instance_id(template, agent, &values);
        let guards = grant_guards(&manifest, &values);
        let grant_guards = serde_json::to_string(&guards).expect("guards serialize");
        let mut effective_manifest = manifest;
        effective(&mut effective_manifest, &values);
        let stored = canonical_params(&values);
        let mut refusal = None;
        let reply = self.store().write(|tx| {
            let existing = tx.query_row("SELECT f.package,f.owner,f.approved_version,f.removed,t.params,t.expires_at FROM flows f LEFT JOIN template_instances t USING(flow_id) WHERE f.flow_id=?1", [&id], |r| Ok((r.get::<_,Option<String>>(0)?, r.get::<_,Option<String>>(1)?,r.get::<_,Option<u32>>(2)?,r.get::<_,bool>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<i64>>(5)?))).optional()?;
            if existing.as_ref().is_some_and(|(p,o,_,_,values,_)| p.as_deref()!=Some(template) || o.as_deref()!=Some(agent) || values.as_deref()!=Some(&stored)) {
                refusal = Some(refused("instance_id_conflict", Value::Null));
                return Ok(Value::Null);
            }
            let reopen = existing.as_ref().is_some_and(|(_,_,_,removed,_,expiry)| *removed || expiry.is_some_and(|at| at <= now));
            if existing.is_none() || reopen {
                let agent_live: u32 = tx.query_row("SELECT count(*) FROM template_instances t JOIN flows f USING(flow_id) WHERE f.package=?1 AND f.owner=?2 AND f.removed=0 AND t.expires_at>?3", params![template,agent,now], |r|r.get(0))?;
                let total_live: u32 = tx.query_row("SELECT count(*) FROM template_instances t JOIN flows f USING(flow_id) WHERE f.package=?1 AND f.removed=0 AND t.expires_at>?2", params![template,now], |r|r.get(0))?;
                let limit = if agent_live >= limits.max_per_agent { Some((limits.max_per_agent,"agent")) }
                    else { limits.max_live.filter(|limit| total_live >= *limit).map(|limit| (limit,"template")) };
                if let Some((limit,scope)) = limit {
                    refusal = Some(refused("instance_limit_reached", json!({"limit":limit,"scope":scope})));
                    return Ok(Value::Null);
                }
            }
            let expiry = existing.as_ref().and_then(|e|e.5).unwrap_or(expires_at).max(expires_at);
            // Record a passed deadline before a renewal can replace it, even if
            // the scheduler has not yet had a chance to observe the expiry.
            expire_schedules(tx, now)?;
            tx.execute("INSERT INTO flows (flow_id,owner,approved_version,created_at,package,first_approved_at) VALUES (?1,?2,?3,?4,?5,?4) ON CONFLICT(flow_id) DO UPDATE SET approved_version=excluded.approved_version,removed=0,first_approved_at=COALESCE(flows.first_approved_at,excluded.first_approved_at)", params![id,agent,version,now,template])?;
            tx.execute("INSERT INTO template_instances(flow_id,params,expires_at,grant_guards) VALUES (?1,?2,?3,?5) ON CONFLICT(flow_id) DO UPDATE SET expires_at=excluded.expires_at,grant_guards=excluded.grant_guards,incarnation=template_instances.incarnation+?4", params![id,stored,expiry,i64::from(reopen),grant_guards])?;
            tx.execute("UPDATE template_instances SET expiry_notified=0 WHERE flow_id=?1 AND expires_at>?2", params![id,now])?;
            let event = if existing.is_none() || existing.as_ref().is_some_and(|e|e.3) {
                Some("flow_instance_created")
            } else if existing.as_ref().is_some_and(|e|e.2 != Some(version) || e.5 != Some(expiry)) {
                Some("flow_instance_extended")
            } else { None };
            if let Some(event) = event { crate::template_events::enqueue(tx, &id, event, now)?; }
            let time = jiff::Timestamp::from_millisecond(now).map_err(|e|crate::CoreError::Invalid(e.to_string()))?;
            if existing.as_ref().and_then(|e|e.2) != Some(version) {
                if let Some(spec) = effective_manifest.trigger.schedule.clone() {
                    crate::schedule::table::approve_instance(tx, &crate::schedule::ScheduledFlow { flow_id:id.clone(),version:version.into(),spec }, time, &self.config().schedule)?;
                } else { crate::schedule::table::remove(tx,&id)?; }
            }
            if crate::install::is_disabled(tx,&id)? { crate::schedule::table::disable(tx,&id,time)?; }
            else if reopen { crate::schedule::table::enable(tx,&id,time)?; }
            Ok(json!({"flow_id":id,"template":template,"version":version,"agent_id":agent,"params":values,"expires_at":expiry,"new":existing.is_none()}))
        })?;
        if let Some(error) = refusal {
            return Err(error);
        }
        self.shared.signal.bump();
        Ok(reply)
    }

    pub fn end_instance(&self, flow_id: &str) -> Result<Value, TemplateError> {
        let now = self.config().clock.now_ms();
        let result = self.store().write(|tx| {
            if !is_instance(tx, flow_id)? {
                return Ok(None);
            }
            let ended = end(tx, flow_id, now)?;
            Ok(Some(json!({"flow_id":flow_id,"ended":ended})))
        })?;
        self.shared.signal.bump();
        result.ok_or_else(|| refused("package_not_template", Value::Null))
    }
}

pub fn is_instance(conn: &rusqlite::Connection, flow_id: &str) -> crate::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM template_instances WHERE flow_id=?1)",
        [flow_id],
        |r| r.get(0),
    )?)
}

pub(crate) fn expired(conn: &rusqlite::Connection, flow_id: &str, now: i64) -> crate::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM template_instances WHERE flow_id=?1 AND expires_at<=?2)",
        rusqlite::params![flow_id, now],
        |r| r.get(0),
    )?)
}

pub(crate) fn end(tx: &rusqlite::Transaction<'_>, flow_id: &str, now: i64) -> crate::Result<bool> {
    expire_schedules(tx, now)?;
    let changed = tx.execute("UPDATE flows SET removed=1 WHERE flow_id=?1 AND removed=0 AND EXISTS(SELECT 1 FROM template_instances WHERE flow_id=?1)", [flow_id])?;
    if changed > 0 {
        crate::template_events::enqueue(tx, flow_id, "flow_instance_ended", now)?;
        let now = jiff::Timestamp::from_millisecond(now)
            .map_err(|e| crate::CoreError::Invalid(e.to_string()))?;
        crate::schedule::table::disable(tx, flow_id, now)?;
    }
    Ok(changed > 0)
}

/// All terminal run transitions share this hook, so failure, cancellation and
/// successful completion end one-shots and journaled self-end requests alike.
pub(crate) fn finish_run(tx: &rusqlite::Transaction<'_>, run_id: &str) -> crate::Result<()> {
    use rusqlite::OptionalExtension;
    let flow = tx.query_row("SELECT r.flow_id,r.ended_at FROM runs r JOIN template_instances t USING(flow_id) WHERE r.run_id=?1 AND r.end_instance=1 AND r.ended_at IS NOT NULL AND r.template_incarnation=t.incarnation", [run_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).optional()?;
    if let Some((flow, at)) = flow {
        end(tx, &flow, at)?;
    }
    Ok(())
}

pub(crate) fn expire_schedules(tx: &rusqlite::Transaction<'_>, now: i64) -> crate::Result<()> {
    crate::template_events::prune(tx, now)?;
    let ids = {
        let mut stmt = tx.prepare("SELECT t.flow_id FROM template_instances t JOIN flows f USING(flow_id) WHERE t.expires_at<=?1 AND t.expiry_notified=0 AND f.removed=0 ORDER BY t.flow_id")?;
        stmt.query_map([now], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in ids {
        crate::template_events::enqueue(tx, &id, "flow_instance_expired", now)?;
        tx.execute(
            "UPDATE template_instances SET expiry_notified=1 WHERE flow_id=?1",
            [&id],
        )?;
    }
    tx.execute("UPDATE schedules SET state='disabled',updated_at=?1 WHERE flow_id IN (SELECT flow_id FROM template_instances WHERE expires_at<=?1)", [now])?;
    Ok(())
}

/// This local effect and its journal outcome commit together. Ending is deferred
/// until the requesting run reaches a terminal state, including after replay.
pub(crate) fn request_end(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    args: &basal_proto::JsonText,
) -> crate::Result<basal_host::HostOutcome> {
    let value: Value = serde_json::from_str(args.as_str())
        .map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
    if !value.as_object().is_some_and(|object| object.is_empty()) {
        return Ok(crate::kv::rejection(
            "invalid_arguments",
            "instance.end takes no arguments",
        ));
    }
    let changed = tx.execute(
        "UPDATE runs SET end_instance=1 WHERE run_id=?1 AND template_incarnation IS NOT NULL",
        [run_id],
    )?;
    if changed == 0 {
        return Ok(crate::kv::rejection(
            "not_a_template_instance",
            "only template instances may end themselves",
        ));
    }
    Ok(basal_host::HostOutcome::fulfilled(
        basal_proto::JsonText::new("true").expect("small JSON"),
    ))
}

struct UniqueJson(Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with no duplicate keys")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<UniqueJson, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<UniqueJson, E> {
                Ok(UniqueJson(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<UniqueJson, E> {
                Ok(UniqueJson(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<UniqueJson, E> {
                Ok(UniqueJson(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<UniqueJson, E> {
                Ok(UniqueJson(json!(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<UniqueJson, E> {
                Ok(UniqueJson(json!(v)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<UniqueJson, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<UniqueJson, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((name, UniqueJson(value))) =
                    map.next_entry::<String, UniqueJson>()?
                {
                    if values.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!("duplicate key {name}")));
                    }
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

pub(crate) fn decode_header_match<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let UniqueJson(value) = UniqueJson::deserialize(deserializer)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

pub(crate) fn decode_params<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, Value>>, D::Error> {
    let UniqueJson(value) = UniqueJson::deserialize(deserializer)?;
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}
pub(crate) fn decode_template<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    let UniqueJson(value) = UniqueJson::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(serde::de::Error::custom("template must be an object"));
    }
    Ok(Some(value))
}

/// Bind each grant slot to its own recorded parameter boundary before replacing
/// placeholders. A script-only parameter cannot constrain a static grant.
pub(crate) fn effective_instance(
    manifest: &mut Manifest,
    values: &Value,
    guards: &[basal_host::builtins::TemplateRoot],
) {
    for primitive in basal_proto::Primitive::ALL {
        if let Some(grant) = manifest.builtin_grant(primitive) {
            let selected = guards
                .iter()
                .filter(|guard| {
                    grant
                        .roots
                        .iter()
                        .any(|root| root.strip_prefix("$params.") == Some(guard.param.as_str()))
                })
                .cloned()
                .collect();
            manifest.template_grants.insert(primitive.code(), selected);
        }
    }
    effective(manifest, values);
}
