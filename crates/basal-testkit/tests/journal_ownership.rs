//! One activation owns a run: a superseded activation writes nothing more,
//! its worker is killed, and nothing is dispatched twice. Storage failures
//! fail closed.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use basal_core::channel::WorkerChannel;
use basal_core::runs::Lease;
use basal_core::{ActivationEnd, Boundary, RunState, Runtime, Step};
use basal_testkit::ProcessSource;
use basal_testkit::harness::{FnHooks, World, wait_until};
use basal_testkit::worker_binary;
use common::{admit, admit_as, finish, query_i64, result, runtime, runtime_with};
use serde_json::json;

type Slot = Arc<OnceLock<Runtime>>;
const OWNER_DRIVER_WAIT: Duration = Duration::from_secs(3);

/// Hooks that, the first time `when` matches, take the run over for a
/// second owner and keep its lease.
fn takeover_at(
    slot: Slot,
    taken: Arc<Mutex<Option<Lease>>>,
    when: impl Fn(&Boundary) -> bool + Send + Sync + 'static,
) -> FnHooks<impl Fn(&str, &Boundary) -> Step + Send + Sync> {
    let once = AtomicBool::new(false);
    FnHooks(move |run_id: &str, b: &Boundary| {
        if when(b) && !once.swap(true, Ordering::SeqCst) {
            let rt = slot.get().expect("runtime set");
            let generation = rt.run(run_id).expect("run").generation;
            let lease = rt
                .with_owner("owner-b")
                .take_over(run_id, generation)
                .expect("take over");
            if let Ok(mut t) = taken.lock() {
                *t = Some(lease);
            }
        }
        Step::Continue
    })
}

fn issued_generation(rt: &Runtime, run_id: &str, position: u64) -> i64 {
    query_i64(
        rt,
        &format!(
            "SELECT issued_generation FROM journal WHERE run_id = '{run_id}' AND position = {position}"
        ),
    )
}

/// The run is taken over after the worker issued a call and before its row
/// committed. The old activation's insert is fenced out: it dispatches
/// nothing and its worker is killed. The new owner journals and dispatches
/// the call once.
#[test]
fn owner_loss_before_call_commit_dispatches_nothing() {
    let world = World::new("owner-before-commit");
    let slot: Slot = Arc::new(OnceLock::new());
    let taken = Arc::new(Mutex::new(None));
    let hooks = takeover_at(slot.clone(), taken.clone(), |b| {
        matches!(b, Boundary::HostCallReceived { position: 0 })
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let _ = slot.set(rt.clone());
    let run_id = admit(
        &rt,
        &world,
        "const r = await ops.call('mock', 'post', { n: 1 }); return r.applied.n;",
    );
    let mut old = world.source.spawn().expect("worker");
    let end = rt.activate(&run_id, &mut old).expect("activation");
    assert_eq!(end, ActivationEnd::OwnerLost);
    assert!(
        old.exited(Duration::from_secs(5)),
        "the old worker was not killed"
    );
    rt.quiesce();
    assert_eq!(world.mock.total_sends(), 0, "the old activation dispatched");
    assert!(rt.calls(&run_id).expect("calls").is_empty());

    let lease = taken.lock().ok().and_then(|mut t| t.take()).expect("lease");
    let new_generation = lease.generation;
    let mut fresh = world.source.spawn().expect("worker");
    let end = rt.with_owner("owner-b").drive(lease, &mut fresh);
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    rt.quiesce();
    assert_eq!(world.mock.total_sends(), 1);
    assert_eq!(world.mock.effects().len(), 1);
    assert_eq!(
        issued_generation(&rt, &run_id, 0),
        new_generation as i64,
        "the call row was written by the old activation"
    );
}

/// The run is taken over after the call's row committed and before the
/// call left. The old activation was authorized when it committed, so it
/// dispatches; the new owner's recovery sees the dispatch in flight and does
/// not send the call again, even though it is a mutation that ignores
/// idempotency keys.
#[test]
fn owner_loss_after_call_commit_dispatches_once() {
    let world = World::new("owner-after-commit");
    let slot: Slot = Arc::new(OnceLock::new());
    let taken = Arc::new(Mutex::new(None));
    let takeover = takeover_at(slot.clone(), taken.clone(), |b| {
        matches!(b, Boundary::CallCommitted { position: 0 })
    });
    let mock = world.mock.clone();
    let blocked = AtomicUsize::new(0);
    // The gated call is released once the new activation is blocked on it.
    let hooks = FnHooks(move |run_id: &str, b: &Boundary| {
        if matches!(b, Boundary::BlockedReceived) && blocked.fetch_add(1, Ordering::SeqCst) == 1 {
            mock.open_gate("g");
        }
        (takeover.0)(run_id, b)
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let _ = slot.set(rt.clone());
    let run_id = admit(
        &rt,
        &world,
        "const r = await ops.call('mock', 'post', { n: 1, gate: 'g' }); return r.applied.n;",
    );
    let mut old = world.source.spawn().expect("worker");
    let driver = {
        let rt = rt.clone();
        let run_id = run_id.clone();
        std::thread::spawn(move || {
            let end = rt.activate(&run_id, &mut old);
            (end, old)
        })
    };
    // The gate normally opens in the new owner's activation. If the old
    // driver dispatches inline, it cannot return to start that activation.
    let waited = wait_until(
        Instant::now() + OWNER_DRIVER_WAIT,
        &format!("superseded run {run_id}'s activation driver to exit"),
        || driver.is_finished(),
    );
    if waited.is_err() {
        world.mock.open_gate("g");
        wait_until(
            Instant::now() + OWNER_DRIVER_WAIT,
            "the released superseded activation driver to exit",
            || driver.is_finished(),
        )
        .expect("driver cleanup");
    }
    let (end, mut old) = driver.join().expect("driver thread");
    if let Err(error) = waited {
        old.kill();
        rt.quiesce();
        panic!("{error}");
    }
    let end = end.expect("activation");
    assert_eq!(end, ActivationEnd::OwnerLost);
    assert!(
        old.exited(Duration::from_secs(5)),
        "the old worker was not killed"
    );

    let lease = taken.lock().ok().and_then(|mut t| t.take()).expect("lease");
    let mut fresh = world.source.spawn().expect("worker");
    let end = rt.with_owner("owner-b").drive(lease, &mut fresh);
    world.mock.open_gate("g");
    rt.quiesce();
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert_eq!(world.mock.total_sends(), 1, "the call was dispatched twice");
    assert_eq!(world.mock.effects().len(), 1);
}

#[test]
fn a_stuck_ownership_driver_wait_deadline_releases_and_reaps_its_worker() {
    let world = World::new("owner-driver-wait");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        "await ops.call('mock', 'post', { gate: 'observer' }); return 1;",
    );
    let mut worker = world.source.spawn().expect("worker");
    let driver = {
        let rt = rt.clone();
        std::thread::spawn(move || {
            let end = rt.activate(&run_id, &mut worker);
            (end, worker)
        })
    };
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let waited = wait_until(
            Instant::now() + OWNER_DRIVER_WAIT,
            "a deliberately stalled ownership driver",
            || driver.is_finished(),
        );
        tx.send(waited).expect("deadline observer still exists");
        driver
    });
    // The outer bound must remain independent of the driver's wait. Always
    // release its host gate so the test can join and reap before failing.
    let waited = rx.recv_timeout(Duration::from_secs(5));
    world.mock.open_gate("observer");
    wait_until(
        Instant::now() + Duration::from_secs(5),
        "the released ownership deadline observer to exit",
        || waiter.is_finished(),
    )
    .expect("observer cleanup");
    let driver = waiter.join().expect("deadline observer");
    wait_until(
        Instant::now() + Duration::from_secs(5),
        "the released ownership driver to exit",
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
        .expect("the ownership driver wait did not fail within five seconds")
        .expect_err("the stalled ownership driver unexpectedly exited");
    assert!(
        error.contains("deliberately stalled ownership driver"),
        "{error}"
    );
}

/// The run is taken over while the old worker is blocked, before a
/// delivery order is committed: the old activation releases nothing. A
/// second takeover lands after an order committed: the frame for that
/// order may still go out, but the old activation cannot finish the run.
#[test]
fn owner_loss_around_order_commit_delivers_nothing_more() {
    let world = World::new("owner-order");
    let slot: Slot = Arc::new(OnceLock::new());
    let taken = Arc::new(Mutex::new(None));
    let hooks = takeover_at(slot.clone(), taken.clone(), |b| {
        matches!(b, Boundary::BlockedReceived)
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let _ = slot.set(rt.clone());
    let script = "const r = await ops.call('mock', 'echo', { n: 1, delay_ms: 50 }); return r.n;";
    let run_id = admit(&rt, &world, script);
    let old_source = ProcessSource::new(worker_binary());
    let mut old = old_source.spawn().expect("worker");
    let end = rt.activate(&run_id, &mut old).expect("activation");
    assert_eq!(end, ActivationEnd::OwnerLost);
    assert!(
        old.exited(Duration::from_secs(5)),
        "the old worker was not killed"
    );
    assert_eq!(
        old_source.counts.deliver.load(Ordering::SeqCst),
        0,
        "the superseded activation delivered an outcome"
    );
    let lease = taken.lock().ok().and_then(|mut t| t.take()).expect("lease");
    let mut fresh = world.source.spawn().expect("worker");
    let end = rt.with_owner("owner-b").drive(lease, &mut fresh);
    rt.quiesce();
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert_eq!(world.mock.total_sends(), 1);

    // Takeover right after an order committed.
    let world = World::new("owner-order-2");
    let slot: Slot = Arc::new(OnceLock::new());
    let taken = Arc::new(Mutex::new(None));
    let hooks = takeover_at(slot.clone(), taken.clone(), |b| {
        matches!(b, Boundary::OrderCommitted { position: 0, .. })
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let _ = slot.set(rt.clone());
    let run_id = admit(&rt, &world, script);
    let mut old = world.source.spawn().expect("worker");
    let end = rt.activate(&run_id, &mut old).expect("activation");
    assert_eq!(end, ActivationEnd::OwnerLost);
    assert_eq!(rt.run(&run_id).expect("run").state, RunState::Running);
    let lease = taken.lock().ok().and_then(|mut t| t.take()).expect("lease");
    let mut fresh = world.source.spawn().expect("worker");
    let end = rt.with_owner("owner-b").drive(lease, &mut fresh);
    rt.quiesce();
    assert_eq!(end, ActivationEnd::Succeeded { value: "1".into() });
    assert_eq!(world.mock.total_sends(), 1);
}

/// The store fails just as a call arrives: nothing is dispatched and the
/// worker is killed. A completion the store cannot record is not
/// acknowledged, so the host keeps it and delivers it again to the next
/// runtime.
#[test]
fn storage_failure_before_commit_dispatches_nothing() {
    let world = World::new("fail-closed");
    let slot: Slot = Arc::new(OnceLock::new());
    let inner = slot.clone();
    let hooks = FnHooks(move |_: &str, b: &Boundary| {
        if matches!(b, Boundary::HostCallReceived { position: 0 }) {
            inner.get().expect("runtime").store().cut();
        }
        Step::Continue
    });
    let rt = runtime_with(&world, Arc::new(hooks));
    let _ = slot.set(rt.clone());
    let run_id = admit(
        &rt,
        &world,
        "const r = await ops.call('mock', 'post', { n: 1 }); return r.applied.n;",
    );
    let mut worker = world.source.spawn().expect("worker");
    let end = rt.activate(&run_id, &mut worker).expect("activation");
    assert_eq!(end, ActivationEnd::Crashed);
    assert!(worker.exited(Duration::from_secs(5)));
    rt.quiesce();
    assert_eq!(
        world.mock.total_sends(),
        0,
        "dispatched without a committed row"
    );
    drop(rt);

    // A completion offered to a failed store is not acknowledged. (A fresh
    // world: the hooks above hold the first runtime, and with it the
    // store's lease, until the test ends.)
    let world = World::new("fail-closed-sink");
    let rt = runtime(&world);
    let run_id = admit_as(
        &rt,
        &world,
        "const x = await llm({ prompt: 'p' }); return x.done;",
        "trigger-2",
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    rt.store().cut();
    world.mock.complete_all();
    assert!(world.mock.pending_long().is_empty());
    rt.quiesce();
    drop(rt);
    world.mock.detach();
    let rt = runtime(&world);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), json!(true));
}
