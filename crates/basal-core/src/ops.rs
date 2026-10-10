//! Health: what an operator needs to see at a glance.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;
use crate::model::to_u64;

/// A snapshot of the runtime's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    /// Run count per state.
    pub runs: BTreeMap<String, u64>,
    /// Non-terminal runs by state (`pending`, `running`, `suspended`,
    /// `needs_reconcile`).
    pub unfinished: BTreeMap<String, Vec<String>>,
    /// Calls whose outcome is unknown, as (run, position).
    pub unknown_calls: Vec<(String, u64)>,
    /// Calls with no outcome anywhere, across every run, as (run, position).
    pub open_obligations: Vec<(String, u64)>,
    pub quarantined: u64,
    pub draining: bool,
}

/// How long the oldest run in each waiting state has existed, in
/// milliseconds on the runtime's clock, measured from its admission (the
/// store keeps no time of entry into each state, and admission is the time
/// that matters to whoever is waiting for the run's effect).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunAges {
    pub oldest_pending_ms: Option<i64>,
    pub oldest_suspended_ms: Option<i64>,
    pub oldest_needs_reconcile_ms: Option<i64>,
}

pub fn run_ages(conn: &Connection, now_ms: i64) -> Result<RunAges> {
    let oldest = |state: &str| -> Result<Option<i64>> {
        let admitted: Option<i64> = conn.query_row(
            "SELECT MIN(admitted_at) FROM runs WHERE state = ?1",
            [state],
            |r| r.get(0),
        )?;
        Ok(admitted.map(|a| now_ms.saturating_sub(a).max(0)))
    };
    Ok(RunAges {
        oldest_pending_ms: oldest("pending")?,
        oldest_suspended_ms: oldest("suspended")?,
        oldest_needs_reconcile_ms: oldest("needs_reconcile")?,
    })
}

/// A flow's most recent finished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    pub run_id: String,
    pub state: String,
    pub error_kind: Option<String>,
    /// When it ended, in milliseconds since the Unix epoch (its admission
    /// time if the store holds no end time, which a finished run always has).
    pub ended_at: i64,
}

/// One flow's health, as core reads it to decide whether a claim holds:
/// whether it is enabled, how its last run ended, how old its oldest due and
/// unstarted work is, how many runs in a row failed, and which runs wait for
/// an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowHealth {
    pub event_backlog: u64,
    pub event_overflow: u64,
    pub grant_losses: Vec<crate::grant_loss::GrantLoss>,
    pub agent_retirement: Option<serde_json::Value>,
    pub waiting_reason: Option<String>,
    pub flow_id: String,
    pub enabled: bool,
    pub owner: Option<String>,
    pub approved_version: Option<u32>,
    /// Who disabled the flow, as recorded with the disable:
    /// `operator:<who>`, `agent:<id>` or `runtime` (auto-disable).
    pub disabled_by: Option<String>,
    /// Why, as recorded with the disable.
    pub disabled_reason: Option<String>,
    /// Disabled by the runtime itself for sustained saturation.
    pub auto_disabled: bool,
    pub last_run: Option<LastRun>,
    /// The age of the oldest work that is due and has not started: a
    /// schedule's passed due time not yet planned, a planned fire not yet
    /// admitted, or an admitted run not yet started. `None` when there is
    /// none, which is healthy: a quiet flow had nothing to do.
    pub oldest_overdue_ms: Option<i64>,
    /// Finished runs that failed (or hit an engine mismatch) since the last
    /// one that succeeded. Cancelled runs neither count nor reset it.
    pub consecutive_failures: u64,
    pub needs_reconcile: Vec<String>,
}

pub fn flow_health(conn: &Connection, now_ms: i64) -> Result<Vec<FlowHealth>> {
    let mut flows = Vec::new();
    // Fetch rejected readiness evidence once, not once per deadline failure.
    let mut rejections = conn.prepare_cached(
        "SELECT r.run_id, m.value FROM runs r JOIN mailbox m USING(run_id) WHERE r.state='failed' AND r.error_kind='deadline' AND m.settlement='rejected' \
         UNION ALL SELECT r.run_id, j.value FROM runs r JOIN journal j USING(run_id) WHERE r.state='failed' AND r.error_kind='deadline' AND j.settlement='rejected'"
    )?;
    let mut readiness_failures = std::collections::BTreeSet::new();
    for row in rejections.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (run_id, value) = row?;
        if typed_scope_rejection(&value, false) {
            readiness_failures.insert(run_id);
        }
    }
    let ids: Vec<String> = conn
        .prepare("SELECT flow_id FROM flows ORDER BY flow_id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for flow_id in ids {
        let Some(record) = crate::install::flow(conn, &flow_id)? else {
            continue;
        };
        let mut last_run = None;
        let mut consecutive_failures = 0;
        let mut counting = true;
        let mut stmt = conn.prepare_cached(
            "SELECT run_id, state, error_kind, COALESCE(ended_at, admitted_at) FROM runs \
             WHERE flow_id = ?1 \
             AND state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled') \
             ORDER BY admit_seq DESC, run_id DESC",
        )?;
        let finished = stmt.query_map([&flow_id], |r| {
            Ok(LastRun {
                run_id: r.get(0)?,
                state: r.get(1)?,
                error_kind: r.get(2)?,
                ended_at: r.get(3)?,
            })
        })?;
        for run in finished {
            let run = run?;
            if counting {
                let readiness_failure = if run.error_kind.as_deref() == Some("readiness_rejection")
                {
                    true
                } else if run.error_kind.as_deref() == Some("deadline") {
                    readiness_failures.contains(&run.run_id)
                } else {
                    false
                };
                match run.state.as_str() {
                    "failed" if readiness_failure => {}
                    "failed" if run.error_kind.as_deref() == Some("agent_retired") => {}
                    "failed" | "engine_mismatch" => consecutive_failures += 1,
                    "succeeded" => counting = false,
                    _ => {}
                }
            }
            if last_run.is_none() {
                last_run = Some(run);
            }
            if !counting && last_run.is_some() {
                break;
            }
        }
        let needs_reconcile: Vec<String> = conn
            .prepare_cached(
                "SELECT run_id FROM runs WHERE flow_id = ?1 AND state = 'needs_reconcile' \
                 ORDER BY admit_seq",
            )?
            .query_map([&flow_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let next_due: Option<i64> = conn
            .query_row(
                "SELECT next_due_ms FROM schedules WHERE flow_id = ?1 AND state = 'active'",
                [&flow_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let planned: Option<i64> = conn.query_row(
            "SELECT MIN(due_ms) FROM schedule_fires WHERE flow_id = ?1",
            [&flow_id],
            |r| r.get(0),
        )?;
        let pending: Option<i64> = conn.query_row(
            "SELECT MIN(admitted_at) FROM runs WHERE flow_id = ?1 AND state = 'pending'",
            [&flow_id],
            |r| r.get(0),
        )?;
        let oldest_overdue_ms = [next_due, planned, pending]
            .into_iter()
            .flatten()
            .filter(|since| *since <= now_ms)
            .map(|since| now_ms - since)
            .max();
        let waiting_reason = conn.query_row("SELECT refusal_detail FROM journal JOIN runs USING (run_id) WHERE runs.flow_id = ?1 AND journal.dispatch = 'deferred' AND runs.state IN ('running','pending','suspended') ORDER BY admit_seq, position LIMIT 1", [&flow_id], |r| r.get::<_, String>(0)).optional()?;
        let waiting_reason = match waiting_reason {
            Some(reason) => Some(reason),
            None => conn.query_row(
                "SELECT scope_health FROM flows WHERE flow_id=?1",
                [&flow_id],
                |r| r.get(0),
            )?,
        };
        flows.push(FlowHealth {
            event_backlog: conn.query_row(
                "SELECT COUNT(*) FROM event_receipts WHERE flow_id=?1 AND state='backlog'",
                [&flow_id],
                |r| r.get(0),
            )?,
            event_overflow: conn
                .query_row(
                    "SELECT overflow FROM event_flow_health WHERE flow_id=?1",
                    [&flow_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0),
            grant_losses: crate::grant_loss::losses(conn, Some(&flow_id))?
                .into_iter()
                .filter(|g| matches!(g.state.as_str(), "polling" | "stopped"))
                .collect(),
            agent_retirement: conn
                .query_row(
                    "SELECT agent_retirement FROM flows WHERE flow_id=?1",
                    [&flow_id],
                    |r| r.get::<_, Option<String>>(0),
                )?
                .map(|body| {
                    serde_json::from_str(&body)
                        .map_err(|e| crate::CoreError::Corrupt(e.to_string()))
                })
                .transpose()?,
            waiting_reason,
            auto_disabled: record.disabled_by.as_deref() == Some(crate::install::RUNTIME_ACTOR),
            disabled_reason: record.disabled_reason,
            flow_id,
            enabled: record.enabled,
            owner: record.owner,
            approved_version: record.approved_version,
            disabled_by: record.disabled_by,
            last_run,
            oldest_overdue_ms,
            consecutive_failures,
            needs_reconcile,
        });
    }
    Ok(flows)
}

pub(crate) fn typed_scope_rejection(value: &str, readiness_only: bool) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(value) else {
        return false;
    };
    if !value["module"].is_string() {
        return false;
    }
    match value["code"].as_str() {
        Some("no_flow_scope" | "target_flow_unsupported") => true,
        Some(
            "scope_not_carrier"
            | "scope_ended"
            | "scope_not_live"
            | "scope_epoch_required"
            | "scope_not_synced"
            | "scope_changed"
            | "scope_unsupported"
            | "resource_busy"
            | "consent_unavailable",
        ) => !readiness_only,
        _ => false,
    }
}

pub(crate) fn health(conn: &Connection) -> Result<Health> {
    let mut h = Health::default();
    let mut counts = conn.prepare("SELECT state, COUNT(*) FROM runs GROUP BY state")?;
    for row in counts.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (state, count) = row?;
        h.runs.insert(state, to_u64(count, "run count")?);
    }
    let mut unfinished = conn.prepare("SELECT run_id, state FROM runs WHERE state IN ('pending','running','suspended','needs_reconcile') ORDER BY run_id")?;
    for row in unfinished.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (run_id, state) = row?;
        h.unfinished.entry(state).or_default().push(run_id);
    }
    let calls = |filter: &str| -> Result<Vec<(String, u64)>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT j.run_id, j.position FROM journal j JOIN runs r ON r.run_id=j.run_id \
             WHERE j.settlement IS NULL AND NOT EXISTS \
             (SELECT 1 FROM mailbox m WHERE m.run_id=j.run_id AND m.position=j.position) \
             {filter} ORDER BY j.run_id, j.position"
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.map(|row| {
            let (run, position) = row?;
            Ok((run, to_u64(position, "position")?))
        })
        .collect()
    };
    h.unknown_calls = calls("AND j.dispatch='unknown'")?;
    h.open_obligations = calls(
        "AND NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.run_id=j.run_id AND q.position=j.position AND q.reason='run_cancelled')",
    )?;
    let q: i64 = conn.query_row("SELECT COUNT(*) FROM quarantine", [], |r| r.get(0))?;
    h.quarantined = to_u64(q, "quarantine count")?;
    let draining: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'draining'", [], |r| {
            r.get(0)
        })
        .optional()?;
    h.draining = draining.as_deref() == Some("1");
    Ok(h)
}
