//! Scoped opens and durable unsent waits through the production host adapters
//! and the real confined worker. Time advances only on the injectable clock.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use basal_core::{Clock, Config, InstallGate, InstallRequest, NoHooks, RunState, Runtime, Store};
use basal_host::core_host::CoreHost;
use basal_host::flow_scope::{FlowScope, RegisteredScope, ScopedRoutes};
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError, management_body, tool_body};
use basal_proto::JsonText;
use basal_testkit::harness::{World, test_manifest};
use serde_json::{Value, json};
use subc_protocol::{Principal, RouteTarget};

#[derive(Default)]
struct Wire {
    scope: Mutex<Option<RegisteredScope>>,
    hash: Mutex<String>,
    routes: Mutex<ScopedRoutes<u64>>,
    opens: Mutex<Vec<FlowScope>>,
    calls: Mutex<Vec<(Option<u64>, String, Value)>>,
    refusals: Mutex<VecDeque<WireError>>,
    open_refusals: Mutex<VecDeque<WireError>>,
    core_capable: Mutex<bool>,
}
impl Wire {
    fn invoke(&self, route: Option<u64>, module: &str, body: Value) -> Result<Value, WireError> {
        self.calls.lock().unwrap().push((route, module.into(), body.clone()));
        if let Some(refusal) = self.refusals.lock().unwrap().pop_front() { return Err(refusal); }
        Ok(body.get("params").or_else(|| body.get("arguments")).cloned().unwrap_or(body))
    }
    fn route(&self, flow: &str, target: &RouteTarget) -> Result<Option<u64>, WireError> {
        self.routes.lock().unwrap().route(flow, target, |scope| {
            let mut opens = self.opens.lock().unwrap();
            opens.push(scope.clone());
            if let Some(refusal) = self.open_refusals.lock().unwrap().pop_front() { return Err(refusal); }
            Ok(opens.len() as u64)
        })
    }
}
impl Transport for Wire {
    fn configure_flow(&self, flow: &str, owned: bool, scope: Option<RegisteredScope>) {
        self.routes.lock().unwrap().configure(flow, owned, scope.map(|s| s.selector));
    }
    fn catalog(&self) -> Result<Value, WireError> {
        let mut modules = vec![json!({"module_id":"mock","roles":[{"role":"management_surface","operations":[{"name":"send","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],"control_ops":[]})];
        modules.push(json!({"module_id":"prefrontal-core", "capabilities":{"provides":if *self.core_capable.lock().unwrap() {vec!["flow-scopes/v1"]} else {vec![]}}, "roles":[{"role":"management_surface","operations":[{"name":"board.read","kind":"query"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}));
        Ok(json!({"generation":1,"modules":modules,"subc_ops":[]}))
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        if op == "flow.install_status" {
            let mut value = json!({"state":"active","code_hash":*self.hash.lock().unwrap()});
            if let Some(scope) = self.scope.lock().unwrap().as_ref() {
                value["scope"] = serde_json::to_value(&scope.selector).unwrap();
                value["flow_scope_targets"] = json!(scope.targets);
            }
            self.calls.lock().unwrap().push((None, module.into(), management_body(op, params)));
            return Ok(value);
        }
        if op == "sink.digest" {
            self.calls.lock().unwrap().push((None, module.into(), management_body(op, params)));
            return Ok(json!({"disposition":"stored","fire_id":"wf-test","replayed":false}));
        }
        self.invoke(None, module, management_body(op, params))
    }
    fn tool(&self, module: &str, name: &str, args: Value, key: &str) -> Result<Value, WireError> {
        self.invoke(None, module, tool_body(name, args, key)?)
    }
    fn management_for_flow(&self, flow: &str, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        let route = self.route(flow, &RouteTarget::ManagementSurface {module_id:module.into()})?;
        self.invoke(route, module, management_body(op, params))
    }
    fn tool_for_flow(&self, flow: &str, module: &str, name: &str, args: Value, key: &str) -> Result<Value, WireError> {
        let route = self.route(flow, &RouteTarget::ToolProvider {module_id:module.into()})?;
        self.invoke(route, module, tool_body(name, args, key)?)
    }
}

fn scope(epoch: u64) -> RegisteredScope {
    RegisteredScope {selector:FlowScope {owner:Principal::Reserved {module_id:"prefrontal-core".into()},scope_ref:"flow:flow-test".into(),epoch},targets:["mock".into()].into_iter().collect()}
}
struct Fixture { _world: World, wire: Arc<Wire>, clock: Clock, rt: Runtime }
impl Fixture {
    fn new(tag: &str, script: &str, registered: bool, owned: bool) -> Self {
        let world = World::new(tag);
        let wire = Arc::new(Wire::default());
        if registered { *wire.scope.lock().unwrap() = Some(scope(1)); }
        let clock = Clock::manual(1_000);
        let catalog = Arc::new(SubcCatalog::new(wire.clone()));
        let host = Arc::new(RoutingHost::new(Arc::new(ModuleOpsHost::new(wire.clone(), catalog)), Arc::new(CoreHost::new(wire.clone())), Arc::new(world.mock.clone())));
        let rt = Runtime::new(Arc::new(Store::open(world.store_path(), world.durability).unwrap()), host,
            Arc::new(world.catalog.clone()), Arc::new(NoHooks), Some(world.source.clone()), Config {
                install_gate:InstallGate::Core, clock:clock.clone(), retry_backoff:Duration::from_millis(100), auto_resume:false, ..Config::default()
            });
        let mut manifest = test_manifest();
        manifest.as_object_mut().unwrap().remove("llm");
        manifest["ops"] = json!([{"module":"mock","op":"send"}]);
        let installed = rt.install(&InstallRequest {script:script.into(),manifest:manifest.to_string(),author:if owned {"ALF"} else {"operator"}.into(),loop_override:false}).unwrap();
        rt.approve("flow-test", 1, &installed.code_hash, "approval").unwrap();
        *wire.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
        Self {_world:world,wire,clock,rt}
    }
    fn admit(&self, trigger: &str) -> String { self.rt.admit_trigger("flow-test", trigger, JsonText::null()).unwrap().run_id().unwrap().into() }
    fn activate(&self, run: &str) { self.rt.resume(run).unwrap(); self.rt.quiesce(); }
    fn defer(&self, reason: RefusalReason) -> String {
        let queue = if matches!(reason,RefusalReason::ConsentUnavailable|RefusalReason::AgentRetired|RefusalReason::ResourceBusy) {&self.wire.refusals} else {&self.wire.open_refusals};
        queue.lock().unwrap().push_back(basal_host::transport::refusal(subc_protocol::ErrorBody::new(reason.as_str(),"ignored")));
        let run = self.admit("one"); self.activate(&run); run
    }
    fn retry_at(&self, run: &str) -> i64 { self.rt.store().read(|c| Ok(c.query_row("SELECT retry_not_before FROM journal WHERE run_id = ?1", [run], |r| r.get(0))?)).unwrap() }
    fn sends(&self) -> usize { self.wire.calls.lock().unwrap().iter().filter(|(_,_,v)| v["method"] == "send").count() }
}
const SCRIPT: &str = "return await ops.call('mock','send',{value:7});";

#[test]
fn module_ops_open_under_core_selector_and_reuse_across_runs() {
    let f = Fixture::new("scope-reuse", SCRIPT, true, true);
    for trigger in ["first", "second"] { let run = f.admit(trigger); f.activate(&run); assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded); }
    assert_eq!(*f.wire.opens.lock().unwrap(), vec![scope(1).selector]);
    assert_eq!(f.sends(),2);
    assert!(f.wire.calls.lock().unwrap().iter().filter(|(_,_,v)| v["method"] == "send").all(|(route,_,_)| *route == Some(1)));
}

#[test]
fn new_epoch_or_ref_drops_the_old_route() {
    let f = Fixture::new("scope-epoch", SCRIPT, true, true);
    for (n, scope) in [(1,scope(1)), (2,scope(2)), (3,{let mut s=scope(2);s.selector.scope_ref="replacement".into();s})] {
        *f.wire.scope.lock().unwrap()=Some(scope);
        let run=f.admit(&n.to_string()); f.activate(&run);
    }
    assert_eq!(f.wire.opens.lock().unwrap().len(),3);
}

#[test]
fn agent_without_scope_suspends_unsent_but_global_calls_as_before() {
    let f=Fixture::new("no-scope",SCRIPT,false,true); let run=f.admit("one"); f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Suspended);
    assert_eq!(f.sends(),0); assert!(f.wire.opens.lock().unwrap().is_empty());
    assert!(f.rt.flow_health().unwrap()[0].waiting_reason.as_ref().unwrap().contains("no_flow_scope"));
    let global=Fixture::new("global-scope",SCRIPT,false,false); let run=global.admit("one"); global.activate(&run);
    assert_eq!(global.rt.run(&run).unwrap().state,RunState::Succeeded);
    assert_eq!(global.sends(),1); assert!(global.wire.opens.lock().unwrap().is_empty());
}

fn assert_deferred(reason:RefusalReason) {
    let f=Fixture::new(reason.as_str(),SCRIPT,true,true); let run=f.defer(reason);
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Suspended);
    assert_eq!(f.rt.calls(&run).unwrap()[0].dispatch,basal_core::DispatchState::Deferred);
    assert_eq!(f.retry_at(&run),1100);
    let health=&f.rt.flow_health().unwrap()[0];
    assert_eq!(health.consecutive_failures,0); assert!(!health.auto_disabled);
    let detail=health.waiting_reason.as_ref().unwrap();
    assert!(detail.contains(reason.as_str()) && detail.contains("mock") && detail.contains("send"));
    let before=f.rt.calls(&run).unwrap()[0].clone();
    f.clock.set(1100); f.activate(&run);
    let after=f.rt.calls(&run).unwrap()[0].clone();
    assert_eq!(after.position,before.position); assert_eq!(after.idempotency_key,before.idempotency_key);
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Succeeded);
    assert_eq!(after.attempts,2);
}
macro_rules! mapping {
    ($name:ident,$reason:ident) => { #[test] fn $name() { assert_deferred(RefusalReason::$reason); } };
}
mapping!(scope_not_carrier_is_deferred_unsent,ScopeNotCarrier);
mapping!(scope_ended_is_deferred_unsent,ScopeEnded);
mapping!(scope_not_live_is_deferred_unsent,ScopeNotLive);
mapping!(scope_epoch_required_is_deferred_unsent,ScopeEpochRequired);
mapping!(scope_not_synced_is_deferred_unsent,ScopeNotSynced);
mapping!(scope_changed_is_deferred_unsent,ScopeChanged);
mapping!(target_flow_unsupported_is_deferred_without_auto_disable,TargetFlowUnsupported);

#[test]
fn recovery_does_not_resend_early_and_preserves_position_and_key() {
    let f=Fixture::new("scope-recovery",SCRIPT,true,true); let run=f.defer(RefusalReason::ScopeNotSynced);
    let before=f.rt.calls(&run).unwrap()[0].clone();
    f.rt.recover().unwrap();
    assert!(f.rt.startable().unwrap().is_empty());
    // Simulate a worker cut while running: recovery requeues it, but its
    // already journaled refusal must still suspend rather than send early.
    f.rt.store().write(|tx| {tx.execute("UPDATE runs SET state='pending',awaited=NULL WHERE run_id=?1",[&run])?;Ok(())}).unwrap();
    f.activate(&run); assert_eq!(f.sends(),0);
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Suspended);
    assert_eq!(f.rt.calls(&run).unwrap()[0].attempts,1);
    f.clock.set(1100); f.activate(&run);
    let after=f.rt.calls(&run).unwrap()[0].clone();
    assert_eq!(after.position,before.position); assert_eq!(after.idempotency_key,before.idempotency_key);
    assert_eq!(after.attempts,2); assert_eq!(f.sends(),1);
    let queries=f.wire.calls.lock().unwrap().iter().filter(|(_,_,v)|v["method"]=="flow.install_status").count();
    assert_eq!(queries,3);
}

#[test]
fn resource_busy_wait_uses_relative_hint_and_counts_against_expiry() {
    let f=Fixture::new("busy-expiry",SCRIPT,true,true);
    let mut refusal=FlowRefusal::new(RefusalReason::ResourceBusy,"mock","send"); refusal.retry_after_ms=Some(250);
    f.wire.refusals.lock().unwrap().push_back(WireError::Typed(refusal));
    let run=f.admit("one"); f.activate(&run); assert_eq!(f.retry_at(&run),1250);
    f.clock.set(1249); assert!(f.rt.startable().unwrap().is_empty()); assert_eq!(f.sends(),1);
    let deadline=f.rt.run(&run).unwrap().deadline_at.unwrap();
    f.clock.set(deadline); f.rt.enforce_deadlines().unwrap();
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Failed);
    assert_eq!(f.rt.run(&run).unwrap().error_kind.as_deref(),Some("deadline"));
    assert_eq!(f.sends(),1);
}

#[test]
fn consent_unavailable_waits_for_explicit_grant_and_names_provider_action() {
    let f=Fixture::new("consent-wait",SCRIPT,true,true); let run=f.defer(RefusalReason::ConsentUnavailable);
    assert_eq!(f.retry_at(&run),i64::MAX); f.clock.set(5000); assert!(f.rt.startable().unwrap().is_empty());
    let health=f.rt.flow_health().unwrap(); let reason=health[0].waiting_reason.as_ref().unwrap();
    assert!(reason.contains("consent_unavailable") && reason.contains("mock") && reason.contains("send"));
    assert!(f.rt.decision_cards().unwrap().is_empty());
}

#[test]
fn agent_retired_fails_and_core_disables_without_auto_disable_count() {
    let f=Fixture::new("retired",SCRIPT,true,true); let run=f.defer(RefusalReason::AgentRetired);
    assert_eq!(f.rt.run(&run).unwrap().state,RunState::Failed);
    let flow=f.rt.flow("flow-test").unwrap().unwrap(); assert!(!flow.enabled); assert_eq!(flow.disabled_by.as_deref(),Some("core"));
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures,0);
    assert!(f.rt.decision_cards().unwrap().is_empty()); assert_eq!(f.sends(),1);
}

#[test]
fn unsupported_manifest_target_does_not_activate() {
    let f=Fixture::new("activation-cap",SCRIPT,true,true);
    f.wire.scope.lock().unwrap().as_mut().unwrap().targets.clear();
    let run=f.admit("one"); f.activate(&run);
    assert_eq!(f.rt.activation_count(&run).unwrap(),0); assert_eq!(f.sends(),0);
    assert!(f.rt.flow_health().unwrap()[0].waiting_reason.as_ref().unwrap().contains("mock"));
}

#[test]
fn carrier_core_payload_and_route_remain_unchanged() {
    let f=Fixture::new("core-carrier","return await sink.digest('ALF',{title:'title',body:'body'});",true,true);
    let run=f.admit("one"); f.activate(&run);
    let calls=f.wire.calls.lock().unwrap();
    let (route,module,body)=calls.iter().find(|(_,_,v)|v["method"]=="sink.digest").unwrap();
    assert_eq!(*route,None); assert_eq!(module,"prefrontal-core"); assert!(f.wire.opens.lock().unwrap().is_empty());
    let intent=f.rt.calls(&run).unwrap()[0].request.clone().unwrap();
    assert_eq!(body,&management_body("sink.digest",serde_json::from_str(intent.as_str()).unwrap()));
    assert!(body["params"].get("scope").is_none() && body["params"].get("flow").is_none());
}
