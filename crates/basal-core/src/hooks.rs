//! Named points in the runtime where a test can stop the world.
//!
//! Each [`Boundary`] sits right after a durable commit or right after a
//! frame crosses to the worker: the places a crash can land between two
//! records. The cut and kill harnesses enumerate the boundaries an uncut run
//! passes and then crash at each one. In production the hooks are
//! [`NoHooks`] and cost one virtual call.

// Generate the inventory from the declaration itself: adding a crash point
// also requires an explicit scenario in the exhaustive verification matrix.
macro_rules! boundaries {
    ($(#[$outer:meta])* $vis:vis enum $name:ident {
        $($(#[$doc:meta])* $variant:ident $({$($field:ident: $ty:ty),*})?),* $(,)?
    }) => {
        $(#[$outer])*
        $vis enum $name {
            $($(#[$doc])* $variant $({$($field: $ty),*})?),*
        }
        impl $name {
            /// One representative of every boundary, including data-bearing ones.
            /// Positions and generations are zero in this inventory.
            pub const ALL: &'static [Self] = &[
                $(Self::$variant $({$($field: 0),*})?),*
            ];
        }
    };
}

boundaries! {
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
    /// The activation has taken and registered its generation, before its
    /// first new or recovered dispatch can start.
    DriverActivated { generation: u64 },
    /// A driver's identity record is appended, before its required sync.
    DriverAppended { position: u64 },
    /// A remote Intent is durable, before the host is called.
    DriverSynced { position: u64 },
    /// The host returned, before Accepted or Arrived is appended.
    DriverDispatched { position: u64 },
    /// Delivery order is recorded, before Deliver or a synchronous reply.
    DriverReleased { position: u64 },
    /// The last point before the Open-to-Closing seal. RunStoreBuilt is too
    /// late to test cancellation winning this race: the run is already sealed.
    DriverBeforeClose,
    /// An outcome or accepted handle is durable, before its acknowledgement.
    HostArrived { position: u64 },
    /// The restore marker, lost runs and new epoch committed atomically.
    RestoreMarked,
    /// Old segment directories were quarantined and directory entries synced.
    RestoreQuarantined,
    /// The new epoch has a durable active segment and registered spare.
    RestoreStarted,
    /// Restore completion committed; the next boot must recover normally.
    RestoreFinished,
    /// Removal and its immutable live-run target IDs committed, before closes.
    RemovalCaptured,
    /// Legacy temporary-file sweep and audit capture, before the phase commit.
    MigrationSweepReady,
    /// Sweep and pre-migration evidence are durable.
    MigrationSweepCommitted,
    /// New control-plane schema is created, before the phase commit.
    MigrationCreateReady,
    /// Schema and run-log metadata are durable.
    MigrationCreateCommitted,
    /// Finished-run conversion is built, before the phase commit.
    MigrationFinishedReady,
    /// Finished histories, call detail and receipts are durable.
    MigrationFinishedCommitted,
    /// First-segment snapshots are synced, before live admissions commit.
    MigrationLiveReady,
    /// Converted live admissions and their snapshot watermark are durable.
    MigrationLiveCommitted,
    /// Broca table replacement is built with its forget trigger disabled.
    MigrationBrocaReady,
    /// Broca calls have their new ownership and waits/wakes are preserved.
    MigrationBrocaCommitted,
    /// Legacy schema removal is audited, before the drop transaction commits.
    MigrationDropReady,
    /// Legacy tables and dependent views are gone.
    MigrationDropCommitted,
    /// Both schema post-checks and the page-size rewrite passed, before commit.
    MigrationPagesReady,
    /// The page-size phase and its post-checks committed.
    MigrationPagesCommitted,
    /// DELETE mode and the target page size are selected, before VACUUM.
    MigrationBeforeVacuum,
    /// VACUUM completed, before WAL mode is restored.
    MigrationAfterVacuum,
    /// Persisted post-checks passed, before migration completion commits.
    MigrationFinishReady,
    /// Migration completion committed; normal startup may proceed.
    MigrationFinishCommitted,

}
}

impl Boundary {
    pub const MIGRATION: [Self; 18] = [
        Self::MigrationSweepReady,
        Self::MigrationSweepCommitted,
        Self::MigrationCreateReady,
        Self::MigrationCreateCommitted,
        Self::MigrationFinishedReady,
        Self::MigrationFinishedCommitted,
        Self::MigrationLiveReady,
        Self::MigrationLiveCommitted,
        Self::MigrationBrocaReady,
        Self::MigrationBrocaCommitted,
        Self::MigrationDropReady,
        Self::MigrationDropCommitted,
        Self::MigrationPagesReady,
        Self::MigrationPagesCommitted,
        Self::MigrationBeforeVacuum,
        Self::MigrationAfterVacuum,
        Self::MigrationFinishReady,
        Self::MigrationFinishCommitted,
    ];
    pub const RESTORE: [Self; 4] = [
        Self::RestoreMarked,
        Self::RestoreQuarantined,
        Self::RestoreStarted,
        Self::RestoreFinished,
    ];
    pub const HOST: [Self; 1] = [Self::HostArrived { position: 0 }];
    pub const RUN_DRIVER: [Self; 6] = [
        Self::DriverActivated { generation: 0 },
        Self::DriverAppended { position: 0 },
        Self::DriverSynced { position: 0 },
        Self::DriverDispatched { position: 0 },
        Self::DriverReleased { position: 0 },
        Self::DriverBeforeClose,
    ];
    pub const RUN_LOG_GC: [Self; 3] = [
        Self::RunLogGcCommitted,
        Self::RunLogGcUnlinked,
        Self::RunLogGcSynced,
    ];

    /// File-writer crash points, exercised independently on every platform.
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
    /// The reply is recorded, but the dispatch remains registered as in flight
    /// until `Dispatch::send` returns and drops its registration.
    #[cfg(test)]
    fn before_dispatch_drop(&self, _run_id: &str, _position: u64) {}
    /// Observe records while the run's actor mutex is held and its activation
    /// generation has been checked. Implementations must not call hosts or take
    /// another actor; production's NoHooks does nothing.
    fn at_run(
        &self,
        _: &str,
        _: &str,
        _: &Boundary,
        _: Option<u64>,
        _: &crate::run_records::State,
    ) -> Step {
        Step::Continue
    }
}

/// No interference.
pub struct NoHooks;

impl Hooks for NoHooks {
    fn at(&self, _: &str, _: &Boundary) -> Step {
        Step::Continue
    }
}

#[cfg(test)]
#[path = "verification_matrix.rs"]
mod verification_matrix;
