//! Append-only run-log mechanism. Production stays on the SQLite run journal
//! until live run state moves to an in-memory registry backed by this log.
//! The registry adapter owns short control-plane transactions; this module owns
//! files and never holds a registry lock across a sync wait.

mod format;
mod gc;
mod platform;
mod recovery;
mod registry;
mod restore;
pub use registry::SqliteRegistry;
#[cfg(test)]
mod tests;

use crate::{Boundary, Hooks, Step};
pub use format::SegmentEnd;
use format::{HEADER_LEN, Header, LINK, SEAL};
pub use recovery::{Recovered, RecoveredRecord};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io(String),
    Corrupt(&'static str),
    RestoreRequired(&'static str),
    Fenced,
    Cut,
    Stopped,
    Registry(String),
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Offsets name the end of a complete record (or of an empty segment's header).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Lsn {
    pub sequence: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub store_id: [u8; 16],
    pub log_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentState {
    Reserved,
    Active,
}

/// Rows and LSNs are scoped to the configured log epoch. The registry adapter
/// must exclude durable markers from other epochs before building this snapshot.
#[derive(Debug, Clone)]
pub struct RegistrySnapshot {
    pub segments: BTreeMap<u64, SegmentState>,
    pub first_retained: u64,
    /// Final end of the deleted predecessor, used to anchor the first Link.
    pub predecessor: Option<SegmentEnd>,
    pub log_hwm: Lsn,
    pub floor_synced: Lsn,
    pub admit_lsns: Vec<Lsn>,
}
impl RegistrySnapshot {
    fn known(&self) -> Lsn {
        self.admit_lsns
            .iter()
            .copied()
            .chain([self.log_hwm, self.floor_synced])
            .max()
            .unwrap_or_default()
    }
}

/// Implementations must recheck the lease inside each reserve/promote transaction,
/// and promote atomically advances the log high-water mark (`log_hwm`). Calls
/// must release the connection before returning. The file writer holds neither
/// per-run state locks nor per-flow budget-accounting locks.
pub trait SegmentRegistry: Send + Sync + 'static {
    fn check_lease(&self, expected: u64) -> Result<()>;
    fn snapshot(&self) -> Result<RegistrySnapshot>;
    fn reserve(&self, expected: u64, sequence: u64) -> Result<()>;
    fn promote(&self, expected: u64, sequence: u64, synced: Lsn) -> Result<()>;
    /// Commit the longest reclaimable prefix and return its deleted predecessor.
    /// The writer supplies sealed ends; the adapter rechecks live/history rows.
    fn reclaim(
        &self,
        _expected: u64,
        _segments: &[ReclaimSegment],
        _synced: Lsn,
        _live_starts: &BTreeMap<String, Lsn>,
    ) -> Result<Option<SegmentEnd>> {
        Err(Error::Registry("reclamation is not bound".into()))
    }
}

/// The records/replay layer supplies its closed set of record types. Types 1
/// and 2 are reserved for Link and Seal; unknown types must return an error.
/// The frame type must agree with the decoded record's type, so a valid payload
/// cannot disguise an unknown type byte.
pub trait LogRecord: Serialize + DeserializeOwned + Send + 'static {
    fn record_type(&self) -> u8;
    fn decode(record_type: u8, payload: &[u8]) -> Result<Self>;
    /// Anonymous record formats cannot be reclaimed: SQLite cannot prove that
    /// their effects were committed. Run records return their full run ID.
    fn run_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ReclaimSegment {
    pub end: SegmentEnd,
    pub runs: std::collections::BTreeSet<String>,
}

/// A nonblocking view of the published, epoch-qualified sync watermark.
#[derive(Clone)]
pub struct Watermark {
    epoch: u64,
    synced: Arc<Mutex<Lsn>>,
}
impl crate::run_admission::AdmissionLog for Watermark {
    fn synced_lsn(&self) -> crate::run_store_sql::EpochLsn {
        crate::run_store_sql::EpochLsn {
            log_epoch: self.epoch,
            lsn: *self.synced.lock().unwrap(),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    /// The run-log directory, not the production store root.
    pub directory: PathBuf,
    pub identity: Identity,
    pub lease_epoch: u64,
    pub target_size: u64,
    pub hooks: Arc<dyn Hooks>,
}
impl Config {
    pub const TARGET_SIZE: u64 = 8 * 1024 * 1024;
}

enum Command<R> {
    Reclaim(BTreeMap<String, Lsn>, mpsc::Sender<Result<usize>>),
    Append(Vec<R>, mpsc::Sender<Result<Lsn>>),
    Sync(Lsn, mpsc::Sender<Result<Lsn>>),
    Rotate(mpsc::Sender<Result<Lsn>>),
    Stop,
}

pub struct RunLog<R: LogRecord> {
    tx: mpsc::Sender<Command<R>>,
    worker: Option<thread::JoinHandle<()>>,
    synced: Arc<Mutex<Lsn>>,
    epoch: u64,
    #[cfg(test)]
    sync_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl<R: LogRecord> RunLog<R> {
    /// Explicit initialization is for a fresh epoch only. The caller performing
    /// journal migration or store-restore recovery decides when it is safe;
    /// ordinary open never initializes missing logs.
    pub fn create(config: Config, registry: Arc<dyn SegmentRegistry>) -> Result<Self> {
        let (mut engine, recovered) = Engine::open::<R>(config, registry, true)?;
        debug_assert!(recovered.records.is_empty());
        engine.ensure_spare()?;
        Ok(Self::spawn(engine))
    }

    pub fn open(
        config: Config,
        registry: Arc<dyn SegmentRegistry>,
    ) -> Result<(Self, Recovered<R>)> {
        let (mut engine, recovered) = Engine::open(config, registry, false)?;
        // A new writer lease never appends to a segment created by an old one.
        if engine.header.lease != engine.config.lease_epoch || engine.sealed {
            engine.ensure_spare()?;
            engine.rotate()?;
        }
        engine.ensure_spare()?;
        Ok((Self::spawn(engine), recovered))
    }

    fn spawn(mut engine: Engine) -> Self {
        let epoch = engine.config.identity.log_epoch;
        let (tx, rx) = mpsc::channel();
        let synced = Arc::new(Mutex::new(engine.synced));
        let published = synced.clone();
        #[cfg(test)]
        let sync_count = engine.sync_count.clone();
        let worker = thread::spawn(move || {
            let mut fatal: Option<Error> = None;
            while let Ok(first) = rx.recv() {
                let mut commands = vec![first];
                // Bounded draining prevents an append flood from starving sync.
                commands.extend(rx.try_iter().take(1023));
                let mut waiters = Vec::new();
                let mut stop = false;
                for command in commands {
                    match command {
                        Command::Reclaim(starts, reply) => {
                            let result = match &fatal {
                                Some(e) => Err(e.clone()),
                                None => engine.reclaim(&starts),
                            };
                            if let Err(e) = &result {
                                fatal = Some(e.clone());
                            }
                            let _ = reply.send(result);
                        }
                        Command::Stop => {
                            stop = true;
                            break;
                        }
                        Command::Append(records, reply) => {
                            let result = match &fatal {
                                Some(e) => Err(e.clone()),
                                None => engine.append(&records),
                            };
                            if let Err(e) = &result {
                                fatal = Some(e.clone());
                            }
                            let _ = reply.send(result);
                        }
                        Command::Rotate(reply) => {
                            let result = match &fatal {
                                Some(e) => Err(e.clone()),
                                None => engine.ensure_spare().and_then(|()| engine.rotate()),
                            };
                            if let Err(e) = &result {
                                fatal = Some(e.clone());
                            }
                            waiters.push((engine.end.lsn, reply));
                        }
                        Command::Sync(lsn, reply) => waiters.push((lsn, reply)),
                    }
                }
                if !waiters.is_empty() && fatal.is_none() {
                    if waiters.iter().any(|(lsn, _)| *lsn > engine.end.lsn) {
                        fatal = Some(Error::Corrupt("sync past appended end"));
                    } else if let Err(e) = engine.sync() {
                        fatal = Some(e);
                    }
                }
                *published.lock().unwrap() = engine.synced;
                for (_, reply) in waiters {
                    let _ = reply.send(match &fatal {
                        Some(e) => Err(e.clone()),
                        None => Ok(engine.synced),
                    });
                }
                if stop {
                    break;
                }
                if fatal.is_none() {
                    // Replenish after publishing rotation's watermark, rather
                    // than putting the next reserve on a caller's sync path.
                    if let Err(e) = engine.ensure_spare() {
                        fatal = Some(e);
                    }
                }
            }
        });
        Self {
            tx,
            worker: Some(worker),
            synced,
            epoch,
            #[cfg(test)]
            sync_count,
        }
    }

    pub fn append(&self, records: Vec<R>) -> Result<Lsn> {
        // Rotation during append may wait for sync and registry transactions.
        crate::run_store::lock_order::assert_sync_allowed();
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Append(records, tx))
            .map_err(|_| Error::Stopped)?;
        rx.recv().map_err(|_| Error::Stopped)?
    }

    pub fn sync_through(&self, lsn: Lsn) -> Result<Lsn> {
        crate::run_store::lock_order::assert_sync_allowed();
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Sync(lsn, tx))
            .map_err(|_| Error::Stopped)?;
        rx.recv().map_err(|_| Error::Stopped)?
    }

    pub fn rotate(&self) -> Result<Lsn> {
        crate::run_store::lock_order::assert_sync_allowed();
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Rotate(tx))
            .map_err(|_| Error::Stopped)?;
        rx.recv().map_err(|_| Error::Stopped)?
    }

    pub fn synced_lsn(&self) -> Lsn {
        *self.synced.lock().unwrap()
    }

    pub fn watermark(&self) -> Watermark {
        Watermark {
            epoch: self.epoch,
            synced: self.synced.clone(),
        }
    }

    /// Snapshot required starts under actors first, then release them before
    /// calling this method. Starts may advance, but must never move backwards.
    pub fn reclaim(&self, live_starts: BTreeMap<String, Lsn>) -> Result<usize> {
        crate::run_store::lock_order::assert_lock_free();
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Reclaim(live_starts, tx))
            .map_err(|_| Error::Stopped)?;
        rx.recv().map_err(|_| Error::Stopped)?
    }
}

impl<R: LogRecord> Drop for RunLog<R> {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Engine {
    contents: BTreeMap<u64, gc::Contents>,
    config: Config,
    registry: Arc<dyn SegmentRegistry>,
    directory: PathBuf,
    _lock: Arc<platform::DirectoryLock>,
    file: File,
    header: Header,
    end: SegmentEnd,
    sealed: bool,
    spare: Option<u64>,
    synced: Lsn,
    #[cfg(test)]
    sync_count: Arc<std::sync::atomic::AtomicUsize>,
}

fn segment_path(directory: &Path, sequence: u64) -> PathBuf {
    directory.join(format!("{sequence:016x}.seg"))
}

impl Engine {
    fn check(&self) -> Result<()> {
        self.registry.check_lease(self.config.lease_epoch)
    }
    fn at(&self, boundary: Boundary) -> Result<()> {
        match self.config.hooks.at("", &boundary) {
            Step::Continue => Ok(()),
            Step::Crash => Err(Error::Cut),
        }
    }
    fn register(&self, sequence: u64) -> Result<()> {
        self.check()?;
        self.registry
            .promote(self.config.lease_epoch, sequence, self.end.lsn)
    }
    fn create_file(&self, sequence: u64) -> Result<(File, Header)> {
        self.check()?;
        let mut file = platform::create(&segment_path(&self.directory, sequence))?;
        let header = Header::write(
            &mut file,
            self.config.identity,
            self.config.lease_epoch,
            sequence,
        )?;
        self.check()?;
        platform::sync(&file)?;
        self.check()?;
        platform::sync_dir(&self.directory)?;
        self.at(Boundary::RunLogCreated)?;
        Ok((file, header))
    }
    fn ensure_spare(&mut self) -> Result<()> {
        if self.spare.is_some() {
            return Ok(());
        }
        let seq = self
            .header
            .seq
            .checked_add(1)
            .ok_or(Error::Corrupt("sequence overflow"))?;
        self.check()?;
        self.registry.reserve(self.config.lease_epoch, seq)?;
        self.at(Boundary::RunLogReserved)?;
        self.create_file(seq)?;
        self.spare = Some(seq);
        Ok(())
    }
    fn write_frame(&mut self, bytes: &[u8]) -> Result<Lsn> {
        if self.sealed {
            return Err(Error::Corrupt("append after seal"));
        }
        self.file.write_all(bytes)?;
        self.end = SegmentEnd {
            lsn: Lsn {
                sequence: self.header.seq,
                offset: self.end.lsn.offset + bytes.len() as u64,
            },
            crc: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        };
        self.contents.get_mut(&self.header.seq).unwrap().end = self.end;
        Ok(self.end.lsn)
    }
    fn append<R: LogRecord>(&mut self, records: &[R]) -> Result<Lsn> {
        self.check()?;
        for record in records {
            if record.record_type() <= SEAL {
                return Err(Error::Corrupt("reserved record type"));
            }
            let bytes = format::frame(record.record_type(), record)?;
            if self.end.lsn.offset + bytes.len() as u64 > self.config.target_size
                && self.end.lsn.offset > HEADER_LEN
            {
                self.ensure_spare()?;
                self.rotate()?;
            }
            self.check()?;
            self.write_frame(&bytes)?;
            self.contents
                .get_mut(&self.header.seq)
                .unwrap()
                .note(record.run_id());
        }
        self.at(Boundary::RunLogAppended)?;
        Ok(self.end.lsn)
    }
    fn sync_file(&self) -> Result<()> {
        self.check()?;
        platform::sync(&self.file)?;
        #[cfg(test)]
        self.sync_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        self.check()?;
        if self.end.lsn > self.synced {
            self.sync_file()?;
            self.synced = self.end.lsn;
        }
        Ok(())
    }
    fn rotate(&mut self) -> Result<Lsn> {
        let sequence = self
            .spare
            .ok_or(Error::Corrupt("rotation requires a created spare"))?;
        self.check()?;
        if !self.sealed {
            self.write_frame(&format::frame(SEAL, &())?)?;
            self.sealed = true;
        }
        self.sync_file()?;
        let previous = self.end;
        self.spare = None;
        let path = segment_path(&self.directory, sequence);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let header = Header::read(&mut file)?;
        file.seek(SeekFrom::Start(HEADER_LEN))?;
        self.file = file;
        self.header = header;
        self.end = SegmentEnd {
            lsn: Lsn {
                sequence,
                offset: HEADER_LEN,
            },
            crc: header.crc,
        };
        self.sealed = false;
        self.contents.insert(sequence, gc::Contents::new(self.end));
        self.check()?;
        self.write_frame(&format::frame(LINK, &previous)?)?;
        self.sync_file()?;
        self.at(Boundary::RunLogLinked)?;
        self.register(sequence)?;
        self.at(Boundary::RunLogPromoted)?;
        self.synced = self.end.lsn;
        Ok(self.synced)
    }
}
