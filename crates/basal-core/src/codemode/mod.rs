//! Codemode: one short JavaScript program, run once in a fresh confined
//! worker, that calls tools from a catalog and returns one result.
//!
//! This module holds what codemode keeps durable and the pure checks
//! admission needs:
//!
//! - [`store`]: the run and call tables, kept apart from the flow journal,
//!   with conditional outcome and terminal writes;
//! - [`retention`]: the sweep that prunes a run 24 hours after it ends and
//!   leaves a permanent tombstone;
//! - [`canonical`]: RFC 8785 canonical JSON;
//! - [`catalog`]: the catalog digest and tool input schema compilation.
//! - [`admission`]: ordered checks and a transaction that grants at most one
//!   caller permission to start a run.
//! - [`supervisor`]: blocking worker supervision, one-attempt tool dispatch and
//!   ordered termination, independent of the flow driver.
//!
//! Codemode rows never enter the flow machinery: no journal row, lease,
//! retry, suspension, replay, reconcile, flow retention or flow op reads or
//! writes these tables.

pub mod admission;
pub mod canonical;
pub mod catalog;
pub mod retention;
pub mod store;
pub mod supervisor;
