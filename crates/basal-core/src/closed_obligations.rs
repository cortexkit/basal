//! Calls that are still unsettled when their run ends.
//!
//! A closed run never dispatches again, so nothing will resend such a call,
//! and its obligation row is the only record left of it. Every row must end
//! settled, or wait where the operator can see it: on a reconcile card, or in
//! the self-diagnosis findings once no card is open. A row nobody can settle
//! and nobody is told about would stay forever.

use rusqlite::{Transaction, params};
use serde_json::{Value, json};

use crate::{
    CoreError, Result,
    model::StoredClass,
    run_host::RecoveredHosts,
    run_records::CallState,
    run_store_sql::{self as sql, Receipt, SettleResult},
};

/// What closing a run does with one call that has an intent but no outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Closing {
    /// The host accepted the call under a handle, so its completion can
    /// still arrive and settle the row. The row is kept as the run left it.
    Awaiting,
    /// The row is settled without an observed outcome, for this reason.
    Unobserved(String),
    /// The call may have had an effect that only the operator can establish.
    /// The row is recorded `unknown` with this reason, and a reconcile card is
    /// raised for it.
    Unknown(String),
}

/// The settlement reason recorded when the operator cancelled the run at
/// this attempt of the call.
const OPERATOR_CANCELLED: &str = "operator_cancelled";

pub(crate) fn closing(call: &CallState) -> Closing {
    // A completion is matched to its call by handle. Without one, nothing
    // that arrives later can ever settle the row.
    if call.accepted.is_some() {
        return Closing::Awaiting;
    }
    let reason = call.ambiguous_reason().as_str().to_owned();
    match call.identity.class {
        // A query has no effect, so its outcome matters only to the script
        // that asked, and that script has ended.
        StoredClass::Query => Closing::Unobserved(reason),
        // The target deduplicates by idempotency key, so the effect happened
        // at most once, and with the run closed no later send can repeat it.
        // A card would ask the operator a question with no decision behind it.
        StoredClass::KeyedMutation => Closing::Unobserved(reason),
        // The operator already answered for this attempt by cancelling.
        _ if cancelled(call) => Closing::Unobserved(OPERATOR_CANCELLED.into()),
        // A plain mutation may have taken effect, and the operator may need
        // to know that.
        _ => Closing::Unknown(reason),
    }
}

fn cancelled(call: &CallState) -> bool {
    call.intent.as_ref().is_some_and(|intent| {
        call.resolutions
            .last()
            .is_some_and(|r| r.attempts == intent.attempts && r.choice == "cancel")
    })
}

/// Settles the ledgers of a call whose closed-run obligation was settled
/// without an observed outcome, and records why. Its reservations are kept
/// as the charge, as for any completion that reports no usage.
pub(crate) fn finish_unobserved(
    tx: &Transaction<'_>,
    run_id: &str,
    position: u64,
    class: StoredClass,
    reason: &str,
    now: i64,
) -> Result<()> {
    crate::tokens::settle(
        tx,
        crate::tokens::Key::Call { run_id, position },
        crate::tokens::Report::Unreported,
        now,
    )?;
    crate::classify_cost::settle(tx, "flow", run_id, position, None, None)?;
    // A card raised while the run was live now asks about a settled row.
    tx.execute(
        "UPDATE decision_cards SET state='stale',answered_at=?3 WHERE kind='reconcile' AND run_id=?1 AND position=?2 AND state='open'",
        params![run_id, crate::model::position_i64(position)?, now],
    )?;
    crate::reconcile::audit(
        tx,
        crate::install::RUNTIME_ACTOR,
        "obligation.settled_unobserved",
        Some(run_id),
        Some(position),
        &json!({"class": class.as_str(), "reason": reason}).to_string(),
    )
}

/// Settles an `unknown` closed-run obligation after the operator answered
/// its card with something other than an observed result. The run is closed,
/// so no answer can resend the call or give a script its outcome; the answer
/// is kept in the audit, and `applied` also keeps the receipt a live run
/// would have recorded.
pub(crate) fn settle_answered(
    tx: &Transaction<'_>,
    run_id: &str,
    position: u64,
    choice: &str,
    now: i64,
) -> Result<bool> {
    let Some(row) = sql::obligation(tx, run_id, position)? else {
        return Ok(false);
    };
    if row.state != sql::ObligationState::Unknown {
        return Ok(false);
    }
    let receipt = if choice == crate::decisions::APPLIED {
        let outcome = crate::reconcile::reconciled_as_applied();
        Receipt::Value(crate::ids::payload_hash(outcome.settlement, &outcome.value))
    } else {
        Receipt::Unknown
    };
    if sql::settle_obligation(tx, run_id, position, row.handle.as_deref(), receipt, now)?
        != SettleResult::Settled
    {
        return Ok(false);
    }
    let class = row
        .detail
        .pointer("/identity/class")
        .and_then(Value::as_str)
        .map(StoredClass::parse)
        .transpose()?
        .ok_or_else(|| CoreError::Corrupt("unknown obligation has no call class".into()))?;
    finish_unobserved(
        tx,
        run_id,
        position,
        class,
        &format!("operator_{choice}"),
        now,
    )?;
    Ok(true)
}

// Selects only rows an older build closed differently from `closing`, and
// `unknown` rows whose card the operator has already answered. Settled rows
// leave this table, so the scan reads unsettled rows only, and on a repaired
// store it returns nothing. A row without a call identity is a history-lost
// Broca snapshot, which has its own run-level diagnostic.
const REPAIR: &str = "SELECT o.run_id,o.position,o.state,o.detail,h.flow_id,
    (SELECT d.choice FROM decision_cards d WHERE d.kind='reconcile' AND d.run_id=o.run_id AND d.position=o.position
        AND d.instance=json_extract(o.detail,'$.intent.attempts') AND d.state<>'open' AND d.choice IS NOT NULL
        ORDER BY d.seq DESC LIMIT 1)
    FROM obligations o JOIN run_history h USING(run_id)
    WHERE o.state<>'settled' AND json_type(o.detail,'$.identity') IS NOT NULL AND (
        (o.handle IS NULL AND (o.state='open' OR json_extract(o.detail,'$.identity.class') IN ('query','keyed_mutation')))
        OR (o.state='unknown' AND EXISTS(SELECT 1 FROM decision_cards d WHERE d.kind='reconcile' AND d.run_id=o.run_id
            AND d.position=o.position AND d.instance=json_extract(o.detail,'$.intent.attempts')
            AND d.state<>'open' AND d.choice IS NOT NULL)))
    ORDER BY o.run_id,o.position";

struct Stranded {
    run_id: String,
    position: u64,
    state: String,
    detail: String,
    flow_id: String,
    answered: Option<String>,
}

impl Stranded {
    fn repair(&self, tx: &Transaction<'_>, now: i64) -> Result<bool> {
        let (id, position) = (self.run_id.as_str(), self.position);
        if let Some(choice) = &self.answered
            && self.state == "unknown"
        {
            return settle_answered(tx, id, position, choice, now);
        }
        let corrupt = |e: serde_json::Error| CoreError::Corrupt(format!("obligation detail: {e}"));
        let call: CallState = serde_json::from_str(&self.detail).map_err(corrupt)?;
        match closing(&call) {
            Closing::Awaiting => Ok(false),
            Closing::Unobserved(reason) => {
                if sql::settle_obligation(tx, id, position, None, Receipt::Unknown, now)?
                    != SettleResult::Settled
                {
                    return Ok(false);
                }
                finish_unobserved(tx, id, position, call.identity.class, &reason, now)?;
                Ok(true)
            }
            Closing::Unknown(reason) if self.state == "open" => {
                let mut detail: Value = serde_json::from_str(&self.detail).map_err(corrupt)?;
                detail["unknown"] = json!(reason);
                sql::mark_obligation_unknown_with(tx, id, position, &detail)
            }
            Closing::Unknown(_) => Ok(false),
        }
    }
}

impl RecoveredHosts {
    /// Brings obligations written by older builds in line with `closing`, and
    /// settles `unknown` rows whose card was answered before card answers
    /// settled closed runs. It runs once at startup; on a store with nothing
    /// to repair it is one read and no write. Returns the rows it changed.
    pub(crate) fn repair_closed_obligations(&self) -> Result<usize> {
        let rows = self.store.read(|conn| {
            let mut stmt = conn.prepare(REPAIR)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Stranded {
                        run_id: r.get(0)?,
                        position: r.get(1)?,
                        state: r.get(2)?,
                        detail: r.get(3)?,
                        flow_id: r.get(4)?,
                        answered: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut repaired = 0;
        for row in rows {
            // Recovery already holds the store's single-writer lease; the flow's
            // accounting lock is still taken so the ledger updates follow the
            // same order as every closed-run settlement.
            let changed = self
                .registry()
                .accounting_write(&row.flow_id, &self.store, |tx| {
                    row.repair(tx, self.clock.now_ms())
                })?;
            repaired += usize::from(changed);
        }
        Ok(repaired)
    }
}
