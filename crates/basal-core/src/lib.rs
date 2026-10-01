//! basal's runtime core: everything with durable state.
//!
//! - [`store`]: the SQLite store, its durability settings and schema.
//! - [`admission`]: one run per trigger, deduplicated, tombstoned after
//!   pruning.
//! - [`runs`]: the run state machine and activation ownership.
//! - [`journal`]: the issue log, the mailbox of outcomes not yet released,
//!   and the quarantine.
//! - [`driver`] and [`runtime`]: an activation driven on a worker channel,
//!   calls dispatched concurrently to a [`basal_host::Host`], completions
//!   accepted from anywhere.
//! - [`reconcile`], [`retention`] and [`ops`]: the operator's side.
//!
//! The core never spawns a process and never links the worker. It drives a
//! worker through [`channel::WorkerChannel`], so whoever owns the pool
//! supplies real processes.

pub mod admission;
pub mod channel;
pub mod driver;
pub mod error;
pub mod hooks;
pub mod ids;
pub mod journal;
pub mod local;
pub mod model;
pub mod ops;
pub mod reconcile;
pub mod retention;
pub mod runs;
pub mod runtime;
pub mod schema;
pub mod store;

pub use admission::{Admission, TriggerSpec};
pub use channel::{ChannelError, WorkerChannel, WorkerSource};
pub use error::{CoreError, Result};
pub use hooks::{Boundary, Hooks, NoHooks, Step};
pub use model::{CallRow, DispatchState, Run, RunState, StoredClass};
pub use reconcile::Resolution;
pub use retention::PruneReport;
pub use runtime::{ActivationEnd, Config, Runtime};
pub use store::{Durability, Pragmas, Store};
