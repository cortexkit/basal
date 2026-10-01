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
//! - [`manifest`] and [`install`]: what a flow may do, validated against
//!   the catalog, approved by code hash, disabled at once.
//! - [`authorize`], [`audit`], [`tokens`], [`kv`] and [`rate`]: what
//!   decides, in the parent and before dispatch, whether a call may go out
//!   and how much a flow may spend.
//! - [`clock`]: the time those limits read, settable in tests.
//! - [`cards`]: the consent card between installing a version and
//!   approving it.
//! - [`schedule`]: schedule triggers, due times in a named zone, missed
//!   fires, and their admission.
//!
//! The core never spawns a process and never links the worker. It drives a
//! worker through [`channel::WorkerChannel`], so whoever owns the pool
//! supplies real processes.

pub mod admission;
pub mod audit;
pub mod authorize;
pub mod cards;
pub mod channel;
pub mod clock;
pub mod driver;
pub mod error;
pub mod hooks;
pub mod ids;
pub mod install;
pub mod journal;
pub mod kv;
pub mod manifest;
pub mod model;
pub mod ops;
pub mod rate;
pub mod reconcile;
pub mod retention;
pub mod runs;
pub mod runtime;
pub mod schedule;
pub mod schema;
pub mod store;
pub mod tokens;

pub use admission::{Admission, TriggerSpec};
pub use authorize::ShellDenylist;
pub use channel::{ChannelError, WorkerChannel, WorkerSource};
pub use clock::Clock;
pub use error::{CoreError, Result};
pub use hooks::{Boundary, Hooks, NoHooks, Step};
pub use install::{Actor, InstallError, InstallRequest, Installed, Warning};
pub use kv::KvLimits;
pub use manifest::{Manifest, ManifestError};
pub use model::{CallRow, DispatchState, Run, RunState, StoredClass};
pub use rate::RateLimits;
pub use reconcile::Resolution;
pub use retention::PruneReport;
pub use runtime::RunLimits;
pub use runtime::{ActivationEnd, Config, Runtime};
pub use schedule::{ScheduleSpec, Scheduler, SchedulerConfig};
pub use store::{Durability, Pragmas, Store};
