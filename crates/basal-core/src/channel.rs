//! The parent's side of a worker channel.
//!
//! The core never spawns processes. Whoever owns the worker pool hands the
//! driver a [`WorkerChannel`] already greeted; the driver sends frames,
//! receives frames, and kills the worker when an activation must not
//! continue.

use std::fmt;
use std::time::Duration;

use basal_proto::{ParentMessage, Welcome, WorkerMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    /// Nothing arrived in time.
    Timeout,
    /// The worker is gone.
    Closed,
    /// The channel carried something that is not a valid frame, or failed.
    Broken(String),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("timed out waiting for the worker"),
            Self::Closed => f.write_str("the worker closed its channel"),
            Self::Broken(e) => write!(f, "broken channel: {e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// One greeted worker, bound to one activation at a time.
pub trait WorkerChannel: Send {
    /// The worker's handshake reply: its engine and prelude hash feed the
    /// runtime fingerprint.
    fn welcome(&self) -> &Welcome;
    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError>;
    fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError>;
    /// Ends the worker at once. Any frame it had in flight is lost.
    fn kill(&mut self);
}

/// Hands out greeted workers. The module's pool implements it; tests spawn
/// a process per call.
pub trait WorkerSource: Send + Sync {
    fn worker(&self) -> Result<Box<dyn WorkerChannel>, ChannelError>;
}
