use basal_core::{Config, InstallGate, NoHooks, RunState, Runtime, Store};
use basal_host::core_host::CoreHost;
use basal_host::mock::MockHost;
use basal_host::transport::{Transport, WireError};
use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
use basal_proto::CallKind;
use basal_testkit::harness::World;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct NoWire;
impl Transport for NoWire {
    fn catalog(&self) -> Result<Value, WireError> {
        unreachable!()
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        unreachable!()
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
}
struct EntropyHost {
    effects: MockHost,
    entropy: CoreHost,
    draws: AtomicUsize,
    replaying: AtomicBool,
}
impl Host for EntropyHost {
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.effects.classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.effects.dispatch(request)
    }
    fn now_ms(&self) -> f64 {
        self.effects.now_ms()
    }
    fn random(&self) -> f64 {
        assert!(
            !self.replaying.load(Ordering::SeqCst),
            "replay drew fresh entropy instead of using the journal"
        );
        self.draws.fetch_add(1, Ordering::SeqCst);
        self.entropy.random()
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.effects.attach(sink)
    }
}
fn open(world: &World, host: Arc<EntropyHost>) -> Runtime {
    Runtime::new(
        Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
        host,
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            install_gate: InstallGate::Off,
            auto_resume: false,
            ..Config::default()
        },
    )
}
#[test]
fn independent_production_draws_are_journaled_and_replayed_without_resampling() {
    let mut samples = Vec::new();
    for _ in 0..2 {
        let world = World::new("production-entropy");
        let host = Arc::new(EntropyHost {
            effects: world.mock.clone(),
            entropy: CoreHost::new(Arc::new(NoWire)),
            draws: AtomicUsize::new(0),
            replaying: AtomicBool::new(false),
        });
        let rt = open(&world, host.clone());
        let spec = world
            .spec(
                &rt,
                "const r = Math.random(); await ops.call('mock','long',{}); return r;",
            )
            .unwrap();
        let run = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
        rt.resume(&run).unwrap();
        rt.quiesce();
        assert_eq!(rt.run(&run).unwrap().state, RunState::Suspended);
        let sample = rt.calls(&run).unwrap()[0]
            .outcome
            .as_ref()
            .unwrap()
            .value
            .as_str()
            .parse::<f64>()
            .unwrap();
        assert!((0.0..1.0).contains(&sample));
        assert_eq!(host.draws.load(Ordering::SeqCst), 1);
        drop(rt);
        host.replaying.store(true, Ordering::SeqCst);
        let recovered = open(&world, host.clone());
        recovered.recover().unwrap();
        world.mock.complete_all();
        recovered.resume(&run).unwrap();
        recovered.quiesce();
        let finished = recovered.run(&run).unwrap();
        assert_eq!(finished.state, RunState::Succeeded);
        assert_eq!(finished.result.unwrap().parse::<f64>().unwrap(), sample);
        assert_eq!(host.draws.load(Ordering::SeqCst), 1);
        samples.push(sample);
    }
    assert_ne!(samples[0], samples[1]);
}
