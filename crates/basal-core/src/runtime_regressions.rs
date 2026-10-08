use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::{CallClass, catalog::MockCatalog};
use basal_proto::{Confinement, PreludeHash, Welcome};
use rusqlite::params;

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct TestHost {
    replies: Mutex<VecDeque<std::result::Result<Dispatched, TransportError>>>,
    refusals: Mutex<Vec<CallRequest>>,
}

impl Host for TestHost {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Mutation {
            honours_idempotency_keys: true,
        }
    }
    fn dispatch(&self, _: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        lock(&self.replies).pop_front().expect("scripted reply")
    }
    fn refusal_committed(&self, request: &CallRequest, _: &FlowRefusal) {
        lock(&self.refusals).push(request.clone());
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        123.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

struct Fixture {
    rt: Runtime,
    host: Arc<TestHost>,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new(now: i64) -> Self {
        let path = std::env::temp_dir().join(format!(
            "basal-runtime-regression-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let store = Arc::new(
            Store::open(path.join("core.db"), crate::Durability { fullfsync: false }).unwrap(),
        );
        let host = Arc::new(TestHost::default());
        let rt = Runtime::new(
            store,
            host.clone(),
            Arc::new(MockCatalog::new()),
            Arc::new(crate::NoHooks),
            None,
            Config {
                clock: Clock::manual(now),
                install_gate: crate::InstallGate::Off,
                retry_backoff: Duration::ZERO,
                ..Config::default()
            },
        );
        Self { rt, host, path }
    }

    fn pending(&self, id: &str) {
        self.rt.store().write(|tx| {
            tx.execute("INSERT OR IGNORE INTO flows(flow_id,created_at) VALUES ('flow',0)", [])?;
            tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,admit_seq) VALUES (?1,'flow',?1,1,'null','return 1;',?2,?3,'pending',0,?4)",
                params![id, r#"{"id":"flow","version":1,"purpose":"Test runtime transitions","trigger":{"schedule":{"interval":"1h"}}}"#, crate::ids::code_hash("return 1;", r#"{"id":"flow","version":1,"purpose":"Test runtime transitions","trigger":{"schedule":{"interval":"1h"}}}"#).as_slice(), NEXT.fetch_add(1, Ordering::SeqCst) as i64])?;
            Ok(())
        }).unwrap();
    }

    fn call(&self, id: &str, class: StoredClass) -> Lease {
        self.pending(id);
        self.rt
            .store()
            .write(|tx| {
                let lease = runs::claim(tx, id, "owner", self.rt.config.clock.now_ms())?;
                journal::insert_call(
                    tx,
                    &lease,
                    "flow",
                    &journal::NewCall {
                        position: 0,
                        kind: &CallKind::Op {
                            module: "provider".into(),
                            op: "act".into(),
                        },
                        args: &JsonText::null(),
                        class,
                        request: None,
                    },
                )?;
                Ok(lease)
            })
            .unwrap()
    }

    fn deferred(&self, id: &str) -> Lease {
        let lease = self.call(id, StoredClass::KeyedMutation);
        self.rt
            .store()
            .write(|tx| {
                journal::defer(
                    tx,
                    id,
                    0,
                    &FlowRefusal::new(RefusalReason::ResourceBusy, "provider", "act"),
                    i64::MAX,
                )
            })
            .unwrap();
        lease
    }

    fn request(&self, id: &str) -> CallRequest {
        Runtime::request(&self.rt.run(id).unwrap(), &self.rt.calls(id).unwrap()[0], 1)
    }

    fn approve(&self, id: &str) {
        let run = self.rt.run(id).unwrap();
        let installed = self
            .rt
            .install(&crate::InstallRequest {
                script: run.script,
                manifest: run.manifest,
                author: "operator".into(),
                loop_override: false,
            })
            .unwrap();
        self.rt
            .approve("flow", 1, &installed.code_hash, "test approval")
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.rt.quiesce();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn completed_thread_handles_are_reaped_during_normal_spawning() {
    let f = Fixture::new(100);
    for _ in 0..32 {
        f.rt.spawn(|| {});
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while lock(&f.rt.shared.threads).iter().any(|h| !h.is_finished()) {
        assert!(Instant::now() < deadline, "dispatch threads did not finish");
        thread::yield_now();
    }
    f.rt.spawn(|| {});
    assert!(
        lock(&f.rt.shared.threads).len() <= 1,
        "completed handles leaked"
    );
}

#[test]
fn terminal_transitions_settle_every_deferred_call() {
    for ending in ["failed", "engine_mismatch", "cancel", "expire"] {
        let f = Fixture::new(100);
        let lease = f.deferred("run");
        f.rt.store()
            .write(|tx| {
                match ending {
                    "failed" => {
                        runs::exit(
                            tx,
                            &lease,
                            &runs::Exit::Failed {
                                kind: "script".into(),
                                detail: "boom".into(),
                            },
                        )?;
                    }
                    "engine_mismatch" => {
                        runs::exit(
                            tx,
                            &lease,
                            &runs::Exit::EngineMismatch {
                                detail: "changed".into(),
                            },
                        )?;
                    }
                    "cancel" => runs::cancel(tx, "run", "stop")?,
                    _ => {
                        tx.execute("UPDATE runs SET deadline_at=99 WHERE run_id='run'", [])?;
                        runs::expire(tx, 100, None)?;
                    }
                }
                assert!(
                    journal::unsettled_positions(tx, "run")?.is_empty(),
                    "{ending} left an unsent obligation"
                );
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn scope_refusal_after_an_ambiguous_send_stays_unknown() {
    let f = Fixture::new(100);
    f.call("run", StoredClass::KeyedMutation);
    lock(&f.host.replies).extend([
        Err(TransportError::maybe_sent(
            UnknownReason::ReplyTimeout,
            "lost",
        )),
        Err(TransportError::Refused(FlowRefusal::new(
            RefusalReason::FlowScopeRequired,
            "provider",
            "act",
        ))),
    ]);
    f.rt.dispatch(f.request("run"), StoredClass::KeyedMutation, true)
        .unwrap();
    assert_eq!(
        f.rt.unknown_reason("run", 0).unwrap(),
        Some(UnknownReason::ReplyTimeout)
    );
    assert!(
        lock(&f.host.refusals).is_empty(),
        "an unknown call was acknowledged as refused"
    );
}

#[test]
fn refused_resend_keeps_the_last_ambiguous_send_reason() {
    let f = Fixture::new(100);
    f.call("run", StoredClass::KeyedMutation);
    lock(&f.host.replies).extend([
        Err(TransportError::maybe_sent(
            UnknownReason::ReplyUnreadable,
            "lost",
        )),
        Err(TransportError::Refused(FlowRefusal::new(
            RefusalReason::ResourceBusy,
            "provider",
            "act",
        ))),
    ]);
    f.rt.dispatch(f.request("run"), StoredClass::KeyedMutation, true)
        .unwrap();
    assert_eq!(
        f.rt.unknown_reason("run", 0).unwrap(),
        Some(UnknownReason::ReplyUnreadable)
    );
}

#[test]
fn retirement_fails_through_the_terminal_transition_and_blocks_approval_reenable() {
    let f = Fixture::new(100);
    f.deferred("run");
    // A second dispatched position learns that the agent retired while the
    // first position is still deferred.
    f.rt.store()
        .write(|tx| {
            let lease = Lease {
                run_id: "run".into(),
                owner: "owner".into(),
                generation: 1,
            };
            journal::insert_call(
                tx,
                &lease,
                "flow",
                &journal::NewCall {
                    position: 1,
                    kind: &CallKind::Op {
                        module: "provider".into(),
                        op: "act".into(),
                    },
                    args: &JsonText::null(),
                    class: StoredClass::KeyedMutation,
                    request: None,
                },
            )?;
            Ok(())
        })
        .unwrap();
    let request = Runtime::request(&f.rt.run("run").unwrap(), &f.rt.calls("run").unwrap()[1], 1);
    lock(&f.host.replies).push_back(Err(TransportError::Refused(FlowRefusal::new(
        RefusalReason::AgentRetired,
        "provider",
        "act",
    ))));
    f.rt.dispatch(request, StoredClass::KeyedMutation, true)
        .unwrap();
    assert!(
        f.rt.store()
            .read(|c| journal::unsettled_positions(c, "run"))
            .unwrap()
            .is_empty(),
        "retirement stranded the deferred call"
    );
    assert_eq!(
        f.rt.flow("flow").unwrap().unwrap().disabled_by.as_deref(),
        Some(crate::install::CORE_ACTOR),
        "retirement is still attributed to core"
    );
    assert_eq!(f.rt.run("run").unwrap().ended_at, Some(100));
    f.approve("run");
    assert!(
        !f.rt.flow("flow").unwrap().unwrap().enabled,
        "approval cannot revive a retired agent's flow"
    );
}

#[test]
fn retention_prunes_old_history_but_keeps_live_obligations_and_card_episodes() {
    let f = Fixture::new(365 * 24 * 60 * 60 * 1000);
    f.call("live", StoredClass::KeyedMutation);
    f.rt.store().write(|tx| {
        for id in ["old","live"] {
            tx.execute("INSERT INTO call_audit(run_id,position,flow_id,op,args_digest,outcome,at) VALUES (?1,0,'flow','op',zeroblob(32),'allowed',0)",[id])?;
            tx.execute("INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES (?1,0,'rejected','null',zeroblob(32),'unknown_call',0)",[id])?;
            tx.execute("INSERT INTO audit(at,actor,action,run_id,detail) VALUES (0,'operator','cancel',?1,'')",[id])?;
        }
        tx.execute("INSERT INTO outbox(at,kind,flow_id,body,delivered_at) VALUES (0,'delivered','flow','{}',0),(0,'pending','flow','{}',NULL)",[])?;
        tx.execute("INSERT INTO schedule_dropped(flow_id,trigger_id,due_ms,payload,reason,at) VALUES ('flow','old',0,'null','disabled',0)",[])?;
        tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,saturated) VALUES ('flow',60000,0,1),('obsolete',60000,0,1)",[])?;
        tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start) VALUES ('flow',60000,0)",[])?;
        tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at,settled_at) VALUES ('old','flow','old',0,60000,0,1,'settled',0,0),('live','flow','live',0,60000,0,1,'reserved',0,NULL)",[])?;
        tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at) VALUES ('old','reenable','flow',1,1,'{}','expired',0,0),('latest','reenable','flow',1,2,'{}','expired',0,0),('open','reenable','other-flow',1,3,'{}','open',0,NULL)",[])?;
        Ok(())
    }).unwrap();
    f.rt.enforce_deadlines().unwrap();
    f.rt.store()
        .read(|c| {
            for table in [
                "call_audit",
                "quarantine",
                "audit",
                "outbox",
                "token_ledger",
            ] {
                let count: i64 =
                    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(count, 1, "{table} history was not pruned safely");
            }
            let dropped: i64 =
                c.query_row("SELECT count(*) FROM schedule_dropped", [], |r| r.get(0))?;
            assert_eq!(dropped, 0);
            let rates: i64 = c.query_row("SELECT count(*) FROM rate_windows", [], |r| r.get(0))?;
            assert_eq!(rates, 1, "a live call can still refund its dispatch window");
            let tokens: i64 =
                c.query_row("SELECT count(*) FROM token_windows", [], |r| r.get(0))?;
            assert_eq!(
                tokens, 1,
                "an open token reservation still needs its window"
            );
            let cards: i64 =
                c.query_row("SELECT count(*) FROM decision_cards", [], |r| r.get(0))?;
            assert_eq!(
                cards, 2,
                "open cards and the latest re-enable episode must survive"
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn claim_expiry_acknowledges_the_deferred_snapshot_after_commit() {
    let f = Fixture::new(100);
    f.deferred("run");
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE runs SET state='pending',owner=NULL,deadline_at=99 WHERE run_id='run'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let mut worker = IdleWorker {
        welcome: Welcome {
            protocol_version: basal_proto::PROTOCOL_VERSION,
            engine: "test".into(),
            prelude_hash: PreludeHash([0; 32]),
            confinement: Confinement::None,
        },
        killed: false,
    };
    assert!(matches!(
        f.rt.activate("run", &mut worker).unwrap(),
        ActivationEnd::NotRunnable {
            state: RunState::Failed
        }
    ));
    assert_eq!(
        lock(&f.host.refusals).len(),
        1,
        "Broca snapshot was not acknowledged"
    );
}

#[test]
fn a_mailbox_winner_prevents_a_false_reconcile_transition() {
    let f = Fixture::new(100);
    f.call("run", StoredClass::KeyedMutation);
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE runs SET state='suspended',owner=NULL WHERE run_id='run'",
                [],
            )?;
            journal::accept_outcome(
                tx,
                "run",
                0,
                None,
                &HostOutcome::fulfilled(JsonText::null()),
                Source::Host,
            )?;
            journal::record_unknown(tx, "run", 0, UnknownReason::ConnectionLost)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.rt.run("run").unwrap().state, RunState::Pending);
    assert!(
        f.rt.store()
            .read(|c| journal::unknown_positions(c, "run"))
            .unwrap()
            .is_empty()
    );
}

struct IdleWorker {
    welcome: Welcome,
    killed: bool,
}
impl crate::WorkerChannel for IdleWorker {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }
    fn send(
        &mut self,
        _: &basal_proto::ParentMessage,
    ) -> std::result::Result<(), crate::ChannelError> {
        panic!("corrupt recovery must not reach the worker")
    }
    fn recv(
        &mut self,
        _: Duration,
    ) -> std::result::Result<basal_proto::WorkerMessage, crate::ChannelError> {
        panic!("no activation")
    }
    fn kill(&mut self) {
        self.killed = true;
    }
}

#[test]
fn a_non_storage_driver_error_exits_the_claim_and_frees_the_slot() {
    let f = Fixture::new(100);
    let lease = f.call("run", StoredClass::KeyedMutation);
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE journal SET kind_code=999,module=NULL,op=NULL WHERE run_id='run'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let mut worker = IdleWorker {
        welcome: Welcome {
            protocol_version: basal_proto::PROTOCOL_VERSION,
            engine: "test".into(),
            prelude_hash: PreludeHash([0; 32]),
            confinement: Confinement::None,
        },
        killed: false,
    };
    let end = f.rt.drive(lease, &mut worker);
    assert!(matches!(end, ActivationEnd::Failed { .. }), "{end:?}");
    assert_eq!(
        f.rt.run("run").unwrap().state,
        RunState::Failed,
        "dead owner still holds slot"
    );
    assert!(worker.killed);
}

#[test]
fn malformed_optional_refusal_detail_does_not_halt_deadline_enforcement() {
    let f = Fixture::new(100);
    f.deferred("run");
    f.rt.store().write(|tx| { tx.execute("UPDATE runs SET deadline_at=99 WHERE run_id='run'", [])?; tx.execute("UPDATE journal SET refusal_detail=json_set(refusal_detail,'$.retry_after_ms','bad') WHERE run_id='run'", [])?; Ok(()) }).unwrap();
    assert_eq!(
        f.rt.enforce_deadlines()
            .expect("one malformed optional field must not halt runtime"),
        vec!["run"]
    );
    assert!(
        f.rt.store()
            .read(|c| journal::unsettled_positions(c, "run"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn enabling_an_enabled_flow_preserves_its_saturation_streak() {
    let f = Fixture::new(100);
    f.pending("run");
    f.rt.store().write(|tx| {
        tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,saturated) VALUES ('flow',60,0,1)", [])?;
        assert!(!crate::install::enable(tx, "flow", 100).unwrap());
        let saturated: bool = tx.query_row("SELECT saturated FROM rate_windows", [], |r| r.get(0))?;
        assert!(saturated, "idempotent enable erased the loop brake");
        Ok(())
    }).unwrap();
}

#[test]
fn runtime_clock_dates_run_end_activation_and_quarantine_consistently() {
    let f = Fixture::new(1234);
    let lease = f.call("run", StoredClass::KeyedMutation);
    f.rt.cancel("run", "operator").unwrap();
    f.rt.complete(&Completion {
        run_id: "run".into(),
        position: 0,
        handle: "handle".into(),
        outcome: HostOutcome::fulfilled(JsonText::null()),
    })
    .unwrap();
    f.rt.store()
        .read(|c| {
            let started: i64 = c.query_row(
                "SELECT started_at FROM activations WHERE run_id=?1 AND generation=?2",
                params![lease.run_id, lease.generation],
                |r| r.get(0),
            )?;
            let ended: i64 =
                c.query_row("SELECT ended_at FROM runs WHERE run_id='run'", [], |r| {
                    r.get(0)
                })?;
            let quarantined: i64 = c.query_row("SELECT at FROM quarantine", [], |r| r.get(0))?;
            let audited: i64 = c.query_row("SELECT at FROM audit", [], |r| r.get(0))?;
            assert_eq!(
                (started, ended, quarantined, audited),
                (1234, 1234, 1234, 1234)
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn wait_for_reports_unsatisfied_predicates_as_timeouts() {
    let f = Fixture::new(100);
    f.pending("run");
    assert!(
        f.rt.wait_for("run", Duration::ZERO, RunState::is_terminal)
            .is_err(),
        "timeout looked like success"
    );
    assert_eq!(
        f.rt.wait_for("run", Duration::ZERO, |s| s == RunState::Pending)
            .unwrap(),
        RunState::Pending
    );
}

#[test]
fn missing_rows_are_not_storage_failures() {
    assert!(!matches!(
        CoreError::from(rusqlite::Error::QueryReturnedNoRows),
        CoreError::Store(_)
    ));
}

#[test]
fn retention_runs_from_the_production_deadline_pass() {
    let f = Fixture::new(365 * 24 * 60 * 60 * 1000);
    f.pending("old");
    f.approve("old");
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE runs SET state='succeeded',ended_at=0 WHERE run_id='old'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    f.rt.enforce_deadlines().unwrap();
    assert!(
        f.rt.run("old").is_err(),
        "production never pruned a finished run"
    );
    let count: i64 =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM tombstones WHERE kind='trigger'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(count, 1, "deduplication must survive pruning");
    assert_eq!(
        f.rt.admit_trigger("flow", "old", JsonText::null()).unwrap(),
        Admission::Tombstoned {
            run_id: "old".into()
        }
    );
}

#[test]
fn pruning_is_a_bounded_batch() {
    let f = Fixture::new(100);
    for i in 0..140 {
        f.pending(&format!("run-{i:03}"));
    }
    f.rt.store()
        .write(|tx| {
            tx.execute("UPDATE runs SET state='succeeded',ended_at=0", [])?;
            Ok(())
        })
        .unwrap();
    let first = f.rt.prune(100, 0).unwrap();
    assert!(!first.pruned.is_empty());
    assert!(
        first.pruned.len() <= 128,
        "unbounded store transaction: {} runs",
        first.pruned.len()
    );
    let second = f.rt.prune(100, 0).unwrap();
    assert_eq!(first.pruned.len() + second.pruned.len(), 140);
}

#[test]
fn journal_size_counts_bytes_once_across_arrival_delivery_duplicates_and_rollback() {
    let f = Fixture::new(100);
    let lease = f.call("run", StoredClass::KeyedMutation);
    let outcome = HostOutcome::fulfilled(JsonText::new(r#""é""#).unwrap());
    assert_eq!(
        f.rt.store().read(|c| journal::run_size(c, "run")).unwrap(),
        (1, 4)
    );
    f.rt.store()
        .write(|tx| {
            journal::accept_outcome(tx, "run", 0, None, &outcome, Source::Host)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.rt.store().read(|c| journal::run_size(c, "run")).unwrap(),
        (1, 8)
    );
    f.rt.store()
        .write(|tx| {
            journal::accept_outcome(tx, "run", 0, None, &outcome, Source::Host)?;
            journal::release_next(tx, &lease, &[0])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.rt.store().read(|c| journal::run_size(c, "run")).unwrap(),
        (1, 8)
    );
    let rollback: Result<()> = f.rt.store().write(|tx| {
        journal::insert_call(
            tx,
            &lease,
            "flow",
            &journal::NewCall {
                position: 1,
                kind: &CallKind::Primitive(basal_proto::Primitive::Now),
                args: &JsonText::null(),
                class: StoredClass::Sync,
                request: None,
            },
        )?;
        journal::record_sync(
            tx,
            &lease,
            1,
            basal_proto::Settlement::Fulfilled,
            &JsonText::new("123").unwrap(),
            Some(123.0),
        )?;
        Err(CoreError::Invalid("rollback".into()))
    });
    assert!(rollback.is_err());
    assert_eq!(
        f.rt.store().read(|c| journal::run_size(c, "run")).unwrap(),
        (1, 8)
    );
    // Simulate an existing store without counters: the first subsequent
    // writer must seed them from the old journal rather than reset its size.
    f.rt.store()
        .write(|tx| {
            tx.execute("DELETE FROM meta WHERE key LIKE 'journal_%'", [])?;
            journal::insert_call(
                tx,
                &lease,
                "flow",
                &journal::NewCall {
                    position: 1,
                    kind: &CallKind::Primitive(basal_proto::Primitive::Now),
                    args: &JsonText::null(),
                    class: StoredClass::Sync,
                    request: Some(&JsonText::null()),
                },
            )?;
            journal::record_sync(
                tx,
                &lease,
                1,
                basal_proto::Settlement::Fulfilled,
                &JsonText::new("123").unwrap(),
                Some(123.0),
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.rt.store().read(|c| journal::run_size(c, "run")).unwrap(),
        (2, 19)
    );
}

#[test]
fn pruning_preserves_a_terminal_run_until_its_refusal_acknowledgement_is_delivered() {
    let f = Fixture::new(100);
    let lease = f.deferred("run");
    f.rt.store()
        .write(|tx| {
            runs::exit(
                tx,
                &lease,
                &runs::Exit::Failed {
                    kind: "script".into(),
                    detail: "boom".into(),
                },
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        f.rt.prune(100, 0).unwrap().pruned.is_empty(),
        "provider snapshot was pruned before its acknowledgement"
    );
    f.rt.flush_refusals().unwrap();
    assert_eq!(lock(&f.host.refusals).len(), 1);
    assert_eq!(f.rt.prune(100, 0).unwrap().pruned, vec!["run"]);
}

#[test]
fn provider_deferral_uses_its_own_backoff_not_transport_or_install_gate_settings() {
    let mut f = Fixture::new(100);
    Arc::make_mut(&mut f.rt.config).retry_backoff = Duration::from_millis(7);
    Arc::make_mut(&mut f.rt.config).install_gate_retry_max = Duration::from_millis(9);
    f.call("run", StoredClass::KeyedMutation);
    lock(&f.host.replies).push_back(Err(TransportError::Refused(FlowRefusal::new(
        RefusalReason::ResourceBusy,
        "provider",
        "act",
    ))));
    f.rt.dispatch(f.request("run"), StoredClass::KeyedMutation, true)
        .unwrap();
    assert_eq!(
        f.rt.store()
            .read(|c| journal::deferred_until(c, "run", 0))
            .unwrap(),
        Some(1100)
    );
}

#[test]
fn pruning_keeps_unacknowledged_broca_snapshots_and_open_cards() {
    let f = Fixture::new(100);
    for id in ["broca", "card", "done"] {
        let lease = f.call(id, StoredClass::KeyedMutation);
        f.rt.store()
            .write(|tx| {
                journal::accept_outcome(
                    tx,
                    id,
                    0,
                    None,
                    &HostOutcome::fulfilled(JsonText::null()),
                    Source::Host,
                )?;
                runs::exit(
                    tx,
                    &lease,
                    &runs::Exit::Failed {
                        kind: "script".into(),
                        detail: "boom".into(),
                    },
                )?;
                Ok(())
            })
            .unwrap();
    }
    f.rt.store().write(|tx| {
        tx.execute("INSERT INTO broca_calls(send_id,run_id,position,snapshot) VALUES ('broca','broca',0,'{\"acknowledged\":false}'),('done','done',0,'{\"acknowledged\":true}')", [])?;
        tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at) VALUES ('card','reconcile','flow',1,'card',0,'card',1,'{}','open',0)", [])?;
        Ok(())
    }).unwrap();
    let report = f.rt.prune(100, 0).unwrap();
    assert_eq!(report.pruned, vec!["done"]);
    assert_eq!(report.kept_unsettled, vec!["broca", "card"]);
    f.rt.store()
        .read(|c| {
            let count: i64 = c.query_row("SELECT count(*) FROM broca_calls", [], |r| r.get(0))?;
            assert_eq!(
                count, 1,
                "only acknowledged snapshots may cascade with their run"
            );
            Ok(())
        })
        .unwrap();
}
