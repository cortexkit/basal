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
    let rt = runtime(&world, InstallGate::Core);
    let id = ensure(
        &rt,
        "await kv.set('identity', self.agent_id); await ops.call('mock','long',{}); return {identity:self, seen:await kv.get('identity')};",
        1,
        1,
    );
    let run = admit(&rt, &id, "first");
    let hash = basal_core::ids::hex(&rt.run(&run).unwrap().code_hash);
    world.mock.set_install_status(
        &id,
        1,
        Some(InstallStatus::Active {
            code_hash: hash,
            scope: None,
        }),
    );
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
    let hash = rt.get_package("dark-wake", 2).unwrap()["code_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    world.mock.set_install_status(
        &id,
        2,
        Some(InstallStatus::Active {
            code_hash: hash.clone(),
            scope: None,
        }),
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
    world.mock.set_install_status(
        &other,
        2,
        Some(InstallStatus::Active {
            code_hash: hash,
            scope: None,
        }),
    );
    assert!(matches!(
        rt.resume(&other_run).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(
        common::result(&rt.run(&other_run).unwrap())["seen"],
        Value::Null
    );
    assert_eq!(
        world.mock.install_queries(),
        vec![(id.clone(), 1), (id.clone(), 1), (id, 2), (other, 2)]
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
    let flow_before = rt.flow(&id).unwrap().unwrap();
    world
        .mock
        .set_install_status(&id, 1, Some(InstallStatus::Unknown));
    assert!(matches!(
        rt.resume(&run).unwrap(),
        ActivationEnd::Revoked { .. }
    ));
    assert_eq!(rt.get_package("dark-wake", 1).unwrap(), before);
    let mut expected = flow_before.clone();
    expected.approved_version = None;
    assert_eq!(rt.flow(&id).unwrap().unwrap(), expected);
    rt.ensure_instance("dark-wake", 1, OWNER, 2).unwrap();
    assert_eq!(rt.flow(&id).unwrap().unwrap(), expected);
    assert_eq!(
        rt.admit_trigger(&id, "revoked", JsonText::null()).unwrap(),
        Admission::NotApproved
    );
    assert!(rt.outbox().unwrap().is_empty());
    ensure(&rt, "return 2;", 2, 3);
    let upgraded = admit(&rt, &id, "upgraded");
    let hash = basal_core::ids::hex(&rt.run(&upgraded).unwrap().code_hash);
    world.mock.set_install_status(
        &id,
        2,
        Some(InstallStatus::Active {
            code_hash: hash,
            scope: None,
        }),
    );
    assert!(matches!(
        rt.resume(&upgraded).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let mut expected = flow_before;
    expected.approved_version = Some(2);
    assert_eq!(rt.flow(&id).unwrap().unwrap(), expected);
}

/// A second SQLite connection observes the real writer lock, rather than a
/// flag pretending that the catalog was called outside a transaction.
struct WriterProbeCatalog {
    catalog: basal_host::MockCatalog,
    store_path: std::path::PathBuf,
    lookups: std::sync::atomic::AtomicUsize,
}

impl basal_host::Catalog for WriterProbeCatalog {
    fn supports_flow_scopes(&self, module: &str) -> bool {
        self.catalog.supports_flow_scopes(module)
    }
    fn event(&self, module: &str, name: &str, version: u32) -> Option<basal_host::EventDecl> {
        self.catalog.event(module, name, version)
    }
    fn op(&self, module: &str, op: &str) -> Option<basal_host::OpDecl> {
        self.catalog.op(module, op)
    }
    fn agent_known(&self, agent: &str) -> bool {
        self.catalog.agent_known(agent)
    }
    fn agent_id(&self, agent: &str) -> Option<String> {
        let conn = rusqlite::Connection::open(&self.store_path).unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(
            conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").is_ok(),
            "agent lookup must not hold the SQLite writer"
        );
        self.lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.catalog.agent_id(agent)
    }
}

#[test]
fn instance_agent_lookup_leaves_the_sqlite_writer_available() {
    let world = World::new("pkg-lookup-lock");
    world.catalog.add_named_agent(OWNER, "RenameMe");
    let store = Arc::new(basal_core::Store::open(world.store_path(), world.durability).unwrap());
    let catalog = Arc::new(WriterProbeCatalog {
        catalog: world.catalog.clone(),
        store_path: world.store_path(),
        lookups: std::sync::atomic::AtomicUsize::new(0),
    });
    let rt = basal_core::Runtime::new(
        store,
        Arc::new(world.mock.clone()),
        catalog.clone(),
        Arc::new(NoHooks),
        None,
        Config {
            auto_resume: false,
            ..common::config()
        },
    );
    rt.register_package("return 1;", &manifest(1).to_string())
        .unwrap();
    let first = rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap();
    assert_eq!(rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap(), first);
    assert_eq!(catalog.lookups.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn removed_instance_schedule_pauses_and_resumes_without_catch_up() {
    let world = World::new("pkg-schedule-removal");
    let start: jiff::Timestamp = "2026-05-01T00:00:00Z".parse().unwrap();
    let clock = basal_core::Clock::manual(start.as_millisecond());
    world.catalog.add_named_agent(OWNER, "RenameMe");
    let rt = world
        .runtime(
            Arc::new(NoHooks),
            Config {
                clock: clock.clone(),
                auto_resume: false,
                ..common::config()
            },
        )
        .unwrap();
    let mut m = manifest(1);
    m["trigger"] = json!({"schedule":{"interval":"15m"}});
    rt.register_package("return trigger;", &m.to_string())
        .unwrap();
    let id = rt.ensure_instance("dark-wake", 1, OWNER, 1).unwrap()["flow_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let scheduler = rt.scheduler().unwrap();
    let before = rt.flow(&id).unwrap();
    rt.remove_instance("dark-wake", OWNER, 2).unwrap();
    assert_eq!(rt.flow(&id).unwrap(), before);
    for minute in [15, 30, 45, 60] {
        clock.set(start.as_millisecond() + minute * 60_000);
        let tick = scheduler.tick().unwrap();
        assert!(
            tick.planned.is_empty() && tick.admitted.is_empty() && tick.waiting == 0,
            "{tick:?}"
        );
        assert!(scheduler.dropped().unwrap().is_empty());
    }
    clock.set(start.as_millisecond() + 65 * 60_000);
    rt.ensure_instance("dark-wake", 1, OWNER, 3).unwrap();
    assert_eq!(rt.flow(&id).unwrap(), before);
    let resumed = scheduler.schedule(&id).unwrap().unwrap();
    assert_eq!(resumed.anchor, start);
    assert_eq!(
        resumed.next_due.unwrap().as_millisecond(),
        start.as_millisecond() + 75 * 60_000
    );
    assert!(scheduler.tick().unwrap().admitted.is_empty());
    clock.set(start.as_millisecond() + 75 * 60_000);
    let tick = scheduler.tick().unwrap();
    assert_eq!(tick.new_runs().len(), 1);
    let run = rt.run(tick.new_runs()[0]).unwrap();
    let trigger: Value = serde_json::from_str(run.trigger.as_str()).unwrap();
    assert_eq!(
        trigger,
        json!({"kind":"schedule","due":"2026-05-01T01:15:00Z"})
    );
    assert!(scheduler.dropped().unwrap().is_empty());

    // Neither removal nor reconciliation can undo an operator's disable.
    rt.disable_flow(&id, &basal_core::Actor::Operator("operator".into()), "stop")
        .unwrap();
    let disabled = rt.flow(&id).unwrap();
    rt.remove_instance("dark-wake", OWNER, 4).unwrap();
    clock.set(start.as_millisecond() + 120 * 60_000);
    rt.ensure_instance("dark-wake", 1, OWNER, 5).unwrap();
    assert_eq!(rt.flow(&id).unwrap(), disabled);
    assert_eq!(
        scheduler.schedule(&id).unwrap().unwrap().state,
        basal_core::schedule::ScheduleState::Disabled
    );
    assert!(scheduler.tick().unwrap().admitted.is_empty());
    assert!(scheduler.dropped().unwrap().is_empty());

    // Enabling while removed changes the flow's brake, not its membership.
    rt.remove_instance("dark-wake", OWNER, 6).unwrap();
    rt.enable_flow(&id).unwrap();
    clock.set(start.as_millisecond() + 180 * 60_000);
    let tick = scheduler.tick().unwrap();
    assert!(tick.planned.is_empty() && tick.admitted.is_empty());
    assert!(scheduler.dropped().unwrap().is_empty());
    rt.ensure_instance("dark-wake", 1, OWNER, 7).unwrap();
    assert_eq!(
        scheduler
            .schedule(&id)
            .unwrap()
            .unwrap()
            .next_due
            .unwrap()
            .as_millisecond(),
        start.as_millisecond() + 195 * 60_000
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
