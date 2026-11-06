//! Retention in bounded transactions, on the runtime's deadline-service cadence.
//!
//! By default, a successful run's payloads and event body become eligible for
//! pruning 24 hours after it ends; run summaries, call metadata, activations
//! and code snapshots keep 90 days. Either is deferred while any call, token
//! reservation or open decision card still needs the run.
//! Quarantine, call/audit history, delivered
//! outbox notifications, settled token ledgers and dropped schedules keep
//! thirty days; any existing run protects its dependent history. Unsent
//! notifications and open reservations/cards stay until settled. Old rate
//! windows are removed only outside the current saturation streak and any
//! window a live call could refund; token windows stay while a reservation or
//! existing run needs them. Closed decision cards retain the latest re-enable
//! episode per flow forever, because its number allocates the next episode.
//! Trigger and idempotency tombstones stay forever: no source redelivery
//! horizon is established, so deleting one could permit a second effect.
//! Full batches schedule another bounded pass after one second rather than
//! limiting cleanup throughput to one batch per normal maintenance interval.
//!
//! `fs.write` temporary files are not history and do not wait for the
//! retention interval: every maintenance pass removes the ones whose write
//! is no longer running in this process ([`Runtime::sweep_fs_temps`]).

use rusqlite::params;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use basal_host::builtins::fs::{self as host_fs, TempHold, TempLease, TempLedger};

use crate::error::Result;
use crate::runtime::{Runtime, Shared};

/// What a prune did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned: Vec<String>,
    /// Terminal runs past retention kept because a call still has no
    /// outcome anywhere.
    pub kept_unsettled: Vec<String>,
}

pub(crate) const BATCH: usize = 128;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub interval: Duration,
    pub runs: Duration,
    pub payloads: Duration,
    pub history: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            runs: Duration::from_secs(90 * 24 * 3600),
            payloads: Duration::from_secs(24 * 3600),
            history: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Runtime {
    /// Successful runs retain summaries only; no payload-pruning pass is needed.
    pub fn prune_payloads(&self, _now_ms: i64, _retention_ms: i64) -> Result<usize> {
        Ok(0)
    }

    pub fn prune(&self, now_ms: i64, retention_ms: i64) -> Result<PruneReport> {
        crate::run_retention::prune(self.store(), now_ms, retention_ms)
    }

    pub(crate) fn retention_due(&self, now: i64) -> Result<()> {
        self.sweep_fs_temps()?;
        // Codemode runs are pruned when they are due, not on the flow
        // retention interval: their 24-hour horizon is part of the codemode
        // contract, and `next_wake_at` wakes the engine for it.
        crate::codemode::retention::sweep(self.store(), now)?;
        let mut next = self
            .shared
            .retention_next
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if next.is_some_and(|at| now < at) {
            return Ok(());
        }
        // Before pruning, while the journal still names the directories
        // earlier writes went to.
        {}
        let config = &self.config.retention;
        let payloads = self.prune_payloads(now, millis(config.payloads))?;
        let runs = self.prune(now, millis(config.runs))?;
        let history_full = crate::run_retention::prune_history(
            self.store(),
            now.saturating_sub(millis(config.history)),
            now.saturating_sub(millis(self.config.rate.window)),
            |flow| {
                self.shared
                    .staged_hosts
                    .as_ref()
                    .expect("recovered run registry")
                    .registry()
                    .audit_at(flow)
            },
        )?;
        let events_full = self
            .store()
            .write(|tx| crate::events::prune_receipts(tx, now))?
            == BATCH;
        let backlog =
            payloads == BATCH || runs.pruned.len() == BATCH || history_full || events_full;
        let interval = if backlog {
            config.interval.min(Duration::from_secs(1))
        } else {
            config.interval
        };
        *next = Some(now.saturating_add(millis(interval).max(1)));
        Ok(())
    }
}

/// The `fs.write` temporary files this process is writing now, and whether
/// any record may have outlived its write.
///
/// A record is only ever removed with its file by [`Runtime::sweep_fs_temps`]
/// once no write in this process holds it. Holding the `live` lock while a
/// record is checked and removed means a write of the same call cannot
/// start in between: a write marks its call live before it records the
/// file, so the sweep either sees it live and leaves it, or finishes first.
pub(crate) struct FsTemps {
    /// Call key to the number of writes in this process holding it.
    live: Mutex<HashMap<String, usize>>,
    /// Whether a record may be waiting for cleanup: true at startup, since
    /// a previous process may have died mid-write, and set again whenever a
    /// write ends without clearing its record.
    pending: AtomicBool,
}

impl Default for FsTemps {
    fn default() -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            pending: AtomicBool::new(true),
        }
    }
}

impl FsTemps {
    fn live(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn enter(&self, key: &str) {
        *self.live().entry(key.to_owned()).or_default() += 1;
    }

    fn leave(&self, key: &str, uncleared: bool) {
        let mut live = self.live();
        if uncleared {
            self.pending.store(true, Ordering::SeqCst);
        }
        if let Some(count) = live.get_mut(key) {
            *count -= 1;
            if *count == 0 {
                live.remove(key);
            }
        }
    }
}

/// The ledger `fs.write` records its temporary files in: the runtime's own
/// store. It holds the runtime weakly, like the completion sink, so a host
/// outliving the runtime does not keep the store open.
pub(crate) fn fs_temp_ledger(shared: &Arc<Shared>) -> Arc<dyn TempLedger> {
    Arc::new(FsTempLedger {
        shared: Arc::downgrade(shared),
    })
}

struct FsTempLedger {
    shared: Weak<Shared>,
}

impl TempLedger for FsTempLedger {
    fn record(&self, lease: &TempLease) -> std::result::Result<Box<dyn TempHold>, String> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| "the runtime is gone".to_owned())?;
        shared.fs_temps.enter(&lease.call_key);
        // From here on, dropping the hold releases the call again.
        let hold = FsTempHold {
            shared: self.shared.clone(),
            key: lease.call_key.clone(),
            dir: os_bytes(lease.dir.as_os_str()),
            cleared: false,
        };
        let roots = serde_json::to_string(&lease.roots).map_err(|e| e.to_string())?;
        shared
            .store
            .write(|tx| {
                tx.execute(
                    "INSERT OR REPLACE INTO fs_temps (call_key, dir, target, temp, roots) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        lease.call_key,
                        hold.dir,
                        os_bytes(&lease.target),
                        lease.temp.to_string_lossy(),
                        roots
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        Ok(Box::new(hold))
    }
}

struct FsTempHold {
    shared: Weak<Shared>,
    key: String,
    dir: Vec<u8>,
    cleared: bool,
}

impl TempHold for FsTempHold {
    fn clear(mut self: Box<Self>) {
        if let Some(shared) = self.shared.upgrade() {
            self.cleared = shared
                .store
                .write(|tx| {
                    tx.execute(
                        "DELETE FROM fs_temps WHERE call_key = ?1 AND dir = ?2",
                        params![self.key, self.dir],
                    )?;
                    Ok(())
                })
                .is_ok();
        }
    }
}

impl Drop for FsTempHold {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.fs_temps.leave(&self.key, !self.cleared);
        }
    }
}

impl Runtime {
    /// Removes every recorded `fs.write` temporary file that no write in
    /// this process holds, and its record: what a cancelled, failed or
    /// expired run's write, or a crash at any point, left behind. Removal
    /// is [`host_fs::remove_temp`]'s: only a regular file with exactly the
    /// recorded name, in exactly the recorded directory, inside the write's
    /// roots. A record that cannot be acted on safely is dropped without
    /// touching anything; one whose removal failed for a reason that may
    /// pass is kept for the next pass. Costs nothing while no record can be
    /// waiting. Returns how many records were settled.
    pub fn sweep_fs_temps(&self) -> Result<usize> {
        let temps = &self.shared.fs_temps;
        if !temps.pending.swap(false, Ordering::SeqCst) {
            return Ok(0);
        }
        let swept = self.sweep_fs_temps_batch();
        if !matches!(swept, Ok((_, true))) {
            temps.pending.store(true, Ordering::SeqCst);
        }
        swept.map(|(settled, _)| settled)
    }

    /// One bounded pass; also says whether every record it saw is settled
    /// or held by a write in this process.
    fn sweep_fs_temps_batch(&self) -> Result<(usize, bool)> {
        type Row = (String, Vec<u8>, Vec<u8>, String, String);
        let rows: Vec<Row> = self.store().read(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT call_key, dir, target, temp, roots FROM fs_temps LIMIT {BATCH}"
            ))?;
            Ok(stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let mut done = rows.len() < BATCH;
        let mut settled = 0;
        for (call_key, dir, target, temp, roots) in rows {
            let live = self.shared.fs_temps.live();
            if live.contains_key(&call_key) {
                // The write holding it clears it, or marks it pending when
                // it ends without doing so.
                continue;
            }
            let removal = match (
                serde_json::from_str::<Vec<String>>(&roots),
                os_from_bytes(dir.clone()),
                os_from_bytes(target),
            ) {
                (Ok(roots), Some(record_dir), Some(target)) => host_fs::remove_temp(&TempLease {
                    call_key: call_key.clone(),
                    dir: PathBuf::from(record_dir),
                    target,
                    temp: OsString::from(temp),
                    roots,
                }),
                // No roots to check a path against: nothing may be removed.
                (Err(_), _, _) => Ok(host_fs::TempRemoval::Refused(
                    basal_host::builtins::Denial::invalid("unreadable roots"),
                )),
                // Bytes `os_bytes` cannot have produced, so the record names no
                // file a write created: nothing may be removed.
                _ => Ok(host_fs::TempRemoval::Refused(
                    basal_host::builtins::Denial::invalid("unreadable path"),
                )),
            };
            match removal {
                Ok(outcome) => {
                    if let host_fs::TempRemoval::Refused(why) = &outcome {
                        tracing::warn!(
                            call_key = %call_key,
                            reason = %why.message,
                            "fs.write temporary file left in place: its record no longer names a file basal may remove"
                        );
                    }
                    self.store().write(|tx| {
                        tx.execute(
                            "DELETE FROM fs_temps WHERE call_key = ?1 AND dir = ?2",
                            params![call_key, dir],
                        )?;
                        Ok(())
                    })?;
                    settled += 1;
                }
                Err(_) => done = false,
            }
            drop(live);
        }
        Ok((settled, done))
    }
}

/// A path or file name as stored in an `fs_temps` record. On Unix these are
/// the raw bytes, which every record already written holds. On Windows,
/// where a name is a sequence of UTF-16 units that need not be valid
/// Unicode, they are those units in little-endian order, so any name
/// round-trips exactly.
#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

/// The inverse of [`os_bytes`]. `None` for bytes it cannot have produced (an
/// odd length on Windows), which name nothing basal may remove.
#[cfg(unix)]
fn os_from_bytes(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes))
}

#[cfg(windows)]
fn os_from_bytes(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect();
    Some(OsString::from_wide(&units))
}

#[cfg(test)]
mod os_bytes_tests {
    use super::*;

    #[test]
    fn stored_names_round_trip_exactly() {
        for name in ["", "plain", "résumé 日本語", ".basal-1-2.tmp"] {
            let value = OsString::from(name);
            assert_eq!(os_from_bytes(os_bytes(&value)), Some(value));
        }
    }

    /// Records written before this encoding existed hold the raw bytes, so
    /// on Unix the stored form must still be exactly those bytes.
    #[cfg(unix)]
    #[test]
    fn unix_names_are_stored_as_their_raw_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let raw = vec![b'a', 0xff, b'/', b'b'];
        assert_eq!(os_bytes(&OsString::from_vec(raw.clone())), raw);
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_keep_unpaired_surrogates_and_refuse_odd_lengths() {
        use std::os::windows::ffi::OsStringExt;
        let value = OsString::from_wide(&[0xd800, u16::from(b'a')]);
        assert_eq!(os_bytes(&value), [0x00, 0xd8, b'a', 0x00]);
        assert_eq!(os_from_bytes(os_bytes(&value)), Some(value));
        assert_eq!(os_from_bytes(vec![b'a']), None);
    }
}
