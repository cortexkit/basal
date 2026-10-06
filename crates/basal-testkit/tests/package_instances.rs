mod common;

use basal_core::{ActivationEnd, Admission, Config, InstallGate, NoHooks, RunState};
use basal_host::InstallStatus;
use basal_proto::JsonText;
use basal_testkit::harness::{World, test_manifest};
use serde_json::{Value, json};
use std::sync::Arc;

const OWNER: &str = "agent_47120287c700722b";
fn manifest(version: u32) -> Value {
    let mut m = test_manifest();
    m["id"] = json!("dark-wake");
    m["version"] = json!(version);
    m["sinks"][0]["agent"] = json!("$self");
    m["status"] = json!(["$self"]);
    m["facts"]["targets"] = json!(["$self"]);
    m
}
fn runtime(world: &World, gate: InstallGate) -> basal_core::Runtime {
    world.catalog.add_named_agent(OWNER, "RenameMe");
    world
        .runtime(
            Arc::new(NoHooks),
            Config {
                install_gate: gate,
                auto_resume: false,
                ..common::config()
            },
        )
        .unwrap()
}
fn ensure(rt: &basal_core::Runtime, script: &str, version: u32, generation: i64) -> String {
    rt.register_package(script, &manifest(version).to_string())
        .unwrap();
    rt.ensure_instance("dark-wake", version, OWNER, generation)
        .unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn admit(rt: &basal_core::Runtime, id: &str, trigger: &str) -> String {
    let admission = rt.admit_trigger(id, trigger, JsonText::null()).unwrap();
    assert!(
        matches!(admission, Admission::Admitted { .. }),
        "{admission:?}"
    );
    admission.run_id().unwrap().to_owned()
}

#[test]
fn self_grants_resolve_to_the_owner_and_sink_calls_carry_stable_id() {
    let world = World::new("pkg-resolution");
    let rt = runtime(&world, InstallGate::Core);
    let id = ensure(
        &rt,
        r#"
        await sink.digest('$self', 'digest');
        await sink.status(self.agent_id, 'status');
        const factsResult = await facts('$self');
        let foreign;
        try { await sink.digest('ALF', 'foreign'); } catch(e) { foreign = e.data.code; }
        return {identity:self, factsAgent:factsResult.agent, foreign};
    "#,
        1,
        1,
    );
    let run = admit(&rt, &id, "first");
    let r = rt.run(&run).unwrap();
    world.mock.set_install_status(
        &id,
        1,
        Some(InstallStatus::Active {
            code_hash: basal_core::ids::hex(&r.code_hash),
            scope: None,
        }),
    );
    let end = rt.resume(&run).unwrap();
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert_eq!(
        common::result(&rt.run(&run).unwrap()),
        json!({"identity":{"agent_id":OWNER},"factsAgent":OWNER,"foreign":"denied"})
    );
    assert_eq!(world.mock.install_queries(), vec![(id.clone(), 1)]);
    assert_eq!(world.mock.effects().len(), 2);
    for e in world.mock.effects() {
        let args: Value = serde_json::from_str(&e.args).unwrap();
        assert_eq!(args["agent"], OWNER);
        assert_eq!(args["flow_id"], id);
        assert_eq!(args["flow_version"], 1);
    }
}

#[test]
fn self_replays_from_admission_and_upgrade_keeps_kv_and_old_run_version() {
    let world = World::new("pkg-replay");
    let rt = runtime(&world, InstallGate::Off);
    let id = ensure(
        &rt,
        "await kv.set('identity', self.agent_id); await ops.call('mock','long',{}); return {identity:self, seen:await kv.get('identity')};",
        1,
        1,
    );
    let run = admit(&rt, &id, "first");
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    ensure(
        &rt,
        "return {identity:self, seen:await kv.get('identity')};",
        2,
        2,
    );
    assert_eq!(rt.run(&run).unwrap().flow_version, Some(1));
    // A corrupt mutable owner is not activation input: replay uses the snapshot.
    rt.store()
        .write(|tx| Ok(tx.execute("UPDATE flows SET owner='renamed' WHERE flow_id=?1", [&id])?))
        .unwrap();
    world.mock.complete_all();
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(
        common::result(&rt.run(&run).unwrap()),
        json!({"identity":{"agent_id":OWNER},"seen":OWNER})
    );
    rt.store()
        .write(|tx| {
            Ok(tx.execute(
                "UPDATE flows SET owner=?2 WHERE flow_id=?1",
                rusqlite::params![id, OWNER],
            )?)
        })
        .unwrap();
    let upgraded = admit(&rt, &id, "second");
    assert_eq!(rt.run(&upgraded).unwrap().flow_version, Some(2));
    assert!(matches!(
        rt.resume(&upgraded).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(common::result(&rt.run(&upgraded).unwrap())["seen"], OWNER);
    world.catalog.add_agent("agent_second");
    let other = rt
        .ensure_instance("dark-wake", 2, "agent_second", 1)
        .unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let other_run = admit(&rt, &other, "first");
    assert!(matches!(
        rt.resume(&other_run).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(
        common::result(&rt.run(&other_run).unwrap())["seen"],
        Value::Null
    );
}

#[test]
fn removed_instance_admits_nothing_and_cancels_at_next_activation_without_disabling() {
    let world = World::new("pkg-remove-run");
    let rt = runtime(&world, InstallGate::Off);
    let id = ensure(&rt, "await ops.call('mock','long',{}); return 1;", 1, 1);
    let run = admit(&rt, &id, "first");
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    let before = rt.flow(&id).unwrap();
    rt.remove_instance("dark-wake", OWNER, 2).unwrap();
    assert_eq!(rt.flow(&id).unwrap(), before);
    assert_eq!(
        rt.admit_trigger(&id, "removed", JsonText::null()).unwrap(),
        Admission::NotApproved
    );
    world.mock.complete_all();
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Revoked { .. }
    ));
    assert_eq!(rt.run(&run).unwrap().state, RunState::Cancelled);
    assert_eq!(rt.flow(&id).unwrap(), before);
    assert!(rt.outbox().unwrap().is_empty());
    rt.ensure_instance("dark-wake", 1, OWNER, 3).unwrap();
    assert!(matches!(
        rt.admit_trigger(&id, "back", JsonText::null()).unwrap(),
        Admission::Admitted { .. }
    ));
}

#[test]
fn instance_core_revocation_does_not_mutate_package_bytes() {
    let world = World::new("pkg-core-revoke");
    let rt = runtime(&world, InstallGate::Core);
    let id = ensure(&rt, "return 1;", 1, 1);
    let run = admit(&rt, &id, "first");
    let before = rt.get_package("dark-wake", 1).unwrap();
    world
        .mock
        .set_install_status(&id, 1, Some(InstallStatus::Unknown));
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Revoked { .. }
    ));
    assert_eq!(rt.get_package("dark-wake", 1).unwrap(), before);
    assert!(!rt.flow(&id).unwrap().unwrap().enabled);
    rt.ensure_instance("dark-wake", 1, OWNER, 2).unwrap();
    assert!(!rt.flow(&id).unwrap().unwrap().enabled);
    rt.enable_flow(&id).unwrap();
    assert_eq!(
        rt.admit_trigger(&id, "revoked", JsonText::null()).unwrap(),
        Admission::NotApproved
    );
}

#[test]
fn generation_and_instance_changes_roll_back_together_and_survive_restart() {
    let world = World::new("pkg-atomic");
    let rt = runtime(&world, InstallGate::Off);
    rt.register_package("return 1;", &manifest(1).to_string())
        .unwrap();
    let id = basal_core::packages::instance_id("dark-wake", OWNER);
    rt.store().write(|tx| Ok(tx.execute_batch("CREATE TRIGGER fail_generation BEFORE INSERT ON instance_generations BEGIN SELECT RAISE(ABORT,'write failed'); END;")?)).unwrap();
    assert!(rt.ensure_instance("dark-wake", 1, OWNER, 1).is_err());
    assert!(rt.flow(&id).unwrap().is_none());
    rt.store()
        .write(|tx| Ok(tx.execute_batch("DROP TRIGGER fail_generation")?))
        .unwrap();
    let reply = rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap();
    rt.store().write(|tx| Ok(tx.execute_batch("CREATE TRIGGER fail_generation BEFORE INSERT ON instance_generations BEGIN SELECT RAISE(ABORT,'write failed'); END;")?)).unwrap();
    assert!(rt.remove_instance("dark-wake", OWNER, 2).is_err());
    assert!(
        !rt.store()
            .read(|c| basal_core::packages::removed(c, &id))
            .unwrap()
    );
    rt.store()
        .write(|tx| Ok(tx.execute_batch("DROP TRIGGER fail_generation")?))
        .unwrap();
    drop(rt);
    let rt = runtime(&world, InstallGate::Off);
    assert_eq!(rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap(), reply);
    assert!(rt.remove_instance("dark-wake", OWNER, 0).is_err());
}

#[test]
fn schedules_and_resource_windows_stay_per_instance_across_changes() {
    let world = World::new("pkg-state");
    let rt = runtime(&world, InstallGate::Off);
    for version in [1, 2] {
        let mut m = manifest(version);
        m["trigger"] = json!({"schedule":{"interval":"15m"}});
        rt.register_package("return 1;", &m.to_string()).unwrap();
    }
    let id = rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned();
    rt.store().write(|tx| {
        tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start,reserved) VALUES (?1,60000,0,17)",[&id])?;
        tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,runs,dispatches) VALUES (?1,60000,0,3,5)",[&id])?;
        Ok(())
    }).unwrap();
    rt.disable_flow(&id, &basal_core::Actor::Runtime, "stop")
        .unwrap();
    let schedule = rt
        .store()
        .read(|c| basal_core::schedule::table::load(c, &id))
        .unwrap();
    rt.ensure_instance("dark-wake", 1, OWNER, 2).unwrap();
    assert_eq!(
        rt.store()
            .read(|c| basal_core::schedule::table::load(c, &id))
            .unwrap(),
        schedule
    );
    rt.remove_instance("dark-wake", OWNER, 3).unwrap();
    assert_eq!(
        rt.store()
            .read(|c| basal_core::schedule::table::load(c, &id))
            .unwrap(),
        schedule
    );
    rt.ensure_instance("dark-wake", 2, OWNER, 4).unwrap();
    rt.ensure_instance("dark-wake", 1, OWNER, 5).unwrap();
    assert!(!rt.flow(&id).unwrap().unwrap().enabled);
    rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT reserved FROM token_windows WHERE flow_id=?1",
                    [&id],
                    |r| r.get::<_, i64>(0)
                )?,
                17
            );
            assert_eq!(
                c.query_row(
                    "SELECT runs,dispatches FROM rate_windows WHERE flow_id=?1",
                    [&id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                )?,
                (3, 5)
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM installs WHERE flow_id=?1",
                    [&id],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
}
