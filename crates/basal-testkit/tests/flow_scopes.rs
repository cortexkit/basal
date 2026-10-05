//! Exercise scoped provider calls and journaled retry delays with the real
//! confined JavaScript worker. An injectable clock advances retries and expiry
//! without sleeping.
use basal_core::channel::WorkerChannel;
use basal_core::{Clock, Config, InstallGate, InstallRequest, NoHooks, RunState, Runtime, Store};
use basal_host::core_host::CoreHost;
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::flow_scope::{FlowScope, RegisteredScope, ScopedRoutes};
use basal_host::routing::{ModuleOpsHost, RoutingHost};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError, management_body, tool_body};
use basal_host::{CallRequest, Dispatched, Host};
use basal_proto::{CallKind, JsonText, Settlement};
use basal_testkit::harness::{World, test_manifest, wait_until};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
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
        self.calls
            .lock()
            .unwrap()
            .push((route, module.into(), body.clone()));
        if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
            return Err(refusal);
        }
        Ok(body
            .get("params")
            .or_else(|| body.get("arguments"))
            .cloned()
            .unwrap_or(body))
    }
    fn route(&self, flow: &str, target: &RouteTarget) -> Result<Option<u64>, WireError> {
        self.routes.lock().unwrap().route(flow, target, |scope| {
            let mut opens = self.opens.lock().unwrap();
            opens.push(scope.clone());
            if let Some(refusal) = self.open_refusals.lock().unwrap().pop_front() {
                return Err(refusal);
            }
            Ok(opens.len() as u64)
        })
    }
}
impl Transport for Wire {
    fn provider_ready(&self, flow: &str, module: &str, action: &str) -> Result<(), FlowRefusal> {
        self.routes.lock().unwrap().ready(flow, module, action)
    }
    fn configure_flow(&self, flow: &str, owned: bool, scope: Option<RegisteredScope>) {
        self.routes.lock().unwrap().configure(flow, owned, scope);
    }
    fn catalog(&self) -> Result<Value, WireError> {
        let mut modules = vec![
            json!({"module_id":"mock","roles":[{"role":"management_surface","operations":[{"name":"send","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],"control_ops":[]}),
            json!({"module_id":"plexus","roles":[{"role":"management_surface","operations":[{"name":"pr.comment","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],"control_ops":[]}),
        ];
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
            self.calls
                .lock()
                .unwrap()
                .push((None, module.into(), management_body(op, params)));
            return Ok(value);
        }
        if op == "sink.digest" {
            self.calls
                .lock()
                .unwrap()
                .push((None, module.into(), management_body(op, params)));
            if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
                return Err(refusal);
            }
            return Ok(json!({"disposition":"stored","fire_id":"wf-test","replayed":false}));
        }
        self.invoke(None, module, management_body(op, params))
    }
    fn tool(&self, module: &str, name: &str, args: Value, key: &str) -> Result<Value, WireError> {
        self.invoke(None, module, tool_body(name, args, key)?)
    }
    fn management_for_flow(
        &self,
        flow: &str,
        module: &str,
        op: &str,
        params: Value,
    ) -> Result<Value, WireError> {
        let route = self.route(
            flow,
            &RouteTarget::ManagementSurface {
                module_id: module.into(),
            },
        )?;
        self.invoke(route, module, management_body(op, params))
    }
    fn tool_for_flow(
        &self,
        flow: &str,
        module: &str,
        name: &str,
        args: Value,
        key: &str,
    ) -> Result<Value, WireError> {
        let route = self.route(
            flow,
            &RouteTarget::ToolProvider {
                module_id: module.into(),
            },
        )?;
        self.invoke(route, module, tool_body(name, args, key)?)
    }
}

fn scope(epoch: u64) -> RegisteredScope {
    RegisteredScope {
        selector: FlowScope {
            owner: Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            scope_ref: "flow:flow-test".into(),
            epoch,
        },
        targets: ["mock".into()].into_iter().collect(),
    }
}
struct Fixture {
    _world: World,
    wire: Arc<Wire>,
    clock: Clock,
    rt: Runtime,
}
// Healthy scoped calls keep the production activation budget under host load.
const ACTIVATION_WAIT: Duration = Duration::from_secs(60);
// Only the fixture with a deliberately withheld provider reply uses this limit.
const STALLED_ACTIVATION_WAIT: Duration = Duration::from_secs(3);

impl Fixture {
    fn new(tag: &str, script: &str, registered: bool, owned: bool) -> Self {
        Self::with_activation_deadline(tag, script, registered, owned, ACTIVATION_WAIT)
    }

    fn with_activation_deadline(
        tag: &str,
        script: &str,
        registered: bool,
        owned: bool,
        activation_deadline: Duration,
    ) -> Self {
        let world = World::new(tag);
        let wire = Arc::new(Wire::default());
        if registered {
            *wire.scope.lock().unwrap() = Some(scope(1));
        }
        let clock = Clock::manual(1_000);
        let catalog = Arc::new(SubcCatalog::new(wire.clone()));
        let host = Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(wire.clone(), catalog)),
            Arc::new(CoreHost::new(wire.clone())),
            Arc::new(world.mock.clone()),
        ));
        let rt = Runtime::new(
            Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
            host,
            Arc::new(world.catalog.clone()),
            Arc::new(NoHooks),
            Some(world.source.clone()),
            Config {
                activation_deadline,
                install_gate: InstallGate::Core,
                clock: clock.clone(),
                retry_backoff: Duration::from_millis(100),
                auto_resume: false,
                ..Config::default()
            },
        );
        let mut manifest = test_manifest();
        manifest.as_object_mut().unwrap().remove("llm");
        manifest["ops"] = json!([{"module":"mock","op":"send"}]);
        let installed = rt
            .install(&InstallRequest {
                script: script.into(),
                manifest: manifest.to_string(),
                author: if owned { "ALF" } else { "operator" }.into(),
                loop_override: false,
            })
            .unwrap();
        rt.approve("flow-test", 1, &installed.code_hash, "approval")
            .unwrap();
        *wire.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
        Self {
            _world: world,
            wire,
            clock,
            rt,
        }
    }
    fn admit(&self, trigger: &str) -> String {
        self.rt
            .admit_trigger("flow-test", trigger, JsonText::null())
            .unwrap()
            .run_id()
            .unwrap()
            .into()
    }
    fn activate(&self, run: &str) {
        self.rt.resume(run).unwrap();
        self.rt.quiesce();
    }
    fn defer(&self, reason: RefusalReason) -> String {
        let queue = if matches!(
            reason,
            RefusalReason::ConsentUnavailable
                | RefusalReason::AgentRetired
                | RefusalReason::ResourceBusy
        ) {
            &self.wire.refusals
        } else {
            &self.wire.open_refusals
        };
        queue
            .lock()
            .unwrap()
            .push_back(basal_host::transport::refusal(
                subc_protocol::ErrorBody::new(reason.as_str(), "ignored"),
            ));
        let run = self.admit("one");
        self.activate(&run);
        run
    }
    fn retry_at(&self, run: &str) -> i64 {
        self.rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT retry_not_before FROM journal WHERE run_id = ?1",
                    [run],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }
    fn sends(&self) -> usize {
        self.wire
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, v)| v["method"] == "send")
            .count()
    }
}
const SCRIPT: &str = "return await ops.call('mock','send',{value:7});";

#[test]
fn only_the_uncaught_readiness_rejection_object_is_exempt_from_failures() {
    for script in [
        "Date.now(); return await ops.call('mock','send',{});",
        // A later delivered call must not replace the originating position.
        "Date.now(); let rejection; try { await ops.call('mock','send',{}); } catch (e) { rejection=e; } Date.now(); rejection.name='Changed'; rejection.data=null; throw rejection;",
    ] {
        let uncaught = Fixture::new("uncaught-readiness", script, false, true);
        let run = uncaught.admit("one");
        uncaught.activate(&run);
        assert_eq!(
            uncaught.rt.run(&run).unwrap().error_kind.as_deref(),
            Some("readiness_rejection")
        );
        assert_eq!(
            uncaught.rt.flow_health().unwrap()[0].consecutive_failures,
            0
        );
        assert_eq!(uncaught.rt.calls(&run).unwrap()[1].position, 1);
    }
    for script in [
        "try { await ops.call('mock','send',{}); } catch (_) {} throw new Error('unrelated');",
        "try { await ops.call('mock','send',{}); } catch (e) { const fake = new Error(e.message); fake.name=e.name; fake.data=e.data; throw fake; }",
        "try { await ops.call('mock','send',{}); } catch (_) {} throw 'unrelated';",
        "try { await ops.call('mock','send',{}); } catch (_) {} throw null;",
    ] {
        let caught = Fixture::new("caught-then-throw", script, false, true);
        let run = caught.admit("one");
        caught.activate(&run);
        assert_eq!(
            caught.rt.run(&run).unwrap().error_kind.as_deref(),
            Some("script")
        );
        assert_eq!(caught.rt.flow_health().unwrap()[0].consecutive_failures, 1);
    }

    // Both rejections are genuine host errors. Only the uncaught one's row
    // determines whether the failure is a readiness problem.
    let other_host = Fixture::new(
        "caught-readiness-then-host-rejection",
        "try { await ops.call('mock','send',{}); } catch (_) {} return await sink.digest('ALF',{title:'denied'});",
        false,
        true,
    );
    other_host
        .wire
        .refusals
        .lock()
        .unwrap()
        .push_back(basal_host::transport::refusal(
            subc_protocol::ErrorBody::new("denied", "denied"),
        ));
    let run = other_host.admit("one");
    other_host.activate(&run);
    assert_eq!(
        other_host.rt.run(&run).unwrap().error_kind.as_deref(),
        Some("script")
    );
    assert_eq!(
        other_host.rt.flow_health().unwrap()[0].consecutive_failures,
        1
    );
    let calls = other_host.rt.calls(&run).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| call.outcome.as_ref().unwrap().settlement == Settlement::Rejected)
    );
}

#[test]
fn module_ops_open_under_core_selector_and_reuse_across_runs() {
    let f = Fixture::new("scope-reuse", SCRIPT, true, true);
    for trigger in ["first", "second"] {
        let run = f.admit(trigger);
        f.activate(&run);
        assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded);
    }
    assert_eq!(*f.wire.opens.lock().unwrap(), vec![scope(1).selector]);
    assert_eq!(f.sends(), 2);
    assert!(
        f.wire
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, v)| v["method"] == "send")
            .all(|(route, _, _)| *route == Some(1))
    );
}

#[test]
fn new_epoch_or_ref_drops_the_old_route() {
    let f = Fixture::new("scope-epoch", SCRIPT, true, true);
    for (n, scope) in [
        (1, scope(1)),
        (2, scope(2)),
        (3, {
            let mut s = scope(2);
            s.selector.scope_ref = "replacement".into();
            s
        }),
    ] {
        *f.wire.scope.lock().unwrap() = Some(scope);
        let run = f.admit(&n.to_string());
        f.activate(&run);
    }
    assert_eq!(f.wire.opens.lock().unwrap().len(), 3);
}

#[test]
fn all_flows_without_scope_reject_unsent_without_deferring() {
    for owned in [true, false] {
        let f = Fixture::new("no-scope", SCRIPT, false, owned);
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
        assert_eq!(f.sends(), 0);
        assert!(f.wire.opens.lock().unwrap().is_empty());
        let call = &f.rt.calls(&run).unwrap()[0];
        assert_ne!(call.dispatch, basal_core::DispatchState::Deferred);
        let error: Value =
            serde_json::from_str(call.outcome.as_ref().unwrap().value.as_str()).unwrap();
        assert_eq!(error["code"], "no_flow_scope");
        assert_eq!(error["module"], "mock");
        let health = &f.rt.flow_health().unwrap()[0];
        assert_eq!(health.consecutive_failures, 0);
        assert!(
            health
                .waiting_reason
                .as_ref()
                .unwrap()
                .contains("waiting for core")
        );
        let counts: (i64, i64) =
            f.rt.store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT SUM(runs),SUM(dispatches) FROM rate_windows",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .unwrap();
        assert_eq!(counts, (1, 0));
    }
}

fn assert_deferred(reason: RefusalReason) {
    let f = Fixture::new(reason.as_str(), SCRIPT, true, true);
    let run = f.defer(reason);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Suspended);
    assert_eq!(
        f.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    assert_eq!(f.retry_at(&run), 1100);
    let health = &f.rt.flow_health().unwrap()[0];
    assert_eq!(health.consecutive_failures, 0);
    assert!(!health.auto_disabled);
    let detail = health.waiting_reason.as_ref().unwrap();
    assert!(detail.contains(reason.as_str()) && detail.contains("mock") && detail.contains("send"));
    let before = f.rt.calls(&run).unwrap()[0].clone();
    f.clock.set(1100);
    f.activate(&run);
    let after = f.rt.calls(&run).unwrap()[0].clone();
    assert_eq!(after.position, before.position);
    assert_eq!(after.idempotency_key, before.idempotency_key);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(after.attempts, 2);
}
macro_rules! mapping {
    ($name:ident,$reason:ident) => {
        #[test]
        fn $name() {
            assert_deferred(RefusalReason::$reason);
        }
    };
}
mapping!(scope_not_carrier_is_deferred_unsent, ScopeNotCarrier);
mapping!(scope_ended_is_deferred_unsent, ScopeEnded);
mapping!(scope_not_live_is_deferred_unsent, ScopeNotLive);
mapping!(scope_epoch_required_is_deferred_unsent, ScopeEpochRequired);
mapping!(scope_not_synced_is_deferred_unsent, ScopeNotSynced);
mapping!(scope_changed_is_deferred_unsent, ScopeChanged);
mapping!(scope_unsupported_is_deferred_unsent, ScopeUnsupported);
#[test]
fn daemon_target_flow_unsupported_rejects_without_dispatch_or_failure_count() {
    let f = Fixture::new("target-unsupported", SCRIPT, true, true);
    let run = f.defer(RefusalReason::TargetFlowUnsupported);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    assert_eq!(f.sends(), 0);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    assert_ne!(
        f.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    let dispatches: i64 = f
        .rt
        .store()
        .read(|c| Ok(c.query_row("SELECT SUM(dispatches) FROM rate_windows", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(dispatches, 0);
}

#[test]
fn flow_scope_required_from_multiple_targets_fails_unsent_without_retry() {
    for module in ["mock", "plexus"] {
        let f = Fixture::new("scope-required", SCRIPT, true, true);
        let mut manifest = test_manifest();
        manifest.as_object_mut().unwrap().remove("llm");
        manifest["version"] = json!(2);
        manifest["ops"] =
            json!([{"module":module,"op":if module=="mock" {"send"} else {"pr.comment"}}]);
        let op = if module == "mock" {
            "send"
        } else {
            "pr.comment"
        };
        // The provider code is authoritative even when its optional detail
        // differs between providers; prose and transmission hints are not parsed.
        let body = subc_protocol::ErrorBody::new("flow_scope_required", "no scope")
            .with_detail(json!({"transmission":"not_sent"}));
        f.wire
            .open_refusals
            .lock()
            .unwrap()
            .push_back(basal_host::transport::refusal(body));
        if module == "plexus" {
            // The transport's catalog resolves both test targets as ordinary
            // mutating operations; each is approved independently.
            let installed =
                f.rt.install(&InstallRequest {
                    script: format!("return await ops.call('{module}','{op}',{{}});"),
                    manifest: manifest.to_string(),
                    author: "ALF".into(),
                    loop_override: true,
                })
                .unwrap();
            f.rt.approve("flow-test", 2, &installed.code_hash, "approval-2")
                .unwrap();
            *f.wire.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
            f.wire
                .scope
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .targets
                .insert(module.into());
        }
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
        let call = &f.rt.calls(&run).unwrap()[0];
        assert_ne!(call.dispatch, basal_core::DispatchState::Deferred);
        assert_eq!(call.attempts, 1);
        let value: Value =
            serde_json::from_str(call.outcome.as_ref().unwrap().value.as_str()).unwrap();
        assert_eq!(value["code"], "flow_scope_required");
        assert_eq!(value["module"], module);
        assert_eq!(f.sends(), 0);
    }
}

#[test]
fn resource_busy_wait_uses_relative_hint_and_counts_against_expiry() {
    let f = Fixture::new("busy-expiry", SCRIPT, true, true);
    let mut refusal = FlowRefusal::new(RefusalReason::ResourceBusy, "mock", "send");
    refusal.retry_after_ms = Some(250);
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::Typed(refusal));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(f.retry_at(&run), 1250);
    f.clock.set(1249);
    assert!(f.rt.startable().unwrap().is_empty());
    assert_eq!(f.sends(), 1);
    let deadline = f.rt.run(&run).unwrap().deadline_at.unwrap();
    f.clock.set(deadline);
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    assert_eq!(
        f.rt.run(&run).unwrap().error_kind.as_deref(),
        Some("deadline")
    );
    assert_eq!(f.sends(), 1);
    assert!(
        f.rt.store()
            .read(|c| basal_core::journal::unsettled_positions(c, &run))
            .unwrap()
            .is_empty()
    );
    let code: String =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT json_extract(value,'$.code') FROM mailbox WHERE run_id=?1",
                    [&run],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(code, "resource_busy");
    f.rt.recover().unwrap();
    assert!(f.rt.startable().unwrap().is_empty());
    assert_eq!(f.sends(), 1);
}

#[test]
fn resource_busy_hint_is_capped_by_existing_run_deadline() {
    let f = Fixture::new("busy-hint-cap", SCRIPT, true, true);
    let mut refusal = FlowRefusal::new(RefusalReason::ResourceBusy, "mock", "send");
    refusal.retry_after_ms = Some(u64::MAX);
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::Typed(refusal));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(
        f.retry_at(&run),
        f.rt.run(&run).unwrap().deadline_at.unwrap()
    );
}

#[test]
fn scope_close_follows_new_install_selector_without_reopening_the_old_route() {
    let f = Fixture::new("scope-close-host", SCRIPT, true, true);
    let first = f.admit("first");
    f.activate(&first);
    f.wire.routes.lock().unwrap().closed(1, true);
    let second = f.admit("second");
    f.activate(&second);
    assert_eq!(f.rt.run(&second).unwrap().state, RunState::Suspended);
    assert_eq!(f.wire.opens.lock().unwrap().len(), 1);
    *f.wire.scope.lock().unwrap() = Some(scope(2));
    f.clock.set(1100);
    f.activate(&second);
    assert_eq!(f.rt.run(&second).unwrap().state, RunState::Succeeded);
    assert_eq!(f.wire.opens.lock().unwrap()[1], scope(2).selector);
}

#[test]
fn consent_unavailable_waits_for_explicit_grant_and_names_provider_action() {
    let f = Fixture::new("consent-wait", SCRIPT, true, true);
    let run = f.defer(RefusalReason::ConsentUnavailable);
    assert_eq!(f.retry_at(&run), i64::MAX);
    f.clock.set(5000);
    assert!(f.rt.startable().unwrap().is_empty());
    let health = f.rt.flow_health().unwrap();
    let reason = health[0].waiting_reason.as_ref().unwrap();
    assert!(
        reason.contains("consent_unavailable")
            && reason.contains("mock")
            && reason.contains("send")
    );
    assert!(f.rt.decision_cards().unwrap().is_empty());
}

#[test]
fn agent_retired_fails_and_core_disables_without_auto_disable_count() {
    let f = Fixture::new(
        "retired",
        "return await sink.digest('ALF',{title:'retired'});",
        true,
        true,
    );
    let run = f.defer(RefusalReason::AgentRetired);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    let flow = f.rt.flow("flow-test").unwrap().unwrap();
    assert!(!flow.enabled);
    assert_eq!(flow.disabled_by.as_deref(), Some("core"));
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    assert!(f.rt.decision_cards().unwrap().is_empty());
    assert_eq!(f.sends(), 0);
}

#[test]
fn unsupported_target_preflight_rejects_without_sending_but_run_activates() {
    let f = Fixture::new(
        "activation-cap",
        "return await ops.call('mock','send',{}).catch(e=>e.data.code);",
        true,
        true,
    );
    f.wire
        .scope
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .targets
        .clear();
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(f.rt.activation_count(&run).unwrap(), 1);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(
        f.rt.run(&run).unwrap().result.as_deref(),
        Some("\"target_flow_unsupported\"")
    );
    assert_eq!(f.sends(), 0);
    assert!(
        f.rt.flow_health().unwrap()[0]
            .waiting_reason
            .as_ref()
            .unwrap()
            .contains("mock")
    );
}

#[test]
fn carrier_core_payload_and_route_remain_unchanged() {
    let f = Fixture::new(
        "core-carrier",
        "return await sink.digest('ALF',{title:'title',body:'body'});",
        true,
        true,
    );
    let run = f.admit("one");
    f.activate(&run);
    let state = f.rt.run(&run).unwrap();
    assert_eq!(state.state, RunState::Succeeded, "{state:#?}");
    let calls = f.wire.calls.lock().unwrap();
    let (route, module, body) = calls
        .iter()
        .find(|(_, _, v)| v["method"] == "sink.digest")
        .unwrap();
    assert_eq!(*route, None);
    assert_eq!(module, "prefrontal-core");
    assert!(f.wire.opens.lock().unwrap().is_empty());
    let intent = f.rt.calls(&run).unwrap()[0].request.clone().unwrap();
    assert_eq!(
        body,
        &management_body(
            "sink.digest",
            serde_json::from_str(intent.as_str()).unwrap()
        )
    );
    assert!(body["params"].get("scope").is_none() && body["params"].get("flow").is_none());
}

#[test]
fn a_stalled_scope_activation_deadline_reaps_its_worker() {
    let f = Fixture::with_activation_deadline(
        "scope-activation-wait",
        SCRIPT,
        true,
        true,
        STALLED_ACTIVATION_WAIT,
    );
    let run = f.admit("one");
    let mut worker = f._world.source.spawn().expect("worker");
    // Only provider dispatch reads this queue. Holding it withholds a reply
    // without blocking the install gate or the runtime's deadline checks.
    let held_reply = f.wire.refusals.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let driver = {
        let rt = f.rt.clone();
        std::thread::spawn(move || {
            let end = rt.activate(&run, &mut worker);
            tx.send(()).expect("deadline observer still exists");
            (end, worker)
        })
    };
    // Release the provider even if the inner deadline was removed, then join
    // and reap before asserting the independently observed deadline result.
    let waited = rx.recv_timeout(Duration::from_secs(5));
    drop(held_reply);
    wait_until(
        Instant::now() + ACTIVATION_WAIT,
        "the released scope activation driver to exit",
        || driver.is_finished(),
    )
    .expect("driver cleanup");
    let (end, mut worker) = driver.join().expect("driver thread");
    let reaped = worker.exited(Duration::from_secs(3));
    worker.kill();
    f.rt.quiesce();
    waited.expect("the scope activation did not fail within five seconds");
    let end = end.expect("activation");
    assert!(
        matches!(&end, basal_core::ActivationEnd::Failed { kind, .. } if kind == "deadline"),
        "{end:?}"
    );
    assert!(reaped, "the activation deadline did not reap its worker");
}

#[test]
fn core_flow_ops_require_a_scope_and_never_change_script_arguments() {
    let wire = Arc::new(Wire::default());
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire.clone())));
    let args = json!({"agent":"ALF","limit":3});
    let request = CallRequest {
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        position: 0,
        kind: CallKind::Op {
            module: "prefrontal-core".into(),
            op: "board.read".into(),
        },
        args: JsonText::new(args.to_string()).unwrap(),
        idempotency_key: "key-a".into(),
        attempt: 1,
    };
    let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
        panic!("core op must fail loudly")
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert!(outcome.value.as_str().contains("basal_scope_bug"));
    assert!(wire.calls.lock().unwrap().is_empty());
    *wire.core_capable.lock().unwrap() = true;
    let mut core_scope = scope(1);
    core_scope.targets.insert("prefrontal-core".into());
    host.configure_flow("flow-a", true, Some(core_scope));
    host.dispatch(&request).unwrap();
    assert_eq!(
        wire.calls.lock().unwrap()[0],
        (
            Some(1),
            "prefrontal-core".into(),
            management_body("board.read", args)
        )
    );
    host.configure_flow("flow-a", false, None);
    assert!(matches!(
        host.dispatch(&request),
        Err(basal_host::TransportError::Refused(FlowRefusal {
            reason: RefusalReason::NoFlowScope,
            ..
        }))
    ));
    assert_eq!(wire.calls.lock().unwrap().len(), 1);
}
