mod common;

use basal_core::{Config, InstallGate, InstallRequest, NoHooks, RunState, Runtime, Store};
use basal_host::broca::wire::*;
use basal_host::broca::{BrocaError, BrocaHost, Route, Transport};
use basal_host::flow_refusal::FlowRefusal;
use basal_host::flow_scope::{FlowScope, RegisteredScope, ScopedRoutes};
use basal_host::routing::RoutingHost;
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, InstallStatus, TransportError,
};
use basal_proto::{CallKind, JsonText};
use basal_testkit::harness::{World, test_manifest};
use std::sync::{Arc, Mutex};
use subc_protocol::{Principal, RouteTarget};

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
#[derive(Default)]
struct Models {
    routes: Mutex<ScopedRoutes<u64>>,
    sends: Mutex<Vec<u64>>,
}
impl Transport for Models {
    fn configure_flow(&self, flow: &str, owned: bool, scope: Option<RegisteredScope>) {
        self.routes.lock().unwrap().configure(flow, owned, scope);
    }
    fn provider_ready(&self, flow: &str, action: &str) -> Result<(), FlowRefusal> {
        self.routes.lock().unwrap().ready(flow, "broca", action)
    }
    fn send(&self, route: &Route, _: &[u8]) -> Result<SendResult, BrocaError> {
        let handle = self
            .routes
            .lock()
            .unwrap()
            .route(
                route.flow_id.as_deref().unwrap(),
                &RouteTarget::ManagementSurface {
                    module_id: "broca".into(),
                },
                |_| Ok(17),
            )
            .map_err(|e| BrocaError::Wire(format!("{e:?}")))?
            .unwrap();
        self.sends.lock().unwrap().push(handle);
        Ok(SendResult::Finished {
            run_id: "model-run".into(),
            reason: RunFinishReason::Completed,
        })
    }
    fn watch(&self, _: &Route) -> Result<(), BrocaError> {
        Ok(())
    }
    fn result(&self, _: &Route, _: &RunResultParams) -> Result<RunResultResponse, BrocaError> {
        Ok(RunResultResponse {
            run_id: "model-run".into(),
            state: "completed".into(),
            reason: None,
            error: None,
            final_message: Some(FinalMessage {
                ordinal: 0,
                mid: "m".into(),
                text: "answer".into(),
            }),
        })
    }
    fn status(&self, _: &Route, _: &StatusParams) -> Result<RunStatusResponse, BrocaError> {
        Ok(RunStatusResponse::Completed {
            metadata: Terminal::default(),
        })
    }
}

#[test]
fn global_model_call_rejects_without_scope_then_uses_registered_scope() {
    let world = World::new("global-model-scope");
    let gate = Arc::new(Gate::default());
    let models = Arc::new(Models::default());
    let broca = Arc::new(BrocaHost::new(
        models.clone(),
        Arc::new(basal_host::broca::fake::MemoryStore::default()),
        "/".into(),
        "basal".into(),
        world.selector.clone(),
    ));
    let host = Arc::new(RoutingHost::new(
        Arc::new(world.mock.clone()),
        gate.clone(),
        broca,
    ));
    let rt = Runtime::new(
        Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
        host,
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            selector: world.selector.clone(),
            install_gate: InstallGate::Core,
            ..common::config()
        },
    );
    let installed = rt
        .install(&InstallRequest {
            script: "return await llm({prompt:'hello'}).catch(e=>e.data);".into(),
            manifest: test_manifest().to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    rt.approve("flow-test", 1, &installed.code_hash, "approval")
        .unwrap();
    *gate.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
    let first = rt
        .admit_trigger("flow-test", "first", JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    rt.resume(&first).unwrap();
    rt.quiesce();
    let result: serde_json::Value =
        serde_json::from_str(rt.run(&first).unwrap().result.as_ref().unwrap()).unwrap();
    assert_eq!(result["code"], "no_flow_scope");
    assert_eq!(result["module"], "broca");
    assert!(models.sends.lock().unwrap().is_empty());
    assert!(world.selector.requests.lock().unwrap().is_empty());
    assert_ne!(
        rt.calls(&first).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    *gate.scope.lock().unwrap() = Some(RegisteredScope {
        selector: FlowScope {
            owner: Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            scope_ref: "global-flow".into(),
            epoch: 1,
        },
        targets: ["broca".into()].into_iter().collect(),
    });
    let second = rt
        .admit_trigger("flow-test", "second", JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    rt.resume(&second).unwrap();
    rt.quiesce();
    assert_eq!(rt.run(&second).unwrap().state, RunState::Succeeded);
    assert_eq!(
        rt.run(&second).unwrap().result.as_deref(),
        Some("{\"text\":\"answer\"}")
    );
    assert_eq!(*models.sends.lock().unwrap(), vec![17]);
    assert_eq!(world.selector.requests.lock().unwrap().len(), 1);
    assert_eq!(world.selector.reports.lock().unwrap().len(), 1);
}
