//! Operator projections for the phone. Store snapshots contain no network calls;
//! registry decoration and reach filtering happen after the snapshot is released.

use super::*;
use basal_core::manifest::{Audience, Trigger};
use std::collections::BTreeMap;

struct Entry {
    value: Value,
    manifest: Option<Manifest>,
    raw_manifest: Value,
}

impl Module {
    pub(super) fn op_overview(&self, caller: &Caller, params: Value) -> OpResult {
        operator(caller, "flow.overview")?;
        #[derive(Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            agent_id: Option<String>,
            workspace_id: Option<String>,
            #[serde(default)]
            include_removed: bool,
        }
        let p: Params = decode(params)?;
        if p.agent_id.as_deref().is_some_and(|id| !stable_agent_id(id)) {
            return Err(invalid_params("agent_id must be a stable agent_ id"));
        }
        // Entorhinal owns live placement. Read it once, before opening even a
        // read snapshot, so a slow registry never holds the store connection.
        let membership = p
            .agent_id
            .as_deref()
            .map(|id| self.catalog.workspaces_of(id));
        let membership_unavailable = membership.as_ref().is_some_and(Result::is_err);
        let workspaces = membership.and_then(Result::ok).unwrap_or_default();
        let now = self.rt.config().clock.now_ms();
        let entries = self
            .rt
            .store()
            .read(|c| entries(c, None, p.include_removed))
            .map_err(|e| self.core_error(e))?;
        let mut names = BTreeMap::new();
        let mut flows = Vec::new();
        for mut entry in entries {
            let audience = entry.manifest.as_ref().and_then(|m| m.audience.as_ref());
            if let Some(workspace) = &p.workspace_id
                && !matches!(audience, Some(Audience::Workspace { id }) if id == workspace)
            {
                continue;
            }
            if let Some(agent) = &p.agent_id {
                let owned = entry.value["owner"].as_str() == Some(agent);
                let reached = match audience {
                    Some(Audience::Global {}) => true,
                    Some(Audience::Workspace { id }) => workspaces.contains(id),
                    Some(Audience::Agent { id }) => id == agent,
                    None => {
                        let targets: Vec<_> = entry
                            .manifest
                            .as_ref()
                            .map(|m| m.digest_sinks().map(|s| s.agent.as_str()).collect())
                            .unwrap_or_default();
                        if targets.contains(&agent.as_str()) {
                            true
                        } else if targets.is_empty() {
                            false
                        } else {
                            self.catalog
                                .resolve_agents(&targets)
                                .map_err(|e| agent_registry_unavailable(&e))?
                                .iter()
                                .flatten()
                                .any(|id| id == agent)
                        }
                    }
                };
                if !owned && !reached {
                    continue;
                }
            }
            self.decorate(&mut entry, &mut names);
            flows.push(entry.value);
        }
        let mut reply = json!({"as_of_ms":now,"flows":flows});
        if membership_unavailable {
            reply["workspace_membership"] = json!("unavailable");
        }
        Ok(reply)
    }

    pub(super) fn op_details(&self, caller: &Caller, params: Value) -> OpResult {
        operator(caller, "flow.details")?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            flow_id: String,
            runs: Option<u32>,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let limit = p.runs.unwrap_or(20);
        if !(1..=50).contains(&limit) {
            return Err(invalid_params("runs must be from 1 to 50"));
        }
        let result = self
            .rt
            .store()
            .read(|c| {
                let Some(mut entry) = entries(c, Some(&p.flow_id), true)?.pop() else {
                    return Ok(None);
                };
                entry.value["manifest"] = entry.raw_manifest.clone();
                entry.value["installs"] = json!(installs(c, &p.flow_id)?);
                entry.value["runs"] = json!(runs(c, &p.flow_id, limit)?);
                Ok(Some(entry))
            })
            .map_err(|e| self.core_error(e))?;
        let Some(mut entry) = result else {
            return Err(OpError::new("not_found", format!("no flow {}", p.flow_id)));
        };
        self.decorate(&mut entry, &mut BTreeMap::new());
        Ok(entry.value)
    }

    pub(super) fn op_uninstall(&self, caller: &Caller, params: Value) -> OpResult {
        operator(caller, "flow.uninstall")?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            flow_id: String,
        }
        let p: Params = serde_json::from_value(params).map_err(invalid_params)?;
        let changed = self
            .rt
            .uninstall_flow(&p.flow_id)
            .map_err(|e| self.install_error(e))?;
        Ok(json!({"flow_id":p.flow_id,"removed":true,"changed":changed}))
    }

    fn decorate(&self, entry: &mut Entry, names: &mut BTreeMap<String, Option<String>>) {
        if let Some(Audience::Workspace { id }) =
            entry.manifest.as_ref().and_then(|m| m.audience.as_ref())
        {
            let name = names
                .entry(id.clone())
                .or_insert_with(|| self.catalog.workspace_name(id).ok().flatten());
            entry.value["audience"]["name"] = json!(name);
        }
    }
}

fn operator(caller: &Caller, method: &str) -> Result<(), OpError> {
    if !matches!(caller, Caller::Operator) {
        return Err(OpError::not_permitted(method, caller));
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned + Default>(params: Value) -> Result<T, OpError> {
    if params.is_null() {
        Ok(T::default())
    } else {
        serde_json::from_value(params).map_err(invalid_params)
    }
}

fn stable_agent_id(id: &str) -> bool {
    id.strip_prefix("agent_").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn parse_json(text: &str) -> basal_core::Result<Value> {
    serde_json::from_str(text).map_err(|e| CoreError::Corrupt(e.to_string()))
}

fn entries(
    c: &rusqlite::Connection,
    flow_id: Option<&str>,
    include_removed: bool,
) -> basal_core::Result<Vec<Entry>> {
    let mut statement = c.prepare(
        "SELECT f.flow_id,f.approved_version,f.state,f.removed,f.owner,f.disabled_by,f.disabled_reason,
            COALESCE(i.manifest,p.manifest),i.author,COALESCE(i.version,f.approved_version),f.package,t.params,t.expires_at,
            EXISTS(SELECT 1 FROM runs WHERE flow_id=f.flow_id AND state='needs_reconcile'),
            (SELECT manifest FROM installs pending WHERE pending.flow_id=f.flow_id AND pending.version>COALESCE(f.approved_version,0) AND pending.state='validated'
             AND NOT EXISTS(SELECT 1 FROM install_cards WHERE flow_id=pending.flow_id AND version=pending.version AND state<>'pending') ORDER BY version DESC LIMIT 1)
         FROM flows f
         LEFT JOIN installs i ON i.flow_id=f.flow_id AND i.version=COALESCE(f.approved_version,(SELECT MAX(version) FROM installs WHERE flow_id=f.flow_id))
         LEFT JOIN package_versions p ON p.package=f.package AND p.version=f.approved_version
         LEFT JOIN template_instances t ON t.flow_id=f.flow_id
         WHERE (?1 IS NULL OR f.flow_id=?1) AND (?2 OR f.removed=0) ORDER BY f.flow_id")?;
    let rows = statement.query_map(rusqlite::params![flow_id, include_removed], |r| {
        let flow_id: String = r.get(0)?;
        let approved: Option<u32> = r.get(1)?;
        let state: String = r.get(2)?;
        let removed: bool = r.get(3)?;
        let author: Option<String> = r.get::<_, Option<String>>(8)?.or(r.get(4)?);
        let owner = author.filter(|a| a != "operator" && a != LOCAL_AUTHOR);
        let disabled_by: Option<String> = r.get(5)?;
        let disabled_reason: Option<String> = r.get(6)?;
        let reconcile: bool = r.get(13)?;
        let state = if removed { "removed" } else if approved.is_none() { "pending_approval" } else if state == "disabled" { "disabled" } else if reconcile { "needs_reconcile" } else { "enabled" };
        Ok((json!({
            "flow_id":flow_id,"owner":owner,"version":r.get::<_,Option<u32>>(9)?,"state":state,
            "disabled":disabled_by.map(|by|json!({"by":disabled_kind(&by),"reason":disabled_reason})),
            "audience":null,"last_run":null,
        }), r.get::<_,Option<String>>(7)?, r.get::<_,Option<String>>(10)?, r.get::<_,Option<String>>(11)?, r.get::<_,Option<i64>>(12)?,r.get::<_,Option<String>>(14)?,approved.is_some()))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut entries = Vec::new();
    for (mut value, text, package, params, expires_at, pending, approved) in rows {
        let flow_id = value["flow_id"].as_str().expect("flow id").to_owned();
        let raw_manifest = text
            .as_deref()
            .map(parse_json)
            .transpose()?
            .unwrap_or(Value::Null);
        let mut manifest = text
            .as_deref()
            .map(Manifest::parse_stored)
            .transpose()
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        if let Some(m) = &mut manifest {
            if let Some(params) = params {
                basal_core::templates::effective(m, &parse_json(&params)?);
                value["template"] = json!(package);
                value["expires_at_ms"] = json!(expires_at);
            }
            let pending = pending.as_deref().map(parse_json).transpose()?;
            value["title"] = json!(
                approved
                    .then_some(m.title.as_deref())
                    .flatten()
                    .or_else(|| pending.as_ref().and_then(|p| p["title"].as_str()))
                    .unwrap_or(&flow_id)
            );
            value["purpose"] = json!(m.purpose);
            value["trigger"] = json!(render_trigger(&m.trigger));
            value["audience"] = json!(m.audience);
            if matches!(m.audience, Some(Audience::Workspace { .. })) {
                value["audience"]["name"] = Value::Null;
            }
        } else {
            value["title"] = json!(flow_id);
            value["purpose"] = Value::Null;
            value["trigger"] = Value::Null;
        }
        value["last_run"] = c.query_row(
            "SELECT ended_at,state FROM runs WHERE flow_id=?1 AND ended_at IS NOT NULL AND state IN ('succeeded','failed','engine_mismatch','cancelled') ORDER BY ended_at DESC,admit_seq DESC,run_id DESC LIMIT 1",
            [&flow_id], |r| Ok(json!({"at_ms":r.get::<_,i64>(0)?,"outcome":r.get::<_,String>(1)?})),
        ).optional()?.unwrap_or(Value::Null);
        entries.push(Entry {
            value,
            manifest,
            raw_manifest,
        });
    }
    Ok(entries)
}

fn installs(c: &rusqlite::Connection, flow_id: &str) -> basal_core::Result<Vec<Value>> {
    let mut statement = c.prepare(
        "SELECT i.version,i.code_hash,i.author,i.installed_at,COALESCE(i.approval_kind,CASE WHEN c.card_id IS NOT NULL THEN 'card' ELSE 'unknown' END),i.approval_rule,
            i.state,i.approved_at,c.state,c.decided_at
         FROM installs i LEFT JOIN install_cards c ON c.card_id=(
            SELECT card_id FROM install_cards WHERE flow_id=i.flow_id AND version=i.version ORDER BY created_at DESC,card_id DESC LIMIT 1)
         WHERE i.flow_id=?1 ORDER BY i.version DESC")?;
    Ok(statement.query_map([flow_id], |r| {
        let stored_state: String = r.get(6)?;
        let card_state: Option<String> = r.get(8)?;
        let state = match stored_state.as_str() {
            "approved" | "superseded" => "approved",
            _ => card_state.as_deref().unwrap_or("pending"),
        };
        let author: String = r.get(2)?;
        let requested_by = if author == LOCAL_AUTHOR { "local" } else { &author };
        let hash: Vec<u8> = r.get(1)?;
        let hash: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let kind: String = r.get::<_,Option<String>>(4)?.unwrap_or_else(||"unknown".into());
        let mut approval = json!({"kind":kind});
        if let Some(rule) = r.get::<_,Option<String>>(5)? {
            approval["rule"] = json!(rule);
        }
        let mut value = json!({"version":r.get::<_,u32>(0)?,"code_hash":hash,"requested_by":requested_by,
            "requested_at_ms":r.get::<_,i64>(3)?,"approval":approval,"state":state});
        if let Some(at) = r.get::<_,Option<i64>>(9)?.or(r.get(7)?) {
            value["decided_at_ms"] = json!(at);
        }
        Ok(value)
    })?.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn runs(c: &rusqlite::Connection, flow_id: &str, limit: u32) -> basal_core::Result<Vec<Value>> {
    let mut statement = c.prepare(
        "SELECT run_id,trigger,admitted_at,ended_at,state,error_kind,error_detail FROM runs
         WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC LIMIT ?2",
    )?;
    let rows = statement
        .query_map(rusqlite::params![flow_id, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, trigger, at, end, outcome, kind, detail)| {
            let trigger = parse_json(&trigger)?;
            let label = if trigger["kind"] == "schedule" {
                "schedule".to_owned()
            } else if let Some(subject) = trigger["event_notice"]["subject"].as_str() {
                basal_core::events::parse_subject_identity(subject)?
                    .1
                    .to_owned()
            } else {
                "manual".to_owned()
            };
            Ok(
                json!({"run_id":id,"trigger":label,"admitted_at_ms":at,"ended_at_ms":end,
            "outcome":outcome,"error_kind":kind,"error":detail.map(|d|one_line(&d,200))}),
            )
        })
        .collect()
}

/// Keep registry text and failure details on one plain line, bounded by UTF-8
/// bytes rather than characters so a phone never receives half a code point.
fn one_line(text: &str, max_bytes: usize) -> String {
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if ch == '\u{1b}' && chars.peek() == Some(&']') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '\u{7}' {
                    break;
                }
                if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
        } else {
            plain.push(ch);
        }
    }
    let mut line = String::new();
    for word in plain
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|s| !s.is_empty())
    {
        if !line.is_empty() {
            if line.len() == max_bytes {
                break;
            }
            line.push(' ');
        }
        for ch in word.chars() {
            if line.len() + ch.len_utf8() > max_bytes {
                return line.trim_end().to_owned();
            }
            line.push(ch);
        }
    }
    line.trim_end().to_owned()
}

fn render_trigger(trigger: &Trigger) -> String {
    let mut parts = Vec::new();
    if let Some(schedule) = &trigger.schedule {
        if let Some(interval) = &schedule.interval {
            let (n, unit) = interval.split_at(interval.len().saturating_sub(1));
            let word = match unit {
                "s" => "second",
                "m" => "minute",
                "h" => "hour",
                "d" => "day",
                _ => "unit",
            };
            parts.push(format!(
                "every {n} {word}{}",
                if n == "1" { "" } else { "s" }
            ));
        }
        if let Some(cron) = &schedule.cron {
            parts.push(format!(
                "cron {cron}, {}",
                schedule.tz.as_deref().unwrap_or("UTC")
            ));
        }
        if let Some(at) = &schedule.at {
            let at = at
                .parse::<jiff::Timestamp>()
                .map(|t| {
                    t.to_zoned(jiff::tz::TimeZone::UTC)
                        .strftime("%Y-%m-%d %H:%M:%S UTC")
                        .to_string()
                })
                .unwrap_or_else(|_| at.clone());
            parts.push(format!("at {at}"));
        }
    }
    for event in trigger.events.iter().flatten() {
        let mut line = format!("on {} {}", event.module, event.name);
        if !event.header_match.is_empty() {
            line.push_str(" (");
            line.push_str(
                &event
                    .header_match
                    .iter()
                    .map(|(key, value)| format!("{key} {value}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            line.push(')');
        }
        parts.push(line);
    }
    one_line(&parts.join("; "), usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(value: Value) -> String {
        render_trigger(&serde_json::from_value(value).unwrap())
    }

    #[test]
    fn phone_trigger_renderer_covers_schedules_and_multiple_events() {
        for (spec, expected) in [
            (json!({"interval":"5m"}), "every 5 minutes"),
            (json!({"interval":"1h"}), "every 1 hour"),
            (json!({"interval":"90s"}), "every 90 seconds"),
            (json!({"interval":"2d"}), "every 2 days"),
            (
                json!({"cron":"*/5 * * * *","tz":"Europe/Madrid"}),
                "cron */5 * * * *, Europe/Madrid",
            ),
            (json!({"cron":"0 * * * *"}), "cron 0 * * * *, UTC"),
            (
                json!({"at":"2026-10-24T09:00:00Z"}),
                "at 2026-10-24 09:00:00 UTC",
            ),
        ] {
            assert_eq!(render(json!({"schedule":spec})), expected);
        }
        assert_eq!(
            render(json!({"events":[
                {"module":"plexus","name":"github_ci_job_failed","version":1,"match":{"repo":"cortexkit/*"}},
                {"module":"broca","name":"run.ended","version":1}
            ]})),
            "on plexus github_ci_job_failed (repo cortexkit/*); on broca run.ended"
        );
    }

    #[test]
    fn phone_failure_line_is_plain_bounded_utf8() {
        let line = one_line(&format!("broken\n\t\u{1b}[31m{}", "é".repeat(200)), 200);
        assert!(!line.chars().any(char::is_control));
        assert!(line.len() <= 200);
        assert!(line.starts_with("broken é"));
        assert_eq!(
            one_line(
                "a\u{1b}]8;;https://example.invalid\u{7}link\u{1b}]8;;\u{1b}\\z",
                200
            ),
            "alinkz"
        );
        assert_eq!(one_line(" \n hello\rworld\t", 200), "hello world");
    }
}
