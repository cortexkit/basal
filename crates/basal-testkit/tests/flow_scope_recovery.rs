mod common;

use basal_core::{Clock, Config, InstallGate, NoHooks, RunState, Runtime, Store};
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::flow_scope::{FlowScope, RegisteredScope};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, InstallStatus,
    TransportError,
};
use basal_proto::{CallKind, JsonText};
use basal_testkit::harness::{World, test_manifest};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::Principal;

#[derive(Default)]
struct Provider {
    calls: Mutex<Vec<CallRequest>>,
    hash: Mutex<String>,
    queries: Mutex<usize>,
}
impl Host for Provider {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Mutation {
            honours_idempotency_keys: false,
        }
    }
    fn dispatch(&self, call: &CallRequest) -> Result<Dispatched, TransportError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(call.clone());
        if calls.len() == 1 {
            return Err(TransportError::Refused(FlowRefusal::new(
                RefusalReason::ScopeNotSynced,
                "mock",
                "send",
            )));
        }
        Ok(Dispatched::Completed(HostOutcome::fulfilled(
            JsonText::new("7").unwrap(),
        )))
    }
    fn now_ms(&self) -> f64 {
        1000.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn install_status(&self, _: &str, _: u32) -> Result<InstallStatus, TransportError> {
        *self.queries.lock().unwrap() += 1;
        Ok(InstallStatus::Active {
            code_hash: self.hash.lock().unwrap().clone(),
            scope: Some(RegisteredScope {
                selector: FlowScope {
                    owner: Principal::Reserved {
                        module_id: "prefrontal-core".into(),
                    },
                    scope_ref: "flow-test".into(),
                    epoch: 1,
                },
                targets: ["mock".into()].into_iter().collect(),
            }),
        })
    }
}
fn open(world: &World, provider: Arc<Provider>, clock: &Clock) -> Runtime {
    Runtime::new(
        Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
        provider,
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        Config {
            clock: clock.clone(),
            install_gate: InstallGate::Core,
            retry_backoff: Duration::from_millis(100),
            auto_resume: false,
            ..common::config()
        },
    )
}

#[test]
fn reopening_store_never_resends_early_and_keeps_position_and_call_key() {
    let world = World::new("scope-reopen-store");
    let clock = Clock::manual(1000);
    let provider = Arc::new(Provider::default());
    let rt = open(&world, provider.clone(), &clock);
    let mut manifest = test_manifest();
    manifest.as_object_mut().unwrap().remove("llm");
    manifest["ops"] = serde_json::json!([{"module":"mock","op":"send"}]);
    let spec = world
        .spec_with(
            &rt,
            "return await ops.call('mock','send',{n:7});",
            &manifest,
        )
        .unwrap();
    *provider.hash.lock().unwrap() = basal_core::ids::hex(
        &rt.store()
            .read(|c| basal_core::install::approved(c, "flow-test"))
            .unwrap()
            .unwrap()
            .code_hash,
    );
    let run = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    rt.resume(&run).unwrap();
    rt.quiesce();
    assert_eq!(rt.run(&run).unwrap().state, RunState::Suspended);
    let key = rt.calls(&run).unwrap()[0].idempotency_key.clone();
    // A cut can leave an activation pending as well as suspended. Neither
    // representation authorizes an early send when the issue row is deferred.
    rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE runs SET state='pending',awaited=NULL WHERE run_id=?1",
                [&run],
            )?;
            Ok(())
        })
        .unwrap();
    drop(rt);
    let rt = open(&world, provider.clone(), &clock);
    rt.recover().unwrap();
    rt.resume(&run).unwrap();
    rt.quiesce();
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    assert_eq!(rt.run(&run).unwrap().state, RunState::Suspended);
    clock.set(1100);
    rt.resume(&run).unwrap();
    rt.quiesce();
    assert_eq!(rt.run(&run).unwrap().state, RunState::Succeeded);
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].position, 0);
    assert_eq!(calls[1].idempotency_key, key);
    assert_eq!(rt.calls(&run).unwrap().len(), 1);
    assert_eq!(*provider.queries.lock().unwrap(), 3);
}
