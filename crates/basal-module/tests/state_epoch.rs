mod common;

use basal_core::Admission;
use basal_module::caller::Caller;
use basal_proto::JsonText;
use common::{Options, agent, events_manifest, fixture, install_approved};
use serde_json::{Value, json};

const OWNER: &str = "agent_47120287c700722b";

#[test]
fn old_worker_finishes_its_kv_writes_before_new_epoch_admission() {
    use basal_core::RunState;
    use std::time::Duration;
    let f = fixture("epoch-worker", Options::with_worker());
    let mut m = events_manifest("values");
    install_approved(
        &f,
        &Caller::Operator,
        "await ops.call('mock', 'echo', {gate:'hold'}); await kv.set('late', 99); return 1;",
        &m,
    );
    let old = common::admit(&f, "values", "old");
    f.module.engine.pass().unwrap();
    assert!(f.module.engine.wait_active(&old, Duration::from_secs(60)));
    m["version"] = json!(2);
    m["state_epoch"] = json!(2);
    install_approved(&f, &Caller::Operator, "return await kv.get('late');", &m);
    assert_eq!(
        f.module
            .rt
            .admit_trigger("values", "new", JsonText::null())
            .unwrap(),
        Admission::StateBusy
    );
    f.mock.open_gate("hold");
    f.module.engine.run_until_idle(50).unwrap();
    let finished = f.module.rt.run(&old).unwrap();
    assert_eq!(finished.flow_version, Some(1));
    assert_eq!(finished.state, RunState::Succeeded);
    let keys = || {
        f.module
            .rt
            .store()
            .read(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM kv WHERE flow_id='values'", [], |r| {
                        r.get::<_, usize>(0)
                    })?,
                )
            })
            .unwrap()
    };
    assert_eq!(keys(), 1, "v1 wrote after v2 was approved");
    let new = common::admit(&f, "values", "new");
    assert_eq!(keys(), 0, "v2 resets at admission, not worker startup");
    f.module.engine.run_until_idle(50).unwrap();
    let finished = f.module.rt.run(&new).unwrap();
    assert_eq!(finished.flow_version, Some(2));
    assert_eq!(finished.state, RunState::Succeeded);
    assert_eq!(finished.result.as_ref().unwrap().as_str(), "null");
}

#[test]
fn flow_epoch_list_health_and_dry_run_report_recorded_state() {
    let f = fixture("epoch-ops", Options::default());
    let m = events_manifest("values");
    install_approved(&f, &Caller::Operator, "return 1;", &m);
    let list = || {
        f.module
            .handle(&Caller::Operator, "flow.list", json!({}))
            .unwrap()
    };
    let health = || {
        f.module
            .handle(&Caller::Core, "flow.health", json!({}))
            .unwrap()
    };
    let dry = |version| {
        f.module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":"values","version":version,"trigger":{}}),
            )
            .unwrap()
    };
    assert_eq!(list()["flows"][0]["state_epoch"], 1);
    assert_eq!(health()["flows"][0]["state_epoch"], 1);
    assert!(health()["flows"][0]["last_state_reset_at"].is_null());
    assert_eq!(dry(1)["state_reset"], false);
    let mut m = m;
    m["version"] = json!(2);
    m["state_epoch"] = json!(2);
    install_approved(&f, &Caller::Operator, "return 1;", &m);
    assert_eq!(dry(2)["state_reset"], true);
    assert_eq!(
        list()["flows"][0]["state_epoch"],
        1,
        "dry run does not reset real state"
    );
    assert!(matches!(
        f.module
            .rt
            .admit_trigger("values", "new", JsonText::null())
            .unwrap(),
        Admission::Admitted { .. }
    ));
    assert_eq!(list()["flows"][0]["state_epoch"], 2);
    assert_eq!(health()["flows"][0]["state_epoch"], 2);
    assert_eq!(
        health()["flows"][0]["last_state_reset_at"],
        "2026-05-01T00:00:00Z"
    );
    assert_eq!(dry(2)["state_reset"], false);
    assert_eq!(
        dry(1)["state_reset"],
        true,
        "rollback preview uses equality"
    );
}

#[test]
fn template_dry_run_and_package_get_show_every_instance_epoch() {
    let f = fixture("epoch-template-ops", Options::default());
    f.catalog.add_named_agent(OWNER, "Owner");
    let mut m = events_manifest("watch");
    m["sinks"][0]["agent"] = json!("$self");
    m["params"] = json!({"note":{"type":"string","max_bytes":32}});
    m["template"] = json!({"max_per_agent":2,"max_live":2,"max_lifetime":"1d"});
    let register = |m: &Value| {
        f.module
            .handle(
                &Caller::Core,
                "package.register",
                json!({"script":"return 1;","manifest":m.to_string()}),
            )
            .unwrap()
    };
    register(&m);
    let get = |version| {
        f.module
            .handle(
                &Caller::Core,
                "package.get",
                json!({"package":"watch","version":version}),
            )
            .unwrap()
    };
    assert_eq!(get(1)["state_epoch"], 1);
    assert!(get(1).get("instance_state_epochs").is_none());
    let create = |version, note| {
        f.module.handle(&agent(OWNER), "flow.instantiate", json!({"template":"watch","version":version,"params":{"note":note},"expires_in":"1h"})).unwrap()
    };
    let a = create(1, "a")["flow_id"].as_str().unwrap().to_owned();
    let b = create(1, "b")["flow_id"].as_str().unwrap().to_owned();
    m["version"] = json!(2);
    m["state_epoch"] = json!(2);
    register(&m);
    create(2, "a");
    create(2, "b");
    assert_eq!(get(2)["state_epoch"], 2);
    assert_eq!(get(2)["instance_state_epochs"], json!({&a:1,&b:1}));
    let dry = || {
        f.module
            .handle(
                &agent(OWNER),
                "flow.dry_run",
                json!({"flow_id":"watch","version":2,"params":{"note":"preview"},"trigger":{}}),
            )
            .unwrap()
    };
    assert_eq!(dry()["state_reset"], true);
    f.module
        .rt
        .admit_trigger(&a, "first-a", JsonText::null())
        .unwrap();
    assert_eq!(get(2)["instance_state_epochs"], json!({&a:2,&b:1}));
    assert_eq!(dry()["state_reset"], true, "one old instance is enough");
    f.module
        .rt
        .admit_trigger(&b, "first-b", JsonText::null())
        .unwrap();
    assert_eq!(get(2)["instance_state_epochs"], json!({&a:2,&b:2}));
    assert_eq!(dry()["state_reset"], false);
    assert_eq!(
        f.module
            .handle(
                &Caller::Operator,
                "package.get",
                json!({"package":"watch","version":2})
            )
            .unwrap_err()
            .code,
        "not_permitted"
    );
}
