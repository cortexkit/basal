//! Runs and their state machine.
//!
//! A run is driven by at most one activation at a time. The activation is
//! identified by the run's owner and its activation generation, changed only
//! by compare-and-set; every statement that extends or advances the run
//! carries that pair in its WHERE clause (the fence), so a superseded
//! activation's write changes nothing and is reported as
//! [`CoreError::OwnerLost`].

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::{CoreError, Result};
use crate::model::{RUN_COLUMNS, Run, RunState};
use crate::store::now_ms;

/// The identity of one activation of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run_id: String,
    pub owner: String,
    pub generation: u64,
}

/// The fence every activation-scoped statement carries. Its parameters are
/// always ?1 = run id, ?2 = owner, ?3 = generation.
pub const FENCE: &str = "EXISTS (SELECT 1 FROM runs WHERE run_id = ?1 AND owner = ?2 \
    AND generation = ?3 AND state = 'running')";

impl Lease {
    pub fn generation_i64(&self) -> Result<i64> {
        i64::try_from(self.generation)
            .map_err(|_| CoreError::Corrupt(format!("generation {}", self.generation)))
    }

    pub fn lost(&self) -> CoreError {
        CoreError::OwnerLost {
            run_id: self.run_id.clone(),
            generation: self.generation,
        }
    }

    /// Whether this activation still owns the run.
    pub fn holds(&self, conn: &Connection) -> Result<bool> {
        let held: bool = conn.query_row(
            &format!("SELECT {FENCE}"),
            params![self.run_id, self.owner, self.generation_i64()?],
            |r| r.get(0),
        )?;
        Ok(held)
    }

    /// After a fenced statement changed nothing: the owner-lost error if the
    /// fence no longer holds, otherwise `otherwise`.
    pub fn explain(&self, conn: &Connection, otherwise: CoreError) -> CoreError {
        match self.holds(conn) {
            Ok(true) => otherwise,
            Ok(false) => self.lost(),
            Err(e) => e,
        }
    }
}

pub fn load(conn: &Connection, run_id: &str) -> Result<Run> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM runs WHERE run_id = ?1"),
        [run_id],
        Run::from_row,
    )
    .optional()?
    .ok_or_else(|| CoreError::NoSuchRun(run_id.to_owned()))?
}

pub fn state(conn: &Connection, run_id: &str) -> Result<RunState> {
    let s: Option<String> = conn
        .query_row("SELECT state FROM runs WHERE run_id = ?1", [run_id], |r| {
            r.get(0)
        })
        .optional()?;
    RunState::parse(&s.ok_or_else(|| CoreError::NoSuchRun(run_id.to_owned()))?)
}

/// The run's readiness sequence: bumped by every outcome, acceptance or
/// unknown result recorded for one of its calls.
pub fn readiness(conn: &Connection, run_id: &str) -> Result<u64> {
    let r: i64 = conn.query_row(
        "SELECT readiness FROM runs WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    crate::model::to_u64(r, "readiness")
}

/// Takes a pending run for a new activation: compare-and-set from
/// `pending` to `running`, with a new generation. Two callers racing for
/// the same run cannot both succeed.
pub fn claim(tx: &Transaction, run_id: &str, owner: &str) -> Result<Lease> {
    let changed = tx.execute(
        "UPDATE runs SET state = 'running', owner = ?2, generation = generation + 1, \
         awaited = NULL WHERE run_id = ?1 AND state = 'pending'",
        params![run_id, owner],
    )?;
    if changed == 0 {
        let state = state(tx, run_id)?;
        return Err(CoreError::WrongState {
            run_id: run_id.to_owned(),
            state,
            operation: "start an activation",
        });
    }
    let generation: i64 = tx.query_row(
        "SELECT generation FROM runs WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO activations (run_id, generation, owner, started_at) VALUES (?1, ?2, ?3, ?4)",
        params![run_id, generation, owner, now_ms()],
    )?;
    Ok(Lease {
        run_id: run_id.to_owned(),
        owner: owner.to_owned(),
        generation: crate::model::to_u64(generation, "generation")?,
    })
}

/// Takes a running run away from its current activation, as a lease
/// takeover does when the owner is presumed dead. The old activation's next
/// fenced write fails.
pub fn take_over(
    tx: &Transaction,
    run_id: &str,
    owner: &str,
    expected_generation: u64,
) -> Result<Lease> {
    let expected = i64::try_from(expected_generation)
        .map_err(|_| CoreError::Invalid(format!("generation {expected_generation}")))?;
    let changed = tx.execute(
        "UPDATE runs SET owner = ?2, generation = generation + 1 \
         WHERE run_id = ?1 AND state = 'running' AND generation = ?3",
        params![run_id, owner, expected],
    )?;
    if changed == 0 {
        let state = state(tx, run_id)?;
        return Err(CoreError::WrongState {
            run_id: run_id.to_owned(),
            state,
            operation: "take over the activation",
        });
    }
    let generation = expected_generation + 1;
    tx.execute(
        "INSERT INTO activations (run_id, generation, owner, started_at) VALUES (?1, ?2, ?3, ?4)",
        params![run_id, expected + 1, owner, now_ms()],
    )?;
    Ok(Lease {
        run_id: run_id.to_owned(),
        owner: owner.to_owned(),
        generation,
    })
}

/// At startup every `running` run belongs to a process that no longer
/// exists (the store's single-writer lease guarantees no other live process
/// has it open), so each goes back to `pending` with a new generation.
pub fn requeue_orphans(tx: &Transaction) -> Result<Vec<String>> {
    let mut stmt = tx.prepare("SELECT run_id FROM runs WHERE state = 'running' ORDER BY run_id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute(
        "UPDATE runs SET state = 'pending', owner = NULL, generation = generation + 1 \
         WHERE state = 'running'",
        [],
    )?;
    Ok(ids)
}

/// Records the runtime fingerprint at a run's first activation.
pub fn record_fingerprint(tx: &Transaction, lease: &Lease, fingerprint: &str) -> Result<()> {
    let changed = tx.execute(
        &format!(
            "UPDATE runs SET fingerprint = ?4 WHERE run_id = ?1 AND fingerprint IS NULL AND {FENCE}"
        ),
        params![
            lease.run_id,
            lease.owner,
            lease.generation_i64()?,
            fingerprint
        ],
    )?;
    if changed == 0 {
        return Err(lease.explain(
            tx,
            CoreError::Invalid("fingerprint already recorded".into()),
        ));
    }
    Ok(())
}

/// How an activation leaves `running`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Succeeded {
        value: String,
    },
    Failed {
        kind: String,
        detail: String,
    },
    EngineMismatch {
        detail: String,
    },
    NeedsReconcile {
        positions: Vec<u64>,
    },
    /// Back to `pending` for another activation. `broken` counts it as a
    /// failed activation (the worker died or broke the protocol).
    Requeue {
        broken: bool,
    },
}

/// Moves the run out of `running`, fenced by the activation's lease.
pub fn exit(tx: &Transaction, lease: &Lease, exit: &Exit) -> Result<RunState> {
    let g = lease.generation_i64()?;
    let now = now_ms();
    let (changed, state) = match exit {
        Exit::Succeeded { value } => {
            // Success also requires that every issued call has a released
            // outcome: a run never succeeds with work still in flight.
            let n = tx.execute(
                &format!(
                    "UPDATE runs SET state = 'succeeded', owner = NULL, result = ?4, ended_at = ?5 \
                     WHERE run_id = ?1 AND {FENCE} AND NOT EXISTS \
                     (SELECT 1 FROM journal WHERE run_id = ?1 AND delivery_order IS NULL)"
                ),
                params![lease.run_id, lease.owner, g, value, now],
            )?;
            if n == 0 && lease.holds(tx)? {
                return Err(CoreError::Invalid(
                    "the worker reported completion with calls still unresolved".into(),
                ));
            }
            (n, RunState::Succeeded)
        }
        Exit::Failed { kind, detail } => (
            tx.execute(
                &format!(
                    "UPDATE runs SET state = 'failed', owner = NULL, error_kind = ?4, \
                     error_detail = ?5, ended_at = ?6 WHERE run_id = ?1 AND {FENCE}"
                ),
                params![lease.run_id, lease.owner, g, kind, detail, now],
            )?,
            RunState::Failed,
        ),
        Exit::EngineMismatch { detail } => (
            tx.execute(
                &format!(
                    "UPDATE runs SET state = 'engine_mismatch', owner = NULL, \
                     error_kind = 'engine_mismatch', error_detail = ?4, ended_at = ?5 \
                     WHERE run_id = ?1 AND {FENCE}"
                ),
                params![lease.run_id, lease.owner, g, detail, now],
            )?,
            RunState::EngineMismatch,
        ),
        Exit::NeedsReconcile { positions } => {
            let detail = format!("unknown outcome at positions {positions:?}");
            (
                tx.execute(
                    &format!(
                        "UPDATE runs SET state = 'needs_reconcile', owner = NULL, \
                         error_kind = 'unknown_outcome', error_detail = ?4 \
                         WHERE run_id = ?1 AND {FENCE}"
                    ),
                    params![lease.run_id, lease.owner, g, detail],
                )?,
                RunState::NeedsReconcile,
            )
        }
        Exit::Requeue { broken } => (
            tx.execute(
                &format!(
                    "UPDATE runs SET state = 'pending', owner = NULL, broken = broken + ?4 \
                     WHERE run_id = ?1 AND {FENCE}"
                ),
                params![lease.run_id, lease.owner, g, i64::from(*broken)],
            )?,
            RunState::Pending,
        ),
    };
    if changed == 0 {
        return Err(lease.lost());
    }
    Ok(state)
}

/// What a `Suspended` ending turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parked {
    /// Nothing arrived; the run waits for a completion.
    Suspended,
    /// Something arrived after the worker gave up its VM; the run is
    /// runnable again.
    Runnable,
    /// A call's outcome became unknown; the run needs an operator.
    NeedsReconcile,
}

/// Moves a run whose worker reported `Suspended` out of `running`.
///
/// The worker reports `Suspended` and drops its VM; a completion can land
/// between that report and this transaction. So, in one transaction, it
/// re-reads the awaited positions' outcomes, the mailbox and the readiness
/// sequence the activation last saw: if anything arrived, the run becomes
/// runnable instead of suspended, and no wakeup is lost.
pub fn park(
    tx: &Transaction,
    lease: &Lease,
    awaited: &[u64],
    seen_readiness: u64,
) -> Result<Parked> {
    if !lease.holds(tx)? {
        return Err(lease.lost());
    }
    let g = lease.generation_i64()?;
    let readiness: i64 = tx.query_row(
        "SELECT readiness FROM runs WHERE run_id = ?1",
        [&lease.run_id],
        |r| r.get(0),
    )?;
    let in_mailbox: i64 = tx.query_row(
        "SELECT COUNT(*) FROM mailbox WHERE run_id = ?1",
        [&lease.run_id],
        |r| r.get(0),
    )?;
    let mut settled_awaited = false;
    for p in awaited {
        let p = i64::try_from(*p).map_err(|_| CoreError::Invalid(format!("position {p}")))?;
        let settled: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM journal WHERE run_id = ?1 AND position = ?2 \
             AND settlement IS NOT NULL) OR EXISTS (SELECT 1 FROM mailbox WHERE run_id = ?1 \
             AND position = ?2)",
            params![lease.run_id, p],
            |r| r.get(0),
        )?;
        settled_awaited |= settled;
    }
    let unknown: i64 = tx.query_row(
        "SELECT COUNT(*) FROM journal WHERE run_id = ?1 AND dispatch = 'unknown' \
         AND settlement IS NULL",
        [&lease.run_id],
        |r| r.get(0),
    )?;
    let arrived = settled_awaited
        || in_mailbox > 0
        || crate::model::to_u64(readiness, "readiness")? != seen_readiness;
    let awaited_text =
        serde_json::to_string(awaited).map_err(|e| CoreError::Invalid(e.to_string()))?;
    let (state, parked) = if unknown > 0 {
        ("needs_reconcile", Parked::NeedsReconcile)
    } else if arrived {
        ("pending", Parked::Runnable)
    } else {
        ("suspended", Parked::Suspended)
    };
    let changed = tx.execute(
        &format!(
            "UPDATE runs SET state = ?4, owner = NULL, awaited = ?5 WHERE run_id = ?1 AND {FENCE}"
        ),
        params![lease.run_id, lease.owner, g, state, awaited_text],
    )?;
    if changed == 0 {
        return Err(lease.lost());
    }
    Ok(parked)
}

/// Cancels a run that has not ended. Bumps the generation so an activation
/// still driving it loses ownership at its next fenced write. Calls already
/// dispatched keep their obligations.
pub fn cancel(tx: &Transaction, run_id: &str, detail: &str) -> Result<()> {
    let changed = tx.execute(
        "UPDATE runs SET state = 'cancelled', owner = NULL, generation = generation + 1, \
         error_kind = 'cancelled', error_detail = ?2, ended_at = ?3 \
         WHERE run_id = ?1 AND state NOT IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled')",
        params![run_id, detail, now_ms()],
    )?;
    if changed == 0 {
        let state = state(tx, run_id)?;
        return Err(CoreError::WrongState {
            run_id: run_id.to_owned(),
            state,
            operation: "cancel",
        });
    }
    Ok(())
}

/// Number of activations ever started for the run.
pub fn activation_count(conn: &Connection, run_id: &str) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM activations WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    crate::model::to_u64(n, "activation count")
}
