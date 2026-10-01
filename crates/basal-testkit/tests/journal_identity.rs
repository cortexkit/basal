//! A run replays only under the code and runtime it was recorded with, and
//! a replay that disagrees with the journal fails as nondeterminism.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use basal_core::{ActivationEnd, NoHooks, RunState, Runtime, Store};
use basal_proto::ArgsDigest;
use basal_testkit::ProcessSource;
use basal_testkit::harness::World;
use basal_testkit::worker_binary;
use common::{admit, config, finish, runtime, sql};

fn suspended_run(world: &World, rt: &Runtime) -> String {
    let run_id = admit(
        rt,
        world,
        "const a = await ops.call('mock', 'echo', { n: 1 }); await llm({ prompt: 'p' }); return a.n;",
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    world.mock.complete_all();
    run_id
}

/// The journal says position 0 was called with other arguments: the replay
/// diverges and the run fails with a typed nondeterminism error.
#[test]
fn divergence_fails_the_run_as_nondeterminism() {
    let world = World::new("divergence");
    let rt = runtime(&world);
    let run_id = suspended_run(&world, &rt);
    let other = ArgsDigest::of(&basal_proto::JsonText::new("{\"n\":2}").expect("small"));
    let hex: String = other.0.iter().map(|b| format!("{b:02x}")).collect();
    sql(
        &rt,
        &format!(
            "UPDATE journal SET args_digest = x'{hex}' WHERE run_id = '{run_id}' AND position = 0"
        ),
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Failed, "{run:#?}");
    assert_eq!(
        run.error_kind.as_deref(),
        Some("nondeterminism"),
        "{run:#?}"
    );
    assert!(
        run.error_detail
            .as_deref()
            .unwrap_or("")
            .contains("Divergence"),
        "{run:#?}"
    );
}

/// The run's code changed under it: it ends as `engine_mismatch` and is
/// never replayed (no second `Activate`).
#[test]
fn code_hash_mismatch_is_engine_mismatch_without_replay() {
    let world = World::new("code-hash");
    let rt = runtime(&world);
    let run_id = suspended_run(&world, &rt);
    sql(
        &rt,
        &format!("UPDATE runs SET script = script || ' ' WHERE run_id = '{run_id}'"),
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::EngineMismatch, "{run:#?}");
    assert_eq!(world.source.counts.activate.load(Ordering::SeqCst), 1);
}

/// A worker reporting a different engine than the one the run was recorded
/// under: the parent checks the whole fingerprint itself (the worker
/// compares only the prelude hash), and ends the run as `engine_mismatch`
/// without replaying.
#[test]
fn fingerprint_mismatch_is_engine_mismatch_without_replay() {
    let world = World::new("fingerprint");
    let run_id = {
        let rt = runtime(&world);
        let run_id = suspended_run(&world, &rt);
        rt.quiesce();
        run_id
    };
    world.mock.detach();
    let mut other = ProcessSource::new(worker_binary());
    other.engine_override = Some("quickjs-ng 9.9.9 via rquickjs 9.9.9".into());
    let other = Arc::new(other);
    let store = Arc::new(Store::open(world.store_path(), world.durability).expect("store"));
    let rt = Runtime::new(
        store,
        Arc::new(world.mock.clone()),
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(other.clone()),
        config(),
    );
    let end = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(end, ActivationEnd::EngineMismatch { .. }),
        "{end:?}"
    );
    assert_eq!(
        rt.run(&run_id).expect("run").state,
        RunState::EngineMismatch
    );
    assert_eq!(
        other.counts.activate.load(Ordering::SeqCst),
        0,
        "it replayed"
    );
}
