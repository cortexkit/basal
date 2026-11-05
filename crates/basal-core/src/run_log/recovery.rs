use super::*;
use std::fs::OpenOptions;

#[derive(Debug)]
pub struct RecoveredRecord<R> {
    pub lsn: Lsn,
    pub record: R,
}

#[derive(Debug)]
pub struct Recovered<R> {
    pub records: Vec<RecoveredRecord<R>>,
    pub end: Lsn,
}

fn at(config: &Config, boundary: Boundary) -> Result<()> {
    match config.hooks.at("", &boundary) {
        Step::Continue => Ok(()),
        Step::Crash => Err(Error::Cut),
    }
}

fn new_file(
    config: &Config,
    registry: &dyn SegmentRegistry,
    directory: &Path,
    seq: u64,
) -> Result<(File, Header)> {
    registry.check_lease(config.lease_epoch)?;
    let mut file = platform::create(&segment_path(directory, seq))?;
    let header = Header::write(&mut file, config.identity, config.lease_epoch, seq)?;
    registry.check_lease(config.lease_epoch)?;
    platform::sync(&file)?;
    registry.check_lease(config.lease_epoch)?;
    platform::sync_dir(directory)?;
    at(config, Boundary::RunLogCreated)?;
    Ok((file, header))
}

fn truncate(
    config: &Config,
    registry: &dyn SegmentRegistry,
    file: &mut File,
    end: u64,
) -> Result<()> {
    registry.check_lease(config.lease_epoch)?;
    file.set_len(end)?;
    file.seek(SeekFrom::Start(end))?;
    registry.check_lease(config.lease_epoch)?;
    platform::sync(file)?;
    Ok(())
}

impl Engine {
    pub(super) fn open<R: LogRecord>(
        config: Config,
        registry: Arc<dyn SegmentRegistry>,
        initialize: bool,
    ) -> Result<(Self, Recovered<R>)> {
        Self::open_locked(config, registry, initialize, None)
    }

    pub(super) fn open_locked<R: LogRecord>(
        config: Config,
        registry: Arc<dyn SegmentRegistry>,
        initialize: bool,
        lock: Option<Arc<platform::DirectoryLock>>,
    ) -> Result<(Self, Recovered<R>)> {
        registry.check_lease(config.lease_epoch)?;
        if initialize {
            platform::mkdir(&config.directory)?;
            if let Some(parent) = config.directory.parent() {
                registry.check_lease(config.lease_epoch)?;
                platform::sync_dir(parent)?;
            }
        } else if !config.directory.is_dir() {
            return Err(Error::RestoreRequired("log directory absent"));
        }
        let lock = match lock {
            Some(lock) => lock,
            None => Arc::new(platform::DirectoryLock::acquire(
                &config.directory.join(".lock"),
            )?),
        };
        let directory = config
            .directory
            .join(format!("e{}", config.identity.log_epoch));
        if initialize {
            platform::mkdir(&directory)?;
            registry.check_lease(config.lease_epoch)?;
            platform::sync_dir(&config.directory)?;
        } else if !directory.is_dir() {
            return Err(Error::RestoreRequired("epoch directory absent"));
        }
        let mut snapshot = registry.snapshot()?;
        if initialize && !snapshot.segments.is_empty() {
            return Err(Error::Registry(
                "initialization requires an empty registry".into(),
            ));
        }
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(seq) = name
                .strip_suffix(".seg")
                .filter(|s| s.len() == 16)
                .and_then(|s| u64::from_str_radix(s, 16).ok())
            else {
                continue;
            };
            if !snapshot.segments.contains_key(&seq) {
                if seq < snapshot.first_retained {
                    registry.check_lease(config.lease_epoch)?;
                    std::fs::remove_file(entry.path())?;
                    registry.check_lease(config.lease_epoch)?;
                    platform::sync_dir(&directory)?;
                } else {
                    return Err(Error::RestoreRequired("unregistered segment"));
                }
            }
        }
        if initialize {
            let seq = snapshot.first_retained;
            registry.check_lease(config.lease_epoch)?;
            registry.reserve(config.lease_epoch, seq)?;
            at(&config, Boundary::RunLogReserved)?;
            snapshot.segments.insert(seq, SegmentState::Reserved);
        }
        if snapshot.segments.is_empty() {
            return Err(Error::RestoreRequired("empty segment registry"));
        }
        let known = snapshot.known();
        let mut previous = snapshot.predecessor;
        let mut last = None;
        let mut spare = None;
        let mut records = Vec::new();
        let mut contents = BTreeMap::new();
        let count = snapshot.segments.len();
        for (index, (&seq, &state)) in snapshot.segments.iter().enumerate() {
            if seq < snapshot.first_retained
                || (index == 0 && seq != snapshot.first_retained)
                || previous.is_some_and(|end| end.lsn.sequence.checked_add(1) != Some(seq))
            {
                return Err(Error::Corrupt("segment sequence gap"));
            }
            let reserved = state == SegmentState::Reserved;
            // A restored store may know an LSN beyond this old spare. Read its
            // real valid end first; the final end comparison detects that restore.
            // Rejecting known >= spare.header here would misclassify an early
            // log end as damage before the reserved-file repair rules can run.
            if reserved && index + 1 != count {
                return Err(Error::Corrupt("reserved segment is not last"));
            }
            let path = segment_path(&directory, seq);
            let opened = OpenOptions::new().read(true).write(true).open(&path);
            let mut file = match opened {
                Ok(file) => file,
                Err(e) if reserved && e.kind() == io::ErrorKind::NotFound => {
                    new_file(&config, registry.as_ref(), &directory, seq)?.0
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // The run-log design (run-log.md §6.3, Recovery reading)
                    // and its restore acceptance matrix require holes to fail
                    // closed: a later active segment proves a sealed predecessor
                    // was damaged. GC removes its row first. The design's §12
                    // missing-segment restore rule therefore applies only to an
                    // early end, never a hole in the chain.
                    if snapshot
                        .segments
                        .range((seq + 1)..)
                        .any(|(_, s)| *s == SegmentState::Active)
                    {
                        return Err(Error::Corrupt("registered sealed segment missing"));
                    }
                    return Err(Error::RestoreRequired("registered active segment missing"));
                }
                Err(e) => return Err(e.into()),
            };
            let header = match Header::read(&mut file) {
                Ok(header) => header,
                Err(Error::Corrupt(_)) if reserved => {
                    // Finish the file repair before touching any row.
                    // A restart after unlink or after creation repeats safely.
                    drop(file);
                    registry.check_lease(config.lease_epoch)?;
                    std::fs::remove_file(&path)?;
                    registry.check_lease(config.lease_epoch)?;
                    platform::sync_dir(&directory)?;
                    let (new, header) = new_file(&config, registry.as_ref(), &directory, seq)?;
                    file = new;
                    header
                }
                Err(e) => return Err(e),
            };
            if header.identity != config.identity {
                return Err(Error::RestoreRequired("foreign segment identity"));
            }
            if header.seq != seq {
                return Err(Error::Corrupt("header sequence mismatch"));
            }
            if header.lease > config.lease_epoch {
                return Err(Error::Fenced);
            }
            if last
                .as_ref()
                .is_some_and(|(_, h, _, _): &(File, Header, SegmentEnd, bool)| {
                    h.lease > header.lease
                })
            {
                return Err(Error::Corrupt("lease epochs run backwards"));
            }
            let mut end = SegmentEnd {
                lsn: Lsn {
                    sequence: seq,
                    offset: HEADER_LEN,
                },
                crc: header.crc,
            };
            let mut sealed = false;
            let mut runs = gc::Contents::new(end);
            if let Some(expected) = previous {
                let first = format::read_frame(&mut file);
                let link = match first {
                    Ok(Some(frame)) if frame.kind == LINK => {
                        format::decode::<SegmentEnd>(&frame.payload).map(|link| (frame, link))
                    }
                    Err(Error::Io(e)) => return Err(Error::Io(e)),
                    _ => Err(Error::Corrupt("missing Link")),
                };
                match link {
                    Ok((frame, link)) => {
                        if link != expected {
                            return Err(Error::Corrupt("broken Link chain"));
                        }
                        if last.as_ref().is_some_and(|(_, _, _, sealed)| !sealed) {
                            return Err(Error::Corrupt("unsealed predecessor"));
                        }
                        end = SegmentEnd {
                            lsn: Lsn {
                                sequence: seq,
                                offset: frame.end,
                            },
                            crc: frame.crc,
                        };
                    }
                    Err(_) if reserved => {
                        // The header-only spare stays reserved. The
                        // next rotation computes its Link from recovered end.
                        truncate(&config, registry.as_ref(), &mut file, HEADER_LEN)?;
                        if header.lease != config.lease_epoch {
                            drop(file);
                            registry.check_lease(config.lease_epoch)?;
                            std::fs::remove_file(&path)?;
                            registry.check_lease(config.lease_epoch)?;
                            platform::sync_dir(&directory)?;
                            new_file(&config, registry.as_ref(), &directory, seq)?;
                        }
                        spare = Some(seq);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            loop {
                let frame = match format::read_frame(&mut file) {
                    Ok(None) => break,
                    result if sealed => {
                        let _ = result;
                        return Err(Error::Corrupt("record after seal"));
                    }
                    Ok(Some(frame)) => frame,
                    Err(Error::Corrupt(_)) => {
                        if end.lsn < known
                            || index + 1 < count
                                && snapshot
                                    .segments
                                    .values()
                                    .skip(index + 1)
                                    .any(|s| *s == SegmentState::Active)
                        {
                            return Err(Error::Corrupt(
                                "torn tail below durable end or in sealed segment",
                            ));
                        }
                        truncate(&config, registry.as_ref(), &mut file, end.lsn.offset)?;
                        break;
                    }
                    Err(e) => return Err(e),
                };
                if frame.kind == LINK {
                    return Err(Error::Corrupt("unexpected Link"));
                }
                let decoded = if frame.kind == SEAL {
                    format::decode::<()>(&frame.payload).map(|()| None)
                } else if frame.kind > SEAL {
                    R::decode(frame.kind, &frame.payload).and_then(|record| {
                        if record.record_type() != frame.kind {
                            Err(Error::Corrupt("unknown record type"))
                        } else {
                            Ok(Some(record))
                        }
                    })
                } else {
                    Err(Error::Corrupt("unknown record type"))
                };
                let record = match decoded {
                    Ok(record) => record,
                    Err(_) => {
                        if end.lsn < known
                            || index + 1 < count
                                && snapshot
                                    .segments
                                    .values()
                                    .skip(index + 1)
                                    .any(|s| *s == SegmentState::Active)
                        {
                            return Err(Error::Corrupt(
                                "invalid record below durable end or in sealed segment",
                            ));
                        }
                        truncate(&config, registry.as_ref(), &mut file, end.lsn.offset)?;
                        break;
                    }
                };
                end = SegmentEnd {
                    lsn: Lsn {
                        sequence: seq,
                        offset: frame.end,
                    },
                    crc: frame.crc,
                };
                if let Some(record) = record {
                    runs.note(record.run_id());
                    records.push(RecoveredRecord {
                        lsn: end.lsn,
                        record,
                    });
                } else {
                    sealed = true;
                }
            }
            if reserved {
                // Make the repaired prefix durable before promoting.
                // Repeating this sync and transaction after a restart is safe.
                registry.check_lease(config.lease_epoch)?;
                platform::sync(&file)?;
                registry.check_lease(config.lease_epoch)?;
                registry.promote(config.lease_epoch, seq, end.lsn)?;
                at(&config, Boundary::RunLogPromoted)?;
            }
            file.seek(SeekFrom::Start(end.lsn.offset))?;
            runs.end = end;
            contents.insert(seq, runs);
            previous = Some(end);
            last = Some((file, header, end, sealed));
        }
        let (file, header, end, sealed) = last.ok_or(Error::Corrupt("no active segment"))?;
        if end.lsn < known {
            return Err(Error::RestoreRequired("log below durable end"));
        }
        // The valid recovered prefix is synced before a caller can depend on it.
        registry.check_lease(config.lease_epoch)?;
        platform::sync(&file)?;
        let recovered = Recovered {
            records,
            end: end.lsn,
        };
        Ok((
            Self {
                contents,
                config,
                registry,
                directory,
                _lock: lock,
                file,
                header,
                end,
                sealed,
                spare,
                synced: end.lsn,
                #[cfg(test)]
                sync_count: Arc::default(),
            },
            recovered,
        ))
    }
}
