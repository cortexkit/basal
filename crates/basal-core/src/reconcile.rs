//! The exit from `needs_reconcile`: an operator states what happened to a
//! call whose outcome basal could not prove.

use basal_host::HostOutcome;
use rusqlite::{Transaction, params};

use crate::error::Result;

use crate::model::RunState;

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
    let position = position.map(crate::model::position_i64).transpose()?;
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

impl Runtime {
    /// Records an operator resolution against the recovered run registry.
    pub fn reconcile(
        &self,
        run_id: &str,
        position: u64,
        resolution: Resolution,
        actor: &str,
    ) -> Result<RunState> {
        let state = self
            .shared
            .staged_hosts
            .as_ref()
            .expect("recovered run registry")
            .resolve(
                run_id,
                position,
                resolution,
                actor,
                None,
                self.config().clock.now_ms(),
            )?;
        self.shared.signal.bump();
        Ok(state)
    }
}
