//! The install gate: before every activation, core is asked whether it
//! still stands behind the run's flow version.
//!
//! When the operator revokes a flow version in core, core refuses that
//! version's sink writes, but nothing else would tell basal. So before every
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
//! worker is handed anything. A revoke that lands after the check is not
//! seen until the next activation asks again; until then core's own refusal
//! of the revoked version's sink writes (digest items and status lines, the
//! only way a flow reaches an agent) still holds, so the window is
//! deliberately bounded by one activation.

use std::time::Duration;

use basal_host::InstallStatus;

use crate::error::Result;
use crate::ids::{code_hash, hex};
use crate::install;
use crate::model::Run;
use crate::runs::{self, Exit, Lease};
use crate::runtime::{ActivationEnd, Runtime};

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

/// What the gate decided.
pub(crate) enum Gate {
    /// Core stands behind the run's code: activate it.
    Open,
    /// Nothing is activated; the run already left `running`.
    Closed(ActivationEnd),
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// An earlier read is only a hint: reconciliation can clear removal before
/// this writer is acquired. The mark and cancellation must share one transaction.
fn cancel_removed(
    tx: &rusqlite::Transaction,
    lease: &Lease,
    flow_id: &str,
    detail: &str,
) -> Result<bool> {
    if !crate::packages::removed(tx, flow_id)? {
        return Ok(false);
    }
    if !lease.holds(tx)? {
        return Err(lease.lost());
    }
    runs::forget_claim(tx, lease)?;
    runs::cancel(tx, &lease.run_id, detail)?;
    Ok(true)
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
    fn back_off(&self, run_id: &str) -> Result<Duration> {
        let now = self.config.clock.now_ms();
        let live = self.store().read(|conn| {
            let mut stmt = conn.prepare("SELECT run_id FROM runs WHERE state IN ('pending','running','suspended','needs_reconcile')")?;
            let ids = stmt.query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
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

    fn clear_backoff(&self, run_id: &str) {
        self.shared
            .gate_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(run_id);
    }

    /// The gate for one claimed activation of `run`, held under `lease`.
    pub(crate) fn check_install(&self, lease: &Lease, run: &Run) -> Result<Gate> {
        if self
            .store()
            .read(|c| crate::packages::removed(c, &run.flow_id))?
        {
            let detail = "instance removed before activating".to_owned();
            let cancelled = self
                .store()
                .write(|tx| cancel_removed(tx, lease, &run.flow_id, &detail))?;
            if cancelled {
                self.clear_backoff(&lease.run_id);
                self.shared.signal.bump();
                return Ok(Gate::Closed(ActivationEnd::Revoked {
                    version: run.flow_version.unwrap_or(0),
                    cause: RevokeCause::Removed,
                    detail,
                }));
            }
        }
        if self.config.install_gate == InstallGate::Off {
            return Ok(Gate::Open);
        }
        let flow_id = &run.flow_id;
        let Some(version) = run.flow_version else {
            // Admission records the version of every run; without one there
            // is nothing to ask core about, and no answer means no activation.
            return self.defer(lease, "the run records no flow version".into());
        };
        if let Some(reason) = self
            .store()
            .read(|c| install::revocation(c, flow_id, version))?
        {
            return self.refuse(lease, run, version, RevokeCause::AlreadyRevoked, reason);
        }
        let run_hash = hex(&code_hash(&run.script, &run.manifest));
        let (cause, reason) = match self.shared.host.install_status(flow_id, version) {
            Ok(InstallStatus::Active { code_hash, scope }) if code_hash == run_hash => {
                if let Some(registered) = &scope {
                    let previous: Option<String> = self.store().read(|c| {
                        Ok(c.query_row(
                            "SELECT scope_health FROM flows WHERE flow_id=?1",
                            [flow_id],
                            |r| r.get(0),
                        )?)
                    })?;
                    if let Some(previous) = previous {
                        let previous: serde_json::Value = serde_json::from_str(&previous)
                            .map_err(|e| crate::error::CoreError::Corrupt(e.to_string()))?;
                        let clears = previous["reason"] == "no_flow_scope"
                            || (previous["reason"] == "target_flow_unsupported"
                                && previous["provider"]
                                    .as_str()
                                    .is_some_and(|p| registered.targets.contains(p)));
                        if clears {
                            self.store().write(|tx| {
                                tx.execute(
                                    "UPDATE flows SET scope_health=NULL WHERE flow_id=?1",
                                    [flow_id],
                                )?;
                                Ok(())
                            })?;
                        }
                    }
                }
                self.shared.host.configure_flow(flow_id, true, scope);
                self.clear_backoff(&run.run_id);
                return Ok(Gate::Open);
            }
            Ok(InstallStatus::Active { code_hash, .. }) => (
                RevokeCause::HashMismatch {
                    core: code_hash.clone(),
                    run: run_hash.clone(),
                },
                format!(
                    "core approved code hash {code_hash} for {flow_id} v{version}, \
                     but the run holds code hash {run_hash}"
                ),
            ),
            Ok(InstallStatus::Revoked { .. }) => (
                RevokeCause::Revoked,
                format!("core revoked {flow_id} v{version}"),
            ),
            Ok(InstallStatus::Unknown) => (
                RevokeCause::Unknown,
                format!("core holds no install of {flow_id} v{version}"),
            ),
            Err(e) => {
                return self.defer(
                    lease,
                    format!("core gave no install status for {flow_id} v{version}: {e}"),
                );
            }
        };
        self.refuse(lease, run, version, cause, reason)
    }

    /// No answer from core: the run goes back to `pending` untouched and
    /// waits out its backoff.
    fn defer(&self, lease: &Lease, detail: String) -> Result<Gate> {
        self.store().write(|tx| {
            runs::exit(tx, lease, &Exit::Deferred)?;
            tx.execute(
                "UPDATE runs SET error_detail = ?2 WHERE run_id = ?1",
                rusqlite::params![lease.run_id, detail],
            )?;
            Ok(())
        })?;
        let retry_in = self.back_off(&lease.run_id)?;
        self.shared.signal.bump();
        Ok(Gate::Closed(ActivationEnd::Deferred { detail, retry_in }))
    }

    /// Core does not stand behind the run's code: the version is recorded
    /// as revoked and the run is cancelled, in one transaction.
    fn refuse(
        &self,
        lease: &Lease,
        run: &Run,
        version: u32,
        cause: RevokeCause,
        reason: String,
    ) -> Result<Gate> {
        let now = self.config.clock.now_ms();
        let detail = format!("cancelled before activating: {reason}");
        self.store().write(|tx| {
            if !lease.holds(tx)? {
                return Err(lease.lost());
            }
            install::revoke(tx, &run.flow_id, version, &reason, now)?;
            runs::forget_claim(tx, lease)?;
            runs::cancel(tx, &lease.run_id, &detail)?;
            crate::reconcile::audit(
                tx,
                install::CORE_ACTOR,
                "cancel",
                Some(&lease.run_id),
                None,
                &reason,
            )
        })?;
        self.clear_backoff(&lease.run_id);
        self.shared.signal.bump();
        Ok(Gate::Closed(ActivationEnd::Revoked {
            version,
            cause,
            detail,
        }))
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod removal_tests;
