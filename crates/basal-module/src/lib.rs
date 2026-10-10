//! `ck-basal`, the supervised module: basal's core turned into a running
//! service.
//!
//! - [`manifest`]: the subc manifest, built in Rust.
//! - [`serve`]: the subc handler; [`caller`]: who is calling, from the
//!   daemon's stamp on the route.
//! - [`module`]: the module assembled, recovery first.
//! - [`pool`] and [`process`]: confined worker processes, bound to one flow
//!   each, kept warm.
//! - [`engine`]: the loops that tick schedules, enforce deadlines and drive
//!   runs.
//! - [`ops`]: the `flow.*` ops and their authorization; [`card`]: the
//!   install card; [`dryrun`]: the capture-only (or operator-live) dry run.
//! - [`fatal`]: a store failure ends the process so recovery can run.
//! - [`metrics`]: the module's own counters.
//! - [`unconfigured`]: the hosts the production binary runs with until real
//!   adapters exist; [`harness`]: the same module against the mocks, driven
//!   over stdio for tests.
//! - `windows` (Windows only): the worker started through `basal-launch`,
//!   its kill, and the exit codes that are confinement faults.
//! - `rig_kill` (unit tests or the `rig-kill-hook` feature): the one-shot kill
//!   switch the ckdev-flows rig's contract suite uses for its crash case.

pub mod caller;
pub mod card;
pub mod codemode;
pub mod dryrun;
pub mod engine;
pub mod fatal;
pub mod harness;
pub mod manifest;
pub mod metrics;
pub mod module;
pub mod ops;
pub mod pool;
pub mod process;
// Exercise evidence capture in ordinary unit tests without enabling a kill
// switch in deployed binaries, which still require the explicit feature.
#[cfg(any(test, feature = "rig-kill-hook"))]
pub mod rig_kill;
pub mod scope_connection;
pub mod serve;
pub mod unconfigured;
#[cfg(windows)]
mod windows;
