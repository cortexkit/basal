//! Retention: pruning finished runs, keeping tombstones.

use rusqlite::{Transaction, params};

use crate::admission::trigger_key;
use crate::error::Result;
use crate::journal;
use crate::runtime::Runtime;

/// What a prune did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned: Vec<String>,
    /// Terminal runs past retention kept because a call still has no
    /// outcome anywhere.
    pub kept_unsettled: Vec<String>,
}

fn prune_in(tx: &Transaction, cutoff: i64, now: i64) -> Result<PruneReport> {
    let mut stmt = tx.prepare(
        "SELECT run_id, flow_id, trigger_id FROM runs \
         WHERE state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled') \
         AND ended_at IS NOT NULL AND ended_at <= ?1 ORDER BY run_id",
    )?;
    let candidates = stmt
        .query_map([cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut report = PruneReport::default();
    for (run_id, flow_id, trigger_id) in candidates {
        if !journal::unsettled_positions(tx, &run_id)?.is_empty() {
            report.kept_unsettled.push(run_id);
            continue;
        }
        // An open token reservation is an obligation too: its usage has not
        // been reported. The call audit and the token ledger are records of
        // what the flow did and spent, and outlive the journal.
        if crate::tokens::open_reservations(tx, &run_id)? > 0 {
            report.kept_unsettled.push(run_id);
            continue;
        }
        // Tombstones outlive the run: a redelivered trigger is refused, and
        // the run's idempotency keys stay reserved.
        tx.execute(
            "INSERT OR IGNORE INTO tombstones (kind, key, run_id, pruned_at) VALUES ('trigger', ?1, ?2, ?3)",
            params![trigger_key(&flow_id, &trigger_id), run_id, now],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO tombstones (kind, key, run_id, pruned_at) \
             SELECT 'idempotency_key', idempotency_key, run_id, ?2 FROM journal \
             WHERE run_id = ?1 AND class IN ('mutation', 'keyed_mutation')",
            params![run_id, now],
        )?;
        tx.execute("DELETE FROM mailbox WHERE run_id = ?1", [&run_id])?;
        tx.execute("DELETE FROM journal WHERE run_id = ?1", [&run_id])?;
        tx.execute("DELETE FROM activations WHERE run_id = ?1", [&run_id])?;
        tx.execute("DELETE FROM trigger_inbox WHERE run_id = ?1", [&run_id])?;
        tx.execute("DELETE FROM runs WHERE run_id = ?1", [&run_id])?;
        report.pruned.push(run_id);
    }
    Ok(report)
}

impl Runtime {
    /// Prunes terminal runs that ended at least `retention_ms` before
    /// `now_ms` and have no open obligation. Never touches a run that is
    /// not terminal (`needs_reconcile` included).
    pub fn prune(&self, now_ms: i64, retention_ms: i64) -> Result<PruneReport> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        self.store().write(|tx| prune_in(tx, cutoff, now_ms))
    }

    /// Drops tombstones written before `before_ms`. Call it only once no
    /// source can still redeliver a trigger that old (the broker's
    /// redelivery horizon); until then a tombstone is what refuses it.
    pub fn prune_tombstones(&self, before_ms: i64) -> Result<usize> {
        self.store()
            .write(|tx| Ok(tx.execute("DELETE FROM tombstones WHERE pruned_at < ?1", [before_ms])?))
    }
}
