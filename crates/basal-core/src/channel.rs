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
    /// A second handle on this worker's incoming frames, which another thread
    /// can block on while this side keeps sending and can still kill the
    /// worker. Each frame goes to whichever reader takes it first, so a caller
    /// that splits off a receiver reads only through that receiver.
    fn receiver(&mut self) -> Box<dyn WorkerReceiver>;
    /// A send side another thread can block on while this side can still
    /// kill the worker. A worker that is not reading its input makes a send
    /// wait until the pipe has room, so a caller that must keep draining the
    /// worker or enforcing deadlines writes from another thread. Once split,
    /// every frame goes through the writer, never through [`Self::send`].
    ///
    /// `None` means this channel cannot split its send side. The codemode
    /// supervisor refuses such a channel rather than write from its event
    /// loop; flow drivers never split.
    fn writer(&mut self) -> Option<Box<dyn WorkerWriter>> {
        None
    }
    /// Scripted in-process peers may couple recv to send without a split pipe.
    /// Real workers always use the split receiver so the parent owns deadlines.
    #[cfg(any(test, feature = "test-support"))]
    fn fixture_inline_recv(&self) -> bool {
        false
    }
    /// Ends the worker at once. Any frame it had in flight is lost, and a
    /// receive blocked on a split-off [`WorkerReceiver`] returns an error.
    fn kill(&mut self);
}

/// The receive side of a [`WorkerChannel`], readable from its own thread.
pub trait WorkerReceiver: Send {
    /// Blocks until the worker sends a frame or its channel ends. There is
    /// no timeout: the thread that owns the worker's deadline kills the
    /// worker, and the kill ends this wait.
    fn recv(&mut self) -> Result<WorkerMessage, ChannelError>;
}

/// The send side of a [`WorkerChannel`], writable from its own thread.
pub trait WorkerWriter: Send {
    /// Blocks until the frame is written or the channel fails. There is no
    /// timeout: [`WorkerChannel::kill`] ends the worker, and a write blocked
    /// on it then fails instead of waiting for room that never comes.
    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError>;
}

/// Hands out greeted workers. The module's pool implements it; tests spawn
/// a process per call.
pub trait WorkerSource: Send + Sync {
    fn worker(&self) -> Result<Box<dyn WorkerChannel>, ChannelError>;
}
