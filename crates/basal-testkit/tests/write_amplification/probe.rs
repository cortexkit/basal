//! SQLite commit accounting for the ignored Linux write benchmark.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_int, c_void};
use std::io::{Read, Seek, SeekFrom};
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
    pub fn thread_writes() -> u64 {
        std::fs::read_to_string("/proc/thread-self/io")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("write_bytes:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
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
    sqlite_bytes: u64,
}

pub struct State {
    phase: String,
    statements: Vec<String>,
    last_key: Option<(String, Vec<String>)>,
    totals: BTreeMap<(String, Vec<String>), Total>,
    wal: PathBuf,
    page_size: u64,
    salt: [u8; 8],
    previous_frames: u64,
    pending_frames: u64,
    last_io: u64,
    checkpoint_bytes: u64,
    pending_checkpoint_bytes: u64,
    checkpoints: u64,
    threshold: c_int,
    errors: Vec<String>,
    initial_io: Io,
    event_gaps: u64,
    transaction_start: u64,
    transaction_checkpoint_bytes: u64,
    commit_thread_io: u64,
    page_owners: BTreeMap<u32, String>,
    transactions: Vec<Value>,
}

/// Unregisters callbacks under the connection lock before freeing their context.
pub struct Probe<'a> {
    state: Box<Mutex<State>>,
    store: &'a basal_core::Store,
}

impl<'a> Probe<'a> {
    pub fn start(store: &'a basal_core::Store, wal: PathBuf, threshold: c_int) -> Self {
        let initial_io = Io::read();
        let probe = Self {
            store,
            state: Box::new(Mutex::new(State {
                phase: "runs".into(),
                statements: Vec::new(),
                last_key: None,
                totals: BTreeMap::new(),
                wal,
                page_size: 0,
                salt: [0; 8],
                previous_frames: 0,
                pending_frames: 0,
                last_io: initial_io.writes,
                checkpoint_bytes: 0,
                pending_checkpoint_bytes: 0,
                checkpoints: 0,
                threshold,
                errors: Vec::new(),
                initial_io,
                event_gaps: 0,
                transaction_start: Io::thread_writes(),
                transaction_checkpoint_bytes: 0,
                commit_thread_io: 0,
                page_owners: BTreeMap::new(),
                transactions: Vec::new(),
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

    pub fn initial_io(&self) -> Io {
        self.state.lock().unwrap().initial_io
    }

    pub fn checkpoint(&self, conn: &Connection) {
        let before = Io::read().writes;
        let thread_before = Io::thread_writes();
        let (log, done): (i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(log, done, "no outstanding reader may hide checkpoint work");
        let after = Io::read().writes;
        let mut state = self.state.lock().unwrap();
        state.event_gaps += before - state.last_io;
        state.pending_checkpoint_bytes = 0;
        let phase = state.phase.clone();
        let total = state
            .totals
            .entry((phase, vec!["checkpoint tail".into()]))
            .or_default();
        total.bytes += after - before;
        total.sqlite_bytes += Io::thread_writes() - thread_before;
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
                    "kind": transaction_kind(phase, statements),
                    "transactions": total.transactions, "frames": total.frames,
                    "wal_frame_bytes": total.frames * (state.page_size + 24),
                    "sqlite_write_bytes": total.sqlite_bytes,
                    "write_bytes": total.bytes})
            })
            .collect();
        let mut steps: BTreeMap<&str, Value> = ["admission", "claim", "terminal_summary", "registry_segment", "retention", "checkpoint", "schedule_planning", "read_only_fence", "other"]
            .into_iter().map(|name| (name, json!({"transactions": 0, "wal_frames": 0, "wal_frame_bytes": 0, "sqlite_write_bytes": 0}))).collect();
        for kind in &kinds {
            let step = steps.get_mut(kind["kind"].as_str().unwrap()).unwrap();
            for (dest, source) in [
                ("transactions", "transactions"),
                ("wal_frames", "frames"),
                ("wal_frame_bytes", "wal_frame_bytes"),
                ("sqlite_write_bytes", "sqlite_write_bytes"),
            ] {
                step[dest] = json!(step[dest].as_u64().unwrap() + kind[source].as_u64().unwrap());
            }
        }
        json!({"kinds": kinds, "steps": steps, "transactions": state.transactions,
            "sql_event_gap_write_bytes": state.event_gaps,
            "autocheckpoints": state.checkpoints,
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
            state.transaction_start = Io::thread_writes();
            state.transaction_checkpoint_bytes = 0;
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
            let sqlite_bytes =
                Io::thread_writes() - state.transaction_start - state.transaction_checkpoint_bytes;
            let total = state.totals.entry(key).or_default();
            total.transactions += 1;
            total.frames += frames;
            total.bytes += bytes;
            total.sqlite_bytes += sqlite_bytes;
            state.commit_thread_io = Io::thread_writes();
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
        if let Some(key) = state.last_key.clone() {
            // COMMIT's PROFILE can precede its WAL callback. Attribute the
            // SQLite tail on this same connection thread to that commit too.
            let now = Io::thread_writes();
            let tail = now - state.commit_thread_io;
            state.totals.get_mut(&key).unwrap().sqlite_bytes += tail;
            state.commit_thread_io = now;
        }
        let mut header = [0; 32];
        let mut file = std::fs::File::open(&state.wal).unwrap();
        file.read_exact(&mut header).expect("WAL header");
        // Frame spacing is defined by the WAL header, not by SQLite's default
        // page size. One probe observes a fixed page size across WAL resets.
        let page_size = u64::from(u32::from_be_bytes(header[8..12].try_into().unwrap()));
        assert!((512..=65_536).contains(&page_size) && page_size.is_power_of_two());
        if state.page_size != 0 {
            assert_eq!(state.page_size, page_size);
        }
        state.page_size = page_size;
        let salt: [u8; 8] = header[16..24].try_into().unwrap();
        let frames = frames as u64;
        let added = if salt != state.salt {
            frames
        } else {
            frames - state.previous_frames
        };
        // Reading dbstat here observes the committed btrees, including indexes
        // and overflow pages. Disable trace only for this observer query so it
        // cannot recursively acquire State's mutex or pollute the workload SQL.
        unsafe {
            ffi::sqlite3_trace_v2(db, 0, None, std::ptr::null_mut());
        }
        let owners = (|| -> rusqlite::Result<BTreeMap<u32, String>> {
            let conn = unsafe { Connection::from_handle(db)? };
            conn.prepare("SELECT pageno,name FROM dbstat")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })();
        unsafe {
            ffi::sqlite3_trace_v2(
                db,
                (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE) as u32,
                Some(trace),
                context,
            );
        }
        let owners = owners.expect("dbstat page ownership");
        let mut pages = Vec::new();
        for frame in frames - added..frames {
            file.seek(SeekFrom::Start(32 + frame * (page_size + 24)))
                .unwrap();
            let mut number = [0; 4];
            file.read_exact(&mut number).unwrap();
            let page = u32::from_be_bytes(number);
            let owner = owners
                .get(&page)
                .or_else(|| state.page_owners.get(&page))
                .map(String::as_str)
                .unwrap_or("freelist_or_unmapped");
            pages.push(json!({"page": page, "object": owner}));
        }
        state.page_owners = owners;
        let (phase, statements) = state
            .last_key
            .clone()
            .unwrap_or_else(|| (state.phase.clone(), state.statements.clone()));
        state.transactions.push(json!({"phase": phase, "kind": transaction_kind(&phase, &statements),
            "sql": statements, "wal_frames": added, "wal_frame_bytes": added * (page_size + 24), "pages": pages}));
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
            let thread_before = Io::thread_writes();
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
            let after = Io::read().writes;
            let bytes = after - before;
            let sqlite_bytes = Io::thread_writes() - thread_before;
            state.transaction_checkpoint_bytes += sqlite_bytes;
            let phase = state.phase.clone();
            let total = state
                .totals
                .entry((phase, vec!["automatic checkpoint".into()]))
                .or_default();
            total.bytes += bytes;
            total.sqlite_bytes += sqlite_bytes;
            state.checkpoint_bytes += bytes;
            if state.last_key.is_some() {
                state.event_gaps += before - state.last_io;
                state.last_io = after;
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

fn transaction_kind(phase: &str, statements: &[String]) -> &'static str {
    let has = |text: &str| statements.iter().any(|s| s.contains(text));
    if has("checkpoint") {
        "checkpoint"
    } else if phase.contains("retention") {
        "retention"
    } else if has("INSERT INTO run_history") {
        "terminal_summary"
    } else if has("INSERT INTO live_runs") {
        "admission"
    } else if has("run_log_segments") {
        "registry_segment"
    } else if has("schedule_fires") || has("schedules") {
        "schedule_planning"
    } else if has("UPDATE runs SET state='running'") {
        "claim"
    } else if statements.len() == 1 && has("CREATE TABLE IF NOT EXISTS cortexkit_fence") {
        "read_only_fence"
    } else {
        "other"
    }
}
