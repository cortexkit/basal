//! Retention in bounded transactions, on the runtime's deadline-service cadence.
//!
//! Terminal runs, journal, mailbox, activations and Broca snapshots are kept
//! for seven days by default, and longer while any call, token reservation or
//! open decision card needs them. Quarantine, call/audit history, delivered
//! outbox notifications, settled token ledgers and dropped schedules keep
//! thirty days; any existing run protects its dependent history. Unsent
//! notifications and open reservations/cards stay until settled. Old rate
//! windows are removed only outside the current saturation streak and any
//! window a live call could refund; token windows stay while a reservation or
//! existing run needs them. Closed decision cards retain the latest re-enable
//! episode per flow forever, because its number allocates the next episode.
//! Trigger and idempotency tombstones stay forever: no source redelivery
//! horizon is established, so deleting one could permit a second effect.
//! Full batches schedule another bounded pass after one second rather than
//! limiting cleanup throughput to one batch per normal maintenance interval.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::time::Duration;

use crate::admission::trigger_key;
use crate::error::Result;
use crate::runtime::Runtime;

/// What a prune did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned: Vec<String>,
    /// Terminal runs past retention kept because a call still has no
    /// outcome anywhere.
    pub kept_unsettled: Vec<String>,
}

pub(crate) const BATCH: usize = 128;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub interval: Duration,
    pub runs: Duration,
    pub history: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            runs: Duration::from_secs(7 * 24 * 3600),
            history: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

// Kept in SQL so permanently unsettled old runs do not consume every slot
// of the bounded candidate batch and starve later, prunable runs.
pub(crate) const OBLIGATIONS: &str = "EXISTS (SELECT 1 FROM journal j WHERE j.run_id=r.run_id AND j.settlement IS NULL \
    AND NOT EXISTS (SELECT 1 FROM mailbox m WHERE m.run_id=j.run_id AND m.position=j.position) \
    AND NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.run_id=j.run_id AND q.position=j.position AND q.reason='run_cancelled')) \
    OR EXISTS (SELECT 1 FROM token_ledger t WHERE t.run_id=r.run_id AND t.state='reserved') \
    OR EXISTS (SELECT 1 FROM decision_cards d WHERE d.run_id=r.run_id AND d.state='open') \
    OR EXISTS (SELECT 1 FROM broca_calls b WHERE b.run_id=r.run_id AND COALESCE(json_extract(b.snapshot,'$.acknowledged'),0)=0) \
    OR EXISTS (SELECT 1 FROM outbox o WHERE o.kind='refusal_committed' AND CAST(json_extract(o.body,'$.run_id') AS TEXT)=r.run_id)";

// Unary + leaves the ordering unchanged, but prevents SQLite from choosing an
// ordered full-table scan just to satisfy LIMIT. Search the age range first.
// The literals also expose the exact format templates to query-plan tests.
macro_rules! candidates_sql {
    () => {
        "SELECT r.run_id FROM runs r WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') \
         AND r.ended_at<=?1 AND {predicate} ({OBLIGATIONS}) ORDER BY +r.run_id LIMIT {BATCH}"
    };
}
/// Run candidate query template; substitutes `predicate`, `OBLIGATIONS` and `BATCH`.
pub const CANDIDATES_SQL: &str = candidates_sql!();

macro_rules! history_batch_sql {
    () => {
        "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {predicate} ORDER BY +rowid LIMIT {BATCH})"
    };
}
/// Bounded history deletion template; substitutes `table`, `predicate` and `BATCH`.
pub const HISTORY_BATCH_SQL: &str = history_batch_sql!();

pub(crate) const HISTORY_PREDICATES: &[(&str, &str)] = &[
    (
        "quarantine",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=quarantine.run_id)",
    ),
    (
        "call_audit",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=call_audit.run_id)",
    ),
    (
        "audit",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=audit.run_id) AND NOT EXISTS (SELECT 1 FROM decision_cards d WHERE d.elicitation_id=audit.elicitation_id AND d.state='open')",
    ),
    ("outbox", "delivered_at<=?1 AND kind<>'refusal_committed'"),
    ("schedule_dropped", "at<=?1"),
    (
        "token_ledger",
        "state='settled' AND settled_at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=token_ledger.run_id)",
    ),
    (
        "token_windows",
        "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM token_ledger t WHERE t.flow_id=token_windows.flow_id AND t.window_ms=token_windows.window_ms AND t.window_start=token_windows.window_start AND (t.state='reserved' OR EXISTS (SELECT 1 FROM runs r WHERE r.run_id=t.run_id)))",
    ),
    (
        "decision_cards",
        "state<>'open' AND answered_at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=decision_cards.run_id) AND (kind<>'reenable' OR EXISTS (SELECT 1 FROM decision_cards newer WHERE newer.kind='reenable' AND newer.flow_id=decision_cards.flow_id AND newer.instance>decision_cards.instance))",
    ),
];

pub(crate) const RATE_WINDOWS_PREDICATE: &str = "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM call_audit a JOIN runs r USING(run_id) WHERE a.flow_id=rate_windows.flow_id AND a.at>=rate_windows.window_start AND a.at<rate_windows.window_start+rate_windows.window_ms)";

fn candidates(conn: &Connection, cutoff: i64, unsettled: bool) -> Result<Vec<String>> {
    let predicate = if unsettled { "" } else { "NOT" };
    let mut stmt = conn.prepare(&format!(
        candidates_sql!(),
        predicate = predicate,
        OBLIGATIONS = OBLIGATIONS,
        BATCH = BATCH,
    ))?;
    Ok(stmt
        .query_map([cutoff], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn prune_in(tx: &Transaction, cutoff: i64, now: i64, run_id: &str) -> Result<PruneReport> {
    // Recheck under the write transaction: an acknowledgement or a decision
    // card may have changed since the candidate scan released the connection.
    let candidate = tx
        .query_row(
            &format!(
                "SELECT r.flow_id,r.trigger_id,({OBLIGATIONS}) FROM runs r \
         WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') \
         AND r.ended_at<=?1 AND r.run_id=?2"
            ),
            params![cutoff, run_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((flow_id, trigger_id, unsettled)) = candidate else {
        return Ok(PruneReport::default());
    };
    if unsettled {
        return Ok(PruneReport {
            kept_unsettled: vec![run_id.to_owned()],
            ..PruneReport::default()
        });
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
    tx.execute("DELETE FROM mailbox WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM journal WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM activations WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM trigger_inbox WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM runs WHERE run_id = ?1", [run_id])?;
    tx.execute(
        "DELETE FROM meta WHERE key IN (?1,?2)",
        params![
            format!("journal_calls:{run_id}"),
            format!("journal_bytes:{run_id}")
        ],
    )?;
    Ok(PruneReport {
        pruned: vec![run_id.to_owned()],
        ..PruneReport::default()
    })
}

impl Runtime {
    /// Prunes terminal runs that ended at least `retention_ms` before
    /// `now_ms` and have no open obligation. Never touches a run that is
    /// not terminal (`needs_reconcile` included).
    pub fn prune(&self, now_ms: i64, retention_ms: i64) -> Result<PruneReport> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        let ids = self.store().read(|c| candidates(c, cutoff, false))?;
        let mut report = PruneReport {
            kept_unsettled: self.store().read(|c| candidates(c, cutoff, true))?,
            ..PruneReport::default()
        };
        for id in ids {
            // Release the single connection between runs; a batch can never
            // monopolize it for the whole retained history.
            let pruned = self.store().write(|tx| prune_in(tx, cutoff, now_ms, &id))?;
            report.pruned.extend(pruned.pruned);
            report.kept_unsettled.extend(pruned.kept_unsettled);
        }
        Ok(report)
    }

    pub(crate) fn retention_due(&self, now: i64) -> Result<()> {
        let mut next = self
            .shared
            .retention_next
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if next.is_some_and(|at| now < at) {
            return Ok(());
        }
        let config = &self.config.retention;
        let runs = self.prune(now, millis(config.runs))?;
        let history_full = self.prune_history(now, now.saturating_sub(millis(config.history)))?;
        let backlog = runs.pruned.len() == BATCH || history_full;
        let interval = if backlog {
            config.interval.min(Duration::from_secs(1))
        } else {
            config.interval
        };
        *next = Some(now.saturating_add(millis(interval).max(1)));
        Ok(())
    }

    fn prune_history(&self, now: i64, cutoff: i64) -> Result<bool> {
        let mut full = false;
        for &(table, predicate) in HISTORY_PREDICATES {
            full |= self.delete_history_batch(table, predicate, cutoff)? == BATCH;
        }
        let streak = millis(self.config.rate.window).saturating_mul(i64::from(
            self.config.rate.saturated_windows_to_disable.max(1),
        ));
        full |= self.delete_history_batch(
            "rate_windows",
            RATE_WINDOWS_PREDICATE,
            cutoff.min(now.saturating_sub(streak)),
        )? == BATCH;
        Ok(full)
    }

    fn delete_history_batch(&self, table: &str, predicate: &str, cutoff: i64) -> Result<usize> {
        // The names/predicates are private constants, never caller input.
        self.store().write(|tx| {
            Ok(tx.execute(
                &format!(
                    history_batch_sql!(),
                    table = table,
                    predicate = predicate,
                    BATCH = BATCH
                ),
                [cutoff],
            )?)
        })
    }
}
