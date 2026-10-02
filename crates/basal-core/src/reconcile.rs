//! The exit from `needs_reconcile`: an operator states what happened to a
//! call whose outcome basal could not prove.

use basal_host::HostOutcome;
use rusqlite::{Transaction, params};

use crate::error::{CoreError, Result};
use crate::journal::{self, Source};
use crate::model::RunState;
use crate::runs;
use crate::runtime::Runtime;
use crate::store::now_ms;

/// What the operator observed about the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The call did take effect, with this outcome; the run continues with
    /// it.
    ObservedResult(HostOutcome),
    /// The call did not take effect; it is sent again with the same
    /// idempotency key.
    NotApplied,
    /// End the run. Calls already sent stay open obligations: when their
    /// outcomes arrive they are logged (refused, since the run is
    /// cancelled), which settles them.
    Cancel,
    /// The call did take effect, but its result was not observed: the
    /// script receives a rejection with code [`RECONCILED_AS_APPLIED`] and
    /// no value, so it learns the effect happened without ever seeing an
    /// invented result. The rejection is recorded as the call's outcome and
    /// released to the script like any other, so a replay of the run
    /// delivers the same rejection from the journal.
    ReconciledAsApplied,
}

/// The rejection code a script receives for a call the operator
/// reconciled as applied without its result.
pub const RECONCILED_AS_APPLIED: &str = "reconciled_as_applied";

/// The outcome released for a call reconciled as applied. It depends on
/// nothing but the code, so it is the same whenever it is produced.
pub fn reconciled_as_applied() -> basal_host::HostOutcome {
    crate::kv::rejection(
        RECONCILED_AS_APPLIED,
        "the operator reconciled this call as applied; its result was not observed",
    )
}

pub(crate) fn audit(
    tx: &Transaction,
    actor: &str,
    action: &str,
    run_id: Option<&str>,
    position: Option<u64>,
    detail: &str,
) -> Result<()> {
    audit_answer(tx, actor, action, run_id, position, detail, None)
}

/// Writes an audit row. `elicitation_id` names the decision card the
/// action was answered through, if it came through one.
pub(crate) fn audit_answer(
    tx: &Transaction,
    actor: &str,
    action: &str,
    run_id: Option<&str>,
    position: Option<u64>,
    detail: &str,
    elicitation_id: Option<&str>,
) -> Result<()> {
    let position = position
        .map(|p| i64::try_from(p).map_err(|_| CoreError::Invalid(format!("position {p}"))))
        .transpose()?;
    tx.execute(
        "INSERT INTO audit (at, actor, action, run_id, position, detail, elicitation_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            now_ms(),
            actor,
            action,
            run_id,
            position,
            detail,
            elicitation_id
        ],
    )?;
    Ok(())
}

/// Records a resolution of an unknown call inside `tx`, audited, and
/// returns the run's state after it. `elicitation_id` names the decision
/// card the resolution was answered through, if any. On a run in
/// `needs_reconcile`, once no unknown call remains the run becomes
/// `pending`; the caller wakes it after the commit.
pub(crate) fn resolve(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    resolution: &Resolution,
    actor: &str,
    elicitation_id: Option<&str>,
) -> Result<RunState> {
    let state = runs::state(tx, run_id)?;
    let wrong = |operation| CoreError::WrongState {
        run_id: run_id.to_owned(),
        state,
        operation,
    };
    let open = journal::unsettled_positions(tx, run_id)?;
    if !open.contains(&position) {
        return Err(CoreError::Invalid(format!(
            "position {position} of run {run_id} has no open obligation"
        )));
    }
    let audit = |action: &str, detail: &str| {
        audit_answer(
            tx,
            actor,
            action,
            Some(run_id),
            Some(position),
            detail,
            elicitation_id,
        )
    };
    match resolution {
        Resolution::ObservedResult(outcome) => {
            if state != RunState::NeedsReconcile && !state.is_terminal() {
                return Err(wrong("record an observed result"));
            }
            journal::accept_outcome(tx, run_id, position, None, outcome, Source::Reconcile)?;
            audit("reconcile.observed_result", outcome.value.as_str())?;
        }
        Resolution::ReconciledAsApplied => {
            if state != RunState::NeedsReconcile {
                return Err(wrong("reconcile a call as applied"));
            }
            journal::accept_outcome(
                tx,
                run_id,
                position,
                None,
                &reconciled_as_applied(),
                Source::Reconcile,
            )?;
            audit("reconcile.applied", "")?;
        }
        Resolution::NotApplied => {
            if state != RunState::NeedsReconcile {
                return Err(wrong("mark a call not applied"));
            }
            tx.execute(
                "UPDATE journal SET dispatch = 'not_applied' WHERE run_id = ?1 \
                 AND position = ?2 AND settlement IS NULL",
                params![
                    run_id,
                    i64::try_from(position)
                        .map_err(|_| CoreError::Invalid(format!("position {position}")))?
                ],
            )?;
            audit("reconcile.not_applied", "")?;
        }
        Resolution::Cancel => {
            if state != RunState::NeedsReconcile {
                return Err(wrong("cancel through reconcile"));
            }
            runs::cancel(
                tx,
                run_id,
                &format!("cancelled by {actor} while reconciling"),
            )?;
            audit("reconcile.cancel", "")?;
            return Ok(RunState::Cancelled);
        }
    }
    if state == RunState::NeedsReconcile && journal::unknown_positions(tx, run_id)?.is_empty() {
        tx.execute(
            "UPDATE runs SET state = 'pending', error_kind = NULL, error_detail = NULL \
             WHERE run_id = ?1 AND state = 'needs_reconcile'",
            [run_id],
        )?;
        return Ok(RunState::Pending);
    }
    Ok(state)
}

impl Runtime {
    /// Records an operator's resolution of an unknown call, audited.
    ///
    /// On a run in `needs_reconcile`, once no unknown call remains the run
    /// becomes runnable. On a run that has already ended, only an observed
    /// result is accepted: it settles the call's obligation without
    /// resuming anything.
    pub fn reconcile(
        &self,
        run_id: &str,
        position: u64,
        resolution: Resolution,
        actor: &str,
    ) -> Result<RunState> {
        let state = self
            .store()
            .write(|tx| resolve(tx, run_id, position, &resolution, actor, None))?;
        self.shared.signal.bump();
        if state == RunState::Pending {
            self.wake(run_id);
        }
        Ok(state)
    }
}
