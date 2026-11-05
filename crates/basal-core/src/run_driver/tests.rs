use super::*;
use crate::{
    NoHooks,
    model::RunState,
    run_log::{Lsn, RecoveredRecord},
    run_records::{Deferred, DispatchCount, Record, RecordLog},
    run_store::{TerminalWrite, lock_order},
    run_store_sql::{self as sql, EpochLsn, GcFloor, Meta},
};
use basal_proto::{CallKind, Primitive};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

#[path = "verification_tests.rs"]
mod verification_matrix;

#[derive(Default)]
struct MemoryLog {
    records: Mutex<Vec<Record>>,
    synced: Mutex<usize>,
}
impl RecordLog for MemoryLog {
    fn append(&self, record: Record) -> Result<Lsn> {
        let mut records = self.records.lock().unwrap();
        records.push(record);
        Ok(Lsn {
            sequence: 1,
            offset: records.len() as u64,
        })
    }
    fn sync_through(&self, lsn: Lsn) -> Result<()> {
        lock_order::assert_sync_allowed();
        let mut synced = self.synced.lock().unwrap();
        *synced = (*synced).max(lsn.offset as usize);
        Ok(())
    }
}
impl MemoryLog {
    fn recover(self: &Arc<Self>, id: &str) -> Records {
        let mut records = self.records.lock().unwrap();
        records.truncate(*self.synced.lock().unwrap());
        let recovered: Vec<_> = records
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, record)| RecoveredRecord {
                lsn: Lsn {
                    sequence: 1,
                    offset: i as u64 + 1,
                },
                record,
            })
            .collect();
        Records::recover(id.into(), 10, self.clone(), &recovered).unwrap()
    }
}

struct Cut(Mutex<Option<Boundary>>);
impl Hooks for Cut {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        lock_order::assert_lock_free();
        let mut cut = self.0.lock().unwrap();
        if cut.as_ref() == Some(boundary) {
            cut.take();
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

struct Service {
    now: AtomicI64,
    gate: Mutex<Gate>,
    log: Arc<MemoryLog>,
    sent: Mutex<Vec<Intent>>,
    replies: Mutex<VecDeque<Reply>>,
    acks: Mutex<Vec<u64>>,
    refusals: Mutex<Vec<u64>>,
    closed: AtomicBool,
    stopped: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    file_log: Mutex<Option<Arc<crate::run_log::RunLog<Record>>>>,
}
impl CloseDriver for Service {
    fn stop_worker(&self, _: &str) -> Result<()> {
        lock_order::assert_lock_free();
        if let Some(sender) = self.stopped.lock().unwrap().take() {
            sender.send(()).unwrap();
        }
        Ok(())
    }
    fn closed(&self, _: &str) {
        lock_order::assert_lock_free();
        self.closed.store(true, Ordering::Release);
    }
}
fn call(position: u64, op: &str) -> HostCall {
    HostCall {
        position,
        kind: CallKind::Op {
            module: "test".into(),
            op: op.into(),
        },
        args: JsonText::null(),
    }
}
fn arrival(position: u64, value: &str) -> Arrival {
    let value = JsonText::new(value).unwrap();
    Arrival {
        position,
        arrival_seq: 0,
        outcome: "fulfilled".into(),
        settlement: Settlement::Fulfilled,
        value_hash: crate::ids::payload_hash(Settlement::Fulfilled, &value),
        value,
        handle: None,
        usage: None,
        cost_micros: None,
        kv_effect: None,
        durability: Default::default(),
    }
}
impl Services for Service {
    fn install(&self, _: &LiveRun) -> Result<Gate> {
        lock_order::assert_lock_free();
        Ok(*self.gate.lock().unwrap())
    }
    fn prepare(&self, _: &LiveRun, call: &HostCall, state: &State) -> Result<Prepared> {
        lock_order::assert_lock_free();
        if crate::kv::is_kv(&call.kind) {
            return Ok(Prepared::Kv(crate::kv::KvLimits::default()));
        }
        if call.kind.is_synchronous() {
            return Ok(Prepared::Sync {
                value: JsonText::new("25").unwrap(),
                clock_ms: Some(self.now_ms()),
            });
        }
        let op = CallIdentity::new(call.clone(), StoredClass::Query).op;
        if op == "local" {
            return Ok(Prepared::Local {
                value: JsonText::null(),
                effect: Some(KvEffect::Set {
                    key: "counter".into(),
                    value: JsonText::new("7").unwrap(),
                }),
            });
        }
        if op == "read" {
            return Ok(Prepared::Local {
                value: JsonText::new(state.overlay.get("counter").unwrap().clone().unwrap())
                    .unwrap(),
                effect: None,
            });
        }
        if op == "refuse" {
            return Ok(Prepared::Refused {
                class: StoredClass::Query,
                value: JsonText::new("{\"code\":\"denied\"}").unwrap(),
                notify: true,
            });
        }
        let class = match op.as_str() {
            "write" => StoredClass::Mutation,
            "keyed" => StoredClass::KeyedMutation,
            _ => StoredClass::Query,
        };
        Ok(Prepared::Remote(Box::new(Intent {
            call: CallIdentity::new(call.clone(), class),
            idempotency_key: None,
            frozen_request: JsonText::new("{\"frozen\":true}").unwrap(),
            attempts: 1,
            dispatch_count_window: Some(DispatchCount {
                window_start: 0,
                count: 1,
            }),
            audit_at: self.now_ms(),
            reservation: (op == "model").then_some(crate::run_records::Reservation {
                input_tokens: 5,
                output_tokens: 3,
                cost_micros: 2,
                window_start: 0,
            }),
        })))
    }
    fn dispatch(&self, _: &LiveRun, intent: &Intent) -> Result<Reply> {
        lock_order::assert_lock_free();
        if let Some(file) = self.file_log.lock().unwrap().as_ref() {
            assert!(file.synced_lsn() > Lsn::default());
        } else {
            let records = self.log.records.lock().unwrap();
            let synced = *self.log.synced.lock().unwrap();
            assert!(
                records[..synced]
                    .iter()
                    .any(|r| matches!(&r.body, Body::Intent(i) if i == intent)),
                "host observed an unsynced Intent"
            );
        }
        self.sent.lock().unwrap().push(intent.clone());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Reply::Completed(arrival(intent.call.position, "42"))))
    }
    fn dispatch_committed(&self, _: &LiveRun, position: u64, _: Option<&str>) {
        lock_order::assert_lock_free();
        self.acks.lock().unwrap().push(position);
    }
    fn refusal_committed(&self, _: &LiveRun, position: u64) {
        lock_order::assert_lock_free();
        self.refusals.lock().unwrap().push(position);
    }
    fn dispatch_check(&self, tx: &rusqlite::Transaction<'_>, _: &LiveRun) -> Result<()> {
        tx.execute("UPDATE meta SET value=value WHERE key='store_id'", [])?;
        Ok(())
    }
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::Acquire)
    }
}

#[test]
fn reservations_settle_in_the_original_windows_and_fold_once() {
    let f = Fixture::new();
    let a = f.claim(0);
    let mut result = arrival(0, "7");
    result.usage = Some(crate::run_records::Usage {
        input_tokens: 4,
        output_tokens: 2,
    });
    result.cost_micros = Some(1);
    f.service
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Completed(result));
    f.send(&a, 0, "model");
    let rows = accounting::rows(&a.state().unwrap(), &budgets()).unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.window.0.starts_with("token:"))
            .unwrap()
            .amount,
        6
    );
    a.finish(f.terminal(RunState::Failed)).unwrap();
    f.store.read(|conn| {
        let h = sql::history(conn,&f.live.run_id)?.unwrap();
        assert_eq!((h.tokens_charged,h.cost_microusd),(6,1));
        let (pending,input,output): (i64,i64,i64) = conn.query_row("SELECT reserved,input_tokens,output_tokens FROM token_windows WHERE window_start=0",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        assert_eq!((pending,input,output),(0,4,2));
        let state: String = conn.query_row("SELECT state FROM token_ledger",[],|r| r.get(0))?;
        assert_eq!(state,"settled");
        let cost: f64 = conn.query_row("SELECT charged FROM classify_costs",[],|r| r.get(0))?;
        assert_eq!(cost,0.000001);
        Ok(())
    }).unwrap();
}

#[test]
fn recovered_unactivated_run_counts_against_other_runs_budget() {
    let f = Fixture::new();
    let a = f.claim(0);
    drop(a.issue(call(0, "model")).unwrap());
    let recovered = f.log.recover(&f.live.run_id);
    let registry = Arc::new(RunStore::new_staged(f.store.clone(), Arc::new(NoHooks)));
    let mut limits = budgets();
    limits.tokens = 8;
    let driver = Driver::new_staged(
        f.store.clone(),
        registry,
        f.service.clone(),
        Arc::new(NoHooks),
        limits,
    );
    driver.register(&f.live, recovered).unwrap();
    let mut other = f.live.clone();
    other.run_id = "another-run".into();
    other.trigger_id = "another-trigger".into();
    f.store
        .write(|tx| {
            let mut identity = sql::identity(tx, &f.live.run_id)?.unwrap();
            identity.run_id = other.run_id.clone();
            sql::insert_identity(tx, &identity)?;
            sql::insert_live(tx, &other)
        })
        .unwrap();
    driver
        .register(
            &other,
            Records::new(other.run_id.clone(), 10, f.log.clone()),
        )
        .unwrap();
    let Claim::Active(a) = driver.claim(other, 0).unwrap() else {
        panic!("gate closed")
    };
    assert!(
        matches!(a.issue(call(0,"model")),Err(CoreError::Invalid(message)) if message == "token budget exhausted")
    );
    assert!(f.service.sent.lock().unwrap().is_empty());
}

struct Worker {
    welcome: basal_proto::Welcome,
    messages: VecDeque<basal_proto::WorkerMessage>,
    sent: Vec<basal_proto::ParentMessage>,
    killed: bool,
}
impl Worker {
    fn new(messages: Vec<basal_proto::WorkerMessage>) -> Self {
        Self {
            welcome: basal_proto::Welcome {
                protocol_version: basal_proto::PROTOCOL_VERSION,
                engine: "staged-test".into(),
                prelude_hash: basal_proto::PreludeHash([0; 32]),
                codemode_prelude_hash: basal_proto::PreludeHash([0; 32]),
                confinement: basal_proto::Confinement::None,
            },
            messages: messages.into(),
            sent: Vec::new(),
            killed: false,
        }
    }
}
impl crate::channel::WorkerChannel for Worker {
    fn welcome(&self) -> &basal_proto::Welcome {
        &self.welcome
    }
    fn send(
        &mut self,
        message: &basal_proto::ParentMessage,
    ) -> std::result::Result<(), crate::ChannelError> {
        self.sent.push(message.clone());
        Ok(())
    }
    fn recv(
        &mut self,
        _: Duration,
    ) -> std::result::Result<basal_proto::WorkerMessage, crate::ChannelError> {
        self.messages.pop_front().ok_or(crate::ChannelError::Closed)
    }
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("driver uses timed receives")
    }
    fn kill(&mut self) {
        self.killed = true;
    }
}
fn request() -> basal_proto::ActivationRequest {
    basal_proto::ActivationRequest {
        activation_id: 0,
        profile: basal_proto::Profile::Flow,
        tools: Vec::new(),
        prelude_hash: basal_proto::PreludeHash([0; 32]),
        script: "return 1".into(),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        params_input: JsonText::null(),
        budgets: basal_proto::Budgets::default(),
        prefix: Vec::new(),
    }
}

#[test]
fn worker_loop_serves_real_kv_overlay_and_shared_sync_release_order() {
    use basal_proto::{ActivationResult, ParentMessage, WorkerMessage};
    let f = Fixture::new();
    let a = f.claim(0);
    let kv = |position, primitive, args: &str| HostCall {
        position,
        kind: CallKind::Primitive(primitive),
        args: JsonText::new(args).unwrap(),
    };
    let mut worker = Worker::new(vec![
        WorkerMessage::HostCall(kv(0, Primitive::KvSet, "{\"key\":\"x\",\"value\":3}")),
        WorkerMessage::HostCall(kv(1, Primitive::Now, "null")),
        WorkerMessage::Blocked { awaiting: vec![0] },
        WorkerMessage::HostCall(kv(2, Primitive::KvGet, "{\"key\":\"x\"}")),
        WorkerMessage::Blocked { awaiting: vec![2] },
        WorkerMessage::Finished {
            activation_id: a.generation,
            result: ActivationResult::Completed {
                value: JsonText::new("3").unwrap(),
            },
        },
    ]);
    let end = a
        .drive(
            &mut worker,
            request(),
            Arc::new(Effects),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(end, crate::ActivationEnd::Succeeded { value: "3".into() });
    let deliveries: Vec<_> = worker
        .sent
        .iter()
        .filter_map(|m| {
            if let ParentMessage::Deliver(o) = m {
                Some((o.position, o.delivery_order, o.value.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(deliveries, vec![(1, 0, "20"), (0, 1, "true"), (2, 2, "3")]);
    assert!(!worker.killed);
    f.store
        .read(|conn| {
            let bytes: i64 =
                conn.query_row("SELECT bytes FROM kv WHERE key='x'", [], |r| r.get(0))?;
            assert_eq!(bytes, 2);
            assert!(sql::call(conn, &f.live.run_id, 0)?.is_none());
            Ok(())
        })
        .unwrap();
}

#[test]
fn expired_run_never_resends_or_starts_worker() {
    let f = Fixture::new();
    let a = f.claim(0);
    drop(a.issue(call(0, "keyed")).unwrap());
    f.service.now.store(2000, Ordering::Release);
    let mut worker = Worker::new(Vec::new());
    assert!(
        matches!(a.drive(&mut worker,request(),Arc::new(Effects),Duration::from_secs(5)).unwrap(),crate::ActivationEnd::Failed { kind, .. } if kind == "deadline")
    );
    assert!(worker.sent.is_empty());
    assert!(f.service.sent.lock().unwrap().is_empty());
    assert!(
        f.store
            .read(|c| sql::call(c, &f.live.run_id, 0))
            .unwrap()
            .is_some()
    );
}

struct Effects;
impl TerminalEffects for Effects {
    fn capture(&self, _: &LiveRun, _: &State, _: RunState) -> Result<Box<dyn TerminalWrite>> {
        Ok(Box::new(Effects))
    }
}
impl TerminalWrite for Effects {
    fn apply(&self, tx: &rusqlite::Transaction<'_>) -> Result<()> {
        tx.execute("INSERT INTO terminal_effects VALUES(1)", [])?;
        Ok(())
    }
}
fn budgets() -> Budgets {
    Budgets {
        dispatch_window_ms: 1000,
        token_window_ms: 1000,
        dispatches: 100,
        tokens: 100,
        cost_micros: 100,
        cost_window: None,
    }
}

struct Fixture {
    store: Arc<Store>,
    log: Arc<MemoryLog>,
    registry: Arc<RunStore<Records>>,
    service: Arc<Service>,
    driver: Arc<Driver>,
    live: LiveRun,
    cut: Arc<Cut>,
}
impl Fixture {
    fn new() -> Self {
        let dir = crate::test_support::timestamp_scratch("run-driver", "staged");
        let store =
            Arc::new(Store::open(dir.join("store.db"), crate::Durability::default()).unwrap());
        let epoch = EpochLsn {
            log_epoch: 1,
            lsn: Lsn::default(),
        };
        let live = LiveRun {
            run_id: "run-driver".into(),
            flow_id: "flow".into(),
            flow_version: 1,
            trigger_id: "trigger".into(),
            trigger_kind: "manual".into(),
            trigger: json!({}),
            attempt: 0,
            admitted_at: 10,
            admit_seq: 1,
            admit_lsn: epoch,
            deadline_ms: 1000,
            self_input: json!({}),
            params_input: json!({}),
            grant_guards: json!([]),
            template_incarnation: None,
            end_instance: false,
        };
        store.write(|tx| {
            sql::create_test_schema(tx, &Meta { store_id: store.store_id().into(), log_epoch: 1, log_hwm: epoch, commit_seq: 0, gc_floor: GcFloor { log_epoch: 1, first_retained: 0, predecessor: None, synced_lsn: Lsn::default(), live_starts: Default::default() }, migration_phase: "test".into() })?;
            tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,?1,'return 1','{}')", [crate::ids::code_hash("return 1","{}").as_slice()])?;
            tx.execute_batch("CREATE TABLE terminal_effects(value INTEGER)")?;
            sql::insert_identity(tx, &sql::RunIdentity { run_id: live.run_id.clone(), flow_id: live.flow_id.clone(), flow_version: 1, code_hash: crate::ids::code_hash("return 1","{}"), snapshot_id: 1 })?;
            sql::insert_live(tx,&live)
        }).unwrap();
        let log = Arc::new(MemoryLog::default());
        let service = Arc::new(Service {
            now: AtomicI64::new(20),
            gate: Mutex::new(Gate::Open),
            log: log.clone(),
            sent: Mutex::new(Vec::new()),
            replies: Mutex::new(VecDeque::new()),
            acks: Mutex::new(Vec::new()),
            refusals: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            stopped: Mutex::new(None),
            file_log: Mutex::new(None),
        });
        let cut = Arc::new(Cut(Mutex::new(None)));
        let registry = Arc::new(RunStore::new_staged(store.clone(), Arc::new(NoHooks)));
        registry
            .insert(
                live.run_id.clone(),
                live.flow_id.clone(),
                0,
                Records::new(live.run_id.clone(), 10, log.clone()),
            )
            .unwrap();
        let driver = Driver::new_staged(
            store.clone(),
            registry.clone(),
            service.clone(),
            cut.clone(),
            budgets(),
        );
        Self {
            store,
            log,
            registry,
            service,
            driver,
            live,
            cut,
        }
    }
    fn claim(&self, generation: u64) -> Activation {
        match self.driver.claim(self.live.clone(), generation).unwrap() {
            Claim::Active(a) => *a,
            _ => panic!("gate did not pass"),
        }
    }
    fn send(&self, a: &Activation, position: u64, op: &str) {
        match a.issue(call(position, op)).unwrap() {
            Issued::Dispatch(d) => d.send().unwrap(),
            _ => panic!("not remote"),
        }
    }
    fn terminal(&self, state: RunState) -> Terminal {
        Terminal::new(self.live.clone(), state, 50, budgets(), Arc::new(Effects))
    }
}

#[test]
fn closing_counts_dispatch_before_its_append_to_sync_gap() {
    use std::sync::mpsc;
    struct Pause {
        arrived: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl Hooks for Pause {
        fn at(&self, _: &str, boundary: &Boundary) -> Step {
            lock_order::assert_lock_free();
            if boundary == &Boundary::DriverAppended {
                self.arrived.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            Step::Continue
        }
    }
    let f = Fixture::new();
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let driver = Driver::new_staged(
        f.store.clone(),
        f.registry.clone(),
        f.service.clone(),
        Arc::new(Pause {
            arrived: arrived_tx,
            resume: Mutex::new(resume_rx),
        }),
        budgets(),
    );
    let Claim::Active(a) = driver.claim(f.live.clone(), 0).unwrap() else {
        panic!("gate closed")
    };
    let issuing = std::thread::spawn(move || a.issue(call(0, "write")));
    arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    *f.service.stopped.lock().unwrap() = Some(stopped_tx);
    let (done_tx, done_rx) = mpsc::channel();
    let terminal = f.terminal(RunState::Cancelled);
    let closer = std::thread::spawn(move || {
        done_tx.send(driver.cancel(terminal)).unwrap();
    });
    stopped_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    resume_tx.send(()).unwrap();
    let Issued::Dispatch(dispatch) = issuing.join().unwrap().unwrap() else {
        panic!("not dispatch")
    };
    dispatch.send().unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    closer.join().unwrap();
    let detail = f
        .store
        .read(|c| sql::call(c, &f.live.run_id, 0))
        .unwrap()
        .unwrap();
    assert_eq!(detail.outcome, sql::CallOutcome::Fulfilled);
}

#[test]
fn park_wake_does_not_lose_arrival_racing_suspension() {
    let f = Fixture::new();
    let a = f.claim(0);
    f.service
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Accepted("handle".into()));
    f.send(&a, 0, "query");
    assert!(a.park(&[0]).unwrap());
    let run = f.registry.get(&f.live.run_id).unwrap();
    let entrant = run.enter().unwrap_or_else(|_| panic!("closed"));
    entrant
        .finish(|records| {
            let mut arrived = arrival(0, "9");
            arrived.handle = Some("handle".into());
            arrived.durability.host_callback = true;
            records
                .append(a.generation, Body::Arrived(arrived))
                .map(|_| ())
        })
        .unwrap();
    drop(entrant);
    assert!(f.driver.wake(&f.live.run_id).unwrap());
    assert!(!f.driver.wake(&f.live.run_id).unwrap());
    let next = f.claim(a.generation);
    assert!(!next.park(&[0]).unwrap());
    assert_eq!(next.release(&[0]).unwrap().unwrap().value.as_str(), "9");
    assert_eq!(next.state().unwrap().activation_count, 2);
}

#[test]
fn terminal_effect_failure_rolls_back_summary_overlay_counts_and_tombstones() {
    struct Fail;
    impl TerminalEffects for Fail {
        fn capture(&self, _: &LiveRun, _: &State, _: RunState) -> Result<Box<dyn TerminalWrite>> {
            Ok(Box::new(Fail))
        }
    }
    impl TerminalWrite for Fail {
        fn apply(&self, tx: &rusqlite::Transaction<'_>) -> Result<()> {
            tx.execute("INSERT INTO terminal_effects VALUES(99)", [])?;
            Err(CoreError::Invalid("injected terminal failure".into()))
        }
    }
    let f = Fixture::new();
    let a = f.claim(0);
    a.issue(call(0, "local")).unwrap();
    f.send(&a, 1, "write");
    let mut terminal = f.terminal(RunState::Cancelled);
    terminal.effects = Arc::new(Fail);
    assert!(a.finish(terminal).is_err());
    f.store
        .read(|conn| {
            assert!(sql::live(conn, &f.live.run_id)?.is_some());
            assert!(sql::history(conn, &f.live.run_id)?.is_none());
            for table in [
                "kv",
                "run_calls",
                "tombstones",
                "obligations",
                "terminal_effects",
                "rate_windows",
            ] {
                let count: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(count, 0, "{table}");
            }
            Ok(())
        })
        .unwrap();
    a.finish(f.terminal(RunState::Cancelled)).unwrap();
    assert_eq!(
        f.store
            .read(
                |c| Ok(c.query_row("SELECT dispatches FROM rate_windows", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .unwrap(),
        1
    );
}

#[test]
fn real_runlog_driver_commits_history_watermark_and_terminal_receipts() {
    use crate::run_log::{Config, Identity, RunLog, SqliteRegistry};
    let f = Fixture::new();
    let directory = crate::test_support::timestamp_scratch("run-driver", "real-log");
    let log = Arc::new(
        RunLog::create(
            Config {
                directory,
                identity: Identity {
                    store_id: [7; 16],
                    log_epoch: 1,
                },
                lease_epoch: f.store.lease_epoch(),
                target_size: Config::TARGET_SIZE,
                hooks: Arc::new(NoHooks),
            },
            Arc::new(SqliteRegistry::new(f.store.clone(), 1)),
        )
        .unwrap(),
    );
    *f.service.file_log.lock().unwrap() = Some(log.clone());
    let registry = Arc::new(RunStore::new_bound(
        f.store.clone(),
        Arc::new(NoHooks),
        Arc::new(log.watermark()),
    ));
    let driver = Driver::new_staged(
        f.store.clone(),
        registry,
        f.service.clone(),
        Arc::new(NoHooks),
        budgets(),
    );
    driver
        .register(
            &f.live,
            Records::new(f.live.run_id.clone(), 10, log.clone()),
        )
        .unwrap();
    let Claim::Active(a) = driver.claim(f.live.clone(), 0).unwrap() else {
        panic!("gate closed")
    };
    f.send(&a, 0, "keyed");
    a.release(&[0]).unwrap().unwrap();
    let mut terminal = f.terminal(RunState::Succeeded);
    terminal.result = Some(json!({"answer":42}));
    a.finish(terminal).unwrap();
    f.store
        .read(|conn| {
            let history = sql::history(conn, &f.live.run_id)?.unwrap();
            assert_eq!(history.result, Some(json!({"answer":42})));
            assert_eq!(history.commit_seq, 1);
            assert!(sql::call(conn, &f.live.run_id, 0)?.is_none());
            assert_eq!(
                sql::obligation(conn, &f.live.run_id, 0)?.unwrap().state,
                sql::ObligationState::Settled
            );
            assert!(sql::meta(conn)?.log_hwm.lsn > Lsn::default());
            Ok(())
        })
        .unwrap();
}

#[test]
fn gate_deferral_writes_no_claim_and_durable_deadline_is_fixed() {
    let f = Fixture::new();
    *f.service.gate.lock().unwrap() = Gate::Deferred;
    assert!(matches!(
        f.driver.claim(f.live.clone(), 0).unwrap(),
        Claim::Deferred
    ));
    assert!(f.log.records.lock().unwrap().is_empty());
    *f.service.gate.lock().unwrap() = Gate::Open;
    let first = f.claim(0);
    first.requeue(true).unwrap();
    f.service.now.store(100, Ordering::Release);
    let second = f.claim(first.generation);
    assert_eq!(second.state().unwrap().claim.unwrap().deadline_at, 1020);
    assert_eq!(second.state().unwrap().broken, 1);
    assert!(matches!(
        first.script_started(),
        Err(CoreError::OwnerLost { .. })
    ));
}

#[test]
fn local_overlay_sync_order_and_non_success_terminal_detail() {
    for terminal_state in [
        RunState::Failed,
        RunState::EngineMismatch,
        RunState::Cancelled,
    ] {
        let f = Fixture::new();
        let a = f.claim(0);
        a.issue(call(0, "local")).unwrap();
        a.issue(call(1, "read")).unwrap();
        let issued = HostCall {
            position: 2,
            kind: CallKind::Primitive(Primitive::Now),
            args: JsonText::null(),
        };
        let Issued::Reply(sync) = a.issue(issued).unwrap() else {
            panic!("no sync reply")
        };
        assert_eq!(sync.delivery_order, 0);
        assert_eq!(a.release(&[1]).unwrap().unwrap().value.as_str(), "7");
        assert_eq!(a.release(&[0]).unwrap().unwrap().delivery_order, 2);
        a.issue(call(3, "refuse")).unwrap();
        f.send(&a, 4, "write");
        a.requeue(true).unwrap();
        a.finish(f.terminal(terminal_state)).unwrap();
        assert!(f.registry.get(&f.live.run_id).is_none());
        f.store
            .read(|conn| {
                let h = sql::history(conn, &f.live.run_id)?.unwrap();
                assert_eq!(h.broken, 1);
                assert_eq!(h.activation_count, 1);
                assert_eq!(h.writes_sent, 1);
                assert_eq!(h.state, terminal_state);
                assert!(sql::live(conn, &f.live.run_id)?.is_none());
                for p in 0..5 {
                    assert!(sql::call(conn, &f.live.run_id, p)?.is_some());
                }
                let kv: String = conn.query_row(
                    "SELECT value FROM kv WHERE flow_id='flow' AND key='counter'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(kv, "7");
                let tombstones: i64 =
                    conn.query_row("SELECT COUNT(*) FROM tombstones", [], |r| r.get(0))?;
                assert_eq!(tombstones, 2);
                assert_eq!(
                    sql::obligation(conn, &f.live.run_id, 4)?.unwrap().state,
                    sql::ObligationState::Settled
                );
                Ok(())
            })
            .unwrap();
        assert!(f.service.closed.load(Ordering::Acquire));
        assert_eq!(*f.service.refusals.lock().unwrap(), vec![3]);
    }
}

#[test]
fn recovery_snapshot_cannot_resend_a_completed_or_accepted_call() {
    for reply in [
        Reply::Completed(arrival(0, "42")),
        Reply::Accepted("h".into()),
    ] {
        let f = Fixture::new();
        let a = f.claim(0);
        a.defer_preparation(call(0, "query"), StoredClass::Query, "not sent".into(), 0)
            .unwrap();
        let mut prepared = a.recover_dispatches().unwrap();
        assert_eq!(prepared.len(), 1);
        let dispatch = prepared.pop().unwrap();
        f.service.replies.lock().unwrap().push_back(reply);

        // Recovery has copied the unresolved Intent. Finish the first send and
        // drop its entrant before recovery checks whether the position is idle.
        let retries = a
            .recover_dispatches_after_snapshot(false, || {
                std::thread::spawn(move || dispatch.send().unwrap())
                    .join()
                    .unwrap();
            })
            .unwrap();
        for dispatch in retries {
            dispatch.send().unwrap();
        }

        assert_eq!(f.service.sent.lock().unwrap().len(), 1);
        let state = a.state().unwrap();
        assert_eq!(state.calls[&0].intent.as_ref().unwrap().attempts, 1);
        assert!(state.calls[&0].prior_intents.is_empty());
        assert!(state.calls[&0].arrival.is_some() || state.calls[&0].accepted.is_some());
    }
}

#[test]
fn pending_preparation_recovery_has_one_authorization_owner() {
    let f = Fixture::new();
    let a = f.claim(0);
    a.defer_preparation(call(0, "query"), StoredClass::Query, "not sent".into(), 0)
        .unwrap();

    // Both recovery paths observe the pending preparation. The second path
    // publishes its Intent and completes before the first path can authorize.
    let retries = a
        .recover_dispatches_after_snapshot(false, || {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let dispatches = a.recover_dispatches().unwrap();
                        assert_eq!(dispatches.len(), 1);
                        for dispatch in dispatches {
                            dispatch.send().unwrap();
                        }
                    })
                    .join()
                    .unwrap();
            });
        })
        .unwrap();
    for dispatch in retries {
        dispatch.send().unwrap();
    }

    assert_eq!(f.service.sent.lock().unwrap().len(), 1);
    let state = a.state().unwrap();
    assert_eq!(state.calls[&0].intent.as_ref().unwrap().attempts, 1);
    assert!(state.calls[&0].prior_intents.is_empty());
    assert!(state.calls[&0].arrival.is_some());
}

#[test]
fn retry_consumes_unsent_proof_before_another_possible_send() {
    let f = Fixture::new();
    let a = f.claim(0);
    f.service
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Deferred(Deferred {
            position: 0,
            retry_not_before: 0,
            cause: "not sent".into(),
            audit_at: 20,
            proven_unsent: true,
        }));
    f.send(&a, 0, "write");
    let mut retries = a.recover_dispatches().unwrap();
    assert_eq!(retries.len(), 1);
    let retry = retries.pop().unwrap();
    // The retry's durable Intent is possibly sent even if its thread dies.
    drop(retry);
    assert!(a.recover_dispatches().unwrap().is_empty());
    assert!(a.state().unwrap().calls[&0].unknown.is_some());
    assert_eq!(f.service.sent.lock().unwrap().len(), 1);
}

#[test]
fn each_driver_boundary_preserves_every_causal_recovery_clause() {
    for boundary in Boundary::RUN_DRIVER {
        crash_case(boundary);
    }
}

pub(crate) fn crash_case(boundary: Boundary) {
    let f = Fixture::new();
    let a = f.claim(0);
    a.requeue(true).unwrap();
    f.send(&a, 0, "write");
    f.send(&a, 1, "keyed");
    let mut model = arrival(2, "11");
    model.usage = Some(crate::run_records::Usage {
        input_tokens: 4,
        output_tokens: 2,
    });
    model.cost_micros = Some(1);
    f.service
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Completed(model));
    f.send(&a, 2, "model");
    let deadline = a.state().unwrap().claim.unwrap().deadline_at;
    let key = a.state().unwrap().calls[&1]
        .intent
        .as_ref()
        .unwrap()
        .idempotency_key
        .clone();
    // A durable unkeyed Intent with no outcome must not be resent. A keyed
    // one may retry, but must retain its frozen request and original key.
    drop(a.issue(call(3, "write")).unwrap());
    drop(a.issue(call(4, "keyed")).unwrap());
    let retry_key = a.state().unwrap().calls[&4]
        .intent
        .as_ref()
        .unwrap()
        .idempotency_key
        .clone();
    *f.cut.0.lock().unwrap() = Some(boundary.clone());
    let result = if boundary == Boundary::DriverReleased {
        a.release(&[0]).map(|_| ())
    } else {
        match a.issue(call(5, "keyed")) {
            Ok(Issued::Dispatch(d)) => d.send(),
            Err(e) => Err(e),
            _ => panic!("not dispatch"),
        }
    };
    assert!(matches!(result, Err(CoreError::Cut)), "{boundary:?}");
    let recovered = f.log.recover(&f.live.run_id);
    let state = recovered.state();
    assert_eq!(
        state.claim.as_ref().unwrap().deadline_at,
        deadline,
        "{boundary:?}"
    );
    assert_eq!(state.broken, 1, "{boundary:?}");
    // The outcome was synced before acknowledgement, even if its later
    // Released record and worker delivery order were not synced.
    for p in [0, 1] {
        assert_eq!(
            state.calls[&p].arrival.as_ref().unwrap().value.as_str(),
            "42",
            "{boundary:?}"
        );
    }
    assert_eq!(
        state.calls[&1].intent.as_ref().unwrap().idempotency_key,
        key,
        "{boundary:?}"
    );
    let synced_sends = state.calls.values().filter(|c| c.intent.is_some()).count();
    assert_eq!(
        state.calls[&2].arrival.as_ref().unwrap().value.as_str(),
        "11"
    );
    assert_eq!(
        state.calls[&2]
            .arrival
            .as_ref()
            .unwrap()
            .usage
            .as_ref()
            .unwrap()
            .input_tokens,
        4
    );
    let accounted = accounting::rows(state, &budgets()).unwrap();
    assert_eq!(
        accounted
            .iter()
            .filter(|r| r.window.0.starts_with("token:"))
            .map(|r| r.amount)
            .sum::<u64>(),
        6,
        "{boundary:?}: token budget"
    );
    assert_eq!(
        accounted
            .iter()
            .filter(|r| r.window.0.starts_with("cost:"))
            .map(|r| r.amount)
            .sum::<u64>(),
        1,
        "{boundary:?}: dollar budget"
    );
    assert!(
        synced_sends >= f.service.sent.lock().unwrap().len(),
        "{boundary:?}: budget undercount"
    );
    // Resume in a new registry. A possibly sent unkeyed write is Unknown,
    // never automatically dispatched a second time.
    let resumed_fixture = Fixture::new();
    let store = resumed_fixture.store.clone();
    let registry = Arc::new(RunStore::new_staged(store.clone(), Arc::new(NoHooks)));
    let generation = state.generation;
    registry
        .insert(
            f.live.run_id.clone(),
            f.live.flow_id.clone(),
            generation,
            recovered,
        )
        .unwrap();
    let driver = Driver::new_staged(
        store,
        registry,
        f.service.clone(),
        Arc::new(NoHooks),
        budgets(),
    );
    let Claim::Active(resume) = driver.claim(f.live.clone(), generation).unwrap() else {
        panic!("not resumed")
    };
    for dispatch in resume.recover_dispatches().unwrap() {
        dispatch.send().unwrap();
    }
    let resumed = resume.state().unwrap();
    assert!(resumed.calls[&3].unknown.is_some());
    assert_eq!(
        resumed.calls[&4].intent.as_ref().unwrap().idempotency_key,
        retry_key
    );
    assert_eq!(
        resumed.calls[&4]
            .intent
            .as_ref()
            .unwrap()
            .frozen_request
            .as_str(),
        "{\"frozen\":true}"
    );
    let rows = accounting::rows(&resumed, &budgets()).unwrap();
    assert!(
        rows.iter()
            .filter(|r| r.window.0.starts_with("dispatch:"))
            .map(|r| r.amount)
            .sum::<u64>()
            >= f.service.sent.lock().unwrap().len() as u64
    );
    assert_eq!(resume.state().unwrap().claim.unwrap().deadline_at, deadline);
    assert_eq!(
        f.service
            .sent
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.call.position == 0)
            .count(),
        1
    );
}

#[test]
fn history_lost_is_never_registered_or_claimed_for_activation() {
    let f = Fixture::new();
    f.store
        .write(|tx| {
            tx.execute("UPDATE live_runs SET history_lost=1", [])?;
            Ok(())
        })
        .unwrap();
    let before = f.log.records.lock().unwrap().len();
    assert!(
        f.driver
            .register(
                &f.live,
                Records::new(f.live.run_id.clone(), f.live.admitted_at, f.log.clone())
            )
            .is_err()
    );
    assert!(f.driver.claim(f.live.clone(), 0).is_err());
    assert_eq!(f.log.records.lock().unwrap().len(), before);
    assert!(f.service.sent.lock().unwrap().is_empty());
}

#[test]
fn not_applied_authorizes_only_one_unkeyed_retry_and_retains_its_key() {
    let f = Fixture::new();
    let a = f.claim(0);
    let Issued::Dispatch(dispatch) = a.issue(call(0, "write")).unwrap() else {
        panic!("dispatch")
    };
    drop(dispatch);
    assert!(a.recover_dispatches().unwrap().is_empty());
    let intent = a.state().unwrap().calls[&0].intent.clone().unwrap();
    a.run
        .enter()
        .unwrap()
        .finish(|r| {
            r.append(
                a.generation,
                Body::Resolution(crate::run_records::Resolution {
                    position: 0,
                    attempts: intent.attempts,
                    idempotency_key: intent.idempotency_key.clone(),
                    choice: "not_applied".into(),
                    observed_outcome: None,
                    usage: None,
                    elicitation_id: "operator-answer".into(),
                }),
            )?;
            Ok(())
        })
        .unwrap();
    let mut retries = a.recover_dispatches().unwrap();
    assert_eq!(retries.len(), 1);
    let retry = retries.pop().unwrap();
    assert_eq!(retry.intent.idempotency_key, intent.idempotency_key);
    assert_eq!(retry.intent.attempts, 2);
    drop(retry);
    assert!(a.recover_dispatches().unwrap().is_empty());
    assert!(a.state().unwrap().calls[&0].unknown.is_some());
    assert!(f.service.sent.lock().unwrap().is_empty());
}
