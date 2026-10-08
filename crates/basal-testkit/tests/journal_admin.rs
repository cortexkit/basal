//! Admission, retention, durability settings and obligations.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::{Admission, Durability, RunState, Store};
use basal_testkit::harness::{World, scratch};
use common::{admit, admit_as, finish, runtime};

fn far_future() -> i64 {
    i64::MAX / 2
}

#[test]
fn admission_deduplicates_and_tombstones_refuse_after_pruning() {
    let world = World::new("admission");
    let rt = runtime(&world);
    let spec = world.spec(&rt, "return 1;").expect("approve");
    let first = rt.admit(&spec).expect("admit");
    let Admission::Admitted { run_id } = first.clone() else {
        panic!("{first:?}");
    };
    assert_eq!(
        rt.admit(&spec).expect("again"),
        Admission::Duplicate {
            run_id: run_id.clone()
        }
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded);
    let report = rt.prune(far_future(), 0).expect("prune");
    assert_eq!(report.pruned, vec![run_id.clone()]);
    assert!(rt.run(&run_id).is_err());
    assert_eq!(
        rt.admit(&spec).expect("after pruning"),
        Admission::Tombstoned { run_id }
    );
}

#[test]
fn retrigger_admits_a_new_run_with_new_keys() {
    let world = World::new("retrigger");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "return (await ops.call('mock', 'send', {})).op;",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded);
    let Admission::Admitted { run_id: again } = rt.retrigger(&run_id).expect("retrigger") else {
        panic!("not admitted");
    };
    assert_ne!(again, run_id);
    finish(&rt, &world, &again);
    let k1 = rt.calls(&run_id).expect("calls")[0].idempotency_key.clone();
    let k2 = rt.calls(&again).expect("calls")[0].idempotency_key.clone();
    assert_ne!(k1, k2);
    assert_eq!(world.mock.effects().len(), 2);
}

/// Pruning never touches a run that is not terminal, nor a terminal run
/// with a call still lacking an outcome.
#[test]
fn pruning_spares_unfinished_runs_and_open_obligations() {
    let world = World::new("prune");
    let rt = runtime(&world);
    let pending = admit_as(&rt, &world, "return 1;", "pending");
    let suspended = admit_as(&rt, &world, "return (await llm({})).done;", "suspended");
    let ended = rt.resume(&suspended).expect("activation");
    assert!(
        matches!(ended, basal_core::ActivationEnd::Suspended { .. }),
        "{ended:?}"
    );
    // A rejection still waits for every dispatched outcome. The run stays
    // suspended and unprunable while its long call remains an obligation.
    let failed = admit_as(
        &rt,
        &world,
        r#"
        ops.call('mock', 'long', {});
        ops.call('mock', 'send', { delay_ms: 300 });
        throw new Error('boom');
        "#,
        "failed",
    );
    let run = rt
        .run_to_rest(&failed, Duration::from_secs(30))
        .expect("run");
    assert_eq!(run.state, RunState::Suspended, "{run:#?}");
    assert_eq!(run.error_kind, None);

    let report = rt.prune(far_future(), 0).expect("prune");
    assert!(report.pruned.is_empty(), "{report:?}");
    assert!(report.kept_unsettled.is_empty());
    for id in [&pending, &suspended, &failed] {
        assert!(rt.run(id).is_ok(), "{id} was pruned");
    }

    // The send completes by itself, then the host completes the long call.
    // Only after both outcomes are consumed may the rejection end the run.
    rt.wait_for(&failed, Duration::from_secs(10), |_| {
        rt.health()
            .map(|h| {
                h.open_obligations
                    .iter()
                    .filter(|(r, _)| *r == failed)
                    .count()
                    <= 1
            })
            .unwrap_or(false)
    })
    .expect("wait");
    let handle = world.mock.handle_for(&failed, 0).expect("accepted");
    world.mock.complete(
        &handle,
        basal_host::HostOutcome::fulfilled(basal_proto::JsonText::null()),
    );
    rt.quiesce();
    let ended = rt.resume(&failed).expect("resume after long call settles");
    assert!(
        matches!(ended, basal_core::ActivationEnd::Failed { ref kind, .. } if kind == "script"),
        "{ended:?}"
    );
    assert!(
        !rt.health()
            .expect("health")
            .open_obligations
            .iter()
            .any(|(r, _)| *r == failed)
    );
    assert_eq!(rt.run(&failed).expect("run").state, RunState::Failed);
    let report = rt.prune(far_future(), 0).expect("prune");
    assert_eq!(report.pruned, vec![failed]);
    assert!(rt.run(&pending).is_ok());
    assert!(rt.run(&suspended).is_ok());
}

/// The store runs `synchronous = FULL`, and `fullfsync` (with
/// `checkpoint_fullfsync`) matches the durability it was opened with, for
/// both choices.
#[test]
fn store_runs_synchronous_full_and_the_chosen_fullfsync() {
    for fullfsync in [true, false] {
        let dir = scratch("pragmas");
        let store =
            Arc::new(Store::open(dir.join("basal.db"), Durability { fullfsync }).expect("open"));
        let p = store.pragmas().expect("pragmas");
        assert_eq!(p.synchronous, 2, "synchronous is not FULL");
        assert_eq!(p.fullfsync, fullfsync);
        assert_eq!(p.checkpoint_fullfsync, fullfsync);
        assert!(p.journal_mode_wal);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
    assert!(Durability::default().fullfsync);
}

#[test]
fn drain_stops_admission_and_lists_unfinished_runs() {
    let world = World::new("drain");
    let rt = runtime(&world);
    let suspended = admit_as(&rt, &world, "return (await llm({})).done;", "a");
    rt.resume(&suspended).expect("activation");
    let unfinished = rt.drain().expect("drain");
    assert_eq!(unfinished.get("suspended"), Some(&vec![suspended.clone()]));
    let mut spec = world.spec(&rt, "return 1;").expect("approve");
    spec.trigger_id = "b".into();
    assert_eq!(rt.admit(&spec).expect("admit"), Admission::Draining);
    rt.undrain().expect("undrain");
    assert!(matches!(
        rt.admit(&spec).expect("admit"),
        Admission::Admitted { .. }
    ));
}
