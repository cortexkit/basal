mod common;

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use basal_core::NoHooks;
use basal_host::mock::MockHost;
use basal_host::transport::{Transport, WireError};
use basal_host::{MockCatalog, MockConsent};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use basal_module::pool::Binding;
use serde_json::{Value, json};
use subc_protocol::Principal;
use subc_protocol::manifest::ManagementOperationKind;

#[derive(Default)]
struct Provider {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
    routes: Mutex<Vec<String>>,
    returned: std::sync::atomic::AtomicUsize,
}

impl Provider {
    fn wait_entered(&self, count: usize) {
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(180), |s| s.0 < count)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "provider not entered: {} < {count}",
            state.0
        );
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

impl Transport for Provider {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(json!({}))
    }
    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        panic!("codemode must not use a management route")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("codemode must not use an unscoped route")
    }
    fn tool_for_flow(
        &self,
        key: &str,
        module: &str,
        op: &str,
        input: Value,
        _: &str,
    ) -> Result<Value, WireError> {
        assert!(key.starts_with("codemode:"));
        assert_eq!((module, op), ("mock", "echo"));
        self.routes.lock().unwrap().push(key.into());
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.changed.notify_all();
        state = self.changed.wait_while(state, |s| !s.1).unwrap();
        self.returned
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_all();
        drop(state);
        Ok(input)
    }
}

struct Rig {
    f: common::Fixture,
    host: MockHost,
    provider: Arc<Provider>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.provider.release();
    }
}

fn rig(tag: &str, missing_worker: bool) -> Rig {
    let host = MockHost::new();
    let provider = Arc::new(Provider::default());
    let consent = MockConsent::new();
    let mut f = common::fixture(
        tag,
        common::Options {
            worker_binary: missing_worker
                .then(|| std::env::temp_dir().join("no-such-codemode-worker")),
            hosts: Some(Hosts {
                host: Arc::new(host.clone()),
                transport: provider.clone(),
                catalog: Arc::new(MockCatalog::standard()),
                consent: Arc::new(consent.clone()),
                hooks: Arc::new(NoHooks),
            }),
            ..common::Options::default()
        },
    );
    f.mock = host.clone();
    f.consent = consent;
    Rig { f, host, provider }
}

fn run(rig: &Rig, id: &str, program: &str, catalog: Value) -> Value {
    admit(
        &rig.f.module,
        &rig.provider,
        request(&rig.host, id, program, catalog),
    )
}

fn admit(module: &basal_module::module::Module, provider: &Provider, request: Value) -> Value {
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(module.handle(&Caller::Core, "codemode.run", request));
        });
        let response = rx.recv_timeout(Duration::from_secs(4));
        if response.is_err() {
            provider.release();
        }
        response
            .expect("admission did not return independently of execution")
            .unwrap()
    })
}

fn request(host: &MockHost, id: &str, program: &str, catalog: Value) -> Value {
    host.set_scope_description(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        id,
        Ok(serde_json::from_value(json!({
            "status": "live", "scope_epoch": 19, "daemon_incarnation": "daemon:test",
            "owner_synced": true, "owner_configured": true,
            "scope": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"},
                "ref": id, "scope_epoch": 19, "kind": "head", "owner_authorized": true,
                "attributes": {"agent_id": "agent", "run_id": id}}
        }))
        .unwrap()),
    );
    json!({
        "run_id": id, "agent_id": "agent", "program": program, "catalog": catalog,
        "scope": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": id, "epoch": 19},
        "deadline_ms": common::T0 + 300_000
    })
}

fn result(rig: &Rig, id: &str) -> Value {
    rig.f
        .module
        .handle(&Caller::Core, "codemode.result", json!({"run_id": id}))
        .unwrap()
}

fn ended(rig: &Rig, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let value = result(rig, id);
        if value["status"] != "running" {
            return value;
        }
        assert!(Instant::now() < deadline, "run did not end: {value}");
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

fn catalog() -> Value {
    json!([{"name": "echo", "module": "mock", "op": "echo", "input_schema": {"type": "object"}}])
}

#[test]
fn codemode_manifest_declares_core_only_operation_kinds() {
    for (name, kind) in [
        ("codemode.run", ManagementOperationKind::Mutate),
        ("codemode.result", ManagementOperationKind::Query),
        ("codemode.cancel", ManagementOperationKind::Mutate),
    ] {
        let entry = basal_module::manifest::OPERATIONS
            .iter()
            .find(|e| e.0 == name)
            .expect("declared op");
        assert_eq!(entry.1, kind);
        assert!(entry.2.contains("core only"));
    }
}

#[test]
fn codemode_ops_refuse_every_non_core_caller_before_decoding() {
    let rig = rig("codemode-callers", false);
    for method in ["codemode.run", "codemode.result", "codemode.cancel"] {
        for caller in [
            Caller::Operator,
            common::agent("agent"),
            Caller::Other("other".into()),
            Caller::Local,
        ] {
            let error = rig.f.module.handle(&caller, method, json!({})).unwrap_err();
            assert_eq!(
                error.code,
                if caller == Caller::Local {
                    "operator_attestation_required"
                } else {
                    "not_permitted"
                },
                "{method}: {caller:?}"
            );
        }
    }
}

#[test]
fn codemode_run_returns_while_provider_is_held_and_other_ops_are_served() {
    let rig = rig("codemode-async", false);
    let admitted = std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let rig = &rig;
        scope.spawn(move || {
            let value = run(
                rig,
                "async",
                "console.log('before'); return await tools.echo({answer: 42});",
                catalog(),
            );
            let _ = tx.send(value);
        });
        rig.provider.wait_entered(1);
        let response = rx.recv_timeout(Duration::from_secs(2));
        if response.is_err() {
            rig.provider.release();
        }
        response.expect("admission must return without waiting for the provider")
    });
    assert_eq!(admitted["status"], "running");
    let pending = result(&rig, "async");
    assert_eq!(pending["status"], "running");
    assert_eq!(
        pending["calls"],
        json!([{"tool": "echo", "outcome": "pending"}])
    );
    for key in ["value", "error", "description"] {
        assert!(!pending.as_object().unwrap().contains_key(key));
    }
    assert_eq!(pending["output"], "before\n");
    rig.f
        .module
        .handle(&Caller::Core, "flow.health", json!({}))
        .unwrap();
    rig.f.clock.advance(37);
    rig.provider.release();
    let completed = ended(&rig, "async");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["value"], json!({"answer": 42}));
    assert_eq!(
        completed["calls"],
        json!([{"tool": "echo", "outcome": "ok", "duration_ms": 37}])
    );
    assert!(!completed.as_object().unwrap().contains_key("error"));
    assert!(
        !completed["calls"][0]
            .as_object()
            .unwrap()
            .contains_key("code")
    );
}

#[test]
fn codemode_cancel_unknown_terminal_and_running_and_hide_flow_ids() {
    let rig = rig("codemode-cancel", false);
    for op in ["codemode.result", "codemode.cancel"] {
        assert_eq!(
            rig.f
                .module
                .handle(&Caller::Core, op, json!({"run_id": "unknown"}))
                .unwrap_err()
                .code,
            "unknown_run"
        );
    }
    let flow = common::install_approved(
        &rig.f,
        &Caller::Operator,
        "return 1;",
        &common::events_manifest("flow-only"),
    );
    let flow_id = common::admit(&rig.f, &flow, "trigger");
    assert_eq!(
        rig.f
            .module
            .handle(&Caller::Core, "codemode.result", json!({"run_id": flow_id}))
            .unwrap_err()
            .code,
        "unknown_run"
    );
    run(&rig, "terminal", "return null;", json!([]));
    let stored = ended(&rig, "terminal");
    assert_eq!(stored["status"], "completed");
    assert!(stored.as_object().unwrap().contains_key("value"));
    assert_eq!(
        rig.f
            .module
            .handle(
                &Caller::Core,
                "codemode.cancel",
                json!({"run_id": "terminal"})
            )
            .unwrap(),
        stored
    );
    run(
        &rig,
        "cancel",
        "console.log('kept'); await tools.echo({});",
        catalog(),
    );
    rig.provider.wait_entered(1);
    let cancelled = rig
        .f
        .module
        .handle(
            &Caller::Core,
            "codemode.cancel",
            json!({"run_id": "cancel"}),
        )
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["output"], "kept\n");
    assert_eq!(
        cancelled["calls"],
        json!([{"tool": "echo", "outcome": "outcome_unknown", "code": "no_outcome", "duration_ms": 0}])
    );
    for key in ["value", "error", "description"] {
        assert!(!cancelled.as_object().unwrap().contains_key(key));
    }
    rig.provider.release();
    assert_eq!(result(&rig, "cancel"), cancelled);
}

#[test]
fn codemode_workers_are_separate_from_flow_pool_and_never_shared() {
    let rig = rig("codemode-pools", false);
    let before = rig.f.module.pool.stats();
    run(&rig, "one", "await tools.echo({}); return 1;", catalog());
    run(&rig, "two", "await tools.echo({}); return 2;", catalog());
    rig.provider.wait_entered(2);
    assert_eq!(rig.f.module.pool.stats(), before);
    let workers = rig.f.module.codemode_pool.handouts();
    assert_eq!(workers.len(), 2);
    assert_ne!(workers[0].pid, workers[1].pid);
    assert!(workers.iter().all(|w| w.binding == Binding::Codemode));
    rig.provider.release();
    assert_eq!(ended(&rig, "one")["value"], 1);
    assert_eq!(ended(&rig, "two")["value"], 2);
    run(&rig, "three", "return 3;", json!([]));
    assert_eq!(ended(&rig, "three")["value"], 3);
    let workers = rig.f.module.codemode_pool.handouts();
    assert_eq!(workers.len(), 3);
    assert!(workers[..2].iter().all(|w| w.pid != workers[2].pid));
    assert_eq!(rig.f.module.pool.stats(), before);
}

#[test]
fn codemode_pool_retires_even_an_unused_lease() {
    let rig = rig("codemode-fresh", false);
    drop(
        rig.f
            .module
            .codemode_pool
            .acquire(Binding::Codemode)
            .unwrap(),
    );
    assert_eq!(rig.f.module.codemode_pool.stats().live, 0);
    drop(
        rig.f
            .module
            .codemode_pool
            .acquire(Binding::Codemode)
            .unwrap(),
    );
    let workers = rig.f.module.codemode_pool.handouts();
    assert_ne!(workers[0].pid, workers[1].pid);
    assert_eq!(rig.f.module.codemode_pool.stats().live, 0);
}

#[test]
fn codemode_spawn_failure_is_failed_worker_lost() {
    let rig = rig("codemode-spawn-failure", true);
    run(&rig, "spawn", "return 1;", json!([]));
    let failed = ended(&rig, "spawn");
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["code"], "worker_lost");
    assert!(!failed["error"]["message"].as_str().unwrap().is_empty());
    assert!(rig.f.module.codemode_pool.handouts().is_empty());
}

fn shutdown_module(
    tag: &str,
) -> (
    basal_module::module::Module,
    std::path::PathBuf,
    MockHost,
    Arc<Provider>,
) {
    use basal_module::module::{Module, ModuleConfig};
    use basal_module::pool::ProcessSpawner;
    let dir = common::scratch(tag);
    let host = MockHost::new();
    let provider = Arc::new(Provider::default());
    let pool = common::pool_config(&common::Options::default());
    let module = Module::start(
        ModuleConfig {
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
            host: Arc::new(host.clone()),
            transport: provider.clone(),
            catalog: Arc::new(MockCatalog::standard()),
            consent: Arc::new(MockConsent::new()),
            hooks: Arc::new(NoHooks),
        },
        Arc::new(ProcessSpawner::new(&pool)),
    )
    .unwrap();
    (module, dir, host, provider)
}

#[test]
fn codemode_shutdown_after_store_cut_revokes_a_held_worker() {
    let (module, dir, host, provider) = shutdown_module("codemode-shutdown-cut");
    admit(
        &module,
        &provider,
        request(&host, "cut", "return await tools.echo({});", catalog()),
    );
    provider.wait_entered(1);
    let workers = module.codemode_pool.clone();
    assert_eq!(workers.stats().busy, 1);
    module.rt.store().cut();
    let (tx, rx) = std::sync::mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(module);
        let _ = tx.send(());
    });
    let returned = rx.recv_timeout(Duration::from_secs(2));
    if returned.is_err() {
        provider.release();
    }
    dropping.join().unwrap();
    returned.expect("shutdown after a storage cut must not wait for the provider");
    let deadline = Instant::now() + Duration::from_secs(2);
    while workers.stats().busy != 0 && Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(10));
    }
    let busy = workers.stats().busy;
    provider.release();
    assert_eq!(
        busy, 0,
        "shutdown must revoke the worker without waiting for the provider"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn module_drop_releases_codemode_store_before_a_held_provider_returns() {
    let (module, dir, host, provider) = shutdown_module("codemode-shutdown-held");
    admit(
        &module,
        &provider,
        request(&host, "held", "return await tools.echo({});", catalog()),
    );
    provider.wait_entered(1);
    let (tx, rx) = std::sync::mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(module);
        let _ = tx.send(());
    });
    let returned = rx.recv_timeout(Duration::from_secs(2));
    if returned.is_err() {
        provider.release();
    }
    dropping.join().expect("module shutdown panicked");
    returned.expect("module drop must not wait for the held provider");
    // Opening before releasing the provider proves neither a driver nor a
    // detached tool thread retains the previous process's writer lease.
    let store = basal_core::Store::open(
        dir.join("basal.db"),
        basal_core::Durability { fullfsync: false },
    );
    provider.release();
    let store = store.expect("module drop returned while a thread still held the store");
    let state = provider.state.lock().unwrap();
    let (state, timeout) = provider
        .changed
        .wait_timeout_while(state, Duration::from_secs(2), |_| {
            provider.returned.load(std::sync::atomic::Ordering::SeqCst) == 0
        })
        .unwrap();
    assert!(
        !timeout.timed_out(),
        "provider did not return after release"
    );
    drop(state);
    store
        .read(|conn| {
            let basal_core::codemode::store::Lookup::Found(run) =
                basal_core::codemode::store::lookup(conn, "held")?
            else {
                panic!("missing run")
            };
            assert_eq!(run.status, basal_core::codemode::store::Status::Cancelled);
            assert!(run.value.is_none());
            Ok(())
        })
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn codemode_linux_worker_address_space_equals_protocol_limit() {
    let rig = rig("codemode-rlimit", false);
    run(&rig, "limit", "return await tools.echo({});", catalog());
    rig.provider.wait_entered(1);
    assert_eq!(result(&rig, "limit")["status"], "running");
    let pid = rig.f.module.codemode_pool.handouts()[0].pid;
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // Read the live child directly; /proc may be mounted in a different PID
    // namespace by the test runner. A null new-limit pointer cannot change it.
    let read = unsafe {
        libc::prlimit(
            pid as libc::pid_t,
            libc::RLIMIT_AS,
            std::ptr::null(),
            &mut limits,
        )
    };
    assert_eq!(
        read,
        0,
        "reading child limit: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(limits.rlim_cur, basal_proto::CODEMODE_ADDRESS_SPACE_BYTES);
    assert_eq!(limits.rlim_max, basal_proto::CODEMODE_ADDRESS_SPACE_BYTES);
    rig.provider.release();
    assert_eq!(ended(&rig, "limit")["status"], "completed");
}

fn completed(program: &str, expected: Value) {
    let rig = rig("codemode-within-budget", false);
    run(&rig, "budget", program, json!([]));
    let value = ended(&rig, "budget");
    assert_eq!(value["status"], "completed", "{value}");
    assert_eq!(value["value"], expected);
}

fn exhausted(program: &str, status: &str) {
    let rig = rig("codemode-exceed-budget", false);
    run(&rig, "budget", program, json!([]));
    let value = ended(&rig, "budget");
    assert_eq!(value["status"], status, "{value}");
    assert_eq!(value["error"]["code"], status);
    assert!(!value["error"]["message"].as_str().unwrap().is_empty());
    assert!(!value.as_object().unwrap().contains_key("value"));
}

#[test]
fn codemode_js_cpu_within_budget_completes_end_to_end() {
    completed(
        "let s = 0; for (let i=0; i<100000; i++) s += i; return s;",
        json!(4_999_950_000u64),
    );
}

#[test]
fn codemode_js_cpu_exceeding_budget_is_exact_end_to_end() {
    exhausted("while (true) {}", "budget_exhausted:js_cpu");
}

#[test]
fn codemode_memory_within_budget_completes_end_to_end() {
    completed(
        "return new ArrayBuffer(1024 * 1024).byteLength;",
        json!(1_048_576),
    );
}

#[test]
fn codemode_memory_exceeding_budget_is_exact_even_when_caught_end_to_end() {
    exhausted(
        "try { new ArrayBuffer(80 * 1024 * 1024); } catch (_) {} return 4;",
        "budget_exhausted:memory",
    );
}

#[test]
fn codemode_stack_within_budget_completes_end_to_end() {
    completed(
        "function f(n) { return n ? f(n-1) + 1 : 0; } return f(10);",
        json!(10),
    );
}

#[test]
fn codemode_stack_exceeding_budget_is_exact_even_when_caught_end_to_end() {
    exhausted(
        "function f() { return f(); } try { f(); } catch (_) {} return 4;",
        "budget_exhausted:stack",
    );
}

const OPS_DOC: &str = include_str!("../../../docs/ops.md");

/// The doc text from `## <heading>` up to the next second-level heading.
fn doc_section(heading: &str) -> Option<&'static str> {
    let start = OPS_DOC.find(&format!("\n## {heading}\n"))? + 1;
    let rest = &OPS_DOC[start..];
    Some(&rest[..rest[3..].find("\n## ").map_or(rest.len(), |end| end + 3)])
}

#[test]
fn ops_doc_documents_codemode_ops_as_core_only_against_operations() {
    use basal_module::manifest::OPERATIONS;
    // Every op-named section of the doc names a served operation.
    for line in OPS_DOC.lines() {
        if let Some(heading) = line.strip_prefix("## ")
            && !heading.contains(' ')
            && heading.contains('.')
        {
            assert!(
                OPERATIONS.iter().any(|(name, _, _)| *name == heading),
                "docs/ops.md documents {heading}, which OPERATIONS does not serve"
            );
        }
    }
    // The agent-relayed list names only served operations, and no codemode op.
    let relayed = OPS_DOC
        .split("Core's scoped relay exposes these five operations: ")
        .nth(1)
        .and_then(|rest| rest.split(". ").next())
        .expect("the relayed operations are listed");
    let relayed: Vec<&str> = relayed.split('`').skip(1).step_by(2).collect();
    assert_eq!(relayed.len(), 5, "{relayed:?}");
    for name in &relayed {
        assert!(
            OPERATIONS.iter().any(|(served, _, _)| served == name),
            "{name}"
        );
        assert!(!name.starts_with("codemode."), "{name} must not be relayed");
    }
    // Each codemode op has its own section saying it is core-only and never
    // agent-relayed, with the kind OPERATIONS declares.
    let codemode: Vec<_> = OPERATIONS
        .iter()
        .filter(|(name, _, _)| name.starts_with("codemode."))
        .collect();
    assert_eq!(codemode.len(), 3);
    let overview = doc_section("Codemode operations").expect("codemode overview");
    for (name, kind, description) in codemode {
        assert!(description.contains("core only"), "{name}: {description}");
        let section = doc_section(name).unwrap_or_else(|| panic!("{name} is undocumented"));
        assert!(
            section.contains("\nCore-only, not agent-relayed."),
            "{name}: {section}"
        );
        let kind = format!("Kind: {}.", format!("{kind:?}").to_lowercase());
        assert!(section.contains(&kind), "{name} must document {kind}");
        assert!(section.contains("[codemode result](#codemode-result)"));
        assert!(overview.contains(&format!("`{name}`")), "{name}");
    }
    assert!(overview.contains("are core-only and not agent-relayed"));
    for code in ["operator_attestation_required", "not_permitted"] {
        assert!(overview.contains(&format!("`{code}`")), "{code}");
    }
    let run = doc_section("codemode.run").unwrap();
    assert!(run.contains(r#""description?":"string""#));
}

/// Checks `value` against a documented shape. A key ending in `?` is
/// optional and, when present, must still match its type, so a documented
/// optional key is never null unless its type allows it.
fn conforms(value: &Value, shape: &Value) -> Result<(), String> {
    match shape {
        Value::Object(fields) => {
            let object = value
                .as_object()
                .ok_or(format!("{value} is not an object"))?;
            for key in object.keys() {
                if !fields.contains_key(key) && !fields.contains_key(&format!("{key}?")) {
                    return Err(format!("undocumented key {key}"));
                }
            }
            for (key, ty) in fields {
                match (key.strip_suffix('?'), object.get(key.trim_end_matches('?'))) {
                    (_, Some(field)) => conforms(field, ty).map_err(|e| format!("{key}: {e}"))?,
                    (Some(_), None) => {}
                    (None, None) => return Err(format!("missing key {key}")),
                }
            }
            Ok(())
        }
        Value::Array(items) => value
            .as_array()
            .ok_or(format!("{value} is not an array"))?
            .iter()
            .try_for_each(|item| conforms(item, &items[0])),
        Value::String(ty) => ty
            .split('|')
            .any(|t| match t {
                "any" => true,
                "integer" => value.as_i64().is_some(),
                "string" => value.is_string(),
                "object" => value.is_object(),
                literal => value.as_str() == Some(literal),
            })
            .then_some(())
            .ok_or(format!("{value} must match {ty}")),
        _ => Err("invalid documented shape".into()),
    }
}

fn documented_result_shape() -> Value {
    let block = OPS_DOC
        .split(
            "Result (machine-checked by `codemode_results_decode_against_documented_shape`):\n```json\n",
        )
        .nth(1)
        .expect("documented codemode result");
    serde_json::from_str(block.split("\n```").next().unwrap()).unwrap()
}

fn alternatives(ty: &Value) -> std::collections::BTreeSet<String> {
    ty.as_str().unwrap().split('|').map(str::to_owned).collect()
}

/// Every status and outcome basal can report. The matches have no wildcard,
/// so a new variant does not compile until it is listed here.
fn every_status_and_outcome() -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    use basal_core::codemode::store::{Budget, Outcome, Status};
    let statuses = [
        Status::Running,
        Status::Completed,
        Status::Failed,
        Status::BudgetExhausted(Budget::JsCpu),
        Status::BudgetExhausted(Budget::Memory),
        Status::BudgetExhausted(Budget::Stack),
        Status::BudgetExhausted(Budget::Wall),
        Status::BudgetExhausted(Budget::ToolCalls),
        Status::Cancelled,
        Status::Interrupted,
    ];
    for status in statuses {
        match status {
            Status::Running
            | Status::Completed
            | Status::Failed
            | Status::BudgetExhausted(
                Budget::JsCpu | Budget::Memory | Budget::Stack | Budget::Wall | Budget::ToolCalls,
            )
            | Status::Cancelled
            | Status::Interrupted => {}
        }
    }
    let outcomes = [
        Outcome::Pending,
        Outcome::Ok,
        Outcome::Error,
        Outcome::Refused,
        Outcome::ConsentUnavailable,
        Outcome::ToolUnavailable,
        Outcome::OutcomeUnknown,
        Outcome::Cancelled,
    ];
    for outcome in outcomes {
        match outcome {
            Outcome::Pending
            | Outcome::Ok
            | Outcome::Error
            | Outcome::Refused
            | Outcome::ConsentUnavailable
            | Outcome::ToolUnavailable
            | Outcome::OutcomeUnknown
            | Outcome::Cancelled => {}
        }
    }
    (
        statuses.iter().map(|s| s.as_str().to_owned()).collect(),
        outcomes.iter().map(|o| o.as_str().to_owned()).collect(),
    )
}

#[test]
fn codemode_results_decode_against_documented_shape() {
    let shape = documented_result_shape();
    let (statuses, outcomes) = every_status_and_outcome();
    assert_eq!(alternatives(&shape["status"]), statuses);
    assert_eq!(alternatives(&shape["calls"][0]["outcome"]), outcomes);
    // The call outcomes table has a row for every outcome and no other.
    let table = doc_section("Codemode result")
        .unwrap()
        .split("### Call outcomes")
        .nth(1)
        .expect("call outcomes table");
    let rows: std::collections::BTreeSet<String> = table
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .map(|rest| rest[..rest.find('`').unwrap()].to_owned())
        .collect();
    assert_eq!(rows, outcomes);

    let rig = rig("codemode-doc-shape", false);
    // A refused call that the program catches, truncated output and a
    // description: every optional key but `error`.
    let mut described = request(
        &rig.host,
        "described",
        "console.log('kept'); console.log('dropped line'); \
         try { await tools.echo(5); } catch (e) { return [e.code, e.tool, e.outcome]; }",
        catalog(),
    );
    described["description"] = json!("Checks the documented shape");
    described["limits"] = json!({"output_bytes": 8});
    admit(&rig.f.module, &rig.provider, described);
    let described = ended(&rig, "described");
    assert_eq!(described["status"], "completed", "{described}");
    assert_eq!(
        described["value"],
        json!(["invalid_input", "echo", "refused"])
    );
    assert_eq!(described["description"], "Checks the documented shape");
    assert_eq!(described["output"], "kept\n");
    assert_eq!(described["warnings"][0]["code"], "output_truncated");
    assert_eq!(
        described["calls"],
        json!([{"tool": "echo", "outcome": "refused", "code": "invalid_input"}])
    );

    run(&rig, "failed", "throw new Error('boom');", json!([]));
    let failed = ended(&rig, "failed");
    assert_eq!(failed["error"]["code"], "script", "{failed}");

    run(&rig, "held", "return await tools.echo({});", catalog());
    rig.provider.wait_entered(1);
    let running = result(&rig, "held");
    assert_eq!(running["status"], "running");
    let cancelled = rig
        .f
        .module
        .handle(&Caller::Core, "codemode.cancel", json!({"run_id": "held"}))
        .unwrap();
    assert_eq!(cancelled["calls"][0]["code"], "no_outcome", "{cancelled}");

    run(&rig, "ok", "return await tools.echo({n: 1});", catalog());
    rig.provider.release();
    let ok = ended(&rig, "ok");
    assert_eq!(ok["calls"][0]["outcome"], "ok", "{ok}");

    for value in [&described, &failed, &running, &cancelled, &ok] {
        conforms(value, &shape).unwrap_or_else(|e| panic!("{e}: {value}"));
        // Absent optional keys are omitted, never null.
        for key in ["value", "error", "description"] {
            assert_ne!(value.get(key), Some(&Value::Null), "{key}: {value}");
        }
    }
    for value in [&failed, &running, &cancelled, &ok] {
        assert!(value.get("description").is_none(), "{value}");
    }
    // The check rejects a null optional key, an undocumented key and a
    // missing required key, so the passes above are not vacuous.
    let mut null_description = described.clone();
    null_description["description"] = Value::Null;
    assert!(conforms(&null_description, &shape).is_err());
    let mut extra = described.clone();
    extra["extra"] = json!(1);
    assert!(conforms(&extra, &shape).is_err());
    let mut missing = described.clone();
    missing.as_object_mut().unwrap().remove("output");
    assert!(conforms(&missing, &shape).is_err());
}
