//! Health: what an operator needs to see at a glance.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;
use crate::journal;
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
        let mut stmt = conn.prepare(
            "SELECT run_id, state, error_kind, COALESCE(ended_at, admitted_at) FROM runs \
             WHERE flow_id = ?1 \
             AND state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled') \
             ORDER BY admit_seq DESC, run_id DESC",
        )?;
        let finished = stmt
            .query_map([&flow_id], |r| {
                Ok(LastRun {
                    run_id: r.get(0)?,
                    state: r.get(1)?,
                    error_kind: r.get(2)?,
                    ended_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for run in finished {
            if counting {
                match run.state.as_str() {
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
            .prepare(
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
        flows.push(FlowHealth {
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

pub(crate) fn health(conn: &Connection) -> Result<Health> {
    let mut h = Health::default();
    let mut stmt = conn.prepare("SELECT run_id, state FROM runs ORDER BY run_id")?;
    let runs = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (run_id, state) in runs {
        *h.runs.entry(state.clone()).or_insert(0) += 1;
        if matches!(
            state.as_str(),
            "pending" | "running" | "suspended" | "needs_reconcile"
        ) {
            h.unfinished.entry(state).or_default().push(run_id.clone());
        }
        for p in journal::unknown_positions(conn, &run_id)? {
            h.unknown_calls.push((run_id.clone(), p));
        }
        for p in journal::unsettled_positions(conn, &run_id)? {
            h.open_obligations.push((run_id.clone(), p));
        }
    }
    let q: i64 = conn.query_row("SELECT COUNT(*) FROM quarantine", [], |r| r.get(0))?;
    h.quarantined = to_u64(q, "quarantine count")?;
    let draining: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'draining'", [], |r| {
            r.get(0)
        })
        .ok();
    h.draining = draining.as_deref() == Some("1");
    Ok(h)
}
