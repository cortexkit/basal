//! Codemode retention: 24 runtime-clock hours after a run ends, its program,
//! catalog, description, output and calls are deleted, and a permanent
//! tombstone keeps its id so the id is never admitted or executed again.
//!
//! The sweep runs from the runtime's deadline pass (see
//! `Runtime::retention_due`), on the engine's own thread, never on an async
//! executor. Each pruned run is its own short transaction, and one sweep
//! handles at most a bounded batch; a full batch leaves the next one due
//! at once, so the engine's next pass continues it.

use rusqlite::{Connection, Transaction, params};

use crate::error::Result;
use crate::store::Store;

/// How long an ended run stays readable: 24 hours on the runtime's clock.
pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

const BATCH: usize = 128;

/// When the earliest ended run becomes prunable, if any run has ended. A run
/// is prunable once more than [`RETENTION_MS`] has passed since it ended.
pub fn next_sweep_at(conn: &Connection) -> Result<Option<i64>> {
    let ended: Option<i64> = conn.query_row(
        "SELECT MIN(ended_at) FROM codemode_runs WHERE ended_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(ended.map(|at| at.saturating_add(RETENTION_MS).saturating_add(1)))
}

/// Prunes up to one batch of runs that ended more than [`RETENTION_MS`]
/// before `now_ms`, oldest first, and returns their ids. A running run is
/// never pruned.
pub fn sweep(store: &Store, now_ms: i64) -> Result<Vec<String>> {
    let cutoff = now_ms.saturating_sub(RETENTION_MS);
    let ids: Vec<String> = store.read(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT run_id FROM codemode_runs WHERE ended_at IS NOT NULL AND ended_at < ?1 \
             ORDER BY ended_at, run_id LIMIT {BATCH}"
        ))?;
        Ok(stmt
            .query_map([cutoff], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut pruned = Vec::with_capacity(ids.len());
    for id in ids {
        if store.write(|tx| prune(tx, &id, cutoff, now_ms))? {
            pruned.push(id);
        }
    }
    Ok(pruned)
}

fn prune(tx: &Transaction, run_id: &str, cutoff: i64, now_ms: i64) -> Result<bool> {
    // Re-check under the write transaction; the candidate read released the
    // connection.
    let due: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = ?1 \
         AND ended_at IS NOT NULL AND ended_at < ?2)",
        params![run_id, cutoff],
        |r| r.get(0),
    )?;
    if !due {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO codemode_tombstones (run_id, pruned_at) VALUES (?1, ?2)",
        params![run_id, now_ms],
    )?;
    tx.execute("DELETE FROM codemode_calls WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM codemode_runs WHERE run_id = ?1", [run_id])?;
    Ok(true)
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
