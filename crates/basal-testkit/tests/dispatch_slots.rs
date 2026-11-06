//! The per-flow concurrency slot and the per-run wall-clock deadline: runs
//! of one flow run one at a time in trigger order, a run holds the slot
//! until it is terminal (suspended included), and the deadline fails a
//! stuck run, kills its worker and frees the slot. Run deadlines use a manual
//! clock; fixture waits are bounded so a stuck driver cannot hang the suite.

mod common;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use basal_core::{ActivationEnd, Clock, Config, NoHooks, RunState, Runtime};
use basal_testkit::harness::{World, wait_until};
use common::{admit_as, admit_trigger, config, finish, result};
use serde_json::json;

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
    use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
    use basal_proto::CallKind;

    struct EnteredHost {
        mock: basal_host::mock::MockHost,
        entered: mpsc::Sender<()>,
    }
    impl Host for EnteredHost {
        fn classify(&self, kind: &CallKind) -> CallClass {
            self.mock.classify(kind)
        }
        fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
            if request.args.as_str().contains("stuck") {
                self.entered.send(()).unwrap();
            }
            self.mock.dispatch(request)
        }
        fn attach(&self, sink: Arc<dyn CompletionSink>) {
            self.mock.attach(sink);
        }
        fn now_ms(&self) -> f64 {
            self.mock.now_ms()
        }
        fn random(&self) -> f64 {
            self.mock.random()
        }
    }

    let world = World::new("slot-deadline");
    let clock = Clock::manual(T0);
    let (entered, in_host) = mpsc::channel();
    let rt = Runtime::new(
        Arc::new(basal_core::Store::open(world.store_path(), world.durability).unwrap()),
        Arc::new(EnteredHost {
            mock: world.mock.clone(),
            entered,
        }),
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            clock: clock.clone(),
            ..config()
        },
    )
    .unwrap();
    rt.retain_detail_for_test().unwrap();
    // The first run's call never answers until the test opens its gate.
    let first = admit_trigger(
        &rt,
        &world,
        "await ops.call('mock', 'echo', { gate: 'stuck' }); await ops.call('mock', 'echo', {}); return 1;",
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
    // Observe host entry, not just Intent creation: advancing the clock before
    // dispatch would test expiry before the host receives the call instead.
    in_host.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(rt.run(&first).expect("run").state, RunState::Running);
    assert!(matches!(
        rt.resume(&second).expect("activation"),
        ActivationEnd::Waiting { .. }
    ));

    // Past the default ten-minute deadline.
    clock.advance(601_000);
    let enforcer = {
        let rt = rt.clone();
        std::thread::spawn(move || rt.enforce_deadlines())
    };
    let reaped = wait_until(
        Instant::now() + DRIVER_WAIT,
        "expired run's worker to be reaped",
        || {
            world
                .source
                .counts
                .live_pid
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
        },
    );
    let sealed = wait_until(
        Instant::now() + DRIVER_WAIT,
        "expired run to be sealed Closing",
        || rt.fixture_routing(&first) == Some(basal_core::run_store::Routing::Closing),
    );
    let held_slot = rt.resume(&second).expect("activation while draining");
    let not_terminal = !rt.run(&first).unwrap().state.is_terminal();
    let independent = admit_as(&rt, &world, "return 3;", "independent");
    let (done, independent_done) = mpsc::channel();
    let other_driver = {
        let rt = rt.clone();
        std::thread::spawn(move || done.send(rt.resume(&independent)).unwrap())
    };
    let independent_end = independent_done.recv_timeout(DRIVER_WAIT);
    // Close drains authorized host calls synchronously. The worker dies at the
    // deadline, but the run becomes terminal and releases its flow slot only
    // after the host reply; independent flows must still progress meanwhile.
    world.mock.open_gate("stuck");
    wait_until(
        Instant::now() + DRIVER_WAIT,
        "deadline close to drain after the reply",
        || driver.is_finished() && enforcer.is_finished() && other_driver.is_finished(),
    )
    .expect("bounded drain");
    let (end, mut worker) = driver.join().expect("driver thread");
    enforcer.join().unwrap().expect("enforce");
    other_driver.join().unwrap();
    reaped.expect("deadline must reap the worker before the host replies");
    sealed.expect("deadline must seal the run before the host replies");
    assert!(
        not_terminal,
        "terminal projection must wait for the entrant"
    );
    assert!(matches!(held_slot, ActivationEnd::Waiting {holder} if holder == first));
    assert!(
        matches!(independent_end.unwrap().unwrap(), ActivationEnd::Succeeded {value} if value == "3")
    );
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

    // The script was sealed at the deadline and never issued its second call.
    assert_eq!(world.mock.total_sends(), 1);
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
    // The five-second receive timeout is the test's own check that the
    // observer's STALLED_DRIVER_WAIT fired. Open the mock's "observer" gate
    // whatever the result, so the stalled call returns and the driver hands
    // back its worker for reaping.
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
    let (_, worker) = driver.join().expect("driver thread");
    let pid = worker.pid();
    drop(worker);
    basal_testkit::process::assert_reaped(pid);
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
