use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::run_records::{Body, CallIdentity, Deferred, Intent, State};
use crate::runs::Lease;
use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
use basal_host::{CallClass, Dispatched, TransportError, catalog::MockCatalog};
use basal_proto::{Confinement, PreludeHash, Welcome};
use serde_json::Value;

use crate::DispatchState;

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct TestHost {
    replies: Mutex<VecDeque<std::result::Result<Dispatched, TransportError>>>,
    refusals: Mutex<Vec<CallRequest>>,
    on_dispatch: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Host for TestHost {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Mutation {
            honours_idempotency_keys: true,
        }
    }
    fn dispatch(&self, _: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        if let Some(action) = lock(&self.on_dispatch).take() {
            action();
        }
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

#[test]
fn a_retry_after_revocation_is_not_sent_and_an_ambiguous_mutation_needs_reconcile() {
    let f = Fixture::new(100);
    f.call("revoked-retry", StoredClass::KeyedMutation);
    f.approve("revoked-retry");
    lock(&f.host.replies).extend([
        Err(TransportError::maybe_sent(
            UnknownReason::ConnectionLost,
            "lost reply",
        )),
        Ok(Dispatched::Completed(HostOutcome::fulfilled(
            JsonText::null(),
        ))),
    ]);
    let rt = f.rt.clone();
    *lock(&f.host.on_dispatch) = Some(Box::new(move || {
        rt.store()
            .write(|tx| crate::install::revoke(tx, "flow", 1, "core revoked", 100))
            .unwrap();
    }));
    f.rt.fixture_dispatch_pending("revoked-retry").unwrap();
    assert_eq!(
        lock(&f.host.replies).len(),
        1,
        "a later retry reached the host"
    );
    assert_eq!(
        f.rt.unknown_reason("revoked-retry", 0).unwrap(),
        Some(UnknownReason::ConnectionLost)
    );
    assert_eq!(
        f.rt.run("revoked-retry").unwrap().state,
        RunState::NeedsReconcile
    );
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
        let store =
            Arc::new(Store::open(path.join("core.db"), crate::Durability::throwaway()).unwrap());
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
        )
        .expect("runtime startup");
        Self { rt, host, path }
    }

    fn pending(&self, id: &str) {
        self.rt
            .store()
            .write(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO flows(flow_id,created_at) VALUES('flow',0)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        self.rt.fixture_run(id,"flow",1,"return 1;",r#"{"id":"flow","version":1,"purpose":"Test runtime transitions","trigger":{"schedule":{"interval":"1h"}}}"#,State::new(0)).unwrap();
    }

    fn call(&self, id: &str, class: StoredClass) -> Lease {
        self.call_with(
            id,
            class,
            basal_proto::HostCall {
                position: 0,
                kind: CallKind::Op {
                    module: "provider".into(),
                    op: "act".into(),
                },
                args: JsonText::null(),
            },
        )
    }
    fn call_with(&self, id: &str, class: StoredClass, call: basal_proto::HostCall) -> Lease {
        self.seed_call(id, class, call, true)
    }
    fn possibly_sent(&self, id: &str, class: StoredClass) -> Lease {
        self.seed_call(
            id,
            class,
            basal_proto::HostCall {
                position: 0,
                kind: CallKind::Op {
                    module: "provider".into(),
                    op: "act".into(),
                },
                args: JsonText::null(),
            },
            false,
        )
    }
    fn seed_call(
        &self,
        id: &str,
        class: StoredClass,
        call: basal_proto::HostCall,
        unsent: bool,
    ) -> Lease {
        self.pending(id);
        let lease = self.rt.staged_claim(id).unwrap().unwrap();
        self.rt
            .fixture_append(
                id,
                Body::Intent(Intent {
                    call: CallIdentity::new(call.clone(), class),
                    idempotency_key: Some(crate::ids::idempotency_key("flow", id, call.position)),
                    frozen_request: call.args,
                    attempts: 1,
                    dispatch_count_window: Some(crate::run_records::DispatchCount {
                        window_start: crate::tokens::window_start(
                            self.rt.config.clock.now_ms(),
                            i64::try_from(self.rt.config.rate.window.as_millis()).unwrap(),
                        ),
                        count: 1,
                    }),
                    audit_at: self.rt.config.clock.now_ms(),
                    reservation: None,
                }),
            )
            .unwrap();
        if unsent {
            self.rt
                .fixture_append(
                    id,
                    Body::Deferred(Deferred {
                        position: 0,
                        retry_not_before: self.rt.config.clock.now_ms(),
                        cause: "fixture intent has not been sent".into(),
                        audit_at: self.rt.config.clock.now_ms(),
                        proven_unsent: true,
                        refund_dispatch: true,
                        attempt: None,
                    }),
                )
                .unwrap();
        }
        lease
    }

    fn deferred(&self, id: &str) -> Lease {
        let lease = self.call(id, StoredClass::KeyedMutation);
        self.defer(
            id,
            &FlowRefusal::new(RefusalReason::ResourceBusy, "provider", "act"),
            i64::MAX,
        );
        lease
    }
    fn defer(&self, id: &str, refusal: &FlowRefusal, retry: i64) {
        self.rt
            .fixture_append(
                id,
                Body::Deferred(Deferred {
                    position: 0,
                    retry_not_before: retry,
                    cause: serde_json::to_string(refusal).unwrap(),
                    audit_at: self.rt.config.clock.now_ms(),
                    proven_unsent: true,
                    refund_dispatch: true,
                    attempt: None,
                }),
            )
            .unwrap();
    }
    fn retry(&self, id: &str) {
        let live = self
            .rt
            .store()
            .read(|c| crate::run_store_sql::live(c, id))
            .unwrap()
            .unwrap();
        let state = self.rt.staged_snapshot(id).unwrap();
        if let Some(mut deferred) = state.calls[&0].deferred.clone() {
            deferred.retry_not_before = self.rt.config.clock.now_ms();
            self.rt
                .fixture_append(id, Body::Deferred(deferred))
                .unwrap();
        }
        let driver = self.rt.staged_driver(id).unwrap();
        let activation = driver.owned(live, state.generation).unwrap();
        drop(activation.recover_dispatches().unwrap());
    }

    fn requeue(&self, id: &str) {
        let live = self
            .rt
            .store()
            .read(|c| crate::run_store_sql::live(c, id))
            .unwrap()
            .unwrap();
        let generation = self.rt.staged_snapshot(id).unwrap().generation;
        self.rt
            .staged_driver(id)
            .unwrap()
            .owned(live, generation)
            .unwrap()
            .requeue(false)
            .unwrap();
        self.rt.shared.staged_owners.lock().unwrap().remove(id);
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

fn last_refusal_outcome(f: &Fixture, id: &str) -> Option<serde_json::Value> {
    let call = if let Some(state) = f.rt.staged_snapshot(id) {
        state.calls.get(&0)?.clone()
    } else {
        let row =
            f.rt.store()
                .read(|c| crate::run_store_sql::call(c, id, 0))
                .unwrap()?;
        serde_json::from_value::<crate::run_records::CallState>(row.detail).unwrap()
    };
    let deferred = call
        .deferred
        .as_ref()
        .or_else(|| call.prior_deferrals.last())?;
    let refusal: FlowRefusal = serde_json::from_str(&deferred.cause).unwrap();
    serde_json::from_str(refusal.outcome().value.as_str()).ok()
}

#[test]
fn deferred_refusal_survives_resend_and_terminal_cleanup() {
    let f = Fixture::new(100);
    f.call("run", StoredClass::KeyedMutation);
    let refusal = FlowRefusal::new(
        RefusalReason::SinkStoreFailed,
        "prefrontal-core",
        "sink.wait_resolve",
    );
    f.defer("run", &refusal, i64::MAX);
    assert_eq!(
        last_refusal_outcome(&f, "run").unwrap()["code"],
        "sink_store_failed"
    );
    f.retry("run");
    assert_eq!(f.rt.calls("run").unwrap()[0].dispatch, DispatchState::Sent);
    assert_eq!(
        last_refusal_outcome(&f, "run").unwrap()["code"],
        "sink_store_failed"
    );
    f.defer("run", &refusal, i64::MAX);
    f.rt.fixture_finish("run", RunState::Failed, None, Some(("test", "stop")))
        .unwrap();
    assert_eq!(
        last_refusal_outcome(&f, "run").unwrap()["code"],
        "sink_store_failed"
    );
}

#[test]
fn secret_bearing_transport_failures_never_enter_retry_diagnostics() {
    let f = Fixture::new(100);
    f.call("run", StoredClass::Query);
    let secret = "synthetic-dispatch-sensitive-value";
    for _ in 0..=f.rt.config.unavailable_retries {
        lock(&f.host.replies).push_back(Err(TransportError::unsent(format!(
            "request with {secret} failed"
        ))));
    }
    f.rt.fixture_dispatch_pending("run").unwrap();
    let state = f.rt.staged_snapshot("run").unwrap();
    let call = &state.calls[&0];
    assert!(
        call.arrival
            .as_ref()
            .unwrap()
            .value
            .as_str()
            .contains(secret),
        "the scripted transport error must actually run"
    );
    assert!(!call.prior_deferrals.is_empty());
    for deferred in &call.prior_deferrals {
        assert!(!deferred.cause.contains(secret));
        assert!(!deferred.cause.is_empty());
    }
}

#[test]
fn completed_thread_handles_are_reaped_during_normal_spawning() {
    const JOBS: usize = 32;
    // Generous deadlock watchdog only; the test never waits on wall-clock
    // time to decide whether the jobs have run.
    const WATCHDOG: Duration = Duration::from_secs(30);
    let f = Fixture::new(100);
    // The jobs block on `release` until all have been spawned, so none can
    // finish early, then each reports through `done` that its body returned.
    let release = Arc::new(std::sync::Barrier::new(JOBS + 1));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    for job in 0..JOBS {
        let release = release.clone();
        let done_tx = done_tx.clone();
        f.rt.spawn(move || {
            release.wait();
            done_tx.send(job).unwrap();
        });
    }
    assert!(
        lock(&f.rt.shared.threads).len() >= JOBS,
        "running handles were dropped"
    );
    release.wait();
    for _ in 0..JOBS {
        done_rx
            .recv_timeout(WATCHDOG)
            .expect("a dispatch job did not report completion");
    }
    // A job's body has returned, but its thread finishes just after that;
    // wait for every handle to report finished before provoking the reap.
    let deadline = Instant::now() + WATCHDOG;
    while lock(&f.rt.shared.threads).iter().any(|h| !h.is_finished()) {
        assert!(Instant::now() < deadline, "dispatch threads did not exit");
        thread::yield_now();
    }
    f.rt.spawn(|| {});
    assert!(
        lock(&f.rt.shared.threads).len() <= 1,
        "completed handles leaked"
    );
}

#[test]
fn terminal_transitions_settle_every_proven_unsent_deferred_call() {
    for ending in ["failure", "mismatch", "cancel", "deadline"] {
        let f = Fixture::new(100);
        f.deferred("run");
        match ending {
            "failure" => {
                f.rt.fixture_finish("run", RunState::Failed, None, Some(("failed", "stop")))
                    .unwrap()
            }
            "mismatch" => {
                f.rt.fixture_finish(
                    "run",
                    RunState::EngineMismatch,
                    None,
                    Some(("engine_mismatch", "engine")),
                )
                .unwrap()
            }
            "cancel" => f.rt.cancel("run", "operator").unwrap(),
            _ => {
                let deadline = f.rt.run("run").unwrap().deadline_at.unwrap();
                f.rt.config.clock.set(deadline + 1);
                f.rt.enforce_deadlines().unwrap();
            }
        }
        assert!(f.rt.run("run").unwrap().state.is_terminal(), "{ending}");
        f.rt.store()
            .read(|c| {
                let receipt = crate::run_store_sql::obligation(c, "run", 0)?.unwrap();
                assert_eq!(
                    receipt.state,
                    crate::run_store_sql::ObligationState::Settled
                );
                assert_eq!(
                    crate::run_store_sql::call(c, "run", 0)?.unwrap().outcome,
                    crate::run_store_sql::CallOutcome::Rejected
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(
            lock(&f.host.refusals).len(),
            1,
            "{ending}: final refusal was not acknowledged after commit"
        );
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
    f.rt.fixture_dispatch_pending(&(f.request("run")).run_id)
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
    f.rt.fixture_dispatch_pending(&(f.request("run")).run_id)
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
    f.approve("run");
    let identity = CallIdentity::new(
        basal_proto::HostCall {
            position: 1,
            kind: CallKind::Op {
                module: "provider".into(),
                op: "act".into(),
            },
            args: JsonText::null(),
        },
        StoredClass::KeyedMutation,
    );
    f.rt.fixture_append(
        "run",
        Body::Intent(Intent {
            call: identity,
            idempotency_key: Some(crate::ids::idempotency_key("flow", "run", 1)),
            frozen_request: JsonText::null(),
            attempts: 1,
            dispatch_count_window: Some(crate::run_records::DispatchCount {
                window_start: 0,
                count: 1,
            }),
            audit_at: 100,
            reservation: None,
        }),
    )
    .unwrap();
    f.rt.fixture_append(
        "run",
        Body::Deferred(Deferred {
            position: 1,
            retry_not_before: 100,
            cause: "fixture intent has not been sent".into(),
            audit_at: 100,
            proven_unsent: true,
            refund_dispatch: true,
            attempt: None,
        }),
    )
    .unwrap();
    lock(&f.host.replies).push_back(Err(TransportError::Refused(FlowRefusal::new(
        RefusalReason::AgentRetired,
        "provider",
        "act",
    ))));
    f.rt.fixture_dispatch_pending("run").unwrap();
    f.rt.fixture_finish(
        "run",
        RunState::Failed,
        None,
        Some(("agent_retired", "agent retired")),
    )
    .unwrap();
    f.rt.store()
        .read(|c| {
            for position in 0..2 {
                assert_eq!(
                    crate::run_store_sql::obligation(c, "run", position)?
                        .unwrap()
                        .state,
                    crate::run_store_sql::ObligationState::Settled
                );
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.rt.flow("flow").unwrap().unwrap().disabled_by.as_deref(),
        Some(crate::install::CORE_ACTOR)
    );
    assert_eq!(f.rt.run("run").unwrap().ended_at, Some(100));
    f.approve("run");
    assert!(
        !f.rt.flow("flow").unwrap().unwrap().enabled,
        "approval revived an unchanged retired flow"
    );
}

#[test]
fn retention_prunes_old_history_but_keeps_live_obligations_and_card_episodes() {
    let f = Fixture::new(0);
    f.possibly_sent("live", StoredClass::KeyedMutation);
    f.rt.fixture_append(
        "live",
        Body::Unknown {
            position: 0,
            reason: "connection_lost".into(),
        },
    )
    .unwrap();
    f.rt.config.clock.set(365 * 24 * 60 * 60 * 1000);
    f.rt.store().write(|tx|{
  for id in ["old","live"]{tx.execute("INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES(?1,0,'rejected','null',zeroblob(32),'unknown_call',0)",[id])?;tx.execute("INSERT INTO audit(at,actor,action,run_id,detail) VALUES(0,'operator','cancel',?1,'')",[id])?;}
  tx.execute("INSERT INTO outbox(at,kind,flow_id,body,delivered_at) VALUES(0,'delivered','flow','{}',0),(0,'pending','flow','{}',NULL)",[])?;
  tx.execute("INSERT INTO schedule_dropped(flow_id,trigger_id,due_ms,payload,reason,at) VALUES('flow','old',0,'null','disabled',0)",[])?;
  tx.execute("INSERT INTO rate_windows(flow_id,window_ms,window_start,saturated) VALUES('flow',60000,0,1),('obsolete',60000,0,1)",[])?;
  tx.execute("INSERT INTO token_windows(flow_id,window_ms,window_start) VALUES('flow',60000,0)",[])?;
  tx.execute("INSERT INTO token_ledger(send_id,flow_id,run_id,position,window_ms,window_start,reserved,state,reserved_at,settled_at) VALUES('old','flow','old',0,60000,0,1,'settled',0,0),('live','flow','live',0,60000,0,1,'reserved',0,NULL)",[])?;
  tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at) VALUES('old','reenable','flow',1,1,'{}','expired',0,0),('latest','reenable','flow',1,2,'{}','expired',0,0),('open','reenable','other-flow',1,3,'{}','open',0,NULL)",[])?;Ok(())
 }).unwrap();
    f.rt.enforce_deadlines().unwrap();
    f.rt.store()
        .read(|c| {
            for table in ["quarantine", "audit", "outbox", "token_ledger"] {
                let count: i64 =
                    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(count, 1, "{table}");
            }
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM schedule_dropped", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM rate_windows", [], |r| r
                    .get::<_, i64>(0))?,
                1,
                "the live accounting audit timestamp must retain its original window"
            );
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM token_windows", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM decision_cards", [], |r| r
                    .get::<_, i64>(0))?,
                2
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn claim_expiry_acknowledges_the_deferred_snapshot_after_commit() {
    let f = Fixture::new(100);
    f.deferred("run");
    let deadline = f.rt.run("run").unwrap().deadline_at.unwrap();
    f.requeue("run");
    f.rt.config.clock.set(deadline + 1);
    let mut worker = IdleWorker {
        welcome: Welcome {
            protocol_version: basal_proto::PROTOCOL_VERSION,
            engine: "test".into(),
            prelude_hash: PreludeHash([0; 32]),
            codemode_prelude_hash: PreludeHash([0; 32]),
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
        "final deferred snapshot was not acknowledged"
    );
}

#[test]
fn an_arrival_winner_prevents_a_false_reconcile_transition() {
    let f = Fixture::new(100);
    f.possibly_sent("run", StoredClass::KeyedMutation);
    f.requeue("run");
    assert_eq!(
        f.rt.complete(&Completion {
            run_id: "run".into(),
            position: 0,
            handle: "late".into(),
            outcome: HostOutcome::fulfilled(JsonText::null())
        })
        .unwrap(),
        CompletionAck::Accepted
    );
    assert_eq!(
        f.rt.unknown(&basal_host::UnknownOutcome {
            run_id: "run".into(),
            position: 0,
            handle: "late".into(),
            reason: UnknownReason::ConnectionLost,
            detail: "late uncertainty".into()
        })
        .unwrap(),
        Some(CompletionAck::Duplicate)
    );
    assert_eq!(f.rt.run("run").unwrap().state, RunState::Pending);
    assert_eq!(f.rt.unknown_reason("run", 0).unwrap(), None);
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
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("no activation")
    }
    fn kill(&mut self) {
        self.killed = true;
    }
}

#[test]
fn malformed_optional_refusal_detail_does_not_halt_deadline_enforcement() {
    let f = Fixture::new(100);
    f.deferred("run");
    let mut deferred = f.rt.staged_snapshot("run").unwrap().calls[&0]
        .deferred
        .clone()
        .unwrap();
    let mut detail: Value = serde_json::from_str(&deferred.cause).unwrap();
    detail["retry_after_ms"] = json!("bad");
    deferred.cause = detail.to_string();
    f.rt.fixture_append("run", Body::Deferred(deferred))
        .unwrap();
    let deadline = f.rt.run("run").unwrap().deadline_at.unwrap();
    f.rt.config.clock.set(deadline + 1);
    assert_eq!(
        f.rt.enforce_deadlines()
            .expect("malformed optional field halted deadline processing"),
        vec!["run"]
    );
    f.rt.store()
        .read(|c| {
            assert_eq!(
                crate::run_store_sql::obligation(c, "run", 0)?
                    .unwrap()
                    .state,
                crate::run_store_sql::ObligationState::Settled
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(lock(&f.host.refusals).len(), 1);
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
    f.possibly_sent("run", StoredClass::KeyedMutation);
    f.rt.complete(&Completion {
        run_id: "run".into(),
        position: 0,
        handle: "handle".into(),
        outcome: HostOutcome::fulfilled(JsonText::null()),
    })
    .unwrap();
    f.rt.cancel("run", "operator").unwrap();
    assert_eq!(
        f.rt.complete(&Completion {
            run_id: "run".into(),
            position: 0,
            handle: "handle".into(),
            outcome: HostOutcome::fulfilled(JsonText::new("7").unwrap())
        })
        .unwrap(),
        CompletionAck::Quarantined
    );
    f.rt.store()
        .read(|c| {
            let history = crate::run_store_sql::history(c, "run")?.unwrap();
            let quarantined: i64 = c.query_row("SELECT at FROM quarantine", [], |r| r.get(0))?;
            let audited: i64 = c.query_row("SELECT at FROM audit", [], |r| r.get(0))?;
            assert_eq!(
                (
                    history.first_claim_at.unwrap(),
                    history.ended_at,
                    quarantined,
                    audited
                ),
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
    let now = 365 * 24 * 60 * 60 * 1000;
    let f = Fixture::new(0);
    f.pending("old");
    f.approve("old");
    f.rt.fixture_finish("old", RunState::Succeeded, None, None)
        .unwrap();
    f.rt.config.clock.set(now);
    f.rt.enforce_deadlines().unwrap();
    assert!(
        f.rt.run("old").is_err(),
        "production never pruned a finished run"
    );
    assert_eq!(
        f.rt.store()
            .read(|c| Ok(c.query_row(
                "SELECT COUNT(*) FROM tombstones WHERE kind='trigger'",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        1
    );
    assert_eq!(
        f.rt.admit_trigger("flow", "old", JsonText::null()).unwrap(),
        Admission::Tombstoned {
            run_id: "old".into()
        }
    );
}

#[test]
fn pruning_is_a_bounded_batch() {
    let f = Fixture::new(0);
    for i in 0..140 {
        let id = format!("run-{i:03}");
        f.pending(&id);
        f.rt.fixture_finish(&id, RunState::Succeeded, None, None)
            .unwrap();
    }
    let first = f.rt.prune(100, 0).unwrap();
    assert!(!first.pruned.is_empty());
    assert!(first.pruned.len() <= 128, "unbounded maintenance batch");
    let second = f.rt.prune(100, 0).unwrap();
    assert_eq!(first.pruned.len() + second.pruned.len(), 140);
    f.rt.store()
        .read(crate::run_cutover::check_broca_ownership)
        .unwrap();
}

#[test]
fn replay_budget_counts_payload_once_across_duplicate_arrivals_release_and_rejected_records() {
    let mut f = Fixture::new(100);
    f.possibly_sent("run", StoredClass::Query);
    let completion = Completion {
        run_id: "run".into(),
        position: 0,
        handle: "h".into(),
        outcome: HostOutcome::fulfilled(JsonText::new("7").unwrap()),
    };
    assert_eq!(f.rt.complete(&completion).unwrap(), CompletionAck::Accepted);
    assert_eq!(
        f.rt.complete(&completion).unwrap(),
        CompletionAck::Duplicate
    );
    let next = basal_proto::HostCall {
        position: 1,
        kind: CallKind::Op {
            module: "provider".into(),
            op: "act".into(),
        },
        args: JsonText::null(),
    };
    // null arguments + null frozen request + the one-byte result = 9 bytes;
    // the next call's null adds 4. Neither delivery nor a duplicate adds payload.
    Arc::make_mut(&mut f.rt.config).limits.max_journal_bytes = 13;
    assert!(f.rt.fixture_prepare("run", &next).is_ok());
    Arc::make_mut(&mut f.rt.config).limits.max_journal_bytes = 12;
    assert!(matches!(
        f.rt.fixture_prepare("run", &next),
        Err(CoreError::Worker(_))
    ));
    assert!(
        f.rt.fixture_append(
            "run",
            Body::Released {
                position: 0,
                delivery_order: 999
            }
        )
        .is_err()
    );
    f.rt.fixture_append(
        "run",
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
    )
    .unwrap();
    Arc::make_mut(&mut f.rt.config).limits.max_journal_bytes = 13;
    assert!(f.rt.fixture_prepare("run", &next).is_ok());
}

#[test]
fn pruning_preserves_a_terminal_run_until_its_refusal_acknowledgement_is_delivered() {
    let f = Fixture::new(100);
    f.deferred("run");
    f.rt.fixture_close("run", RunState::Failed, None, Some(("script", "boom")))
        .unwrap();
    assert!(
        f.rt.prune(100, 0).unwrap().pruned.is_empty(),
        "provider snapshot pruned before acknowledgement"
    );
    f.rt.flush_refusals().unwrap();
    assert_eq!(lock(&f.host.refusals).len(), 1);
    assert_eq!(f.rt.prune(100, 0).unwrap().pruned, vec!["run"]);
    f.rt.store()
        .read(crate::run_cutover::check_broca_ownership)
        .unwrap();
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
    f.rt.fixture_dispatch_pending(&(f.request("run")).run_id)
        .unwrap();
    assert_eq!(
        f.rt.deferred_calls()
            .unwrap()
            .into_iter()
            .find(|(id, _, position, _)| id == "run" && *position == 0)
            .map(|(_, _, _, at)| at),
        // The fixture reauthorizes its recovered unsent intent as attempt 2.
        // Provider backoff is therefore 2 seconds, never the 7/9 ms settings.
        Some(2100)
    );
}

#[test]
fn pruning_keeps_a_terminal_run_with_an_unsettled_obligation() {
    let f = Fixture::new(100);
    f.possibly_sent("run", StoredClass::Query);
    f.rt.fixture_append(
        "run",
        Body::Accepted {
            position: 0,
            handle: "late".into(),
        },
    )
    .unwrap();
    f.rt.fixture_finish("run", RunState::Failed, None, Some(("script", "boom")))
        .unwrap();
    assert_eq!(f.rt.run("run").unwrap().state, RunState::Failed);
    let report = f.rt.prune(i64::MAX / 2, 0).unwrap();
    assert!(report.pruned.is_empty(), "{report:?}");
    assert_eq!(report.kept_unsettled, vec!["run"]);
    assert_eq!(
        f.rt.complete(&basal_host::Completion {
            run_id: "run".into(),
            position: 0,
            handle: "late".into(),
            outcome: HostOutcome::fulfilled(JsonText::null())
        })
        .unwrap(),
        CompletionAck::Accepted
    );
    assert_eq!(f.rt.prune(i64::MAX / 2, 0).unwrap().pruned, vec!["run"]);
    f.rt.store()
        .read(crate::run_cutover::check_broca_ownership)
        .unwrap();
}

#[test]
fn pruning_keeps_unacknowledged_broca_snapshots_and_open_cards() {
    let f = Fixture::new(100);
    for id in ["broca", "card", "done"] {
        f.possibly_sent(id, StoredClass::KeyedMutation);
        f.rt.complete(&basal_host::Completion {
            run_id: id.into(),
            position: 0,
            handle: format!("handle:{id}"),
            outcome: HostOutcome::fulfilled(JsonText::null()),
        })
        .unwrap();
        f.rt.fixture_finish(id, RunState::Failed, None, Some(("script", "boom")))
            .unwrap();
    }
    f.rt.store().write(|tx|{tx.execute("INSERT INTO broca_calls(send_id,run_id,position,snapshot) VALUES('broca','broca',0,'{\"acknowledged\":false}'),('done','done',0,'{\"acknowledged\":true}')",[])?;tx.execute("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,run_id,position,call_key,instance,card,state,created_at) VALUES('card','reconcile','flow',1,'card',0,'card',1,'{}','open',0)",[])?;Ok(())}).unwrap();
    let report = f.rt.prune(100, 0).unwrap();
    assert_eq!(report.pruned, vec!["done"]);
    assert_eq!(report.kept_unsettled, vec!["broca", "card"]);
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM broca_calls", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            crate::run_cutover::check_broca_ownership(c)
        })
        .unwrap();
}

/// A closed flow call settles its committed reservation from the refusal's
/// detail marker, not from a generic provider_error code. Both actual charges
/// differ from the reserved ceiling, so retaining the ceiling cannot pass.
#[test]
fn a_flow_classify_refusal_charges_by_the_spend_marker_in_its_detail() {
    for (detail, charged) in [
        (json!({"class":"permanent","no_spend":true}), 0.0),
        (json!({"class":"transient","cost_usd":0.003}), 0.003),
    ] {
        let f = Fixture::new(100);
        f.possibly_sent("run", StoredClass::KeyedMutation);
        f.rt.fixture_append(
            "run",
            Body::Accepted {
                position: 0,
                handle: "late".into(),
            },
        )
        .unwrap();
        f.rt.store().write(|tx|{tx.execute("INSERT INTO classify_costs(kind,run_id,position,flow_id,created_at,reserved,args) VALUES('flow','run',0,'flow',100,0.10,'{}')",[])?;Ok(())}).unwrap();
        f.rt.fixture_update_state("run", |state| {
            state.calls.get_mut(&0).unwrap().pre_accounted_reservation = true
        })
        .unwrap();
        f.rt.fixture_finish("run", RunState::Failed, None, Some(("script", "stopped")))
            .unwrap();
        let outcome = HostOutcome {
            settlement: basal_proto::Settlement::Rejected,
            value: JsonText::new(
                json!({"code":"provider_error","message":"failed","detail":detail}).to_string(),
            )
            .unwrap(),
            usage: None,
        };
        assert_eq!(
            f.rt.complete(&Completion {
                run_id: "run".into(),
                position: 0,
                handle: "late".into(),
                outcome
            })
            .unwrap(),
            CompletionAck::Accepted
        );
        assert_eq!(
            f.rt.store()
                .read(|c| crate::classify_cost::spent(c, "flow", "run"))
                .unwrap(),
            charged,
            "{detail}"
        );
    }
}

#[test]
fn production_constructor_keeps_success_summary_result_and_counters_without_call_detail() {
    let f = Fixture::new(100);
    f.possibly_sent("success", StoredClass::Query);
    f.rt.complete(&Completion {
        run_id: "success".into(),
        position: 0,
        handle: "h".into(),
        outcome: HostOutcome::fulfilled(JsonText::new("7").unwrap()),
    })
    .unwrap();
    f.rt.fixture_append(
        "success",
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
    )
    .unwrap();
    f.rt.fixture_finish("success", RunState::Succeeded, Some(json!(7)), None)
        .unwrap();
    assert!(
        f.rt.calls("success").unwrap().is_empty(),
        "production success exposed retained call detail"
    );
    assert_eq!(
        f.rt.prune_payloads(100 + 24 * 60 * 60 * 1000, 24 * 60 * 60 * 1000)
            .unwrap(),
        0,
        "success payload retention no longer has a 24-hour phase"
    );
    f.rt.store().read(|c|{let history=crate::run_store_sql::history(c,"success")?.unwrap();assert_eq!(history.result,Some(json!(7)));assert_eq!(history.activation_count,1);assert_eq!(history.call_counts["class"]["query"],1);assert_eq!(c.query_row("SELECT COUNT(*) FROM run_calls WHERE run_id='success'",[],|r|r.get::<_,i64>(0))?,0);assert_eq!(c.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name IN ('runs','journal','journal_payloads','mailbox','call_audit','activations','event_body_journal')",[],|r|r.get::<_,i64>(0))?,0);Ok(())}).unwrap();
}

#[test]
fn script_dedup_frees_only_the_last_retained_reference() {
    let f = Fixture::new(0);
    f.pending("first");
    f.pending("second");
    assert_eq!(
        f.rt.store()
            .read(
                |c| Ok(c.query_row("SELECT COUNT(*) FROM run_snapshots", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .unwrap(),
        1
    );
    f.rt.fixture_finish("first", RunState::Succeeded, None, None)
        .unwrap();
    f.rt.config.clock.set(100);
    f.rt.fixture_finish("second", RunState::Succeeded, None, None)
        .unwrap();
    assert_eq!(f.rt.prune(100, 100).unwrap().pruned, vec!["first"]);
    assert_eq!(f.rt.run("second").unwrap().script, "return 1;");
    f.rt.prune(200, 100).unwrap();
    assert_eq!(
        f.rt.store()
            .read(
                |c| Ok(c.query_row("SELECT COUNT(*) FROM run_snapshots", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .unwrap(),
        0
    );
}

#[test]
fn invalid_call_payload_does_not_publish_identity_or_change_counters() {
    let f = Fixture::new(100);
    f.pending("run");
    let _activation = f.rt.fixture_activation("run").unwrap();
    let mut identity = CallIdentity::new(
        basal_proto::HostCall {
            position: 0,
            kind: CallKind::Op {
                module: "provider".into(),
                op: "act".into(),
            },
            args: JsonText::null(),
        },
        StoredClass::Query,
    );
    identity.args_digest = basal_proto::ArgsDigest([9; 32]);
    let bad = Intent {
        call: identity,
        idempotency_key: Some("bad".into()),
        frozen_request: JsonText::null(),
        attempts: 1,
        dispatch_count_window: Some(crate::run_records::DispatchCount {
            window_start: 0,
            count: 1,
        }),
        audit_at: 100,
        reservation: None,
    };
    assert!(f.rt.fixture_append("run", Body::Intent(bad)).is_err());
    assert!(f.rt.calls("run").unwrap().is_empty());
    assert_eq!(f.rt.activation_count("run").unwrap(), 1);
    assert_eq!(f.rt.staged_snapshot("run").unwrap().next_delivery_order, 0);
}
