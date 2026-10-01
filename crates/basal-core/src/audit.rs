//! The call audit: one durable row per call a flow makes, allowed or
//! refused.
//!
//! The row is written in the transaction that journals the call, so a call
//! cannot be recorded without its audit row or the reverse. It is written
//! once: a replay serves recorded calls inside the worker without the
//! parent seeing them, a recovery that sends a recorded call again does not
//! journal it again, and the table's key on (run, position) refuses a
//! second row outright.

use basal_proto::{ArgsDigest, CallKind};
use rusqlite::{Connection, Transaction, params};

use crate::error::{CoreError, Result};
use crate::model::to_u64;

/// The outcome recorded for a call that was allowed.
pub const ALLOWED: &str = "allowed";

/// Writes the audit row of the call at (run, position).
pub fn record(
    tx: &Transaction,
    flow_id: &str,
    run_id: &str,
    position: u64,
    kind: &CallKind,
    digest: &ArgsDigest,
    outcome: &str,
    at: i64,
) -> Result<()> {
    let position =
        i64::try_from(position).map_err(|_| CoreError::Invalid(format!("position {position}")))?;
    tx.execute(
        "INSERT INTO call_audit (run_id, position, flow_id, op, args_digest, outcome, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            run_id,
            position,
            flow_id,
            kind.to_string(),
            digest.0.as_slice(),
            outcome,
            at
        ],
    )?;
    Ok(())
}

/// One audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub position: u64,
    pub flow_id: String,
    pub op: String,
    pub args_digest: [u8; 32],
    pub outcome: String,
}

/// A run's audit rows, in position order.
pub fn rows(conn: &Connection, run_id: &str) -> Result<Vec<AuditRow>> {
    let mut stmt = conn.prepare(
        "SELECT position, flow_id, op, args_digest, outcome FROM call_audit \
         WHERE run_id = ?1 ORDER BY position",
    )?;
    let rows = stmt
        .query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(position, flow_id, op, digest, outcome)| {
            Ok(AuditRow {
                position: to_u64(position, "audit position")?,
                flow_id,
                op,
                args_digest: crate::model::digest(digest)?,
                outcome,
            })
        })
        .collect()
}
