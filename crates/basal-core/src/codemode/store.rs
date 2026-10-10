//! The codemode run store: one row per run in `codemode_runs`, one row per
//! recorded tool call in `codemode_calls`, and a permanent tombstone per
//! pruned run id in `codemode_tombstones`.
//!
//! These functions are the only writers. Every write that could race
//! another is conditional and reports whether it took effect:
//!
//! - an outcome is recorded only on a call that is still `pending`, so a
//!   recorded outcome never changes, and an answer that arrives after its
//!   call was settled by termination is dropped;
//! - a terminal status is committed only on a run that is still `running`,
//!   so the first terminal write wins and later ones change nothing.
//!
//! The schema's triggers (see `schema::CODEMODE`) back each rule, so a
//! writer that forgot a condition fails loudly instead of overwriting.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::error::{CoreError, Result};
use crate::ids::idempotency_key;

/// The namespace in a codemode call's idempotency key, where a flow call
/// has its flow id.
pub const CALL_KEY_NAMESPACE: &str = "codemode";

/// The idempotency key sent with the codemode call at `position` of
/// `run_id`. The same call always carries the same key, and no flow call
/// shares it, because codemode keys use their own namespace.
pub fn call_key(run_id: &str, position: u64) -> String {
    idempotency_key(CALL_KEY_NAMESPACE, run_id, position)
}

/// The engine budget a run exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    JsCpu,
    Memory,
    Stack,
    Wall,
    ToolCalls,
}

/// A run's status. Every status but `Running` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Failed,
    BudgetExhausted(Budget),
    Cancelled,
    Interrupted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::BudgetExhausted(Budget::JsCpu) => "budget_exhausted:js_cpu",
            Self::BudgetExhausted(Budget::Memory) => "budget_exhausted:memory",
            Self::BudgetExhausted(Budget::Stack) => "budget_exhausted:stack",
            Self::BudgetExhausted(Budget::Wall) => "budget_exhausted:wall",
            Self::BudgetExhausted(Budget::ToolCalls) => "budget_exhausted:tool_calls",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "budget_exhausted:js_cpu" => Self::BudgetExhausted(Budget::JsCpu),
            "budget_exhausted:memory" => Self::BudgetExhausted(Budget::Memory),
            "budget_exhausted:stack" => Self::BudgetExhausted(Budget::Stack),
            "budget_exhausted:wall" => Self::BudgetExhausted(Budget::Wall),
            "budget_exhausted:tool_calls" => Self::BudgetExhausted(Budget::ToolCalls),
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

/// A call's outcome. `Pending` is the only one that may still change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Ok,
    Error,
    Refused,
    ConsentUnavailable,
    ToolUnavailable,
    OutcomeUnknown,
    Cancelled,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Refused => "refused",
            Self::ConsentUnavailable => "consent_unavailable",
            Self::ToolUnavailable => "tool_unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "ok" => Self::Ok,
            "error" => Self::Error,
            "refused" => Self::Refused,
            "consent_unavailable" => Self::ConsentUnavailable,
            "tool_unavailable" => Self::ToolUnavailable,
            "outcome_unknown" => Self::OutcomeUnknown,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// The code a call ends with when its run ended after the intent to send it
/// was committed but before an outcome was recorded.
pub const NO_OUTCOME: &str = "no_outcome";

/// A run as admitted, before anything has happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun<'a> {
    pub run_id: &'a str,
    pub agent_id: &'a str,
    pub program: &'a str,
    /// The catalog as admitted, as JSON array text.
    pub catalog: &'a str,
    pub catalog_digest: &'a str,
    pub description: Option<&'a str>,
    /// The admitted `limits`, as JSON object text.
    pub limits: &'a str,
    /// The admitted scope `{owner, ref, epoch}`, as JSON object text.
    pub scope: &'a str,
    pub deadline_ms: i64,
    pub admitted_at: i64,
}

/// `{code, message}` of a run that ended `failed`, `budget_exhausted:*` or
/// `interrupted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    pub code: String,
    pub message: String,
}

/// What a run ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub status: Status,
    /// The program's return value as JSON text, only when `Completed`.
    pub value: Option<String>,
    pub error: Option<RunError>,
    /// The kept `console.log` lines.
    pub output: String,
    /// The run's warnings, as JSON array text.
    pub warnings: String,
}

/// How a call row starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStart<'a> {
    /// Waiting for an in-flight slot; no intent, not sent.
    Queued,
    /// Combined intent and provider entry at `at`, for callers whose send
    /// boundary is immediate. Supervisors use `Prepared` for the separate steps.
    Intent { at: i64 },
    /// Durable intent before provider entry; duration has not started yet.
    Prepared { at: i64 },
    /// Settled by the parent without being sent (a refusal before send).
    Settled {
        outcome: Outcome,
        code: Option<&'a str>,
    },
}

/// A stored run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: String,
    pub agent_id: String,
    pub program: String,
    pub catalog: String,
    pub catalog_digest: String,
    pub description: Option<String>,
    pub limits: String,
    pub scope: String,
    pub deadline_ms: i64,
    pub status: Status,
    pub value: Option<String>,
    pub error: Option<RunError>,
    pub output: String,
    pub warnings: String,
    pub admitted_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// A stored call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub position: u64,
    pub tool: String,
    pub idempotency_key: String,
    pub input_bytes: u64,
    pub intent_at: Option<i64>,
    /// Runtime-clock provider entry, separate from the durable intent.
    pub entered_at: Option<i64>,
    pub outcome: Outcome,
    pub code: Option<String>,
    pub duration_ms: Option<i64>,
}

/// What a run id names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Never admitted.
    Unknown,
    /// Admitted, ended and pruned: the id is never admitted again.
    Pruned,
    Found(Box<RunRecord>),
}

fn sql_i64(n: u64) -> Result<i64> {
    i64::try_from(n).map_err(|_| CoreError::Invalid(format!("{n} is out of range")))
}

/// Inserts an admitted run, `running` when `ended` is `None` and otherwise
/// already terminal at `admitted_at` (a run admitted at or after its
/// deadline). Returns `false`, writing nothing, when the id is already
/// stored or was pruned.
pub fn insert_run(tx: &Transaction, run: &NewRun, ended: Option<&Terminal>) -> Result<bool> {
    let terminal = ended.filter(|t| t.status.is_terminal());
    if ended.is_some() && terminal.is_none() {
        return Err(CoreError::Invalid(
            "a run inserted as ended needs a terminal status".into(),
        ));
    }
    let status = terminal.map_or(Status::Running, |t| t.status);
    let inserted = tx.execute(
        "INSERT INTO codemode_runs (run_id, agent_id, program, catalog, catalog_digest, description, \
         limits, scope, deadline_ms, status, value, error_code, error_message, output, warnings, \
         admitted_at, ended_at, duration_ms) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18 \
         WHERE NOT EXISTS (SELECT 1 FROM codemode_tombstones WHERE run_id = ?1) \
         ON CONFLICT (run_id) DO NOTHING",
        params![
            run.run_id,
            run.agent_id,
            run.program,
            run.catalog,
            run.catalog_digest,
            run.description,
            run.limits,
            run.scope,
            run.deadline_ms,
            status.as_str(),
            terminal.and_then(|t| t.value.as_deref()),
            terminal.and_then(|t| t.error.as_ref().map(|e| e.code.as_str())),
            terminal.and_then(|t| t.error.as_ref().map(|e| e.message.as_str())),
            terminal.map_or("", |t| t.output.as_str()),
            terminal.map_or("[]", |t| t.warnings.as_str()),
            run.admitted_at,
            terminal.map(|_| run.admitted_at),
            terminal.map(|_| 0_i64),
        ],
    )?;
    Ok(inserted == 1)
}

const RUN_COLUMNS: &str = "run_id, agent_id, program, catalog, catalog_digest, description, limits, \
     scope, deadline_ms, status, value, error_code, error_message, output, warnings, admitted_at, \
     ended_at, duration_ms";

fn run_record(r: &Row) -> rusqlite::Result<(RunRecord, String)> {
    let code: Option<String> = r.get(11)?;
    let message: Option<String> = r.get(12)?;
    Ok((
        RunRecord {
            run_id: r.get(0)?,
            agent_id: r.get(1)?,
            program: r.get(2)?,
            catalog: r.get(3)?,
            catalog_digest: r.get(4)?,
            description: r.get(5)?,
            limits: r.get(6)?,
            scope: r.get(7)?,
            deadline_ms: r.get(8)?,
            status: Status::Running,
            value: r.get(10)?,
            error: code
                .zip(message)
                .map(|(code, message)| RunError { code, message }),
            output: r.get(13)?,
            warnings: r.get(14)?,
            admitted_at: r.get(15)?,
            ended_at: r.get(16)?,
            duration_ms: r.get(17)?,
        },
        r.get(9)?,
    ))
}

/// The run `run_id` names, if any.
pub fn lookup(conn: &Connection, run_id: &str) -> Result<Lookup> {
    let row = conn
        .query_row(
            &format!("SELECT {RUN_COLUMNS} FROM codemode_runs WHERE run_id = ?1"),
            [run_id],
            run_record,
        )
        .optional()?;
    if let Some((mut record, status)) = row {
        record.status = Status::parse(&status)
            .ok_or_else(|| CoreError::Corrupt(format!("codemode run status {status:?}")))?;
        return Ok(Lookup::Found(Box::new(record)));
    }
    let pruned: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM codemode_tombstones WHERE run_id = ?1)",
        [run_id],
        |r| r.get(0),
    )?;
    Ok(if pruned {
        Lookup::Pruned
    } else {
        Lookup::Unknown
    })
}

/// The run's calls in position order.
pub fn calls(conn: &Connection, run_id: &str) -> Result<Vec<CallRecord>> {
    let mut stmt = conn.prepare(
        "SELECT position, tool, idempotency_key, input_bytes, intent_at, outcome, code, duration_ms, entered_at \
         FROM codemode_calls WHERE run_id = ?1 ORDER BY position",
    )?;
    let rows = stmt
        .query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(
            |(
                position,
                tool,
                key,
                input_bytes,
                intent_at,
                outcome,
                code,
                duration_ms,
                entered_at,
            )| {
                Ok(CallRecord {
                    position: u64::try_from(position)
                        .map_err(|_| CoreError::Corrupt(format!("call position {position}")))?,
                    tool,
                    idempotency_key: key,
                    input_bytes: u64::try_from(input_bytes)
                        .map_err(|_| CoreError::Corrupt(format!("input size {input_bytes}")))?,
                    intent_at,
                    entered_at,
                    outcome: Outcome::parse(&outcome)
                        .ok_or_else(|| CoreError::Corrupt(format!("call outcome {outcome:?}")))?,
                    code,
                    duration_ms,
                })
            },
        )
        .collect()
}

/// Running runs: `(of agent_id, in total)`. Admission counts both in the
/// transaction that inserts, against the per-agent and per-basal caps.
pub fn running_counts(conn: &Connection, agent_id: &str) -> Result<(u64, u64)> {
    let (agent, total): (i64, i64) = conn.query_row(
        "SELECT COUNT(CASE WHEN agent_id = ?1 THEN 1 END), COUNT(*) FROM codemode_runs \
         WHERE status = 'running'",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((agent.max(0) as u64, total.max(0) as u64))
}

/// Every run still `running`, oldest first. At startup these are the runs a
/// previous process left behind.
pub fn running_runs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT run_id FROM codemode_runs WHERE status = 'running' ORDER BY admitted_at, run_id",
    )?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Records a call at `position` of a running run, keyed by [`call_key`].
/// Returns `false`, writing nothing, when the run is not running (a call
/// the worker issued after termination began is not recorded).
pub fn insert_call(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    tool: &str,
    input_bytes: u64,
    start: CallStart,
) -> Result<bool> {
    let (intent_at, entered_at, outcome, code) = match start {
        CallStart::Queued => (None, None, Outcome::Pending, None),
        CallStart::Intent { at } => (Some(at), Some(at), Outcome::Pending, None),
        CallStart::Prepared { at } => (Some(at), None, Outcome::Pending, None),
        CallStart::Settled { outcome, code } => {
            if outcome == Outcome::Pending {
                return Err(CoreError::Invalid(
                    "a settled call needs an outcome other than pending".into(),
                ));
            }
            (None, None, outcome, code)
        }
    };
    let inserted = tx.execute(
        "INSERT INTO codemode_calls (run_id, position, tool, idempotency_key, input_bytes, \
         intent_at, entered_at, outcome, code) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9 \
         WHERE EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = ?1 AND status = 'running')",
        params![
            run_id,
            sql_i64(position)?,
            tool,
            call_key(run_id, position),
            sql_i64(input_bytes)?,
            intent_at,
            entered_at,
            outcome.as_str(),
            code,
        ],
    )?;
    Ok(inserted == 1)
}

/// Commits the intent to send a queued call, at `at`. Returns `false`,
/// changing nothing, unless the call exists, is still pending and has no
/// intent yet: a call termination already cancelled, or one whose intent was
/// already committed, is never sent again.
pub fn begin_queued(tx: &Transaction, run_id: &str, position: u64, at: i64) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE codemode_calls SET intent_at = ?3, entered_at = ?3 \
         WHERE run_id = ?1 AND position = ?2 AND outcome = 'pending' AND intent_at IS NULL",
        params![run_id, sql_i64(position)?, at],
    )?;
    Ok(changed == 1)
}

/// Records the durable intent without starting provider time. Unlike the
/// combined `begin_queued` transition, a supervisor may crash between this
/// transaction and entering the provider.
pub fn prepare_queued(tx: &Transaction, run_id: &str, position: u64, at: i64) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE codemode_calls SET intent_at = ?3 \
         WHERE run_id = ?1 AND position = ?2 AND outcome = 'pending' AND intent_at IS NULL \
         AND EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = ?1 AND status = 'running')",
        params![run_id, sql_i64(position)?, at],
    )?;
    Ok(changed == 1)
}

/// Starts provider time exactly once, only for a still-pending intent. A kill
/// that has already settled the call prevents a scheduled attempt from starting.
pub fn enter_call(tx: &Transaction, run_id: &str, position: u64, at: i64) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE codemode_calls SET entered_at = ?3 \
         WHERE run_id = ?1 AND position = ?2 AND outcome = 'pending' \
         AND intent_at IS NOT NULL AND entered_at IS NULL \
         AND EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = ?1 AND status = 'running')",
        params![run_id, sql_i64(position)?, at],
    )?;
    Ok(changed == 1)
}

/// Records a call's outcome at `at`, only if the call is still pending.
/// Returns `false`, changing nothing, when an outcome was already recorded,
/// including the one termination records for a call it settled: a late
/// answer never replaces it. Duration runs from provider entry to the recorded
/// outcome at `at`; a call never entered has none.
pub fn record_outcome(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    outcome: Outcome,
    code: Option<&str>,
    at: i64,
) -> Result<bool> {
    if outcome == Outcome::Pending {
        return Err(CoreError::Invalid(
            "an outcome other than pending is recorded".into(),
        ));
    }
    let changed = tx.execute(
        "UPDATE codemode_calls SET outcome = ?3, code = ?4, \
         duration_ms = CASE WHEN entered_at IS NULL THEN NULL ELSE max(?5 - entered_at, 0) END \
         WHERE run_id = ?1 AND position = ?2 AND outcome = 'pending'",
        params![run_id, sql_i64(position)?, outcome.as_str(), code, at],
    )?;
    Ok(changed == 1)
}

/// Settles every pending call of a running run, as termination does before
/// it releases the scope: a queued call becomes `cancelled`, and a call whose
/// intent was committed becomes `outcome_unknown` / `no_outcome`, its
/// duration running to `at`. Recorded outcomes are left as they are.
/// Returns how many calls were cancelled and how many marked unknown.
pub fn settle_pending_calls(tx: &Transaction, run_id: &str, at: i64) -> Result<(usize, usize)> {
    let running: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = ?1 AND status = 'running')",
        [run_id],
        |r| r.get(0),
    )?;
    if !running {
        return Ok((0, 0));
    }
    let cancelled = tx.execute(
        "UPDATE codemode_calls SET outcome = 'cancelled' \
         WHERE run_id = ?1 AND outcome = 'pending' AND intent_at IS NULL",
        [run_id],
    )?;
    let unknown = tx.execute(
        "UPDATE codemode_calls SET outcome = 'outcome_unknown', code = ?2, \
          duration_ms = CASE WHEN entered_at IS NULL THEN NULL ELSE max(?3 - entered_at, 0) END \
         WHERE run_id = ?1 AND outcome = 'pending' AND intent_at IS NOT NULL",
        params![run_id, NO_OUTCOME, at],
    )?;
    Ok((cancelled, unknown))
}

/// Commits a running run's terminal status at `at`, settling any call still
/// pending first. Returns `false`, changing nothing, when the run is not
/// running: whichever terminal write commits first is the run's end.
pub fn commit_terminal(
    tx: &Transaction,
    run_id: &str,
    terminal: &Terminal,
    at: i64,
) -> Result<bool> {
    if !terminal.status.is_terminal() {
        return Err(CoreError::Invalid(
            "running is not a terminal status".into(),
        ));
    }
    settle_pending_calls(tx, run_id, at)?;
    let changed = tx.execute(
        "UPDATE codemode_runs SET status = ?2, value = ?3, error_code = ?4, error_message = ?5, \
         output = ?6, warnings = ?7, ended_at = ?8, duration_ms = max(?8 - admitted_at, 0) \
         WHERE run_id = ?1 AND status = 'running'",
        params![
            run_id,
            terminal.status.as_str(),
            terminal.value,
            terminal.error.as_ref().map(|e| e.code.as_str()),
            terminal.error.as_ref().map(|e| e.message.as_str()),
            terminal.output,
            terminal.warnings,
            at,
        ],
    )?;
    Ok(changed == 1)
}

#[cfg(test)]
#[path = "store_tests.rs"]
pub(crate) mod tests;
