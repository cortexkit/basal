//! The per-flow run rate limit, the dispatch budget, and auto-disable.
//!
//! Each flow may admit a bounded number of runs and dispatch a bounded
//! number of calls per rate window. The counters are durable, so a restart
//! does not reset them. A window in which either limit refused something is
//! saturated; a flow saturated for K consecutive windows is disabled and
//! its owner is told, which bounds what a loop the cause-propagation rule
//! cannot see (a round trip through an outside system) can cost.
//!
//! Windows are fixed intervals anchored at the Unix epoch, like token
//! windows, so "consecutive" means adjacent window numbers: a window with
//! no refusal, or no activity at all, breaks the streak.

use std::time::Duration;

use rusqlite::{Transaction, params};

use crate::error::{CoreError, Result};
use crate::install::{self, Actor};
use crate::tokens::window_start;

/// The limits, from the runtime's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    pub window: Duration,
    /// Runs admitted per flow per window.
    pub max_runs: u32,
    /// Calls dispatched to hosts per flow per window.
    pub max_dispatches: u32,
    /// Consecutive saturated windows after which the flow is disabled.
    pub saturated_windows_to_disable: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_runs: 60,
            max_dispatches: 600,
            saturated_windows_to_disable: 3,
        }
    }
}

impl RateLimits {
    fn window_ms(&self) -> Result<i64> {
        let ms = i64::try_from(self.window.as_millis())
            .map_err(|_| CoreError::Invalid("rate window too long".into()))?;
        if ms <= 0 {
            return Err(CoreError::Invalid("rate window must be positive".into()));
        }
        Ok(ms)
    }
}

/// Which limit a step counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Run,
    Dispatch,
}

/// What [`take`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    Allowed,
    /// The window's limit is reached. `disabled` says whether this refusal
    /// completed K saturated windows in a row and disabled the flow.
    Refused {
        disabled: bool,
    },
}

/// Counts one more admission or dispatch of the flow in the window that
/// contains `now_ms`, or refuses it when the window is full: [`check`]
/// then, if allowed, [`count`].
pub fn take(
    tx: &Transaction,
    flow_id: &str,
    kind: Kind,
    now_ms: i64,
    limits: &RateLimits,
) -> Result<Take> {
    let take = check(tx, flow_id, kind, now_ms, limits)?;
    if take == Take::Allowed {
        count(tx, flow_id, kind, now_ms, limits)?;
    }
    Ok(take)
}

fn column(kind: Kind, limits: &RateLimits) -> (&'static str, u32) {
    match kind {
        Kind::Run => ("runs", limits.max_runs),
        Kind::Dispatch => ("dispatches", limits.max_dispatches),
    }
}

/// Counts one admission or dispatch in the current window. Call it only
/// after [`check`] allowed it, in the same transaction, once nothing else
/// can refuse the step: the count commits with the run or call it counts.
pub fn count(
    tx: &Transaction,
    flow_id: &str,
    kind: Kind,
    now_ms: i64,
    limits: &RateLimits,
) -> Result<()> {
    let window_ms = limits.window_ms()?;
    let start = window_start(now_ms, window_ms);
    let (column, _) = column(kind, limits);
    tx.execute(
        "INSERT OR IGNORE INTO rate_windows (flow_id, window_ms, window_start) VALUES (?1, ?2, ?3)",
        params![flow_id, window_ms, start],
    )?;
    tx.execute(
        &format!(
            "UPDATE rate_windows SET {column} = {column} + 1 WHERE flow_id = ?1 \
             AND window_ms = ?2 AND window_start = ?3"
        ),
        params![flow_id, window_ms, start],
    )?;
    Ok(())
}

/// Whether the window that contains `now_ms` has room for one more
/// admission or dispatch. A refusal marks the window saturated and, when
/// that completes K saturated windows in a row, disables the flow; both
/// commit with the caller's transaction, together with the rejection the
/// refusal causes.
pub fn check(
    tx: &Transaction,
    flow_id: &str,
    kind: Kind,
    now_ms: i64,
    limits: &RateLimits,
) -> Result<Take> {
    let window_ms = limits.window_ms()?;
    let start = window_start(now_ms, window_ms);
    tx.execute(
        "INSERT OR IGNORE INTO rate_windows (flow_id, window_ms, window_start) VALUES (?1, ?2, ?3)",
        params![flow_id, window_ms, start],
    )?;
    let (column, max) = column(kind, limits);
    let used: i64 = tx.query_row(
        &format!(
            "SELECT {column} FROM rate_windows WHERE flow_id = ?1 AND window_ms = ?2 \
             AND window_start = ?3"
        ),
        params![flow_id, window_ms, start],
        |r| r.get(0),
    )?;
    if used < i64::from(max) {
        return Ok(Take::Allowed);
    }
    tx.execute(
        "UPDATE rate_windows SET saturated = 1 WHERE flow_id = ?1 AND window_ms = ?2 \
         AND window_start = ?3",
        params![flow_id, window_ms, start],
    )?;
    let k = i64::from(limits.saturated_windows_to_disable.max(1));
    let earliest = start - (k - 1) * window_ms;
    let streak: i64 = tx.query_row(
        "SELECT COUNT(*) FROM rate_windows WHERE flow_id = ?1 AND window_ms = ?2 \
         AND saturated = 1 AND window_start BETWEEN ?3 AND ?4",
        params![flow_id, window_ms, earliest, start],
        |r| r.get(0),
    )?;
    if streak < k {
        return Ok(Take::Refused { disabled: false });
    }
    let reason = format!(
        "{k} consecutive saturated rate windows of {} ms (limits: {} runs, {} dispatches per window)",
        window_ms, limits.max_runs, limits.max_dispatches
    );
    let disabled =
        install::disable(tx, flow_id, &Actor::Runtime, &reason, now_ms).map_err(|e| match e {
            install::InstallError::Store(e) => e,
            other => CoreError::Invalid(other.to_string()),
        })?;
    if disabled {
        // The operator decides whether the flow runs again, on a card
        // raised from this row; it commits with the disable it is about.
        let rule = serde_json::json!({
            "rule": "rate_saturation",
            "limit": column,
            "used": used,
            "saturated_windows": k,
            "window_ms": window_ms,
            "max_runs": limits.max_runs,
            "max_dispatches": limits.max_dispatches,
        });
        crate::decisions::record_auto_disable(tx, flow_id, &rule, now_ms)?;
    }
    Ok(Take::Refused { disabled })
}
