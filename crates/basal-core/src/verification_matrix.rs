use crate::{Boundary, run_store::lock_order};

#[test]
fn every_post_port_boundary_has_a_crash_case() {
    const {
        assert!(
            cfg!(debug_assertions),
            "run the matrix with lock checks enabled"
        )
    };
    let mut covered = std::collections::BTreeSet::new();
    for boundary in Boundary::ALL {
        // No wildcard: every declared production boundary requires a crash scenario.
        match boundary {
            Boundary::AdmissionWritten | Boundary::AdmissionCommitted => {
                crate::run_admission::tests::crash_case(boundary.clone());
            }
            Boundary::RunStoreBuilt | Boundary::RunStoreCommitted => {
                crate::run_store::tests::crash_case(boundary.clone());
            }
            Boundary::RunLogAppended
            | Boundary::RunLogReserved
            | Boundary::RunLogCreated
            | Boundary::RunLogLinked
            | Boundary::RunLogPromoted => {
                crate::run_log::tests::crash_case(boundary.clone());
            }
            Boundary::RunLogGcCommitted | Boundary::RunLogGcUnlinked | Boundary::RunLogGcSynced => {
                crate::run_log::tests::sqlite::gc_crash_case(boundary.clone());
            }
            Boundary::MidSnapshot => {
                crate::run_records::tests::mid_snapshot_crash_keeps_durable_invariants_with_or_without_unsynced_tail();
            }
            Boundary::DriverActivated { .. }
            | Boundary::DriverAppended { .. }
            | Boundary::DriverSynced { .. }
            | Boundary::DriverDispatched { .. }
            | Boundary::DriverReleased { .. }
            | Boundary::DriverBeforeClose => {
                crate::run_driver::tests::crash_case(boundary.clone());
            }
            Boundary::HostArrived { .. } => {
                crate::run_host::tests::arrival_before_acknowledgement_crash_recovers_the_same_outcome();
            }
            Boundary::RestoreMarked
            | Boundary::RestoreQuarantined
            | Boundary::RestoreStarted
            | Boundary::RestoreFinished => {
                crate::run_log::tests::sqlite::restore::crash_case(boundary.clone());
                crate::staged_runtime::tests::verification_matrix::crash_case(boundary.clone());
            }
            Boundary::RemovalCaptured => {
                crate::run_removal::tests::crash_case(boundary.clone());
            }
            Boundary::MigrationSweepReady
            | Boundary::MigrationSweepCommitted
            | Boundary::MigrationCreateReady
            | Boundary::MigrationCreateCommitted
            | Boundary::MigrationFinishedReady
            | Boundary::MigrationFinishedCommitted
            | Boundary::MigrationLiveReady
            | Boundary::MigrationLiveCommitted
            | Boundary::MigrationBrocaReady
            | Boundary::MigrationBrocaCommitted
            | Boundary::MigrationDropReady
            | Boundary::MigrationDropCommitted
            | Boundary::MigrationPagesReady
            | Boundary::MigrationPagesCommitted
            | Boundary::MigrationBeforeVacuum
            | Boundary::MigrationAfterVacuum
            | Boundary::MigrationFinishReady
            | Boundary::MigrationFinishCommitted => {
                crate::run_cutover::tests::migration_case(boundary.clone());
            }
        }
        assert!(covered.insert(boundary.clone()), "duplicate {boundary:?}");
    }
    assert_eq!(covered.len(), 43);
}

#[test]
fn debug_lock_order_assertions_are_active() {
    use lock_order::{Lock, enter};
    const { assert!(cfg!(debug_assertions)) };
    let sqlite = enter(Lock::Sqlite);
    assert!(std::panic::catch_unwind(|| enter(Lock::Actor)).is_err());
    assert!(std::panic::catch_unwind(lock_order::assert_sync_allowed).is_err());
    assert!(std::panic::catch_unwind(lock_order::assert_lock_free).is_err());
    drop(sqlite);
    lock_order::assert_lock_free();
}

#[test]
fn callbacks_during_closing_retry_without_losing_the_outcome() {
    crate::run_host::tests::closing_and_missing_obligations_are_retryable_q5_e();
}
