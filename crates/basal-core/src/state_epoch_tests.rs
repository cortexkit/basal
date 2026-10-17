use crate as basal_core;
use basal_core::admission::{self, AdmitContext};
use basal_core::error::CoreError;
use basal_core::manifest::Manifest;
use basal_core::{Admission, Clock, Config, Durability, InstallGate, NoHooks, Runtime, Store};
use basal_host::{MockCatalog, mock::MockHost};
use basal_proto::JsonText;
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    rt: Runtime,
    dir: std::path::PathBuf,
    cleanup: bool,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.rt.quiesce();
        if self.cleanup {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
fn fixture(tag: &str) -> Fixture {
    let dir = basal_core::test_support::timestamp_scratch("epoch", tag);
    fixture_at(dir)
}
fn fixture_at(dir: std::path::PathBuf) -> Fixture {
    let rt = Runtime::new(
        Arc::new(Store::open(dir.join("core.db"), Durability::throwaway()).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            clock: Clock::manual(1000),
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    );
    Fixture {
        rt,
        dir,
        cleanup: true,
    }
}
fn manifest(version: u32, epoch: Option<u32>) -> String {
    let mut m = json!({"id":"flow","version":version,"purpose":"Store values.","trigger":{"schedule":{"interval":"1m"}}});
    if let Some(epoch) = epoch {
        m["state_epoch"] = json!(epoch);
    }
    m.to_string()
}
fn approve(f: &Fixture, version: u32, epoch: Option<u32>) {
    let hash = install(f, version, epoch);
    f.rt.approve("flow", version, &hash, &format!("approval-{version}"))
        .unwrap();
}
fn install(f: &Fixture, version: u32, epoch: Option<u32>) -> [u8; 32] {
    f.rt.install(&basal_core::install::InstallRequest {
        script: "return 1;".into(),
        manifest: manifest(version, epoch),
        author: "operator".into(),
        loop_override: false,
    })
    .unwrap()
    .code_hash
}
fn seed(f: &Fixture, id: &str) {
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES (?1,'key','42',5,1)",
                [id],
            )?;
            Ok(())
        })
        .unwrap();
}
fn state(f: &Fixture, id: &str) -> (u32, usize) {
    f.rt.store()
        .read(|c| {
            Ok((
                basal_core::state_epoch::recorded(c, id)?,
                c.query_row("SELECT count(*) FROM kv WHERE flow_id=?1", [id], |r| {
                    r.get(0)
                })?,
            ))
        })
        .unwrap()
}
fn admit(f: &Fixture, id: &str, trigger: &str) -> String {
    match f.rt.admit_trigger(id, trigger, JsonText::null()).unwrap() {
        Admission::Admitted { run_id } => run_id,
        other => panic!("not admitted: {other:?}"),
    }
}
fn finish(f: &Fixture, run: &str) {
    f.rt.store()
        .write(|tx| {
            let lease = basal_core::runs::claim(tx, run, "worker", 1000)?;
            basal_core::runs::exit(
                tx,
                &lease,
                &basal_core::runs::Exit::Succeeded {
                    value: "null".into(),
                },
            )?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn manifest_epoch_defaults_is_bounded_and_round_trips() {
    assert_eq!(Manifest::parse(&manifest(1, None)).unwrap().state_epoch, 1);
    for epoch in [1, 2, i32::MAX as u32] {
        let parsed = Manifest::parse(&manifest(1, Some(epoch))).unwrap();
        assert_eq!(parsed.state_epoch, epoch);
        assert_eq!(
            Manifest::parse(&serde_json::to_string(&parsed).unwrap()).unwrap(),
            parsed
        );
    }
    for epoch in [
        json!(0),
        json!(-1),
        json!(2147483648_u64),
        json!(1.5),
        json!("2"),
        Value::Null,
    ] {
        let mut m: Value = serde_json::from_str(&manifest(1, None)).unwrap();
        m["state_epoch"] = epoch;
        assert!(Manifest::parse(&m.to_string()).is_err(), "{m}");
    }
    let mut m: Value = serde_json::from_str(&manifest(1, Some(2))).unwrap();
    m["state_epochs"] = json!(2);
    assert!(Manifest::parse(&m.to_string()).is_err());
}

#[test]
fn absent_epoch_upgrade_keeps_existing_values() {
    let f = fixture("default");
    approve(&f, 1, None);
    seed(&f, "flow");
    let first = admit(&f, "flow", "first");
    finish(&f, &first);
    approve(&f, 2, None);
    assert_eq!(state(&f, "flow"), (1, 1));
    admit(&f, "flow", "second");
    assert_eq!(state(&f, "flow"), (1, 1));
    assert_eq!(f.rt.flow_health().unwrap()[0].last_state_reset_at, None);
}

#[test]
fn epoch_reset_commits_with_admission_and_rolls_back_with_it() {
    let mut f = fixture("transaction");
    approve(&f, 1, None);
    seed(&f, "flow");
    let hash = install(&f, 2, Some(2));
    assert_eq!(state(&f, "flow"), (1, 1));
    f.rt.approve("flow", 2, &hash, "approval-2").unwrap();
    assert_eq!(state(&f, "flow"), (1, 1), "approval does not reset");
    let ctx = AdmitContext {
        now_ms: 1234,
        rate: f.rt.config().rate,
        default_deadline_ms: 600_000,
    };
    let result: basal_core::Result<()> = f.rt.store().write(|tx| {
        assert!(matches!(
            admission::admit_current(
                tx,
                f.rt.store().store_id(),
                "flow",
                "rollback",
                JsonText::null(),
                &ctx
            )?,
            Admission::Admitted { .. }
        ));
        assert_eq!(basal_core::state_epoch::recorded(tx, "flow")?, 2);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM kv WHERE flow_id='flow'", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        Err(CoreError::Invalid("simulate failure before commit".into()))
    });
    assert!(result.is_err());
    assert_eq!(state(&f, "flow"), (1, 1));
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM state_resets", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
    let run = admit(&f, "flow", "committed");
    assert_eq!(f.rt.run(&run).unwrap().state, basal_core::RunState::Pending);
    assert_eq!(state(&f, "flow"), (2, 0), "reset is durable before running");
    f.rt.store()
        .read(|c| {
            let log: (String, u32, u32, usize, i64) = c.query_row(
                "SELECT flow_id,old_epoch,new_epoch,keys_cleared,at FROM state_resets",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            assert_eq!(log, ("flow".into(), 1, 2, 1, 1000));
            Ok(())
        })
        .unwrap();
    let health = f.rt.flow_health().unwrap();
    assert_eq!(health[0].state_epoch, 2);
    assert_eq!(health[0].last_state_reset_at, Some(1000));
    let dir = f.dir.clone();
    f.cleanup = false;
    drop(f);
    let reopened = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
    reopened
        .read(|c| {
            assert_eq!(basal_core::state_epoch::recorded(c, "flow")?, 2);
            assert_eq!(
                c.query_row("SELECT count(*) FROM kv WHERE flow_id='flow'", [], |r| {
                    r.get::<_, usize>(0)
                })?,
                0
            );
            assert_eq!(
                c.query_row("SELECT state FROM runs WHERE run_id=?1", [&run], |r| r
                    .get::<_, String>(
                    0
                ))?,
                "pending"
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM state_resets", [], |r| r
                    .get::<_, usize>(0))?,
                1
            );
            Ok(())
        })
        .unwrap();
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn epoch_rollback_resets_on_difference_not_order() {
    let f = fixture("rollback");
    approve(&f, 1, None);
    approve(&f, 2, Some(2));
    let run = admit(&f, "flow", "up");
    finish(&f, &run);
    seed(&f, "flow");
    // Ordinary install versions are monotonic; a rollback republishes the old
    // layout as a new approved version with its original epoch.
    approve(&f, 3, Some(1));
    assert_eq!(state(&f, "flow"), (2, 1));
    admit(&f, "flow", "down");
    assert_eq!(state(&f, "flow"), (1, 0));
}

#[test]
fn epoch_reset_preserves_token_rate_and_runtime_records() {
    let f = fixture("preservation");
    approve(&f, 1, None);
    let old_run = admit(&f, "flow", "old");
    finish(&f, &old_run);
    approve(&f, 2, Some(2));
    seed(&f, "flow");
    f.rt.store().write(|tx| {
        tx.execute_batch("INSERT INTO flows(flow_id,created_at) VALUES ('other',0);
            INSERT INTO token_windows(flow_id,window_ms,window_start,reserved,output_tokens) VALUES ('flow',60000,0,10,17);
            INSERT INTO rate_windows(flow_id,window_ms,window_start,runs,dispatches,saturated) VALUES ('flow',60000,-60000,8,9,1);
            INSERT INTO event_receipts VALUES ('flow','subject','seen','{}','admitted',0);
            INSERT INTO event_receipts VALUES ('flow','subject','waiting','{}','backlog',0);
            INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES ('flow','planned',2,2000,'{}',1000);
            INSERT INTO tombstones VALUES ('trigger','old-key','pruned-run',0);
            INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('card','reenable','flow',1,1,'{}','open',0);")?;
        tx.execute("INSERT INTO journal(run_id,position,kind_code,args,args_digest,idempotency_key,class,dispatch,issued_generation) VALUES (?1,0,1,'{}',zeroblob(32),'journal-key','query','sent',1)", [&old_run])?;
        Ok(())
    }).unwrap();
    seed(&f, "other");
    let tables = [
        "schedules",
        "schedule_fires",
        "event_receipts",
        "tombstones",
        "journal",
        "decision_cards",
        "install_cards",
        "token_windows",
    ];
    let snapshot = || {
        f.rt.store()
            .read(|c| {
                tables
                    .iter()
                    .map(|table| {
                        let mut stmt = c.prepare(&format!("SELECT * FROM {table} ORDER BY 1"))?;
                        let cols = stmt.column_count();
                        Ok(stmt
                            .query_map([], |r| {
                                (0..cols)
                                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                                    .collect::<rusqlite::Result<Vec<_>>>()
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?)
                    })
                    .collect::<basal_core::Result<Vec<_>>>()
            })
            .unwrap()
    };
    let before = snapshot();
    admit(&f, "flow", "new");
    assert_eq!(state(&f, "flow"), (2, 0));
    assert_eq!(state(&f, "other"), (1, 1));
    assert_eq!(snapshot(), before, "only kv is reset");
    f.rt.store().read(|c| {
        let rates: (u32,u32,bool) = c.query_row("SELECT runs,dispatches,saturated FROM rate_windows WHERE flow_id='flow' AND window_start=-60000", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!(rates, (8,9,true));
        assert_eq!(c.query_row("SELECT runs FROM rate_windows WHERE flow_id='flow' AND window_start=0", [], |r| r.get::<_,u32>(0))?, 2, "admission adds to the old rate window");
        assert_eq!(c.query_row("SELECT count(*) FROM trigger_inbox WHERE run_id=?1", [&old_run], |r| r.get::<_,u32>(0))?, 1);
        Ok(())
    }).unwrap();
}

#[test]
fn epoch_migration_defaults_existing_kv_to_one_without_changing_it() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for m in basal_core::schema::MIGRATIONS
        .iter()
        .filter(|m| m.version < 29)
    {
        conn.execute_batch(m.statements).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO flows(flow_id,created_at) VALUES ('existing',0);
        INSERT INTO kv VALUES ('existing','key','42',5,3);",
    )
    .unwrap();
    conn.execute_batch(
        basal_core::schema::MIGRATIONS
            .iter()
            .find(|m| m.version == 29)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(
        basal_core::state_epoch::recorded(&conn, "existing").unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT value,revision FROM kv WHERE flow_id='existing'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
        )
        .unwrap(),
        ("42".into(), 3)
    );
}

#[test]
fn epoch_change_waits_for_old_runs_and_fences_late_writes() {
    use basal_core::{journal, kv, runs};
    use basal_proto::{CallKind, Primitive};
    let f = fixture("old-writes");
    approve(&f, 1, None);
    let old = admit(&f, "flow", "old");
    approve(&f, 2, Some(2));
    assert_eq!(
        f.rt.admit_trigger("flow", "new", JsonText::null()).unwrap(),
        Admission::StateBusy,
        "pending old work also prevents a reset"
    );
    let lease =
        f.rt.store()
            .write(|tx| runs::claim(tx, &old, "worker", 1000))
            .unwrap();
    assert_eq!(f.rt.run(&old).unwrap().flow_version, Some(1));
    assert_eq!(
        f.rt.admit_trigger("flow", "new", JsonText::null()).unwrap(),
        Admission::StateBusy
    );
    let kind = CallKind::Primitive(Primitive::KvSet);
    let args = JsonText::new(r#"{"key":"late","value":99}"#).unwrap();
    let call = journal::NewCall {
        position: 0,
        kind: &kind,
        args: &args,
        class: basal_core::model::StoredClass::Local,
        request: None,
    };
    f.rt.store()
        .write(|tx| {
            journal::insert_call(tx, &lease, "flow", &call)?;
            let outcome = kv::apply(tx, "flow", &kind, &args, &f.rt.config().kv)?;
            journal::accept_outcome(tx, &old, 0, None, &outcome, journal::Source::Local)?;
            journal::release_next(tx, &lease, &[0])?;
            runs::exit(tx, &lease, &runs::Exit::Succeeded { value: "99".into() })?;
            Ok(())
        })
        .unwrap();
    assert_eq!(state(&f, "flow"), (1, 1), "old run writes after approval");
    admit(&f, "flow", "new");
    assert_eq!(state(&f, "flow"), (2, 0));
    let late = f.rt.store().write(|tx| {
        journal::insert_call(
            tx,
            &lease,
            "flow",
            &journal::NewCall {
                position: 1,
                ..call
            },
        )?;
        kv::apply(tx, "flow", &kind, &args, &f.rt.config().kv)?;
        Ok(())
    });
    assert!(matches!(late, Err(CoreError::OwnerLost { .. })));
    assert_eq!(state(&f, "flow"), (2, 0));
}

#[test]
fn template_instances_reset_independently_on_first_admission() {
    let f = fixture("instances");
    let agent = f.rt.catalog().agent_id("SYNAPSE").unwrap().unwrap();
    let mut m: Value = serde_json::from_str(&manifest(1, None)).unwrap();
    m["id"] = json!("watch");
    m["params"] = json!({"note":{"type":"string","max_bytes":32}});
    m["template"] = json!({"max_per_agent":2,"max_live":2,"max_lifetime":"1d"});
    f.rt.register_package("return 1;", &m.to_string()).unwrap();
    let a =
        f.rt.instantiate("watch", 1, &agent, &json!({"note":"a"}), "1h")
            .unwrap();
    let b =
        f.rt.instantiate("watch", 1, &agent, &json!({"note":"b"}), "1h")
            .unwrap();
    let a = a["flow_id"].as_str().unwrap();
    let b = b["flow_id"].as_str().unwrap();
    seed(&f, a);
    seed(&f, b);
    m["version"] = json!(2);
    m["state_epoch"] = json!(2);
    let bytes = m.to_string();
    f.rt.register_package("return 1;", &bytes).unwrap();
    f.rt.instantiate("watch", 2, &agent, &json!({"note":"a"}), "1h")
        .unwrap();
    f.rt.instantiate("watch", 2, &agent, &json!({"note":"b"}), "1h")
        .unwrap();
    let p = f.rt.get_package("watch", 2).unwrap();
    assert_eq!(p["manifest"], bytes);
    assert_eq!(p["state_epoch"], 2);
    assert_eq!(p["instance_state_epochs"], json!({a:1,b:1}));
    assert_eq!(state(&f, a), (1, 1));
    assert_eq!(state(&f, b), (1, 1));
    admit(&f, a, "new-a");
    assert_eq!(state(&f, a), (2, 0));
    assert_eq!(state(&f, b), (1, 1));
    assert!(
        f.rt.store()
            .read(|c| basal_core::state_epoch::would_reset(c, "watch", 2, true))
            .unwrap()
    );
    admit(&f, b, "new-b");
    assert_eq!(state(&f, b), (2, 0));
    assert_eq!(
        f.rt.get_package("watch", 2).unwrap()["instance_state_epochs"],
        json!({a:2,b:2})
    );
    assert!(
        !f.rt
            .store()
            .read(|c| basal_core::state_epoch::would_reset(c, "watch", 2, true))
            .unwrap()
    );
}

#[test]
fn epoch_crash_after_admission_reopens_complete_reset() {
    const CHILD_DIR: &str = "BASAL_EPOCH_CRASH_CHILD_DIR";
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        let f = fixture_at(dir.into());
        approve(&f, 1, None);
        seed(&f, "flow");
        approve(&f, 2, Some(2));
        admit(&f, "flow", "crash");
        // Exit without dropping the runtime or store, after commit but before
        // a worker starts. The parent must recover the entire admitted state.
        std::process::exit(0);
    }
    let dir = basal_core::test_support::timestamp_scratch("epoch", "crash");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "state_epoch::tests::epoch_crash_after_admission_reopens_complete_reset",
            "--nocapture",
        ])
        .env(CHILD_DIR, &dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
    store
        .read(|c| {
            assert_eq!(basal_core::state_epoch::recorded(c, "flow")?, 2);
            assert_eq!(
                c.query_row("SELECT count(*) FROM kv", [], |r| r.get::<_, usize>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT state,flow_version FROM runs", [], |r| Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u32>(1)?
                )))?,
                ("pending".into(), 2)
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM trigger_inbox", [], |r| r
                    .get::<_, usize>(0))?,
                1
            );
            assert_eq!(
                c.query_row(
                    "SELECT old_epoch,new_epoch,keys_cleared FROM state_resets",
                    [],
                    |r| Ok((
                        r.get::<_, u32>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, usize>(2)?
                    ))
                )?,
                (1, 2, 1)
            );
            Ok(())
        })
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reset_preview_keeps_flow_and_package_namespaces_separate() {
    let f = fixture("preview-namespace");
    approve(&f, 1, None);
    f.rt.store().write(|tx| {
        tx.execute("INSERT INTO flows(flow_id,package,state_epoch,created_at) VALUES ('instance','flow',2,0)", [])?;
        Ok(())
    }).unwrap();
    f.rt.store()
        .read(|c| {
            assert!(!basal_core::state_epoch::would_reset(c, "flow", 1, false)?);
            assert!(basal_core::state_epoch::would_reset(c, "flow", 1, true)?);
            assert!(basal_core::state_epoch::would_reset(c, "flow", 2, false)?);
            assert!(!basal_core::state_epoch::would_reset(c, "flow", 2, true)?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn scheduler_skips_only_state_busy_flow_for_this_pass() {
    let f = fixture("schedule-busy");
    approve(&f, 1, None);
    let old = admit(&f, "flow", "old");
    approve(&f, 2, Some(2));
    seed(&f, "flow");
    let mut b: Value = serde_json::from_str(&manifest(1, None)).unwrap();
    b["id"] = json!("other");
    let installed =
        f.rt.install(&basal_core::install::InstallRequest {
            script: "return 1;".into(),
            manifest: b.to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    f.rt.approve("other", 1, &installed.code_hash, "other-approved")
        .unwrap();
    // Every fire is valid and due before the clock at 1000 ms. Only the old
    // run's state-epoch fence may hold back the waiting flow's fires.
    f.rt.store().write(|tx| {
        tx.execute_batch("INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES
            ('flow','waiting-1',2,900,'{\"first\":true}',1000),
            ('flow','waiting-2',2,901,'{\"second\":true}',1000),
            ('other','ready-1',1,902,'{}',1000),
            ('other','ready-2',1,903,'{}',1000);")?;
        Ok(())
    }).unwrap();
    let waiting = || {
        f.rt.store().read(|c| {
        Ok(c.prepare("SELECT seq,trigger_id,version,due_ms,payload,planned_at FROM schedule_fires WHERE flow_id='flow' ORDER BY seq")?
            .query_map([], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,u32>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }).unwrap()
    };
    let before = waiting();
    let scheduler = f.rt.scheduler().unwrap();
    let first = scheduler.tick().unwrap();
    assert_eq!(
        first.admitted.len(),
        2,
        "busy A must not block B in the same tick"
    );
    assert_eq!(
        first
            .admitted
            .iter()
            .map(|fire| (fire.flow_id.as_str(), fire.trigger_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("other", "ready-1"), ("other", "ready-2")]
    );
    assert!(
        first
            .admitted
            .iter()
            .all(|fire| matches!(fire.admission, Admission::Admitted { .. }))
    );
    assert_eq!(first.waiting, 2);
    assert_eq!(waiting(), before, "both A fires stay planned and untouched");
    assert_eq!(state(&f, "flow"), (1, 1));
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM schedule_dropped", [], |r| r
                    .get::<_, usize>(0))?,
                0,
                "valid planned fires do not need quarantine"
            );
            assert_eq!(
                c.query_row(
                    "SELECT runs FROM rate_windows WHERE flow_id='flow'",
                    [],
                    |r| r.get::<_, u32>(0)
                )?,
                1,
                "no busy-fire retry loop or rate charge"
            );
            Ok(())
        })
        .unwrap();
    finish(&f, &old);
    let second = scheduler.tick().unwrap();
    assert_eq!(second.waiting, 0);
    assert_eq!(
        second
            .admitted
            .iter()
            .map(|fire| (fire.flow_id.as_str(), fire.trigger_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("flow", "waiting-1"), ("flow", "waiting-2")]
    );
    assert!(
        second
            .admitted
            .iter()
            .all(|fire| matches!(fire.admission, Admission::Admitted { .. }))
    );
    assert!(waiting().is_empty());
    assert_eq!(state(&f, "flow"), (2, 0));
}
