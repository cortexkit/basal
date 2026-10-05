//! The per-flow concurrency slot and the per-run wall-clock deadline: runs
//! of one flow run one at a time in trigger order, a run holds the slot
//! until it is terminal (suspended included), and the deadline fails a
//! stuck run, kills its worker and frees the slot. Run deadlines use a manual
//! clock; fixture waits are bounded so a stuck driver cannot hang the suite.

mod common;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use basal_core::channel::WorkerChannel;
use basal_core::{ActivationEnd, Clock, Config, NoHooks, RunState, Runtime};
use basal_testkit::harness::{World, wait_until};
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
// A driver that is merely slow to finish must not fail on a loaded host.
const DRIVER_WAIT: Duration = Duration::from_secs(60);
// This short limit is exercised only while the fixture deliberately holds a gate.
const STALLED_DRIVER_WAIT: Duration = Duration::from_secs(3);

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
    // Killing the worker cannot release a host call accidentally dispatched on
    // the driver thread. Bound the join and open the fixture's gate on expiry.
    let waited = wait_until(
        Instant::now() + DRIVER_WAIT,
        &format!("expired run {first}'s activation driver to exit"),
        || driver.is_finished(),
    );
    if waited.is_err() {
        world.mock.open_gate("stuck");
        wait_until(
            Instant::now() + DRIVER_WAIT,
            "the released activation driver to exit",
            || driver.is_finished(),
        )
        .expect("driver cleanup");
    }
    let (end, mut worker) = driver.join().expect("driver thread");
    if let Err(error) = waited {
        worker.kill();
        rt.quiesce();
        panic!("{error}");
    }
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
fn a_stuck_driver_wait_deadline_releases_and_reaps_its_worker() {
    let world = World::new("slot-driver-wait");
    let rt = common::runtime(&world);
    let run = admit_trigger(
        &rt,
        &world,
        "await ops.call('mock', 'echo', { gate: 'observer' }); return 1;",
        "t1",
    );
    let mut worker = world.source.spawn().expect("worker");
    let driver = {
        let rt = rt.clone();
        let run = run.clone();
        std::thread::spawn(move || {
            let end = rt.activate(&run, &mut worker);
            (end, worker)
        })
    };
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let waited = wait_until(
            Instant::now() + STALLED_DRIVER_WAIT,
            "a deliberately stalled activation driver",
            || driver.is_finished(),
        );
        tx.send(waited).expect("deadline observer still exists");
        driver
    });
    // This independent bound detects a missing inner deadline. Release the
    // host call even on failure so the driver can return its worker for reaping.
    let waited = rx.recv_timeout(Duration::from_secs(5));
    world.mock.open_gate("observer");
    wait_until(
        Instant::now() + DRIVER_WAIT,
        "the released driver deadline observer to exit",
        || waiter.is_finished(),
    )
    .expect("observer cleanup");
    let driver = waiter.join().expect("deadline observer");
    wait_until(
        Instant::now() + DRIVER_WAIT,
        "the released activation driver to exit",
        || driver.is_finished(),
    )
    .expect("driver cleanup");
    let (_, mut worker) = driver.join().expect("driver thread");
    worker.kill();
    assert!(
        worker.exited(Duration::from_secs(3)),
        "worker was not reaped"
    );
    rt.quiesce();
    let error = waited
        .expect("the driver wait did not fail within five seconds")
        .expect_err("the stalled driver unexpectedly exited");
    assert!(
        error.contains("deliberately stalled activation driver"),
        "{error}"
    );
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
