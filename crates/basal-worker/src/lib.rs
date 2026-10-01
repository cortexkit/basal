//! `ck-basal-worker`: the confined engine that runs basal flows.
//!
//! The worker is stateless compute. basal's parent process spawns it, talks
//! to it over stdin and stdout in `basal-proto` frames, and owns everything
//! durable. The worker confines itself before reading anything (see
//! [`confinement`]), then runs one activation at a time: a fresh QuickJS
//! runtime per activation, locked down by the prelude, replaying the run's
//! recorded journal prefix locally and crossing to the parent only for calls
//! the journal has not seen.

pub mod clock;
pub mod confinement;
pub mod engine;
pub mod link;
pub mod probe;
pub mod serve;
