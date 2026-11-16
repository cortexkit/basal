mod common;
use basal_module::caller::Caller;
use serde_json::{Value, json};

#[test]
fn attribution_read_pages_in_order_and_refuses_every_other_caller() {
    let f = common::attribution::setup("attribution-read");
    let reader = common::attribution::reader();
    let page = f
        .module
        .handle(&reader, "attribution.read", json!({"limit":2}))
        .unwrap();
    assert_eq!(page["through"], 4);
    assert_eq!(page["next"], 2);
    let last = f
        .module
        .handle(
            &reader,
            "attribution.read",
            json!({"after":page["next"],"limit":2}),
        )
        .unwrap();
    assert_eq!(last["facts"].as_array().unwrap().len(), 2);
    assert_eq!(last["next"], Value::Null);
    assert_eq!(last["facts"][0]["subject"]["kind"], "session");
    assert_eq!(last["facts"][1]["parent"], Value::Null);
    let all = f
        .module
        .handle(&reader, "attribution.read", json!({}))
        .unwrap();
    let mut joined = page["facts"].as_array().unwrap().clone();
    joined.extend(last["facts"].as_array().unwrap().clone());
    assert_eq!(all["facts"], json!(joined));
    for params in [
        json!({"after":-1}),
        json!({"after":"2"}),
        json!({"limit":0}),
        json!({"limit":501}),
        json!({"owner":"agent_attribution"}),
    ] {
        assert_eq!(
            f.module
                .handle(&reader, "attribution.read", params)
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    for caller in [
        Caller::Operator,
        Caller::Core,
        Caller::Local,
        Caller::Other("reserved:astrocyte".into()),
        common::agent("agent_attribution"),
        Caller::Module {
            module_id: "broca".into(),
        },
    ] {
        assert_eq!(
            f.module
                .handle(&caller, "attribution.read", json!({}))
                .unwrap_err()
                .code,
            "not_permitted"
        );
    }
}

#[test]
fn attribution_instance_creation_rolls_back_fact_and_outbox_together() {
    let f = common::attribution::setup("attribution-instance-rollback");
    let before = f
        .module
        .handle(
            &common::attribution::reader(),
            "attribution.read",
            json!({}),
        )
        .unwrap();
    f.module.rt.store().write(|tx| { tx.execute_batch("CREATE TRIGGER fail_instance AFTER INSERT ON template_instances BEGIN SELECT RAISE(ABORT,'synthetic rollback'); END;")?; Ok(()) }).unwrap();
    assert!(
        f.module
            .rt
            .instantiate(
                "attribution-template",
                1,
                "agent_attribution",
                &json!({"note":"second"}),
                "1h"
            )
            .is_err()
    );
    assert_eq!(
        f.module
            .handle(
                &common::attribution::reader(),
                "attribution.read",
                json!({})
            )
            .unwrap(),
        before
    );
    assert_eq!(f.module.rt.pending_bus_events(500).unwrap().len(), 4);
}
