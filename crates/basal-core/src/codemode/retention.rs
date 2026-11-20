//! Codemode retention: 24 runtime-clock hours after a run ends, its program,
//! catalog, description, output and calls are deleted, and a permanent
//! tombstone keeps its id so the id is never admitted or executed again.
//!
//! The sweep runs from the runtime's deadline pass (see
//! `Runtime::retention_due`), on the engine's own thread, never on an async
//! executor. Each pruned run is its own short transaction; a full run batch
//! leaves the next one due at once. Uncollected late results are also swept
//! in bounded batches, without scheduling a separate wake.

use rusqlite::{Connection, Transaction, params};

use crate::error::Result;
use crate::store::Store;

/// How long an ended run stays readable: 24 hours on the runtime's clock.
pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

// Seven days from settlement is well beyond a live custodian's collection
// cadence (core drains every five minutes). The 24-hour payload reduction
// runs on publication or collection, so an idle feed may still have full rows.
const LATE_RESULT_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

const BATCH: usize = 128;

/// The candidate test, shared by the candidate scan and the re-check under the
/// write transaction so the two can never disagree: `?1` is the cutoff. A
/// running run has no `ended_at` and never matches.
const PRUNABLE: &str = "ended_at IS NOT NULL AND ended_at < ?1";

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
/// never pruned. Also prunes up to one batch of overdue late results.
pub fn sweep(store: &Store, now_ms: i64) -> Result<Vec<String>> {
    let cutoff = now_ms.saturating_sub(RETENTION_MS);
    let ids: Vec<String> = store.read(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT run_id FROM codemode_runs WHERE {PRUNABLE} \
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
    sweep_pruned_payloads(store)?;
    sweep_late_results(store, now_ms)?;
    Ok(pruned)
}

fn sweep_pruned_payloads(store: &Store) -> Result<()> {
    // Include runs pruned by older builds. Their tombstones prove that no
    // model retry can need the saved request, even if spend is still uncertain.
    const MODEL: &str =
        "SELECT c.rowid FROM classify_costs c JOIN codemode_tombstones t USING(run_id)
                        WHERE c.kind='program' AND c.args <> '{}'";
    const WITHDRAWAL: &str =
        "SELECT c.rowid FROM codemode_tool_calls c JOIN codemode_tombstones t USING(run_id)
                             WHERE json_type(c.withdrawal,'$.result') IS NOT NULL";
    let due = store.read(|conn| {
        Ok(conn.query_row(
            &format!("SELECT EXISTS({MODEL}) OR EXISTS({WITHDRAWAL})"),
            [],
            |r| r.get::<_, bool>(0),
        )?)
    })?;
    if due {
        store.write(|tx| {
            tx.execute(
                &format!("UPDATE classify_costs SET args='{{}}' WHERE rowid IN ({MODEL} LIMIT {BATCH})"),
                [],
            )?;
            tx.execute(
                &format!("UPDATE codemode_tool_calls SET withdrawal=?1 WHERE rowid IN ({WITHDRAWAL} LIMIT {BATCH})"),
                [r#"{"answer":"completed","outcome":"unknown","result_retained":false}"#],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

fn sweep_late_results(store: &Store, now_ms: i64) -> Result<()> {
    let cutoff = now_ms.saturating_sub(LATE_RESULT_RETENTION_MS);
    let due = store.read(|conn| {
        Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM codemode_late_results WHERE settled_at <= ?1)",
            [cutoff],
            |r| r.get::<_, bool>(0),
        )?)
    })?;
    if !due {
        return Ok(());
    }

    store.write(|tx| {
        let deleted = {
            let mut stmt = tx.prepare(
                "DELETE FROM codemode_late_results WHERE seq IN (
                    SELECT seq FROM codemode_late_results WHERE settled_at <= ?1
                    ORDER BY seq LIMIT ?2
                ) RETURNING custodian, seq",
            )?;
            stmt.query_map(params![cutoff, BATCH], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        raise_late_cursor_floors(tx, deleted)
    })
}

/// Preserve cursor validity after deleting rows. Reads still start at the
/// caller's cursor, not at this floor. Call inside the deletion transaction.
pub fn raise_late_cursor_floors(
    tx: &Transaction,
    deleted: impl IntoIterator<Item = (String, i64)>,
) -> Result<()> {
    let mut floor = tx.prepare(
        "INSERT INTO codemode_late_acks(custodian, through_seq) VALUES (?1, ?2)
         ON CONFLICT(custodian) DO UPDATE SET through_seq = MAX(through_seq, excluded.through_seq)",
    )?;
    for (custodian, seq) in deleted {
        floor.execute(params![custodian, seq])?;
    }
    Ok(())
}

fn prune(tx: &Transaction, run_id: &str, cutoff: i64, now_ms: i64) -> Result<bool> {
    // Re-check under the write transaction; the candidate read released the
    // connection.
    let due: bool = tx.query_row(
        &format!("SELECT EXISTS (SELECT 1 FROM codemode_runs WHERE {PRUNABLE} AND run_id = ?2)"),
        params![cutoff, run_id],
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
