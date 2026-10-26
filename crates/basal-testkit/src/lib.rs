//! Test support for basal: a test parent that spawns the real
//! `ck-basal-worker` binary and serves its host calls from a deterministic
//! mock with an in-memory journal; worker channels over real processes for
//! basal-core's driver; the journal cut harness; the test parent binary the
//! kill harness `kill -9`s (`src/bin/basal-test-parent.rs`); worker and journal
//! benchmarks; and a local HTTPS server for the `net.fetch` tests (`https`).

#![cfg(any(test, feature = "test-support"))]

mod backoff;
pub mod binaries;
pub mod channel;
pub mod command;
pub mod fuzz;
pub mod git;
pub mod harness;
pub mod https;
pub mod mock;
pub mod parent;
pub mod process;
pub mod run_scopes;

pub use binaries::dev_binary;
pub use channel::{FrameCounts, ProcessChannel, ProcessSource, worker_binary};
pub use mock::{Answer, ScriptHost};
pub use parent::{Ending, Journal, JournalEntry, Report, TestParent};
pub use process::{ParentError, WorkerProcess};
