//! Core's flow-scope gate refuses a flow-scoped call to an operation flows
//! may not use, with the reply pinned in `tests/vectors/flow-scope-gate-v1`.
//! These tests drive that reply through basal's flow-call path (worker,
//! runtime, `ModuleOpsHost`'s flow route, the transport client's error
//! mapping and the engine's passes) and check that it reaches the script whole
//! and ends the call: basal never defers or resends it, and it affects flow
//! health exactly as any other final provider refusal does.
use super::*;
use basal_host::transport::Transport;
use subc_client_rs::consumer::CallError;

const REFUSAL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/vectors/flow-scope-gate-v1/flow-scope-refusal.json"
);
const FLOW: &str = "host-flow";

fn refusal_vector() -> Value {
    serde_json::from_slice(&std::fs::read(REFUSAL).unwrap()).unwrap()
}

/// A provider's error reply as basal's subc transport hands it on: the client
/// decodes the error body, and `map_error` classifies it.
fn provider_reply(body: Value) -> WireError {
    map_error(CallError::Module(serde_json::from_value(body).unwrap()))
}

/// Forwards catalog, management and tool calls to the fake, and records the
/// management calls basal sends on a flow's scoped route rather than its own
/// unscoped one.
struct FlowRouted {
    fake: Arc<Fake>,
    routed: Mutex<Vec<(String, String)>>,
}

impl Transport for FlowRouted {
    fn catalog(&self) -> Result<Value, WireError> {
        self.fake.catalog()
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.fake.management(module, op, params)
    }
    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        key: &str,
    ) -> Result<Value, WireError> {
        self.fake.tool(module, name, arguments, key)
    }
    fn management_for_flow(
        &self,
        _flow: &str,
        module: &str,
        op: &str,
        params: Value,
    ) -> Result<Value, WireError> {
        self.routed.lock().unwrap().push((module.into(), op.into()));
        self.fake.management(module, op, params)
    }
}

struct Rig {
    fake: Arc<Fake>,
    transport: Arc<FlowRouted>,
    f: common::Fixture,
    run: String,
}

/// Installs and approves a flow whose script makes one `prefrontal-core` call
/// under the flow's scope, queues `reply` as core's answer to it, and runs the
/// engine until it is idle.
fn run_refused(tag: &str, script: &str, reply: WireError) -> Rig {
    let fake = Fake::new();
    // Core advertises flow-scopes/v1, so its non-carrier ops go on the flow's
    // scoped route; the op itself is catalogued so the install and the
    // dispatch both resolve it.
    fake.catalog.lock().unwrap()["modules"]
        .as_array_mut()
        .unwrap()
        .push(json!({"module_id":"prefrontal-core",
            "capabilities":{"provides":["flow-scopes/v1"]},
            "roles":[{"role":"management_surface","operations":[{"name":"sink.digest","kind":"mutate"}],
                "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}],
            "control_ops":[]}));
    let transport = Arc::new(FlowRouted {
        fake: fake.clone(),
        routed: Mutex::default(),
    });
    let catalog = Arc::new(SubcCatalog::new(transport.clone()));
    let hosts = Hosts {
        transport: Arc::new(basal_module::unconfigured::UnconfiguredTransport),
        host: Arc::new(RoutingHost::new(
            Arc::new(ModuleOpsHost::new(transport.clone(), catalog.clone())),
            Arc::new(CoreHost::new(transport.clone())),
            Arc::new(UnconfiguredHost::new()),
        )),
        catalog,
        consent: Arc::new(CoreConsent::new(fake.clone())),
        hooks: Arc::new(NoHooks),
    };
    let f = common::fixture(
        tag,
        common::Options {
            hosts: Some(hosts),
            ..common::Options::with_worker()
        },
    );
    let mut m = manifest();
    m["ops"] = json!([{"module":"prefrontal-core","op":"sink.digest"}]);
    common::install(&f, &Caller::Operator, script, &m);
    approve(&fake, &f.module);
    fake.enqueue("sink.digest", vec![Err(reply)]);
    let run = common::admit(&f, FLOW, tag);
    f.module.engine.run_until_idle(50).unwrap();
    Rig {
        fake,
        transport,
        f,
        run,
    }
}

fn health(f: &common::Fixture) -> basal_core::ops::FlowHealth {
    f.module
        .rt
        .flow_health()
        .unwrap()
        .into_iter()
        .find(|h| h.flow_id == FLOW)
        .unwrap()
}

#[test]
fn core_flow_scope_refusal_reaches_the_script_whole_and_is_never_retried() {
    let vector = refusal_vector();
    let rig = run_refused(
        "flow-scope-refusal",
        "try { await ops.call('prefrontal-core','sink.digest',{}); return 'fulfilled'; } \
         catch (e) { return {message: e.message, data: e.data}; }",
        provider_reply(vector.clone()),
    );
    let Rig {
        fake,
        transport,
        f,
        run,
    } = &rig;

    // The call went to core on the flow's scoped route, once.
    assert_eq!(
        *transport.routed.lock().unwrap(),
        [("prefrontal-core".to_owned(), "sink.digest".to_owned())]
    );
    assert_eq!(fake.calls_to("prefrontal-core", "sink.digest").len(), 1);

    // The script saw a rejection carrying core's code, message and detail
    // unchanged, and finished.
    let done = f.module.rt.run(run).unwrap();
    assert_eq!(done.state, RunState::Succeeded, "{:?}", done.error_detail);
    let seen: Value = serde_json::from_str(done.result.as_deref().unwrap()).unwrap();
    assert_eq!(seen["data"]["code"], "flow_scope_not_supported");
    assert_eq!(seen["message"], vector["message"]);
    assert_eq!(seen["data"]["message"], vector["message"]);
    assert_eq!(
        seen["data"]["detail"],
        json!({"allowed_ops":["diagnostics.findings"],"op":"sink.digest"})
    );
    assert_eq!(seen["data"], vector);

    // The journal holds a final rejection: sent once, not deferred, with no
    // retry time.
    let row = &f.module.rt.calls(run).unwrap()[0];
    assert_eq!(row.dispatch, basal_core::DispatchState::Sent);
    assert_eq!(row.attempts, 1);
    assert_eq!(
        row.outcome.as_ref().unwrap().settlement,
        Settlement::Rejected
    );
    let retry: Option<i64> = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT retry_not_before FROM journal WHERE run_id=?1",
                [run],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(retry, None);

    // Past the longest deferral backoff (a minute), and before the flow's
    // hourly schedule fires again, another pass sends nothing more.
    f.clock.set(common::T0 + 10 * 60_000);
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(fake.calls_to("prefrontal-core", "sink.digest").len(), 1);
    assert_eq!(transport.routed.lock().unwrap().len(), 1);
    assert_eq!(f.module.rt.calls(run).unwrap().len(), 1);

    // Caught by the script, the refusal is no failure at all.
    let health = health(f);
    assert_eq!(health.consecutive_failures, 0);
    assert!(health.enabled && !health.auto_disabled);
}

#[test]
fn uncaught_core_flow_scope_refusal_counts_like_any_final_provider_refusal() {
    // Any other final provider refusal with a detail, for comparison.
    let other = json!({"code":"provider_denied","message":"no","detail":{"why":"policy"}});
    let mut outcomes = Vec::new();
    for (tag, body) in [
        ("flow-scope-uncaught", refusal_vector()),
        ("other-uncaught", other),
    ] {
        let rig = run_refused(
            tag,
            "return await ops.call('prefrontal-core','sink.digest',{});",
            provider_reply(body),
        );
        let done = rig.f.module.rt.run(&rig.run).unwrap();
        let health = health(&rig.f);
        outcomes.push((
            done.state,
            done.error_kind,
            health.consecutive_failures,
            health.enabled,
            health.auto_disabled,
        ));
    }
    // Left uncaught, either refusal fails the run as a script failure that
    // the health view counts, and neither disables the flow.
    assert_eq!(
        outcomes[0],
        (RunState::Failed, Some("script".to_owned()), 1, true, false)
    );
    assert_eq!(outcomes[0], outcomes[1]);
}
