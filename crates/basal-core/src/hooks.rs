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
    /// One admission's rows are written inside the batch, before commit.
    AdmissionWritten,
    /// The admission batch committed, before receipts may be acknowledged.
    AdmissionCommitted,
    /// A terminal projection is built, before the group transaction begins.
    RunStoreBuilt,
    /// The terminal transaction committed, before routing becomes Closed.
    RunStoreCommitted,
    /// A run-log batch is appended but no sync watermark has been released.
    RunLogAppended,
    /// The next segment's reserved row is committed, before file creation.
    RunLogReserved,
    /// The spare's header and directory entry are durable.
    RunLogCreated,
    /// The new segment's Link is durable, before registry promotion.
    RunLogLinked,
    /// The new segment is registered active, before publishing its watermark.
    RunLogPromoted,
    /// The GC floor and registry deletions committed, before any unlink.
    RunLogGcCommitted,
    /// One reclaimed file was unlinked; other files may still be present.
    RunLogGcUnlinked,
    /// Reclaimed directory entries are durable where directory sync is supported.
    RunLogGcSynced,
    /// A snapshot is copied and appended atomically, before its sync wait.
    MidSnapshot,
    /// A driver's identity record is appended, before its required sync.
    DriverAppended,
    /// A remote Intent is durable, before the host is called.
    DriverSynced,
    /// The host returned, before Accepted or Arrived is appended.
    DriverDispatched,
    /// Released is appended, before Deliver or terminal projection.
    DriverReleased,
    /// A host outcome is durable, before its acknowledgement leaves the runtime.
    HostArrived,
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
    /// The router selected a model, but the call is not journaled or sent yet.
    ModelSelected { position: u64 },
    /// A remote call's row is committed; it has not been dispatched.
    CallCommitted { position: u64 },
    /// A synchronous call's row, value and order are committed; the reply
    /// has not been sent.
    SyncCommitted { position: u64 },
    /// The reply to a synchronous call was sent.
    SyncReplied { position: u64 },
    /// A local call's row, effect and outcome are committed.
    LocalCommitted { position: u64 },
    /// A refused call's row, audit row and rejection are committed.
    RefusalCommitted { position: u64 },
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

impl Boundary {
    pub const HOST: [Self; 1] = [Self::HostArrived];
    pub const RUN_DRIVER: [Self; 4] = [
        Self::DriverAppended,
        Self::DriverSynced,
        Self::DriverDispatched,
        Self::DriverReleased,
    ];
    pub const RUN_LOG_GC: [Self; 3] = [
        Self::RunLogGcCommitted,
        Self::RunLogGcUnlinked,
        Self::RunLogGcSynced,
    ];

    /// File-writer crash points, exercised before the runtime switches paths.
    pub const RUN_LOG: [Self; 5] = [
        Self::RunLogAppended,
        Self::RunLogReserved,
        Self::RunLogCreated,
        Self::RunLogLinked,
        Self::RunLogPromoted,
    ];
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
