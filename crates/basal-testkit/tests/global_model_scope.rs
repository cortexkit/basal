use basal_core::{Config, InstallGate, InstallRequest, NoHooks, RunState, Runtime, Store};
use basal_host::flow_scope::{FlowScope, RegisteredScope};
use basal_host::routing::RoutingHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, InstallStatus, TransportError,
};
use basal_proto::{CallKind, JsonText};
use basal_testkit::harness::{World, test_manifest};
use std::sync::{Arc, Mutex};
use subc_protocol::Principal;

#[derive(Default)]
struct Gate {
    hash: Mutex<String>,
    scope: Mutex<Option<RegisteredScope>>,
}
impl Host for Gate {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        unreachable!()
    }
    fn now_ms(&self) -> f64 {
        1000.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn install_status(&self, _: &str, _: u32) -> Result<InstallStatus, TransportError> {
        Ok(InstallStatus::Active {
            code_hash: self.hash.lock().unwrap().clone(),
            scope: self.scope.lock().unwrap().clone(),
        })
    }
}
/// A flow with no provider routes still gets the parent’s local primitives.
#[test]
fn local_primitives_need_no_provider_scope() {
    let world = World::new("local-primitives-scope");
    let gate = Arc::new(Gate::default());
    let host = Arc::new(RoutingHost::new(Arc::new(world.mock.clone()), gate.clone()));
    let rt = {
        let runtime = Runtime::new(
            Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
            host,
            Arc::new(world.catalog.clone()),
            Arc::new(NoHooks),
            Some(world.source.clone()),
            Config {
                install_gate: InstallGate::Core,
                ..Config::default()
            },
        )
        .expect("runtime startup");
        runtime.retain_detail_for_test().expect("rig inspection");
        runtime
    };
    let installed = rt
        .install(&InstallRequest {
            script: "await kv.set('n', 1); const n = await kv.get('n'); \
                     const t = Date.now(); const r = Math.random(); \
                     return {n, clock: typeof t, random: typeof r};"
                .into(),
            manifest: test_manifest().to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    rt.approve("flow-test", 1, &installed.code_hash, "approval")
        .unwrap();
    *gate.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
    // No target is registered for this flow.
    *gate.scope.lock().unwrap() = Some(RegisteredScope {
        selector: FlowScope {
            owner: Principal::Reserved {
                module_id: "prefrontal".into(),
            },
            scope_ref: "no-model-flow".into(),
            epoch: 1,
        },
        targets: Default::default(),
    });
    let run = rt
        .admit_trigger("flow-test", "local", JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    rt.resume(&run).unwrap();
    rt.quiesce();
    let ran = rt.run(&run).unwrap();
    assert_eq!(ran.state, RunState::Succeeded, "{:?}", ran.result);
    let result: serde_json::Value = serde_json::from_str(ran.result.as_ref().unwrap()).unwrap();
    assert_eq!(result["n"], 1);
    assert_eq!(result["clock"], "number");
    assert_eq!(result["random"], "number");
    assert_eq!(world.mock.total_sends(), 0);
}
