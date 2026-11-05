use super::*;
use crate::{
    Durability, NoHooks,
    hooks::{Boundary, Step},
};
use rusqlite::params;
use std::{
    sync::{
        Barrier,
        atomic::{AtomicBool, AtomicUsize},
        mpsc,
    },
    time::Duration,
};

struct Fixture {
    dir: std::path::PathBuf,
    store: Arc<Store>,
    cleanup: bool,
}
impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = crate::test_support::timestamp_scratch("run-store", tag);
        let store = Arc::new(Store::open(dir.join("core.db"), Durability::throwaway()).unwrap());
        store.write(|tx| {
            tx.execute_batch("CREATE TABLE terminal_facts(run_id TEXT PRIMARY KEY, facts TEXT, charge INTEGER);
                CREATE TABLE budget(used INTEGER); INSERT INTO budget VALUES(2);")?;
            Ok(())
        }).unwrap();
        Self {
            dir,
            store,
            cleanup: true,
        }
    }
    fn path(&self, hooks: Arc<dyn Hooks>) -> Arc<RunStore<Records>> {
        Arc::new(RunStore::new_staged(self.store.clone(), hooks))
    }
    fn count(&self) -> i64 {
        self.store
            .read(|c| Ok(c.query_row("SELECT count(*) FROM terminal_facts", [], |r| r.get(0))?))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[derive(Default)]
struct Records {
    trace: Arc<Mutex<Vec<&'static str>>>,
    facts: String,
}
impl RunRecords for Records {
    type Identity = ();
    type Refusal = ();
    fn append_identity(&mut self, _: &()) -> Result<Lsn> {
        self.trace.lock().unwrap().push("identity");
        Ok(Lsn {
            sequence: 0,
            offset: 123,
        })
    }
    fn sync_through(&mut self, cut: Lsn) -> Result<()> {
        assert_eq!(cut.offset, 123);
        lock_order::assert_sync_allowed();
        self.trace.lock().unwrap().push("sync");
        Ok(())
    }
    fn append_refusal(&mut self, _: ()) -> Result<()> {
        self.trace.lock().unwrap().push("refusal");
        Ok(())
    }
    fn all_calls_released(&self) -> bool {
        self.facts != "unreleased"
    }
}
struct Driver;
impl CloseDriver for Driver {
    fn stop_worker(&self, _: &str) -> Result<()> {
        lock_order::assert_lock_free();
        Ok(())
    }
    fn closed(&self, _: &str) {
        lock_order::assert_lock_free();
    }
}
struct Builder {
    fail_once: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
}
impl Default for Builder {
    fn default() -> Self {
        Self {
            fail_once: Arc::new(AtomicBool::new(false)),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }
}
struct Write {
    id: String,
    facts: String,
    charge: u64,
    fail_once: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
}
impl TerminalWrite for Write {
    fn apply(&self, tx: &Transaction<'_>) -> Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        tx.execute(
            "INSERT INTO terminal_facts VALUES(?1,?2,?3)",
            params![self.id, self.facts, self.charge],
        )?;
        tx.execute("UPDATE budget SET used=used+?1", [self.charge])?;
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(CoreError::Invalid("injected savepoint failure".into()));
        }
        Ok(())
    }
}
impl TerminalBuilder<Records> for Builder {
    fn is_success(&self) -> bool {
        false
    }
    fn build(
        &self,
        id: &str,
        records: &Records,
        reservations: &[Reservation],
    ) -> Result<Box<dyn TerminalWrite>> {
        Ok(Box::new(Write {
            id: id.into(),
            facts: records.facts.clone(),
            charge: reservations.iter().map(|r| r.amount).sum(),
            fail_once: self.fail_once.clone(),
            attempts: self.attempts.clone(),
        }))
    }
}
struct BudgetReader;
impl Budget for BudgetReader {
    fn committed(&self, c: &Connection, _: &str, _: &Window) -> Result<u64> {
        Ok(c.query_row("SELECT used FROM budget", [], |r| r.get(0))?)
    }
}
fn reservation(position: u64, amount: u64) -> Reservation {
    Reservation {
        position,
        amount,
        window: Window("dispatch:0".into()),
        audit_at: 100,
    }
}

#[test]
fn generation_and_routing_fence_append_nothing() {
    let f = Fixture::new("fence");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("r".into(), "f".into(), 7, Records::default())
        .unwrap();
    assert!(matches!(
        run.activate(6, |a| {
            a.records().facts.push_str("wrong");
            Ok(())
        }),
        Err(CoreError::OwnerLost { .. })
    ));
    run.activate(7, |a| {
        assert!(a.records().facts.is_empty());
        Ok(())
    })
    .unwrap();
    path.close(&run, &Builder::default(), &Driver).unwrap();
    assert!(matches!(
        run.activate(8, |_| -> Result<()> {
            panic!("Closed must fence even the current generation")
        }),
        Err(CoreError::OwnerLost { .. })
    ));
    assert!(matches!(run.enter(), Err(Routing::Closed)));
    assert!(path.get("r").is_none());
}

#[test]
fn abandoning_dispatch_inside_activation_does_not_relock_its_actor() {
    let f = Fixture::new("abandoned-dispatch");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("r".into(), "f".into(), 1, Records::default())
        .unwrap();
    run.activate(1, |a| {
        drop(a.dispatch_entrant());
        Ok(())
    })
    .unwrap();
    assert_eq!(run.actor.lock().entrants, 0);
    let error = run.activate(1, |a| {
        let _dispatch = a.dispatch_entrant();
        Err::<(), _>(CoreError::Invalid("authorization stopped".into()))
    });
    assert!(error.is_err());
    assert_eq!(run.actor.lock().entrants, 0);
    path.close(&run, &Builder::default(), &Driver).unwrap();
}

#[test]
fn close_drains_counted_dispatch_and_callbacks_retry() {
    let f = Fixture::new("drain");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("r".into(), "f".into(), 1, Records::default())
        .unwrap();
    let entrant = run
        .activate(1, |a| {
            a.records().facts.push_str("intent");
            Ok(a.dispatch_entrant())
        })
        .unwrap();
    let (stopped, stop) = mpsc::channel();
    struct Stop(mpsc::Sender<()>);
    impl CloseDriver for Stop {
        fn stop_worker(&self, _: &str) -> Result<()> {
            lock_order::assert_lock_free();
            self.0.send(()).unwrap();
            Ok(())
        }
        fn closed(&self, _: &str) {
            lock_order::assert_lock_free();
        }
    }
    let (p, r) = (path.clone(), run.clone());
    let close = std::thread::spawn(move || p.close(&r, &Builder::default(), &Stop(stopped)));
    stop.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(run.routing(), Routing::Closing);
    assert!(matches!(run.enter(), Err(Routing::Closing)));
    assert!(matches!(
        run.activate(1, |_| -> Result<()> { panic!("old activation") }),
        Err(CoreError::OwnerLost { .. })
    ));
    assert_eq!(f.count(), 0, "the counted dispatch prevents terminal build");
    entrant
        .finish(|records| {
            records.facts.push_str("+arrival");
            Ok(())
        })
        .unwrap();
    drop(entrant);
    close.join().unwrap().unwrap();
    assert_eq!(run.routing(), Routing::Closed);
    let facts: String = f
        .store
        .read(|c| Ok(c.query_row("SELECT facts FROM terminal_facts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(facts, "intent+arrival");
}

#[test]
fn concurrent_budget_checks_reserve_before_unlock_and_fold_without_gap() {
    let f = Fixture::new("budget");
    let path = f.path(Arc::new(NoHooks));
    let runs: Vec<_> = (0..8)
        .map(|i| {
            path.insert(format!("r{i}"), "f".into(), 1, Records::default())
                .unwrap()
        })
        .collect();
    let start = Arc::new(Barrier::new(runs.len()));
    let jobs: Vec<_> = runs
        .iter()
        .map(|r| {
            let (r, start, store) = (r.clone(), start.clone(), f.store.clone());
            std::thread::spawn(move || {
                start.wait();
                r.activate(1, |a| {
                    a.reserve(&store, &BudgetReader, reservation(1, 1), 5)
                })
                .unwrap()
            })
        })
        .collect();
    let allowed: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(
        allowed.iter().filter(|v| **v).count(),
        3,
        "two committed plus three live exhaust the budget"
    );
    assert_eq!(path.audit_at("f"), vec![100; 3]);
    let winner = runs
        .iter()
        .zip(&allowed)
        .find(|(_, allowed)| **allowed)
        .unwrap()
        .0;
    path.close(winner, &Builder::default(), &Driver).unwrap();
    let loser = runs
        .iter()
        .zip(&allowed)
        .find(|(_, allowed)| !**allowed)
        .unwrap()
        .0;
    assert!(
        !loser
            .activate(1, |a| a.reserve(
                &f.store,
                &BudgetReader,
                reservation(1, 1),
                5
            ))
            .unwrap(),
        "folding must not expose an under-count window"
    );
    assert_eq!(path.audit_at("f").len(), 2);
}

#[test]
fn saturation_sync_precedes_sqlite_and_refusal_follows_commit() {
    let f = Fixture::new("saturation");
    let path = f.path(Arc::new(NoHooks));
    let records = Records::default();
    let trace = records.trace.clone();
    let run = path.insert("r".into(), "f".into(), 1, records).unwrap();
    run.activate(1, |a| {
        a.saturate(&f.store, &(), (), |tx| {
            assert_eq!(*trace.lock().unwrap(), vec!["identity", "sync"]);
            tx.execute(
                "INSERT INTO terminal_facts VALUES('saturated','prefix',0)",
                [],
            )?;
            trace.lock().unwrap().push("transaction");
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        vec!["identity", "sync", "transaction", "refusal"]
    );
    assert_eq!(f.count(), 1);
    run.activate(1, |a| {
        a.saturate(&f.store, &(), (), |_| {
            Err::<(), _>(CoreError::Invalid("rollback".into()))
        })
    })
    .unwrap_err();
    assert_eq!(
        *trace.lock().unwrap(),
        vec![
            "identity",
            "sync",
            "transaction",
            "refusal",
            "identity",
            "sync"
        ]
    );
}

#[test]
fn staged_saturation_disables_with_sqlite_only_notice_and_card() {
    use crate::{Config, InstallGate, InstallRequest, Runtime, run_store_sql as sql};
    use basal_host::{MockCatalog, mock::MockHost};
    use serde_json::json;
    let f = Fixture::new("disable");
    let rt = Runtime::new(
        f.store.clone(),
        Arc::new(MockHost::new()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            install_gate: InstallGate::Off,
            ..Config::default()
        },
    );
    let installed = rt.install(&InstallRequest { script: "return 1;".into(), author: "operator".into(), loop_override: false, manifest: json!({"id":"flow","version":1,"state_epoch":1,"purpose":"Saturation test","trigger":{"schedule":{"interval":"1m"}}}).to_string() }).unwrap();
    rt.approve("flow", 1, &installed.code_hash, "approve-flow")
        .unwrap();
    f.store.write(|tx| {
        tx.execute_batch(sql::DDL)?;
        tx.execute_batch("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,zeroblob(32),'return 1','{}');
            INSERT INTO run_identities SELECT 'live',flow_id,flow_version,code_hash,snapshot_id FROM run_snapshots WHERE flow_id='flow' LIMIT 1;
            INSERT INTO run_identities SELECT 'history',flow_id,flow_version,code_hash,snapshot_id FROM run_snapshots WHERE flow_id='flow' LIMIT 1;
            INSERT INTO live_runs(run_id,flow_id,flow_version,trigger_id,trigger_kind,trigger,attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,self_input,params_input,grant_guards,template_incarnation,end_instance)
            VALUES('live','flow',1,'fire','schedule','{}',0,100,1,0,0,0,60000,'{}','{}','[]',NULL,0);")?;
        sql::insert_history(tx, &sql::RunHistory {
            run_id: "history".into(), flow_id: "flow".into(), flow_version: 1, trigger_id: "old-fire".into(), trigger_kind: "schedule".into(), trigger: json!({}), admitted_at: 0, admit_seq: 99, first_claim_at: Some(1), ended_at: 2, state: crate::model::RunState::Failed, error_kind: Some("worker".into()), error_detail: None, result: None, result_size: 0, call_counts: json!({}), writes_sent: 0, tokens_charged: 0, cost_microusd: 0, broken: 0, activation_count: 1, script_started: true, phase: "script".into(), deadline_exempt: false, history_lost: false, commit_seq: 1,
        })?;
        Ok(())
    }).unwrap();
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("live".into(), "flow".into(), 1, Records::default())
        .unwrap();
    let limits = crate::rate::RateLimits {
        saturated_windows_to_disable: 1,
        max_dispatches: 1,
        ..Default::default()
    };
    let result = run
        .activate(1, |a| a.saturate_dispatch(&f.store, &(), (), 1000, &limits))
        .unwrap();
    assert!(matches!(
        result,
        crate::rate::Take::Refused { disabled: true }
    ));
    f.store
        .read(|c| {
            assert_eq!(
                c.query_row::<String, _, _>(
                    "SELECT state FROM flows WHERE flow_id='flow'",
                    [],
                    |r| r.get(0)
                )?,
                "disabled"
            );
            assert_eq!(
                c.query_row::<i64, _, _>(
                    "SELECT saturated FROM rate_windows WHERE flow_id='flow'",
                    [],
                    |r| r.get(0)
                )?,
                1
            );
            assert_eq!(
                c.query_row::<i64, _, _>(
                    "SELECT count(*) FROM decision_cards WHERE kind='reenable' AND flow_id='flow'",
                    [],
                    |r| r.get(0)
                )?,
                1
            );
            let body: String = c.query_row(
                "SELECT body FROM outbox WHERE kind='flow.owner_notice'",
                [],
                |r| r.get(0),
            )?;
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                body["run_id"], "live",
                "live admission wins even over a newer history sequence"
            );
            assert!(body["error_kind"].is_null());
            Ok(())
        })
        .unwrap();
    f.store
        .write(|tx| {
            tx.execute("DELETE FROM live_runs", [])?;
            crate::owner_notice::auto_disabled(tx, "flow", 2000)
        })
        .unwrap();
    f.store
        .read(|c| {
            let body: String = c.query_row(
                "SELECT body FROM outbox WHERE kind='flow.owner_notice' ORDER BY at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["run_id"], "history");
            assert_eq!(body["error_kind"], "worker");
            Ok(())
        })
        .unwrap();
    f.store
        .write(|tx| {
            tx.execute("DELETE FROM run_history", [])?;
            crate::owner_notice::auto_disabled(tx, "flow", 3000)?;
            assert_eq!(
                tx.query_row::<i64, _, _>(
                    "SELECT count(*) FROM outbox WHERE kind='flow.owner_notice'",
                    [],
                    |r| r.get(0)
                )?,
                2,
                "no run means no notice"
            );
            Ok(())
        })
        .unwrap();
    rt.quiesce();
}

#[test]
fn build_and_commit_wait_hold_actor_and_accounting_until_closed() {
    let f = Fixture::new("locks-through-close");
    let (reached, wait) = mpsc::channel();
    let (resume, gate) = mpsc::channel();
    struct Pause {
        reached: mpsc::Sender<()>,
        gate: Mutex<mpsc::Receiver<()>>,
    }
    impl Hooks for Pause {
        fn at(&self, _: &str, boundary: &Boundary) -> Step {
            if *boundary == Boundary::RunStoreCommitted {
                lock_order::assert_lock_free();
                self.reached.send(()).unwrap();
                self.gate
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            Step::Continue
        }
    }
    let path = f.path(Arc::new(Pause {
        reached,
        gate: Mutex::new(gate),
    }));
    let run = path
        .insert("r".into(), "f".into(), 1, Records::default())
        .unwrap();
    let (p, r) = (path.clone(), run.clone());
    let close = std::thread::spawn(move || p.close(&r, &Builder::default(), &Driver));
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(run.routing(), Routing::Closing);
    assert!(
        run.actor.raw.try_lock().is_err(),
        "actor held after commit until Closed"
    );
    assert!(
        run.accounting.raw.try_lock().is_err(),
        "accounting held after commit until Closed"
    );
    assert!(
        matches!(run.enter(), Err(Routing::Closing)),
        "callback can retry without waiting for Closed"
    );
    resume.send(()).unwrap();
    close.join().unwrap().unwrap();
    assert_eq!(run.routing(), Routing::Closed);
    assert!(run.actor.raw.try_lock().is_ok());
    assert!(run.accounting.raw.try_lock().is_ok());
}

#[test]
fn failed_savepoint_retries_alone_and_does_not_double_fold() {
    let f = Fixture::new("savepoint");
    let path = f.path(Arc::new(NoHooks));
    let a = path
        .insert("a".into(), "fa".into(), 1, Records::default())
        .unwrap();
    let b = path
        .insert("b".into(), "fb".into(), 1, Records::default())
        .unwrap();
    for run in [&a, &b] {
        run.activate(1, |a| {
            a.reserve(&f.store, &BudgetReader, reservation(1, 2), 100)
        })
        .unwrap();
    }
    let bad = Builder::default();
    bad.fail_once.store(true, Ordering::SeqCst);
    let bad_attempts = bad.attempts.clone();
    let good = Builder::default();
    let good_attempts = good.attempts.clone();
    std::thread::scope(|scope| {
        let p = &path;
        let a = &a;
        scope.spawn(move || p.close(a, &bad, &Driver).unwrap());
        path.close(&b, &good, &Driver).unwrap();
    });
    assert_eq!(f.count(), 2);
    assert_eq!(bad_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(good_attempts.load(Ordering::SeqCst), 1);
    let used: u64 = f
        .store
        .read(|c| BudgetReader.committed(c, "", &Window("".into())))
        .unwrap();
    assert_eq!(used, 6);
    assert_eq!(a.routing(), Routing::Closed);
    assert_eq!(b.routing(), Routing::Closed);
}

struct CrashOn(Boundary);
impl Hooks for CrashOn {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        lock_order::assert_lock_free();
        if *boundary == self.0 {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

pub(crate) fn crash_case(boundary: Boundary) {
    let mut f = Fixture::new("crash");
    let path = f.path(Arc::new(CrashOn(boundary.clone())));
    // The records adapter stands in for the later replay stage. These are
    // already synced facts, independently of terminal projection durability.
    let durable = r#"{"unkeyed_dispatches":1,"key":"stable-key","ack":"answer","deadline":60000,"broken":3,"budget":9}"#;
    let log_path = f.dir.join("records-stub.json");
    {
        use std::io::Write;
        let mut log = std::fs::File::create(&log_path).unwrap();
        log.write_all(durable.as_bytes()).unwrap();
        log.sync_all().unwrap();
    }
    let run = path
        .insert(
            "r".into(),
            "f".into(),
            1,
            Records {
                facts: durable.into(),
                ..Records::default()
            },
        )
        .unwrap();
    assert_eq!(
        path.close(&run, &Builder::default(), &Driver),
        Err(CoreError::Cut)
    );
    assert_eq!(run.routing(), Routing::Closing);
    assert!(
        path.get("r").is_some(),
        "do not reclaim or release a slot after a crash"
    );
    drop(path);
    // Opening a fresh store discards the cut process. SQLite is authoritative
    // after commit; before commit recovery must keep the durable log facts.
    let dir = f.dir.clone();
    f.cleanup = false;
    drop(f);
    let reopened = Store::open(dir.join("core.db"), Durability::throwaway()).unwrap();
    let stored: Option<String> = reopened
        .read(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT facts FROM terminal_facts WHERE run_id='r'",
                [],
                |r| r.get(0),
            )
            .optional()?)
        })
        .unwrap();
    assert_eq!(stored.is_some(), boundary == Boundary::RunStoreCommitted);
    let log_facts = std::fs::read_to_string(&log_path).unwrap();
    let recovered: serde_json::Value =
        serde_json::from_str(stored.as_deref().unwrap_or(&log_facts)).unwrap();
    assert_eq!(recovered["unkeyed_dispatches"], 1);
    assert_eq!(recovered["key"], "stable-key");
    assert_eq!(recovered["ack"], "answer");
    assert_eq!(recovered["deadline"], 60000);
    assert_eq!(recovered["broken"], 3);
    assert_eq!(recovered["budget"], 9);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn success_requires_released_calls_and_failed_build_can_resume() {
    let f = Fixture::new("success");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert(
            "r".into(),
            "f".into(),
            1,
            Records {
                facts: "unreleased".into(),
                ..Records::default()
            },
        )
        .unwrap();
    struct Success;
    impl TerminalBuilder<Records> for Success {
        fn is_success(&self) -> bool {
            true
        }
        fn build(
            &self,
            id: &str,
            records: &Records,
            rows: &[Reservation],
        ) -> Result<Box<dyn TerminalWrite>> {
            Builder::default().build(id, records, rows)
        }
    }
    assert!(matches!(
        path.close(&run, &Success, &Driver),
        Err(CoreError::Invalid(_))
    ));
    assert_eq!(f.count(), 0);
    path.close(&run, &Builder::default(), &Driver).unwrap();
    assert_eq!(
        f.count(),
        1,
        "a failed/cancelled projection can resume Closing"
    );
    assert_eq!(
        run.actor.lock().generation,
        2,
        "resume must not advance generation twice"
    );
}

#[test]
fn settled_and_refunded_reservations_keep_the_original_audit_window() {
    let f = Fixture::new("settle");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("r".into(), "f".into(), 1, Records::default())
        .unwrap();
    let entrant = run
        .activate(1, |a| {
            assert!(a.reserve(&f.store, &BudgetReader, reservation(1, 5), 20)?);
            Ok(a.dispatch_entrant())
        })
        .unwrap();
    entrant
        .settle(1, &reservation(1, 0).window, 0, |records| {
            records.facts = "durable unsent refusal".into();
            Ok(())
        })
        .unwrap();
    drop(entrant);
    assert_eq!(path.audit_at("f"), vec![100]);
    run.activate(1, |a| {
        assert!(a.reserve(&f.store, &BudgetReader, reservation(2, 3), 5)?);
        Ok(())
    })
    .unwrap();
    assert_eq!(run.accounting.lock().runs["r"][0].amount, 0);
}

#[test]
fn crash_build_to_commit_preserves_invariant_c() {
    crash_case(Boundary::RunStoreBuilt);
}
#[test]
fn crash_commit_to_closed_preserves_invariant_c() {
    crash_case(Boundary::RunStoreCommitted);
}
#[test]
fn closed_boundary_set_has_crash_cases() {
    let declared: std::collections::BTreeSet<_> = include_str!("../hooks.rs")
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("RunStore"))
        .map(|line| line.trim_end_matches(',').to_owned())
        .collect();
    assert_eq!(
        declared,
        ["RunStoreBuilt".into(), "RunStoreCommitted".into()].into()
    );
}

#[test]
#[cfg(debug_assertions)]
fn lock_order_rejects_sqlite_to_actor_and_sync_wait() {
    let f = Fixture::new("order");
    let path = f.path(Arc::new(NoHooks));
    let run = path
        .insert("r".into(), "f".into(), 1, Records::default())
        .unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f
            .store
            .read(|_| run.activate(1, |_| Ok(())))))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.store.read(|_| {
            lock_order::assert_sync_allowed();
            Ok(())
        })))
        .is_err()
    );
    let _accounting = lock_order::enter(Lock::Accounting);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.activate(1, |_| Ok(()))))
            .is_err()
    );
}
