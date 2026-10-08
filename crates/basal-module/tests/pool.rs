//! The worker pool, driven through the engine with the real worker binary:
//! workers bound to one flow for life, a killed worker replaced while its
//! run carries on, warm spares used, retirement, and a run's deadline that
//! starts only after the worker's handshake (proved on the manual clock).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use basal_core::{Boundary, Hooks, RunState, Step};
use basal_module::pool::{Binding, Pool, PoolConfig, ProcessSpawner, Source, Spawn};
use basal_module::process::{SpawnError, WorkerProcess};
use basal_testkit::harness::wait_until;
use common::{
    Fixture, GatedSpawner, HOUR, Options, T0, admit, agent, events_manifest, install_approved,
};

const SCRIPT: &str = "const r = await ops.call('mock', 'echo', { n: 1 }); return r.n;";
const WAIT: Duration = Duration::from_secs(60);
const STALLED_WAIT: Duration = Duration::from_secs(3);

fn pool_config(o: &Options) -> PoolConfig {
    let mut config = common::pool_config(o);
    config.handshake_timeout = WAIT;
    config.acquire_timeout = WAIT;
    config
}

struct PoolFixture {
    inner: Fixture,
    spawner: Arc<RecordedSpawner>,
}

impl std::ops::Deref for PoolFixture {
    type Target = Fixture;

    fn deref(&self) -> &Fixture {
        &self.inner
    }
}

fn fixture(tag: &str, mut o: Options) -> PoolFixture {
    o.activation_deadline = WAIT;
    o.pool_wait_timeout = Some(WAIT);
    let inner = o
        .spawner
        .take()
        .unwrap_or_else(|| Arc::new(ProcessSpawner::new(&pool_config(&o))));
    let spawner = Arc::new(RecordedSpawner {
        inner,
        pids: Mutex::new(Vec::new()),
    });
    o.spawner = Some(spawner.clone());
    PoolFixture {
        inner: common::fixture(tag, o),
        spawner,
    }
}

fn stop_workers(f: &PoolFixture, runs: &[String]) {
    for run in runs {
        f.module.pool.kill_for_run(run);
    }
    f.module.pool.stop();
    f.spawner.reap_workers();
    // Wait for busy activations and pending spawns to release their pool entries.
    // Stop and reap again to cover workers created while the pool was closing.
    let deadline = Instant::now() + WAIT;
    while f.module.pool.stats().busy != 0 || f.module.pool.stats().spawning != 0 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for killed pool workers to be reaped"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    f.module.pool.stop();
    f.spawner.reap_workers();
}

fn wait_quiet(f: &PoolFixture, runs: &[String], description: &str) {
    if let Err(error) = wait_until(Instant::now() + WAIT, description, || {
        f.module.engine.active_count() == 0
    }) {
        stop_workers(f, runs);
        panic!("{error}");
    }
}

fn run_until_idle(f: &PoolFixture, run: &str) {
    // The engine's convenience method has an unbounded quiescence wait.
    // Drive the same passes here so a stuck activation fails with its run ID.
    let deadline = Instant::now() + WAIT;
    for _ in 0..50 {
        let report = f.module.engine.pass().expect("engine pass");
        if let Err(error) = wait_until(
            deadline,
            &format!("run {run} and its worker to become idle"),
            || f.module.engine.active_count() == 0,
        ) {
            stop_workers(f, &[run.to_owned()]);
            panic!("{error}");
        }
        if report.started.is_empty() && report.admitted.is_empty() && report.expired.is_empty() {
            return;
        }
    }
    stop_workers(f, &[run.to_owned()]);
    panic!("run {run} did not become idle after 50 engine passes");
}

fn wait_for_spares(f: &PoolFixture) {
    if !f.module.pool.wait_for_spares(1, WAIT) {
        stop_workers(f, &[]);
        panic!("timed out waiting for a greeted spare worker");
    }
}

fn workers_of(pool: &Pool, flow: &str) -> Vec<u64> {
    pool.handouts()
        .into_iter()
        .filter(|h| h.binding == Binding::Flow(flow.to_owned()))
        .map(|h| h.worker)
        .collect()
}

#[test]
fn a_worker_is_never_reused_across_flows() {
    let f = fixture("pool-isolation", Options::default());
    let a = install_approved(&f, &agent("SYNAPSE"), SCRIPT, &events_manifest("flow-a"));
    let b = install_approved(&f, &agent("SYNAPSE"), SCRIPT, &events_manifest("flow-b"));
    for (flow, trigger) in [(&a, "a-1"), (&b, "b-1"), (&a, "a-2"), (&b, "b-2")] {
        let run = admit(&f, flow, trigger);
        run_until_idle(&f, &run);
        assert_eq!(
            f.module.rt.run(&run).expect("run").state,
            RunState::Succeeded
        );
    }
    let on_a = workers_of(&f.module.pool, &a);
    let on_b = workers_of(&f.module.pool, &b);
    assert_eq!(on_a.len(), 2);
    assert_eq!(on_b.len(), 2);
    assert!(
        on_a.iter().all(|w| !on_b.contains(w)),
        "a worker served both flows: {:?}",
        f.module.pool.handouts()
    );
    // Reuse within a flow happens, so the check above is not vacuous.
    assert_eq!(
        on_a[0], on_a[1],
        "the second run of a flow reuses its worker"
    );
    assert_eq!(on_b[0], on_b[1]);
}

/// Kills the worker of a run the first time the run commits a call's row,
/// that is, mid-activation, while the script waits on that call.
struct KillWorkerOnFirstCall {
    pool: Arc<OnceLock<Pool>>,
    done: std::sync::atomic::AtomicBool,
}

impl Hooks for KillWorkerOnFirstCall {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        if *boundary == (Boundary::CallCommitted { position: 0 })
            && !self.done.swap(true, Ordering::SeqCst)
        {
            let killed = self.pool.get().is_some_and(|p| p.kill_for_run(run_id));
            assert!(killed, "the run's worker was found and killed");
        }
        Step::Continue
    }
}

#[test]
fn a_killed_worker_is_replaced_and_the_run_continues() {
    let slot = Arc::new(OnceLock::new());
    let hooks = Arc::new(KillWorkerOnFirstCall {
        pool: slot.clone(),
        done: Default::default(),
    });
    let f = fixture(
        "pool-kill",
        Options {
            hooks,
            ..Options::default()
        },
    );
    let _ = slot.set(f.module.pool.clone());
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        "const r = await ops.call('mock', 'echo', { n: 1 });\n\
         await sink.digest('SYNAPSE', { title: 'once' }, 'piggyback');\n\
         return r.n;",
        &events_manifest("flow-kill"),
    );
    let run = admit(&f, &flow, "k-1");
    run_until_idle(&f, &run);
    let state = f.module.rt.run(&run).expect("run");
    assert_eq!(state.state, RunState::Succeeded, "{state:#?}");
    assert_eq!(state.broken, 1, "exactly one activation lost its worker");
    let used = workers_of(&f.module.pool, &flow);
    assert_eq!(used.len(), 2, "{:?}", f.module.pool.handouts());
    assert_ne!(used[0], used[1], "the run continued on another worker");
    let metrics = &f.module.metrics;
    assert_eq!(metrics.workers_killed.load(Ordering::Relaxed), 1);
    wait_for_spares(&f);
    assert_eq!(
        metrics.workers_respawned.load(Ordering::Relaxed),
        1,
        "the killed worker was replaced"
    );
    // The digest write happened once.
    let digests = f
        .mock
        .effects()
        .into_iter()
        .filter(|e| e.op == "sink.digest")
        .count();
    assert_eq!(digests, 1);
}

#[test]
fn an_activation_uses_a_warm_spare() {
    let f = fixture("pool-spare", Options::default());
    wait_for_spares(&f);
    let spawned_before = f.module.metrics.workers_spawned.load(Ordering::Relaxed);
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        SCRIPT,
        &events_manifest("flow-spare"),
    );
    let run = admit(&f, &flow, "s-1");
    run_until_idle(&f, &run);
    assert_eq!(
        f.module.rt.run(&run).expect("run").state,
        RunState::Succeeded
    );
    let handouts = f.module.pool.handouts();
    let last = handouts.last().expect("a handout");
    assert_eq!(last.source, Source::Spare, "{handouts:?}");
    assert_eq!(
        f.module.metrics.spawned_on_demand.load(Ordering::Relaxed),
        0,
        "no activation waited for a spawn"
    );
    // The spare was replaced in the background.
    wait_for_spares(&f);
    assert!(f.module.metrics.workers_spawned.load(Ordering::Relaxed) > spawned_before);
}

#[test]
fn a_bound_worker_is_retired_after_its_activation_count_and_its_idle_period() {
    let f = fixture(
        "pool-retire",
        Options {
            max_activations: 2,
            idle_retire: Duration::from_secs(60),
            ..Options::default()
        },
    );
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        SCRIPT,
        &events_manifest("flow-retire"),
    );
    for trigger in ["r-1", "r-2", "r-3"] {
        let run = admit(&f, &flow, trigger);
        run_until_idle(&f, &run);
    }
    let used = workers_of(&f.module.pool, &flow);
    assert_eq!(used[0], used[1]);
    assert_ne!(used[1], used[2], "retired after two activations");
    assert_eq!(f.module.pool.stats().bound, vec![(flow.clone(), 1)]);

    // Idle past the period on the runtime's clock: the next pass retires it.
    f.clock.advance(61_000);
    f.module.engine.pass().expect("pass");
    assert!(f.module.pool.stats().bound.is_empty());
    assert!(f.module.metrics.workers_retired.load(Ordering::Relaxed) >= 2);
}

#[test]
fn the_run_deadline_starts_after_the_worker_answers_its_handshake() {
    let o = Options {
        warm_spares: 0,
        default_deadline: Duration::from_secs(10),
        ..Options::default()
    };
    let (spawner, requested) = GatedSpawner::new(&pool_config(&o));
    let f = fixture(
        "pool-deadline",
        Options {
            spawner: Some(spawner.clone()),
            ..o
        },
    );
    let flow = install_approved(&f, &agent("SYNAPSE"), SCRIPT, &events_manifest("flow-late"));
    let run = admit(&f, &flow, "d-1");
    let report = f.module.engine.pass().expect("pass");
    assert_eq!(report.started, vec![run.clone()]);
    // The activation is waiting for a worker to be spawned. A launch that
    // stalls for an hour on the runtime's clock...
    requested
        .recv_timeout(WAIT)
        .expect("the activation asked for a spawn");
    f.clock.set(T0 + HOUR);
    spawner.release(1);
    wait_quiet(
        &f,
        std::slice::from_ref(&run),
        "the worker's handshake activation to finish",
    );
    // ...does not count against a ten-second deadline: the run is claimed
    // only once the worker has answered.
    let state = f.module.rt.run(&run).expect("run");
    assert_eq!(state.state, RunState::Succeeded, "{state:#?}");
    assert_eq!(state.deadline_at, Some(T0 + HOUR + 10_000));
}

struct RecordedSpawner {
    inner: Arc<dyn Spawn>,
    pids: Mutex<Vec<libc::pid_t>>,
}

impl RecordedSpawner {
    fn reap_workers(&self) {
        for pid in self.pids.lock().unwrap().iter().copied() {
            let deadline = Instant::now() + WAIT;
            loop {
                let mut status = 0;
                // SAFETY: the spawner recorded direct children of this test.
                let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if result == pid
                    || (result == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                {
                    break;
                }
                assert_eq!(result, 0, "could not wait for worker {pid}");
                // A live, unreaped child may not have a run binding when startup
                // failed. Kill it directly rather than relying on that binding.
                // SAFETY: waitpid just identified a live child this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for killed worker {pid} to be reaped"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Spawn for RecordedSpawner {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        let process = self.inner.spawn()?;
        self.pids
            .lock()
            .unwrap()
            .push(process.pid().try_into().expect("worker pid"));
        Ok(process)
    }
}

#[test]
fn a_wait_deadline_stops_and_reaps_its_workers() {
    let spawner = Arc::new(RecordedSpawner {
        inner: Arc::new(ProcessSpawner::new(&pool_config(&Options::default()))),
        pids: Mutex::new(Vec::new()),
    });
    let f = fixture(
        "pool-wait-deadline",
        Options {
            spawner: Some(spawner.clone()),
            ..Options::default()
        },
    );
    wait_for_spares(&f);
    let pids = spawner.pids.lock().unwrap().clone();
    assert_eq!(pids.len(), 1, "one real spare worker was started");
    let pool = f.module.pool.clone();
    let release = Arc::new(AtomicBool::new(false));
    let ready = release.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let result = wait_until(
            Instant::now() + STALLED_WAIT,
            "a deliberately stalled pool fixture",
            || ready.load(Ordering::SeqCst),
        );
        if result.is_err() {
            pool.stop();
        }
        tx.send(result).expect("deadline observer still exists");
    });
    // An independent outer bound catches removal of the wait's own deadline.
    // Release the waiter even on failure, so neither a thread nor a worker leaks.
    let result = rx.recv_timeout(Duration::from_secs(5));
    release.store(true, Ordering::SeqCst);
    let join_deadline = Instant::now() + WAIT;
    while !waiter.is_finished() && Instant::now() < join_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        waiter.is_finished(),
        "timed out waiting for the released deadline observer to exit"
    );
    waiter.join().expect("deadline waiter exits after release");
    // Probe before the test's fallback shutdown: only the observer's timeout
    // path may have reaped these workers.
    let observer_stopped = result.as_ref().is_ok_and(|r| r.is_err());
    if !observer_stopped {
        f.module.pool.stop();
    }
    for pid in pids {
        let mut status = 0;
        // SAFETY: this nonblocking wait probes a child started by this test.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1, "worker {pid} was not reaped by pool shutdown");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    let error = result
        .expect("pool wait did not enforce its own three-second deadline")
        .expect_err("the stalled fixture must time out");
    assert_eq!(
        error,
        "timed out waiting for a deliberately stalled pool fixture"
    );
}
