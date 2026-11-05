//! Opt-in Linux benchmark counters. Not compiled into production binaries.
//!
//! Thread-local scopes follow the log onto its writer thread. Only actual log
//! file operations are observed: registry SQLite work is deliberately outside
//! the scope of each I/O sample. Logical appended bytes and OS allocation bytes
//! are different measures and must not be added together.

use std::{
    cell::RefCell,
    io,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub append_bytes: u64,
    pub write_bytes: u64,
    pub file_syncs: u64,
    pub directory_syncs: u64,
    pub segment_creates: u64,
    pub rotations: u64,
}

#[derive(Clone, Default)]
pub struct Measurement(Arc<Mutex<Stats>>);

thread_local! {
    static ACTIVE: RefCell<Option<Measurement>> = const { RefCell::new(None) };
}

pub(super) enum Operation {
    Append(u64),
    FileSync,
    DirectorySync,
    Create,
}

pub(super) struct Scope(Option<Measurement>);
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.replace(self.0.take()));
    }
}

impl Measurement {
    /// Sample after draining the writer, and again after close for the final tail.
    pub fn stats(&self) -> Stats {
        *self.0.lock().unwrap()
    }

    pub(super) fn enter(&self) -> Scope {
        Scope(ACTIVE.with(|active| active.replace(Some(self.clone()))))
    }
}

pub(super) fn current() -> Option<Measurement> {
    ACTIVE.with(|active| active.borrow().clone())
}

pub(super) fn rotated() {
    if let Some(m) = current() {
        m.0.lock().unwrap().rotations += 1;
    }
}

fn written() -> u64 {
    std::fs::read_to_string("/proc/thread-self/io")
        .expect("Linux thread I/O accounting")
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes:"))
        .expect("write_bytes counter")
        .trim()
        .parse()
        .expect("numeric I/O counter")
}

pub(super) fn observe<T>(
    operation: Operation,
    body: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let Some(measurement) = current() else {
        return body();
    };
    let before = written();
    let result = body();
    let bytes = written() - before;
    let mut stats = measurement.0.lock().unwrap();
    stats.write_bytes += bytes;
    if result.is_ok() {
        match operation {
            Operation::Append(bytes) => stats.append_bytes += bytes,
            Operation::FileSync => stats.file_syncs += 1,
            Operation::DirectorySync => stats.directory_syncs += 1,
            Operation::Create => stats.segment_creates += 1,
        }
    }
    result
}
