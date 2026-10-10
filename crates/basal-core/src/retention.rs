//! Retention in bounded transactions, on the runtime's deadline-service cadence.
//!
//! Terminal runs, journal, mailbox, activations and Broca snapshots are kept
//! for seven days by default, and longer while any call, token reservation or
//! open decision card needs them. Quarantine, call/audit history, delivered
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

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use basal_host::builtins::fs::{self as host_fs, TempHold, TempLease, TempLedger};

use crate::admission::trigger_key;
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
    pub history: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            runs: Duration::from_secs(7 * 24 * 3600),
            history: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

// Kept in SQL so permanently unsettled old runs do not consume every slot
// of the bounded candidate batch and starve later, prunable runs.
pub(crate) const OBLIGATIONS: &str = "EXISTS (SELECT 1 FROM journal j WHERE j.run_id=r.run_id AND j.settlement IS NULL \
    AND NOT EXISTS (SELECT 1 FROM mailbox m WHERE m.run_id=j.run_id AND m.position=j.position) \
    AND NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.run_id=j.run_id AND q.position=j.position AND q.reason='run_cancelled')) \
    OR EXISTS (SELECT 1 FROM token_ledger t WHERE t.run_id=r.run_id AND t.state='reserved') \
    OR EXISTS (SELECT 1 FROM decision_cards d WHERE d.run_id=r.run_id AND d.state='open') \
    OR EXISTS (SELECT 1 FROM broca_calls b WHERE b.run_id=r.run_id AND COALESCE(json_extract(b.snapshot,'$.acknowledged'),0)=0) \
    OR EXISTS (SELECT 1 FROM outbox o WHERE o.kind='refusal_committed' AND CAST(json_extract(o.body,'$.run_id') AS TEXT)=r.run_id)";

// Unary + leaves the ordering unchanged, but prevents SQLite from choosing an
// ordered full-table scan just to satisfy LIMIT. Search the age range first.
// The templates are public constants so the query-plan tests check the exact
// SQL this module runs, and fail if the two drift apart.
macro_rules! candidates_sql {
    () => {
        "SELECT r.run_id FROM runs r WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') \
         AND r.ended_at<=?1 AND {predicate} ({OBLIGATIONS}) ORDER BY +r.run_id LIMIT {BATCH}"
    };
}
/// Run candidate query template; substitutes `predicate`, `OBLIGATIONS` and `BATCH`.
pub const CANDIDATES_SQL: &str = candidates_sql!();

macro_rules! history_batch_sql {
    () => {
        "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {predicate} ORDER BY +rowid LIMIT {BATCH})"
    };
}
/// Bounded history deletion template; substitutes `table`, `predicate` and `BATCH`.
pub const HISTORY_BATCH_SQL: &str = history_batch_sql!();

pub(crate) const HISTORY_PREDICATES: &[(&str, &str)] = &[
    (
        "quarantine",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=quarantine.run_id)",
    ),
    (
        "call_audit",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=call_audit.run_id)",
    ),
    (
        "audit",
        "at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=audit.run_id) AND NOT EXISTS (SELECT 1 FROM decision_cards d WHERE d.elicitation_id=audit.elicitation_id AND d.state='open')",
    ),
    ("outbox", "delivered_at<=?1 AND kind<>'refusal_committed'"),
    ("schedule_dropped", "at<=?1"),
    (
        "token_ledger",
        "state='settled' AND settled_at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=token_ledger.run_id)",
    ),
    (
        "token_windows",
        "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM token_ledger t WHERE t.flow_id=token_windows.flow_id AND t.window_ms=token_windows.window_ms AND t.window_start=token_windows.window_start AND (t.state='reserved' OR EXISTS (SELECT 1 FROM runs r WHERE r.run_id=t.run_id)))",
    ),
    (
        "decision_cards",
        "state<>'open' AND answered_at<=?1 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=decision_cards.run_id) AND (kind<>'reenable' OR EXISTS (SELECT 1 FROM decision_cards newer WHERE newer.kind='reenable' AND newer.flow_id=decision_cards.flow_id AND newer.instance>decision_cards.instance))",
    ),
];

pub(crate) const RATE_WINDOWS_PREDICATE: &str = "window_start + window_ms<=?1 AND NOT EXISTS (SELECT 1 FROM call_audit a JOIN runs r USING(run_id) WHERE a.flow_id=rate_windows.flow_id AND a.at>=rate_windows.window_start AND a.at<rate_windows.window_start+rate_windows.window_ms)";

fn candidates(conn: &Connection, cutoff: i64, unsettled: bool) -> Result<Vec<String>> {
    let predicate = if unsettled { "" } else { "NOT" };
    let mut stmt = conn.prepare(&format!(
        candidates_sql!(),
        predicate = predicate,
        OBLIGATIONS = OBLIGATIONS,
        BATCH = BATCH,
    ))?;
    Ok(stmt
        .query_map([cutoff], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn prune_in(tx: &Transaction, cutoff: i64, now: i64, run_id: &str) -> Result<PruneReport> {
    // Recheck under the write transaction: an acknowledgement or a decision
    // card may have changed since the candidate scan released the connection.
    let candidate = tx
        .query_row(
            &format!(
                "SELECT r.flow_id,r.trigger_id,({OBLIGATIONS}) FROM runs r \
         WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') \
         AND r.ended_at<=?1 AND r.run_id=?2"
            ),
            params![cutoff, run_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((flow_id, trigger_id, unsettled)) = candidate else {
        return Ok(PruneReport::default());
    };
    if unsettled {
        return Ok(PruneReport {
            kept_unsettled: vec![run_id.to_owned()],
            ..PruneReport::default()
        });
    }
    // Tombstones outlive the run: a redelivered trigger is refused, and
    // the run's idempotency keys stay reserved.
    tx.execute(
        "INSERT OR IGNORE INTO tombstones (kind, key, run_id, pruned_at) VALUES ('trigger', ?1, ?2, ?3)",
        params![trigger_key(&flow_id, &trigger_id), run_id, now],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO tombstones (kind, key, run_id, pruned_at) \
         SELECT 'idempotency_key', idempotency_key, run_id, ?2 FROM journal \
         WHERE run_id = ?1 AND class IN ('mutation', 'keyed_mutation')",
        params![run_id, now],
    )?;
    tx.execute("DELETE FROM mailbox WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM journal WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM activations WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM trigger_inbox WHERE run_id = ?1", [run_id])?;
    tx.execute("DELETE FROM runs WHERE run_id = ?1", [run_id])?;
    tx.execute(
        "DELETE FROM meta WHERE key IN (?1,?2)",
        params![
            format!("journal_calls:{run_id}"),
            format!("journal_bytes:{run_id}")
        ],
    )?;
    Ok(PruneReport {
        pruned: vec![run_id.to_owned()],
        ..PruneReport::default()
    })
}

impl Runtime {
    /// Prunes terminal runs that ended at least `retention_ms` before
    /// `now_ms` and have no open obligation. Never touches a run that is
    /// not terminal (`needs_reconcile` included).
    pub fn prune(&self, now_ms: i64, retention_ms: i64) -> Result<PruneReport> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        let ids = self.store().read(|c| candidates(c, cutoff, false))?;
        let mut report = PruneReport {
            kept_unsettled: self.store().read(|c| candidates(c, cutoff, true))?,
            ..PruneReport::default()
        };
        for id in ids {
            // Release the single connection between runs; a batch can never
            // monopolize it for the whole retained history.
            let pruned = self.store().write(|tx| prune_in(tx, cutoff, now_ms, &id))?;
            report.pruned.extend(pruned.pruned);
            report.kept_unsettled.extend(pruned.kept_unsettled);
        }
        Ok(report)
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
        self.sweep_legacy_fs_temps()?;
        let config = &self.config.retention;
        let runs = self.prune(now, millis(config.runs))?;
        let history_full = self.prune_history(now, now.saturating_sub(millis(config.history)))?;
        let backlog = runs.pruned.len() == BATCH || history_full;
        let interval = if backlog {
            config.interval.min(Duration::from_secs(1))
        } else {
            config.interval
        };
        *next = Some(now.saturating_add(millis(interval).max(1)));
        Ok(())
    }

    fn prune_history(&self, now: i64, cutoff: i64) -> Result<bool> {
        let mut full = false;
        for &(table, predicate) in HISTORY_PREDICATES {
            full |= self.delete_history_batch(table, predicate, cutoff)? == BATCH;
        }
        let streak = millis(self.config.rate.window).saturating_mul(i64::from(
            self.config.rate.saturated_windows_to_disable.max(1),
        ));
        full |= self.delete_history_batch(
            "rate_windows",
            RATE_WINDOWS_PREDICATE,
            cutoff.min(now.saturating_sub(streak)),
        )? == BATCH;
        Ok(full)
    }

    fn delete_history_batch(&self, table: &str, predicate: &str, cutoff: i64) -> Result<usize> {
        // The names/predicates are private constants, never caller input.
        self.store().write(|tx| {
            Ok(tx.execute(
                &format!(
                    history_batch_sql!(),
                    table = table,
                    predicate = predicate,
                    BATCH = BATCH
                ),
                [cutoff],
            )?)
        })
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

/// Marks that every `fs.write` call's journaled directory has been checked
/// once for temporary files named the way earlier versions named them.
const LEGACY_FS_TEMPS_SWEPT: &str = "fs_temps_legacy_swept";

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

    /// Once per store: removes temporary files earlier versions of basal
    /// left, named `.basal-<process id>-<sequence>.tmp`, which no record
    /// describes. Only the directories journaled `fs.write` calls wrote to
    /// are examined, each opened and checked against the call's own roots,
    /// and only regular files with exactly that name shape are removed.
    /// Best effort: a directory that cannot be opened is skipped.
    fn sweep_legacy_fs_temps(&self) -> Result<()> {
        let done: bool = self.store().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
                [LEGACY_FS_TEMPS_SWEPT],
                |r| r.get(0),
            )?)
        })?;
        if done {
            return Ok(());
        }
        let writes: Vec<(String, String)> = self.store().read(|c| {
            let mut stmt = c.prepare(
                "SELECT DISTINCT json_extract(request, '$.args.path'), \
                 json_extract(request, '$.grant.roots') FROM journal \
                 WHERE kind_code = ?1 AND request IS NOT NULL AND json_valid(request)",
            )?;
            Ok(stmt
                .query_map([i64::from(basal_proto::Primitive::FsWrite.code())], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                })?
                .filter_map(|row| match row {
                    Ok((Some(path), Some(roots))) => Some(Ok((path, roots))),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (path, roots) in writes {
            let Ok(roots) = serde_json::from_str::<Vec<String>>(&roots) else {
                continue;
            };
            let _ = host_fs::remove_legacy_temps(&path, &roots);
        }
        self.store().write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
                [LEGACY_FS_TEMPS_SWEPT],
            )?;
            Ok(())
        })
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
