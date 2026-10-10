//! Starts basal's worker process under the Windows confinement.
//!
//! On Windows the worker cannot confine itself the way it does on macOS and
//! Linux: most of the confinement has to be fixed by the parent when the
//! process is created. This crate owns that part:
//!
//! - the one AppContainer profile all workers share;
//! - the restricted primary token and the matching start-up thread token;
//! - the job the worker is born in, and its limits;
//! - the creation attributes: Less Privileged AppContainer, mitigation
//!   policies, child-process ban and an explicit list of inherited handles;
//! - a minimal environment, working directory, window station and desktop;
//! - the checks the parent makes on the suspended process before it lets the
//!   first instruction run, and the owned process wrapper afterwards.
//!
//! The worker's own checks, made after it starts and before it reads any
//! input, live in the worker.
//!
//! Off Windows this crate contains no code.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::*;
