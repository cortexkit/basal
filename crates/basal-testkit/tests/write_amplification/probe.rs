//! SQLite commit accounting for the ignored Linux write benchmark.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_int, c_void};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{Connection, ffi};
use serde_json::{Value, json};

#[derive(Clone, Copy, Default)]
pub struct Io {
    pub writes: u64,
    pub cancelled: u64,
    pub wchar: u64,
}

impl Io {
    pub fn read() -> Self {
        let text = std::fs::read_to_string("/proc/self/io").expect("Linux I/O accounting");
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .expect("I/O counter")
                .trim()
                .parse()
                .expect("numeric I/O counter")
        };
        Self {
            writes: field("write_bytes:"),
            cancelled: field("cancelled_write_bytes:"),
            wchar: field("wchar:"),
        }
    }

    pub fn delta(self, before: Self) -> Value {
        json!({
            "write_bytes": self.writes - before.writes,
            "cancelled_write_bytes": self.cancelled - before.cancelled,
            "wchar": self.wchar - before.wchar,
        })
    }
}

#[derive(Default)]
struct Total {
    transactions: u64,
    frames: u64,
    bytes: u64,
}

pub struct State {
    phase: String,
    statements: Vec<String>,
    last_key: Option<(String, Vec<String>)>,
    totals: BTreeMap<(String, Vec<String>), Total>,
    wal: PathBuf,
    salt: [u8; 8],
    previous_frames: u64,
    pending_frames: u64,
    last_io: u64,
    checkpoint_bytes: u64,
    pending_checkpoint_bytes: u64,
    checkpoints: u64,
    threshold: c_int,
    errors: Vec<String>,
}

/// Unregisters callbacks under the connection lock before freeing their context.
pub struct Probe<'a> {
    state: Box<Mutex<State>>,
    store: &'a basal_core::Store,
}

impl<'a> Probe<'a> {
    pub fn start(store: &'a basal_core::Store, wal: PathBuf, threshold: c_int) -> Self {
        let probe = Self {
            store,
            state: Box::new(Mutex::new(State {
                phase: "runs".into(),
                statements: Vec::new(),
                last_key: None,
                totals: BTreeMap::new(),
                wal,
                salt: [0; 8],
                previous_frames: 0,
                pending_frames: 0,
                last_io: Io::read().writes,
                checkpoint_bytes: 0,
                pending_checkpoint_bytes: 0,
                checkpoints: 0,
                threshold,
                errors: Vec::new(),
            })),
        };
        store
            .read(|conn| {
                probe.attach(conn);
                Ok(())
            })
            .unwrap();
        probe
    }

    fn attach(&self, conn: &Connection) {
        // SAFETY: the boxed mutex has a stable address, callbacks run under the
        // store's connection lock, and detach removes them before it is dropped.
        unsafe {
            let context = (&*self.state as *const Mutex<State>).cast_mut().cast();
            assert_eq!(
                ffi::sqlite3_trace_v2(
                    conn.handle(),
                    (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE) as u32,
                    Some(trace),
                    context,
                ),
                ffi::SQLITE_OK
            );
            ffi::sqlite3_wal_hook(conn.handle(), Some(wal), context);
        }
    }

    fn detach(&self, conn: &Connection) {
        // SAFETY: same live connection used by attach; no callback is in flight.
        unsafe {
            ffi::sqlite3_trace_v2(conn.handle(), 0, None, std::ptr::null_mut());
            ffi::sqlite3_wal_hook(conn.handle(), None, std::ptr::null_mut());
        }
    }

    pub fn phase(&self, phase: &str) {
        self.state.lock().unwrap().phase = phase.into();
    }

    pub fn checkpoint(&self, conn: &Connection) {
        let before = Io::read().writes;
        let (log, done): (i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(log, done, "no outstanding reader may hide checkpoint work");
        let after = Io::read().writes;
        let mut state = self.state.lock().unwrap();
        state.pending_checkpoint_bytes = 0;
        let phase = state.phase.clone();
        state
            .totals
            .entry((phase, vec!["checkpoint tail".into()]))
            .or_default()
            .bytes += after - before;
        state.last_io = after;
    }

    pub fn report(&self) -> Value {
        let state = self.state.lock().unwrap();
        assert!(state.errors.is_empty(), "{:?}", state.errors);
        let kinds: Vec<_> = state
            .totals
            .iter()
            .map(|((phase, statements), total)| {
                json!({"phase": phase, "sql": statements,
                    "transactions": total.transactions, "frames": total.frames,
                    "write_bytes": total.bytes})
            })
            .collect();
        json!({"kinds": kinds, "autocheckpoints": state.checkpoints,
            "autocheckpoint_write_bytes": state.checkpoint_bytes})
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        self.store
            .read(|conn| {
                self.detach(conn);
                // Restore the connection's original auto-checkpoint policy too.
                conn.pragma_update(
                    None,
                    "wal_autocheckpoint",
                    self.state.lock().unwrap().threshold,
                )?;
                Ok(())
            })
            .unwrap();
    }
}

// Callbacks cannot unwind through SQLite. Store any instrumentation failure and
// fail the benchmark after the hooks have been detached instead.
unsafe extern "C" fn trace(
    event: u32,
    context: *mut c_void,
    statement: *mut c_void,
    _: *mut c_void,
) -> c_int {
    let result = std::panic::catch_unwind(|| {
        // SAFETY: SQLite supplies a live prepared statement for these events;
        // attach supplies a live Mutex<State> for the callback context.
        let (sql, readonly, autocommit) = unsafe {
            let stmt = statement.cast::<ffi::sqlite3_stmt>();
            let sql = CStr::from_ptr(ffi::sqlite3_sql(stmt))
                .to_string_lossy()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (
                sql,
                ffi::sqlite3_stmt_readonly(stmt) != 0,
                ffi::sqlite3_get_autocommit(ffi::sqlite3_db_handle(stmt)) != 0,
            )
        };
        let control = sql.starts_with("BEGIN")
            || sql.starts_with("COMMIT")
            || sql.starts_with("ROLLBACK")
            || sql.starts_with("PRAGMA");
        let mutex = unsafe { &*context.cast::<Mutex<State>>() };
        let mut state = mutex.lock().unwrap();
        if event == ffi::SQLITE_TRACE_STMT as u32
            && (sql.starts_with("BEGIN") || (!readonly && !control && autocommit))
        {
            state.last_key = None;
        }
        if event == ffi::SQLITE_TRACE_STMT as u32 && !readonly && !control {
            state.statements.push(sql.clone());
        }
        if event == ffi::SQLITE_TRACE_PROFILE as u32
            && (sql.starts_with("COMMIT") || (!readonly && !control && autocommit))
        {
            let now = Io::read().writes;
            let bytes = now - state.last_io - state.pending_checkpoint_bytes;
            let key = (state.phase.clone(), std::mem::take(&mut state.statements));
            state.last_key = Some(key.clone());
            let frames = state.pending_frames;
            let total = state.totals.entry(key).or_default();
            total.transactions += 1;
            total.frames += frames;
            total.bytes += bytes;
            state.pending_frames = 0;
            state.pending_checkpoint_bytes = 0;
            state.last_io = now;
        }
    });
    if result.is_err() {
        // A poisoned lock or an unavailable /proc counter invalidates the run.
        if let Ok(mut state) = unsafe { &*context.cast::<Mutex<State>>() }.lock() {
            state.errors.push("trace callback failed".into());
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn wal(
    context: *mut c_void,
    db: *mut ffi::sqlite3,
    name: *const std::ffi::c_char,
    frames: c_int,
) -> c_int {
    let result = std::panic::catch_unwind(|| {
        let mut state = unsafe { &*context.cast::<Mutex<State>>() }.lock().unwrap();
        let mut header = [0; 32];
        std::fs::File::open(&state.wal)
            .and_then(|mut f| f.read_exact(&mut header))
            .expect("WAL header");
        let salt: [u8; 8] = header[16..24].try_into().unwrap();
        let frames = frames as u64;
        let added = if salt != state.salt {
            frames
        } else {
            frames - state.previous_frames
        };
        // SQLite can emit COMMIT's PROFILE event before invoking the WAL hook.
        // Attach frames to that completed transaction, not the following one.
        if let Some(key) = state.last_key.clone() {
            state.totals.get_mut(&key).unwrap().frames += added;
        } else {
            state.pending_frames += added;
        }
        state.previous_frames = frames;
        state.salt = salt;
        if state.threshold > 0 && frames >= state.threshold as u64 {
            let before = Io::read().writes;
            // Installing a WAL hook replaces SQLite's auto-checkpoint hook.
            // Reproduce its PASSIVE threshold policy rather than silently
            // measuring a database with auto-checkpointing disabled.
            let rc = unsafe {
                ffi::sqlite3_wal_checkpoint_v2(
                    db,
                    name,
                    ffi::SQLITE_CHECKPOINT_PASSIVE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != ffi::SQLITE_OK {
                state.errors.push(format!("auto-checkpoint returned {rc}"));
            }
            let bytes = Io::read().writes - before;
            let phase = state.phase.clone();
            state
                .totals
                .entry((phase, vec!["automatic checkpoint".into()]))
                .or_default()
                .bytes += bytes;
            state.checkpoint_bytes += bytes;
            if state.last_key.is_some() {
                state.last_io = Io::read().writes;
            } else {
                state.pending_checkpoint_bytes += bytes;
            }
            state.checkpoints += 1;
        }
    });
    if result.is_err()
        && let Ok(mut state) = unsafe { &*context.cast::<Mutex<State>>() }.lock()
    {
        state.errors.push("WAL callback failed".into());
    }
    ffi::SQLITE_OK
}
