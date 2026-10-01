//! The worker pool, driven through the engine with the real worker binary:
//! workers bound to one flow for life, a killed worker replaced while its
//! run carries on, warm spares used, retirement, and a run's deadline that
//! starts only after the worker's handshake (proved on the manual clock).

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use basal_core::{Boundary, Hooks, RunState, Step};
use basal_module::pool::{Binding, Pool, Source};
use common::{
    GatedSpawner, HOUR, Options, T0, admit, agent, events_manifest, fixture, install_approved,
    pool_config,
};

const SCRIPT: &str = "const r = await ops.call('mock', 'echo', { n: 1 }); return r.n;";

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
        f.module.engine.run_until_idle(50).expect("idle");
        assert_eq!(f.module.rt.run(&run).expect("run").state, RunState::Succeeded);
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
    assert_eq!(on_a[0], on_a[1], "the second run of a flow reuses its worker");
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
    f.module.engine.run_until_idle(50).expect("idle");
    let state = f.module.rt.run(&run).expect("run");
    assert_eq!(state.state, RunState::Succeeded, "{state:#?}");
    assert_eq!(state.broken, 1, "exactly one activation lost its worker");
    let used = workers_of(&f.module.pool, &flow);
    assert_eq!(used.len(), 2, "{:?}", f.module.pool.handouts());
    assert_ne!(used[0], used[1], "the run continued on another worker");
    let metrics = &f.module.metrics;
    assert_eq!(metrics.workers_killed.load(Ordering::Relaxed), 1);
    assert!(f.module.pool.wait_for_spares(1, Duration::from_secs(120)));
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
    assert!(f.module.pool.wait_for_spares(1, Duration::from_secs(120)));
    let spawned_before = f.module.metrics.workers_spawned.load(Ordering::Relaxed);
    let flow = install_approved(&f, &agent("SYNAPSE"), SCRIPT, &events_manifest("flow-spare"));
    let run = admit(&f, &flow, "s-1");
    f.module.engine.run_until_idle(50).expect("idle");
    assert_eq!(f.module.rt.run(&run).expect("run").state, RunState::Succeeded);
    let handouts = f.module.pool.handouts();
    let last = handouts.last().expect("a handout");
    assert_eq!(last.source, Source::Spare, "{handouts:?}");
    assert_eq!(
        f.module.metrics.spawned_on_demand.load(Ordering::Relaxed),
        0,
        "no activation waited for a spawn"
    );
    // The spare was replaced in the background.
    assert!(f.module.pool.wait_for_spares(1, Duration::from_secs(120)));
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
    let flow = install_approved(&f, &agent("SYNAPSE"), SCRIPT, &events_manifest("flow-retire"));
    for trigger in ["r-1", "r-2", "r-3"] {
        admit(&f, &flow, trigger);
        f.module.engine.run_until_idle(50).expect("idle");
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
        .recv_timeout(Duration::from_secs(60))
        .expect("the activation asked for a spawn");
    f.clock.set(T0 + HOUR);
    spawner.release(1);
    f.module.engine.wait_quiet();
    // ...does not count against a ten-second deadline: the run is claimed
    // only once the worker has answered.
    let state = f.module.rt.run(&run).expect("run");
    assert_eq!(state.state, RunState::Succeeded, "{state:#?}");
    assert_eq!(state.deadline_at, Some(T0 + HOUR + 10_000));
}
