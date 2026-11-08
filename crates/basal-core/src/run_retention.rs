//! Retention for log-backed runs and their SQLite finished-run history.
//! Successful log-backed runs have no payloads.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{Result, Store, admission::trigger_key, retention::PruneReport};

#[cfg(test)]
#[path = "run_retention_tests.rs"]
mod tests;

/// At most 128 candidates per maintenance pass, in transactions of 16 runs.
/// The connection is returned between chunks so admission and callbacks can run.
pub(crate) const RUN_CHUNK: usize = 16;
const BATCH: usize = crate::retention::BATCH;

// Filter before LIMIT so permanently open obligations cannot starve deletions.
const OBLIGATIONS: &str = "EXISTS (SELECT 1 FROM obligations o WHERE o.run_id=r.run_id AND o.state<>'settled') \
    OR EXISTS (SELECT 1 FROM token_ledger t WHERE t.run_id=r.run_id AND t.state='reserved') \
    OR EXISTS (SELECT 1 FROM classify_costs c WHERE c.kind='flow' AND c.run_id=r.run_id AND c.charged IS NULL) \
    OR EXISTS (SELECT 1 FROM decision_cards d WHERE d.run_id=r.run_id AND d.state='open') \
    OR EXISTS (SELECT 1 FROM broca_calls b WHERE b.run_id=r.run_id AND COALESCE(json_extract(b.snapshot,'$.acknowledged'),0)=0) \
    OR EXISTS (SELECT 1 FROM outbox o WHERE o.kind='refusal_committed' AND CAST(json_extract(o.body,'$.run_id') AS TEXT)=r.run_id)";

const HISTORY_PREDICATES: &[(&str, &str)] = &[
    (
        "quarantine",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM run_identities r WHERE r.run_id=quarantine.run_id)",
    ),
    (
        "audit",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM run_identities r WHERE r.run_id=audit.run_id) AND NOT EXISTS (SELECT 1 FROM decision_cards d WHERE d.elicitation_id=audit.elicitation_id AND d.state='open')",
    ),
    ("outbox", "delivered_at<=?1 AND kind<>'refusal_committed'"),
    ("schedule_dropped", "at<=?1"),
    (
        "token_ledger",
        "state='settled' AND settled_at<=?1 AND NOT EXISTS (SELECT 1 FROM run_identities r WHERE r.run_id=token_ledger.run_id)",
    ),
    (
        "token_windows",
        "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM token_ledger t WHERE t.flow_id=token_windows.flow_id AND t.window_ms=token_windows.window_ms AND t.window_start=token_windows.window_start AND (t.state='reserved' OR EXISTS (SELECT 1 FROM run_identities r WHERE r.run_id=t.run_id)))",
    ),
    (
        "decision_cards",
        "state<>'open' AND answered_at<=?1 AND NOT EXISTS (SELECT 1 FROM run_identities r WHERE r.run_id=decision_cards.run_id) AND (kind<>'reenable' OR EXISTS (SELECT 1 FROM decision_cards newer WHERE newer.kind='reenable' AND newer.flow_id=decision_cards.flow_id AND newer.instance>decision_cards.instance))",
    ),
];

// Live reservations are checked separately against a pre-transaction snapshot.
// Closing a live run records its outstanding calls as obligations, so their
// rate windows remain protected after the live accounting entries are removed.
const RATE_WINDOWS_PREDICATE: &str = "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM obligations o JOIN run_history r USING(run_id) WHERE r.flow_id=rate_windows.flow_id AND o.state<>'settled' AND o.audit_at>=rate_windows.window_start AND o.audit_at<rate_windows.window_start+rate_windows.window_ms)";

fn candidates(conn: &Connection, cutoff: i64, unsettled: bool) -> Result<Vec<String>> {
    let predicate = if unsettled { "" } else { "NOT" };
    let mut stmt = conn.prepare(&format!(
        "SELECT r.run_id FROM run_history r WHERE r.ended_at<=?1 AND {predicate} ({OBLIGATIONS}) ORDER BY +r.run_id LIMIT {BATCH}"
    ))?;
    Ok(stmt
        .query_map([cutoff], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn tombstone_calls(tx: &Transaction<'_>, run_id: &str, now: i64) -> Result<()> {
    // Terminal commit already reserves every write key, including successful
    // calls that have no detail row. Preserve those tombstones and backfill from
    // retained call detail before its history cascade removes it.
    tx.execute(
        "INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) \
         SELECT 'idempotency_key',json_extract(detail,'$.intent.idempotency_key'),run_id,?2 \
         FROM run_calls WHERE run_id=?1 AND class IN ('mutation','keyed_mutation') \
         AND json_extract(detail,'$.intent.idempotency_key') IS NOT NULL",
        params![run_id, now],
    )?;
    Ok(())
}

fn prune_in(tx: &Transaction<'_>, cutoff: i64, now: i64, run_id: &str) -> Result<PruneReport> {
    // Candidates are advisory: both age and every obligation are rechecked in
    // the chunk's transaction, including runs changed after the candidate scan.
    let candidate = tx.query_row(
        &format!("SELECT r.flow_id,r.trigger_id,({OBLIGATIONS}),i.snapshot_id FROM run_history r JOIN run_identities i USING(run_id) WHERE r.ended_at<=?1 AND r.run_id=?2"),
        params![cutoff, run_id],
        |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,i64>(3)?)),
    ).optional()?;
    let Some((flow, trigger, unsettled, snapshot)) = candidate else {
        return Ok(PruneReport::default());
    };
    if unsettled {
        return Ok(PruneReport {
            kept_unsettled: vec![run_id.into()],
            ..PruneReport::default()
        });
    }
    tx.execute(
        "INSERT OR IGNORE INTO tombstones(kind,key,run_id,pruned_at) VALUES('trigger',?1,?2,?3)",
        params![trigger_key(&flow, &trigger), run_id, now],
    )?;
    tombstone_calls(tx, run_id, now)?;
    // Only acknowledged Broca snapshots pass the pruning check. Deleting their
    // broca_calls rows fires a trigger that removes waits and wakes before the
    // run identity is deleted.
    tx.execute("DELETE FROM broca_calls WHERE run_id=?1", [run_id])?;
    tx.execute("DELETE FROM trigger_inbox WHERE run_id=?1", [run_id])?;
    crate::run_store_sql::prune_history(tx, run_id)?;
    tx.execute("DELETE FROM run_snapshots WHERE snapshot_id=?1 AND NOT EXISTS(SELECT 1 FROM run_identities WHERE snapshot_id=?1)", [snapshot])?;
    Ok(PruneReport {
        pruned: vec![run_id.into()],
        ..PruneReport::default()
    })
}

fn chunks(
    ids: &[String],
    mut write: impl FnMut(&[String]) -> Result<PruneReport>,
) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    for chunk in ids.chunks(RUN_CHUNK) {
        let pruned = write(chunk)?;
        report.pruned.extend(pruned.pruned);
        report.kept_unsettled.extend(pruned.kept_unsettled);
    }
    Ok(report)
}

pub(crate) fn prune(store: &Store, now: i64, retention: i64) -> Result<PruneReport> {
    let cutoff = now.saturating_sub(retention);
    let (ids, kept) =
        store.read(|c| Ok((candidates(c, cutoff, false)?, candidates(c, cutoff, true)?)))?;
    let mut report = chunks(&ids, |chunk| {
        store.write(|tx| prune_chunk(tx, chunk, cutoff, now))
    })?;
    report.kept_unsettled.extend(kept);
    Ok(report)
}

fn prune_chunk(tx: &Transaction<'_>, ids: &[String], cutoff: i64, now: i64) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    for id in ids {
        let pruned = prune_in(tx, cutoff, now, id)?;
        report.pruned.extend(pruned.pruned);
        report.kept_unsettled.extend(pruned.kept_unsettled);
    }
    Ok(report)
}

pub(crate) fn prune_history(
    store: &Store,
    cutoff: i64,
    rate_cutoff: i64,
    audit_at: impl Fn(&str) -> Vec<i64>,
) -> Result<bool> {
    let flows = store.read(|conn| {
        let mut stmt = conn.prepare(
            "SELECT flow_id FROM rate_windows WHERE window_start+window_ms<=?1 UNION SELECT flow_id FROM token_windows WHERE window_start+window_ms<=?2",
        )?;
        Ok(stmt
            .query_map([rate_cutoff,cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut live = BTreeMap::new();
    for flow in flows {
        // audit_at takes the accounting lock only for the copy. Neither the
        // candidate read nor the following write holds it across SQLite.
        live.insert(flow.clone(), audit_at(&flow));
    }
    // One JSON parameter avoids SQLite's variable limit for large live sets.
    // Apply the live fence before LIMIT so pinned windows cannot starve others.
    let live_json =
        serde_json::to_string(&live).map_err(|e| crate::CoreError::Invalid(e.to_string()))?;
    let mut full = false;
    for &(table, predicate) in HISTORY_PREDICATES {
        full |= delete_history_batch(
            store,
            table,
            predicate,
            cutoff,
            if table == "token_windows" {
                Some(&live_json)
            } else {
                None
            },
        )? == BATCH;
    }
    full |= delete_history_batch(
        store,
        "rate_windows",
        RATE_WINDOWS_PREDICATE,
        rate_cutoff,
        Some(&live_json),
    )? == BATCH;
    // A close can make the snapshot conservative. Revalidate outside SQLite
    // and promptly revisit windows when live accounting changed during the pass.
    full |= live.iter().any(|(flow, before)| *before != audit_at(flow));
    Ok(full)
}

fn delete_history_batch(
    store: &Store,
    table: &str,
    predicate: &str,
    cutoff: i64,
    live_json: Option<&str>,
) -> Result<usize> {
    // Live token reservations, like dispatch counts, have no ledger rows until
    // close. Protect their windows as well as the committed ledger dependencies.
    let live = if live_json.is_some() {
        format!(
            "AND NOT EXISTS (SELECT 1 FROM json_each(?2) flow, json_each(flow.value) stamp WHERE flow.key={table}.flow_id AND stamp.value>={table}.window_start AND stamp.value<{table}.window_start+{table}.window_ms)"
        )
    } else {
        String::new()
    };
    let (key, order) = if table == "rate_windows" {
        (
            "(flow_id,window_ms,window_start)",
            "flow_id,window_ms,window_start",
        )
    } else {
        ("rowid", "+rowid")
    };
    let columns = if table == "rate_windows" { order } else { key };
    let query = format!(
        "DELETE FROM {table} WHERE {key} IN (SELECT {columns} FROM {table} WHERE {predicate} {live} ORDER BY {order} LIMIT {BATCH})"
    );
    store.write(|tx| {
        Ok(if let Some(json) = live_json {
            tx.execute(&query, params![cutoff, json])?
        } else {
            tx.execute(&query, [cutoff])?
        })
    })
}
