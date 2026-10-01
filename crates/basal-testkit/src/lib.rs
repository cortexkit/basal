//! Test support for basal's worker: a test parent that spawns the real
//! `ck-basal-worker` binary, serves its host calls from a deterministic mock,
//! keeps an in-memory journal and replays it; and the mutation-control
//! runner (`src/bin/mutation-controls.rs`) that proves each safety test can
//! fail.

pub mod mock;
pub mod parent;
pub mod process;

pub use mock::{Answer, MockHost};
pub use parent::{Ending, Journal, JournalEntry, Report, TestParent};
pub use process::{ParentError, WorkerProcess};
