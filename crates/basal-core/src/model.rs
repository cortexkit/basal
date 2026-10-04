//! The run and journal records as Rust values, and their stored encodings.

use basal_proto::{
    ArgsDigest, CallKind, JsonText, Primitive, RecordedCall, RecordedOutcome, Settlement,
};
use rusqlite::Row;

use crate::error::{CoreError, Result};

/// A run's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    /// Admitted, or runnable again; waiting for an activation.
    Pending,
    /// Owned by exactly one activation.
    Running,
    /// Blocked only on long-running calls; no activation holds it.
    Suspended,
    Succeeded,
    Failed,
    /// Stopped on a call whose outcome cannot be proven; an operator must
    /// reconcile it.
    NeedsReconcile,
    /// The code or runtime fingerprint changed under it; it was not
    /// replayed.
    EngineMismatch,
    Cancelled,
}

impl RunState {
    pub const ALL: [RunState; 8] = [
        Self::Pending,
        Self::Running,
        Self::Suspended,
        Self::Succeeded,
        Self::Failed,
        Self::NeedsReconcile,
        Self::EngineMismatch,
        Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::NeedsReconcile => "needs_reconcile",
            Self::EngineMismatch => "engine_mismatch",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| CoreError::Corrupt(format!("run state {s:?}")))
    }

    /// Terminal states: no activation will ever drive the run again.
    /// `needs_reconcile` is not terminal; reconciling it can resume it.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::EngineMismatch | Self::Cancelled
        )
    }
}

/// How the runtime serves a call, fixed when the call is issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredClass {
    /// A clock read or random sample, answered at once.
    Sync,
    /// A local, eager effect committed with its outcome.
    Local,
    Query,
    /// A mutation whose op does not honour idempotency keys.
    Mutation,
    /// A mutation whose op honours idempotency keys.
    KeyedMutation,
}

impl StoredClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Local => "local",
            Self::Query => "query",
            Self::Mutation => "mutation",
            Self::KeyedMutation => "keyed_mutation",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "sync" => Self::Sync,
            "local" => Self::Local,
            "query" => Self::Query,
            "mutation" => Self::Mutation,
            "keyed_mutation" => Self::KeyedMutation,
            other => return Err(CoreError::Corrupt(format!("call class {other:?}"))),
        })
    }

    /// Whether sending the call again can never cause a second effect.
    pub fn safe_to_resend(self) -> bool {
        matches!(self, Self::Query | Self::KeyedMutation)
    }
}

/// Where a call stands with its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchState {
    /// Never sent to a host (synchronous and local calls).
    None,
    /// Authorized and possibly sent; no outcome or acceptance recorded.
    Sent,
    /// The host accepted it as long-running; it will complete later.
    Accepted,
    /// The send failed in a way that may or may not have reached the host.
    Unknown,
    /// An operator reconciled it as not applied; it may be sent again with
    /// the same key.
    NotApplied,
    /// A typed refusal proved no effect; retry only after its durable deadline.
    Deferred,
}

impl DispatchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sent => "sent",
            Self::Accepted => "accepted",
            Self::Unknown => "unknown",
            Self::NotApplied => "not_applied",
            Self::Deferred => "deferred",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "none" => Self::None,
            "sent" => Self::Sent,
            "accepted" => Self::Accepted,
            "unknown" => Self::Unknown,
            "not_applied" => Self::NotApplied,
            "deferred" => Self::Deferred,
            other => return Err(CoreError::Corrupt(format!("dispatch state {other:?}"))),
        })
    }
}

pub fn settlement_str(s: Settlement) -> &'static str {
    match s {
        Settlement::Fulfilled => "fulfilled",
        Settlement::Rejected => "rejected",
    }
}

pub fn parse_settlement(s: &str) -> Result<Settlement> {
    match s {
        "fulfilled" => Ok(Settlement::Fulfilled),
        "rejected" => Ok(Settlement::Rejected),
        other => Err(CoreError::Corrupt(format!("settlement {other:?}"))),
    }
}

/// The stored form of a call kind: code 0 with module and op names for a
/// module op, otherwise the primitive's wire code.
pub fn kind_columns(kind: &CallKind) -> (i64, Option<&str>, Option<&str>) {
    match kind {
        CallKind::Op { module, op } => (0, Some(module.as_str()), Some(op.as_str())),
        CallKind::Primitive(p) => (i64::from(p.code()), None, None),
    }
}

pub fn kind_from_columns(
    code: i64,
    module: Option<String>,
    op: Option<String>,
) -> Result<CallKind> {
    if code == 0 {
        return match (module, op) {
            (Some(module), Some(op)) => Ok(CallKind::Op { module, op }),
            _ => Err(CoreError::Corrupt("module op without names".into())),
        };
    }
    u8::try_from(code)
        .ok()
        .and_then(Primitive::from_code)
        .map(CallKind::Primitive)
        .ok_or_else(|| CoreError::Corrupt(format!("call kind code {code}")))
}

pub fn json(text: String, what: &str) -> Result<JsonText> {
    JsonText::new(text).map_err(|e| CoreError::Corrupt(format!("{what}: {e}")))
}

pub fn digest(bytes: Vec<u8>) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| CoreError::Corrupt(format!("digest of {} bytes", bytes.len())))
}

pub fn to_u64(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| CoreError::Corrupt(format!("{what} {v}")))
}

/// One journal row.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRow {
    pub position: u64,
    pub kind: CallKind,
    pub args: JsonText,
    pub args_digest: ArgsDigest,
    pub idempotency_key: String,
    pub class: StoredClass,
    pub dispatch: DispatchState,
    pub attempts: u32,
    pub handle: Option<String>,
    /// Present only with its delivery order: the journal never holds an
    /// outcome that was not released.
    pub outcome: Option<RecordedOutcome>,
    /// The journal stores the exact prepared request bytes (a model envelope
    /// or core sink/facts request) alongside the intent, so re-issues do not
    /// reconstruct the request from script arguments or a new clock sample.
    pub request: Option<JsonText>,
}

/// The columns [`CallRow::from_row`] expects, in order.
pub const CALL_COLUMNS: &str = "position, kind_code, module, op, args, args_digest, \
    idempotency_key, class, dispatch, attempts, handle, settlement, value, delivery_order, request";

impl CallRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<Self>> {
        let position: i64 = row.get(0)?;
        let code: i64 = row.get(1)?;
        let module: Option<String> = row.get(2)?;
        let op: Option<String> = row.get(3)?;
        let args: String = row.get(4)?;
        let args_digest: Vec<u8> = row.get(5)?;
        let key: String = row.get(6)?;
        let class: String = row.get(7)?;
        let dispatch: String = row.get(8)?;
        let attempts: i64 = row.get(9)?;
        let handle: Option<String> = row.get(10)?;
        let settlement: Option<String> = row.get(11)?;
        let value: Option<String> = row.get(12)?;
        let order: Option<i64> = row.get(13)?;
        let request: Option<String> = row.get(14)?;
        Ok((|| {
            let outcome = match (settlement, value, order) {
                (Some(s), Some(v), Some(o)) => Some(RecordedOutcome {
                    settlement: parse_settlement(&s)?,
                    value: json(v, "outcome value")?,
                    delivery_order: to_u64(o, "delivery order")?,
                }),
                (None, None, None) => None,
                _ => return Err(CoreError::Corrupt("partial outcome".into())),
            };
            Ok(CallRow {
                position: to_u64(position, "position")?,
                kind: kind_from_columns(code, module, op)?,
                args: json(args, "arguments")?,
                args_digest: ArgsDigest(digest(args_digest)?),
                idempotency_key: key,
                class: StoredClass::parse(&class)?,
                dispatch: DispatchState::parse(&dispatch)?,
                attempts: u32::try_from(attempts)
                    .map_err(|_| CoreError::Corrupt(format!("attempts {attempts}")))?,
                handle,
                outcome,
                request: request.map(|r| json(r, "request")).transpose()?,
            })
        })())
    }

    /// The bytes a send of this call carries.
    pub fn dispatch_args(&self) -> JsonText {
        self.request.clone().unwrap_or_else(|| self.args.clone())
    }

    pub fn recorded(&self) -> RecordedCall {
        RecordedCall {
            position: self.position,
            kind: self.kind.clone(),
            args_digest: self.args_digest,
            outcome: self.outcome.clone(),
        }
    }
}

/// A run as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub run_id: String,
    pub flow_id: String,
    pub trigger_id: String,
    pub attempt: u32,
    pub trigger: JsonText,
    pub script: String,
    pub manifest: String,
    pub code_hash: [u8; 32],
    pub fingerprint: Option<String>,
    pub state: RunState,
    pub owner: Option<String>,
    pub generation: u64,
    pub readiness: u64,
    pub awaited: Vec<u64>,
    pub result: Option<String>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
    pub broken: u32,
    pub admitted_at: i64,
    pub ended_at: Option<i64>,
    /// The approved version the run was admitted under.
    pub flow_version: Option<u32>,
    /// The run's place in its flow's trigger order.
    pub admit_seq: Option<i64>,
    /// The run's wall-clock budget, from its manifest or the default.
    pub deadline_ms: Option<i64>,
    /// The time (on the runtime's clock) after which the run fails for
    /// running too long, set when the run is first claimed.
    pub deadline_at: Option<i64>,
}

pub const RUN_COLUMNS: &str = "run_id, flow_id, trigger_id, attempt, trigger, script, manifest, \
    code_hash, fingerprint, state, owner, generation, readiness, awaited, result, error_kind, \
    error_detail, broken, admitted_at, ended_at, flow_version, admit_seq, deadline_ms, deadline_at";

impl Run {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<Self>> {
        let run_id: String = row.get(0)?;
        let flow_id: String = row.get(1)?;
        let trigger_id: String = row.get(2)?;
        let attempt: i64 = row.get(3)?;
        let trigger: String = row.get(4)?;
        let script: String = row.get(5)?;
        let manifest: String = row.get(6)?;
        let code_hash: Vec<u8> = row.get(7)?;
        let fingerprint: Option<String> = row.get(8)?;
        let state: String = row.get(9)?;
        let owner: Option<String> = row.get(10)?;
        let generation: i64 = row.get(11)?;
        let readiness: i64 = row.get(12)?;
        let awaited: Option<String> = row.get(13)?;
        let result: Option<String> = row.get(14)?;
        let error_kind: Option<String> = row.get(15)?;
        let error_detail: Option<String> = row.get(16)?;
        let broken: i64 = row.get(17)?;
        let admitted_at: i64 = row.get(18)?;
        let ended_at: Option<i64> = row.get(19)?;
        let flow_version: Option<i64> = row.get(20)?;
        let admit_seq: Option<i64> = row.get(21)?;
        let deadline_ms: Option<i64> = row.get(22)?;
        let deadline_at: Option<i64> = row.get(23)?;
        Ok((|| {
            let awaited = match awaited {
                None => Vec::new(),
                Some(text) => serde_json::from_str::<Vec<u64>>(&text)
                    .map_err(|e| CoreError::Corrupt(format!("awaited positions: {e}")))?,
            };
            Ok(Run {
                run_id,
                flow_id,
                trigger_id,
                attempt: u32::try_from(attempt)
                    .map_err(|_| CoreError::Corrupt(format!("attempt {attempt}")))?,
                trigger: json(trigger, "trigger")?,
                script,
                manifest,
                code_hash: digest(code_hash)?,
                fingerprint,
                state: RunState::parse(&state)?,
                owner,
                generation: to_u64(generation, "generation")?,
                readiness: to_u64(readiness, "readiness")?,
                awaited,
                result,
                error_kind,
                error_detail,
                broken: u32::try_from(broken)
                    .map_err(|_| CoreError::Corrupt(format!("broken count {broken}")))?,
                admitted_at,
                ended_at,
                flow_version: flow_version
                    .map(|v| {
                        u32::try_from(v)
                            .map_err(|_| CoreError::Corrupt(format!("flow version {v}")))
                    })
                    .transpose()?,
                admit_seq,
                deadline_ms,
                deadline_at,
            })
        })())
    }
}
