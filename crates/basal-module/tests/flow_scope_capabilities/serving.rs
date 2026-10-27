use super::{common, wire::Fake};
use basal_host::subc_catalog::SubcCatalog;
use basal_host::transport::{Transport, WireError};
use basal_module::caller::Caller;
use basal_module::module::Hosts;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct ProviderProbe {
    wire: Arc<Fake>,
    store: Mutex<Option<PathBuf>>,
}

impl Transport for ProviderProbe {
    fn catalog(&self) -> Result<Value, WireError> {
        self.wire.catalog()
    }

    fn management(&self, module: &str, op: &str, params: Value) -> Result<Value, WireError> {
        self.wire.management(module, op, params)
    }

    fn tool(
        &self,
        module: &str,
        name: &str,
        arguments: Value,
        call_key: &str,
    ) -> Result<Value, WireError> {
        if let Some(path) = self.store.lock().unwrap().as_ref() {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
            assert!(
                conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").is_ok(),
                "provider metadata must be read before acquiring the SQLite writer"
            );
        }
        self.wire.tool(module, name, arguments, call_key)
    }
}

fn fixture(tag: &str) -> (common::Fixture, Arc<Fake>) {
    let wire = Fake::new();
    let probe = Arc::new(ProviderProbe {
        wire: wire.clone(),
        store: Mutex::new(None),
    });
    let f = common::fixture(
        tag,
        common::Options {
            hosts: Some(Hosts {
                transport: probe.clone(),
                host: Arc::new(basal_host::mock::MockHost::new()),
                catalog: Arc::new(SubcCatalog::new(probe.clone())),
                consent: Arc::new(basal_host::MockConsent::new()),
                hooks: Arc::new(basal_core::NoHooks),
            }),
            dry_run_max_fires: Some(0),
            ..common::Options::default()
        },
    );
    *probe.store.lock().unwrap() = Some(f.dir.join("basal.db"));
    (f, wire)
}

fn manifest(op: &str) -> Value {
    json!({"id":"serving-flow","version":1,"purpose":"Check flow admission",
        "trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"mock","op":op}]})
}

fn intake(
    f: &common::Fixture,
    method: &str,
    op: &str,
) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        method,
        json!({"script":"return 7;","manifest":manifest(op).to_string()}),
    )
}

fn dry_run(f: &common::Fixture) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        "flow.dry_run",
        json!({"flow_id":"serving-flow"}),
    )
}

fn management_ops(wire: &Fake, allowed: Value) {
    wire.catalog.lock().unwrap()["modules"][0]["flow_operations"] = allowed;
}

fn tool_answer(modes: Option<Value>) -> Value {
    let mut tool = json!({"name":"send","schema_digest":"0".repeat(64),"semantics":1,
        "capabilities":[],"input_schema":{"type":"object"},"emits_events":false});
    if let Some(modes) = modes {
        tool["modes"] = modes;
    }
    json!({"generation":"g","catalog_digest":"d","tools":[tool]})
}

#[test]
fn management_list_refuses_unlisted_ops_at_install_and_package_registration() {
    let (f, wire) = fixture("management-serving");
    management_ops(&wire, json!(["echo"]));
    for method in ["flow.install", "package.register"] {
        let error = intake(&f, method, "post").unwrap_err();
        assert_eq!(error.code, "flow_op_not_served", "{method}: {error}");
        assert_eq!(
            error.detail,
            Some(json!({"module":"mock","op":"post",
            "surface":"management","allowed":["echo"]}))
        );
        assert!(error.message.contains("mock.post"), "{error}");
        assert!(error.message.contains("echo"), "{error}");
    }
    assert!(f.module.rt.flow("serving-flow").unwrap().is_none());
    assert!(f.module.rt.get_package("serving-flow", 1).is_err());
    intake(&f, "flow.install", "echo").unwrap();
    dry_run(&f).unwrap();
}

#[test]
fn empty_management_list_refuses_every_op_and_dry_run_rechecks_the_list() {
    let (f, wire) = fixture("empty-management-serving");
    intake(&f, "flow.install", "echo").unwrap();
    management_ops(&wire, json!([]));
    for op in ["echo", "post"] {
        let error = intake(&f, "flow.install", op).unwrap_err();
        assert_eq!(error.code, "flow_op_not_served");
        assert_eq!(error.detail.as_ref().unwrap()["allowed"], json!([]));
    }
    let error = dry_run(&f).unwrap_err();
    assert_eq!(error.code, "flow_op_not_served");
    assert_eq!(error.detail.as_ref().unwrap()["op"], "echo");
    let stored: String = f
        .module
        .rt
        .store()
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT manifest FROM installs WHERE flow_id='serving-flow'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert!(basal_core::Manifest::parse_stored(&stored).is_ok());
}

#[test]
fn absent_management_list_uses_the_daemons_two_capability_rule_for_non_core_modules() {
    for provides in [
        json!(["call-modes/v1"]),
        json!(["flow-scopes/v1"]),
        json!([]),
    ] {
        let (f, wire) = fixture("absent-management-serving");
        wire.catalog.lock().unwrap()["modules"][0]["capabilities"]["provides"] = provides.clone();
        let installed = intake(&f, "flow.install", "echo");
        if provides.as_array().unwrap().is_empty() {
            let error = installed.unwrap_err();
            assert!(error.message.contains("FlowScopeUnsupported"), "{error}");
            assert!(f.module.rt.flow("serving-flow").unwrap().is_none());
        } else {
            installed.unwrap();
            dry_run(&f).unwrap();
        }
    }
}

#[test]
fn tool_modes_require_flow_even_when_management_flow_operations_is_empty() {
    for modes in [
        None,
        Some(json!([])),
        Some(json!(["classic"])),
        Some(json!(["codemode"])),
    ] {
        let (f, wire) = fixture("tool-not-serving");
        management_ops(&wire, json!([]));
        for method in ["flow.install", "package.register"] {
            wire.enqueue("tool.catalog", vec![Ok(tool_answer(modes.clone()))]);
            let error = intake(&f, method, "send").unwrap_err();
            assert_eq!(error.code, "flow_op_not_served", "{method}: {error}");
            assert_eq!(
                error.detail,
                Some(json!({"module":"mock","op":"send","surface":"tool"}))
            );
            assert!(error.message.contains("mock.send"), "{error}");
        }
        assert!(f.module.rt.flow("serving-flow").unwrap().is_none());
    }
    for modes in [json!(["flow"]), json!(["classic", "flow"])] {
        let (f, wire) = fixture("tool-serving");
        management_ops(&wire, json!([]));
        wire.enqueue("tool.catalog", vec![Ok(tool_answer(Some(modes)))]);
        intake(&f, "flow.install", "send").unwrap();
        dry_run(&f).unwrap();
    }
}

#[test]
fn dry_run_refuses_a_tool_that_stops_serving_flow() {
    let (f, wire) = fixture("tool-dry-serving");
    intake(&f, "flow.install", "send").unwrap();
    wire.enqueue(
        "tool.catalog",
        vec![Ok(tool_answer(Some(json!(["classic"]))))],
    );
    let error = dry_run(&f).unwrap_err();
    assert_eq!(error.code, "flow_op_not_served");
    assert_eq!(error.detail.unwrap()["surface"], "tool");
}

#[test]
fn unreadable_tool_catalog_fails_closed_with_a_readable_reason() {
    for (op, answer) in [
        (
            "role.describe",
            Err(WireError::TimedOut("provider offline".into())),
        ),
        (
            "tool.catalog",
            Err(WireError::TimedOut("catalog offline".into())),
        ),
        ("tool.catalog", Ok(json!({"tools":"not an array"}))),
    ] {
        let (f, wire) = fixture("unreadable-serving");
        for method in ["flow.install", "package.register"] {
            wire.enqueue(op, vec![answer.clone()]);
            let error = intake(&f, method, "send").unwrap_err();
            assert_eq!(error.code, "tool_contract_unreadable", "{method}: {error}");
            assert!(error.message.contains("mock.send"), "{error}");
            assert!(
                error.message.contains("offline") || error.message.contains("tool.catalog"),
                "{error}"
            );
        }
        assert!(f.module.rt.flow("serving-flow").unwrap().is_none());
        intake(&f, "flow.install", "send").unwrap();
        wire.enqueue(op, vec![answer]);
        let error = dry_run(&f).unwrap_err();
        assert_eq!(error.code, "tool_contract_unreadable");
    }
}

#[test]
fn template_dry_run_rechecks_management_allowlist_and_tool_modes() {
    for op in ["echo", "send"] {
        let (f, wire) = fixture("template-serving");
        let mut m = manifest(op);
        m["params"] = json!({"count":{"type":"integer","min":1,"max":2}});
        m["template"] = json!({"max_per_agent":1,"max_lifetime":"1h"});
        f.module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":"return 7;","manifest":m.to_string()}),
            )
            .unwrap();
        if op == "echo" {
            management_ops(&wire, json!([]));
        } else {
            wire.enqueue("tool.catalog", vec![Ok(tool_answer(None))]);
        }
        let error = f
            .module
            .handle(
                &common::agent("ag_synapse"),
                "flow.dry_run",
                json!({"flow_id":"serving-flow","params":{"count":1}}),
            )
            .unwrap_err();
        assert_eq!(error.code, "flow_op_not_served", "{op}: {error}");
        assert_eq!(error.detail.unwrap()["op"], op);
    }
}

#[test]
fn core_carrier_ops_ignore_even_an_explicit_empty_management_list() {
    let (f, wire) = fixture("carrier-serving");
    wire.catalog.lock().unwrap()["modules"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "module_id":"prefrontal-core","flow_operations":[],"capabilities":{"provides":[]},
            "roles":[{"role":"management_surface","operations":[
                {"name":"board.read","kind":"query"},{"name":"work.unsettled","kind":"query"}],
                "config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]
        }));
    let mut m = manifest("echo");
    m["ops"] = json!([{"module":"prefrontal-core","op":"board.read"},
        {"module":"prefrontal-core","op":"work.unsettled"}]);
    m["board_read"] = json!(["SYNAPSE"]);
    for method in ["flow.install", "package.register"] {
        let mut package = m.clone();
        if method == "package.register" {
            package["id"] = json!("carrier-package");
            package["board_read"] = json!(["$self"]);
        }
        f.module
            .handle(
                &Caller::Operator,
                method,
                json!({"script":"return 7;","manifest":package.to_string()}),
            )
            .unwrap();
    }
    dry_run(&f).unwrap();
    assert!(wire.calls("tool.catalog").is_empty());
}

#[test]
fn loop_rule_and_cost_policy_reuse_the_admission_tool_metadata() {
    let catalog = basal_host::MockCatalog::standard();
    catalog.set_tool_contract(
        "plexus",
        "pr.comment",
        json!({
            "modes":["flow"],"emits_events":false,"capabilities":[]
        }),
    );
    let mut m = common::events_manifest("reuse-serving");
    m["ops"] = json!([{"module":"plexus","op":"pr.comment"}]);
    let manifest = basal_core::Manifest::parse(&m.to_string()).unwrap();
    let warnings = basal_core::install::validate(
        &manifest,
        "operator",
        &catalog,
        &basal_core::ShellDenylist::default(),
        false,
    )
    .unwrap();
    assert!(warnings.iter().any(|warning| matches!(warning,
        basal_core::install::Warning::LoopExemptNoEvents { module, op }
            if module == "plexus" && op == "pr.comment")));
    assert_eq!(
        catalog.tool_contract_reads(),
        [("plexus".into(), "pr.comment".into())]
    );
    assert!(catalog.emits_events_reads().is_empty());
}
