//! Staged SQLite control plane for the run-log path.
//!
//! No production constructor installs this schema or calls these APIs before
//! cutover. Mutations take a transaction so admission, terminal accounting and
//! callback settlement can share one [`crate::Store::write`] commit, which
//! checks the writer lease to reject writes from a replaced process.
//! These functions do no I/O outside SQLite and never acquire a run actor.

use std::collections::BTreeMap;

use basal_proto::CallKind;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::error::{CoreError, Result};
use crate::model::{self, RunState, StoredClass};
use crate::run_log::{Lsn, SegmentEnd, SegmentState};

/// Deliberately absent from [`crate::schema::MIGRATIONS`]. Only test setup
/// executes this DDL until the separately approved cutover migration.
pub const DDL: &str = include_str!("schema_run_log.sql");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub run_id: String,
    pub flow_id: String,
    pub flow_version: u32,
    pub code_hash: [u8; 32],
    pub snapshot_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochLsn {
    pub log_epoch: u64,
    pub lsn: Lsn,
}

/// Durable anchor for the first retained segment and every live replay start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcFloor {
    pub log_epoch: u64,
    pub first_retained: u64,
    pub predecessor: Option<SegmentEnd>,
    pub synced_lsn: Lsn,
    pub live_starts: BTreeMap<String, Lsn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub store_id: String,
    pub log_epoch: u64,
    pub log_hwm: EpochLsn,
    pub commit_seq: u64,
    pub gc_floor: GcFloor,
    /// Resume marker for the future production migration; phase names will be
    /// defined alongside that migration, not by this schema layer.
    pub migration_phase: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveRun {
    pub run_id: String,
    pub flow_id: String,
    pub flow_version: u32,
    pub trigger_id: String,
    pub trigger_kind: String,
    pub trigger: Value,
    pub attempt: u32,
    pub admitted_at: i64,
    pub admit_seq: i64,
    pub admit_lsn: EpochLsn,
    pub deadline_ms: i64,
    pub self_input: Value,
    pub params_input: Value,
    pub grant_guards: Value,
    pub template_incarnation: Option<i64>,
    pub end_instance: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunHistory {
    pub run_id: String,
    pub flow_id: String,
    pub flow_version: u32,
    pub trigger_id: String,
    pub trigger_kind: String,
    pub trigger: Value,
    pub admitted_at: i64,
    pub admit_seq: i64,
    pub first_claim_at: Option<i64>,
    pub ended_at: i64,
    pub state: RunState,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub result: Option<Value>,
    pub result_size: u64,
    /// JSON object with `class` and `outcome` maps from class/outcome names
    /// to counts. Kept as compound summary data rather than per-call rows.
    pub call_counts: Value,
    pub writes_sent: u64,
    pub tokens_charged: u64,
    pub cost_microusd: u64,
    pub broken: u32,
    pub activation_count: u64,
    pub script_started: bool,
    pub phase: String,
    pub deadline_exempt: bool,
    pub history_lost: bool,
    pub commit_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObligationState {
    Open,
    Unknown,
    Settled,
}

impl ObligationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Unknown => "unknown",
            Self::Settled => "settled",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "unknown" => Ok(Self::Unknown),
            "settled" => Ok(Self::Settled),
            _ => Err(CoreError::Corrupt(format!("obligation state {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    Value([u8; 32]),
    Unknown,
}

// Settled calls keep only duplicate/conflict classification metadata. Their
// position is the map key; open/unknown obligations retain full call detail.
#[derive(Serialize, Deserialize)]
struct SavedReceipt {
    handle: Option<String>,
    audit_at: i64,
    value_hash: Option<[u8; 32]>,
    settled_at: i64,
}

fn saved_receipts(conn: &Connection, run_id: &str) -> Result<Option<BTreeMap<u64, SavedReceipt>>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT receipts FROM run_history WHERE run_id=?1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?;
    text.map(decode).transpose()
}

fn save_receipt(tx: &Transaction<'_>, row: &Obligation) -> Result<()> {
    let settled_at = row
        .settled_at
        .ok_or_else(|| CoreError::Invalid("incomplete receipt".into()))?;
    let value_hash = match row.receipt {
        Some(Receipt::Value(hash)) => Some(hash),
        Some(Receipt::Unknown) => None,
        None => return Err(CoreError::Invalid("incomplete receipt".into())),
    };
    let mut saved =
        saved_receipts(tx, &row.run_id)?.ok_or_else(|| CoreError::NoSuchRun(row.run_id.clone()))?;
    if saved.contains_key(&row.position) {
        return Err(CoreError::Invalid("duplicate receipt".into()));
    }
    saved.insert(
        row.position,
        SavedReceipt {
            handle: row.handle.clone(),
            audit_at: row.audit_at,
            value_hash,
            settled_at,
        },
    );
    tx.execute(
        "UPDATE run_history SET receipts=?2 WHERE run_id=?1",
        params![row.run_id, encode(&saved)?],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Obligation {
    pub run_id: String,
    pub position: u64,
    pub handle: Option<String>,
    pub state: ObligationState,
    pub audit_at: i64,
    /// Frozen request, reservation, accounting window, usage and cost.
    pub detail: Value,
    pub receipt: Option<Receipt>,
    pub settled_at: Option<i64>,
}

/// Storage decisions only; the host integration owns acknowledgement and
/// diagnostic emission, and must wait for the enclosing transaction's commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleResult {
    Settled,
    Duplicate,
    Conflict,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    Pending,
    Fulfilled,
    Rejected,
    Unknown,
}

impl CallOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fulfilled => "fulfilled",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "fulfilled" => Ok(Self::Fulfilled),
            "rejected" => Ok(Self::Rejected),
            "unknown" => Ok(Self::Unknown),
            _ => Err(CoreError::Corrupt(format!("call outcome {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunCall {
    pub run_id: String,
    pub position: u64,
    pub kind: CallKind,
    pub class: StoredClass,
    pub args_digest: [u8; 32],
    pub outcome: CallOutcome,
    pub error_code: Option<String>,
    pub issued_at: i64,
    pub arrived_at: Option<i64>,
    pub released_at: Option<i64>,
    /// Full metadata and payloads: args, frozen request, value, handle,
    /// refusal detail, usage, cost, and replay/dispatch fields.
    pub detail: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub log_epoch: u64,
    pub seq: u64,
    pub state: SegmentState,
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| CoreError::Invalid(e.to_string()))
}

fn decode<T: DeserializeOwned>(text: String) -> Result<T> {
    serde_json::from_str(&text).map_err(|e| CoreError::Corrupt(e.to_string()))
}

fn number(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| CoreError::Invalid("SQLite integer overflow".into()))
}

fn unsigned(row: &Row<'_>, column: &str) -> Result<u64> {
    u64::try_from(row.get::<_, i64>(column)?)
        .map_err(|_| CoreError::Corrupt(format!("negative {column}")))
}

fn one<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    read: impl FnOnce(&Row<'_>) -> Result<T>,
) -> Result<Option<T>> {
    conn.query_row(sql, params, |r| Ok(read(r)))
        .optional()?
        .transpose()
}

fn blob32(row: &Row<'_>, column: &str) -> Result<[u8; 32]> {
    row.get::<_, Vec<u8>>(column)?
        .try_into()
        .map_err(|_| CoreError::Corrupt(format!("invalid {column} length")))
}

fn validate_identity(tx: &Transaction<'_>, identity: &RunIdentity) -> Result<()> {
    let matches: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_snapshots WHERE snapshot_id=?1
            AND flow_id=?2 AND flow_version=?3 AND code_hash=?4)",
        params![
            identity.snapshot_id,
            identity.flow_id,
            identity.flow_version,
            identity.code_hash.as_slice()
        ],
        |r| r.get(0),
    )?;
    if !matches {
        return Err(CoreError::Invalid(
            "run identity does not match snapshot".into(),
        ));
    }
    Ok(())
}

pub fn identity(conn: &Connection, run_id: &str) -> Result<Option<RunIdentity>> {
    one(
        conn,
        "SELECT * FROM run_identities WHERE run_id=?1",
        [run_id],
        |r| {
            Ok(RunIdentity {
                run_id: r.get("run_id")?,
                flow_id: r.get("flow_id")?,
                flow_version: r.get("flow_version")?,
                code_hash: blob32(r, "code_hash")?,
                snapshot_id: r.get("snapshot_id")?,
            })
        },
    )
}

pub fn insert_live(tx: &Transaction<'_>, run: &LiveRun, identity: &RunIdentity) -> Result<()> {
    validate_identity(tx, identity)?;
    if (run.run_id.as_str(), run.flow_id.as_str(), run.flow_version)
        != (
            identity.run_id.as_str(),
            identity.flow_id.as_str(),
            identity.flow_version,
        )
        || history(tx, &run.run_id)?.is_some()
    {
        return Err(CoreError::Invalid("conflicting run identity".into()));
    }
    tx.execute(
        "INSERT INTO live_runs(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,
            attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,
            self_input,params_input,grant_guards,template_incarnation,end_instance,snapshot_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
        params![
            run.run_id,
            run.flow_id,
            run.flow_version,
            run.trigger_id,
            run.trigger_kind,
            encode(&run.trigger)?,
            run.attempt,
            run.admitted_at,
            run.admit_seq,
            number(run.admit_lsn.log_epoch)?,
            number(run.admit_lsn.lsn.sequence)?,
            number(run.admit_lsn.lsn.offset)?,
            run.deadline_ms,
            encode(&run.self_input)?,
            encode(&run.params_input)?,
            encode(&run.grant_guards)?,
            run.template_incarnation,
            run.end_instance,
            identity.snapshot_id
        ],
    )?;
    Ok(())
}

pub fn live(conn: &Connection, run_id: &str) -> Result<Option<LiveRun>> {
    one(
        conn,
        "SELECT * FROM live_runs WHERE run_id=?1",
        [run_id],
        |r| {
            Ok(LiveRun {
                run_id: r.get("run_id")?,
                flow_id: r.get("flow_id")?,
                flow_version: r.get("flow_version")?,
                trigger_id: r.get("trigger_id")?,
                trigger_kind: r.get("trigger_kind")?,
                trigger: decode(r.get("trigger")?)?,
                attempt: r.get("attempt")?,
                admitted_at: r.get("admitted_at")?,
                admit_seq: r.get("admit_seq")?,
                admit_lsn: EpochLsn {
                    log_epoch: unsigned(r, "admit_epoch")?,
                    lsn: Lsn {
                        sequence: unsigned(r, "admit_segment")?,
                        offset: unsigned(r, "admit_offset")?,
                    },
                },
                deadline_ms: r.get("deadline_ms")?,
                self_input: decode(r.get("self_input")?)?,
                params_input: decode(r.get("params_input")?)?,
                grant_guards: decode(r.get("grant_guards")?)?,
                template_incarnation: r.get("template_incarnation")?,
                end_instance: r.get("end_instance")?,
            })
        },
    )
}

pub fn insert_history(
    tx: &Transaction<'_>,
    run: &RunHistory,
    identity: &RunIdentity,
) -> Result<()> {
    validate_identity(tx, identity)?;
    if (run.run_id.as_str(), run.flow_id.as_str(), run.flow_version)
        != (
            identity.run_id.as_str(),
            identity.flow_id.as_str(),
            identity.flow_version,
        )
    {
        return Err(CoreError::Invalid("conflicting run identity".into()));
    }
    tx.execute(
        "INSERT INTO run_history(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,
            admitted_at,admit_seq,first_claim_at,ended_at,state,error_kind,error_detail,result,
            result_size,call_counts,writes_sent,tokens_charged,cost_microusd,broken,
            activation_count,script_started,phase,deadline_exempt,history_lost,commit_seq,snapshot_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,
            ?20,?21,?22,?23,?24,?25,?26,?27)",
        params![
            run.run_id,
            run.flow_id,
            run.flow_version,
            run.trigger_id,
            run.trigger_kind,
            encode(&run.trigger)?,
            run.admitted_at,
            run.admit_seq,
            run.first_claim_at,
            run.ended_at,
            run.state.as_str(),
            run.error_kind,
            run.error_detail,
            run.result.as_ref().map(encode).transpose()?,
            number(run.result_size)?,
            encode(&run.call_counts)?,
            number(run.writes_sent)?,
            number(run.tokens_charged)?,
            number(run.cost_microusd)?,
            run.broken,
            number(run.activation_count)?,
            run.script_started,
            run.phase,
            run.deadline_exempt,
            run.history_lost,
            number(run.commit_seq)?,
            identity.snapshot_id
        ],
    )?;
    Ok(())
}

pub fn history(conn: &Connection, run_id: &str) -> Result<Option<RunHistory>> {
    one(
        conn,
        "SELECT * FROM run_history WHERE run_id=?1",
        [run_id],
        |r| {
            Ok(RunHistory {
                run_id: r.get("run_id")?,
                flow_id: r.get("flow_id")?,
                flow_version: r.get("flow_version")?,
                trigger_id: r.get("trigger_id")?,
                trigger_kind: r.get("trigger_kind")?,
                trigger: decode(r.get("trigger")?)?,
                admitted_at: r.get("admitted_at")?,
                admit_seq: r.get("admit_seq")?,
                first_claim_at: r.get("first_claim_at")?,
                ended_at: r.get("ended_at")?,
                state: RunState::parse(&r.get::<_, String>("state")?)?,
                error_kind: r.get("error_kind")?,
                error_detail: r.get("error_detail")?,
                result: r
                    .get::<_, Option<String>>("result")?
                    .map(decode)
                    .transpose()?,
                result_size: unsigned(r, "result_size")?,
                call_counts: decode(r.get("call_counts")?)?,
                writes_sent: unsigned(r, "writes_sent")?,
                tokens_charged: unsigned(r, "tokens_charged")?,
                cost_microusd: unsigned(r, "cost_microusd")?,
                broken: r.get("broken")?,
                activation_count: unsigned(r, "activation_count")?,
                script_started: r.get("script_started")?,
                phase: r.get("phase")?,
                deadline_exempt: r.get("deadline_exempt")?,
                history_lost: r.get("history_lost")?,
                commit_seq: unsigned(r, "commit_seq")?,
            })
        },
    )
}

/// The caller adds accounting, calls, obligations and tombstones in this same
/// transaction. There is no separate SQLite update for live execution state.
pub fn finish_live(tx: &Transaction<'_>, run: &RunHistory) -> Result<()> {
    if live(tx, &run.run_id)?.is_none() {
        return Err(CoreError::NoSuchRun(run.run_id.clone()));
    }
    let identity =
        identity(tx, &run.run_id)?.ok_or_else(|| CoreError::NoSuchRun(run.run_id.clone()))?;
    insert_history(tx, run, &identity)?;
    tx.execute("DELETE FROM live_runs WHERE run_id=?1", [&run.run_id])?;
    Ok(())
}

/// Retention must also check open decisions, unacknowledged Broca calls and
/// the refusal outbox in the same transaction. Cascades remove calls and
/// receipts only with the history row.
pub fn prune_history(tx: &Transaction<'_>, run_id: &str) -> Result<bool> {
    let deleted = tx.execute(
        "DELETE FROM run_history WHERE run_id=?1
         AND NOT EXISTS(SELECT 1 FROM obligations WHERE run_id=?1 AND state<>'settled')",
        [run_id],
    )?;
    if deleted == 0 {
        return Ok(false);
    }
    Ok(true)
}

pub fn insert_obligation(tx: &Transaction<'_>, row: &Obligation) -> Result<()> {
    if obligation(tx, &row.run_id, row.position)?.is_some() {
        return Err(CoreError::Invalid("duplicate obligation".into()));
    }
    if row.state == ObligationState::Settled {
        return save_receipt(tx, row);
    }
    let hash = match row.receipt {
        Some(Receipt::Value(hash)) => Some(hash),
        _ => None,
    };
    tx.execute(
        "INSERT INTO obligations(run_id,position,handle,state,audit_at,detail,
            value_hash,receipt_unknown,settled_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            row.run_id,
            number(row.position)?,
            row.handle,
            row.state.as_str(),
            row.audit_at,
            encode(&row.detail)?,
            hash.as_ref().map(|h| h.as_slice()),
            row.receipt == Some(Receipt::Unknown),
            row.settled_at
        ],
    )?;
    Ok(())
}

pub fn obligation(conn: &Connection, run_id: &str, position: u64) -> Result<Option<Obligation>> {
    let row = one(
        conn,
        "SELECT * FROM obligations WHERE run_id=?1 AND position=?2",
        params![run_id, number(position)?],
        |r| {
            let hash: Option<Vec<u8>> = r.get("value_hash")?;
            let receipt = if r.get::<_, bool>("receipt_unknown")? {
                Some(Receipt::Unknown)
            } else {
                hash.map(|h| {
                    h.try_into()
                        .map(Receipt::Value)
                        .map_err(|_| CoreError::Corrupt("invalid receipt hash length".into()))
                })
                .transpose()?
            };
            Ok(Obligation {
                run_id: r.get("run_id")?,
                position: unsigned(r, "position")?,
                handle: r.get("handle")?,
                state: ObligationState::parse(&r.get::<_, String>("state")?)?,
                audit_at: r.get("audit_at")?,
                detail: decode(r.get("detail")?)?,
                receipt,
                settled_at: r.get("settled_at")?,
            })
        },
    )?;
    if row.is_some() {
        return Ok(row);
    }
    Ok(saved_receipts(conn, run_id)?
        .and_then(|mut saved| saved.remove(&position))
        .map(|receipt| Obligation {
            run_id: run_id.into(),
            position,
            handle: receipt.handle,
            state: ObligationState::Settled,
            audit_at: receipt.audit_at,
            detail: Value::Null,
            receipt: Some(receipt.value_hash.map_or(Receipt::Unknown, Receipt::Value)),
            settled_at: Some(receipt.settled_at),
        }))
}

pub fn mark_obligation_unknown(tx: &Transaction<'_>, run_id: &str, position: u64) -> Result<bool> {
    Ok(tx.execute(
        "UPDATE obligations SET state='unknown'
        WHERE run_id=?1 AND position=?2 AND state='open'",
        params![run_id, number(position)?],
    )? != 0)
}

/// Keep the settled receipt until history pruning. Redelivery is read-only:
/// an unknown notification cannot erase an already established value.
pub fn settle_obligation(
    tx: &Transaction<'_>,
    run_id: &str,
    position: u64,
    handle: Option<&str>,
    receipt: Receipt,
    settled_at: i64,
) -> Result<SettleResult> {
    let Some(mut row) = obligation(tx, run_id, position)? else {
        return Ok(SettleResult::Missing);
    };
    if row.handle.as_deref() != handle {
        return Ok(SettleResult::Conflict);
    }
    if row.state == ObligationState::Settled {
        return Ok(
            if row.receipt == Some(receipt) || receipt == Receipt::Unknown {
                SettleResult::Duplicate
            } else {
                SettleResult::Conflict
            },
        );
    }
    row.state = ObligationState::Settled;
    row.receipt = Some(receipt);
    row.settled_at = Some(settled_at);
    save_receipt(tx, &row)?;
    tx.execute(
        "DELETE FROM obligations WHERE run_id=?1 AND position=?2",
        params![run_id, number(position)?],
    )?;
    Ok(SettleResult::Settled)
}

pub fn insert_call(tx: &Transaction<'_>, call: &RunCall) -> Result<()> {
    let (kind, module, op) = model::kind_columns(&call.kind)?;
    let inserted = tx.execute(
        "INSERT INTO run_calls(run_id,position,kind,class,module,op,args_digest,outcome,
            error_code,issued_at,arrived_at,released_at,detail)
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13
         WHERE EXISTS(SELECT 1 FROM run_history WHERE run_id=?1
            AND state IN ('failed','engine_mismatch','cancelled') AND history_lost=0)",
        params![
            call.run_id,
            number(call.position)?,
            kind,
            call.class.as_str(),
            module,
            op,
            call.args_digest.as_slice(),
            call.outcome.as_str(),
            call.error_code,
            call.issued_at,
            call.arrived_at,
            call.released_at,
            encode(&call.detail)?
        ],
    )?;
    if inserted == 0 {
        return Err(CoreError::Invalid(
            "call detail requires non-successful, available history".into(),
        ));
    }
    Ok(())
}

pub fn call(conn: &Connection, run_id: &str, position: u64) -> Result<Option<RunCall>> {
    one(
        conn,
        "SELECT * FROM run_calls WHERE run_id=?1 AND position=?2",
        params![run_id, number(position)?],
        |r| {
            Ok(RunCall {
                run_id: r.get("run_id")?,
                position: unsigned(r, "position")?,
                kind: model::kind_from_columns(r.get("kind")?, r.get("module")?, r.get("op")?)?,
                class: StoredClass::parse(&r.get::<_, String>("class")?)?,
                args_digest: blob32(r, "args_digest")?,
                outcome: CallOutcome::parse(&r.get::<_, String>("outcome")?)?,
                error_code: r.get("error_code")?,
                issued_at: r.get("issued_at")?,
                arrived_at: r.get("arrived_at")?,
                released_at: r.get("released_at")?,
                detail: decode(r.get("detail")?)?,
            })
        },
    )
}

pub fn reserve_segment(tx: &Transaction<'_>, log_epoch: u64, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO run_log_segments(log_epoch,seq,state) VALUES (?1,?2,'reserved')",
        params![number(log_epoch)?, number(seq)?],
    )?;
    Ok(())
}

pub fn segments(conn: &Connection, log_epoch: u64) -> Result<Vec<Segment>> {
    let mut stmt =
        conn.prepare("SELECT * FROM run_log_segments WHERE log_epoch=?1 ORDER BY seq")?;
    let mut rows = stmt.query([number(log_epoch)?])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let state: String = r.get("state")?;
        out.push(Segment {
            log_epoch: unsigned(r, "log_epoch")?,
            seq: unsigned(r, "seq")?,
            state: match state.as_str() {
                "reserved" => SegmentState::Reserved,
                "active" => SegmentState::Active,
                _ => return Err(CoreError::Corrupt(format!("segment state {state:?}"))),
            },
        });
    }
    Ok(out)
}

/// A [`crate::run_log::SegmentRegistry`] adapter must check the writer's
/// expected lease epoch before calling this, so a replaced writer cannot
/// promote a segment. Promotion and its high-water mark share one transaction.
pub fn promote_segment(tx: &Transaction<'_>, synced: EpochLsn) -> Result<bool> {
    let updated = tx.execute(
        "UPDATE run_log_segments SET state='active' WHERE log_epoch=?1 AND seq=?2",
        params![number(synced.log_epoch)?, number(synced.lsn.sequence)?],
    )?;
    if updated == 0 {
        return Ok(false);
    }
    advance_log_hwm(tx, synced)?;
    Ok(true)
}

/// The caller must advance `gc_floor` in the same transaction, before unlinking
/// files. This primitive never performs filesystem reclamation.
pub fn remove_segments_before(
    tx: &Transaction<'_>,
    log_epoch: u64,
    first_retained: u64,
) -> Result<usize> {
    Ok(tx.execute(
        "DELETE FROM run_log_segments WHERE log_epoch=?1 AND seq<?2",
        params![number(log_epoch)?, number(first_retained)?],
    )?)
}

fn meta_value(conn: &Connection, key: &str) -> Result<String> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .optional()?
        .ok_or_else(|| CoreError::Corrupt(format!("missing meta key {key}")))
}

fn set_meta_value(tx: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO meta(key,value) VALUES (?1,?2)
        ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn meta(conn: &Connection) -> Result<Meta> {
    Ok(Meta {
        store_id: meta_value(conn, "store_id")?,
        log_epoch: decode(meta_value(conn, "log_epoch")?)?,
        log_hwm: decode(meta_value(conn, "log_hwm")?)?,
        commit_seq: decode(meta_value(conn, "commit_seq")?)?,
        gc_floor: decode(meta_value(conn, "gc_floor")?)?,
        migration_phase: meta_value(conn, "migration_phase")?,
    })
}

/// Whole-state write for schema setup and epoch reinitialization. Ordinary
/// transactions use the monotonic watermark and commit-sequence helpers.
pub fn write_meta(tx: &Transaction<'_>, state: &Meta) -> Result<()> {
    if state.log_epoch != state.log_hwm.log_epoch || state.log_epoch != state.gc_floor.log_epoch {
        return Err(CoreError::Invalid("metadata mixes log epochs".into()));
    }
    for (key, value) in [
        ("store_id", state.store_id.clone()),
        ("log_epoch", encode(&state.log_epoch)?),
        ("log_hwm", encode(&state.log_hwm)?),
        ("commit_seq", encode(&state.commit_seq)?),
        ("gc_floor", encode(&state.gc_floor)?),
        ("migration_phase", state.migration_phase.clone()),
    ] {
        set_meta_value(tx, key, &value)?;
    }
    Ok(())
}

pub fn restore_pending(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key='restore_pending'",
            [],
            |r| r.get::<_, String>(0),
        )?
        .as_str()
        == "1")
}

pub fn set_restore_pending(tx: &Transaction<'_>, pending: bool) -> Result<()> {
    set_meta_value(tx, "restore_pending", if pending { "1" } else { "0" })
}

pub fn history_lost(conn: &Connection, run_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT history_lost FROM live_runs WHERE run_id=?1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

pub fn advance_log_hwm(tx: &Transaction<'_>, synced: EpochLsn) -> Result<()> {
    let epoch: u64 = decode(meta_value(tx, "log_epoch")?)?;
    let old: EpochLsn = decode(meta_value(tx, "log_hwm")?)?;
    if synced.log_epoch != epoch || old.log_epoch != epoch {
        return Err(CoreError::Invalid(
            "log watermark from a different epoch".into(),
        ));
    }
    if synced.lsn > old.lsn {
        set_meta_value(tx, "log_hwm", &encode(&synced)?)?;
    }
    Ok(())
}

/// Transactions that finish runs or settle late completions use this wrapper
/// to include the published log watermark without waiting for log I/O.
pub fn write_synced<T>(
    store: &crate::Store,
    log: &dyn crate::run_admission::AdmissionLog,
    f: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<T> {
    store.write(|tx| {
        advance_log_hwm(tx, log.synced_lsn())?;
        let value = f(tx)?;
        advance_log_hwm(tx, log.synced_lsn())?;
        Ok(value)
    })
}

pub(crate) fn write_gc_floor(tx: &Transaction<'_>, floor: &GcFloor) -> Result<()> {
    advance_log_hwm(
        tx,
        EpochLsn {
            log_epoch: floor.log_epoch,
            lsn: floor.synced_lsn,
        },
    )?;
    set_meta_value(tx, "gc_floor", &encode(floor)?)?;
    remove_segments_before(tx, floor.log_epoch, floor.first_retained)?;
    Ok(())
}

pub fn next_commit_seq(tx: &Transaction<'_>) -> Result<u64> {
    let old: u64 = decode(meta_value(tx, "commit_seq")?)?;
    let next = old
        .checked_add(1)
        .ok_or_else(|| CoreError::Invalid("commit sequence overflow".into()))?;
    set_meta_value(tx, "commit_seq", &encode(&next)?)?;
    Ok(next)
}

/// Disposable test stores may select explicit metadata. A raw SQLite fixture
/// also needs the control-plane schema; Store::open already installs it.
#[cfg(any(test, feature = "test-support"))]
pub fn create_test_schema(tx: &Transaction<'_>, state: &Meta) -> Result<()> {
    let installed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='live_runs')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        tx.execute_batch(DDL)?;
        crate::run_cutover::initialize_broca_fixture(tx)?;
    }
    write_meta(tx, state)?;
    set_restore_pending(tx, false)
}

#[cfg(test)]
#[path = "run_store_sql_tests.rs"]
mod tests;
