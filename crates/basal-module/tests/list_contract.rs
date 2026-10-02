mod common;

use basal_core::Actor;
use basal_module::caller::Caller;
use common::{
    Fixture, Options, T0, admit, agent, events_manifest, fixture, install, install_approved,
};
use serde_json::{Value, json};

fn list(f: &Fixture, caller: &Caller, params: Value) -> Value {
    f.module.handle(caller, "flow.list", params).expect("list")
}

fn ids(raw: &Value) -> Vec<&str> {
    raw["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["flow_id"].as_str().unwrap())
        .collect()
}

fn populated(name: &str) -> Fixture {
    let f = fixture(name, Options::default());
    install_approved(&f, &agent("A"), "return 1;", &events_manifest("a"));
    install(&f, &agent("B"), "return 1;", &events_manifest("b"));
    install(&f, &Caller::Local, "return 1;", &events_manifest("local"));
    f
}

#[test]
fn list_authorization_rows() {
    let f = populated("list-auth");
    assert_eq!(
        ids(&list(&f, &Caller::Operator, json!({}))),
        ["a", "b", "local"]
    );
    assert_eq!(ids(&list(&f, &agent("B"), json!({}))), ["b"]);
    assert!(ids(&list(&f, &agent("C"), json!({}))).is_empty());
    for caller in [Caller::Core, Caller::Other("other".into())] {
        assert_eq!(
            f.module
                .handle(&caller, "flow.list", json!({}))
                .unwrap_err()
                .code,
            "not_permitted"
        );
    }
    for params in [
        Value::Null,
        json!({"owner":"A"}),
        json!({"flow_ids":[1]}),
        json!([]),
    ] {
        assert_eq!(
            f.module
                .handle(&Caller::Operator, "flow.list", params)
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
}

#[test]
fn list_agent_cannot_see_another_agents_flow() {
    let f = populated("list-agent");
    assert_eq!(ids(&list(&f, &agent("A"), json!({}))), ["a"]);
}

#[test]
fn list_local_cannot_see_an_agents_flow() {
    let f = populated("list-local");
    assert_eq!(ids(&list(&f, &Caller::Local, json!({}))), ["local"]);
}

#[test]
fn list_invisible_requested_ids_are_absent() {
    let f = populated("list-filter");
    assert_eq!(
        ids(&list(&f, &agent("A"), json!({"flow_ids":["b","missing"]}))),
        Vec::<&str>::new()
    );
    assert_eq!(
        ids(&list(
            &f,
            &agent("A"),
            json!({"flow_ids":["a","b","missing","a"]})
        )),
        ["a"]
    );
    assert_eq!(
        ids(&list(
            &f,
            &Caller::Operator,
            json!({"flow_ids":["b","missing"]})
        )),
        ["b"]
    );
    assert!(ids(&list(&f, &Caller::Local, json!({"flow_ids":[]}))).is_empty());
}

// Decode against the shape embedded in the relay documentation, including
// required nullable fields and the closed string sets.
fn matches_shape(value: &Value, shape: &Value, nullable: bool) {
    if nullable && value.is_null() {
        return;
    }
    match shape {
        Value::Object(fields) => {
            let object = value.as_object().expect("object");
            assert_eq!(object.len(), fields.len(), "exact documented fields");
            for (key, ty) in fields {
                matches_shape(
                    object.get(key).expect("required field"),
                    ty,
                    key == "disabled" || key == "last_run",
                );
            }
        }
        Value::Array(items) => {
            for item in value.as_array().expect("array") {
                matches_shape(item, &items[0], false);
            }
        }
        Value::String(ty) => assert!(
            ty.split('|').any(|t| match t {
                "integer" => value.as_i64().is_some(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "null" => value.is_null(),
                literal => value.as_str() == Some(literal),
            }),
            "{value} must match {ty}"
        ),
        _ => panic!("invalid documented shape"),
    }
}

#[test]
fn list_reply_decodes_against_documented_shape_and_health_state() {
    let f = populated("list-shape");
    let run = admit(&f, "a", "run");
    f.module.engine.run_until_idle(50).unwrap();
    let mut update = events_manifest("a");
    update["version"] = json!(2);
    install(&f, &agent("A"), "return 2;", &update);
    let doc = include_str!("../../../docs/ops.md");
    let section = doc
        .split("Reply (machine-checked by `list_contract`):\n```json\n")
        .nth(1)
        .expect("documented reply");
    let shape: Value = serde_json::from_str(section.split("\n```").next().unwrap()).unwrap();
    for actor in [
        None,
        Some(Actor::Agent("A".into())),
        Some(Actor::Operator("operator".into())),
        Some(Actor::Runtime),
        Some(Actor::Core),
    ] {
        f.module
            .rt
            .enable_flow_as("a", &Actor::Operator("operator".into()))
            .unwrap();
        if let Some(actor) = &actor {
            f.module.rt.disable_flow("a", actor, "stop").unwrap();
        }
        let raw = list(&f, &Caller::Operator, json!({}));
        matches_shape(&raw, &shape, false);
        assert_eq!(raw["as_of"], T0);
        let a = &raw["flows"][0];
        assert_eq!(a["approved_version"], 1);
        assert_eq!(a["pending_version"], 2);
        assert_eq!(a["last_run"]["run_id"], run);
        assert_eq!(a["last_run"]["state"], "succeeded");
        assert!(a["last_run"]["ended_at"].as_i64().unwrap() > 0);
        assert_eq!(a["needs_reconcile"], false);
        if let Some(actor) = &actor {
            let kind = match actor {
                Actor::Agent(_) => "owner",
                Actor::Operator(_) => "operator",
                // Core's own disable is its own kind, never an owner's.
                Actor::Core => "core",
                Actor::Runtime => "auto",
            };
            assert_eq!(a["disabled"], json!({"by":kind,"reason":"stop","at":T0}));
        } else {
            assert!(a["disabled"].is_null());
        }
        assert_eq!(raw["flows"][1]["approved_version"], Value::Null);
        assert_eq!(raw["flows"][1]["pending_version"], 1);
        let health = f
            .module
            .handle(&Caller::Core, "flow.health", json!({}))
            .unwrap();
        for (entry, h) in raw["flows"]
            .as_array()
            .unwrap()
            .iter()
            .zip(health["flows"].as_array().unwrap())
        {
            assert_eq!(entry["state"], h["state"]);
        }
        for key in shape["flows"][0].as_object().unwrap().keys() {
            let mut missing = raw.clone();
            missing["flows"][0].as_object_mut().unwrap().remove(key);
            assert!(std::panic::catch_unwind(|| matches_shape(&missing, &shape, false)).is_err());
        }
    }
}

#[test]
fn list_declined_card_and_reconciliation() {
    use basal_host::CardDecision;
    use basal_host::mock::Fault;
    let f = fixture("list-reconcile", Options::default());
    let declined = install(&f, &agent("A"), "return 1;", &events_manifest("declined"));
    assert!(f.consent.decide(
        declined["card_id"].as_str().unwrap(),
        CardDecision::Reject,
        "operator"
    ));
    let mut m = events_manifest("post");
    m["ops"] = json!([{"module":"mock", "op":"post"}]);
    let flow = install_approved(
        &f,
        &agent("A"),
        "return await ops.call('mock', 'post', {});",
        &m,
    );
    f.mock.inject(
        "mock",
        "post",
        &[Fault {
            proven_unsent: false,
            effect_applied: true,
        }],
    );
    admit(&f, &flow, "post");
    f.module.engine.run_until_idle(50).unwrap();
    let raw = list(&f, &agent("A"), json!({}));
    assert_eq!(raw["flows"][0]["state"], "unapproved");
    assert!(raw["flows"][0]["approved_version"].is_null());
    assert!(raw["flows"][0]["pending_version"].is_null());
    assert!(raw["flows"][1]["pending_version"].is_null());
    assert_eq!(raw["flows"][1]["needs_reconcile"], true);
    assert!(raw["flows"][1]["last_run"].is_null());
}
