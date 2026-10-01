//! Named points in the runtime where a test can stop the world.
//!
//! Each [`Boundary`] sits right after a durable commit or right after a
//! frame crosses to the worker: the places a crash can land between two
//! records. The cut and kill harnesses enumerate the boundaries an uncut run
//! passes and then crash at each one. In production the hooks are
//! [`NoHooks`] and cost one virtual call.

/// A point between two records.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Boundary {
    /// The activation claimed the run (its generation is committed).
    Claimed { generation: u64 },
    /// The runtime fingerprint was recorded at the run's first activation.
    FingerprintRecorded,
    /// A call left unresolved by an earlier activation was authorized to be
    /// sent again.
    ResendAuthorized { position: u64 },
    /// The `Activate` frame was sent.
    ActivateSent { generation: u64 },
    /// A `HostCall` frame arrived; nothing about it is committed.
    HostCallReceived { position: u64 },
    /// A `Blocked` frame arrived; nothing has been released for it.
    BlockedReceived,
    /// A remote call's row is committed; it has not been dispatched.
    CallCommitted { position: u64 },
    /// A synchronous call's row, value and order are committed; the reply
    /// has not been sent.
    SyncCommitted { position: u64 },
    /// The reply to a synchronous call was sent.
    SyncReplied { position: u64 },
    /// A local call's row, effect and outcome are committed.
    LocalCommitted { position: u64 },
    /// The host answered a dispatch; nothing about the answer is committed.
    HostAnswered { position: u64 },
    /// An outcome is committed to the mailbox.
    OutcomeCommitted { position: u64 },
    /// The host's acceptance of a long-running call is committed.
    AcceptedCommitted { position: u64 },
    /// An outcome's delivery order is committed; the `Deliver` frame has not
    /// been sent.
    OrderCommitted { position: u64, order: u64 },
    /// The `Deliver` frame was sent.
    Delivered { position: u64 },
    /// A `LongRunning` frame was sent.
    LongRunningSent,
    /// The worker reported how the activation ended; the run is still
    /// `running`.
    EndingReceived,
    /// The run left `running`.
    Exited,
}

/// What the runtime does at a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// Behave as if the process died here: the store is cut, the worker is
    /// killed and nothing more is written.
    Crash,
}

pub trait Hooks: Send + Sync {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step;
}

/// No interference.
pub struct NoHooks;

impl Hooks for NoHooks {
    fn at(&self, _: &str, _: &Boundary) -> Step {
        Step::Continue
    }
}
