//! Shared helpers for the journal and runtime tests.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use basal_core::{Config, Hooks, NoHooks, Run, Runtime};
use basal_testkit::harness::{World, drive};

pub fn config() -> Config {
    Config {
        activation_deadline: Duration::from_secs(20),
        ..Config::default()
    }
}

pub fn runtime(world: &World) -> Runtime {
    world
        .runtime(Arc::new(NoHooks), config())
        .expect("open runtime")
}

pub fn runtime_with(world: &World, hooks: Arc<dyn Hooks>) -> Runtime {
    world.runtime(hooks, config()).expect("open runtime")
}

pub fn admit(rt: &Runtime, world: &World, script: &str) -> String {
    rt.admit(&world.spec(script))
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

pub fn admit_as(rt: &Runtime, world: &World, script: &str, trigger_id: &str) -> String {
    let mut spec = world.spec(script);
    spec.trigger_id = trigger_id.to_owned();
    rt.admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

/// Drives to a terminal state or `needs_reconcile`, completing long calls
/// whenever the run suspends.
pub fn finish(rt: &Runtime, world: &World, run_id: &str) -> Run {
    let run = drive(rt, &world.mock, run_id, Duration::from_secs(60)).expect("drive");
    rt.quiesce();
    run
}

pub fn result(run: &Run) -> serde_json::Value {
    match &run.result {
        Some(r) => serde_json::from_str(r).expect("result is JSON"),
        None => panic!("run has no result: {run:#?}"),
    }
}

/// Runs a SQL statement against the store directly, standing in for a
/// store written by something else.
pub fn sql(rt: &Runtime, statement: &str) {
    rt.store()
        .write(|tx| Ok(tx.execute_batch(statement)?))
        .expect("sql");
}

pub fn query_i64(rt: &Runtime, statement: &str) -> i64 {
    rt.store()
        .read(|c| Ok(c.query_row(statement, [], |r| r.get::<_, i64>(0))?))
        .expect("query")
}
