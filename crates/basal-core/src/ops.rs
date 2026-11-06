//! Health: what an operator needs to see at a glance.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;

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
    pub skipped_before_install: u64,
    pub filtered_out: u64,
    pub stale_dropped: u64,
    pub event_backlog: u64,
    pub event_overflow: u64,
    pub grant_losses: Vec<crate::grant_loss::GrantLoss>,
    pub agent_retirement: Option<serde_json::Value>,
    pub waiting_reason: Option<String>,
    pub flow_id: String,
    pub state_epoch: u32,
    pub last_state_reset_at: Option<i64>,
    pub enabled: bool,
    pub owner: Option<String>,
    pub approved_version: Option<u32>,
    pub approval: Option<serde_json::Value>,
    pub install_checks: Vec<serde_json::Value>,
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
    /// Earliest end time in the counted failure streak, falling back to
    /// admission time when a terminal run has no recorded end time.
    pub failure_first_seen_ms: Option<i64>,
    pub needs_reconcile: Vec<String>,
}

pub(crate) fn flow_health_staged(
    conn: &Connection,
    now_ms: i64,
    runs: &[crate::Run],
    states: &BTreeMap<String, crate::run_records::State>,
) -> Result<Vec<FlowHealth>> {
    flow_health_with(conn, now_ms, (runs, states))
}

pub(crate) fn counted_failure_staged(
    conn: &Connection,
    id: &str,
    state: &str,
    kind: Option<&str>,
) -> Result<bool> {
    if state == "engine_mismatch" {
        return Ok(true);
    }
    if state != "failed"
        || matches!(
            kind,
            Some("readiness_rejection" | "agent_retired" | "version_revoked")
        )
    {
        return Ok(false);
    }
    Ok(kind != Some("deadline")
        || !conn.query_row(
            "SELECT deadline_exempt FROM run_history WHERE run_id=?1",
            [id],
            |r| r.get::<_, bool>(0),
        )?)
}

fn flow_health_with(
    conn: &Connection,
    now_ms: i64,
    staged: (&[crate::Run], &BTreeMap<String, crate::run_records::State>),
) -> Result<Vec<FlowHealth>> {
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
        let mut failure_first_seen_ms: Option<i64> = None;
        let mut counting = true;
        let mut stmt = conn.prepare_cached("SELECT run_id,state,error_kind,ended_at FROM run_history WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC")?;
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
                if {
                    counted_failure_staged(
                        conn,
                        &run.run_id,
                        &run.state,
                        run.error_kind.as_deref(),
                    )?
                } {
                    consecutive_failures += 1;
                    failure_first_seen_ms =
                        Some(failure_first_seen_ms.map_or(run.ended_at, |at| at.min(run.ended_at)));
                } else if run.state == "succeeded" {
                    counting = false;
                }
            }
            if last_run.is_none() {
                last_run = Some(run);
            }
            if !counting && last_run.is_some() {
                break;
            }
        }
        let needs_reconcile: Vec<String> = {
            let (runs, _) = staged;
            runs.iter()
                .filter(|r| r.flow_id == flow_id && r.state == crate::RunState::NeedsReconcile)
                .map(|r| r.run_id.clone())
                .collect()
        };
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
        let pending: Option<i64> = {
            let (runs, _) = staged;
            runs.iter()
                .filter(|r| r.flow_id == flow_id && r.state == crate::RunState::Pending)
                .map(|r| r.admitted_at)
                .min()
        };
        let oldest_overdue_ms = [next_due, planned, pending]
            .into_iter()
            .flatten()
            .filter(|since| *since <= now_ms)
            .map(|since| now_ms - since)
            .max();
        let waiting_reason = {
            let (runs, states) = staged;
            runs.iter()
                .filter(|r| r.flow_id == flow_id && !r.state.is_terminal())
                .find_map(|r| {
                    states.get(&r.run_id).and_then(|s| {
                        s.calls
                            .values()
                            .find(|c| c.arrival.is_none() && c.deferred.is_some())
                            .and_then(|c| c.deferred.as_ref().map(|d| d.cause.clone()))
                    })
                })
        };
        let waiting_reason = match waiting_reason {
            Some(reason) => Some(reason),
            None => conn.query_row(
                "SELECT scope_health FROM flows WHERE flow_id=?1",
                [&flow_id],
                |r| r.get(0),
            )?,
        };
        flows.push(FlowHealth {
            state_epoch: crate::state_epoch::recorded(conn, &flow_id)?,
            last_state_reset_at: conn.query_row(
                "SELECT last_state_reset_at FROM flows WHERE flow_id=?1",
                [&flow_id],
                |r| r.get(0),
            )?,
            filtered_out: conn
                .query_row(
                    "SELECT filtered_out FROM event_flow_health WHERE flow_id=?1",
                    [&flow_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0),
            stale_dropped: conn
                .query_row(
                    "SELECT stale_dropped FROM event_flow_health WHERE flow_id=?1",
                    [&flow_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0),
            skipped_before_install: conn
                .query_row(
                    "SELECT skipped_before_install FROM event_flow_health WHERE flow_id=?1",
                    [&flow_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0),
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
            approval: crate::install_policy::approval(conn, &flow_id, record.approved_version)?,
            install_checks: crate::install_policy::check_errors(conn, &flow_id)?,
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
            failure_first_seen_ms,
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
        Some(
            "no_flow_scope"
            | "target_flow_unsupported"
            | "flow_delegation_unsupported"
            | "for_target_requires_delegate",
        ) => true,
        Some(
            "scope_not_carrier"
            | "scope_ended"
            | "scope_not_live"
            | "scope_epoch_required"
            | "scope_not_synced"
            | "scope_changed"
            | "scope_unsupported"
            | "resource_busy"
            | "sink_audience_unavailable"
            | "consent_unavailable",
        ) => !readiness_only,
        _ => false,
    }
}
