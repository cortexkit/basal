mod common;

use basal_core::{ActivationEnd, Admission, Config, InstallGate, NoHooks, RunState};
use basal_host::InstallStatus;
use basal_proto::{CallKind, JsonText, Primitive};
use basal_testkit::harness::{Point, Probe, World, test_manifest};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const OWNER: &str = "agent_47120287c700722b";
fn manifest(version: u32) -> Value {
    let mut m = test_manifest();
    m["id"] = json!("watch");
    m["version"] = json!(version);
    m["sinks"][0]["agent"] = json!("$self");
    m["status"] = json!(["$self"]);
    m["facts"]["targets"] = json!(["$self"]);
    m["params"] = json!({"note":{"type":"string","max_bytes":4096}});
    m["template"] = json!({"max_per_agent":2,"max_lifetime":"1d"});
    m
}
fn config(gate: InstallGate) -> Config {
    Config {
        install_gate: gate,
        auto_resume: false,
        ..common::config()
    }
}
fn runtime(world: &World, gate: InstallGate) -> basal_core::Runtime {
    world.catalog.add_named_agent(OWNER, "Owner");
    world.runtime(Arc::new(NoHooks), config(gate)).unwrap()
}
fn create(rt: &basal_core::Runtime, version: u32, script: &str) -> String {
    rt.register_package(script, &manifest(version).to_string())
        .unwrap();
    rt.instantiate("watch", version, OWNER, &json!({"note":"remember"}), "1h")
        .unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .into()
}
fn admit(rt: &basal_core::Runtime, id: &str, key: &str) -> String {
    rt.admit_trigger(id, key, JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .into()
}

#[test]
fn journaled_self_end_survives_crash_and_replay_once() {
    let world = World::new("template-self-end-replay");
    world.catalog.add_named_agent(OWNER, "Owner");
    let probe = Arc::new(Probe::crash_at(
        Point::parse("LocalCommitted { position: 0 }#1").unwrap(),
    ));
    let rt = world
        .runtime(probe.clone(), config(InstallGate::Off))
        .unwrap();
    let id = create(
        &rt,
        1,
        "await instance.end(); await kv.set('finished',true); return {self,params};",
    );
    let run = admit(&rt, &id, "first");
    let _ = rt.run_to_rest(&run, Duration::from_secs(30));
    rt.quiesce();
    assert!(probe.fired());

    drop(rt);
    world.mock.detach();
    let rt = runtime(&world, InstallGate::Off);
    assert!(
        !rt.store()
            .read(|conn| basal_core::packages::removed(conn, &id))
            .unwrap()
    );
    let finished = common::finish(&rt, &world, &run);
    assert_eq!(finished.state, RunState::Succeeded);
    assert_eq!(
        common::result(&finished),
        json!({"self":{"agent_id":OWNER},"params":{"note":"remember"}})
    );
    assert_eq!(
        rt.calls(&run)
            .unwrap()
            .iter()
            .filter(|call| call.kind == CallKind::Primitive(Primitive::InstanceEnd))
            .count(),
        1
    );
    assert_eq!(
        rt.admit_trigger(&id, "later", JsonText::null()).unwrap(),
        Admission::NotApproved
    );
    assert_eq!(rt.end_instance(&id).unwrap()["ended"], false);
    rt.store()
        .read(|conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT revision FROM kv WHERE flow_id=?1 AND key='finished'",
                    [&id],
                    |row| row.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn old_self_end_cannot_end_an_instance_reopened_in_a_new_incarnation() {
    let world = World::new("template-incarnation");
    let rt = runtime(&world, InstallGate::Off);
    let id = create(
        &rt,
        1,
        "await instance.end(); await ops.call('mock','long',{}); return true;",
    );
    let run = admit(&rt, &id, "old");
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    assert_eq!(rt.end_instance(&id).unwrap()["ended"], true);
    assert_eq!(
        rt.instantiate("watch", 1, OWNER, &json!({"note":"remember"}), "2h")
            .unwrap()["new"],
        false
    );
    world.mock.complete_all();
    let finished = common::finish(&rt, &world, &run);
    assert_eq!(finished.state, RunState::Succeeded);
    assert!(matches!(
        rt.admit_trigger(&id, "new", JsonText::null()).unwrap(),
        Admission::Admitted { .. }
    ));
}

#[test]
fn template_version_revocation_stops_only_that_version_and_keeps_registry() {
    let world = World::new("template-revocation");
    let rt = runtime(&world, InstallGate::Core);
    let id = create(&rt, 1, "return params.note;");
    let original = rt.get_package("watch", 1).unwrap();
    let run = admit(&rt, &id, "revoked");
    world.mock.set_install_status(
        &id,
        1,
        Some(InstallStatus::Revoked {
            code_hash: original["code_hash"].as_str().unwrap().into(),
        }),
    );
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Revoked { .. }
    ));
    assert_eq!(rt.get_package("watch", 1).unwrap(), original);
    create(&rt, 1, "return params.note;");
    assert_eq!(
        rt.admit_trigger(&id, "still-revoked", JsonText::null())
            .unwrap(),
        Admission::NotApproved
    );
    create(&rt, 2, "return params.note;");
    let package = rt.get_package("watch", 2).unwrap();
    world.mock.set_install_status(
        &id,
        2,
        Some(InstallStatus::Active {
            code_hash: package["code_hash"].as_str().unwrap().into(),
            scope: None,
        }),
    );
    let run = admit(&rt, &id, "new-version");
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
}

#[test]
fn params_and_self_replay_from_admission_not_mutable_instance_records() {
    let world = World::new("template-input-snapshot");
    let rt = runtime(&world, InstallGate::Off);
    let id = create(
        &rt,
        1,
        "await ops.call('mock','long',{}); return {self,params};",
    );
    let run = admit(&rt, &id, "snapshot");
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    create(&rt, 2, "return {self,params};");
    rt.store()
        .write(|tx| {
            tx.execute("UPDATE flows SET owner='tampered' WHERE flow_id=?1", [&id])?;
            tx.execute(
                "UPDATE template_instances SET params='{\"note\":\"tampered\"}' WHERE flow_id=?1",
                [&id],
            )?;
            Ok(())
        })
        .unwrap();
    world.mock.complete_all();
    let finished = common::finish(&rt, &world, &run);
    assert_eq!(finished.flow_version, Some(1));
    assert_eq!(
        common::result(&finished),
        json!({"self":{"agent_id":OWNER},"params":{"note":"remember"}})
    );
}
