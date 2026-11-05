//! The install gate: before every activation, core is asked whether it
//! still stands behind the run's flow version.
//!
//! When the operator revokes a flow version in core, core refuses that
//! version's sink writes and sends basal a verified revocation hint. Before every
//! activation (a run's first, and every resume after a suspension or a
//! restart) the activation asks core's `flow.install_status` through the
//! host, and goes on only when core answers `active` with the code hash of
//! the exact code the run is about to execute. Otherwise:
//!
//! - `revoked`, or `unknown` (every run's version is one basal approved, and
//!   core keeps every version it approved until it is revoked), or `active`
//!   with another code hash: core does not stand behind this code. The
//!   version is recorded as revoked in basal's store with the reason, so it
//!   is never activated again ([`crate::install::revoke`], which also leaves
//!   the flow with no approved version and disabled by core when it was the
//!   approved one), and the run is cancelled. Calls it already dispatched
//!   keep their obligations, as for every cancel.
//! - No answer (core unreachable, a timeout, a refusal, a reply that does not
//!   decode): fail closed. Nothing is activated and the run is neither failed
//!   nor cancelled: it goes back to `pending` and is not offered again until
//!   a backoff passes, doubling with each unanswered check, so a core outage
//!   delays runs but never loses one, never runs one unapproved, and never
//!   turns into a loop of questions.
//!
//! The check runs after the activation's lease is taken and before the
//! worker is handed anything. A verified push can record revocation during an
//! activation: later dispatch attempts are refused, including local calls.
//! Attempts authorized before that record may still be sent and settle.
//! No revocation polling runs while idle; this gate also covers missed pushes.

use std::time::Duration;

use crate::error::Result;

use crate::runtime::Runtime;

/// Whether activations ask core first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallGate {
    /// Ask core before every activation. Production always runs with this.
    Core,
    /// Never ask. Only for a runtime whose flows were not approved through
    /// core and that can write no sink: the dry run's runtime over its own
    /// throwaway store (its host captures every sink write and sends none),
    /// and tests whose subject is not the gate.
    Off,
}

/// Why core no longer stands behind a run's version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeCause {
    /// The instance was removed from the agent it belonged to (the agent's
    /// persona no longer lists this package instance), so it must not run.
    /// Core's approval of the package version is unchanged.
    Removed,
    /// Core answered `revoked`.
    Revoked,
    /// Core holds no install of a version basal approved.
    Unknown,
    /// Core approved other code under this version than the run would
    /// execute: basal and core disagree about what was approved.
    HashMismatch { core: String, run: String },
    /// An earlier check already recorded the version as revoked.
    AlreadyRevoked,
}

/// A run whose gate got no answer, and when it may be checked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// On the runtime's clock (`Config::clock`), which tests set by hand.
    pub(crate) retry_at_ms: i64,
    /// Consecutive unanswered checks.
    failures: u32,
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Runtime {
    /// How long `run_id` still waits before its gate may ask core again,
    /// if it waits at all.
    pub(crate) fn gate_wait(&self, run_id: &str) -> Option<Duration> {
        let now = self.config.clock.now_ms();
        let backoff = self
            .shared
            .gate_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .copied()?;
        let left = backoff.retry_at_ms.saturating_sub(now);
        (left > 0).then(|| Duration::from_millis(u64::try_from(left).unwrap_or(0)))
    }

    /// Records one more unanswered check and returns how long the run now
    /// waits: `install_gate_retry`, doubled for each earlier unanswered
    /// check in a row, at most `install_gate_retry_max`.
    pub(crate) fn back_off(&self, run_id: &str) -> Result<Duration> {
        let now = self.config.clock.now_ms();
        let live = self.store().read(|conn| {
            let mut stmt = conn.prepare("SELECT run_id FROM live_runs")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
            Ok(ids)
        })?;
        let mut table = self
            .shared
            .gate_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Terminal runs and deleted rows can never retry the install gate.
        // Reap them on insertion so cancellations cannot grow this map forever.
        table.retain(|id, _| live.contains(id));
        let failures = table
            .get(run_id)
            .map_or(0, |b| b.failures)
            .saturating_add(1);
        let wait = self
            .config
            .install_gate_retry
            .saturating_mul(1u32 << (failures - 1).min(16))
            .min(self.config.install_gate_retry_max);
        table.insert(
            run_id.to_owned(),
            Backoff {
                retry_at_ms: now.saturating_add(millis(wait)),
                failures,
            },
        );
        Ok(wait)
    }

    pub(crate) fn clear_backoff(&self, run_id: &str) {
        self.shared
            .gate_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run_id);
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod removal_tests;
