mod common;

use basal_host::CardDecision;
use basal_module::caller::Caller;
use basal_proto::JsonText;
use common::{Options, agent, fixture};
use serde_json::{Map, Value, json};

const OWNER: &str = "agent_47120287c700722b";
const SCRIPT: &str = "return 1;";
const VECTORS: &str = include_str!("../../../test-vectors/state-epoch/vectors.json");

fn install_approved(f: &common::Fixture, manifest: &str) {
    let reply = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script": SCRIPT, "manifest": manifest}),
        )
        .expect("valid vector manifest installs");
    let card = reply["card_id"].as_str().expect("install card id");
    assert!(f.consent.decide(card, CardDecision::Approve, "operator"));
    assert!(f.consent.undelivered().is_empty());
}

fn register_package(f: &common::Fixture, manifest: &str) {
    f.module
        .handle(
            &Caller::Core,
            "package.register",
            json!({"script": SCRIPT, "manifest": manifest}),
        )
        .expect("valid template manifest registers");
}

fn install_flow_case(case: &Value) -> Value {
    let name = case["name"].as_str().unwrap();
    let manifest = case["manifest"].as_str().unwrap();
    let manifest_value: Value = serde_json::from_str(manifest).unwrap();
    let f = fixture(name, Options::default());
    if let Some(previous) = case["previous_manifest"].as_str() {
        let previous_value: Value = serde_json::from_str(previous).unwrap();
        assert_eq!(previous_value["id"], manifest_value["id"]);
        install_approved(&f, previous);
        f.module
            .rt
            .admit_trigger(
                previous_value["id"].as_str().unwrap(),
                "previous-version",
                JsonText::null(),
            )
            .expect("previous approved version admits a run");
    }
    install_approved(&f, manifest);
    let preview = f
        .module
        .handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({
                "flow_id": manifest_value["id"],
                "version": manifest_value["version"],
                "trigger": {}
            }),
        )
        .expect("installed version can be previewed");
    json!({
        "name": name,
        "kind": "flow",
        "manifest": manifest,
        "previous_manifest": case["previous_manifest"],
        "reset": preview["state_reset"]
    })
}

fn refusal_case(case: &Value) -> Value {
    let name = case["name"].as_str().unwrap();
    let manifest = case["manifest"].as_str().unwrap();
    let f = fixture(name, Options::default());
    let refusal = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script": SCRIPT, "manifest": manifest}),
        )
        .expect_err("invalid epoch must be refused by flow.install");
    json!({
        "name": name,
        "kind": "refusal",
        "manifest": manifest,
        "previous_manifest": case["previous_manifest"],
        "refusal": {"code": refusal.code, "message": refusal.message}
    })
}

fn template_case(case: &Value) -> Value {
    let name = case["name"].as_str().unwrap();
    let previous = case["previous_manifest"].as_str().unwrap();
    let manifest = case["manifest"].as_str().unwrap();
    let previous_value: Value = serde_json::from_str(previous).unwrap();
    let manifest_value: Value = serde_json::from_str(manifest).unwrap();
    assert_eq!(previous_value["id"], manifest_value["id"]);
    let package = manifest_value["id"].as_str().unwrap();
    let current_version = manifest_value["version"].as_u64().unwrap();
    let f = fixture(name, Options::default());
    f.catalog.add_named_agent(OWNER, "Vector Owner");
    register_package(&f, previous);

    let instances = case["instances"].as_array().unwrap();
    let mut flow_ids = std::collections::BTreeMap::new();
    for instance in instances {
        let label = instance["name"].as_str().unwrap();
        let created = f
            .module
            .handle(
                &agent(OWNER),
                "flow.instantiate",
                json!({
                    "template": package,
                    "version": previous_value["version"],
                    "params": instance["params"],
                    "expires_in": "1h"
                }),
            )
            .expect("old template instance creates");
        flow_ids.insert(
            label.to_owned(),
            created["flow_id"].as_str().unwrap().to_owned(),
        );
    }

    register_package(&f, manifest);
    for instance in instances {
        let label = instance["name"].as_str().unwrap();
        let created = f
            .module
            .handle(
                &agent(OWNER),
                "flow.instantiate",
                json!({
                    "template": package,
                    "version": current_version,
                    "params": instance["params"],
                    "expires_in": "1h"
                }),
            )
            .expect("current template version instantiates");
        assert_eq!(created["flow_id"].as_str().unwrap(), flow_ids[label]);
        if instance["admit_version"].as_u64() == Some(current_version) {
            f.module
                .rt
                .admit_trigger(
                    &flow_ids[label],
                    &format!("admit-{label}"),
                    JsonText::null(),
                )
                .expect("instance admits current version");
        }
    }

    let package_state = f
        .module
        .handle(
            &Caller::Core,
            "package.get",
            json!({"package": package, "version": current_version}),
        )
        .expect("registered template can be read");
    let actual_epochs = package_state["instance_state_epochs"]
        .as_object()
        .expect("template instance epochs");
    let observed_epochs = instances
        .iter()
        .map(|instance| {
            let label = instance["name"].as_str().unwrap();
            let flow_id = &flow_ids[label];
            (
                label.to_owned(),
                actual_epochs
                    .get(flow_id)
                    .unwrap_or_else(|| panic!("no recorded epoch for {label}: {actual_epochs:?}"))
                    .clone(),
            )
        })
        .collect::<Map<String, Value>>();
    let target_epoch = manifest_value
        .get("state_epoch")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let reset_instances = instances
        .iter()
        .filter_map(|instance| {
            let label = instance["name"].as_str().unwrap();
            (observed_epochs[label].as_u64() != Some(target_epoch)).then(|| label.to_owned())
        })
        .collect::<Vec<_>>();
    let first_params = &instances[0]["params"];
    let preview = f
        .module
        .handle(
            &agent(OWNER),
            "flow.dry_run",
            json!({
                "flow_id": package,
                "version": current_version,
                "params": first_params,
                "trigger": {}
            }),
        )
        .expect("template version can be previewed");

    json!({
        "name": name,
        "kind": "template",
        "manifest": manifest,
        "previous_manifest": previous,
        "instances": instances,
        "reset": preview["state_reset"],
        "instance_state_epochs": observed_epochs,
        "reset_instances": reset_instances
    })
}

#[test]
fn state_epoch_vector_cases_match_basal_install_and_dry_run() {
    let vectors: Value = serde_json::from_str(VECTORS).expect("valid vector JSON");
    let cases = vectors["cases"].as_array().expect("vector cases");
    let generated_cases = cases
        .iter()
        .map(|case| {
            if case.get("refusal").is_some() {
                refusal_case(case)
            } else if case["kind"] == "template" {
                template_case(case)
            } else {
                install_flow_case(case)
            }
        })
        .collect::<Vec<_>>();
    let generated = json!({"schema_version": 1, "cases": generated_cases});
    assert_eq!(
        generated, vectors,
        "committed state epoch vectors differ from basal install and dry-run results"
    );
}

#[test]
fn explicit_epoch_is_not_affected_by_the_absent_epoch_default() {
    let vectors: Value = serde_json::from_str(VECTORS).expect("valid vector JSON");
    let case = vectors["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "one-to-two-resets")
        .expect("explicit epoch vector");
    let text = case["manifest"].as_str().unwrap();
    let manifest = basal_core::manifest::Manifest::parse(text).unwrap();
    assert_eq!(manifest.state_epoch, 2);
    assert_eq!(manifest.version, 2);
}
