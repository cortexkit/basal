mod common;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use basal_core::NoHooks;
use basal_host::mock::MockHost;
use basal_host::transport::{Transport, WireError};
use basal_host::{MockCatalog, MockConsent};
use basal_module::module::Hosts;
use basal_module::tool::Context;
use cortexkit_role_tool_provider::call::{ToolCallRequest, ToolCallRequestExt};
use serde_json::{Value, json};
use subc_protocol::{Principal, scope::ScopeStamp};

#[derive(Default)]
struct Core {
    scopes: basal_testkit::run_scopes::RunScopes,
    host: MockHost,
    opens: Mutex<Vec<Value>>,
    closes: Mutex<Vec<Value>>,
    state: Mutex<(usize, bool)>,
    changed: Condvar,
    refusal: Mutex<Option<WireError>>,
    mismatch: Mutex<bool>,
    identities: Mutex<Vec<subc_protocol::BindIdentity>>,
    store: Mutex<Option<Arc<basal_core::Store>>>,
}

impl Core {
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
    fn wait_sent(&self, count: usize) {
        let sent = self.state.lock().unwrap();
        let (sent, timeout) = self
            .changed
            .wait_timeout_while(sent, Duration::from_secs(180), |sent| sent.0 < count)
            .unwrap();
        assert!(!timeout.timed_out(), "provider not entered: {sent:?}");
    }
}
impl Transport for Core {
    fn tool_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        input: Value,
        key: &str,
        options: &basal_host::transport::RunToolOptions,
    ) -> Result<Value, WireError> {
        self.identities
            .lock()
            .unwrap()
            .push(options.identity.clone());
        assert!(options.reply_timeout > Duration::from_secs(30));
        self.tool_for_flow(run, module, name, input, key)
    }
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(
            json!({"modules":[{"module_id":"mock","capabilities":{"provides":["agent-run-scopes/v1"]},"roles":[{
            "role":"tool_provider","identity_scope":[],"concurrency":"module_managed","emits_push":false,"sub_supervises":false,
            "tools":[{"name":"echo","execution_mode":"mutating","schema":{"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}}]}]}]}),
        )
    }
    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        assert_eq!(module, "prefrontal-core");
        match op {
            "codemode.run_scope.open" => {
                let id = params["run_id"].as_str().unwrap();
                if let Some(store) = self.store.lock().unwrap().as_ref() {
                    assert!(
                        store
                            .read(|conn| Ok(matches!(
                                basal_core::codemode::store::lookup(conn, id)?,
                                basal_core::codemode::store::Lookup::Found(_)
                            )))
                            .unwrap(),
                        "record must precede open"
                    );
                }
                self.opens.lock().unwrap().push(params.clone());
                let agent = if *self.mismatch.lock().unwrap() {
                    "forged"
                } else {
                    params["agent_id"].as_str().unwrap()
                };
                self.host.set_scope_description(Principal::Reserved { module_id:"prefrontal-core".into() },id,Ok(serde_json::from_value(json!({
                    "status":"live","scope_epoch":19,"daemon_incarnation":"test","owner_synced":true,"owner_configured":true,
                    "scope":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":id,"scope_epoch":19,"kind":"ephemeral",
                        "attributes":{"agent_id":agent,"run_id":id},"owner_authorized":true}})).unwrap()));
                // A matching daemon stamp from a previously published scope
                // cannot override core refusing the codemode grant. Keep that
                // stamp valid so this test exercises the refusal itself.
                if let Some(error) = self.refusal.lock().unwrap().clone() {
                    return Err(error);
                }
                let opened = self.scopes.open(
                    serde_json::from_value(params.clone()).unwrap(),
                    vec![basal_host::run_scope::CatalogEntry {
                        tool: "echo".into(),
                        module: "mock".into(),
                        op: "echo".into(),
                    }],
                )?;
                Ok(serde_json::to_value(opened).unwrap())
            }
            "codemode.run_scope.close" => {
                self.closes.lock().unwrap().push(params.clone());
                Ok(
                    serde_json::to_value(
                        self.scopes.close(serde_json::from_value(params).unwrap()),
                    )
                    .unwrap(),
                )
            }
            _ => panic!("unexpected management op {op}"),
        }
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unscoped send")
    }
    fn tool_for_flow(
        &self,
        route: &str,
        module: &str,
        op: &str,
        input: Value,
        key: &str,
    ) -> Result<Value, WireError> {
        assert!(route.starts_with("codemode:"));
        assert_eq!((module, op), ("mock", "echo"));
        assert!(key.starts_with("bk1-"));
        self.state.lock().unwrap().0 += 1;
        self.changed.notify_all();
        drop(
            self.changed
                .wait_while(self.state.lock().unwrap(), |state| !state.1)
                .unwrap(),
        );
        Ok(input)
    }
}
struct Rig {
    fixture: common::Fixture,
    core: Arc<Core>,
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.core.release();
    }
}
fn rig(tag: &str, missing_worker: bool) -> Rig {
    let core = Arc::new(Core::default());
    let hosts = Hosts {
        host: Arc::new(core.host.clone()),
        transport: core.clone(),
        catalog: Arc::new(MockCatalog::standard()),
        consent: Arc::new(MockConsent::new()),
        hooks: Arc::new(NoHooks),
    };
    let save = core.clone();
    let fixture = common::fixture_with_store(
        tag,
        common::Options {
            hosts: Some(hosts),
            worker_binary: missing_worker.then(|| std::env::temp_dir().join("no-worker")),
            ..Default::default()
        },
        move |store| {
            *save.store.lock().unwrap() = Some(store);
            Ok(())
        },
    );
    Rig { fixture, core }
}
fn context() -> Context {
    Context { bind_identity: subc_protocol::BindIdentity::new("/agent/workspace","agent-harness","agent-session"),principal:Principal::Reserved { module_id:"broca".into() },scope:Some(serde_json::from_value::<ScopeStamp>(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"agent-scope","scope_epoch":7,"kind":"head",
        "attributes":{"agent_id":"agent"},"owner_authorized":true})).unwrap()) }
}
fn call(rig: &Rig, key: &str, code: &str, wait: Duration) -> Value {
    rig.fixture
        .module
        .handle_tool(
            &context(),
            ToolCallRequest::new("codemode", json!({"code":code})).with_call_key(key),
            wait,
        )
        .unwrap()
}

#[test]
fn core_scope_shapes_and_stamp_mismatch_prevent_worker_start() {
    let rig = rig("tool-scope-mismatch", false);
    *rig.core.mismatch.lock().unwrap() = true;
    let result = call(&rig, "mismatch", "return 1;", Duration::from_secs(10));
    assert!(
        rig.fixture.module.codemode_pool.handouts().is_empty(),
        "a mismatched stamp started a worker"
    );
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"]["code"], "scope_mismatch");
    let open = rig.core.opens.lock().unwrap()[0].clone();
    assert_eq!(
        open,
        json!({"agent_id":"agent","invoking_scope":{"ref":"agent-scope","epoch":7},"run_id":open["run_id"],"expires_at_ms":common::T0+2_460_000})
    );
    assert_eq!(
        rig.core.closes.lock().unwrap().as_slice(),
        [json!({"run_id":open["run_id"],"epoch":19})]
    );
    assert_eq!(rig.core.state.lock().unwrap().0, 0);
}

#[test]
fn refused_open_preserves_core_code_and_detail_without_worker() {
    for (code, detail) in [
        ("not_granted", None),
        ("expiry_too_far", Some(json!({"ceiling_ms":2_700_000}))),
        (
            "codemode_unavailable",
            Some(json!({"reason":"core_unscoped"})),
        ),
    ] {
        let rig = rig(code, false);
        *rig.core.refusal.lock().unwrap() = Some(match detail.clone() {
            Some(detail) => WireError::RefusedDetails {
                code: code.into(),
                message: "core refusal".into(),
                detail,
            },
            None => WireError::Refused {
                code: code.into(),
                message: "core refusal".into(),
            },
        });
        let result = call(&rig, code, "return 1;", Duration::from_secs(10));
        assert!(
            rig.fixture.module.codemode_pool.handouts().is_empty(),
            "a refused open started a worker"
        );
        assert_eq!(result["status"], code);
        assert_eq!(result["error"]["code"], code);
        assert_eq!(result["error"]["message"], "core refusal");
        assert_eq!(result.get("detail"), detail.as_ref());
        assert!(rig.core.closes.lock().unwrap().is_empty());
        assert_eq!(rig.core.state.lock().unwrap().0, 0);
    }
}

#[test]
fn retried_key_attaches_to_running_run_and_finished_result() {
    let rig = rig("tool-attach", false);
    let first = call(
        &rig,
        "retry",
        "return await tools.echo({n:1});",
        Duration::ZERO,
    );
    rig.core.wait_sent(1);
    let retry = call(&rig, "retry", "not JavaScript", Duration::ZERO);
    assert_eq!(first, retry);
    assert_eq!(rig.core.opens.lock().unwrap().len(), 1);
    assert_eq!(rig.core.state.lock().unwrap().0, 1);
    rig.core.release();
    let result = call(&rig, "retry", "return 999;", Duration::from_secs(10));
    assert_eq!(result["status"], "completed");
    assert_eq!(result["value"], json!({"n":1}));
    assert_eq!(
        call(&rig, "retry", "return 999;", Duration::from_secs(1)),
        result
    );
    assert_eq!(rig.core.closes.lock().unwrap().len(), 1);
}

#[test]
fn keyless_calls_run_twice_have_no_withdrawal_or_late_result_and_warn() {
    let rig = rig("tool-keyless", false);
    rig.core.release();
    for _ in 0..2 {
        let result = rig
            .fixture
            .module
            .handle_tool(
                &context(),
                ToolCallRequest::new(
                    "codemode",
                    json!({"code":"return await tools.echo({n:1});"}),
                ),
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["keyless"], true);
        assert!(
            result["text"]
                .as_str()
                .unwrap()
                .contains("had no key and can't be recovered")
        );
    }
    let opens = rig.core.opens.lock().unwrap();
    assert_eq!(opens.len(), 2);
    assert_ne!(opens[0]["run_id"], opens[1]["run_id"]);
    assert_eq!(rig.core.state.lock().unwrap().0, 2);
    for request in [
        ToolCallRequest::new("tool.withdraw", json!({"call_key":"not-held"})),
        ToolCallRequest::new("late_results", json!({"since":null})),
    ] {
        let result = rig
            .fixture
            .module
            .handle_tool(&context(), request, Duration::ZERO)
            .unwrap();
        assert!(result["answer"] == "unknown_call" || result["entries"] == json!([]));
    }
}

#[test]
fn tool_manifest_description_and_schema_are_pinned_and_old_entries_are_gone() {
    use subc_protocol::manifest::ProviderRole;
    let manifest = basal_module::manifest::manifest();
    assert!(
        manifest
            .capabilities
            .as_ref()
            .unwrap()
            .provides
            .iter()
            .any(|cap| cap == "tool-provider/v1")
    );
    let tools = manifest
        .provides
        .iter()
        .find_map(|role| match role {
            ProviderRole::ToolProvider { tools, .. } => Some(tools),
            _ => None,
        })
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "codemode");
    assert_eq!(
        tools[0].description.as_deref().unwrap(),
        include_str!("fixtures/codemode-description.txt").trim_end()
    );
    assert_eq!(tools[0].schema, basal_module::tool::schema());
    let rig = rig("old-entry-removed", true);
    for op in ["codemode.run", "codemode.result", "codemode.cancel"] {
        assert!(
            !basal_module::manifest::OPERATIONS
                .iter()
                .any(|(name, _, _)| *name == op)
        );
        assert_eq!(
            rig.fixture
                .module
                .handle(&basal_module::caller::Caller::Core, op, json!({}))
                .unwrap_err()
                .code,
            "unknown_method"
        );
        assert!(!include_str!("../../../docs/ops.md").contains(&format!("`{op}`")));
        assert!(!include_str!("../../../docs/ops.md").contains(&format!("## {op}\n")));
    }
}

#[test]
fn daemon_schema_not_program_arguments_governs_dispatch_and_result_renders() {
    let rig = rig("tool-schema", false);
    rig.core.release();
    let result = call(
        &rig,
        "schema",
        "console.log('partial'); try { await tools.echo({n:'wrong'}); } catch (e) { console.log(e.code); } return {answer:42};",
        Duration::from_secs(10),
    );
    assert_eq!(result["status"], "completed");
    assert!(result.get("description").is_none());
    assert_eq!(result["value"], json!({"answer":42}));
    assert_eq!(result["calls"][0]["code"], "invalid_input");
    assert_eq!(rig.core.state.lock().unwrap().0, 0);
    let text = result["text"].as_str().unwrap();
    for section in [
        "completed",
        "person wait",
        "Return value:",
        "Output:",
        "partial",
        "echo | refused | false",
    ] {
        assert!(text.contains(section), "{text}");
    }
    let result = rig
        .fixture
        .module
        .handle_tool(
            &context(),
            ToolCallRequest::new("codemode", json!({"code":"return 1;","agent_id":"forged"}))
                .with_call_key("override"),
            Duration::ZERO,
        )
        .unwrap_err();
    assert_eq!(result.code, "invalid_request");
    assert_eq!(rig.core.opens.lock().unwrap().len(), 1);
}

#[test]
fn codemode_workers_are_separate_from_flow_pool_and_never_shared() {
    let rig = rig("codemode-pools", false);
    let before = rig.fixture.module.pool.stats();
    for key in ["one", "two"] {
        call(&rig, key, "return await tools.echo({n:1});", Duration::ZERO);
    }
    rig.core.wait_sent(2);
    assert_eq!(rig.fixture.module.pool.stats(), before);
    let workers = rig.fixture.module.codemode_pool.handouts();
    assert_eq!(workers.len(), 2);
    assert_ne!(workers[0].pid, workers[1].pid);
    assert!(
        workers
            .iter()
            .all(|worker| worker.binding == basal_module::pool::Binding::Codemode)
    );
    rig.core.release();
    for key in ["one", "two"] {
        assert_eq!(
            call(&rig, key, "", Duration::from_secs(10))["status"],
            "completed"
        );
    }
    assert_eq!(
        call(&rig, "three", "return 3;", Duration::from_secs(10))["value"],
        3
    );
    let workers = rig.fixture.module.codemode_pool.handouts();
    assert_eq!(workers.len(), 3);
    assert!(
        workers[..2]
            .iter()
            .all(|worker| worker.pid != workers[2].pid)
    );
}

#[test]
fn codemode_pool_retires_even_an_unused_lease() {
    let rig = rig("codemode-unused", false);
    for _ in 0..2 {
        drop(
            rig.fixture
                .module
                .codemode_pool
                .acquire(basal_module::pool::Binding::Codemode)
                .unwrap(),
        );
        assert_eq!(rig.fixture.module.codemode_pool.stats().live, 0);
    }
    let workers = rig.fixture.module.codemode_pool.handouts();
    assert_ne!(workers[0].pid, workers[1].pid);
}

#[cfg(target_os = "linux")]
#[test]
fn codemode_linux_worker_address_space_equals_protocol_limit() {
    let rig = rig("codemode-limit", false);
    call(
        &rig,
        "limit",
        "return await tools.echo({n:1});",
        Duration::ZERO,
    );
    rig.core.wait_sent(1);
    let pid = rig.fixture.module.codemode_pool.handouts()[0].pid;
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // The null new-limit pointer reads the live child without changing it.
    assert_eq!(
        unsafe {
            libc::prlimit(
                pid as libc::pid_t,
                libc::RLIMIT_AS,
                std::ptr::null(),
                &mut limits,
            )
        },
        0
    );
    assert_eq!(limits.rlim_cur, basal_proto::CODEMODE_ADDRESS_SPACE_BYTES);
    assert_eq!(limits.rlim_max, basal_proto::CODEMODE_ADDRESS_SPACE_BYTES);
    rig.core.release();
    assert_eq!(
        call(&rig, "limit", "", Duration::from_secs(10))["status"],
        "completed"
    );
}

#[test]
fn codemode_engine_budgets_still_apply_end_to_end() {
    for (code, status) in [
        (
            "return new ArrayBuffer(1024 * 1024).byteLength;",
            "completed",
        ),
        (
            "try { new ArrayBuffer(80 * 1024 * 1024); } catch (_) {} return 4;",
            "budget_exhausted:memory",
        ),
        (
            "function f() { return f(); } try { f(); } catch (_) {} return 4;",
            "budget_exhausted:stack",
        ),
        ("while (true) {}", "budget_exhausted:js_cpu"),
    ] {
        let rig = rig(status, false);
        let result = call(&rig, "budget", code, Duration::from_secs(180));
        assert_eq!(result["status"], status, "{result}");
    }
}

fn shutdown_module(tag: &str) -> (basal_module::module::Module, std::path::PathBuf, Arc<Core>) {
    let dir = common::scratch(tag);
    let core = Arc::new(Core::default());
    let pool = common::pool_config(&common::Options::default());
    let module = basal_module::module::Module::start(
        basal_module::module::ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: basal_core::Durability { fullfsync: false },
            runtime: basal_core::Config {
                clock: basal_core::Clock::manual(common::T0),
                ..Default::default()
            },
            pool: pool.clone(),
            engine: Default::default(),
            dry_run: basal_module::dryrun::DryRunConfig::new(dir.join("dry-run")),
        },
        Hosts {
            host: Arc::new(core.host.clone()),
            transport: core.clone(),
            catalog: Arc::new(MockCatalog::standard()),
            consent: Arc::new(MockConsent::new()),
            hooks: Arc::new(NoHooks),
        },
        Arc::new(basal_module::pool::ProcessSpawner::new(&pool)),
    )
    .unwrap();
    module
        .handle_tool(
            &context(),
            ToolCallRequest::new(
                "codemode",
                json!({"code":"return await tools.echo({n:1});"}),
            )
            .with_call_key("held"),
            Duration::ZERO,
        )
        .unwrap();
    core.wait_sent(1);
    (module, dir, core)
}

#[test]
fn codemode_shutdown_after_store_cut_revokes_a_held_worker() {
    let (module, dir, core) = shutdown_module("codemode-cut");
    let workers = module.codemode_pool.clone();
    assert_eq!(workers.stats().busy, 1);
    module.rt.store().cut();
    let (done, receiver) = std::sync::mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(module);
        done.send(()).unwrap();
    });
    let result = receiver.recv_timeout(Duration::from_secs(2));
    if result.is_err() {
        core.release();
    }
    dropping.join().unwrap();
    result.expect("shutdown must not wait on a held provider");
    assert_eq!(workers.stats().busy, 0);
    core.release();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn module_drop_releases_codemode_store_before_a_held_provider_returns() {
    let (module, dir, core) = shutdown_module("codemode-drop");
    let (done, receiver) = std::sync::mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(module);
        done.send(()).unwrap();
    });
    let result = receiver.recv_timeout(Duration::from_secs(2));
    if result.is_err() {
        core.release();
    }
    dropping.join().unwrap();
    result.expect("shutdown must not retain a store through a provider");
    let store = basal_core::Store::open(
        dir.join("basal.db"),
        basal_core::Durability { fullfsync: false },
    );
    core.release();
    assert!(store.is_ok(), "writer lease retained after drop");
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unhandled_rejections_warn_but_handled_rejections_and_logs_do_not() {
    let rig = rig("unhandled", false);
    let result = rig.fixture.module.handle_tool(&context(),ToolCallRequest::new("codemode",json!({
        "code":"Promise.reject('unobserved'); Promise.reject('handled').catch(() => {}); console.log('not a warning'); return 1;",
        "limits":{"output_bytes":1}})).with_call_key("unhandled"),Duration::from_secs(10)).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["value"], 1);
    assert_eq!(result["output"], "");
    let warnings = result["warnings"].as_array().unwrap();
    let rejections: Vec<_> = warnings
        .iter()
        .filter(|warning| warning["code"] == "unhandled_rejection")
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0]["message"], "unobserved");
    assert!(
        result["text"]
            .as_str()
            .unwrap()
            .contains("Warning: unobserved")
    );
}

#[test]
fn catalog_generation_tracks_content_and_schema_pins_refuse_changes() {
    use cortexkit_role_tool_provider::call::SchemaPin;
    let full = basal_module::tool::catalog(json!({"params":{}})).unwrap();
    let digest = basal_module::tool::catalog(json!({"params":{},"digest_only":true})).unwrap();
    assert_eq!(full["catalog_digest"], digest["catalog_digest"]);
    let excluded = basal_module::tool::catalog(json!({"params":{"exclude":["codemode"]}})).unwrap();
    assert_eq!(excluded["tools"], json!([]));
    assert_ne!(full["generation"], excluded["generation"]);
    let rig = rig("schema-pins", false);
    for (pin, code) in [
        (
            SchemaPin::new("codemode", "0".repeat(64), 1),
            "tool_schema_changed",
        ),
        (
            SchemaPin::new(
                "codemode",
                full["tools"][0]["schema_digest"].as_str().unwrap(),
                999,
            ),
            "tool_semantics_changed",
        ),
    ] {
        let error = rig
            .fixture
            .module
            .handle_tool(
                &context(),
                ToolCallRequest::new("codemode", json!({"code":"return 1;"}))
                    .with_call_key("pinned")
                    .with_schema_pin(&pin),
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(error.code, code);
    }
    assert!(rig.core.opens.lock().unwrap().is_empty());
    assert!(rig.fixture.module.codemode_pool.handouts().is_empty());
}

#[test]
fn forwarded_bind_identity_is_configuration_not_agent_authority() {
    let rig = rig("bind-identity", false);
    rig.core.release();
    let mut caller = context();
    caller.bind_identity =
        subc_protocol::BindIdentity::new("/other/workspace", "forged-agent", "agent:forged");
    caller.bind_identity.project_id = Some("pj-invoking-project".into());
    let expected = caller.bind_identity.clone();
    let result = rig
        .fixture
        .module
        .handle_tool(
            &caller,
            ToolCallRequest::new(
                "codemode",
                json!({
        "code":"return await tools.echo({n:1});"}),
            )
            .with_call_key("identity"),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(rig.core.identities.lock().unwrap().as_slice(), [expected]);
    assert_eq!(rig.core.opens.lock().unwrap()[0]["agent_id"], "agent");
    *rig.core.mismatch.lock().unwrap() = true;
    let result = rig
        .fixture
        .module
        .handle_tool(
            &caller,
            ToolCallRequest::new("codemode", json!({"code":"return 1;"}))
                .with_call_key("identity-mismatch"),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(result["error"]["code"], "scope_mismatch");
    assert_eq!(rig.core.identities.lock().unwrap().len(), 1);
}
