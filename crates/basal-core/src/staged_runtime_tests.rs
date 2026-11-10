use super::*;
use crate::{Clock, Durability, NoHooks, Store, test_support::ScriptedWorker};
use basal_host::{
    Completion, CompletionAck, CompletionSink, Host, TokenUsage,
    catalog::{MockCatalog, OpDecl, OpKind},
};
use basal_proto::{ActivationResult, ParentMessage, WorkerMessage};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[path = "staged_transport_tests.rs"]
mod transport;

#[path = "staged_reclamation_tests.rs"]
mod reclamation;

#[path = "staged_verification_tests.rs"]
pub(crate) mod verification_matrix;

#[derive(Default)]
struct TestHost {
    sent: Mutex<Vec<CallRequest>>,
    replies: Mutex<std::collections::VecDeque<HostOutcome>>,
    transports: Mutex<std::collections::VecDeque<std::result::Result<Dispatched, TransportError>>>,
    committed: Mutex<Vec<CallRequest>>,
    refused: Mutex<Vec<CallRequest>>,
    revokes: std::sync::atomic::AtomicUsize,
    accept: AtomicBool,
}
impl Host for TestHost {
    fn install_status(
        &self,
        _: &str,
        _: u32,
    ) -> std::result::Result<InstallStatus, TransportError> {
        Err(TransportError::Unavailable {
            sent: basal_host::Sent::Never,
            detail: "core restarting".into(),
        })
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        match kind {
            CallKind::Op { op, .. } if op == "write" => CallClass::Mutation {
                honours_idempotency_keys: false,
            },
            CallKind::Op { op, .. } if op == "keyed" => CallClass::Mutation {
                honours_idempotency_keys: true,
            },
            _ => CallClass::Query,
        }
    }
    fn dispatch(&self, request: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        self.sent.lock().unwrap().push(request.clone());
        if let Some(reply) = self.transports.lock().unwrap().pop_front() {
            return reply;
        }
        if self.accept.load(Ordering::SeqCst) {
            return Ok(Dispatched::Accepted {
                handle: format!("handle:{}", request.run_id),
            });
        }
        if let Some(outcome) = self.replies.lock().unwrap().pop_front() {
            return Ok(Dispatched::Completed(outcome));
        }
        Ok(Dispatched::Completed(HostOutcome::fulfilled(
            JsonText::new(
                if matches!(&request.kind,CallKind::Op { op,.. } if op=="classify") {
                    "{\"cost_usd\":0.9}"
                } else {
                    "7"
                },
            )
            .unwrap(),
        )))
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn dispatch_committed(&self, request: &CallRequest) {
        self.committed.lock().unwrap().push(request.clone());
    }
    fn refusal_committed(&self, request: &CallRequest, _: &basal_host::flow_refusal::FlowRefusal) {
        self.refused.lock().unwrap().push(request.clone());
    }
    fn secret_revoke(
        &self,
        _: Value,
    ) -> std::result::Result<Value, basal_host::transport::WireError> {
        self.revokes.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"status":"revoked","grants":0}))
    }
    fn now_ms(&self) -> f64 {
        100.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}

struct Fixture {
    rt: Runtime,
    root: PathBuf,
    host: Arc<TestHost>,
    catalog: Arc<MockCatalog>,
    clock: Clock,
}
impl Fixture {
    fn new(retain: bool) -> Self {
        Self::with_hooks(retain, Arc::new(NoHooks))
    }
    fn with_hooks(retain: bool, hooks: Arc<dyn crate::Hooks>) -> Self {
        let root = crate::test_support::timestamp_scratch("staged-runtime", "reader");
        let store = Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap());
        let host = Arc::new(TestHost::default());
        let catalog = Arc::new(MockCatalog::standard());
        for op in ["query", "classify", "write", "keyed"] {
            catalog.set_op(
                "test",
                op,
                OpDecl {
                    kind: Some(if matches!(op, "write" | "keyed") {
                        OpKind::Mutate
                    } else {
                        OpKind::Query
                    }),
                    cause_echo: true,
                    shell_capable: false,
                },
            );
        }
        catalog.set_flow_capable("test", true);
        catalog.add_agent("agent_reader");
        catalog.set_tool_contract(
            "test",
            "classify",
            json!({"capabilities":[cortexkit_llm_tools::classify::TAG],"modes":["flow"]}),
        );
        let clock = Clock::manual(3_700_000);
        let rt = Runtime::new_staged_scratch(
            store,
            host.clone(),
            catalog.clone(),
            hooks,
            None,
            config(&clock),
            retain,
        )
        .unwrap();
        Self {
            rt,
            root,
            host,
            catalog,
            clock,
        }
    }
    fn install(&self, id: &str, version: u32, extra: Value) {
        let mut manifest = json!({"id":id,"version":version,"explanation":"Exercises the declared trigger and grants for a test, without additional effects.","purpose":"Exercise staged runtime readers","trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"test","op":"query"}]});
        for (key, value) in extra.as_object().unwrap() {
            manifest[key] = value.clone();
        }
        crate::test_support::describe_test_upgrade(&self.rt, &mut manifest, false);
        let installed = self
            .rt
            .install(&crate::InstallRequest {
                script: "return 7;".into(),
                manifest: manifest.to_string(),
                author: "operator".into(),
                loop_override: false,
            })
            .unwrap();
        self.rt
            .approve(id, version, &installed.code_hash, "reader test")
            .unwrap();
    }
    fn admit(&self, id: &str, trigger: &str) -> String {
        match self
            .rt
            .admit_trigger(id, trigger, JsonText::new("{\"kind\":\"manual\"}").unwrap())
            .unwrap()
        {
            Admission::Admitted { run_id } => run_id,
            other => panic!("{other:?}"),
        }
    }
}
fn config(clock: &Clock) -> Config {
    Config {
        clock: clock.clone(),
        install_gate: crate::InstallGate::Off,
        selector: Arc::new(basal_host::selector::FakeSelector::default()),
        deferral_retry: Duration::from_millis(10),
        // Deadline tests advance the manual run clock. The real-time activation
        // watchdog must not turn ordinary worker progress into a speed test.
        activation_deadline: Duration::from_secs(24 * 60 * 60),
        ..Default::default()
    }
}
fn call(position: u64, primitive: Primitive, args: Value) -> WorkerMessage {
    WorkerMessage::HostCall(HostCall {
        position,
        kind: CallKind::Primitive(primitive),
        args: JsonText::new(args.to_string()).unwrap(),
    })
}
fn op(position: u64, name: &str, args: Value) -> WorkerMessage {
    WorkerMessage::HostCall(HostCall {
        position,
        kind: CallKind::Op {
            module: "test".into(),
            op: name.into(),
        },
        args: JsonText::new(args.to_string()).unwrap(),
    })
}
fn finished() -> WorkerMessage {
    WorkerMessage::Finished {
        activation_id: 0,
        result: ActivationResult::Completed {
            value: JsonText::new("7").unwrap(),
        },
    }
}
fn suspended(awaited: Vec<u64>) -> WorkerMessage {
    WorkerMessage::Finished {
        activation_id: 0,
        result: ActivationResult::Suspended { awaited },
    }
}
fn llm(window: &str) -> Value {
    json!({"llm":{"iq":1,"token_cap":{"tokens":10000,"window":window},"max_output":10}})
}

#[test]
fn staged_runtime_admits_executes_and_reads_without_execution_rows() {
    let f = Fixture::new(false);
    f.install("reader", 1, json!({}));
    let id = f.admit("reader", "first");
    assert_eq!(
        f.rt.startable().unwrap(),
        vec![(id.clone(), "reader".into())]
    );
    assert_eq!(f.rt.run_ages().unwrap().oldest_pending_ms, Some(0));
    let mut worker = ScriptedWorker::new(vec![
        call(0, Primitive::KvSet, json!({"key":"x","value":7})),
        call(1, Primitive::Now, Value::Null),
        op(2, "query", json!({})),
        WorkerMessage::Blocked {
            awaiting: vec![0, 2],
        },
        call(3, Primitive::KvGet, json!({"key":"x"})),
        WorkerMessage::Blocked { awaiting: vec![3] },
        finished(),
    ]);
    assert_eq!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { value: "7".into() }
    );
    assert_eq!(
        f.rt.wait_for(&id, Duration::ZERO, RunState::is_terminal)
            .unwrap(),
        RunState::Succeeded
    );
    assert_eq!(f.rt.run(&id).unwrap().result.as_deref(), Some("7"));
    assert_eq!(f.rt.activation_count(&id).unwrap(), 1);
    assert!(
        f.rt.calls(&id).unwrap().is_empty(),
        "successful production-shaped runs retain no call payloads"
    );
    assert_eq!(f.rt.health().unwrap().runs.get("succeeded"), Some(&1));
    assert_eq!(
        f.rt.flow_health().unwrap()[0]
            .last_run
            .as_ref()
            .unwrap()
            .run_id,
        id
    );
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='runs'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='journal'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            assert_eq!(
                c.query_row(
                    "SELECT value FROM kv WHERE flow_id='reader' AND key='x'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "7"
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE 'rig_%'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
    assert!(
        worker.sent.iter().any(
            |m| matches!(m,ParentMessage::Deliver(o) if o.position==3 && o.value.as_str()=="7")
        )
    );
    assert!(f.root.join("run-log").is_dir());
}

#[test]
fn staged_deadline_service_prunes_history_on_its_retention_cadence() {
    let f = Fixture::new(false);
    f.install("retained", 1, json!({}));
    let closed = f.admit("retained", "first");
    let mut worker = ScriptedWorker::new(vec![op(0, "query", json!({})), finished()]);
    assert!(matches!(
        f.rt.activate(&closed, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.rt.prune_payloads(i64::MAX, 0).unwrap(), 0);
    let ended = f.clock.now_ms();
    f.clock.set(ended + 90 * 24 * 3_600_000 - 1);
    f.rt.enforce_deadlines().unwrap();
    assert!(f.rt.run(&closed).is_ok());
    let due = f.rt.shared.retention_next.lock().unwrap().unwrap();
    assert_eq!(due, f.clock.now_ms() + 3_600_000);
    f.clock.advance(1);
    f.rt.enforce_deadlines().unwrap();
    assert!(f.rt.run(&closed).is_ok(), "not due for maintenance yet");
    f.clock.set(due);
    f.rt.enforce_deadlines().unwrap();
    f.rt.store()
        .read(|c| {
            for table in [
                "run_history",
                "obligations",
                "run_identities",
                "run_snapshots",
            ] {
                assert_eq!(
                    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))?,
                    0,
                    "{table}"
                );
            }
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM tombstones WHERE kind='trigger'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn retained_projection_keeps_success_calls_and_one_row_per_activation() {
    let f = Fixture::new(true);
    f.install("retained", 1, json!({}));
    let id = f.admit("retained", "first");
    let mut worker = ScriptedWorker::new(vec![
        call(0, Primitive::Now, Value::Null),
        op(1, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![1] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.rt.calls(&id).unwrap().len(), 2);
    assert_eq!(f.rt.activation_count(&id).unwrap(), 1);
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT state FROM rig_runs WHERE run_id=?1", [&id], |r| {
                    r.get::<_, String>(0)
                })?,
                "succeeded"
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM rig_calls WHERE run_id=?1",
                    [&id],
                    |r| r.get::<_, u64>(0)
                )?,
                2
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM rig_activations WHERE run_id=?1",
                    [&id],
                    |r| r.get::<_, u64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn retained_projection_does_not_count_an_install_gate_deferral_as_an_activation() {
    let mut f = Fixture::new(true);
    f.install("gated", 1, json!({}));
    let id = f.admit("gated", "first");
    Arc::make_mut(&mut f.rt.config).install_gate = crate::InstallGate::Core;
    assert!(matches!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![finished()]))
            .unwrap(),
        ActivationEnd::Deferred { .. }
    ));
    assert_eq!(f.rt.activation_count(&id).unwrap(), 0);
    assert!(f.rt.staged_snapshot(&id).unwrap().claim.is_none());
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM rig_activations", [], |r| r
                    .get::<_, u64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn ordinary_constructor_recovers_run_log_without_rig_projection() {
    let root = crate::test_support::timestamp_scratch("staged-runtime", "ordinary");
    let store = Arc::new(Store::open(root.join("store.db"), Durability::throwaway()).unwrap());
    let rt = Runtime::new(
        store,
        Arc::new(TestHost::default()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config::default(),
    )
    .expect("runtime startup");
    assert!(rt.uses_run_log());
    assert!(root.join("run-log").exists());
    rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE 'rig_%'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn owner_lost_worker_is_reaped_before_close_can_drain() {
    use crate::channel::{ChannelError, WorkerReceiver};
    use std::sync::mpsc;

    struct HeldKill {
        peer: ScriptedWorker,
        killing: mpsc::Sender<()>,
        release: Option<mpsc::Receiver<()>>,
    }
    impl WorkerChannel for HeldKill {
        fn welcome(&self) -> &basal_proto::Welcome {
            self.peer.welcome()
        }
        fn send(&mut self, _: &ParentMessage) -> std::result::Result<(), ChannelError> {
            panic!("a stale lease must not start an activation")
        }
        fn recv(&mut self, _: Duration) -> std::result::Result<WorkerMessage, ChannelError> {
            panic!("a stale lease must not read worker messages")
        }
        fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
            panic!("a stale lease must not start a receiver")
        }
        fn kill(&mut self) {
            if let Some(release) = self.release.take() {
                self.killing.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            self.peer.kill();
        }
    }

    let f = Fixture::new(true);
    f.install("kill-order", 1, json!({}));
    let id = f.admit("kill-order", "first");
    let lease = f.rt.staged_claim(&id).unwrap().unwrap();
    f.rt.take_over(&id, lease.generation).unwrap();
    let (killing, kill_began) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let driver = {
        let rt = f.rt.clone();
        std::thread::spawn(move || {
            rt.staged_drive(
                lease,
                &mut HeldKill {
                    peer: ScriptedWorker::new(vec![]),
                    killing,
                    release: Some(released),
                },
            )
        })
    };
    kill_began.recv_timeout(Duration::from_secs(30)).unwrap();
    // Hold kill before the worker has stopped. The map is the close driver's
    // gate to entrant draining, not just a list of activation threads.
    let registered =
        f.rt.shared
            .staged_workers
            .lock()
            .unwrap()
            .keys()
            .any(|(run, _)| run == &id);
    let (stopped, stop_finished) = mpsc::channel();
    let stopper = {
        let rt = f.rt.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            let services = Services {
                shared: Arc::downgrade(&rt.shared),
                config: rt.config.clone(),
                acknowledgements: Default::default(),
                audit_actor: None,
            };
            stopped.send(services.stop_worker(&id)).unwrap();
        })
    };
    let before_reap = stop_finished.recv_timeout(Duration::from_millis(100));
    release.send(()).unwrap();
    assert_eq!(driver.join().unwrap(), ActivationEnd::OwnerLost);
    if before_reap.is_err() {
        stop_finished
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap();
    }
    stopper.join().unwrap();
    assert!(
        registered,
        "close could begin draining while worker.kill was still in progress"
    );
    assert!(
        before_reap.is_err(),
        "stop_worker returned before the worker was reaped"
    );
}

fn target(position: u64) -> WorkerMessage {
    WorkerMessage::HostCall(HostCall {
        position,
        kind: CallKind::OpFor {
            module: "test".into(),
            op: "query".into(),
        },
        args: JsonText::new("{\"for\":\"agent_reader\",\"args\":{}}").unwrap(),
    })
}
fn pending_preparation(f: &Fixture, flow: &str) -> String {
    let mut extra = llm("1h");
    extra["audience"] = json!({"kind":"global"});
    f.install(flow, 1, extra);
    let id = f.admit(flow, "first");
    f.catalog
        .fail_agent_registry(Some(basal_host::transport::WireError::NeverSent(
            "registry restarting".into(),
        )));
    let mut worker = ScriptedWorker::new(vec![
        target(0),
        call(1, Primitive::KvGet, json!({"key":"x"})),
        WorkerMessage::Blocked {
            awaiting: vec![0, 1],
        },
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    assert_eq!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Suspended { awaited: vec![0] }
    );
    let state = f.rt.staged_snapshot(&id).unwrap();
    assert_eq!(
        state.calls.len(),
        2,
        "a pending preparation must not reuse another concurrent call's position"
    );
    assert!(state.calls[&0].intent.is_none());
    assert!(state.calls[&0].deferred.as_ref().unwrap().proven_unsent);
    assert!(f.host.sent.lock().unwrap().is_empty());
    assert_eq!(f.rt.classify_cost_spent(&id).unwrap(), 0.0);
    id
}

#[test]
fn pending_preparation_retries_once_after_registry_recovers_without_charging_the_failure() {
    let f = Fixture::new(true);
    let id = pending_preparation(&f, "pending");
    assert!(f.rt.startable().unwrap().is_empty());
    assert!(
        f.rt.flow_health().unwrap()[0]
            .waiting_reason
            .as_ref()
            .unwrap()
            .contains("sink_audience_unavailable")
    );
    f.catalog.fail_agent_registry(None);
    f.clock.advance(11);
    assert_eq!(
        f.rt.startable().unwrap(),
        vec![(id.clone(), "pending".into())]
    );
    let mut worker = ScriptedWorker::new(vec![
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.host.sent.lock().unwrap().len(), 1);
    let state = f.rt.staged_snapshot(&id).unwrap();
    assert_eq!(state.calls[&0].intent.as_ref().unwrap().attempts, 1);
    assert!(state.calls[&0].prior_intents.is_empty());
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT SUM(dispatches) FROM rate_windows WHERE flow_id='pending'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn pending_preparation_survives_restart_and_reprepares_without_duplicate_send() {
    let f = Fixture::new(false);
    let id = pending_preparation(&f, "recovered");
    f.catalog.fail_agent_registry(None);
    let mut newer = llm("1d");
    newer["audience"] = json!({"kind":"global"});
    f.install("recovered", 2, newer);
    f.rt.store().cut();
    let Fixture {
        rt,
        root,
        host,
        catalog,
        clock,
    } = f;
    drop(rt);
    catalog.fail_agent_registry(None);
    clock.advance(11);
    let store = Arc::new(Store::open(root.join("store.db"), Durability::default()).unwrap());
    let (log, recovered) = crate::run_log::RunLog::<crate::run_records::Record>::open(
        crate::run_log::Config {
            directory: root.join("run-log"),
            identity: crate::run_log::Identity {
                store_id: store.store_id().as_bytes().try_into().unwrap(),
                log_epoch: 1,
            },
            lease_epoch: store.lease_epoch(),
            target_size: crate::run_log::Config::TARGET_SIZE,
            hooks: Arc::new(NoHooks),
        },
        Arc::new(crate::run_log::SqliteRegistry::new(store.clone(), 1)),
    )
    .unwrap();
    let log = Arc::new(log);
    let hosts = Arc::new(
        crate::run_host::RecoveredHosts::recover(
            store,
            log.clone(),
            Arc::new(log.watermark()),
            &recovered.records,
            run_driver::Budgets {
                dispatch_window_ms: 60_000,
                token_window_ms: 86_400_000,
                dispatches: 1000,
                tokens: u64::MAX,
                cost_micros: u64::MAX,
                cost_window: None,
            },
            Arc::new(NoHooks),
            clock.clone(),
        )
        .unwrap(),
    );
    assert_eq!(
        hosts.budgets_for(&id).unwrap().token_window_ms,
        3_600_000,
        "recovery uses the admitted snapshot, not the newly approved manifest"
    );
    let rt = Runtime::new_staged(
        hosts,
        host.clone(),
        catalog,
        Arc::new(NoHooks),
        None,
        config(&clock),
    )
    .unwrap();
    rt.recover().unwrap();
    assert_eq!(
        rt.startable().unwrap(),
        vec![(id.clone(), "recovered".into())]
    );
    let mut worker = ScriptedWorker::new(vec![
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(host.sent.lock().unwrap().len(), 1);
}

#[test]
fn pending_preparation_deadline_keeps_typed_unsent_refusal_without_an_intent() {
    let f = Fixture::new(false);
    let id = pending_preparation(&f, "expired");
    f.clock.advance(600_001);
    assert_eq!(f.rt.enforce_deadlines().unwrap(), vec![id.clone()]);
    let call =
        f.rt.store()
            .read(|c| sql::call(c, &id, 0))
            .unwrap()
            .unwrap();
    assert!(call.detail["intent"].is_null());
    let value: Value =
        serde_json::from_str(call.detail["arrival"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(value["code"], "sink_audience_unavailable");
    assert!(f.host.sent.lock().unwrap().is_empty());
}

fn pending_catalog_call(
    f: &Fixture,
    error: basal_host::transport::WireError,
    expected_code: &str,
) -> String {
    f.install("catalog-readiness", 1, json!({}));
    let id = f.admit("catalog-readiness", "first");
    f.catalog.fail_op_lookup(Some(error));
    let refusal = crate::authorize::shell_or_unknown(
        &crate::authorize::ShellDenylist::default(),
        f.catalog.as_ref(),
        "test",
        "query",
    )
    .expect("an unavailable declaration must not authorize a call");
    assert_eq!(refusal.code, expected_code);
    let mut worker = ScriptedWorker::new(vec![
        op(0, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    assert_eq!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Suspended { awaited: vec![0] }
    );
    let state = f.rt.staged_snapshot(&id).unwrap();
    assert!(state.calls[&0].intent.is_none());
    let deferred = state.calls[&0].deferred.as_ref().unwrap();
    assert!(deferred.proven_unsent);
    assert!(deferred.refund_dispatch);
    assert_eq!(deferred.retry_not_before, f.clock.now_ms() + 10);
    assert!(f.host.sent.lock().unwrap().is_empty());
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    assert_eq!(catalog_dispatches(f), 0);
    id
}

fn missing_provider() -> basal_host::transport::WireError {
    basal_host::transport::WireError::Typed(basal_host::flow_refusal::FlowRefusal::new(
        basal_host::flow_refusal::RefusalReason::ProviderNotCatalogued,
        "test",
        "query",
    ))
}

fn catalog_dispatches(f: &Fixture) -> u64 {
    f.rt.store()
        .read(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(dispatches),0) FROM rate_windows WHERE flow_id='catalog-readiness'",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
        .unwrap()
}

#[test]
fn catalog_readiness_missing_provider_defers_then_succeeds() {
    let f = Fixture::new(true);
    let id = pending_catalog_call(&f, missing_provider(), "provider_not_catalogued");
    assert!(f.rt.startable().unwrap().is_empty());
    let state = f.rt.staged_snapshot(&id).unwrap();
    let cause: basal_host::flow_refusal::FlowRefusal =
        serde_json::from_str(&state.calls[&0].deferred.as_ref().unwrap().cause).unwrap();
    assert_eq!(cause.reason.as_str(), "provider_not_catalogued");
    assert!(cause.readiness());
    f.catalog.fail_op_lookup(None);
    f.clock.advance(11);
    assert_eq!(
        f.rt.startable().unwrap(),
        vec![(id.clone(), "catalog-readiness".into())]
    );
    let mut worker = ScriptedWorker::new(vec![
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.host.sent.lock().unwrap().len(), 1);
    assert_eq!(catalog_dispatches(&f), 1);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
}

#[test]
fn catalog_readiness_deadline_keeps_typed_unsent_rejection() {
    let f = Fixture::new(false);
    let id = pending_catalog_call(&f, missing_provider(), "provider_not_catalogued");
    f.clock.advance(600_001);
    assert_eq!(f.rt.enforce_deadlines().unwrap(), vec![id.clone()]);
    let call =
        f.rt.store()
            .read(|c| sql::call(c, &id, 0))
            .unwrap()
            .unwrap();
    assert!(call.detail["intent"].is_null());
    let value: Value =
        serde_json::from_str(call.detail["arrival"]["value"].as_str().unwrap()).unwrap();
    assert_eq!(value["code"], "provider_not_catalogued");
    assert_eq!(value["module"], "test");
    assert_eq!(value["action"], "query");
    assert!(f.host.sent.lock().unwrap().is_empty());
    assert_eq!(catalog_dispatches(&f), 0);
    // The wait is free, but exhausting the run deadline still counts as one
    // failed run, just like other deferred calls that never become ready.
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 1);
}

#[test]
fn catalog_readiness_unreadable_catalog_defers() {
    let f = Fixture::new(true);
    let id = pending_catalog_call(
        &f,
        basal_host::transport::WireError::Unreadable("daemon disconnected".into()),
        "catalog_unavailable",
    );
    let state = f.rt.staged_snapshot(&id).unwrap();
    let cause: basal_host::flow_refusal::FlowRefusal =
        serde_json::from_str(&state.calls[&0].deferred.as_ref().unwrap().cause).unwrap();
    assert_eq!(cause.reason.as_str(), "catalog_unavailable");
    assert!(cause.readiness());
}

#[test]
fn catalog_readiness_missing_op_is_still_final_denied() {
    let f = Fixture::new(true);
    f.install("catalog-readiness", 1, json!({}));
    let id = f.admit("catalog-readiness", "first");
    f.catalog.remove_op("test", "query");
    let mut worker = ScriptedWorker::new(vec![
        op(0, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let state = f.rt.staged_snapshot(&id).unwrap();
    assert!(state.calls[&0].deferred.is_none());
    let value: Value =
        serde_json::from_str(state.calls[&0].arrival.as_ref().unwrap().value.as_str()).unwrap();
    assert_eq!(value["code"], "denied");
    assert_eq!(value["message"], "op (test, query) is not in the catalog");
    assert!(f.host.sent.lock().unwrap().is_empty());
}

#[test]
fn catalog_readiness_intake_refuses_unavailable_provider_with_clear_reason() {
    let f = Fixture::new(true);
    let manifest = Manifest::parse_ordinary(
        &json!({"id":"intake","version":1,"explanation":"Exercises the declared trigger and grants for a test, without additional effects.","purpose":"Check catalog readiness at intake",
            "trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"test","op":"query"}]})
        .to_string(),
    )
    .unwrap();
    for (error, expected) in [
        (
            missing_provider(),
            "provider test is not currently catalogued",
        ),
        (
            basal_host::transport::WireError::Unreadable("daemon disconnected".into()),
            "catalog unavailable",
        ),
    ] {
        f.catalog.fail_op_lookup(Some(error));
        let error = crate::install::validate(
            &manifest,
            "operator",
            f.catalog.as_ref(),
            &crate::authorize::ShellDenylist::default(),
            false,
        )
        .unwrap_err();
        let crate::InstallError::OpCatalogUnavailable { module, op, reason } = error else {
            panic!("intake must refuse unavailable declarations, not call them absent: {error:?}")
        };
        assert_eq!(module, "test");
        assert_eq!(op, "query");
        assert!(reason.contains(expected), "{reason}");
    }
}

#[test]
fn consecutive_runs_each_keep_their_own_dollar_cap() {
    let f = Fixture::new(false);
    f.install("dollars",1,json!({"ops":[{"module":"test","op":"classify"}],"model_cost_usd":{"per_run":1.0,"usd":100.0,"window":"1d"}}));
    for trigger in ["first", "second"] {
        let id = f.admit("dollars", trigger);
        let mut worker = ScriptedWorker::new(vec![
            op(0, "classify", json!({"max_cost_usd":1.0})),
            WorkerMessage::Blocked { awaiting: vec![0] },
            finished(),
        ]);
        assert_eq!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Succeeded { value: "7".into() }
        );
        assert!((f.rt.classify_cost_spent(&id).unwrap() - 0.9).abs() < 0.000001);
    }
    assert_eq!(f.host.sent.lock().unwrap().len(), 2);
}

#[test]
fn token_readers_count_live_and_settled_runs_in_each_pinned_window() {
    let f = Fixture::new(false);
    f.host.accept.store(true, Ordering::SeqCst);
    for (flow, window, ms, start) in [
        ("hour", "1h", 3_600_000, 3_600_000),
        ("day", "1d", 86_400_000, 0),
    ] {
        f.install(flow, 1, llm(window));
        let id = f.admit(flow, "first");
        let mut worker = ScriptedWorker::new(vec![
            call(0, Primitive::Llm, json!({"prompt":"hello"})),
            WorkerMessage::Blocked { awaiting: vec![0] },
            suspended(vec![0]),
        ]);
        assert_eq!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Suspended { awaited: vec![0] }
        );
        let usage = f.rt.token_usage(flow, f.clock.now_ms()).unwrap().unwrap();
        assert_eq!((usage.window_ms, usage.window_start), (ms, start));
        assert!(usage.reserved > 0);
        assert_eq!(f.rt.open_reservations(&id).unwrap(), 1);
        let outcome = HostOutcome {
            settlement: Settlement::Fulfilled,
            value: JsonText::new("7").unwrap(),
            usage: Some(TokenUsage {
                input_tokens: Some(3),
                cache_write_tokens: Some(0),
                output_tokens: Some(2),
                cached_input_tokens: Some(0),
            }),
        };
        assert_eq!(
            f.rt.complete(&Completion {
                run_id: id.clone(),
                position: 0,
                handle: format!("handle:{id}"),
                outcome
            })
            .unwrap(),
            CompletionAck::Accepted
        );
        let usage = f.rt.token_usage(flow, f.clock.now_ms()).unwrap().unwrap();
        assert_eq!(usage.counted(), 5);
        assert_eq!(f.rt.open_reservations(&id).unwrap(), 0);
        f.rt.startable().unwrap();
        let mut worker = ScriptedWorker::new(vec![
            WorkerMessage::Blocked { awaiting: vec![0] },
            finished(),
        ]);
        assert!(matches!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Succeeded { .. }
        ));
        assert_eq!(
            f.rt.token_usage(flow, f.clock.now_ms())
                .unwrap()
                .unwrap()
                .counted(),
            5
        );
    }
    let many =
        f.rt.token_usage_many(&["hour", "day"], f.clock.now_ms())
            .unwrap();
    assert_eq!(many["hour"].window_ms, 3_600_000);
    assert_eq!(many["day"].window_ms, 86_400_000);
}

#[test]
fn late_callback_after_close_settles_the_admitted_versions_window() {
    let f = Fixture::new(false);
    f.host.accept.store(true, Ordering::SeqCst);
    f.install("late", 1, llm("1h"));
    let id = f.admit("late", "first");
    let mut worker = ScriptedWorker::new(vec![
        call(0, Primitive::Llm, json!({"prompt":"hello"})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Suspended { .. }
    ));
    f.install("late", 2, llm("1d"));
    f.rt.cancel(&id, "operator").unwrap();
    let outcome = HostOutcome {
        settlement: Settlement::Fulfilled,
        value: JsonText::new("7").unwrap(),
        usage: Some(TokenUsage {
            input_tokens: Some(3),
            cache_write_tokens: Some(0),
            output_tokens: Some(2),
            cached_input_tokens: Some(0),
        }),
    };
    assert_eq!(
        f.rt.complete(&Completion {
            run_id: id.clone(),
            position: 0,
            handle: format!("handle:{id}"),
            outcome
        })
        .unwrap(),
        CompletionAck::Accepted
    );
    assert_eq!(f.rt.open_reservations(&id).unwrap(), 0);
    f.rt.store()
        .read(|c| {
            let usage = crate::tokens::window_usage(c, "late", 3_600_000, 3_600_000)?;
            assert_eq!(usage.counted(), 5);
            assert_eq!(usage.reserved, 0);
            assert_eq!(
                c.query_row(
                    "SELECT window_ms FROM token_ledger WHERE run_id=?1",
                    [&id],
                    |r| r.get::<_, i64>(0)
                )?,
                3_600_000
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn run_limit_also_bounds_pre_authorization_refusals() {
    let mut f = Fixture::new(true);
    Arc::make_mut(&mut f.rt.config).limits.max_calls = 1;
    f.install("bounded", 1, json!({}));
    let id = f.admit("bounded", "first");
    let mut worker = ScriptedWorker::new(vec![
        op(0, "not_granted", json!({})),
        op(1, "not_granted", json!({})),
        finished(),
    ]);
    assert!(
        matches!(f.rt.activate(&id,&mut worker).unwrap(),ActivationEnd::Failed {kind,..} if kind=="limit")
    );
    assert_eq!(f.rt.calls(&id).unwrap().len(), 1);
    assert!(f.host.sent.lock().unwrap().is_empty());
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 1);
}

#[test]
fn worker_budget_denial_is_a_catchable_typed_outcome_without_dispatch() {
    let f = Fixture::new(true);
    f.install(
        "token-denial",
        1,
        json!({"llm":{"iq":1,"token_cap":{"tokens":1,"window":"1h"},"max_output":1}}),
    );
    let id = f.admit("token-denial", "first");
    let mut worker = ScriptedWorker::new(vec![
        call(0, Primitive::Llm, json!({"prompt":"hello"})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let calls = f.rt.calls(&id).unwrap();
    let outcome = calls[0].outcome.as_ref().unwrap();
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "token_cap"
    );
    assert!(f.host.sent.lock().unwrap().is_empty());
    assert_eq!(f.rt.open_reservations(&id).unwrap(), 0);
}

#[test]
fn target_limit_counts_registry_intents_instead_of_the_empty_journal() {
    let f = Fixture::new(true);
    f.catalog.add_agent("agent_second");
    f.install(
        "targets",
        1,
        json!({"audience":{"kind":"global"},"max_targets":1}),
    );
    let id = f.admit("targets", "first");
    let other = WorkerMessage::HostCall(HostCall {
        position: 1,
        kind: CallKind::OpFor {
            module: "test".into(),
            op: "query".into(),
        },
        args: JsonText::new("{\"for\":\"agent_second\",\"args\":{}}").unwrap(),
    });
    let mut worker = ScriptedWorker::new(vec![
        target(0),
        WorkerMessage::Blocked { awaiting: vec![0] },
        other,
        WorkerMessage::Blocked { awaiting: vec![1] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.host.sent.lock().unwrap().len(), 1);
    let calls = f.rt.calls(&id).unwrap();
    let value: Value =
        serde_json::from_str(calls[1].outcome.as_ref().unwrap().value.as_str()).unwrap();
    assert_eq!(value["code"], "too_many_targets");
}

#[test]
fn scheduler_admissions_publish_registry_entries_before_they_are_startable() {
    let f = Fixture::new(false);
    f.install("scheduled", 1, json!({}));
    f.clock.advance(3_600_000);
    let report =
        f.rt.scheduler()
            .unwrap()
            .tick_at(jiff::Timestamp::from_millisecond(f.clock.now_ms()).unwrap())
            .unwrap();
    assert_eq!(report.new_runs().len(), 1);
    let id = report.new_runs()[0];
    assert_eq!(
        f.rt.startable().unwrap(),
        vec![(id.into(), "scheduled".into())]
    );
    let mut worker = ScriptedWorker::new(vec![finished()]);
    assert!(matches!(
        f.rt.activate(id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.rt.run(id).unwrap().state, RunState::Succeeded);
}

#[test]
fn event_body_retry_deadline_and_engine_timers_read_registry_records() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(false);
    f.catalog.set_op(
        "test",
        "events_get",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: true,
            shell_capable: false,
        },
    );
    f.catalog.set_event(
        "test",
        "tick",
        1,
        basal_host::catalog::EventDecl {
            origin: basal_host::catalog::EventOrigin::Internal,
            body: basal_host::catalog::EventBody::Resolved {
                resolve_op: "events_get".into(),
            },
            headers: vec![],
        },
    );
    f.install("events",1,json!({"trigger":{"events":[{"module":"test","name":"tick","version":1}]},"ops":[{"module":"test","op":"events_get"}]}));
    let body = "{\"changed\":true}";
    let mut notice = crate::events::Notice {
        subject: "ck.account.event.test.tick.v1".into(),
        event_key: format!("{:064x}", 1),
        digest: format!("{:x}", Sha256::digest(body.as_bytes())),
        headers: Default::default(),
        published_at_ms: f.clock.now_ms(),
    };
    let refusal = || HostOutcome {
        settlement: Settlement::Rejected,
        value: JsonText::new(
            "{\"error\":{\"code\":\"flow_audience_unavailable\",\"retryable\":true}}",
        )
        .unwrap(),
        usage: None,
    };
    assert_eq!(f.rt.admit_events(&[notice.clone()]).unwrap().admitted, 1);
    let id = f.rt.startable().unwrap()[0].0.clone();
    f.host.replies.lock().unwrap().push_back(refusal());
    assert_eq!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![]))
            .unwrap(),
        ActivationEnd::Requeued
    );
    assert!(f.rt.startable().unwrap().is_empty());
    *f.rt.shared.retention_next.lock().unwrap() = Some(f.clock.now_ms() + 100_000);
    let retry = f.clock.now_ms() + 10;
    assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(retry));
    assert_eq!(
        f.rt.event_body_deadline_failure(&id).unwrap().unwrap().0,
        "flow_audience_unavailable"
    );
    f.clock.set(retry);
    f.host.replies.lock().unwrap().push_back(HostOutcome::fulfilled(JsonText::new(json!({"result":{"body":body,"digest":notice.digest,"event_key":notice.event_key,"event_name":"tick"}}).to_string()).unwrap()));
    assert_eq!(f.rt.startable().unwrap()[0].0, id);
    let mut worker = ScriptedWorker::new(vec![finished()]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let trigger = worker
        .sent
        .iter()
        .find_map(|m| {
            if let ParentMessage::Activate(r) = m {
                Some(serde_json::from_str::<Value>(r.trigger.as_str()).unwrap())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(trigger["event"]["body"], json!({"changed":true}));
    notice.event_key = format!("{:064x}", 2);
    notice.published_at_ms = f.clock.now_ms();
    assert_eq!(f.rt.admit_events(&[notice]).unwrap().admitted, 1);
    let id = f.rt.startable().unwrap()[0].0.clone();
    f.host.replies.lock().unwrap().push_back(refusal());
    assert_eq!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![]))
            .unwrap(),
        ActivationEnd::Requeued
    );
    f.clock.advance(600_001);
    assert_eq!(f.rt.enforce_deadlines().unwrap(), vec![id.clone()]);
    assert_eq!(
        f.rt.run(&id).unwrap().error_kind.as_deref(),
        Some("flow_audience_unavailable")
    );
    assert!(
        f.rt.fixture_state(&id).is_err(),
        "closed runs retain no event-body actor"
    );
}
