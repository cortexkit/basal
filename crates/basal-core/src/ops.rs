//! Health: what an operator needs to see at a glance.

use std::collections::BTreeMap;

use rusqlite::Connection;

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
