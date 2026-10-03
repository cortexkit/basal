//! Test support for basal: a test parent that spawns the real
//! `ck-basal-worker` binary and serves its host calls from a deterministic
//! mock with an in-memory journal; worker channels over real processes for
//! basal-core's driver; the journal cut harness; the test parent binary the
//! kill harness `kill -9`s (`src/bin/basal-test-parent.rs`); and the
//! mutation-control runner (`src/bin/mutation-controls.rs`) that proves each
//! safety test can fail; and a local HTTPS server for the `net.fetch`
//! tests (`https`).

pub mod channel;
pub mod fuzz;
pub mod harness;
pub mod https;
pub mod mock;
pub mod parent;
pub mod process;

pub use channel::{FrameCounts, ProcessChannel, ProcessSource, worker_binary};
pub use mock::{Answer, MockHost};
pub use parent::{Ending, Journal, JournalEntry, Report, TestParent};
pub use process::{ParentError, WorkerProcess};
