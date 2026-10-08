//! Real workers, with a fallback longer than every observer's hang bound.
mod common;

use basal_core::RunState;
use basal_module::engine::{Engine, EngineConfig};
use common::{Options, admit, agent, events_manifest, fixture, install_approved};
use serde_json::json;
use std::time::Duration;

fn loop_engine(f: &common::Fixture, max: usize) -> Engine {
    Engine::new(
        f.module.rt.clone(),
        f.module.pool.clone(),
        f.module.consent.clone(),
        EngineConfig {
            max_concurrent_activations: max,
            pass_interval: Duration::from_secs(600),
        },
        f.module.metrics.clone(),
        f.module.fatal.clone(),
    )
    .unwrap()
}

#[test]
fn completing_a_run_and_returning_pool_capacity_start_the_next_flow() {
    let f = fixture(
        "idle-completion",
        Options {
            max_concurrent: 1,
            ..Options::default()
        },
    );
    let held = install_approved(
        &f,
        &agent("SYNAPSE"),
        "await ops.call('mock','echo',{gate:'hold'}); return 1;",
        &events_manifest("held"),
    );
    let next = install_approved(&f, &agent("SYNAPSE"), "return 2;", &events_manifest("next"));
    let first = admit(&f, &held, "first");
    let second = admit(&f, &next, "second");
    let engine = loop_engine(&f, 1);
    let loop_thread = engine.spawn_loop();
    assert!(engine.wait_active(&first, Duration::from_secs(60)));
    assert_eq!(f.module.rt.run(&second).unwrap().state, RunState::Pending);
    f.mock.open_gate("hold");
    let outcome = f.module.rt.wait_for(&second, Duration::from_secs(60), |s| {
        s == RunState::Succeeded
    });
    engine.stop();
    loop_thread.join().unwrap();
    engine.wait_quiet();
    assert_eq!(outcome.unwrap(), RunState::Succeeded);
    assert_eq!(f.module.rt.run(&first).unwrap().state, RunState::Succeeded);
}

#[test]
fn a_host_completion_resumes_a_suspended_run_without_the_fallback() {
    let f = fixture("idle-host-completion", Options::default());
    let mut manifest = events_manifest("long");
    manifest["ops"]
        .as_array_mut()
        .unwrap()
        .push(json!({"module":"mock","op":"long"}));
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        "return await ops.call('mock','long',{});",
        &manifest,
    );
    let run = admit(&f, &flow, "first");
    let engine = loop_engine(&f, 1);
    let loop_thread = engine.spawn_loop();
    f.module
        .rt
        .wait_for(&run, Duration::from_secs(60), |s| s == RunState::Suspended)
        .unwrap();
    engine.wait_quiet();
    f.mock.complete_all();
    let outcome = f
        .module
        .rt
        .wait_for(&run, Duration::from_secs(60), |s| s == RunState::Succeeded);
    engine.stop();
    loop_thread.join().unwrap();
    engine.wait_quiet();
    assert_eq!(outcome.unwrap(), RunState::Succeeded);
}
