//! The per-flow concurrency slot and the per-run wall-clock deadline: runs
//! of one flow run one at a time in trigger order, a run holds the slot
//! until it is terminal (suspended included), and the deadline fails a
//! stuck run, kills its worker and frees the slot. Time is a manual clock;
//! nothing here waits on wall time to pass.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::{ActivationEnd, Clock, Config, NoHooks, RunState, Runtime};
use basal_testkit::harness::World;
use common::{config, finish, result};
use serde_json::json;

fn admit_trigger(rt: &Runtime, world: &World, script: &str, trigger: &str) -> String {
    let mut spec = world.spec(rt, script).expect("approve");
    spec.trigger_id = trigger.to_owned();
    rt.admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

const T0: i64 = 1_798_761_600_000;

fn manual_runtime(world: &World) -> (Runtime, Clock) {
    let clock = Clock::manual(T0);
    let rt = world
        .runtime(
            Arc::new(NoHooks),
            Config {
                clock: clock.clone(),
                ..config()
            },
        )
        .expect("runtime");
    (rt, clock)
}

#[test]
fn second_run_waits_for_the_first_even_while_suspended() {
    let world = World::new("slot-suspended");
    let rt = common::runtime(&world);
    let script = "const x = await llm({ prompt: 'p' }); return trigger.kind;";
    let first = admit_trigger(&rt, &world, script, "t1");
    let second = admit_trigger(&rt, &world, script, "t2");
    assert!(matches!(
        rt.resume(&first).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    assert_eq!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Waiting {
            holder: first.clone()
        }
    );
    assert_eq!(rt.run(&second).expect("run").state, RunState::Pending);
    assert_eq!(rt.activation_count(&second).expect("count"), 0);
    // The first run finishes; only then does the second start.
    let run = finish(&rt, &world, &first);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert!(matches!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    let run = finish(&rt, &world, &second);
    assert_eq!(result(&run), json!("test"));
}

#[test]
fn deadline_fails_a_stuck_run_and_frees_the_slot() {
    let world = World::new("slot-deadline");
    let (rt, clock) = manual_runtime(&world);
    // The first run's call never answers until the test opens its gate.
    let first = admit_trigger(
        &rt,
        &world,
        "await ops.call('mock', 'echo', { gate: 'stuck' }); return 1;",
        "t1",
    );
    let second = admit_trigger(&rt, &world, "return 2;", "t2");
    let mut worker = world.source.spawn().expect("worker");
    let driver = {
        let rt = rt.clone();
        let first = first.clone();
        std::thread::spawn(move || {
            let end = rt.activate(&first, &mut worker);
            (end, worker)
        })
    };
    // Wait until the first run's call is journaled and in the host: the run
    // is running and stuck.
    rt.wait_for(&first, Duration::from_secs(30), |_| {
        rt.calls(&first).map(|c| c.len() == 1).unwrap_or(false)
    })
    .expect("wait");
    assert_eq!(rt.run(&first).expect("run").state, RunState::Running);
    assert!(matches!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Waiting { .. }
    ));

    // Past the default ten-minute deadline.
    clock.advance(601_000);
    rt.enforce_deadlines().expect("enforce");
    let (end, mut worker) = driver.join().expect("driver thread");
    let end = end.expect("activation");
    assert!(
        matches!(&end, ActivationEnd::OwnerLost)
            || matches!(&end, ActivationEnd::Failed { kind, .. } if kind == "deadline"),
        "{end:?}"
    );
    assert!(
        worker.exited(Duration::from_secs(10)),
        "the worker was not killed"
    );
    let run = rt.run(&first).expect("run");
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.error_kind.as_deref(), Some("deadline"));
    // The run's own deadline, not the activation's wall-time budget.
    assert_eq!(
        run.error_detail.as_deref(),
        Some("the run passed its wall-clock deadline")
    );

    // The slot is free.
    assert!(
        matches!(
            rt.resume(&second).expect("activation"),
            ActivationEnd::Succeeded { .. }
        ),
        "the second run did not start"
    );

    // The stuck call still settles: its obligation is kept. (Its dispatch
    // thread is still inside the host until the gate opens.)
    world.mock.open_gate("stuck");
    rt.quiesce();
    let open = rt.health().expect("health").open_obligations;
    assert!(!open.iter().any(|(r, _)| *r == first), "{open:?}");
}

#[test]
fn deadline_fails_a_suspended_run_and_frees_the_slot() {
    let world = World::new("slot-deadline-suspended");
    let (rt, clock) = manual_runtime(&world);
    let first = admit_trigger(
        &rt,
        &world,
        "return (await llm({ prompt: 'p' })).done;",
        "t1",
    );
    let second = admit_trigger(&rt, &world, "return 2;", "t2");
    assert!(matches!(
        rt.resume(&first).expect("activation"),
        ActivationEnd::Suspended { .. }
    ));
    assert!(matches!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Waiting { .. }
    ));
    clock.advance(601_000);
    assert_eq!(
        rt.enforce_deadlines().expect("enforce"),
        vec![first.clone()]
    );
    let failed = rt.run(&first).expect("run");
    assert_eq!(failed.state, RunState::Failed);
    assert_eq!(failed.error_kind.as_deref(), Some("deadline"));
    let run = finish(&rt, &world, &second);
    assert_eq!(result(&run), json!(2));
    // The suspended run's model call still settles when it completes.
    world.mock.complete_all();
    rt.quiesce();
    assert!(
        rt.health()
            .expect("health")
            .open_obligations
            .iter()
            .all(|(r, _)| *r != first)
    );
}
