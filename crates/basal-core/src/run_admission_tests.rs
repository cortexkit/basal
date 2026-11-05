use super::*;
use crate::run_log::Lsn;
use crate::run_store_sql::{GcFloor, Meta};
use crate::{Clock, Config, Durability, InstallGate, NoHooks, Runtime};
use basal_host::{MockCatalog, mock::MockHost};
use serde_json::json;
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Watermark(EpochLsn);
impl AdmissionLog for Watermark {
    fn synced_lsn(&self) -> EpochLsn {
        self.0
    }
}
fn synced() -> EpochLsn {
    EpochLsn {
        log_epoch: 7,
        lsn: Lsn {
            sequence: 3,
            offset: 512,
        },
    }
}
struct NoWakes;
impl EventWakes for NoWakes {
    fn wake(&self, _: &Transaction<'_>, _: &Notice, _: i64) -> Result<usize> {
        Ok(0)
    }
    fn committed(&self) {}
}
struct CrashOn {
    boundary: Boundary,
    after: usize,
    seen: AtomicUsize,
}
impl Hooks for CrashOn {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        if *boundary == self.boundary && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.after
        {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}
struct Fixture {
    store: Arc<Store>,
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
impl Fixture {
    fn new(tag: &str) -> Self {
        Self::open(crate::test_support::timestamp_scratch("run-admission", tag))
    }
    fn open(dir: std::path::PathBuf) -> Self {
        let store = Arc::new(Store::open(dir.join("core.db"), Durability::throwaway()).unwrap());
        let catalog = MockCatalog::standard();
        catalog.set_event(
            "source",
            "changed",
            1,
            basal_host::EventDecl {
                headers: vec![],
                origin: basal_host::EventOrigin::External,
                body: basal_host::EventBody::Inline,
            },
        );
        let rt = Runtime::new(
            store.clone(),
            Arc::new(MockHost::new()),
            Arc::new(catalog),
            Arc::new(NoHooks),
            None,
            Config {
                clock: Clock::manual(1000),
                install_gate: InstallGate::Off,
                ..Config::default()
            },
        );
        Self {
            store,
            rt,
            dir,
            cleanup: true,
        }
    }
    fn schema(&self) {
        self.rt
            .store()
            .write(|tx| {
                sql::create_test_schema(
                    tx,
                    &Meta {
                        store_id: self.rt.store().store_id().into(),
                        log_epoch: 7,
                        log_hwm: synced(),
                        commit_seq: 0,
                        gc_floor: GcFloor {
                            log_epoch: 7,
                            first_retained: 3,
                            predecessor: None,
                            synced_lsn: synced().lsn,
                            live_starts: Default::default(),
                        },
                        migration_phase: "create".into(),
                    },
                )
            })
            .unwrap();
    }
    fn path(&self, hooks: Arc<dyn Hooks>) -> RunAdmission {
        RunAdmission::new_for_test(self.store.clone(), Arc::new(Watermark(synced())), hooks)
    }
    fn approve(&self, id: &str, version: u32, epoch: u32, trigger: Value) {
        let installed = self.rt.install(&crate::InstallRequest {
            script: "return 1;".into(), author: "operator".into(), loop_override: false,
            manifest: json!({"id":id,"version":version,"state_epoch":epoch,"purpose":"Admission test","trigger":trigger}).to_string(),
        }).unwrap();
        self.rt
            .approve(
                id,
                version,
                &installed.code_hash,
                &format!("approve-{id}-{version}"),
            )
            .unwrap();
    }
    fn event(&self, id: &str) {
        self.approve(
            id,
            1,
            1,
            json!({"events":[{"module":"source","name":"changed","version":1}]}),
        );
    }
    fn scheduled(&self, id: &str) {
        self.approve(id, 1, 1, json!({"schedule":{"interval":"1m"}}));
    }
    fn count(&self, table: &str) -> i64 {
        self.rt
            .store()
            .read(|c| Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?))
            .unwrap()
    }
    fn planned(&self, flow: &str, trigger: &str, due: i64) {
        self.rt.store().write(|tx| {
            tx.execute("INSERT INTO schedule_fires(flow_id,trigger_id,version,due_ms,payload,planned_at) VALUES (?1,?2,1,?3,'{}',0)", params![flow,trigger,due])?;
            Ok(())
        }).unwrap();
    }
    fn ids(&self) -> Vec<String> {
        self.rt
            .store()
            .read(|c| {
                Ok(
                    c.prepare("SELECT run_id FROM live_runs ORDER BY admit_seq,run_id")?
                        .query_map([], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?,
                )
            })
            .unwrap()
    }
}
fn ctx(max_runs: u32) -> AdmitContext {
    AdmitContext {
        now_ms: 1000,
        rate: crate::RateLimits {
            max_runs,
            ..Default::default()
        },
        default_deadline_ms: 600_000,
    }
}
fn notice(n: u64) -> Notice {
    Notice {
        published_at_ms: 1000,
        subject: "ck.account.event.source.changed.v1".into(),
        event_key: format!("{n:064x}"),
        digest: "a".repeat(64),
        headers: Default::default(),
    }
}
fn tick(path: &RunAdmission, at: i64) -> Result<TickReport> {
    path.tick_at(
        Timestamp::from_millisecond(at).unwrap(),
        &SchedulerConfig::default(),
        &ctx(100),
    )
}

#[test]
fn identity_survives_restored_counter_and_uses_full_id() {
    let f = Fixture::new("identity");
    f.event("flow");
    f.schema();
    let path = f.path(Arc::new(NoHooks));
    path.admit_events(&[notice(1)], &ctx(100), &NoWakes, || Ok(()))
        .unwrap();
    let first = f.ids()[0].clone();
    let prefix = format!("run-{}-1-", f.rt.store().store_id());
    let entropy = first
        .strip_prefix(&prefix)
        .expect("ID contains counter and entropy suffix");
    assert_eq!(entropy.len(), 16);
    assert!(u64::from_str_radix(entropy, 16).is_ok());
    f.rt.store()
        .write(|tx| {
            tx.execute("UPDATE meta SET value='1' WHERE key='next_run'", [])?;
            Ok(())
        })
        .unwrap();
    path.admit_events(&[notice(2)], &ctx(100), &NoWakes, || Ok(()))
        .unwrap();
    let ids = f.ids();
    assert_eq!(ids.len(), 2);
    assert_ne!(
        ids[0], ids[1],
        "restored counters must not reuse run identities"
    );
    f.rt.store()
        .read(|c| {
            for id in &ids {
                assert!(id.starts_with(&prefix));
                let live = sql::live(c, id)?.unwrap();
                assert_eq!(live.admit_lsn, synced());
                assert_eq!(live.admit_seq, 1);
                assert_eq!(live.deadline_ms, 600_000);
                assert_eq!(live.self_input, Value::Null);
                assert_eq!(live.params_input, Value::Null);
                assert_eq!(live.grant_guards, Value::Null);
                assert!(!live.end_instance);
                assert_eq!(sql::identity(c, id)?.unwrap().run_id, *id);
            }
            assert!(sql::identity(c, &first[..first.len() - 17])?.is_none());
            assert!(sql::live(c, &first[..first.len() - 17])?.is_none());
            assert_eq!(sql::meta(c)?.log_hwm, synced());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.count("runs"),
        0,
        "production model must not receive staged runs"
    );
}

#[test]
fn event_pull_counts_sequentially_and_acks_only_committed_receipts() {
    let f = Fixture::new("events");
    f.event("flow");
    f.schema();
    let path = f.path(Arc::new(NoHooks));
    let acked = Cell::new(false);
    let report = path
        .admit_events(&[notice(1), notice(2)], &ctx(1), &NoWakes, || {
            assert_eq!(f.count("live_runs"), 1);
            assert_eq!(f.count("event_receipts"), 2);
            acked.set(true);
            Ok(())
        })
        .unwrap();
    assert!(acked.get());
    assert_eq!((report.admitted, report.queued), (1, 1));
    let again = path
        .admit_events(&[notice(1), notice(2)], &ctx(1), &NoWakes, || Ok(()))
        .unwrap();
    assert_eq!(again.duplicate, 2);
    let later = AdmitContext {
        now_ms: 3_600_000,
        ..ctx(1)
    };
    assert_eq!(path.drain_event_backlog(&later).unwrap(), 1);
    assert_eq!(f.count("live_runs"), 2);
    assert_eq!(path.drain_event_backlog(&later).unwrap(), 0);
}

#[test]
fn admission_boundaries_preserve_atomicity_and_redelivery() {
    // This list is the admission stage's closed set. Later crash verification
    // combines it with the boundaries owned by the other lifecycle stages.
    for boundary in [Boundary::AdmissionWritten, Boundary::AdmissionCommitted] {
        crash_case(boundary);
    }
}

pub(crate) fn crash_case(boundary: Boundary) {
    let mut f = Fixture::new("crash");
    f.event("flow");
    f.schema();
    let dir = f.dir.clone();
    let crash = Arc::new(CrashOn {
        boundary: boundary.clone(),
        after: if boundary == Boundary::AdmissionWritten {
            2
        } else {
            1
        },
        seen: AtomicUsize::new(0),
    });
    let path = f.path(crash);
    let acked = Cell::new(false);
    assert_eq!(
        path.admit_events(&[notice(1), notice(2)], &ctx(100), &NoWakes, || {
            acked.set(true);
            Ok(())
        }),
        Err(CoreError::Cut)
    );
    assert!(!acked.get());
    drop(path);
    f.cleanup = false;
    drop(f);
    let restarted = Fixture::open(dir);
    let committed = boundary == Boundary::AdmissionCommitted;
    assert_eq!(restarted.count("live_runs"), if committed { 2 } else { 0 });
    assert_eq!(
        restarted.count("event_receipts"),
        if committed { 2 } else { 0 }
    );
    assert_eq!(
        restarted.count("run_identities"),
        if committed { 2 } else { 0 }
    );
    let before = restarted.ids();
    let path = restarted.path(Arc::new(NoHooks));
    let report = path
        .admit_events(&[notice(1), notice(2)], &ctx(100), &NoWakes, || Ok(()))
        .unwrap();
    if committed {
        assert_eq!(report.duplicate, 2);
        assert_eq!(restarted.ids(), before);
    } else {
        assert_eq!(report.admitted, 2);
    }
    assert_eq!(restarted.count("trigger_inbox"), 2);
    restarted
        .rt
        .store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT runs FROM rate_windows WHERE flow_id='flow'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            assert_eq!(
                c.query_row("SELECT value FROM meta WHERE key='next_run'", [], |r| {
                    r.get::<_, String>(0)
                })?,
                "3"
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn corrupt_scheduled_fire_rolls_back_its_rate_counter_identity_and_epoch_reset() {
    let f = Fixture::new("bad-fire");
    f.approve("flow", 1, 2, json!({"schedule":{"interval":"1m"}}));
    f.schema();
    // A due time outside Timestamp's range is detected after admission writes.
    f.rt.store()
        .write(|tx| {
            tx.execute("UPDATE flows SET state_epoch=1 WHERE flow_id='flow'", [])?;
            tx.execute(
                "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('flow','key','42',5,1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    f.planned("flow", "bad", i64::MAX);
    f.planned("flow", "good", 1000);
    let report = tick(&f.path(Arc::new(NoHooks)), 1000).unwrap();
    assert_eq!(report.new_runs().len(), 1);
    assert_eq!(f.count("schedule_dropped"), 1);
    assert_eq!(f.count("schedule_fires"), 0);
    assert_eq!(f.count("run_identities"), 1);
    assert_eq!(f.count("state_resets"), 1);
    assert_eq!(f.count("kv"), 0);
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT runs FROM rate_windows WHERE flow_id='flow'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            let live = sql::live(c, report.new_runs()[0])?.unwrap();
            assert_eq!(live.trigger_id, "good");
            assert_eq!(live.admit_seq, 1);
            assert_eq!(crate::state_epoch::recorded(c, "flow")?, 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn store_failure_aborts_entire_tick_including_schedule_advancement() {
    let f = Fixture::new("store-fatal");
    f.scheduled("a");
    f.scheduled("b");
    f.schema();
    let before =
        f.rt.store()
            .read(|c| {
                Ok(
                    c.prepare("SELECT flow_id,next_due_ms FROM schedules ORDER BY flow_id")?
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                )
            })
            .unwrap();
    f.rt.store().write(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_admission BEFORE INSERT ON live_runs WHEN NEW.flow_id='b' BEGIN SELECT RAISE(ABORT,'storage failure stand-in'); END;")?;
        Ok(())
    }).unwrap();
    let error = tick(&f.path(Arc::new(NoHooks)), 61_000).unwrap_err();
    assert!(matches!(error, CoreError::Store(_)), "{error:?}");
    for table in [
        "live_runs",
        "run_identities",
        "trigger_inbox",
        "rate_windows",
        "schedule_fires",
    ] {
        assert_eq!(f.count(table), 0, "{table} leaked from failed tick");
    }
    let after =
        f.rt.store()
            .read(|c| {
                Ok(
                    c.prepare("SELECT flow_id,next_due_ms FROM schedules ORDER BY flow_id")?
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                )
            })
            .unwrap();
    assert_eq!(after, before);
}

#[test]
fn draining_and_busy_flows_keep_planned_fires_without_blocking_other_flows() {
    let f = Fixture::new("held-fire");
    f.scheduled("busy");
    f.scheduled("other");
    f.schema();
    f.planned("busy", "old-epoch", 1000);
    let path = f.path(Arc::new(NoHooks));
    tick(&path, 1000).unwrap();
    let old = f.ids()[0].clone();
    f.approve("busy", 2, 2, json!({"schedule":{"interval":"1m"}}));
    f.planned("busy", "new-epoch", 1001);
    f.planned("other", "unrelated", 1001);
    let report = tick(&path, 1001).unwrap();
    assert_eq!(report.new_runs().len(), 1);
    assert_eq!(report.waiting, 1);
    assert_eq!(f.count("state_resets"), 0);
    f.rt.store()
        .write(|tx| admission::set_draining(tx, true))
        .unwrap();
    let report = tick(&path, 61_000).unwrap();
    assert_eq!(report.new_runs().len(), 0);
    assert_eq!(report.planned.len(), 2);
    assert_eq!(report.waiting, 3);
    f.rt.store()
        .write(|tx| {
            admission::set_draining(tx, false)?;
            // Terminal commit belongs to RunStore; release its live slot as a stub.
            tx.execute("DELETE FROM live_runs WHERE run_id=?1", [&old])?;
            Ok(())
        })
        .unwrap();
    let report = tick(&path, 61_000).unwrap();
    assert_eq!(report.waiting, 0);
    assert_eq!(report.new_runs().len(), 3);
    assert_eq!(f.count("state_resets"), 1);
}

#[test]
fn admission_epoch_fence_failure_never_acknowledges() {
    let f = Fixture::new("fence");
    f.event("flow");
    f.schema();
    let path = RunAdmission::new_for_test(
        f.store.clone(),
        Arc::new(Watermark(EpochLsn {
            log_epoch: 6,
            ..synced()
        })),
        Arc::new(NoHooks),
    );
    let acked = Cell::new(false);
    assert!(matches!(
        path.admit_events(&[notice(1)], &ctx(100), &NoWakes, || {
            acked.set(true);
            Ok(())
        }),
        Err(CoreError::Store(_))
    ));
    assert!(!acked.get());
    assert_eq!(f.count("live_runs"), 0);
    assert_eq!(f.count("event_receipts"), 0);
}

#[test]
fn template_admission_snapshots_all_inputs_and_one_shot_end_flag() {
    let f = Fixture::new("template-inputs");
    f.schema();
    let manifest = json!({"id":"package","version":1,"purpose":"Admission inputs",
        "params":{"at":{"type":"timestamp","max_ahead":"1d"},"note":{"type":"string","max_bytes":4096}},
        "template":{"max_per_agent":2,"max_lifetime":"1d"},
        "trigger":{"schedule":{"at":"$params.at"}}
    }).to_string();
    let guards =
        json!([{"param":"path","root":"/allowed/child","under":["/allowed"],"existed":true}]);
    // Seed the already-approved instance state: registration and instantiate
    // validation are covered by the package/template tests, not admission.
    f.store.write(|tx| {
        tx.execute("INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES ('package',1,'return 1;',?1,?2,1000)", params![manifest, crate::ids::code_hash("return 1;", &manifest).as_slice()])?;
        tx.execute("INSERT INTO flows(flow_id,owner,approved_version,created_at,package,first_approved_at) VALUES ('instance','agent_47120287c700722b',1,1000,'package',1000)", [])?;
        tx.execute("INSERT INTO template_instances(flow_id,params,grant_guards,expires_at,incarnation) VALUES ('instance',?1,?2,3600000,9)", params![json!({"note":"Grüße","path":"/allowed/child"}).to_string(), guards.to_string()])?;
        Ok(())
    }).unwrap();
    let path = f.path(Arc::new(NoHooks));
    let admitted = path
        .admit_trigger(
            "instance",
            "manual",
            JsonText::new("{\"payload\":7}").unwrap(),
            &ctx(100),
        )
        .unwrap();
    let id = admitted.run_id().unwrap();
    f.store.write(|tx| {
        tx.execute("UPDATE template_instances SET params='{}',grant_guards='[]',incarnation=10 WHERE flow_id='instance'", [])?;
        Ok(())
    }).unwrap();
    f.store
        .read(|c| {
            let live = sql::live(c, id)?.unwrap();
            assert_eq!(
                live.self_input,
                json!({"agent_id":"agent_47120287c700722b"})
            );
            assert_eq!(
                live.params_input,
                json!({"note":"Grüße","path":"/allowed/child"})
            );
            assert_eq!(live.grant_guards, guards);
            assert_eq!(live.template_incarnation, Some(9));
            assert!(live.end_instance);
            assert_eq!(live.trigger, json!({"payload":7}));
            let identity = sql::identity(c, id)?.unwrap();
            assert_eq!(identity.flow_version, 1);
            assert_eq!(
                identity.code_hash,
                crate::ids::code_hash(
                    "return 1;",
                    &c.query_row("SELECT manifest FROM package_versions", [], |r| r
                        .get::<_, String>(0))?
                )
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn production_constructors_stay_on_original_admission() {
    let f = Fixture::new("production");
    f.event("flow");
    let admitted =
        f.rt.admit_trigger("flow", "manual", JsonText::null())
            .unwrap();
    assert_eq!(
        admitted.run_id(),
        Some(format!("run-{}-1", f.store.store_id()).as_str())
    );
    assert_eq!(f.count("runs"), 1);
    assert!(!f.dir.join("run-log").exists());
    f.store
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name='live_runs'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn history_lost_holds_the_slot_and_blocks_a_new_state_epoch() {
    let f = Fixture::new("history-lost-slot");
    f.schema();
    f.scheduled("busy");
    let path = f.path(Arc::new(NoHooks));
    let Admission::Admitted { run_id } = path
        .admit_trigger("busy", "old", JsonText::null(), &ctx(100))
        .unwrap()
    else {
        panic!("admitted")
    };
    f.store
        .write(|tx| {
            tx.execute(
                "UPDATE live_runs SET history_lost=1 WHERE run_id=?1",
                [&run_id],
            )?;
            Ok(())
        })
        .unwrap();
    f.approve("busy", 2, 2, json!({"schedule":{"interval":"1m"}}));
    assert_eq!(
        path.admit_trigger("busy", "new", JsonText::null(), &ctx(100))
            .unwrap(),
        Admission::StateBusy
    );
    assert_eq!(f.count("live_runs"), 1);
    assert!(f.store.read(|c| sql::history_lost(c, &run_id)).unwrap());
}
