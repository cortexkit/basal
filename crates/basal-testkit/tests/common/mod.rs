//! Shared helpers for the journal and runtime tests.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use basal_core::channel::{ChannelError, WorkerChannel, WorkerReceiver};
use basal_core::{Config, Hooks, NoHooks, Run, Runtime};
use basal_proto::{ParentMessage, Welcome, WorkerMessage};
use basal_testkit::harness::{World, drive, test_manifest};

/// Observe protocol frames at their channel boundary without adding runtime
/// crash points for worker-message timing races.
pub struct ObserveWorker<'a> {
    inner: &'a mut dyn WorkerChannel,
    welcome: Box<dyn Fn() + Send + Sync + 'a>,
    sent: Box<dyn Fn(&ParentMessage) + Send + Sync + 'a>,
    received: Arc<dyn Fn(&WorkerMessage) + Send + Sync>,
}

impl<'a> ObserveWorker<'a> {
    pub fn new(inner: &'a mut dyn WorkerChannel) -> Self {
        Self {
            inner,
            welcome: Box::new(|| {}),
            sent: Box::new(|_| {}),
            received: Arc::new(|_| {}),
        }
    }
    pub fn welcoming(mut self, f: impl Fn() + Send + Sync + 'a) -> Self {
        self.welcome = Box::new(f);
        self
    }
    pub fn sending(mut self, f: impl Fn(&ParentMessage) + Send + Sync + 'a) -> Self {
        self.sent = Box::new(f);
        self
    }
    pub fn receiving(mut self, f: impl Fn(&WorkerMessage) + Send + Sync + 'static) -> Self {
        self.received = Arc::new(f);
        self
    }
}

impl WorkerChannel for ObserveWorker<'_> {
    fn welcome(&self) -> &Welcome {
        (self.welcome)();
        self.inner.welcome()
    }
    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        self.inner.send(message)?;
        (self.sent)(message);
        Ok(())
    }
    fn recv(&mut self, timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        let message = self.inner.recv(timeout)?;
        (self.received)(&message);
        Ok(message)
    }
    fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
        Box::new(ObserveReceiver {
            inner: self.inner.receiver(),
            received: self.received.clone(),
        })
    }
    fn kill(&mut self) {
        self.inner.kill();
    }
}

struct ObserveReceiver {
    inner: Box<dyn WorkerReceiver>,
    received: Arc<dyn Fn(&WorkerMessage) + Send + Sync>,
}
impl WorkerReceiver for ObserveReceiver {
    fn recv(&mut self) -> Result<WorkerMessage, ChannelError> {
        let message = self.inner.recv()?;
        (self.received)(&message);
        Ok(message)
    }
}

pub fn config() -> Config {
    Config {
        // Healthy activations need headroom when cut runs share a loaded host.
        // Short activation budgets belong only to deliberately stalled fixtures.
        activation_deadline: Duration::from_secs(60),
        install_gate: basal_core::InstallGate::Off,
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
    rt.admit(&world.spec(rt, script).expect("approve"))
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

pub fn admit_trigger(rt: &Runtime, world: &World, script: &str, trigger: &str) -> String {
    let mut spec = world.spec(rt, script).expect("approve");
    spec.trigger_id = trigger.to_owned();
    rt.admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned()
}

/// Admits `script` as a flow of its own, named after `trigger_id`, so
/// tests that drive several runs side by side are not serialised by one
/// flow's concurrency slot.
pub fn admit_as(rt: &Runtime, world: &World, script: &str, trigger_id: &str) -> String {
    let mut manifest = test_manifest();
    manifest["id"] = serde_json::json!(format!("flow-{trigger_id}"));
    let mut spec = world.spec_with(rt, script, &manifest).expect("approve");
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
