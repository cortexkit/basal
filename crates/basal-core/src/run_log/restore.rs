//! Restore is a startup operation, before admission, registry recovery or host attach.
//! The directory lock covers detection, marking, quarantine, epoch creation,
//! completion and the lifetime of the returned writer.

use super::*;
use crate::{
    Store,
    run_store_sql::{self as sql, EpochLsn, GcFloor},
};
use rusqlite::OptionalExtension;

fn db(error: crate::CoreError) -> Error {
    match error {
        crate::CoreError::Cut => Error::Cut,
        other => Error::Registry(other.to_string()),
    }
}

fn at(config: &Config, boundary: Boundary) -> Result<()> {
    if config.hooks.at("", &boundary) == Step::Crash {
        Err(Error::Cut)
    } else {
        Ok(())
    }
}

// Observe even foreign epochs: a restored SQLite counter must not reuse a
// sequence that is still visible in any surviving segment or run identity.
fn observe<R: LogRecord>(root: &Path, max_sequence: &mut u64, next_run: &mut u64) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.file_name().to_string_lossy().starts_with('e') {
            continue;
        }
        for file in std::fs::read_dir(entry.path())? {
            let file = file?;
            let name = file.file_name();
            let name = name.to_string_lossy();
            let Some(sequence) = name
                .strip_suffix(".seg")
                .and_then(|s| u64::from_str_radix(s, 16).ok())
            else {
                continue;
            };
            *max_sequence = (*max_sequence).max(sequence);
            let mut file = File::open(file.path())?;
            if Header::read(&mut file).is_err() {
                continue;
            }
            while let Ok(Some(frame)) = format::read_frame(&mut file) {
                if let Ok(record) = R::decode(frame.kind, &frame.payload)
                    && let Some(id) = record.run_id()
                    && let Some(sequence) =
                        id.rsplit('-').nth(1).and_then(|s| s.parse::<u64>().ok())
                {
                    *next_run = (*next_run).max(
                        sequence
                            .checked_add(1)
                            .ok_or(Error::Corrupt("run sequence exhausted"))?,
                    );
                }
            }
        }
    }
    Ok(())
}

impl<R: LogRecord> RunLog<R> {
    /// Open the log-backed runtime before recovering actors. Production journal
    /// construction does not call this method. Missing logs are not initialized by
    /// ordinary `open`; this entry point records history loss before replacing one.
    pub fn open_restoring(mut config: Config, store: Arc<Store>) -> Result<(Self, Recovered<R>)> {
        let absent = !config.directory.is_dir();
        platform::mkdir(&config.directory)?;
        if let Some(parent) = config.directory.parent() {
            platform::sync_dir(parent)?;
        }
        let lock = Arc::new(platform::DirectoryLock::acquire(
            &config.directory.join(".lock"),
        )?);
        let mut meta = store.read(sql::meta).map_err(db)?;
        config.identity.store_id = meta
            .store_id
            .as_bytes()
            .try_into()
            .map_err(|_| Error::Registry("store identity must contain 16 bytes".into()))?;
        config.identity.log_epoch = meta.log_epoch;
        let pending = store.read(sql::restore_pending).map_err(db)?;
        let registry = Arc::new(SqliteRegistry::new(store.clone(), meta.log_epoch));
        registry.check_lease(config.lease_epoch)?;
        let mut foreign_epoch = false;
        for entry in std::fs::read_dir(&config.directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(epoch) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.strip_prefix('e'))
                    .and_then(|s| s.parse::<u64>().ok())
                && epoch != meta.log_epoch
            {
                foreign_epoch = true;
            }
        }
        if !pending && !absent && !foreign_epoch {
            match Engine::open_locked::<R>(
                config.clone(),
                registry.clone(),
                false,
                Some(lock.clone()),
            ) {
                Ok((mut engine, recovered)) => {
                    if engine.header.lease != config.lease_epoch || engine.sealed {
                        engine.ensure_spare()?;
                        engine.rotate()?;
                    }
                    engine.ensure_spare()?;
                    return Ok((Self::spawn(engine), recovered));
                }
                Err(Error::RestoreRequired(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if !pending {
            let mut max_sequence = meta
                .log_hwm
                .lsn
                .sequence
                .max(meta.gc_floor.first_retained)
                .max(meta.gc_floor.synced_lsn.sequence);
            let mut next_run = 1;
            observe::<R>(&config.directory, &mut max_sequence, &mut next_run)?;
            store.write(|tx| {
                let mut state = sql::meta(tx)?;
                let max_registered: Option<u64> = tx.query_row("SELECT MAX(seq) FROM run_log_segments", [], |r| r.get(0))?;
                max_sequence = max_sequence.max(max_registered.unwrap_or(0));
                let max_admit: Option<u64> = tx.query_row("SELECT MAX(admit_segment) FROM live_runs", [], |r| r.get(0))?;
                max_sequence = max_sequence.max(max_admit.unwrap_or(0));
                let sequence = max_sequence.checked_add(1).ok_or_else(|| crate::CoreError::Corrupt("segment sequence exhausted".into()))?;
                let epoch = state.log_epoch.checked_add(1).ok_or_else(|| crate::CoreError::Corrupt("log epoch exhausted".into()))?;
                tx.execute("UPDATE live_runs SET history_lost=1", [])?;
                tx.execute("DELETE FROM run_log_segments", [])?;
                state.log_epoch = epoch;
                state.log_hwm = EpochLsn { log_epoch: epoch, lsn: Lsn { sequence, offset: 0 } };
                state.gc_floor = GcFloor { log_epoch: epoch, first_retained: sequence, predecessor: None, synced_lsn: state.log_hwm.lsn, live_starts: BTreeMap::new() };
                sql::write_meta(tx, &state)?;
                let current: Option<String> = tx.query_row("SELECT value FROM meta WHERE key='next_run'", [], |r| r.get(0)).optional()?;
                let current = current.unwrap_or_else(|| "1".into()).parse::<u64>().map_err(|_| crate::CoreError::Corrupt("invalid next_run".into()))?;
                tx.execute("INSERT INTO meta(key,value) VALUES('next_run',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [current.max(next_run).to_string()])?;
                sql::set_restore_pending(tx, true)
            }).map_err(db)?;
            at(&config, Boundary::RestoreMarked)?;
            meta = store.read(sql::meta).map_err(db)?;
        }
        config.identity.log_epoch = meta.log_epoch;
        let registry = Arc::new(SqliteRegistry::new(store.clone(), meta.log_epoch));
        registry.check_lease(config.lease_epoch)?;
        let initialized = !registry.snapshot()?.segments.is_empty();
        let quarantine = config
            .directory
            .join(format!("restored-{}", meta.log_epoch - 1));
        platform::mkdir(&quarantine)?;
        for entry in std::fs::read_dir(&config.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !entry.file_type()?.is_dir() || !name.starts_with('e') {
                continue;
            }
            if initialized && name == format!("e{}", meta.log_epoch) {
                continue;
            }
            let mut destination = quarantine.join(name.as_ref());
            let mut suffix = 0u64;
            while destination.exists() {
                suffix += 1;
                destination = quarantine.join(format!("{name}-{suffix}"));
            }
            registry.check_lease(config.lease_epoch)?;
            std::fs::rename(entry.path(), destination)?;
        }
        platform::sync_dir(&quarantine)?;
        platform::sync_dir(&config.directory)?;
        at(&config, Boundary::RestoreQuarantined)?;
        let (mut engine, recovered) =
            Engine::open_locked::<R>(config.clone(), registry, !initialized, Some(lock))?;
        if engine.header.lease != config.lease_epoch || engine.sealed {
            engine.ensure_spare()?;
            engine.rotate()?;
        }
        engine.ensure_spare()?;
        at(&config, Boundary::RestoreStarted)?;
        store
            .write(|tx| sql::set_restore_pending(tx, false))
            .map_err(db)?;
        at(&config, Boundary::RestoreFinished)?;
        Ok((Self::spawn(engine), recovered))
    }
}
