use super::*;
use crate::{
    NoHooks, Step,
    broca::BrocaStore,
    model::{RunState, StoredClass},
    run_driver::{Terminal, TerminalEffects},
    run_log::Lsn,
    run_records::{CallIdentity, Intent, State},
    run_store::{CloseDriver, TerminalWrite, lock_order},
    run_store_sql::{EpochLsn, GcFloor, LiveRun, Meta, ObligationState},
};
use basal_host::{UnknownReason, broca::StateStore};
use basal_proto::{CallKind, HostCall, JsonText, Settlement};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Default)]
struct MemoryLog {
    records: Mutex<Vec<Record>>,
    synced: AtomicU64,
    fail_sync: AtomicBool,
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
    fn sync_through(&self, cut: Lsn) -> Result<()> {
        lock_order::assert_sync_allowed();
        if self.fail_sync.swap(false, Ordering::SeqCst) {
            return Err(CoreError::Store("injected sync failure".into()));
        }
        self.synced.fetch_max(cut.offset, Ordering::SeqCst);
        Ok(())
    }
    fn synced_lsn(&self) -> Lsn {
        Lsn {
            sequence: 1,
            offset: self.synced.load(Ordering::SeqCst),
        }
    }
}
impl AdmissionLog for MemoryLog {
    fn synced_lsn(&self) -> EpochLsn {
        EpochLsn {
            log_epoch: 1,
            lsn: RecordLog::synced_lsn(self),
        }
    }
}
impl MemoryLog {
    fn recovered(&self) -> Vec<RecoveredRecord<Record>> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .take(self.synced.load(Ordering::SeqCst) as usize)
            .cloned()
            .enumerate()
            .map(|(i, record)| RecoveredRecord {
                lsn: Lsn {
                    sequence: 1,
                    offset: i as u64 + 1,
                },
                record,
            })
            .collect()
    }
}
struct Cut(AtomicBool);
impl Hooks for Cut {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        lock_order::assert_lock_free();
        if boundary == &Boundary::HostArrived && self.0.swap(false, Ordering::SeqCst) {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}
struct Effects;
impl TerminalEffects for Effects {
    fn capture(&self, _: &LiveRun, _: &State, _: RunState) -> Result<Box<dyn TerminalWrite>> {
        Ok(Box::new(Effects))
    }
}
impl TerminalWrite for Effects {
    fn apply(&self, _: &rusqlite::Transaction<'_>) -> Result<()> {
        Ok(())
    }
}
impl CloseDriver for Effects {
    fn stop_worker(&self, _: &str) -> Result<()> {
        lock_order::assert_lock_free();
        Ok(())
    }
    fn closed(&self, _: &str) {
        lock_order::assert_lock_free();
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
    path: std::path::PathBuf,
    store: Arc<Store>,
    log: Arc<MemoryLog>,
    hosts: Arc<RecoveredHosts>,
    live: LiveRun,
    clock: Clock,
}
impl Fixture {
    fn new() -> Self {
        Self::with_hooks(Arc::new(NoHooks))
    }
    fn with_hooks(hooks: Arc<dyn Hooks>) -> Self {
        let path = crate::test_support::timestamp_scratch("run-host", "staged").join("store.db");
        let store = Arc::new(Store::open(&path, crate::Durability::default()).unwrap());
        let epoch = EpochLsn {
            log_epoch: 1,
            lsn: Lsn::default(),
        };
        let live = LiveRun {
            run_id: "run-host".into(),
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
            sql::create_test_schema(tx, &Meta {
                store_id: store.store_id().into(), log_epoch: 1, log_hwm: epoch, commit_seq: 0,
                gc_floor: GcFloor { log_epoch: 1, first_retained: 0, predecessor: None, synced_lsn: Lsn::default(), live_starts: Default::default() },
                migration_phase: "test".into(),
            })?;
            tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest) VALUES(1,'flow',1,?1,'return 1','{}')", [crate::ids::code_hash("return 1", "{}").as_slice()])?;
            sql::insert_live(tx, &live, &sql::RunIdentity { run_id: live.run_id.clone(), flow_id: live.flow_id.clone(), flow_version: 1, code_hash: crate::ids::code_hash("return 1", "{}"), snapshot_id: 1 })
        }).unwrap();
        let clock = Clock::manual(100);
        store.set_clock(clock.clone());
        let log = Arc::new(MemoryLog::default());
        let hosts = Arc::new(
            RecoveredHosts::recover(
                store.clone(),
                log.clone(),
                log.clone(),
                &[],
                budgets(),
                hooks,
                clock.clone(),
            )
            .unwrap(),
        );
        Self {
            path,
            store,
            log,
            hosts,
            live,
            clock,
        }
    }
    fn run(&self) -> Arc<crate::run_store::Run<Records>> {
        self.hosts.registry.get(&self.live.run_id).unwrap()
    }
    fn seed(&self, durable: bool, accepted: bool) {
        self.run()
            .claim(0, |generation, a| {
                let records = a.records();
                records.append(
                    generation,
                    Body::Claimed {
                        first_claim_ms: 20,
                        deadline_at: 1020,
                    },
                )?;
                records.append(generation, Body::Activated)?;
                let intent = Intent {
                    call: CallIdentity::new(
                        HostCall {
                            position: 0,
                            kind: CallKind::Op {
                                module: "model".into(),
                                op: "generate".into(),
                            },
                            args: JsonText::null(),
                        },
                        StoredClass::Query,
                    ),
                    idempotency_key: None,
                    frozen_request: JsonText::null(),
                    attempts: 1,
                    dispatch_count_window: None,
                    audit_at: 20,
                    reservation: Some(crate::run_records::Reservation {
                        input_tokens: 5,
                        output_tokens: 3,
                        cost_micros: 2,
                        window_start: 0,
                    }),
                };
                if durable {
                    records.append(generation, Body::Intent(intent))?;
                } else {
                    records.append_pending(generation, Body::Intent(intent))?;
                }
                if accepted {
                    records.append(
                        generation,
                        Body::Accepted {
                            position: 0,
                            handle: "handle".into(),
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
    }
    fn terminal(&self) -> Terminal {
        Terminal::new(
            self.live.clone(),
            RunState::Cancelled,
            100,
            budgets(),
            Arc::new(Effects),
        )
    }
    fn close(&self) {
        self.hosts
            .registry
            .close(&self.run(), &self.terminal(), &Effects)
            .unwrap();
    }
    fn state(&self) -> State {
        self.run()
            .enter()
            .unwrap_or_else(|_| panic!("closed"))
            .finish(|r| Ok(r.state().clone()))
            .unwrap()
    }
    fn obligation(&self) -> sql::Obligation {
        self.store
            .read(|c| Ok(sql::obligation(c, &self.live.run_id, 0)?.unwrap()))
            .unwrap()
    }
    fn completion(&self) -> Completion {
        Completion {
            run_id: self.live.run_id.clone(),
            position: 0,
            handle: "handle".into(),
            outcome: HostOutcome {
                settlement: Settlement::Fulfilled,
                value: JsonText::new("{\"cost_usd\":0.000001}").unwrap(),
                usage: Some(basal_host::TokenUsage {
                    input_tokens: Some(3),
                    cache_write_tokens: Some(1),
                    output_tokens: Some(2),
                    cached_input_tokens: Some(0),
                }),
            },
        }
    }
    fn unknown(&self) -> UnknownOutcome {
        UnknownOutcome {
            run_id: self.live.run_id.clone(),
            position: 0,
            handle: "handle".into(),
            reason: UnknownReason::ProviderLostRun,
            detail: "provider forgot the run".into(),
        }
    }
}

#[test]
fn live_matching_adopts_a_handle_without_using_activation_generation() {
    let f = Fixture::new();
    f.seed(true, false);
    f.run()
        .claim(1, |generation, a| {
            a.records().append(generation, Body::Activated).map(|_| ())
        })
        .unwrap();
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
    let state = f.state();
    assert_eq!(state.generation, 2);
    assert_eq!(state.calls[&0].accepted.as_deref(), Some("handle"));
    let arrival = state.calls[&0].arrival.as_ref().unwrap();
    assert_eq!(
        arrival.usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 2
        })
    );
    assert_eq!(arrival.cost_micros, Some(1));
    let frames = f.log.recovered();
    assert!(matches!(
        frames[frames.len() - 2].record.body,
        Body::Accepted { .. }
    ));
    assert!(matches!(
        frames.last().unwrap().record.body,
        Body::Arrived(_)
    ));
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Duplicate
    );
    assert_eq!(
        f.hosts.unknown(&f.unknown()).unwrap(),
        CompletionAck::Duplicate
    );
    let mut wrong = f.completion();
    wrong.handle = "other".into();
    assert_eq!(
        f.hosts.complete(&wrong).unwrap(),
        CompletionAck::Quarantined
    );
    wrong = f.completion();
    wrong.outcome.value = JsonText::new("false").unwrap();
    assert_eq!(
        f.hosts.complete(&wrong).unwrap(),
        CompletionAck::Quarantined
    );
    assert_eq!(f.state(), state);
}

#[test]
fn callbacks_require_a_durable_intent_and_honor_shared_group_sync() {
    let f = Fixture::new();
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Quarantined
    );
    f.seed(false, false);
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Quarantined
    );
    assert!(f.state().calls[&0].arrival.is_none());
    let cut = Lsn {
        sequence: 1,
        offset: f.log.records.lock().unwrap().len() as u64,
    };
    RecordLog::sync_through(f.log.as_ref(), cut).unwrap();
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
    let mut wrong = f.completion();
    wrong.position = 1;
    assert_eq!(
        f.hosts.complete(&wrong).unwrap(),
        CompletionAck::Quarantined
    );
}

#[test]
fn closed_completions_keep_receipts_for_q5_a_b_c_d_and_f() {
    let f = Fixture::new();
    f.seed(true, true);
    f.close();
    assert_eq!(f.obligation().state, ObligationState::Open);
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
    let receipt = f.obligation();
    assert_eq!(receipt.state, ObligationState::Settled);
    assert_eq!(
        receipt.receipt,
        Some(Receipt::Value(crate::ids::payload_hash(
            f.completion().outcome.settlement,
            &f.completion().outcome.value
        )))
    );
    assert_eq!(receipt.settled_at, Some(100));
    f.clock.set(200);
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Duplicate
    );
    let mut wrong = f.completion();
    wrong.outcome.value = JsonText::null();
    assert_eq!(
        f.hosts.complete(&wrong).unwrap(),
        CompletionAck::Quarantined
    );
    assert_eq!(
        f.hosts.unknown(&f.unknown()).unwrap(),
        CompletionAck::Duplicate
    );
    assert_eq!(f.obligation(), receipt);
    wrong = f.completion();
    wrong.position = 99;
    assert_eq!(
        f.hosts.complete(&wrong).unwrap(),
        CompletionAck::Quarantined
    );
    f.store
        .read(|c| {
            let (reserved, input, cache, output): (i64, i64, i64, i64) = c.query_row(
                "SELECT reserved,input_tokens,cache_write_tokens,output_tokens FROM token_windows",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            assert_eq!((reserved, input, cache, output), (0, 3, 1, 2));
            let charged: f64 =
                c.query_row("SELECT charged FROM classify_costs", [], |r| r.get(0))?;
            assert_eq!(charged, 0.000001);
            let diagnostics: i64 =
                c.query_row("SELECT COUNT(*) FROM quarantine", [], |r| r.get(0))?;
            assert_eq!(diagnostics, 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn closed_unknown_keeps_a_receipt_and_conservatively_settles_reservations() {
    let f = Fixture::new();
    f.seed(true, true);
    f.close();
    assert_eq!(
        f.hosts.unknown(&f.unknown()).unwrap(),
        CompletionAck::Accepted
    );
    let receipt = f.obligation();
    assert_eq!(receipt.receipt, Some(Receipt::Unknown));
    assert_eq!(receipt.state, ObligationState::Settled);
    assert_eq!(
        f.hosts.unknown(&f.unknown()).unwrap(),
        CompletionAck::Duplicate
    );
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Quarantined
    );
    assert_eq!(f.obligation(), receipt);
    f.store
        .read(|c| {
            let (pending, unreported): (i64, i64) = c.query_row(
                "SELECT reserved,unreported_tokens FROM token_windows",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!((pending, unreported), (0, 8));
            let charged: f64 =
                c.query_row("SELECT charged FROM classify_costs", [], |r| r.get(0))?;
            assert_eq!(charged, 0.000002);
            Ok(())
        })
        .unwrap();
}

#[test]
fn settlement_failure_rolls_back_receipt_usage_and_acknowledgement() {
    let f = Fixture::new();
    f.seed(true, true);
    f.close();
    f.store.write(|tx| {
        tx.execute_batch("CREATE TRIGGER fail_settle BEFORE UPDATE ON token_ledger BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END")?;
        Ok(())
    }).unwrap();
    assert!(f.hosts.complete(&f.completion()).is_err());
    assert_eq!(f.obligation().state, ObligationState::Open);
    f.store
        .read(|c| {
            let pending: i64 =
                c.query_row("SELECT reserved FROM token_windows", [], |r| r.get(0))?;
            assert_eq!(pending, 8);
            Ok(())
        })
        .unwrap();
    f.store
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER fail_settle")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
}

#[test]
pub(crate) fn closing_and_missing_obligations_are_retryable_q5_e() {
    use std::sync::mpsc;
    use std::time::Duration;
    struct Pause {
        reached: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    impl CloseDriver for Pause {
        fn stop_worker(&self, _: &str) -> Result<()> {
            lock_order::assert_lock_free();
            self.reached.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok(())
        }
        fn closed(&self, _: &str) {}
    }
    let f = Fixture::new();
    f.seed(true, true);
    assert!(matches!(
        f.hosts
            .closed(&f.live.run_id, 0, "handle", Some(&f.completion().outcome)),
        Err(CoreError::Unavailable(_))
    ));
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let registry = f.hosts.registry.clone();
    let run = f.run();
    let terminal = f.terminal();
    let closer = std::thread::spawn(move || {
        registry.close(
            &run,
            &terminal,
            &Pause {
                reached: reached_tx,
                resume: Mutex::new(resume_rx),
            },
        )
    });
    reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        f.hosts
            .complete(&f.completion())
            .unwrap_err()
            .0
            .contains("closing")
    );
    assert!(f.hosts.unknown(&f.unknown()).is_err());
    resume_tx.send(()).unwrap();
    closer.join().unwrap().unwrap();
    assert_eq!(
        f.hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
}

#[test]
pub(crate) fn arrival_before_acknowledgement_crash_recovers_the_same_outcome() {
    for boundary in Boundary::HOST {
        assert_eq!(boundary, Boundary::HostArrived);
        for closed in [false, true] {
            let f = Fixture::with_hooks(Arc::new(Cut(AtomicBool::new(true))));
            f.seed(true, closed);
            f.run()
                .enter()
                .unwrap_or_else(|_| panic!("closed"))
                .finish(|records| records.append(1, Body::Requeued { broken: 2 }).map(|_| ()))
                .unwrap();
            if closed {
                f.close();
            }
            let completion = f.completion();
            assert!(f.hosts.complete(&completion).unwrap_err().0.contains("cut"));
            let frames = f.log.recovered();
            if !closed {
                assert!(matches!(
                    frames.last().unwrap().record.body,
                    Body::Arrived(_)
                ));
            }
            let path = f.path.clone();
            let log = f.log.clone();
            let clock = f.clock.clone();
            drop(f);
            let store = Arc::new(Store::open(path, crate::Durability::default()).unwrap());
            let hosts = RecoveredHosts::recover(
                store.clone(),
                log.clone(),
                log,
                &frames,
                budgets(),
                Arc::new(NoHooks),
                clock,
            )
            .unwrap();
            assert_eq!(
                hosts.complete(&completion).unwrap(),
                CompletionAck::Duplicate
            );
            if closed {
                store
                    .read(|c| {
                        assert_eq!(
                            sql::obligation(c, &completion.run_id, 0)?.unwrap().state,
                            ObligationState::Settled
                        );
                        assert_eq!(sql::history(c, &completion.run_id)?.unwrap().broken, 2);
                        Ok(())
                    })
                    .unwrap();
            } else {
                let run = hosts.registry.get(&completion.run_id).unwrap();
                run.enter()
                    .unwrap_or_else(|_| panic!("closed"))
                    .finish(|records| {
                        assert_eq!(records.state().claim.as_ref().unwrap().deadline_at, 1020);
                        assert_eq!(records.state().broken, 2);
                        assert_eq!(
                            records.state().calls[&0]
                                .arrival
                                .as_ref()
                                .unwrap()
                                .value_hash,
                            crate::ids::payload_hash(
                                completion.outcome.settlement,
                                &completion.outcome.value
                            )
                        );
                        Ok(())
                    })
                    .unwrap();
            }
        }
    }
}

#[test]
fn broca_reads_live_registry_then_finished_history_and_leaves_the_live_fk_alone() {
    let f = Fixture::new();
    f.seed(true, true);
    let broca = BrocaStore::new_staged(&f.hosts);
    let identity = broca.identity(&f.live.run_id).unwrap().unwrap();
    assert_eq!((identity.flow_id.as_str(), identity.version), ("flow", 1));
    assert_eq!(broca.deadline(&f.live.run_id).unwrap(), Some(1020));
    assert!(!broca.run_ended(&f.live.run_id).unwrap());
    // A live identity read must not be reconstructed from a SQLite run row.
    f.store
        .write(|tx| {
            tx.execute(
                "UPDATE run_snapshots SET code_hash=?1 WHERE snapshot_id=(SELECT snapshot_id FROM live_runs WHERE run_id=?2)",
                params![[9u8; 32].as_slice(), f.live.run_id],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        broca.identity(&f.live.run_id).unwrap().unwrap().code_hash,
        identity.code_hash
    );
    f.close();
    assert!(broca.run_ended(&f.live.run_id).unwrap());
    assert_eq!(broca.deadline(&f.live.run_id).unwrap(), None);
    assert_eq!(
        broca.identity(&f.live.run_id).unwrap().unwrap().code_hash,
        crate::ids::hex(&[9u8; 32])
    );
    f.store
        .read(|c| {
            let mut stmt = c.prepare("PRAGMA foreign_key_list(broca_calls)")?;
            let tables = stmt
                .query_map([], |r| r.get::<_, String>(2))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            assert!(tables.iter().any(|t| t == "journal"));
            Ok(())
        })
        .unwrap();
}

#[test]
fn recovery_precedes_runtime_attach_time_delivery() {
    struct AttachHost {
        completion: Completion,
        ack: Mutex<Option<CompletionAck>>,
        broca: BrocaStore,
    }
    impl basal_host::Host for AttachHost {
        fn classify(&self, _: &CallKind) -> basal_host::CallClass {
            basal_host::CallClass::Query
        }
        fn dispatch(
            &self,
            _: &basal_host::CallRequest,
        ) -> std::result::Result<basal_host::Dispatched, basal_host::TransportError> {
            panic!("attach must not dispatch")
        }
        fn now_ms(&self) -> f64 {
            100.0
        }
        fn random(&self) -> f64 {
            0.5
        }
        fn attach(&self, sink: Arc<dyn CompletionSink>) {
            lock_order::assert_lock_free();
            assert!(
                self.broca
                    .identity(&self.completion.run_id)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                self.broca.deadline(&self.completion.run_id).unwrap(),
                Some(1020)
            );
            *self.ack.lock().unwrap() = Some(sink.complete(&self.completion).unwrap());
        }
    }
    let f = Fixture::new();
    f.seed(true, true);
    let hosts = Arc::new(
        RecoveredHosts::recover(
            f.store.clone(),
            f.log.clone(),
            f.log.clone(),
            &f.log.recovered(),
            budgets(),
            Arc::new(NoHooks),
            f.clock.clone(),
        )
        .unwrap(),
    );
    let host = Arc::new(AttachHost {
        completion: f.completion(),
        ack: Mutex::new(None),
        broca: BrocaStore::new_staged(&hosts),
    });
    let runtime = crate::Runtime::new_staged(
        hosts,
        host.clone(),
        Arc::new(basal_host::catalog::MockCatalog::new()),
        Arc::new(NoHooks),
        None,
        crate::runtime::Config {
            clock: f.clock.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(*host.ack.lock().unwrap(), Some(CompletionAck::Accepted));
    assert_eq!(
        runtime.complete(&f.completion()).unwrap(),
        CompletionAck::Duplicate
    );
}

#[test]
fn a_failed_arrival_sync_cannot_acknowledge_an_in_memory_duplicate() {
    let f = Fixture::new();
    f.seed(true, true);
    f.log.fail_sync.store(true, Ordering::SeqCst);
    assert!(f.hosts.complete(&f.completion()).is_err());
    assert!(f.hosts.complete(&f.completion()).is_err());
    assert!(f.hosts.unknown(&f.unknown()).is_err());
    assert!(
        !f.log
            .recovered()
            .iter()
            .any(|frame| matches!(frame.record.body, Body::Arrived(_)))
    );
    let frames = f.log.recovered();
    f.log.records.lock().unwrap().truncate(frames.len());
    let hosts = RecoveredHosts::recover(
        f.store.clone(),
        f.log.clone(),
        f.log.clone(),
        &frames,
        budgets(),
        Arc::new(NoHooks),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        hosts.complete(&f.completion()).unwrap(),
        CompletionAck::Accepted
    );
}

#[path = "run_decisions_tests.rs"]
mod decisions;
