//! The run log's SQLite binding: segment rows, the log watermark and the
//! reclamation floor. Construction never installs the schema; migrations do.

use super::*;
use crate::{
    CoreError, Store,
    run_store_sql::{self as sql, EpochLsn, GcFloor},
};
use rusqlite::{Connection, OptionalExtension};

pub struct SqliteRegistry {
    store: Arc<Store>,
    epoch: u64,
}

fn error(e: CoreError) -> Error {
    match e {
        CoreError::Cut => Error::Cut,
        other => Error::Registry(other.to_string()),
    }
}

fn live_admissions(conn: &Connection) -> crate::Result<BTreeMap<String, EpochLsn>> {
    let mut stmt =
        conn.prepare("SELECT run_id, admit_epoch, admit_segment, admit_offset FROM live_runs WHERE history_lost=0")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            EpochLsn {
                log_epoch: r.get(1)?,
                lsn: Lsn {
                    sequence: r.get(2)?,
                    offset: r.get(3)?,
                },
            },
        ))
    })?;
    rows.map(|r| r.map_err(CoreError::from)).collect()
}

impl SqliteRegistry {
    pub fn new(store: Arc<Store>, log_epoch: u64) -> Self {
        Self {
            store,
            epoch: log_epoch,
        }
    }

    fn metadata(&self, conn: &Connection) -> crate::Result<sql::Meta> {
        let meta = sql::meta(conn)?;
        if meta.store_id != self.store.store_id()
            || meta.log_epoch != self.epoch
            || meta.log_hwm.log_epoch != self.epoch
            || meta.gc_floor.log_epoch != self.epoch
        {
            return Err(CoreError::Invalid(
                "segment registry identity changed".into(),
            ));
        }
        Ok(meta)
    }

    fn write<T>(
        &self,
        expected: u64,
        f: impl FnOnce(&rusqlite::Transaction<'_>, sql::Meta) -> crate::Result<T>,
    ) -> Result<T> {
        crate::run_store::lock_order::assert_registry_allowed();
        if expected != self.store.lease_epoch() {
            return Err(Error::Fenced);
        }
        self.store
            .write(|tx| {
                // Store::write rechecks the persisted fence inside its IMMEDIATE
                // transaction. Also reject a caller configured for another lease.
                if expected != self.store.lease_epoch() {
                    return Err(CoreError::Invalid("segment writer lease changed".into()));
                }
                f(tx, self.metadata(tx)?)
            })
            .map_err(error)
    }
}

impl SegmentRegistry for SqliteRegistry {
    fn check_lease(&self, expected: u64) -> Result<()> {
        crate::run_store::lock_order::assert_registry_allowed();
        if expected != self.store.lease_epoch() {
            return Err(Error::Fenced);
        }
        let valid = self
            .store
            .read(|conn| {
                self.metadata(conn)?;
                let fence: u64 =
                    conn.query_row("SELECT epoch FROM cortexkit_fence WHERE id=0", [], |r| {
                        r.get(0)
                    })?;
                Ok(fence <= expected)
            })
            .map_err(error)?;
        if valid { Ok(()) } else { Err(Error::Fenced) }
    }

    fn snapshot(&self) -> Result<RegistrySnapshot> {
        crate::run_store::lock_order::assert_registry_allowed();
        self.store
            .read(|conn| {
                let meta = self.metadata(conn)?;
                Ok(RegistrySnapshot {
                    segments: sql::segments(conn, self.epoch)?
                        .into_iter()
                        .map(|s| (s.seq, s.state))
                        .collect(),
                    first_retained: meta.gc_floor.first_retained,
                    predecessor: meta.gc_floor.predecessor,
                    log_hwm: meta.log_hwm.lsn,
                    floor_synced: meta.gc_floor.synced_lsn,
                    admit_lsns: live_admissions(conn)?
                        .into_values()
                        .filter(|lsn| lsn.log_epoch == self.epoch)
                        .map(|lsn| lsn.lsn)
                        .collect(),
                })
            })
            .map_err(error)
    }

    fn reserve(&self, expected: u64, sequence: u64) -> Result<()> {
        self.write(expected, |tx, _| {
            sql::reserve_segment(tx, self.epoch, sequence)
        })
    }

    fn promote(&self, expected: u64, sequence: u64, synced: Lsn) -> Result<()> {
        self.write(expected, |tx, _| {
            if sequence != synced.sequence
                || !sql::promote_segment(
                    tx,
                    EpochLsn {
                        log_epoch: self.epoch,
                        lsn: synced,
                    },
                )?
            {
                return Err(CoreError::Invalid("unreserved segment promotion".into()));
            }
            Ok(())
        })
    }

    fn reclaim(
        &self,
        expected: u64,
        segments: &[ReclaimSegment],
        synced: Lsn,
        live_starts: &BTreeMap<String, Lsn>,
    ) -> Result<Option<SegmentEnd>> {
        self.write(expected, |tx, meta| {
            sql::advance_log_hwm(
                tx,
                EpochLsn {
                    log_epoch: self.epoch,
                    lsn: synced,
                },
            )?;
            let live = live_admissions(tx)?;
            let mut starts = BTreeMap::new();
            for (id, admit) in &live {
                if admit.log_epoch != self.epoch {
                    return Err(CoreError::Invalid(
                        "live admission from another log epoch".into(),
                    ));
                }
                // An admission racing the actor snapshot has no older records:
                // its first record must be after the admission's recorded LSN.
                let start = live_starts.get(id).copied().unwrap_or(admit.lsn);
                if start > synced {
                    return Ok(None);
                }
                starts.insert(id.clone(), start);
            }
            let registered = sql::segments(tx, self.epoch)?;
            let mut predecessor = None;
            let mut next = meta.gc_floor.first_retained;
            for segment in segments {
                let seq = segment.end.lsn.sequence;
                if seq != next
                    || segment.end.lsn > synced
                    || !registered
                        .iter()
                        .any(|s| s.seq == seq && s.state == SegmentState::Active)
                    || !registered.iter().any(|s| {
                        Some(s.seq) == seq.checked_add(1) && s.state == SegmentState::Active
                    })
                    || starts.values().any(|start| start.sequence <= seq)
                {
                    break;
                }
                let mut committed = true;
                for run in &segment.runs {
                    // Retention removes finished histories but keeps their
                    // trigger/write tombstones as proof of the terminal commit.
                    if !live.contains_key(run)
                        && tx
                            .query_row(
                                "SELECT 1 FROM run_history WHERE run_id=?1
                                 UNION ALL SELECT 1 FROM tombstones WHERE run_id=?1 LIMIT 1",
                                [run],
                                |_| Ok(()),
                            )
                            .optional()?
                            .is_none()
                    {
                        committed = false;
                        break;
                    }
                }
                if !committed {
                    break;
                }
                predecessor = Some(segment.end);
                next = seq
                    .checked_add(1)
                    .ok_or_else(|| CoreError::Invalid("segment sequence overflow".into()))?;
            }
            if let Some(end) = predecessor {
                sql::write_gc_floor(
                    tx,
                    &GcFloor {
                        log_epoch: self.epoch,
                        first_retained: next,
                        predecessor: Some(end),
                        synced_lsn: synced,
                        live_starts: starts,
                    },
                )?;
            }
            Ok(predecessor)
        })
    }
}
