//! basal's SQLite store: open, durability settings, migrations, and the two
//! ways to touch it (a read, and a write transaction that commits or rolls
//! back as a whole).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cortexkit_store::{SqliteStore, StoreError, open_sqlite};
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::error::{CoreError, Result};
use crate::schema::{MIGRATIONS, NAMESPACE};

/// How hard a commit tries to reach stable storage.
///
/// The store always runs `synchronous = FULL`: an inbox commit precedes a
/// broker acknowledgement and an intent precedes a dispatch, so a commit that
/// a power loss could undo would break both promises. On macOS, `fsync` only
/// hands data to the drive, which may still hold it in a volatile cache;
/// `fullfsync` makes SQLite issue `F_FULLFSYNC`, which asks the drive to
/// flush that cache too. It is far more expensive, so it is a separate
/// choice: measured on an Apple M5 Max under heavy load, a small commit's
/// median was 5.8 ms with `F_FULLFSYNC` against 0.11 ms without, about 50
/// times more (`evidence/slice-2-measurements.json`, from `journal-bench`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Durability {
    pub fullfsync: bool,
}

impl Default for Durability {
    /// `F_FULLFSYNC` on every commit, so a power loss cannot undo a commit
    /// basal has already acted on. At about 6 ms per commit and three
    /// commits per new asynchronous call, that is a small share of flow
    /// calls that take tens of milliseconds to seconds.
    fn default() -> Self {
        Self { fullfsync: true }
    }
}

/// The pragmas actually in effect on the connection, read back after
/// setting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pragmas {
    /// SQLite's code: 2 is FULL.
    pub synchronous: i64,
    pub fullfsync: bool,
    pub checkpoint_fullfsync: bool,
    pub journal_mode_wal: bool,
}

pub struct Store {
    inner: SqliteStore,
    cut: AtomicBool,
    store_id: String,
    clock: Mutex<crate::clock::Clock>,
}

fn backend(e: StoreError) -> CoreError {
    CoreError::Store(e.to_string())
}

pub(crate) fn now_ms() -> i64 {
    crate::clock::write_time().unwrap_or_else(system_now_ms)
}

pub(crate) fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    /// Opens (creating if needed) the store at `path`, takes the
    /// single-writer lease, sets the durability pragmas and applies the
    /// schema.
    pub fn open(path: impl AsRef<Path>, durability: Durability) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let descriptor = StorageDescriptor {
            module_id: "basal".into(),
            storage_namespace: "core".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path.to_string_lossy().into_owned(),
            },
        };
        let inner = open_sqlite(&descriptor).map_err(backend)?;
        inner
            .with_conn(|c| {
                c.pragma_update(None, "synchronous", "FULL")?;
                c.pragma_update(None, "fullfsync", durability.fullfsync)?;
                c.pragma_update(None, "checkpoint_fullfsync", durability.fullfsync)
            })
            .map_err(backend)?;
        let outcome = inner.migrate(NAMESPACE, MIGRATIONS).map_err(backend)?;
        if outcome.store_ahead() {
            return Err(CoreError::Store(format!(
                "the store's schema is at version {} but this build knows only up to {}",
                outcome.recorded, outcome.chain_max
            )));
        }
        let mut store = Self {
            inner,
            cut: AtomicBool::new(false),
            store_id: String::new(),
            clock: Mutex::new(crate::clock::Clock::system()),
        };
        store.store_id = store.write(|tx| {
            let existing: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key = 'store_id'", [], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(id) = existing {
                return Ok(id);
            }
            // A per-store identity mixed into every run id, so run ids (and
            // the idempotency keys derived from them) are never reused even
            // if the store is recreated from scratch.
            let seed = format!(
                "{}:{}:{:?}",
                std::process::id(),
                now_ms(),
                SystemTime::now()
            );
            let id = blake3::hash(seed.as_bytes()).to_hex()[..16].to_owned();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('store_id', ?1)",
                [&id],
            )?;
            Ok(id)
        })?;
        Ok(store)
    }

    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    pub(crate) fn set_clock(&self, clock: crate::clock::Clock) {
        *self.clock.lock().unwrap_or_else(|p| p.into_inner()) = clock;
    }

    /// The durability pragmas as SQLite reports them.
    pub fn pragmas(&self) -> Result<Pragmas> {
        self.read(|c| {
            let synchronous: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            let fullfsync: i64 = c.query_row("PRAGMA fullfsync", [], |r| r.get(0))?;
            let checkpoint: i64 = c.query_row("PRAGMA checkpoint_fullfsync", [], |r| r.get(0))?;
            let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            Ok(Pragmas {
                synchronous,
                fullfsync: fullfsync != 0,
                checkpoint_fullfsync: checkpoint != 0,
                journal_mode_wal: mode.eq_ignore_ascii_case("wal"),
            })
        })
    }

    /// Cuts the store: every later read and write fails with
    /// [`CoreError::Cut`]. Simulates the process dying at this instant for
    /// anything still running in it, without closing the database.
    pub fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
    }

    pub fn is_cut(&self) -> bool {
        self.cut.load(Ordering::SeqCst)
    }

    fn check_cut(&self) -> Result<()> {
        if self.is_cut() {
            Err(CoreError::Cut)
        } else {
            Ok(())
        }
    }

    /// Runs `f` against the connection.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.check_cut()?;
        let mut failure = None;
        let out = self.inner.with_conn(|c| match f(c) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                failure = Some(e);
                Ok(None)
            }
        });
        match (out, failure) {
            (_, Some(e)) => Err(e),
            (Ok(Some(v)), None) => Ok(v),
            (Ok(None), None) => Err(CoreError::Store("read produced no value".into())),
            (Err(e), None) => Err(backend(e)),
        }
    }

    /// Runs `f` in one immediate write transaction, fenced by the store
    /// lease's epoch. Any error from `f` rolls the whole transaction back.
    pub fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        self.check_cut()?;
        let mut failure = None;
        let out = self.inner.with_conn_fenced(|tx| {
            let now = self
                .clock
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .now_ms();
            let _time = crate::clock::WriteTime::enter(now);
            match f(tx) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // `with_conn_fenced` rolls the transaction back when the
                    // closure returns an SQLite error, so save the closure's
                    // own error, return a placeholder SQLite error to trigger
                    // the rollback, and report the saved error to the caller
                    // instead of the placeholder.
                    failure = Some(e);
                    Err(rusqlite::Error::InvalidQuery)
                }
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        let value = out.map_err(backend)?;
        // A cut that landed while the transaction ran happened "after" the
        // commit: the caller must not act on it, exactly as a process that
        // died right after committing would not.
        self.check_cut()?;
        Ok(value)
    }
}
