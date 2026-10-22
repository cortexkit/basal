//! Durable module-event fan-out and a bounded, oldest-first queue per flow.
use std::collections::BTreeMap;

use basal_proto::JsonText;
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{Admission, CoreError, Manifest, Result, Runtime, admission, install};

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;

/// Two minutes of waiting work at the default limit of 60 runs per minute.
/// Historical notices cannot create an unbounded queue on the first pull.
pub const BACKLOG_BOUND: usize = 120;
/// Seven days of receipt history plus one day for delays in the runtime's
/// hourly retention sweep; run inbox keys and tombstones outlive this history.
pub const RECEIPT_RETENTION_MS: i64 = 8 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub subject: String,
    pub event_key: String,
    pub digest: String,
    pub headers: BTreeMap<String, String>,
    /// JetStream's stored publish time, never a publisher-supplied header.
    /// Older journaled triggers have no timestamp and still replay their body.
    #[serde(default)]
    pub published_at_ms: i64,
}

// EventDeclaration currently has no flow-targeting field. Keep the known
// flow-targeted event contracts here so ingress, fan-out and body replay use
// the same rule without inventing a declaration wire field.
const FLOW_TARGETED_EVENTS: &[(&str, &str)] = &[("prefrontal-core", "digest_delivered")];

impl Notice {
    pub fn target_flow(&self) -> Result<Option<&str>> {
        let (module, name, _) = self.identity()?;
        if !FLOW_TARGETED_EVENTS.contains(&(module, name)) {
            return Ok(None);
        }
        self.headers
            .get("flow_id")
            .map(String::as_str)
            .filter(|flow| !flow.is_empty())
            .map(Some)
            .ok_or_else(|| CoreError::Invalid("flow-targeted event lacks flow_id header".into()))
    }

    /// Parse the subject before selecting the fetch key from transport headers.
    /// Key validation belongs to `identity`, after the key has been selected.
    pub fn subject_identity(&self) -> Result<(&str, &str, u32)> {
        let parts: Vec<_> = self.subject.split('.').collect();
        if !(6..=9).contains(&parts.len())
            || parts[0] != "ck"
            || parts[2] != "event"
            || parts.iter().any(|p| p.is_empty() || p.contains(['*', '>']))
            || parts[4..parts.len() - 1].iter().any(|p| {
                p.strip_prefix('v')
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            })
        {
            return Err(CoreError::Invalid("invalid event subject".into()));
        }
        let version_token = parts[parts.len() - 1];
        let version = version_token
            .strip_prefix('v')
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0 && version_token == format!("v{v}"))
            .ok_or_else(|| CoreError::Invalid("invalid event version".into()))?;
        // Borrow the complete dotted name, excluding the prefix and version separator.
        let name_start: usize = parts[..4].iter().map(|p| p.len() + 1).sum();
        let name_end = self.subject.len() - version_token.len() - 1;
        Ok((parts[3], &self.subject[name_start..name_end], version))
    }

    pub fn identity(&self) -> Result<(&str, &str, u32)> {
        let (module, name, version) = self.subject_identity()?;
        let lifecycle_key = module == "basal"
            && version == 1
            && crate::template_events::EVENTS.contains(&name)
            && self
                .headers
                .get("flow_id")
                .zip(self.headers.get("seq"))
                .is_some_and(|(flow, seq)| {
                    !flow.is_empty()
                        && !flow.contains(':')
                        && seq
                            .parse::<i64>()
                            .ok()
                            .is_some_and(|n| n > 0 && seq == &n.to_string())
                        && self.event_key == format!("{flow}:{seq}")
                });
        if !lifecycle_key
            && (self.event_key.len() != 64
                || !self
                    .event_key
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Err(CoreError::Invalid("invalid event key".into()));
        }
        Ok((module, name, version))
    }

    pub fn trigger_id(&self) -> String {
        format!("event:{}:{}", self.subject, self.event_key)
    }

    pub fn trigger(&self) -> Result<JsonText> {
        JsonText::new(serde_json::json!({"event_notice": self}).to_string())
            .map_err(|e| CoreError::Invalid(e.to_string()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub skipped_before_install: usize,
    pub filtered_out: usize,
    pub matched: usize,
    pub admitted: usize,
    pub duplicate: usize,
    pub queued: usize,
    pub overflow: usize,
    /// Wakes committed for pending Broca model calls (see `broca_wakes`).
    pub model_wakes: usize,
}

#[derive(PartialEq, Eq)]
enum HeaderMatch {
    Unrelated,
    Filtered,
    Accepted,
}

fn header_matches(key: &str, pattern: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let (pattern, value) = (pattern.as_bytes(), value.as_bytes());
    let equal = |a: u8, b: u8| {
        if key == "repo" {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };
    let (mut p, mut v, mut star) = (0, 0, None);
    while v < value.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, v));
            p += 1;
        } else if p < pattern.len() && equal(pattern[p], value[v]) {
            p += 1;
            v += 1;
        } else if let Some((start, consumed)) = star {
            v = consumed + 1;
            star = Some((start, v));
            p = start + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn effective_manifest(conn: &rusqlite::Connection, text: &str, flow: &str) -> Result<Manifest> {
    let mut manifest =
        Manifest::parse_stored(text).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    if manifest.params.is_some() {
        let text: String = conn.query_row(
            "SELECT params FROM template_instances WHERE flow_id=?1",
            [flow],
            |r| r.get(0),
        )?;
        let values = serde_json::from_str(&text).map_err(|e| CoreError::Corrupt(e.to_string()))?;
        crate::templates::effective(&mut manifest, &values);
    }
    Ok(manifest)
}

fn matches(manifest: &Manifest, notice: &Notice, flow_id: &str) -> Result<HeaderMatch> {
    if notice
        .target_flow()?
        .is_some_and(|target| target != flow_id)
    {
        return Ok(HeaderMatch::Unrelated);
    }
    let (module, name, version) = notice.identity()?;
    let Some(event) = manifest
        .trigger
        .events
        .iter()
        .flatten()
        .find(|e| e.module == module && e.name == name && e.version == version)
    else {
        return Ok(HeaderMatch::Unrelated);
    };
    Ok(
        if event.header_match.iter().all(|(key, pattern)| {
            notice
                .headers
                .get(key)
                .is_some_and(|value| header_matches(key, pattern, value))
        }) {
            HeaderMatch::Accepted
        } else {
            HeaderMatch::Filtered
        },
    )
}

fn receipt(tx: &Transaction, flow: &str, notice: &Notice, state: &str, now: i64) -> Result<()> {
    tx.execute("INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![flow, notice.subject, notice.event_key, serde_json::to_string(notice).map_err(|e| CoreError::Invalid(e.to_string()))?, state, now])?;
    Ok(())
}

fn after_first_approval(
    tx: &Transaction,
    flow: &str,
    notice: &Notice,
    first_approved: Option<i64>,
) -> Result<bool> {
    if first_approved.is_some_and(|first| notice.published_at_ms >= first) {
        return Ok(true);
    }
    tx.execute("INSERT INTO event_flow_health(flow_id,skipped_before_install) VALUES (?1,1) ON CONFLICT(flow_id) DO UPDATE SET skipped_before_install=skipped_before_install+1", [flow])?;
    Ok(false)
}

fn offer(
    tx: &Transaction,
    store_id: &str,
    flow: &str,
    notice: &Notice,
    ctx: &admission::AdmitContext,
    report: &mut Report,
) -> Result<()> {
    if install::is_disabled(tx, flow)? {
        return Ok(());
    }
    report.matched += 1;
    let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM event_receipts WHERE flow_id=?1 AND subject=?2 AND event_key=?3)",
        params![flow, notice.subject, notice.event_key], |r| r.get(0))?;
    if known {
        report.duplicate += 1;
        return Ok(());
    }
    // Existing waiting notices must not be overtaken by a newer arrival.
    let queued: usize = tx.query_row(
        "SELECT COUNT(*) FROM event_receipts WHERE flow_id=?1 AND state='backlog'",
        [flow],
        |r| r.get(0),
    )?;
    let outcome = if queued == 0 {
        admission::admit_current(
            tx,
            store_id,
            flow,
            &notice.trigger_id(),
            notice.trigger()?,
            ctx,
        )?
    } else {
        // Still check saturation when a queue is full: a sustained flood must
        // trip the same auto-disable policy as any other admission source.
        crate::rate::check(tx, flow, crate::rate::Kind::Run, ctx.now_ms, &ctx.rate)?;
        Admission::RateLimited
    };
    match outcome {
        Admission::Admitted { .. } => {
            receipt(tx, flow, notice, "admitted", ctx.now_ms)?;
            report.admitted += 1;
        }
        Admission::Duplicate { .. } | Admission::Tombstoned { .. } => report.duplicate += 1,
        Admission::RateLimited | Admission::Draining | Admission::StateBusy => {
            if queued < BACKLOG_BOUND {
                receipt(tx, flow, notice, "backlog", ctx.now_ms)?;
                report.queued += 1;
            } else {
                receipt(tx, flow, notice, "refused", ctx.now_ms)?;
                tx.execute("INSERT INTO event_flow_health(flow_id,overflow,last_overflow_at) VALUES (?1,1,?2) ON CONFLICT(flow_id) DO UPDATE SET overflow=overflow+1,last_overflow_at=excluded.last_overflow_at", params![flow, ctx.now_ms])?;
                report.overflow += 1;
            }
        }
        Admission::Disabled | Admission::NotApproved => {}
    }
    Ok(())
}

impl Runtime {
    /// The entire fan-out commits before the caller acknowledges any notice.
    /// A failed transaction leaves every notice eligible for redelivery.
    pub fn admit_events(&self, notices: &[Notice]) -> Result<Report> {
        let ctx = self.admit_context()?;
        let store_id = self.store().store_id().to_owned();
        let report = self.store().write(|tx| {
            let flows: Vec<(String, Option<i64>)> = tx
                .prepare("SELECT flow_id,first_approved_at FROM flows WHERE state='enabled' ORDER BY flow_id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let approved = flows
                .iter()
                .map(|(flow, first_approved)| {
                    let manifest = install::approved(tx, flow)?
                        .map(|approved| effective_manifest(tx, &approved.manifest, flow))
                        .transpose()?;
                    Ok((flow, first_approved, manifest))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut report = Report::default();
            for notice in notices {
                notice.target_flow()?;
                report.model_wakes += broca_wakes(tx, notice, ctx.now_ms)?;
                for (flow, first_approved, approved) in &approved {
                    if let Some(manifest) = approved {
                        match matches(manifest, notice, flow)? {
                            HeaderMatch::Unrelated => continue,
                            HeaderMatch::Filtered => {
                                tx.execute("INSERT INTO event_flow_health(flow_id,filtered_out) VALUES (?1,1) ON CONFLICT(flow_id) DO UPDATE SET filtered_out=filtered_out+1", [flow])?;
                                report.filtered_out += 1;
                                continue;
                            }
                            HeaderMatch::Accepted => {}
                        }
                        // Upgrades retain the first approval point, so events
                        // during an upgrade remain eligible for the current code.
                        if !after_first_approval(tx, flow, notice, **first_approved)? {
                            report.skipped_before_install += 1;
                            continue;
                        }
                        offer(tx, &store_id, flow, notice, &ctx, &mut report)?;
                    }
                }
            }
            Ok(report)
        })?;
        self.shared.signal.bump();
        if report.model_wakes > 0 {
            self.shared.host.model_events_committed();
        }
        Ok(report)
    }

    /// Drain a bounded batch, preserving arrival order within each flow and
    /// rechecking approval, enablement and rate policy at admission time.
    pub fn drain_event_backlog(&self) -> Result<usize> {
        let ctx = self.admit_context()?;
        let store_id = self.store().store_id().to_owned();
        let admitted = self.store().write(|tx| {
            let pending: Vec<(i64, String, String, Option<i64>)> = tx.prepare("SELECT e.rowid,e.flow_id,e.notice,f.first_approved_at FROM event_receipts e JOIN flows f USING(flow_id) WHERE e.state='backlog' AND f.state='enabled' ORDER BY e.received_at,e.rowid LIMIT 128")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut blocked = std::collections::BTreeSet::new();
            let mut admitted = 0;
            for (rowid, flow, encoded, first_approved) in pending {
                if blocked.contains(&flow) { continue; }
                let notice: Notice = serde_json::from_str(&encoded).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                if !after_first_approval(tx, &flow, &notice, first_approved)? {
                    tx.execute("UPDATE event_receipts SET state='refused' WHERE rowid=?1", [rowid])?;
                    continue;
                }
                let approved = install::approved(tx, &flow)?;
                let eligible = approved.as_ref().map(|a| {
                    matches(&effective_manifest(tx, &a.manifest, &flow)?, &notice, &flow)
                }).transpose()?;
                if eligible != Some(HeaderMatch::Accepted) {
                    tx.execute("UPDATE event_receipts SET state='refused' WHERE rowid=?1", [rowid])?;
                    continue;
                }
                match admission::admit_current(tx, &store_id, &flow, &notice.trigger_id(), notice.trigger()?, &ctx)? {
                    Admission::Admitted { .. } => {
                        tx.execute("UPDATE event_receipts SET state='admitted' WHERE rowid=?1", [rowid])?;
                        admitted += 1;
                    }
                    Admission::Duplicate { .. } | Admission::Tombstoned { .. } => {
                        tx.execute("UPDATE event_receipts SET state='admitted' WHERE rowid=?1", [rowid])?;
                    }
                    _ => { blocked.insert(flow); }
                }
            }
            Ok(admitted)
        })?;
        if admitted > 0 {
            self.shared.signal.bump();
        }
        Ok(admitted)
    }
}

/// Commits a wake for each pending Broca model call a Broca run event
/// concerns, inside the reader's admission transaction, so the wake is
/// durable before the notice is acknowledged. `run.ended` names its run;
/// `run.skipped` (Broca dropped a session's events during a bus outage)
/// names only its session. A skipped session no pending call has learned
/// commits a `recheck` for every call whose session is still unknown, since
/// it may be any of theirs. Every other event wakes nothing. Returns how
/// many wakes were committed; a notice that matches no pending call commits
/// nothing and is acknowledged like any other.
fn broca_wakes(tx: &Transaction, notice: &Notice, now: i64) -> Result<usize> {
    use basal_host::broca::WakeKind;
    let (module, name, version) = notice.identity()?;
    if module != "broca" || version != 1 {
        return Ok(0);
    }
    let header = |key: &str| {
        notice
            .headers
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    let run_id = header("run_id");
    let session_ref = header("session_ref");
    if let (Some(run_id), Some(session_ref)) = (run_id, session_ref) {
        learn_session_ref(tx, run_id, session_ref)?;
    }
    let calls = |sql: &str, key: Option<&str>| -> Result<Vec<String>> {
        let mut statement = tx.prepare(sql)?;
        let params: &[&dyn rusqlite::ToSql] = match &key {
            Some(key) => &[key],
            None => &[],
        };
        let rows = statement.query_map(params, |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    };
    let (kind, targets) = match name {
        "run.ended" => {
            let Some(run_id) = run_id else {
                return Ok(0);
            };
            (
                WakeKind::Ended,
                calls(
                    "SELECT send_id FROM broca_waits WHERE broca_run_id = ?1",
                    Some(run_id),
                )?,
            )
        }
        "run.skipped" => {
            let Some(session_ref) = session_ref else {
                return Ok(0);
            };
            let known = calls(
                "SELECT send_id FROM broca_waits WHERE session_ref = ?1",
                Some(session_ref),
            )?;
            if known.is_empty() {
                (
                    WakeKind::Recheck,
                    calls(
                        "SELECT send_id FROM broca_waits WHERE session_ref IS NULL",
                        None,
                    )?,
                )
            } else {
                (WakeKind::Skipped, known)
            }
        }
        _ => return Ok(0),
    };
    // A recheck reads no body, so it keeps no event key: one per call,
    // however many skipped sessions asked for it.
    let (event_key, digest) = if kind == WakeKind::Recheck {
        ("", "")
    } else {
        (notice.event_key.as_str(), notice.digest.as_str())
    };
    for send_id in &targets {
        tx.execute(
            "INSERT OR IGNORE INTO broca_wakes (send_id, event_key, kind, digest, run_id, session_ref, seq, received_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![send_id, event_key, kind.as_str(), digest, run_id, session_ref, header("seq"), now],
        )?;
    }
    Ok(targets.len())
}

/// How a pending model call learns its Broca session_ref: from the headers
/// of any Broca run event that names its run (`run.started` arrives first).
/// Broca derives the value from the session's bind, so taking Broca's own
/// value avoids recomputing it here.
fn learn_session_ref(tx: &Transaction, run_id: &str, session_ref: &str) -> Result<()> {
    tx.execute(
        "UPDATE broca_waits SET session_ref = ?2 WHERE broca_run_id = ?1 AND session_ref IS NULL",
        params![run_id, session_ref],
    )?;
    Ok(())
}

/// Bounded receipt pruning; admitted runs and permanent trigger tombstones
/// remain the deduplication authority after receipt history expires.
pub fn prune_receipts(tx: &Transaction, now: i64) -> Result<usize> {
    Ok(tx.execute("DELETE FROM event_receipts WHERE rowid IN (SELECT rowid FROM event_receipts WHERE state<>'backlog' AND received_at<=?1 ORDER BY +rowid LIMIT 128)", [now.saturating_sub(RECEIPT_RETENTION_MS)])?)
}

/// Outcome before any script instruction executes. Retry times are durable,
/// so an unavailable source never turns a pending run into a busy-loop.
pub(crate) enum Preamble {
    Ready(JsonText),
    Retry,
    Failed { kind: String, detail: String },
}

fn mismatch(detail: impl Into<String>) -> Preamble {
    Preamble::Failed {
        kind: "event_body_mismatch".into(),
        detail: detail.into(),
    }
}

/// Only the historically documented SHA-256 prefix is normalized. Uppercase,
/// whitespace, other algorithms and malformed hexadecimal remain refused.
pub fn digest_hex(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    (hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(hex)
}

pub fn verify_body(
    notice: &Notice,
    reply: &serde_json::Value,
) -> std::result::Result<Vec<u8>, &'static str> {
    use sha2::{Digest, Sha256};
    let result = reply.get("result").unwrap_or(reply);
    let body = result["body"].as_str().ok_or("event_body_mismatch")?;
    let supplied = result["digest"]
        .as_str()
        .filter(|d| !d.starts_with("sha256:"))
        .and_then(digest_hex)
        .ok_or("event_body_mismatch")?;
    let expected = digest_hex(&notice.digest).ok_or("event_body_mismatch")?;
    let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
    if hash != expected || supplied != expected {
        return Err("event_body_mismatch");
    }
    let (_, name, _) = notice.identity().map_err(|_| "event_body_mismatch")?;
    if result["event_key"].as_str() != Some(&notice.event_key)
        || result["event_name"].as_str() != Some(name)
    {
        return Err("event_body_mismatch");
    }
    serde_json::from_str::<serde_json::Value>(body).map_err(|_| "event_body_mismatch")?;
    Ok(body.as_bytes().to_vec())
}

fn script_trigger(notice: &Notice, body: &[u8]) -> Result<JsonText> {
    let (module, name, version) = notice.identity()?;
    let body: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
    JsonText::new(
        serde_json::json!({"event": {
            "module": module, "name": name, "version": version,
            "event_key": notice.event_key, "headers": notice.headers, "body": body
        }})
        .to_string(),
    )
    .map_err(|e| CoreError::Invalid(e.to_string()))
}

fn body_targets_run(notice: &Notice, body: &[u8], flow_id: &str) -> bool {
    match notice.target_flow() {
        Ok(None) => true,
        Ok(Some(target)) => {
            target == flow_id
                && serde_json::from_slice::<serde_json::Value>(body)
                    .is_ok_and(|body| body["flow_id"].as_str() == Some(flow_id))
        }
        Err(_) => false,
    }
}

impl Runtime {
    pub(crate) fn event_preamble(&self, run: &crate::Run, manifest: &Manifest) -> Result<Preamble> {
        use basal_host::{CallClass, CallRequest, Dispatched, TransportError};
        use basal_proto::{CallKind, Settlement};
        use rusqlite::OptionalExtension;
        let payload: serde_json::Value = serde_json::from_str(run.trigger.as_str())
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        let Some(encoded) = payload.get("event_notice") else {
            // Synthetic dry runs already carry the script's {event:{...}}
            // value. They never resolve a live notice or bypass capture mode.
            return Ok(Preamble::Ready(run.trigger.clone()));
        };
        let notice: Notice = serde_json::from_value(encoded.clone())
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        let (module, name, version) = notice.identity()?;
        let now = self.config.clock.now_ms();
        if run.deadline_at.is_some_and(|deadline| now >= deadline) {
            return Ok(Preamble::Failed {
                kind: "deadline".into(),
                detail: "event body unavailable before run deadline".into(),
            });
        }
        let saved: Option<(Option<Vec<u8>>, i64, Option<i64>)> = self.store().read(|c| {
            Ok(c.query_row(
                "SELECT body,attempt,retry_at FROM event_body_journal WHERE run_id=?1",
                [&run.run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
        })?;
        if let Some((Some(body), _, _)) = &saved {
            if !body_targets_run(&notice, body, &run.flow_id) {
                return Ok(mismatch("flow-targeted body does not name the run's flow"));
            }
            return Ok(Preamble::Ready(script_trigger(&notice, body)?));
        }
        if saved
            .as_ref()
            .is_some_and(|(_, _, at)| at.is_some_and(|at| now < at))
        {
            return Ok(Preamble::Retry);
        }
        let op = basal_host::catalog::EVENT_BODY_OP;
        if !manifest.lists_op(module, op) {
            return Ok(Preamble::Failed {
                kind: "event_body_op_not_granted".into(),
                detail: format!("{module}.{op} not granted"),
            });
        }
        let kind = CallKind::Op {
            module: module.into(),
            op: op.into(),
        };
        let attempt = saved.map_or(1, |(_, attempt, _)| attempt.saturating_add(1));
        // A durable query intent precedes the send; a crash may repeat the
        // read, but a committed body is always replayed without another call.
        self.store().write(|tx| {
            tx.execute("INSERT INTO event_body_journal(run_id,attempt) VALUES (?1,?2) ON CONFLICT(run_id) DO UPDATE SET attempt=excluded.attempt,retry_at=NULL", params![run.run_id, attempt])?;
            Ok(())
        })?;
        let request = CallRequest {
            flow_id: run.flow_id.clone(),
            run_id: run.run_id.clone(),
            position: u64::MAX,
            kind: kind.clone(),
            args: JsonText::new(
                serde_json::json!({"event_key":notice.event_key,"event_name":name,"version":version}).to_string(),
            )
            .map_err(|e| CoreError::Invalid(e.to_string()))?,
            idempotency_key: format!("event-body:{}", run.run_id),
            attempt: u32::try_from(attempt).unwrap_or(u32::MAX),
        };
        let dispatched = match self.shared.host.provider_ready(&run.flow_id, &kind) {
            Ok(()) => self
                .shared
                .host
                .dispatch_classified(&request, CallClass::Query),
            Err(refusal) => Err(TransportError::Refused(refusal)),
        };
        if run
            .deadline_at
            .is_some_and(|deadline| self.config.clock.now_ms() >= deadline)
        {
            return Ok(Preamble::Failed {
                kind: "deadline".into(),
                detail: "event body query returned after the run deadline".into(),
            });
        }
        if let Err(TransportError::Refused(refusal)) = &dispatched
            && refusal.reason == basal_host::flow_refusal::RefusalReason::NoFlowScope
        {
            let should_warn = self.mark_event_scope_warning(&run.run_id);
            if should_warn {
                tracing::warn!(
                    flow_id = %run.flow_id,
                    run_id = %run.run_id,
                    provider = %refusal.provider,
                    reason = refusal.reason.as_str(),
                    "event body fetch is waiting for the flow scope"
                );
            }
        }
        match dispatched {
            Ok(Dispatched::Completed(outcome)) => {
                let reply: serde_json::Value = match serde_json::from_str(outcome.value.as_str()) {
                    Ok(reply) => reply,
                    Err(e) => return Ok(mismatch(e.to_string())),
                };
                if outcome.settlement == Settlement::Rejected || reply.get("error").is_some() {
                    let code = reply["error"]["code"]
                        .as_str()
                        .or_else(|| reply["code"].as_str())
                        .unwrap_or("events_get_refused");
                    return Ok(Preamble::Failed {
                        kind: code.into(),
                        detail: reply.to_string(),
                    });
                }
                let body = match verify_body(&notice, &reply) {
                    Ok(body) => body,
                    Err(kind) => return Ok(mismatch(kind)),
                };
                if !body_targets_run(&notice, &body, &run.flow_id) {
                    return Ok(mismatch("flow-targeted body does not name the run's flow"));
                }
                let trigger = script_trigger(&notice, &body)?;
                self.store().write(|tx| {
                    tx.execute("UPDATE event_body_journal SET body=?2,digest=?3,retry_at=NULL WHERE run_id=?1", params![run.run_id, body, notice.digest])?;
                    Ok(())
                })?;
                Ok(Preamble::Ready(trigger))
            }
            Err(TransportError::Refused(refusal)) if !refusal.readiness() => Ok(Preamble::Failed {
                kind: refusal.reason.as_str().into(),
                detail: refusal.message().into(),
            }),
            _ => {
                let shift = u32::try_from(attempt.saturating_sub(1))
                    .unwrap_or(31)
                    .min(31);
                let delay = self
                    .config
                    .deferral_retry
                    .saturating_mul(1u32 << shift)
                    .min(self.config.deferral_retry_max);
                let retry = now
                    .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX))
                    .min(run.deadline_at.unwrap_or(i64::MAX));
                self.store().write(|tx| {
                    tx.execute(
                        "UPDATE event_body_journal SET retry_at=?2 WHERE run_id=?1",
                        params![run.run_id, retry],
                    )?;
                    Ok(())
                })?;
                Ok(Preamble::Retry)
            }
        }
    }
}
