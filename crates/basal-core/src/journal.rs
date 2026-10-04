//! The journal (issue log), the mailbox and the quarantine.
//!
//! - The journal has one row per issued call. A row's outcome is written
//!   only together with its delivery order, when it is handed to a worker.
//! - The mailbox holds outcomes that have arrived but have not been handed
//!   to a worker yet. Each arrival bumps the run's readiness sequence, a
//!   counter an activation compares to notice arrivals it has not looked at.
//! - The quarantine keeps completions that were not applied: ones that
//!   contradict a recorded outcome or name an unknown call, and ones for a
//!   cancelled run.
//!
//! Writes that extend or advance a run (a new call row, authorizing a send,
//! allocating a delivery order) are fenced: the statement itself requires
//! that the run is still `running` under the writing activation's owner and
//! generation (see [`crate::runs::FENCE`]), so a superseded activation's
//! write changes nothing. Writes that record a fact about a call already
//! sent (an outcome arriving, a host accepting a call as long-running, a
//! send ending in an unknown state) are not fenced: they are accepted from
//! any activation, identified by the call itself.

use basal_host::{CompletionAck, HostOutcome, UnknownReason};
use basal_proto::{ArgsDigest, CallKind, JsonText, Outcome, RecordedCall, Settlement};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::{CoreError, Result};
use crate::ids::{idempotency_key, payload_hash};
use crate::model::{
    CALL_COLUMNS, CallRow, DispatchState, StoredClass, json, kind_columns, parse_settlement,
    settlement_str, to_u64,
};
use crate::runs::{FENCE, Lease};
use crate::store::now_ms;

fn pos(p: u64) -> Result<i64> {
    i64::try_from(p).map_err(|_| CoreError::Invalid(format!("position {p}")))
}

/// Every call row of a run, in position order.
pub fn rows(conn: &Connection, run_id: &str) -> Result<Vec<CallRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CALL_COLUMNS} FROM journal WHERE run_id = ?1 ORDER BY position"
    ))?;
    let rows = stmt
        .query_map([run_id], CallRow::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().collect()
}

pub fn row(conn: &Connection, run_id: &str, position: u64) -> Result<Option<CallRow>> {
    conn.query_row(
        &format!("SELECT {CALL_COLUMNS} FROM journal WHERE run_id = ?1 AND position = ?2"),
        params![run_id, pos(position)?],
        CallRow::from_row,
    )
    .optional()?
    .transpose()
}

/// How many calls a run has journaled and how many bytes its journal holds
/// (arguments, requests sent and outcomes, the mailbox included).
pub fn run_size(conn: &Connection, run_id: &str) -> Result<(u64, u64)> {
    let (calls, journal_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(CAST(args AS BLOB)) \
         + COALESCE(length(CAST(value AS BLOB)), 0) \
         + COALESCE(length(CAST(request AS BLOB)), 0)), 0) FROM journal WHERE run_id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let waiting: i64 = conn.query_row(
        "SELECT COALESCE(SUM(length(CAST(value AS BLOB))), 0) FROM mailbox WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    Ok((
        to_u64(calls, "call count")?,
        to_u64(journal_bytes.saturating_add(waiting), "journal bytes")?,
    ))
}

pub fn call_count(conn: &Connection, run_id: &str) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM journal WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    to_u64(n, "call count")
}

/// The prefix a worker replays: every issued call, with the outcomes that
/// already have a committed delivery order. Outcomes waiting in the mailbox
/// are left out; they are released one at a time once the worker has
/// replayed this prefix and reports itself blocked.
pub fn prefix(conn: &Connection, run_id: &str) -> Result<Vec<RecordedCall>> {
    let rows = rows(conn, run_id)?;
    let prefix: Vec<RecordedCall> = rows.iter().map(CallRow::recorded).collect();
    Ok(prefix)
}

/// A call the worker issued that is not yet journaled.
pub struct NewCall<'a> {
    pub position: u64,
    pub kind: &'a CallKind,
    pub args: &'a JsonText,
    pub class: StoredClass,
    /// The exact bytes to send when they are not `args` (a model call's
    /// clamped request).
    pub request: Option<&'a JsonText>,
}

/// Journals a new call, fenced, and only at the run's next position, so
/// positions stay contiguous. Remote calls are journaled as authorized for
/// dispatch in the same statement. Returns the call's idempotency key.
pub fn insert_call(
    tx: &Transaction,
    lease: &Lease,
    flow_id: &str,
    call: &NewCall<'_>,
) -> Result<String> {
    let key = idempotency_key(flow_id, &lease.run_id, call.position);
    let (code, module, op) = kind_columns(call.kind);
    let digest = ArgsDigest::of(call.args);
    let (dispatch, attempts) = match call.class {
        StoredClass::Sync | StoredClass::Local => (DispatchState::None, 0),
        _ => (DispatchState::Sent, 1),
    };
    let changed = tx.execute(
        &format!(
            "INSERT INTO journal (run_id, position, kind_code, module, op, args, args_digest, \
             idempotency_key, class, dispatch, attempts, issued_generation, request) \
             SELECT ?1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?3, ?14 \
             WHERE {FENCE} AND ?4 = (SELECT COUNT(*) FROM journal WHERE run_id = ?1)"
        ),
        params![
            lease.run_id,
            lease.owner,
            lease.generation_i64()?,
            pos(call.position)?,
            code,
            module,
            op,
            call.args.as_str(),
            digest.0.as_slice(),
            key,
            call.class.as_str(),
            dispatch.as_str(),
            attempts,
            call.request.map(JsonText::as_str),
        ],
    )?;
    if changed == 0 {
        let next = call_count(tx, &lease.run_id)?;
        return Err(lease.explain(
            tx,
            CoreError::Worker(format!(
                "call issued at position {}, but the run's next position is {next}",
                call.position
            )),
        ));
    }
    Ok(key)
}

/// The next delivery order for the run: one above every committed order.
const NEXT_ORDER: &str =
    "(SELECT COALESCE(MAX(delivery_order) + 1, 0) FROM journal WHERE run_id = ?1)";

/// Records a synchronous call's value together with its delivery order,
/// fenced. Done in the transaction that journals the call (or, for a row
/// journaled without its value, before the value is sent).
pub fn record_sync(
    tx: &Transaction,
    lease: &Lease,
    position: u64,
    settlement: Settlement,
    value: &JsonText,
    clock_ms: Option<f64>,
) -> Result<Outcome> {
    let hash = payload_hash(settlement, value);
    let changed = tx.execute(
        &format!(
            "UPDATE journal SET settlement = ?5, value = ?6, payload_hash = ?7, clock_ms = ?8, \
             delivery_order = {NEXT_ORDER} \
             WHERE run_id = ?1 AND position = ?4 AND settlement IS NULL AND {FENCE}"
        ),
        params![
            lease.run_id,
            lease.owner,
            lease.generation_i64()?,
            pos(position)?,
            settlement_str(settlement),
            value.as_str(),
            hash.as_slice(),
            clock_ms,
        ],
    )?;
    if changed == 0 {
        return Err(lease.explain(
            tx,
            CoreError::Worker(format!("no unresolved call at position {position}")),
        ));
    }
    let order: i64 = tx.query_row(
        "SELECT delivery_order FROM journal WHERE run_id = ?1 AND position = ?2",
        params![lease.run_id, pos(position)?],
        |r| r.get(0),
    )?;
    Ok(Outcome {
        position,
        settlement,
        value: value.clone(),
        delivery_order: to_u64(order, "delivery order")?,
    })
}

/// The latest clock value recorded for the run, so the next read can be
/// kept monotonic.
pub fn last_clock(conn: &Connection, run_id: &str) -> Result<Option<f64>> {
    let v: Option<f64> = conn.query_row(
        "SELECT MAX(clock_ms) FROM journal WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    Ok(v)
}

/// Authorizes sending an unresolved call again (after a restart, or after an
/// operator reconciled it as not applied), fenced. Returns the attempt
/// number of the new send.
pub fn authorize_resend(tx: &Transaction, lease: &Lease, position: u64) -> Result<u32> {
    let changed = tx.execute(
        &format!(
            "UPDATE journal SET attempts = attempts + 1, dispatch = 'sent', refusal = NULL, retry_not_before = NULL, refusal_detail = NULL \
             WHERE run_id = ?1 AND position = ?4 AND settlement IS NULL \
             AND class IN ('query', 'mutation', 'keyed_mutation') AND {FENCE}"
        ),
        params![
            lease.run_id,
            lease.owner,
            lease.generation_i64()?,
            pos(position)?
        ],
    )?;
    if changed == 0 {
        return Err(lease.explain(
            tx,
            CoreError::Invalid(format!("position {position} cannot be sent again")),
        ));
    }
    let attempts: i64 = tx.query_row(
        "SELECT attempts FROM journal WHERE run_id = ?1 AND position = ?2",
        params![lease.run_id, pos(position)?],
        |r| r.get(0),
    )?;
    u32::try_from(attempts).map_err(|_| CoreError::Corrupt(format!("attempts {attempts}")))
}

/// The refusal and its retry time commit together; a crashed worker can replay
/// the unresolved position, but recovery cannot turn it into an early send.
pub fn defer(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    refusal: &basal_host::flow_refusal::FlowRefusal,
    retry_at: i64,
) -> Result<()> {
    let detail = serde_json::to_string(refusal).map_err(|e| CoreError::Invalid(e.to_string()))?;
    tx.execute("UPDATE journal SET dispatch = 'deferred', refusal = ?3, retry_not_before = ?4, refusal_detail = ?5 WHERE run_id = ?1 AND position = ?2 AND settlement IS NULL AND dispatch IN ('sent', 'deferred')",
        params![run_id, pos(position)?, refusal.reason.as_str(), retry_at, detail])?;
    Ok(())
}

pub fn deferred_until(conn: &Connection, run_id: &str, position: u64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT retry_not_before FROM journal WHERE run_id = ?1 AND position = ?2",
        params![run_id, pos(position)?],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

/// Feeds the existing pending-run loop rather than creating a retry scheduler.
pub fn wake_deferred(tx: &Transaction, now: i64, only: Option<&str>) -> Result<()> {
    tx.execute("UPDATE runs SET state = 'pending', awaited = NULL WHERE state = 'suspended' AND (?2 IS NULL OR run_id = ?2) AND EXISTS (SELECT 1 FROM journal WHERE journal.run_id = runs.run_id AND dispatch = 'deferred' AND retry_not_before <= ?1)", params![now, only])?;
    Ok(())
}

/// Releases the next outcome the blocked worker is waiting on: the earliest
/// arrival in the mailbox among `awaiting`, written into the journal with
/// the next delivery order, fenced, and removed from the mailbox. Returns the
/// outcome to deliver, if any, and the run's readiness sequence as read in
/// the same transaction.
pub fn release_next(
    tx: &Transaction,
    lease: &Lease,
    awaiting: &[u64],
) -> Result<(Option<Outcome>, u64)> {
    if !lease.holds(tx)? {
        return Err(lease.lost());
    }
    let readiness: i64 = tx.query_row(
        "SELECT readiness FROM runs WHERE run_id = ?1",
        [&lease.run_id],
        |r| r.get(0),
    )?;
    let readiness = to_u64(readiness, "readiness")?;
    let mut stmt = tx.prepare(
        "SELECT seq, position, settlement, value, payload_hash FROM mailbox \
         WHERE run_id = ?1 ORDER BY seq",
    )?;
    let entries = stmt
        .query_map([&lease.run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let Some((seq, position, settlement, value, hash)) = entries
        .into_iter()
        .find(|e| u64::try_from(e.1).is_ok_and(|p| awaiting.contains(&p)))
    else {
        return Ok((None, readiness));
    };
    let changed = tx.execute(
        &format!(
            "UPDATE journal SET settlement = ?5, value = ?6, payload_hash = ?7, \
             delivery_order = {NEXT_ORDER} \
             WHERE run_id = ?1 AND position = ?4 AND settlement IS NULL AND {FENCE}"
        ),
        params![
            lease.run_id,
            lease.owner,
            lease.generation_i64()?,
            position,
            settlement,
            value,
            hash
        ],
    )?;
    if changed == 0 {
        return Err(lease.explain(
            tx,
            CoreError::Corrupt(format!("mailbox entry for a resolved call at {position}")),
        ));
    }
    tx.execute("DELETE FROM mailbox WHERE seq = ?1", [seq])?;
    let order: i64 = tx.query_row(
        "SELECT delivery_order FROM journal WHERE run_id = ?1 AND position = ?2",
        params![lease.run_id, position],
        |r| r.get(0),
    )?;
    Ok((
        Some(Outcome {
            position: to_u64(position, "position")?,
            settlement: parse_settlement(&settlement)?,
            value: json(value, "mailbox value")?,
            delivery_order: to_u64(order, "delivery order")?,
        }),
        readiness,
    ))
}

/// Where an arriving outcome came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The host answered while dispatch waited.
    Host,
    /// A long-running call's completion.
    Completion,
    /// A local effect, committed with its outcome.
    Local,
    /// An operator's reconciliation.
    Reconcile,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Completion => "completion",
            Self::Local => "local",
            Self::Reconcile => "reconcile",
        }
    }
}

/// The result of offering an outcome to the mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub ack: CompletionAck,
    /// The run was suspended and is now runnable.
    pub woke: bool,
}

fn quarantine(
    tx: &Transaction,
    run_id: &str,
    position: i64,
    handle: Option<&str>,
    outcome: &HostOutcome,
    hash: &[u8; 32],
    reason: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO quarantine (run_id, position, handle, settlement, value, \
         payload_hash, reason, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run_id,
            position,
            handle,
            settlement_str(outcome.settlement),
            outcome.value.as_str(),
            hash.as_slice(),
            reason,
            now_ms()
        ],
    )?;
    Ok(())
}

/// Offers an outcome for (run, position), from any activation or none.
///
/// The call is identified by run, position, the host's handle (when the
/// call has one) and the payload hash. An identical redelivery is a no-op;
/// a contradictory one is quarantined and never applied; a completion for a
/// cancelled run is refused and logged. Otherwise the outcome waits in the
/// mailbox, the run's readiness sequence is bumped, and a suspended run
/// becomes runnable. It never allocates a delivery order and never changes
/// a running run's state.
pub fn accept_outcome(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    handle: Option<&str>,
    outcome: &HostOutcome,
    source: Source,
) -> Result<Accepted> {
    let p = pos(position)?;
    let hash = payload_hash(outcome.settlement, &outcome.value);
    let not_woken = |ack| Accepted { ack, woke: false };
    let run: Option<(String, i64)> = tx
        .query_row(
            "SELECT state, generation FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let recorded: Option<(Option<String>, Option<Vec<u8>>)> = tx
        .query_row(
            "SELECT handle, payload_hash FROM journal WHERE run_id = ?1 AND position = ?2",
            params![run_id, p],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (Some((state, generation)), Some((row_handle, row_hash))) = (run, recorded) else {
        quarantine(tx, run_id, p, handle, outcome, &hash, "unknown_call")?;
        return Ok(not_woken(CompletionAck::Quarantined));
    };
    if let (Some(h), Some(rh)) = (handle, row_handle.as_deref())
        && h != rh
    {
        quarantine(tx, run_id, p, handle, outcome, &hash, "handle_mismatch")?;
        return Ok(not_woken(CompletionAck::Quarantined));
    }
    if let Some(row_hash) = row_hash {
        if row_hash == hash {
            return Ok(not_woken(CompletionAck::Duplicate));
        }
        quarantine(tx, run_id, p, handle, outcome, &hash, "contradicts_outcome")?;
        return Ok(not_woken(CompletionAck::Quarantined));
    }
    let waiting: Option<Vec<u8>> = tx
        .query_row(
            "SELECT payload_hash FROM mailbox WHERE run_id = ?1 AND position = ?2",
            params![run_id, p],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(waiting) = waiting {
        if waiting == hash {
            return Ok(not_woken(CompletionAck::Duplicate));
        }
        quarantine(tx, run_id, p, handle, outcome, &hash, "contradicts_outcome")?;
        return Ok(not_woken(CompletionAck::Quarantined));
    }
    // The call's outcome is known now, whether or not its run still wants
    // it (a cancelled run's model call still spent tokens), so a model
    // call's token reservation is replaced by its reported usage, once.
    crate::tokens::settle_outcome(tx, run_id, position, outcome, now_ms())?;
    if state == "cancelled" {
        quarantine(tx, run_id, p, handle, outcome, &hash, "run_cancelled")?;
        return Ok(not_woken(CompletionAck::Refused));
    }
    tx.execute(
        "INSERT INTO mailbox (run_id, position, handle, settlement, value, payload_hash, \
         source, arrived_generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run_id,
            p,
            handle,
            settlement_str(outcome.settlement),
            outcome.value.as_str(),
            hash.as_slice(),
            source.as_str(),
            generation
        ],
    )?;
    if handle.is_some() && row_handle.is_none() {
        tx.execute(
            "UPDATE journal SET handle = ?3 WHERE run_id = ?1 AND position = ?2",
            params![run_id, p, handle],
        )?;
    }
    let woke = tx.execute(
        "UPDATE runs SET state = 'pending', awaited = NULL WHERE run_id = ?1 AND state = 'suspended'",
        [run_id],
    )? > 0;
    tx.execute(
        "UPDATE runs SET readiness = readiness + 1 WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(Accepted {
        ack: CompletionAck::Accepted,
        woke,
    })
}

/// Records that the host accepted a call as long-running. A fact about a
/// call already sent, so not fenced.
pub fn record_accepted(tx: &Transaction, run_id: &str, position: u64, handle: &str) -> Result<()> {
    tx.execute(
        "UPDATE journal SET dispatch = 'accepted', handle = ?3 \
         WHERE run_id = ?1 AND position = ?2 AND settlement IS NULL \
         AND dispatch IN ('sent', 'accepted')",
        params![run_id, pos(position)?, handle],
    )?;
    tx.execute(
        "UPDATE runs SET readiness = readiness + 1 WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(())
}

/// Records that a send ended without a provable outcome, and `reason`, why.
/// A run not driven by an activation goes straight to `needs_reconcile`; a
/// running one is moved there by its activation, under its fence.
pub fn record_unknown(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    reason: UnknownReason,
) -> Result<()> {
    tx.execute(
        "UPDATE journal SET dispatch = 'unknown', unknown_reason = ?3 \
         WHERE run_id = ?1 AND position = ?2 \
         AND settlement IS NULL AND NOT EXISTS \
         (SELECT 1 FROM mailbox WHERE run_id = ?1 AND position = ?2)",
        params![run_id, pos(position)?, reason.as_str()],
    )?;
    tx.execute(
        "UPDATE runs SET state = 'needs_reconcile', awaited = NULL, \
         error_kind = 'unknown_outcome', error_detail = ?2 \
         WHERE run_id = ?1 AND state IN ('pending', 'suspended')",
        params![run_id, format!("unknown outcome at position {position}")],
    )?;
    tx.execute(
        "UPDATE runs SET readiness = readiness + 1 WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(())
}

/// Records a host's report that an accepted call's outcome cannot be
/// established, with the host's `detail` as the run's error.
///
/// The call is identified like a completion, by run, position and handle.
/// A report for a call basal does not know, under another handle, or for a
/// call that already has an outcome is not applied (`Quarantined`); a
/// repeated report is `Duplicate`, and a report for a cancelled run is
/// `Refused`. Otherwise the call is marked unknown and a pending or
/// suspended run goes to `needs_reconcile` naming the detail; a run already
/// there gets the detail appended to its error. A run that has ended keeps
/// its state, with the call left visible as an unknown obligation.
///
/// Returns `None` while the run is `running`: an activation owns its state
/// then, so the host must report again later.
pub fn record_host_unknown(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    handle: &str,
    reason: UnknownReason,
    detail: &str,
) -> Result<Option<CompletionAck>> {
    let p = pos(position)?;
    let state: Option<String> = tx
        .query_row("SELECT state FROM runs WHERE run_id = ?1", [run_id], |r| {
            r.get(0)
        })
        .optional()?;
    let row: Option<(Option<String>, String, Option<String>, bool)> = tx
        .query_row(
            "SELECT handle, dispatch, settlement, EXISTS \
             (SELECT 1 FROM mailbox m WHERE m.run_id = journal.run_id AND m.position = journal.position) \
             FROM journal WHERE run_id = ?1 AND position = ?2",
            params![run_id, p],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (Some(state), Some((row_handle, dispatch, settlement, waiting))) = (state, row) else {
        return Ok(Some(CompletionAck::Quarantined));
    };
    if row_handle.as_deref().is_some_and(|h| h != handle) || settlement.is_some() || waiting {
        return Ok(Some(CompletionAck::Quarantined));
    }
    match dispatch.as_str() {
        "unknown" => return Ok(Some(CompletionAck::Duplicate)),
        "sent" | "accepted" => {}
        _ => return Ok(Some(CompletionAck::Quarantined)),
    }
    match state.as_str() {
        "running" => return Ok(None),
        "cancelled" => return Ok(Some(CompletionAck::Refused)),
        _ => {}
    }
    tx.execute(
        "UPDATE journal SET dispatch = 'unknown', unknown_reason = ?3 \
         WHERE run_id = ?1 AND position = ?2",
        params![run_id, p, reason.as_str()],
    )?;
    tx.execute(
        "UPDATE runs SET state = 'needs_reconcile', awaited = NULL, \
         error_kind = 'unknown_outcome', error_detail = ?2 \
         WHERE run_id = ?1 AND state IN ('pending', 'suspended')",
        params![run_id, detail],
    )?;
    // Another call of the run may already have put it in needs_reconcile;
    // keep that call's detail too.
    tx.execute(
        "UPDATE runs SET error_detail = CASE WHEN error_detail IS NULL THEN ?2 \
         ELSE error_detail || '; ' || ?2 END, error_kind = 'unknown_outcome' \
         WHERE run_id = ?1 AND state = 'needs_reconcile' \
         AND (error_detail IS NULL OR instr(error_detail, ?2) = 0)",
        params![run_id, detail],
    )?;
    tx.execute(
        "UPDATE runs SET readiness = readiness + 1 WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(Some(CompletionAck::Accepted))
}

/// Why the call at (run, position) was last recorded unknown, if it ever
/// was.
pub fn unknown_reason(
    conn: &Connection,
    run_id: &str,
    position: u64,
) -> Result<Option<UnknownReason>> {
    let reason: Option<String> = conn
        .query_row(
            "SELECT unknown_reason FROM journal WHERE run_id = ?1 AND position = ?2",
            params![run_id, pos(position)?],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    reason
        .map(|r| {
            UnknownReason::parse(&r)
                .ok_or_else(|| CoreError::Corrupt(format!("unknown reason {r:?}")))
        })
        .transpose()
}

/// Positions whose send ended in an unknown state and that have no outcome.
pub fn unknown_positions(conn: &Connection, run_id: &str) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT position FROM journal WHERE run_id = ?1 AND dispatch = 'unknown' \
         AND settlement IS NULL AND NOT EXISTS \
         (SELECT 1 FROM mailbox m WHERE m.run_id = journal.run_id AND m.position = journal.position) \
         ORDER BY position",
    )?;
    let positions = stmt
        .query_map([run_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    positions
        .into_iter()
        .map(|p| to_u64(p, "position"))
        .collect()
}

/// Positions the host accepted as long-running that have no released
/// outcome yet.
pub fn accepted_positions(conn: &Connection, run_id: &str) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT position FROM journal WHERE run_id = ?1 AND dispatch IN ('accepted', 'deferred') \
         AND settlement IS NULL ORDER BY position",
    )?;
    let positions = stmt
        .query_map([run_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    positions
        .into_iter()
        .map(|p| to_u64(p, "position"))
        .collect()
}

/// Positions with an outcome waiting in the mailbox.
pub fn mailbox_positions(conn: &Connection, run_id: &str) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare("SELECT position FROM mailbox WHERE run_id = ?1 ORDER BY seq")?;
    let positions = stmt
        .query_map([run_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    positions
        .into_iter()
        .map(|p| to_u64(p, "position"))
        .collect()
}

/// The quarantine entries for a run, as (position, reason).
pub fn quarantined(conn: &Connection, run_id: &str) -> Result<Vec<(u64, String)>> {
    let mut stmt =
        conn.prepare("SELECT position, reason FROM quarantine WHERE run_id = ?1 ORDER BY seq")?;
    let entries = stmt
        .query_map([run_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    entries
        .into_iter()
        .map(|(p, r)| Ok((to_u64(p, "position")?, r)))
        .collect()
}

/// Calls with no outcome anywhere: not released, not waiting in the
/// mailbox, and not answered by a refused completion of a cancelled run.
/// These are the run's open obligations.
pub fn unsettled_positions(conn: &Connection, run_id: &str) -> Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT j.position FROM journal j WHERE j.run_id = ?1 AND j.settlement IS NULL \
         AND NOT EXISTS (SELECT 1 FROM mailbox m WHERE m.run_id = j.run_id AND m.position = j.position) \
         AND NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.run_id = j.run_id \
             AND q.position = j.position AND q.reason = 'run_cancelled') \
         ORDER BY j.position",
    )?;
    let positions = stmt
        .query_map([run_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    positions
        .into_iter()
        .map(|p| to_u64(p, "position"))
        .collect()
}
