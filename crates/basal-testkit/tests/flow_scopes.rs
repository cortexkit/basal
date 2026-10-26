//! Exercise scoped provider calls and journaled retry delays with the real
//! confined JavaScript worker. An injectable clock advances retries and expiry
//! without sleeping.
use basal_core::channel::WorkerChannel;
use basal_core::{Clock, Config, InstallGate, InstallRequest, NoHooks, RunState, Runtime, Store};
use basal_host::core_host::CoreHost;
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::flow_scope::{FlowScope, RegisteredScope, ScopedRoutes, flow_route_key};
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
    installs: Mutex<std::collections::HashMap<u32, (String, RegisteredScope)>>,
    stall: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    closed: Mutex<Vec<u64>>,
    retained: Mutex<Vec<String>>,
    scope: Mutex<Option<RegisteredScope>>,
    hash: Mutex<String>,
    routes: Mutex<ScopedRoutes<u64>>,
    opens: Mutex<Vec<FlowScope>>,
    calls: Mutex<Vec<(Option<u64>, String, Value)>>,
    refusals: Mutex<VecDeque<WireError>>,
    open_refusals: Mutex<VecDeque<WireError>>,
    core_capable: Mutex<bool>,
    target_scopes: Mutex<basal_host::target_scope::TargetScopes>,
    target_answers: Mutex<VecDeque<Result<Value, WireError>>>,
    target_asks: Mutex<Vec<Value>>,
    now: Mutex<i64>,
    keyed: Mutex<bool>,
}
impl Wire {
    fn invoke(&self, route: Option<u64>, module: &str, body: Value) -> Result<Value, WireError> {
        self.calls
            .lock()
            .unwrap()
            .push((route, module.into(), body.clone()));
        if module == "mock" {
            let stall = self.stall.lock().unwrap().take();
            if let Some((entered, release)) = stall {
                entered.send(()).unwrap();
                release.recv_timeout(ACTIVATION_WAIT).unwrap();
            }
        }
        if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
            if matches!(refusal, WireError::ScopeCut(_))
                && let Some(route) = route
            {
                self.routes.lock().unwrap().closed(route, true);
            }
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
    fn target_flow(
        &self,
        request: &CallRequest,
        call: &basal_host::target_scope::TargetCall,
    ) -> Result<String, WireError> {
        let mut cache = self.target_scopes.lock().unwrap();
        let result = cache.resolve(&request.flow_id, &request.run_id, call, *self.now.lock().unwrap(),
            |scope| self.routes.lock().unwrap().ended(&scope.route_key()),
            || self.management("prefrontal-core", "flow.target_scope", json!({"flow_id":request.flow_id,"version":call.version,"agent":call.agent,"expires_at_ms":call.expires_at_ms})),
        );
        for key in cache.take_retired() {
            self.configure_flow(&key, None);
        }
        let scope = result?;
        let key = scope.route_key();
        let targets = self
            .routes
            .lock()
            .unwrap()
            .targets(&request.route_key())
            .unwrap();
        self.configure_flow(
            &key,
            Some(RegisteredScope {
                selector: scope.selector(),
                targets,
            }),
        );
        Ok(key)
    }
    fn provider_ready(&self, flow: &str, module: &str, action: &str) -> Result<(), FlowRefusal> {
        self.routes.lock().unwrap().ready(flow, module, action)
    }
    fn configure_flow(&self, flow: &str, scope: Option<RegisteredScope>) {
        let dropped = self.routes.lock().unwrap().configure(flow, scope);
        self.closed.lock().unwrap().extend(dropped);
    }
    fn retain_flow_routes(&self, flow_id: &str, live: &std::collections::HashSet<String>) {
        self.retained.lock().unwrap().push(flow_id.into());
        let dropped = self
            .routes
            .lock()
            .unwrap()
            .retain_flow_routes_for(flow_id, live);
        self.closed.lock().unwrap().extend(dropped);
    }
    fn catalog(&self) -> Result<Value, WireError> {
        let mut modules = vec![
            json!({"module_id":"mock","roles":[{"role":"management_surface","operations":[{"name":"send","kind":"mutate"},{"name":"post","kind":"mutate"},{"name":"echo","kind":"query"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],"control_ops":[]}),
            json!({"module_id":"plexus","roles":[{"role":"management_surface","operations":[{"name":"pr.comment","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],"control_ops":[]}),
        ];
        if *self.keyed.lock().unwrap() {
            modules[0]["roles"][0]["operations"] =
                json!([{"name":"echo","kind":"query"},{"name":"post","kind":"mutate"}]);
            modules[0]["roles"].as_array_mut().unwrap().push(json!({"role":"tool_provider","tools":[{"name":"send","execution_mode":"mutating","schema":{}}],"identity_scope":[],"config_schema":{},"observability":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}));
        }
        modules.push(json!({"module_id":"prefrontal-core", "capabilities":{"provides":if *self.core_capable.lock().unwrap() {vec!["flow-scopes/v1"]} else {vec![]}}, "roles":[{"role":"management_surface","operations":[{"name":"board.read","kind":"query"},{"name":"work.unsettled","kind":"query"},{"name":"board.write","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}));
        Ok(json!({"generation":1,"modules":modules,"subc_ops":[]}))
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        if op == "flow.target_scope" {
            assert_eq!(module, "prefrontal-core");
            let mut asks = self.target_asks.lock().unwrap();
            asks.push(params.clone());
            if let Some(answer) = self.target_answers.lock().unwrap().pop_front() {
                return answer;
            }
            return Ok(
                json!({"ref":format!("target-{}",asks.len()),"epoch":1,"delegates":true,"expires_at_ms":params["expires_at_ms"].as_i64().unwrap()+600_000}),
            );
        }
        if op == "flow.install_status" {
            let mut value = json!({"state":"active","code_hash":*self.hash.lock().unwrap()});
            let install = self
                .installs
                .lock()
                .unwrap()
                .get(&(params["version"].as_u64().unwrap() as u32))
                .cloned();
            let scope = if let Some((hash, scope)) = install {
                value["code_hash"] = json!(hash);
                Some(scope)
            } else {
                self.scope.lock().unwrap().clone()
            };
            if let Some(scope) = scope {
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
    world: World,
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
        Self::with_hooks(
            tag,
            script,
            registered,
            owned,
            activation_deadline,
            Arc::new(NoHooks),
        )
    }
    fn with_hooks(
        tag: &str,
        script: &str,
        registered: bool,
        owned: bool,
        activation_deadline: Duration,
        hooks: Arc<dyn basal_core::hooks::Hooks>,
    ) -> Self {
        Self::with_host_options(
            tag,
            script,
            registered,
            owned,
            activation_deadline,
            hooks,
            false,
        )
    }
    fn with_host_options(
        tag: &str,
        script: &str,
        registered: bool,
        owned: bool,
        activation_deadline: Duration,
        hooks: Arc<dyn basal_core::hooks::Hooks>,
        keyed: bool,
    ) -> Self {
        let world = World::new(tag);
        let wire = Arc::new(Wire::default());
        *wire.keyed.lock().unwrap() = keyed;
        if registered {
            *wire.scope.lock().unwrap() = Some(scope(1));
        }
        let clock = Clock::manual(1_000);
        let catalog = Arc::new(SubcCatalog::new(wire.clone()));
        let host = Arc::new(RoutingHost::new(
            Arc::new(
                ModuleOpsHost::new(wire.clone(), catalog).with_idempotent_ops(if keyed {
                    [("mock".into(), "send".into())].into_iter().collect()
                } else {
                    Default::default()
                }),
            ),
            Arc::new(CoreHost::new(wire.clone())),
            Arc::new(world.mock.clone()),
        ));
        let rt = Runtime::new(
            Arc::new(Store::open(world.store_path(), world.durability).unwrap()),
            host,
            Arc::new(world.catalog.clone()),
            hooks,
            Some(world.source.clone()),
            Config {
                activation_deadline,
                install_gate: InstallGate::Core,
                clock: clock.clone(),
                deferral_retry: Duration::from_millis(100),
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
            world,
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
    fn restart(self) -> Self {
        let Self {
            world,
            wire,
            clock,
            rt,
        } = self;
        drop(rt);
        world.mock.detach();
        let catalog = Arc::new(SubcCatalog::new(wire.clone()));
        let keyed = *wire.keyed.lock().unwrap();
        let host = Arc::new(RoutingHost::new(
            Arc::new(
                ModuleOpsHost::new(wire.clone(), catalog).with_idempotent_ops(if keyed {
                    [("mock".into(), "send".into())].into_iter().collect()
                } else {
                    Default::default()
                }),
            ),
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
                install_gate: InstallGate::Core,
                clock: clock.clone(),
                auto_resume: false,
                ..Config::default()
            },
        );
        Self {
            world,
            wire,
            clock,
            rt,
        }
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
            .filter(|(_, _, v)| v["method"] == "send" || v["name"] == "send")
            .count()
    }
}
const SCRIPT: &str = "return await ops.call('mock','send',{value:7});";

#[derive(Default)]
struct LiveCallRetirementProbe {
    context: Mutex<Option<(Runtime, Arc<Wire>)>>,
    observed: std::sync::atomic::AtomicBool,
}
impl basal_core::hooks::Hooks for LiveCallRetirementProbe {
    fn at(&self, run: &str, boundary: &basal_core::hooks::Boundary) -> basal_core::hooks::Step {
        if *boundary == basal_core::hooks::Boundary::EndingReceived {
            let (rt, wire) = self.context.lock().unwrap().clone().unwrap();
            // Drain the dispatch threads before ending the run, so the counter
            // observes every unregister while the run is still live.
            rt.quiesce();
            assert_eq!(rt.run(run).unwrap().state, RunState::Running);
            assert_eq!(
                wire.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, module, body)| module == "mock" && body["method"] == "send")
                    .count(),
                3
            );
            assert!(
                wire.retained.lock().unwrap().is_empty(),
                "live calls must not run route retirement"
            );
            self.observed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        basal_core::hooks::Step::Continue
    }
}

#[test]
fn live_calls_do_not_retire_flow_routes_until_the_run_ends() {
    let probe = Arc::new(LiveCallRetirementProbe::default());
    let f = Fixture::with_hooks(
        "live-call-retirement",
        "for (let n=0;n<3;n++) await ops.call('mock','send',{n}); return 7;",
        true,
        true,
        ACTIVATION_WAIT,
        probe.clone(),
    );
    f.wire.retained.lock().unwrap().clear();
    *probe.context.lock().unwrap() = Some((f.rt.clone(), f.wire.clone()));
    let run = f.admit("several-calls");
    f.activate(&run);
    *probe.context.lock().unwrap() = None;
    assert!(probe.observed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(*f.wire.retained.lock().unwrap(), vec!["flow-test"]);
}

fn approve_second_version(f: &Fixture, shared: bool) {
    f.wire
        .installs
        .lock()
        .unwrap()
        .insert(1, (f.wire.hash.lock().unwrap().clone(), scope(1)));
    let mut manifest = test_manifest();
    manifest.as_object_mut().unwrap().remove("llm");
    manifest["version"] = json!(2);
    manifest["ops"] = json!([{"module":"mock","op":"send"}]);
    let installed =
        f.rt.install(&InstallRequest {
            script: SCRIPT.into(),
            manifest: manifest.to_string(),
            author: "ALF".into(),
            loop_override: false,
        })
        .unwrap();
    f.wire.installs.lock().unwrap().insert(
        2,
        (
            basal_core::ids::hex(&installed.code_hash),
            scope(if shared { 1 } else { 2 }),
        ),
    );
    f.rt.approve("flow-test", 2, &installed.code_hash, "approval-2")
        .unwrap();
}

#[test]
fn cancelled_v1_dispatch_keeps_its_route_after_v2_activates() {
    for shared in [false, true] {
        let f = Fixture::new("version-overlap", SCRIPT, true, true);
        let (entered, waiting) = mpsc::channel();
        let (release, stalled) = mpsc::channel();
        *f.wire.stall.lock().unwrap() = Some((entered, stalled));
        let first = f.admit("v1");
        let rt = f.rt.clone();
        let first_id = first.clone();
        let activation = std::thread::spawn(move || rt.resume(&first_id).unwrap());
        waiting.recv_timeout(ACTIVATION_WAIT).unwrap();
        assert_eq!(
            f.wire.opens.lock().unwrap().as_slice(),
            &[scope(1).selector]
        );
        approve_second_version(&f, shared);
        f.wire.configure_flow(
            &flow_route_key("flow-test", 2),
            Some(scope(if shared { 1 } else { 2 })),
        );
        assert!(f.wire.closed.lock().unwrap().is_empty());
        f.rt.cancel(&first, "operator").unwrap();
        let second = f.admit("v2");
        f.rt.resume(&second).unwrap();
        assert_eq!(f.rt.run(&second).unwrap().state, RunState::Succeeded);
        assert_eq!(f.rt.run(&first).unwrap().state, RunState::Cancelled);
        assert!(f.wire.closed.lock().unwrap().is_empty());
        let target = RouteTarget::ManagementSurface {
            module_id: "mock".into(),
        };
        assert_eq!(
            f.wire
                .route(&flow_route_key("flow-test", 1), &target)
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            f.wire
                .route(&flow_route_key("flow-test", 2), &target)
                .unwrap(),
            Some(if shared { 1 } else { 2 })
        );
        release.send(()).unwrap();
        activation.join().unwrap();
        f.rt.quiesce();
        assert!(matches!(
            f.wire.route(&flow_route_key("flow-test", 1), &target),
            Err(WireError::Typed(_))
        ));
        assert_eq!(
            *f.wire.closed.lock().unwrap(),
            if shared { vec![] } else { vec![1] }
        );
        f.rt.uninstall_flow("flow-test").unwrap();
        assert_eq!(
            *f.wire.closed.lock().unwrap(),
            if shared { vec![1] } else { vec![1, 2] }
        );
    }
}

#[test]
fn needs_reconcile_v1_keeps_its_route_when_v2_scope_registers() {
    let f = Fixture::new("version-reconcile", SCRIPT, true, true);
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::Unknown("reply lost".into()));
    let first = f.admit("v1");
    f.activate(&first);
    assert_eq!(f.rt.run(&first).unwrap().state, RunState::NeedsReconcile);
    approve_second_version(&f, false);
    f.wire
        .configure_flow(&flow_route_key("flow-test", 2), Some(scope(2)));
    let second = f.admit("v2");
    assert!(matches!(
        f.rt.resume(&second).unwrap(),
        basal_core::ActivationEnd::Waiting { .. }
    ));
    let target = RouteTarget::ManagementSurface {
        module_id: "mock".into(),
    };
    assert_eq!(
        f.wire
            .route(&flow_route_key("flow-test", 1), &target)
            .unwrap(),
        Some(1)
    );
    assert!(f.wire.closed.lock().unwrap().is_empty());
    f.rt.cancel(&first, "operator").unwrap();
    assert_eq!(*f.wire.closed.lock().unwrap(), vec![1]);
}

#[test]
fn instance_provider_uses_the_selector_core_returns_for_its_flow_id() {
    let f = Fixture::new("instance-scope", SCRIPT, true, true);
    let agent = "agent_47120287c700722b";
    f.world.catalog.add_named_agent(agent, "Renamable");
    let mut m = test_manifest();
    m["id"] = json!("dark-wake");
    m["sinks"][0]["agent"] = json!("$self");
    m["status"] = json!(["$self"]);
    m["facts"]["targets"] = json!(["$self"]);
    let registered = f.rt.register_package(SCRIPT, &m.to_string()).unwrap();
    let id = f.rt.ensure_instance("dark-wake", 1, agent, 1).unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut returned = scope(17);
    returned.selector.scope_ref = format!("flow:{id}");
    *f.wire.scope.lock().unwrap() = Some(returned.clone());
    *f.wire.hash.lock().unwrap() = registered["code_hash"].as_str().unwrap().to_owned();
    let run =
        f.rt.admit_trigger(&id, "instance", JsonText::null())
            .unwrap()
            .run_id()
            .unwrap()
            .to_owned();
    f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(*f.wire.opens.lock().unwrap(), vec![returned.selector]);
    let calls = f.wire.calls.lock().unwrap();
    assert!(calls.iter().any(|(route, _, body)| route.is_none()
        && body["method"] == "flow.install_status"
        && body["params"]["flow_id"] == id
        && body["params"]["version"] == 1));
    assert!(
        calls
            .iter()
            .any(|(route, _, body)| *route == Some(1) && body["method"] == "send")
    );
}

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
    // Route ids count opens, so a new epoch closes route 1 and a new
    // scope_ref closes route 2; the current route 3 stays open.
    assert_eq!(*f.wire.closed.lock().unwrap(), vec![1, 2]);
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
    let mut worker = f.world.source.spawn().expect("worker");
    // Only provider dispatch reads this queue. Holding it withholds a reply
    // without blocking the install gate or the runtime's deadline checks.
    let held_reply = f.wire.refusals.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let driver = {
        let rt = f.rt.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let end = rt.activate(&run, &mut worker);
            let took = started.elapsed();
            tx.send(()).expect("deadline observer still exists");
            (end, took, worker)
        })
    };
    // The provider reply is held forever, so the activation can only end by
    // its own STALLED_ACTIVATION_WAIT firing; the outcome check below proves it
    // was the deadline. This receive bound only stops a hang if the deadline
    // never fires, so it is generous: a short bound fails on a loaded machine
    // where the deadline fires late but correctly. Release the held provider
    // reply whatever the result, then join the driver and reap its worker
    // before asserting, so a failure never leaves a worker behind.
    let waited = rx.recv_timeout(ACTIVATION_WAIT);
    drop(held_reply);
    wait_until(
        Instant::now() + ACTIVATION_WAIT,
        "the released scope activation driver to exit",
        || driver.is_finished(),
    )
    .expect("driver cleanup");
    let (end, took, mut worker) = driver.join().expect("driver thread");
    let reaped = worker.exited(ACTIVATION_WAIT);
    worker.kill();
    f.rt.quiesce();
    // A deadline failure alone cannot say which deadline fired: a fixture that
    // kept the production activation deadline would also end this activation
    // with a deadline failure, just later. That deadline is measured from
    // inside `activate`, so such an activation can never end before it, while
    // the fixture's short deadline ends it long before. The bound is the same
    // generous one the hang check relies on, so a loaded machine that fires the
    // short deadline late still passes. It is checked first because a deadline
    // as long as the hang bound may lose the race to it, and the hang message
    // would then hide which deadline was configured.
    let production_deadline = Config::default().activation_deadline;
    assert!(
        took < production_deadline,
        "the stalled activation took {took:?}, at least the production activation deadline of {production_deadline:?}, so the fixture's own deadline did not end it"
    );
    waited.expect("the stalled scope activation never ended");
    let end = end.expect("activation");
    assert!(
        matches!(&end, basal_core::ActivationEnd::Failed { kind, .. } if kind == "deadline"),
        "{end:?}"
    );
    assert!(reaped, "the activation deadline did not reap its worker");
}

#[test]
fn other_core_flow_ops_require_a_scope_and_never_change_script_arguments() {
    let wire = Arc::new(Wire::default());
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire.clone())));
    let args = json!({"agent":"ALF","limit":3});
    let request = CallRequest {
        flow_version: None,
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        position: 0,
        kind: CallKind::Op {
            module: "prefrontal-core".into(),
            op: "board.write".into(),
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
    host.configure_flow("flow-a", Some(core_scope));
    host.dispatch(&request).unwrap();
    assert_eq!(
        wire.calls.lock().unwrap()[0],
        (
            Some(1),
            "prefrontal-core".into(),
            management_body("board.write", args)
        )
    );
    host.configure_flow("flow-a", None);
    assert!(matches!(
        host.dispatch(&request),
        Err(basal_host::TransportError::Refused(FlowRefusal {
            reason: RefusalReason::NoFlowScope,
            ..
        }))
    ));
    assert_eq!(wire.calls.lock().unwrap().len(), 1);
}

fn board_read_fixture(tag: &str, script: &str) -> Fixture {
    let f = Fixture::new(tag, script, false, false);
    f.world.catalog.set_flow_capable("prefrontal-core", false);
    f.world.catalog.set_op(
        "prefrontal-core",
        "board.read",
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    install_target_manifest(&f, |manifest| {
        manifest["ops"] = json!([{"module":"prefrontal-core","op":"board.read"}]);
        manifest["board_read"] = json!(["$audience"]);
    });
    f
}

#[test]
fn board_read_uses_basal_route_with_journaled_approved_stamp() {
    let f = board_read_fixture(
        "board-carrier",
        "return await ops.call('prefrontal-core', 'board.read', {limit: 3, cursor: 'agent_0'});",
    );
    let run = f.admit("board-carrier");
    f.activate(&run);
    let expected = json!({"limit":3,"cursor":"agent_0","stamp":{
        "flow_id":"flow-test","flow_version":2,"code_hash":*f.wire.hash.lock().unwrap(),
    }});
    assert_eq!(run_result(&f, &run), expected);
    let calls = f.wire.calls.lock().unwrap();
    let board_calls: Vec<_> = calls
        .iter()
        .filter(|(_, _, body)| body["method"] == "board.read")
        .collect();
    assert_eq!(board_calls.len(), 1);
    assert_eq!(
        *board_calls[0],
        (
            None,
            "prefrontal-core".into(),
            management_body("board.read", expected.clone())
        )
    );
    assert!(f.wire.opens.lock().unwrap().is_empty());
    assert!(f.wire.target_asks.lock().unwrap().is_empty());
    let (class, request, original): (String, String, String) =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT class, request, args FROM journal WHERE run_id=?1 AND position=0",
                    [&run],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap();
    assert_eq!(class, "query");
    assert_eq!(serde_json::from_str::<Value>(&request).unwrap(), expected);
    assert_eq!(
        serde_json::from_str::<Value>(&original).unwrap(),
        json!({"limit":3,"cursor":"agent_0"})
    );
}

#[test]
fn board_read_refuses_script_supplied_stamp_fields() {
    for (field, value) in [
        (
            "stamp",
            json!({"flow_id":"forged","flow_version":99,"code_hash":"forged"}),
        ),
        ("stamp", Value::Null),
        ("flow_id", json!("forged")),
        ("flow_version", json!(99)),
        ("code_hash", json!("forged")),
    ] {
        let args = json!({field: value});
        let f = board_read_fixture(
            "board-forged-stamp",
            &format!(
                "try {{ await ops.call('prefrontal-core', 'board.read', {args}); return 'sent'; }} catch (e) {{ return e.data; }}"
            ),
        );
        let run = f.admit("board-forged-stamp");
        f.activate(&run);
        let result = run_result(&f, &run);
        assert_eq!(result["code"], "invalid_arguments", "{field}: {result}");
        assert!(result["message"].as_str().unwrap().contains("stamp"));
        assert!(
            f.wire
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, body)| body["method"] != "board.read")
        );
        assert!(f.wire.opens.lock().unwrap().is_empty());
    }
}

#[test]
fn board_read_requires_object_arguments_and_own_audience_selector() {
    for call in [
        "ops.call('prefrontal-core', 'board.read', null)",
        "ops.call('prefrontal-core', 'board.read', [])",
        "ops.call('prefrontal-core', 'board.read', {limit: 1}, {for: 'Alice'})",
    ] {
        let f = board_read_fixture(
            "board-carrier-args",
            &format!("try {{ await {call}; return 'sent'; }} catch (e) {{ return e.data; }}"),
        );
        let run = f.admit("board-carrier-args");
        f.activate(&run);
        assert_eq!(run_result(&f, &run)["code"], "invalid_arguments", "{call}");
        assert!(
            f.wire
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, body)| body["method"] != "board.read")
        );
        assert!(f.wire.target_asks.lock().unwrap().is_empty());
    }
}

fn work_unsettled_fixture(tag: &str, script: &str) -> Fixture {
    let f = Fixture::new(tag, script, false, false);
    // Core does not advertise flow-scopes/v1: a carrier op must still install
    // and run, because it never uses a flow-scoped route.
    f.world.catalog.set_flow_capable("prefrontal-core", false);
    f.world.catalog.set_op(
        "prefrontal-core",
        "work.unsettled",
        basal_host::OpDecl {
            kind: Some(basal_host::OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    install_target_manifest(&f, |manifest| {
        manifest["ops"] = json!([{"module":"prefrontal-core","op":"work.unsettled"}]);
    });
    f
}

/// Core's captured `work.unsettled` vectors, copied unchanged from prefrontal
/// and pinned by hash in basal-module's `board_hygiene` tests.
fn work_unsettled_vector(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-vectors/work-unsettled-v1")
            .join(name),
    )
    .unwrap()
}

/// The bytes core's vectors would hold for `params`. The vectors were captured
/// for a placeholder install (`flow-hygiene` version 1, an all-zero hash), so
/// after checking that basal stamped this fixture's approved install, swap in
/// that placeholder stamp. Everything else is compared byte for byte.
fn as_vector_bytes(f: &Fixture, mut params: Value) -> Vec<u8> {
    assert_eq!(
        params["stamp"],
        json!({"flow_id":"flow-test","flow_version":2,"code_hash":*f.wire.hash.lock().unwrap()})
    );
    params["stamp"] = json!({"flow_id":"flow-hygiene","flow_version":1,"code_hash":"0".repeat(64)});
    (serde_json::to_string_pretty(&params).unwrap() + "\n").into_bytes()
}

#[test]
fn work_unsettled_requests_match_core_vectors_on_basal_route() {
    let f = work_unsettled_fixture(
        "work-unsettled-carrier",
        "const first = await ops.call('prefrontal-core', 'work.unsettled', {});
         const next = await ops.call('prefrontal-core', 'work.unsettled', {cursor: '1199:bg_page_199'});
         return [first, next];",
    );
    let run = f.admit("work-unsettled-carrier");
    f.activate(&run);
    // The wire echoes each request's params, so the result is what was sent.
    let result = run_result(&f, &run);
    let calls = f.wire.calls.lock().unwrap();
    let sent: Vec<_> = calls
        .iter()
        .filter(|(_, _, body)| body["method"] == "work.unsettled")
        .collect();
    assert_eq!(sent.len(), 2);
    for (index, vector) in ["request-global.json", "request-page-2.json"]
        .into_iter()
        .enumerate()
    {
        let (route, module, body) = sent[index];
        assert_eq!(*route, None, "{vector}: sent on a flow-scoped route");
        assert_eq!(module, "prefrontal-core");
        assert_eq!(
            body,
            &management_body("work.unsettled", result[index].clone())
        );
        assert_eq!(
            as_vector_bytes(&f, body["params"].clone()),
            work_unsettled_vector(vector),
            "{vector}"
        );
    }
    assert!(f.wire.opens.lock().unwrap().is_empty());
    assert!(f.wire.target_asks.lock().unwrap().is_empty());
    let classes: Vec<String> =
        f.rt.store()
            .read(|c| {
                let mut stmt =
                    c.prepare("SELECT class FROM journal WHERE run_id=?1 ORDER BY position")?;
                Ok(stmt
                    .query_map([&run], |row| row.get(0))?
                    .collect::<Result<_, _>>()?)
            })
            .unwrap();
    assert_eq!(classes, ["query", "query"]);
}

#[test]
fn work_unsettled_refusals_reach_the_script_typed() {
    let mut refusals: Vec<Value> = ["refusal-non-global.json", "refusal-unknown-stamp.json"]
        .into_iter()
        .map(|name| serde_json::from_slice(&work_unsettled_vector(name)).unwrap())
        .collect();
    // Core's other documented refusals have no captured vector; they use the
    // same `{code, message}` body.
    for code in [
        "sink_principal_refused",
        "sink_item_invalid",
        "sink_flow_hash_mismatch",
        "work_unsettled_not_granted",
    ] {
        refusals.push(json!({"code":code,"message":code}));
    }
    for refusal in refusals {
        let f = work_unsettled_fixture(
            "work-unsettled-refused",
            "try { await ops.call('prefrontal-core', 'work.unsettled', {}); return 'sent'; } catch (e) { return e.data; }",
        );
        f.wire
            .refusals
            .lock()
            .unwrap()
            .push_back(WireError::Refused {
                code: refusal["code"].as_str().unwrap().into(),
                message: refusal["message"].as_str().unwrap().into(),
            });
        let run = f.admit("work-unsettled-refused");
        f.activate(&run);
        assert_eq!(run_result(&f, &run), refusal);
        let calls = f.wire.calls.lock().unwrap();
        let sent: Vec<_> = calls
            .iter()
            .filter(|(_, _, body)| body["method"] == "work.unsettled")
            .collect();
        assert_eq!(sent.len(), 1, "{refusal}");
        assert_eq!(sent[0].0, None, "{refusal}");
    }
}

#[test]
fn work_unsettled_never_uses_a_flow_scoped_route() {
    let wire = Arc::new(Wire::default());
    let host = ModuleOpsHost::new(wire.clone(), Arc::new(SubcCatalog::new(wire.clone())));
    let request = |op: &str| CallRequest {
        flow_version: None,
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        position: 0,
        kind: CallKind::Op {
            module: "prefrontal-core".into(),
            op: op.into(),
        },
        args: JsonText::new(
            json!({"stamp":{"flow_id":"flow-a","flow_version":1,"code_hash":"0".repeat(64)}})
                .to_string(),
        )
        .unwrap(),
        idempotency_key: "key-a".into(),
        attempt: 1,
    };
    // Without flow-scopes/v1 or any scope, it is still sent on basal's route.
    host.dispatch(&request("work.unsettled")).unwrap();
    // With a core scope a flow-scoped route exists, and an ordinary core op
    // takes it; work.unsettled still does not.
    *wire.core_capable.lock().unwrap() = true;
    let mut core_scope = scope(1);
    core_scope.targets.insert("prefrontal-core".into());
    host.configure_flow("flow-a", Some(core_scope));
    host.dispatch(&request("work.unsettled")).unwrap();
    host.dispatch(&request("board.write")).unwrap();
    let routes: Vec<_> = wire
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(route, _, body)| (*route, body["method"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(
        routes,
        [
            (None, "work.unsettled".to_owned()),
            (None, "work.unsettled".to_owned()),
            (Some(1), "board.write".to_owned()),
        ]
    );
}

fn install_target_manifest(f: &Fixture, modify: impl FnOnce(&mut Value)) {
    f.world.catalog.add_named_agent("agent_alice", "Alice");
    for i in 0..80 {
        f.world
            .catalog
            .add_named_agent(&format!("agent_{i}"), &format!("Agent{i}"));
    }
    let mut manifest = test_manifest();
    manifest["version"] = json!(2);
    manifest["audience"] = json!({"kind":"global"});
    manifest["sinks"] = json!([]);
    manifest["status"] = json!([]);
    manifest["claims"] = json!([]);
    manifest.as_object_mut().unwrap().remove("facts");
    manifest.as_object_mut().unwrap().remove("llm");
    manifest["ops"] = json!([
        {"module":"mock","op":"echo"},
        {"module":"mock","op":"send","for_target":"delegate"},
        {"module":"mock","op":"post"}
    ]);
    modify(&mut manifest);
    let script =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT script FROM installs WHERE flow_id='flow-test' AND version=1",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap();
    let installed =
        f.rt.install(&InstallRequest {
            script,
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    f.rt.approve("flow-test", 2, &installed.code_hash, "approval")
        .unwrap();
    *f.wire.hash.lock().unwrap() = basal_core::ids::hex(&installed.code_hash);
}

fn target_fixture(tag: &str, script: &str) -> Fixture {
    let f = Fixture::new(tag, script, true, false);
    install_target_manifest(&f, |_| {});
    f
}

fn run_result(f: &Fixture, run: &str) -> Value {
    let row = f.rt.run(run).unwrap();
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    serde_json::from_str(row.result.unwrap().as_str()).unwrap()
}

#[test]
fn for_requires_an_audience_and_never_sends() {
    let f = Fixture::new(
        "for-without-audience",
        "try { await ops.call('mock','send',{}, {for:'ALF'}); } catch(e) { return e.data.code; }",
        true,
        false,
    );
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!("for_requires_audience"));
    assert!(f.wire.target_asks.lock().unwrap().is_empty());
    assert_eq!(f.sends(), 0);
}

#[test]
fn targeted_call_opens_target_scope_and_ordinary_call_keeps_flow_scope() {
    let f = target_fixture(
        "for-scopes",
        "const a=await ops.call('mock','echo',{n:1},{for:'Alice'}); const b=await ops.call('mock','send',{n:2}); const c=await ops.call('mock','echo',{n:3},{for:'agent_alice'}); return [a.n,b.n,c.n];",
    );
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!([1, 2, 3]));
    let asks = f.wire.target_asks.lock().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(
        asks[0],
        json!({"flow_id":"flow-test","version":2,"agent":"agent_alice","expires_at_ms":f.rt.run(&run).unwrap().deadline_at.unwrap()})
    );
    let opens = f.wire.opens.lock().unwrap();
    assert_eq!(opens.len(), 2);
    assert_eq!(opens[0].scope_ref, "target-1");
    assert_eq!(opens[1].scope_ref, "flow:flow-test");
    let journal = f.rt.calls(&run).unwrap();
    assert!(matches!(journal[0].kind, CallKind::OpFor { .. }));
    let request: Value =
        serde_json::from_str(journal[0].request.as_ref().unwrap().as_str()).unwrap();
    assert_eq!(request["agent"], "agent_alice");
    assert_eq!(request["args"], json!({"n":1}));
}

#[test]
fn target_mutation_requires_its_own_delegating_grant_but_queries_do_not() {
    let f = target_fixture(
        "per-op-delegation",
        "let code; try { await ops.call('mock','post',{}, {for:'Alice'}); } catch(e) { code=e.data.code; } const q=await ops.call('mock','echo',{n:7},{for:'Alice'}); await ops.call('mock','send',{n:8},{for:'Alice'}); return [code,q.n];",
    );
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(
        run_result(&f, &run),
        json!(["for_target_requires_delegate", 7])
    );
    assert_eq!(f.sends(), 1);
    assert!(
        !f.wire
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, v)| v["method"] == "post")
    );
    assert_eq!(f.rt.calls(&run).unwrap()[0].attempts, 0);
    assert!(f.rt.flow_health().unwrap()[0].waiting_reason.is_none());
}

#[test]
fn target_core_refusals_are_typed_unsent_without_retry_or_health_penalty() {
    for code in [
        "sink_target_not_in_audience",
        "flow_not_approved",
        "flow_has_no_audience",
        "scope_expiry_too_far",
    ] {
        let f = target_fixture(
            code,
            "try { await ops.call('mock','send',{}, {for:'Alice'}); } catch(e) { return e.data.code; }",
        );
        f.wire
            .target_answers
            .lock()
            .unwrap()
            .push_back(Err(basal_host::transport::refusal(
                subc_protocol::ErrorBody::new(code, "refused"),
            )));
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(run_result(&f, &run), json!(code));
        assert_eq!(f.sends(), 0);
        assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
        let row = &f.rt.calls(&run).unwrap()[0];
        assert_ne!(row.dispatch, basal_core::DispatchState::Deferred);
        assert_eq!(row.attempts, 1);
        assert!(f.rt.flow_health().unwrap()[0].waiting_reason.is_none());
    }
}

#[test]
fn provider_delegation_unsupported_is_typed_unsent_without_retry_or_health_penalty() {
    let f = target_fixture(
        "provider-delegation",
        "try { await ops.call('mock','send',{}, {for:'Alice'}); } catch(e) { return e.data.code; }",
    );
    f.wire
        .open_refusals
        .lock()
        .unwrap()
        .push_back(basal_host::transport::refusal(
            subc_protocol::ErrorBody::new(
                "flow_delegation_unsupported",
                "provider has not opted in",
            ),
        ));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!("flow_delegation_unsupported"));
    assert_eq!(f.sends(), 0);
    assert_eq!(f.wire.opens.lock().unwrap().len(), 1);
    let row = &f.rt.calls(&run).unwrap()[0];
    assert_eq!(row.attempts, 1);
    assert_ne!(row.dispatch, basal_core::DispatchState::Deferred);
    assert!(f.rt.flow_health().unwrap()[0].waiting_reason.is_none());
    let health: Option<String> =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT scope_health FROM flows WHERE flow_id='flow-test'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert!(health.is_none());
}

#[test]
fn distinct_target_default_bound_and_manifest_override() {
    let script = "let count=0; let code=null; for(let i=0;i<65;i++) { try { await ops.call('mock','echo',{}, {for:'agent_'+i}); count++; } catch(e) { code=e.data.code; } } await ops.call('mock','echo',{}, {for:'agent_0'}); return [count,code];";
    for (tag, limit, expected) in [
        ("target-default", None, 64),
        ("target-raised", Some(65), 65),
        ("target-lowered", Some(1), 1),
    ] {
        let f = Fixture::new(tag, script, true, false);
        install_target_manifest(&f, |m| {
            if let Some(limit) = limit {
                m["max_targets"] = json!(limit);
            }
        });
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(
            run_result(&f, &run),
            json!([
                expected,
                if expected == 65 {
                    Value::Null
                } else {
                    json!("too_many_targets")
                }
            ])
        );
        assert_eq!(f.wire.target_asks.lock().unwrap().len(), expected);
    }
}

#[test]
fn target_scope_is_shared_across_runs_while_it_covers_both_deadlines() {
    let f = target_fixture(
        "shared-target-epoch",
        "return await ops.call('mock','echo',{n:1},{for:'Alice'});",
    );
    let a = f.admit("a");
    f.activate(&a);
    run_result(&f, &a);
    f.clock.advance(1000);
    let b = f.admit("b");
    f.activate(&b);
    run_result(&f, &b);
    assert!(f.rt.run(&b).unwrap().deadline_at > f.rt.run(&a).unwrap().deadline_at);
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
    assert_eq!(f.wire.opens.lock().unwrap().len(), 1);
}

fn target_request(run: &str, deadline: i64) -> (CallRequest, basal_host::target_scope::TargetCall) {
    let call = basal_host::target_scope::TargetCall {
        agent: "agent_alice".into(),
        version: 2,
        expires_at_ms: deadline,
        args: json!({}),
        replay: false,
    };
    let request = CallRequest {
        flow_version: None,
        flow_id: "flow-test".into(),
        run_id: run.into(),
        position: 0,
        kind: CallKind::OpFor {
            module: "mock".into(),
            op: "echo".into(),
        },
        args: JsonText::null(),
        idempotency_key: format!("key-{run}"),
        attempt: 1,
    };
    (request, call)
}

fn answer(reference: &str, epoch: u64, expiry: i64) -> Result<Value, WireError> {
    Ok(json!({"ref":reference,"epoch":epoch,"delegates":false,"expires_at_ms":expiry}))
}

#[test]
fn rolling_target_scopes_preserve_each_runs_pinned_ref() {
    let f = target_fixture("rolling-target-scopes", "return 1;");
    f.wire.configure_flow("flow-test", Some(scope(1)));
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .extend([answer("old", 1, 200), answer("new", 1, 400)]);
    let (a, acall) = target_request("run-a", 100);
    let (b, bcall) = target_request("run-b", 300);
    let first = f.wire.target_flow(&a, &acall).unwrap();
    f.wire
        .management_for_flow(&first, "mock", "echo", json!({"a":1}))
        .unwrap();
    let second = f.wire.target_flow(&b, &bcall).unwrap();
    f.wire
        .management_for_flow(&second, "mock", "echo", json!({"b":1}))
        .unwrap();
    let pinned = f.wire.target_flow(&a, &acall).unwrap();
    f.wire
        .management_for_flow(&pinned, "mock", "echo", json!({"a":2}))
        .unwrap();
    assert_eq!(first, "target:old");
    assert_eq!(second, "target:new");
    assert_eq!(pinned, first);
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 2);
    let calls = f.wire.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter_map(|(route, _, _)| *route)
            .collect::<Vec<_>>(),
        vec![1, 2, 1]
    );
}

#[test]
fn target_scope_expiry_and_deadline_extension_reask_and_expired_refs_are_dropped() {
    let f = target_fixture("expired-target-scopes", "return 1;");
    f.wire.configure_flow("flow-test", Some(scope(1)));
    f.wire.target_answers.lock().unwrap().extend([
        answer("first", 1, 200),
        answer("second", 1, 400),
        answer("third", 1, 800),
    ]);
    let (a, acall) = target_request("run-a", 100);
    assert_eq!(f.wire.target_flow(&a, &acall).unwrap(), "target:first");
    let (b, bcall) = target_request("run-b", 300);
    assert_eq!(f.wire.target_flow(&b, &bcall).unwrap(), "target:second");
    *f.wire.now.lock().unwrap() = 401;
    let (c, ccall) = target_request("run-c", 500);
    assert_eq!(f.wire.target_flow(&c, &ccall).unwrap(), "target:third");
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 3);
    let routes = f.wire.routes.lock().unwrap();
    assert!(routes.targets("target:first").is_none());
    assert!(routes.targets("target:second").is_none());
}

#[test]
fn closed_target_scope_reasks_and_never_reopens_the_ended_ref() {
    let f = target_fixture("closed-target-scopes", "return 1;");
    f.wire.configure_flow("flow-test", Some(scope(1)));
    f.wire.target_answers.lock().unwrap().extend([
        answer("first", 1, 200),
        answer("first", 1, 200),
        answer("second", 1, 200),
    ]);
    let (a, call) = target_request("run-a", 100);
    let first = f.wire.target_flow(&a, &call).unwrap();
    f.wire
        .management_for_flow(&first, "mock", "echo", json!({}))
        .unwrap();
    f.wire.routes.lock().unwrap().closed(1, true);
    let error = f.wire.target_flow(&a, &call).unwrap_err();
    assert!(matches!(error,WireError::Refused { code,.. } if code=="scope_ended"));
    assert_eq!(f.wire.opens.lock().unwrap().len(), 1);
    let second = f.wire.target_flow(&a, &call).unwrap();
    f.wire
        .management_for_flow(&second, "mock", "echo", json!({}))
        .unwrap();
    assert_eq!(second, "target:second");
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 3);
    assert_eq!(f.wire.opens.lock().unwrap()[1].scope_ref, "second");
}

#[test]
fn changed_target_epoch_drops_old_routes_and_invalidates_old_run_pins() {
    let f = target_fixture("changed-target-epoch", "return 1;");
    f.wire.configure_flow("flow-test", Some(scope(1)));
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .extend([answer("shared", 1, 200), answer("shared", 2, 200)]);
    let (a, call) = target_request("run-a", 100);
    let first = f.wire.target_flow(&a, &call).unwrap();
    f.wire
        .management_for_flow(&first, "mock", "echo", json!({}))
        .unwrap();
    let (b, mut replay) = target_request("run-b", 100);
    replay.replay = true;
    let changed = f.wire.target_flow(&b, &replay).unwrap();
    f.wire
        .management_for_flow(&changed, "mock", "echo", json!({}))
        .unwrap();
    let pinned = f.wire.target_flow(&a, &call).unwrap();
    f.wire
        .management_for_flow(&pinned, "mock", "echo", json!({}))
        .unwrap();
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 2);
    let opens = f.wire.opens.lock().unwrap();
    assert_eq!(
        opens.iter().map(|s| s.epoch).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let calls = f.wire.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter_map(|(route, _, _)| *route)
            .collect::<Vec<_>>(),
        vec![1, 2, 2]
    );
}

#[test]
fn replay_reasks_core_and_outside_audience_call_fails_unsent() {
    let script = "const codes=[]; for(let i=0;i<2;i++) { try { await ops.call('mock','echo',{}, {for:'Alice'}); codes.push(null); } catch(e) { codes.push(e.data.code); } } return codes;";
    let probe = Arc::new(basal_testkit::harness::Probe::crash_at(
        basal_testkit::harness::Point::parse("HostAnswered { position: 0 }#1").unwrap(),
    ));
    let f = Fixture::with_hooks(
        "target-replay",
        script,
        true,
        false,
        ACTIVATION_WAIT,
        probe.clone(),
    );
    install_target_manifest(&f, |_| {});
    let run = f.admit("one");
    f.activate(&run);
    assert!(probe.fired());
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
    let f = f.restart();
    f.rt.recover().unwrap();
    assert!(f.rt.calls(&run).unwrap()[0].outcome.is_none());
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .extend((0..2).map(|_| {
            Err(basal_host::transport::refusal(
                subc_protocol::ErrorBody::new("sink_target_not_in_audience", "audience changed"),
            ))
        }));
    f.activate(&run);
    assert_eq!(
        run_result(&f, &run),
        json!(["sink_target_not_in_audience", "sink_target_not_in_audience"])
    );
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 3);
    assert_eq!(
        f.wire
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, v)| v["method"] == "echo")
            .count(),
        1
    );
}

#[test]
fn midcall_target_scope_cut_makes_even_keyed_mutations_outcome_unknown() {
    let script = "return await ops.call('mock','send',{}, {for:'Alice'});";
    let f = Fixture::with_host_options(
        "target-mutation-cut",
        script,
        true,
        false,
        ACTIVATION_WAIT,
        Arc::new(NoHooks),
        true,
    );
    install_target_manifest(&f, |_| {});
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::ScopeCut("scope ended during call".into()));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::NeedsReconcile);
    let row = &f.rt.calls(&run).unwrap()[0];
    assert_eq!(row.class, basal_core::StoredClass::KeyedMutation);
    assert_eq!(row.dispatch, basal_core::DispatchState::Unknown);
    assert!(row.scope_cut);
    assert_eq!(
        f.rt.unknown_reason(&run, 0).unwrap(),
        Some(basal_host::UnknownReason::ConnectionLost)
    );
    assert_eq!(f.sends(), 1);
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
    f.activate(&run);
    assert_eq!(f.sends(), 1);
}

#[test]
fn midcall_target_scope_cut_retries_a_query_on_a_new_ref() {
    let f = target_fixture(
        "target-query-cut",
        "return await ops.call('mock','echo',{n:7},{for:'Alice'});",
    );
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::ScopeCut("scope changed during call".into()));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!({"n":7}));
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 2);
    assert_eq!(
        f.wire
            .opens
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.scope_ref.as_str())
            .collect::<Vec<_>>(),
        vec!["target-1", "target-2"]
    );
}

#[test]
fn delegation_refusals_do_not_penalize_uncaught_runs() {
    for (op, code) in [
        ("post", "for_target_requires_delegate"),
        ("send", "flow_delegation_unsupported"),
    ] {
        let script = format!("return await ops.call('mock','{op}',{{}}, {{for:'Alice'}});");
        let f = target_fixture("delegation-no-health-penalty", &script);
        if op == "send" {
            f.wire
                .open_refusals
                .lock()
                .unwrap()
                .push_back(basal_host::transport::refusal(
                    subc_protocol::ErrorBody::new(code, "not supported"),
                ));
        }
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
        let outcome = f.rt.calls(&run).unwrap()[0].outcome.clone().unwrap();
        let value: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
        assert_eq!(value["code"], code);
        assert_eq!(f.sends(), 0);
    }
}

#[test]
fn midcall_target_scope_cut_survives_restart_without_resending_keyed_mutation() {
    let probe = Arc::new(basal_testkit::harness::Probe::crash_at(
        basal_testkit::harness::Point::parse("HostAnswered { position: 0 }#1").unwrap(),
    ));
    let f = Fixture::with_host_options(
        "target-cut-restart",
        "return await ops.call('mock','send',{}, {for:'Alice'});",
        true,
        false,
        ACTIVATION_WAIT,
        probe.clone(),
        true,
    );
    install_target_manifest(&f, |_| {});
    f.wire
        .refusals
        .lock()
        .unwrap()
        .push_back(WireError::ScopeCut("scope ended during call".into()));
    let run = f.admit("one");
    f.activate(&run);
    assert!(probe.fired());
    let f = f.restart();
    f.rt.recover().unwrap();
    f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::NeedsReconcile);
    assert_eq!(f.sends(), 1);
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
}

#[test]
fn transient_target_audience_unavailability_defers_until_recovery_or_run_deadline() {
    let f = target_fixture(
        "target-audience-transient",
        "return await ops.call('mock','echo',{n:7},{for:'Alice'});",
    );
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .push_back(Err(WireError::Refused {
            code: "sink_audience_unavailable".into(),
            message: "registry offline".into(),
        }));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Suspended);
    assert_eq!(
        f.rt.calls(&run).unwrap()[0].dispatch,
        basal_core::DispatchState::Deferred
    );
    assert!(f.wire.opens.lock().unwrap().is_empty());
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
    f.clock.advance(101);
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!({"n":7}));
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 2);

    let f = target_fixture(
        "target-audience-deadline",
        "return await ops.call('mock','echo',{}, {for:'Alice'});",
    );
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .push_back(Err(WireError::Refused {
            code: "sink_audience_unavailable".into(),
            message: "registry offline".into(),
        }));
    let run = f.admit("one");
    f.activate(&run);
    f.clock.set(f.rt.run(&run).unwrap().deadline_at.unwrap());
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    let refused: String =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT value FROM mailbox WHERE run_id=?1 AND position=0",
                    [&run],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    let value: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(value["code"], "sink_audience_unavailable");
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    assert!(f.wire.opens.lock().unwrap().is_empty());
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
}

#[test]
fn target_agent_registry_failure_defers_unsent_until_recovery_or_run_deadline() {
    for failure in [
        WireError::NeverSent("core restarting".into()),
        WireError::TimedOut("agent.list timed out".into()),
        WireError::Unreadable("agent.list reply undecodable".into()),
    ] {
        let f = target_fixture(
            "target-registry-down",
            "return await ops.call('mock','echo',{n:7},{for:'Alice'});",
        );
        f.world.catalog.fail_agent_registry(Some(failure.clone()));
        let run = f.admit("one");
        f.activate(&run);
        assert_eq!(
            f.rt.run(&run).unwrap().state,
            RunState::Suspended,
            "{failure:?}"
        );
        let row = &f.rt.calls(&run).unwrap()[0];
        assert_eq!(row.dispatch, basal_core::DispatchState::Deferred);
        assert!(row.request.is_none(), "nothing is frozen before resolution");
        assert!(row.outcome.is_none());
        let reason: String =
            f.rt.store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT refusal FROM journal WHERE run_id=?1 AND position=0",
                        [&run],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
        assert_eq!(reason, "sink_audience_unavailable");
        assert!(f.wire.target_asks.lock().unwrap().is_empty());
        assert!(f.wire.opens.lock().unwrap().is_empty());
        // The registry answers again: the due retry resolves the target,
        // freezes its stable id and sends the call.
        f.world.catalog.fail_agent_registry(None);
        f.clock.set(f.retry_at(&run));
        f.activate(&run);
        assert_eq!(run_result(&f, &run), json!({"n":7}));
        let asks = f.wire.target_asks.lock().unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0]["agent"], "agent_alice");
        let row = &f.rt.calls(&run).unwrap()[0];
        let request: Value = serde_json::from_str(row.request.as_ref().unwrap().as_str()).unwrap();
        assert_eq!(request["agent"], "agent_alice");
        assert_eq!(request["args"], json!({"n":7}));
    }

    let f = target_fixture(
        "target-registry-deadline",
        "return await ops.call('mock','echo',{}, {for:'Alice'});",
    );
    f.world
        .catalog
        .fail_agent_registry(Some(WireError::NeverSent("core restarting".into())));
    let run = f.admit("one");
    f.activate(&run);
    f.clock.set(f.rt.run(&run).unwrap().deadline_at.unwrap());
    f.rt.enforce_deadlines().unwrap();
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    let refused: String =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT value FROM mailbox WHERE run_id=?1 AND position=0",
                    [&run],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    let value: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(value["code"], "sink_audience_unavailable");
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    assert!(f.wire.target_asks.lock().unwrap().is_empty());
    assert_eq!(f.sends(), 0);
}

#[test]
fn target_absent_from_an_answered_agent_registry_is_refused_finally() {
    let f = target_fixture(
        "target-registry-absent",
        "try { await ops.call('mock','echo',{}, {for:'Nobody'}); } catch(e) { return e.data.code; }",
    );
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(run_result(&f, &run), json!("sink_audience_unavailable"));
    let row = &f.rt.calls(&run).unwrap()[0];
    assert_ne!(row.dispatch, basal_core::DispatchState::Deferred);
    assert_eq!(row.attempts, 0);
    assert!(f.wire.target_asks.lock().unwrap().is_empty());
    assert!(f.wire.opens.lock().unwrap().is_empty());
}
#[test]
fn target_scope_principal_refusal_is_a_terminal_carrier_bug() {
    let f = target_fixture(
        "target-carrier-bug",
        "return await ops.call('mock','echo',{}, {for:'Alice'});",
    );
    f.wire
        .target_answers
        .lock()
        .unwrap()
        .push_back(Err(WireError::Refused {
            code: "flow_target_scope_principal_refused".into(),
            message: "wrong principal".into(),
        }));
    let run = f.admit("one");
    f.activate(&run);
    assert_eq!(f.rt.run(&run).unwrap().state, RunState::Failed);
    let row = &f.rt.calls(&run).unwrap()[0];
    assert_eq!(row.attempts, 1);
    assert_ne!(row.dispatch, basal_core::DispatchState::Deferred);
    let value: Value = serde_json::from_str(row.outcome.as_ref().unwrap().value.as_str()).unwrap();
    assert_eq!(value["code"], "flow_target_scope_principal_refused");
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 1);
    assert!(f.wire.opens.lock().unwrap().is_empty());
    assert_eq!(f.wire.target_asks.lock().unwrap().len(), 1);
}
