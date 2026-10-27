//! Install and dry-run validation through the production catalog adapter.
mod common;
mod wire;

use std::sync::Arc;

use basal_host::subc_catalog::{DaemonCapabilities, SubcCatalog};
use basal_host::{MockConsent, mock::MockHost};
use basal_module::{caller::Caller, module::Hosts};
use common::{Fixture, Options};
use serde_json::{Value, json};
use subc_protocol::manifest::CAP_CATALOG_EVENTS_V1;

fn acknowledge(capabilities: &DaemonCapabilities, advertised: &[&str]) {
    let ack = serde_json::from_value(json!({
        "negotiated_ver": subc_protocol::PROTOCOL_VERSION,
        "subc_ops": [], "subc_capabilities": advertised,
        "storage": null, "machine_id": null,
    }))
    .unwrap();
    capabilities.observe_hello_ack(&ack);
}

fn entry() -> Value {
    json!({
        "module_id":"plexus",
        "capabilities":{"provides":["call-modes/v1"]},
        "events":[{"name":"github_pr_changed","version":1,"headers":["repo","project","pr"]}],
        "roles":[{"role":"tool_provider","tools":[{
            "name":"events_get","execution_mode":"pure","schema":{}
        }],"identity_scope":[],"concurrency":"serial","emits_push":false,"sub_supervises":false}]
    })
}

fn fixture(entry: Value, capabilities: Arc<DaemonCapabilities>) -> (Fixture, Arc<wire::Fake>) {
    let wire = wire::Fake::new();
    *wire.catalog.lock().unwrap() = json!({"modules":[entry]});
    let catalog = Arc::new(SubcCatalog::new(wire.clone()).with_daemon_capabilities(capabilities));
    let fixture = common::fixture(
        "catalog-events",
        Options {
            hosts: Some(Hosts {
                transport: wire.clone(),
                host: Arc::new(MockHost::new()),
                catalog,
                consent: Arc::new(MockConsent::new()),
                hooks: Arc::new(basal_core::NoHooks),
            }),
            ..Options::default()
        },
    );
    (fixture, wire)
}

fn manifest(name: &str, version: u32) -> Value {
    json!({
        "id":"catalog-event-flow","version":1,"purpose":"Validate a declared event",
        "trigger":{"events":[{"module":"plexus","name":name,"version":version}]},
        "ops":[{"module":"plexus","op":"events_get"}]
    })
}

fn install(f: &Fixture, name: &str, version: u32) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        "flow.install",
        json!({"script":"return 7;","manifest":manifest(name, version).to_string()}),
    )
}

fn dry_run(f: &Fixture) -> Result<Value, basal_module::ops::OpError> {
    f.module.handle(
        &Caller::Operator,
        "flow.dry_run",
        json!({"flow_id":"catalog-event-flow","trigger":{"event":{
            "module":"plexus","name":"github_pr_changed","version":1,
            "event_key":"0".repeat(64),"headers":{},"body":{}
        }}}),
    )
}

#[test]
fn catalog_events_require_an_acknowledged_daemon_capability() {
    let capabilities = Arc::new(DaemonCapabilities::default());
    let (f, _) = fixture(entry(), capabilities.clone());
    assert_eq!(
        install(&f, "github_pr_changed", 1).unwrap_err().code,
        "event_not_declared"
    );
    acknowledge(&capabilities, &["flow-scopes/v1"]);
    assert_eq!(
        install(&f, "github_pr_changed", 1).unwrap_err().code,
        "event_not_declared"
    );
    acknowledge(&capabilities, &[CAP_CATALOG_EVENTS_V1]);
    install(&f, "github_pr_changed", 1).unwrap();
    dry_run(&f).unwrap();
    acknowledge(&capabilities, &[]);
    let error = dry_run(&f).unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert!(error.message.contains("UnknownEvent"), "{error:?}");
}

#[test]
fn header_filters_require_declared_catalog_keys_at_install_and_dry_run() {
    let capabilities = Arc::new(DaemonCapabilities::default());
    acknowledge(&capabilities, &[CAP_CATALOG_EVENTS_V1]);
    let (f, wire) = fixture(entry(), capabilities);
    let mut filtered = manifest("github_pr_changed", 1);
    filtered["trigger"]["events"][0]["match"] = json!({"repo":"cortexkit/*"});
    f.module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({
                "script":"return 7;", "manifest":filtered.to_string()
            }),
        )
        .unwrap();
    assert!(dry_run(&f).is_ok());
    for headers in [Some(json!([])), Some(json!(["project"])), None] {
        let mut catalog = wire.catalog.lock().unwrap();
        let declaration = &mut catalog["modules"][0]["events"][0];
        if let Some(headers) = headers {
            declaration["headers"] = headers;
        } else {
            declaration.as_object_mut().unwrap().remove("headers");
        }
        drop(catalog);
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({
                    "script":"return 7;", "manifest":filtered.to_string()
                }),
            )
            .unwrap_err();
        assert_eq!(error.code, "event_header_not_declared");
        assert!(error.message.contains("repo"), "{error:?}");
        let error = dry_run(&f).unwrap_err();
        assert_eq!(error.code, "event_header_not_declared");
        assert!(error.message.contains("repo"), "{error:?}");
    }
}

#[test]
fn missing_empty_or_unmatched_catalog_events_are_refused() {
    let capabilities = Arc::new(DaemonCapabilities::default());
    acknowledge(&capabilities, &[CAP_CATALOG_EVENTS_V1]);
    for events in [None, Some(json!([]))] {
        let mut entry = entry();
        if let Some(events) = events {
            entry["events"] = events;
        } else {
            entry.as_object_mut().unwrap().remove("events");
        }
        let (f, _) = fixture(entry, capabilities.clone());
        assert_eq!(
            install(&f, "github_pr_changed", 1).unwrap_err().code,
            "event_not_declared"
        );
    }
    let (f, wire) = fixture(entry(), capabilities);
    for (name, version) in [("github_issue_changed", 1), ("github_pr_changed", 2)] {
        assert_eq!(
            install(&f, name, version).unwrap_err().code,
            "event_not_declared"
        );
    }
    install(&f, "github_pr_changed", 1).unwrap();
    for events in [
        None,
        Some(json!([])),
        Some(json!([
            {"name":"github_issue_changed","version":1,"headers":[]}
        ])),
        Some(json!([
            {"name":"github_pr_changed","version":2,"headers":[]}
        ])),
    ] {
        let mut catalog = wire.catalog.lock().unwrap();
        if let Some(events) = events {
            catalog["modules"][0]["events"] = events;
        } else {
            catalog["modules"][0]
                .as_object_mut()
                .unwrap()
                .remove("events");
        }
        drop(catalog);
        let error = dry_run(&f).unwrap_err();
        assert_eq!(error.code, "invalid_params");
        assert!(error.message.contains("UnknownEvent"), "{error:?}");
    }
}

#[test]
fn recorded_live_plexus_catalog_accepts_github_pr_changed_v1() {
    // Event declarations and daemon capabilities trimmed from the live catalog
    // recorded by ck 0.20.79 from daemon 34336c0e and plexus 2cb37682. Retain only
    // required CatalogEntry fields and unchanged events; private roles are empty.
    let mut recorded: Value =
        serde_json::from_str(include_str!("data/live-catalog-0.20.79.json")).unwrap();
    let capabilities = Arc::new(DaemonCapabilities::default());
    let advertised: Vec<&str> = recorded["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cap| cap.as_str().unwrap())
        .collect();
    acknowledge(&capabilities, &advertised);
    recorded["generation"] = json!(1);
    let decoded: subc_client_rs::consumer::CatalogList = serde_json::from_value(recorded).unwrap();
    let mut modules = serde_json::to_value(decoded.modules).unwrap();
    // Install requires a query that reads an event notice's body. Supply basal's
    // synthetic events_get declaration so this public fixture does not expose
    // plexus's private operation names, descriptions or input schemas.
    for module in modules.as_array_mut().unwrap() {
        if module["module_id"] == "plexus" {
            module["roles"] = entry()["roles"].clone();
            module["capabilities"] = entry()["capabilities"].clone();
        }
    }
    let (f, wire) = fixture(entry(), capabilities.clone());
    *wire.catalog.lock().unwrap() = json!({"modules": modules});
    let catalog = SubcCatalog::new(wire).with_daemon_capabilities(capabilities);
    use basal_host::Catalog;
    assert!(
        catalog
            .event("prefrontal-core", "digest_delivered", 1)
            .is_some()
    );
    let installed = install(&f, "github_pr_changed", 1).unwrap();
    println!("live catalog plexus.github_pr_changed v1 install: {installed}");
    for (name, version) in [("undeclared_event", 1), ("github_pr_changed", 99)] {
        let refused = install(&f, name, version).unwrap_err();
        assert_eq!(refused.code, "event_not_declared");
        println!(
            "live catalog plexus.{name} v{version} install: {}",
            refused.code
        );
    }
    let capture = dry_run(&f).unwrap();
    assert_eq!(capture["summary"]["runs"][0]["result"], 7);
    println!(
        "live catalog plexus.github_pr_changed v1 dry-run: {}",
        capture["summary"]["runs"][0]["result"]
    );
}
