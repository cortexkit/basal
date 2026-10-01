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
    /// End the run. Calls already sent keep their obligations.
    Cancel,
}

pub(crate) fn audit(
    tx: &Transaction,
    actor: &str,
    action: &str,
    run_id: Option<&str>,
    position: Option<u64>,
    detail: &str,
) -> Result<()> {
    let position = position
        .map(|p| i64::try_from(p).map_err(|_| CoreError::Invalid(format!("position {p}"))))
        .transpose()?;
    tx.execute(
        "INSERT INTO audit (at, actor, action, run_id, position, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![now_ms(), actor, action, run_id, position, detail],
    )?;
    Ok(())
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
        let state = self.store().write(|tx| {
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
            match &resolution {
                Resolution::ObservedResult(outcome) => {
                    if state != RunState::NeedsReconcile && !state.is_terminal() {
                        return Err(wrong("record an observed result"));
                    }
                    journal::accept_outcome(
                        tx,
                        run_id,
                        position,
                        None,
                        outcome,
                        Source::Reconcile,
                    )?;
                    audit(
                        tx,
                        actor,
                        "reconcile.observed_result",
                        Some(run_id),
                        Some(position),
                        outcome.value.as_str(),
                    )?;
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
                    audit(
                        tx,
                        actor,
                        "reconcile.not_applied",
                        Some(run_id),
                        Some(position),
                        "",
                    )?;
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
                    audit(
                        tx,
                        actor,
                        "reconcile.cancel",
                        Some(run_id),
                        Some(position),
                        "",
                    )?;
                    return Ok(RunState::Cancelled);
                }
            }
            if state == RunState::NeedsReconcile
                && journal::unknown_positions(tx, run_id)?.is_empty()
            {
                tx.execute(
                    "UPDATE runs SET state = 'pending', error_kind = NULL, error_detail = NULL \
                     WHERE run_id = ?1 AND state = 'needs_reconcile'",
                    [run_id],
                )?;
                return Ok(RunState::Pending);
            }
            Ok(state)
        })?;
        self.shared.signal.bump();
        if state == RunState::Pending {
            self.wake(run_id);
        }
        Ok(state)
    }
}
