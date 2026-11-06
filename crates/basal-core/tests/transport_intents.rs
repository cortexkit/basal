use basal_core::run_driver::Issued;
use basal_core::run_records::{Body, Record};
use basal_core::{
    Admission, Clock, Config, Durability, InstallGate, InstallRequest, NoHooks, RunState, Runtime,
    Store, StoredClass,
};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, TransportError,
    UnknownReason, catalog::MockCatalog,
};
use basal_proto::{CallKind, HostCall, JsonText};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Transport {
    class: StoredClass,
    replies: Mutex<VecDeque<Result<Dispatched, TransportError>>>,
    calls: Mutex<Vec<CallRequest>>,
    gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    scope_cut: Mutex<Option<u64>>,
}
impl Host for Transport {
    fn classify(&self, _: &CallKind) -> CallClass {
        match self.class {
            StoredClass::Query => CallClass::Query,
            _ => CallClass::Mutation {
                honours_idempotency_keys: self.class == StoredClass::KeyedMutation,
            },
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        self.calls.lock().unwrap().push(request.clone());
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        if self
            .scope_cut
            .lock()
            .unwrap()
            .is_some_and(|position| position == request.position)
        {
            return Err(TransportError::ScopeCut {
                detail: "synthetic target ended".into(),
            });
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected host resend")
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
    fn now_ms(&self) -> f64 {
        100.0
    }
    fn random(&self) -> f64 {
        0.5
    }
}
struct Fixture {
    rt: Option<Runtime>,
    store: Option<Arc<Store>>,
    host: Arc<Transport>,
    root: PathBuf,
    id: String,
}
impl Fixture {
    fn new(class: StoredClass, replies: Vec<Result<Dispatched, TransportError>>) -> Self {
        Self::with_hooks(class, replies, Arc::new(NoHooks))
    }
    fn with_hooks(
        class: StoredClass,
        replies: Vec<Result<Dispatched, TransportError>>,
        hooks: Arc<dyn basal_core::Hooks>,
    ) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "transport-intents-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(Store::open(root.join("store.db"), Durability::throwaway()).unwrap());
        let host = Arc::new(Transport {
            class,
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
            scope_cut: Mutex::new(None),
        });
        let catalog = Arc::new(MockCatalog::standard());
        catalog.set_flow_capable("test", true);
        catalog.set_op(
            "test",
            "act",
            basal_host::catalog::OpDecl {
                kind: Some(if class == StoredClass::Query {
                    basal_host::catalog::OpKind::Query
                } else {
                    basal_host::catalog::OpKind::Mutate
                }),
                cause_echo: true,
                shell_capable: false,
            },
        );
        let rt = Runtime::new(
            store.clone(),
            host.clone(),
            catalog,
            hooks,
            None,
            Config {
                install_gate: InstallGate::Off,
                auto_resume: false,
                clock: Clock::manual(100),
                retry_backoff: Duration::ZERO,
                unavailable_retries: 2,
                ..Config::default()
            },
        )
        .unwrap();
        let manifest=json!({"id":"retry","version":1,"purpose":"Exercise counted durable transport retries","trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"test","op":"act"}]}).to_string();
        let installed = rt
            .install(&InstallRequest {
                script: "return 7;".into(),
                manifest,
                author: "operator".into(),
                loop_override: false,
            })
            .unwrap();
        rt.approve("retry", 1, &installed.code_hash, "transport test")
            .unwrap();
        let id = match rt
            .admit_trigger(
                "retry",
                "fire",
                JsonText::new("{\"kind\":\"manual\"}").unwrap(),
            )
            .unwrap()
        {
            Admission::Admitted { run_id } => run_id,
            other => panic!("{other:?}"),
        };
        Self {
            rt: Some(rt),
            store: Some(store),
            host,
            root,
            id,
        }
    }
    fn rt(&self) -> &Runtime {
        self.rt.as_ref().unwrap()
    }
    fn send_first(&self) {
        let activation = self.rt().fixture_activation(&self.id).unwrap();
        self.rt()
            .fixture_append(&self.id, Body::ScriptStarted)
            .unwrap();
        let issued = activation
            .issue(HostCall {
                position: 0,
                kind: CallKind::Op {
                    module: "test".into(),
                    op: "act".into(),
                },
                args: JsonText::new("{}").unwrap(),
            })
            .unwrap();
        let Issued::Dispatch(dispatch) = issued else {
            panic!("call was not dispatched")
        };
        dispatch.send().unwrap();
    }
    fn finish(&self) {
        self.rt()
            .fixture_append(
                &self.id,
                Body::Released {
                    position: 0,
                    delivery_order: 0,
                },
            )
            .unwrap();
        self.rt()
            .fixture_finish(&self.id, RunState::Succeeded, Some(json!(7)), None)
            .unwrap();
    }
    fn intents(&mut self) -> Vec<basal_core::run_records::Intent> {
        self.rt.take();
        let store = self.store.as_ref().unwrap();
        let meta = store.read(basal_core::run_store_sql::meta).unwrap();
        let (log, frames) = basal_core::run_log::RunLog::<Record>::open(
            basal_core::run_log::Config {
                directory: self.root.join("run-log"),
                identity: basal_core::run_log::Identity {
                    store_id: meta.store_id.as_bytes().try_into().unwrap(),
                    log_epoch: meta.log_epoch,
                },
                lease_epoch: store.lease_epoch(),
                target_size: basal_core::run_log::Config::TARGET_SIZE,
                hooks: Arc::new(NoHooks),
            },
            Arc::new(basal_core::run_log::SqliteRegistry::new(
                store.clone(),
                meta.log_epoch,
            )),
        )
        .unwrap();
        drop(log);
        frames
            .records
            .into_iter()
            .filter(|f| f.record.run_id == self.id)
            .filter_map(|f| {
                if let Body::Intent(intent) = f.record.body {
                    Some(intent)
                } else {
                    None
                }
            })
            .collect()
    }
    fn dispatches(&self) -> u64 {
        self.store
            .as_ref()
            .unwrap()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT dispatches FROM rate_windows WHERE flow_id='retry'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.rt.take();
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn success() -> Result<Dispatched, TransportError> {
    Ok(Dispatched::Completed(HostOutcome::fulfilled(
        JsonText::new("7").unwrap(),
    )))
}

#[test]
fn proven_unsent_transport_retry_has_two_intents_two_counted_dispatches_and_two_sends() {
    let mut f = Fixture::new(
        StoredClass::Query,
        vec![Err(TransportError::unsent("never forwarded")), success()],
    );
    f.send_first();
    f.rt().fixture_dispatch_pending(&f.id).unwrap();
    f.finish();
    assert_eq!(f.host.calls.lock().unwrap().len(), 2);
    assert_eq!(
        f.dispatches(),
        2,
        "transport attempts consume dispatch budget even if proven unsent"
    );
    let intents = f.intents();
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].attempts, 1);
    assert_eq!(intents[1].attempts, 2);
}

#[test]
fn possibly_sent_unkeyed_transport_failure_is_unknown_without_a_resend() {
    let mut f = Fixture::new(
        StoredClass::Mutation,
        vec![Err(TransportError::maybe_sent(
            UnknownReason::ConnectionLost,
            "lost reply",
        ))],
    );
    f.send_first();
    f.rt().fixture_dispatch_pending(&f.id).unwrap();
    assert_eq!(f.host.calls.lock().unwrap().len(), 1);
    assert_eq!(
        f.rt().unknown_reason(&f.id, 0).unwrap(),
        Some(UnknownReason::ConnectionLost)
    );
    assert_eq!(f.rt().run(&f.id).unwrap().state, RunState::NeedsReconcile);
    assert_eq!(f.intents().len(), 1);
}

#[test]
fn possibly_sent_keyed_transport_retry_keeps_its_key_and_counts_the_retry() {
    let mut f = Fixture::new(
        StoredClass::KeyedMutation,
        vec![
            Err(TransportError::maybe_sent(
                UnknownReason::ReplyTimeout,
                "lost reply",
            )),
            success(),
        ],
    );
    f.send_first();
    f.rt().fixture_dispatch_pending(&f.id).unwrap();
    f.finish();
    {
        let calls = f.host.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].idempotency_key, calls[1].idempotency_key);
        assert_eq!(calls[1].attempt, 2);
    }
    assert_eq!(f.dispatches(), 2);
    let intents = f.intents();
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].idempotency_key, intents[1].idempotency_key);
}

#[test]
fn later_unsent_attempt_cannot_erase_an_earlier_ambiguous_keyed_send() {
    let refusal = basal_host::flow_refusal::FlowRefusal::new(
        basal_host::flow_refusal::RefusalReason::ScopeNotLive,
        "test",
        "act",
    );
    let f = Fixture::new(
        StoredClass::KeyedMutation,
        vec![
            Err(TransportError::maybe_sent(
                UnknownReason::ReplyUnreadable,
                "lost reply",
            )),
            Err(TransportError::unsent("not forwarded")),
            Err(TransportError::Refused(refusal)),
        ],
    );
    f.send_first();
    f.rt().fixture_dispatch_pending(&f.id).unwrap();
    assert_eq!(f.host.calls.lock().unwrap().len(), 3);
    assert_eq!(
        f.rt().unknown_reason(&f.id, 0).unwrap(),
        Some(UnknownReason::ReplyUnreadable)
    );
}

#[test]
fn concurrent_calls_report_their_actual_nonzero_boundary_positions() {
    struct Positions(Mutex<Vec<(&'static str, u64)>>);
    impl basal_core::Hooks for Positions {
        fn at(&self, _: &str, boundary: &basal_core::Boundary) -> basal_core::Step {
            use basal_core::Boundary::*;
            let entry = match boundary {
                DriverAppended { position } => Some(("appended", *position)),
                DriverSynced { position } => Some(("synced", *position)),
                DriverDispatched { position } => Some(("dispatched", *position)),
                HostArrived { position } => Some(("arrived", *position)),
                DriverReleased { position } => Some(("released", *position)),
                _ => None,
            };
            if let Some(entry) = entry {
                self.0.lock().unwrap().push(entry);
            }
            basal_core::Step::Continue
        }
    }
    let positions = Arc::new(Positions(Mutex::new(Vec::new())));
    let f = Fixture::with_hooks(
        StoredClass::Query,
        vec![success(), success()],
        positions.clone(),
    );
    *f.host.gate.lock().unwrap() = Some(Arc::new(std::sync::Barrier::new(2)));
    let activation = f.rt().fixture_activation(&f.id).unwrap();
    let mut sends = Vec::new();
    for position in 0..2 {
        let Issued::Dispatch(dispatch) = activation
            .issue(HostCall {
                position,
                kind: CallKind::Op {
                    module: "test".into(),
                    op: "act".into(),
                },
                args: JsonText::new(json!({"position":position}).to_string()).unwrap(),
            })
            .unwrap()
        else {
            panic!("missing dispatch")
        };
        sends.push(dispatch);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = sends
            .into_iter()
            .map(|dispatch| scope.spawn(move || dispatch.send().unwrap()))
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    assert!(activation.release(&[0, 1]).unwrap().is_some());
    assert!(activation.release(&[0, 1]).unwrap().is_some());
    for kind in ["appended", "synced", "dispatched", "arrived", "released"] {
        let mut observed: Vec<_> = positions
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, p)| *p)
            .collect();
        observed.sort_unstable();
        assert_eq!(observed, [0, 1], "wrong {kind} call position");
    }
}

#[test]
fn scope_cut_is_position_specific_durable_before_reply_and_never_retried_after_restart() {
    use basal_core::{Boundary, Hooks, Step};
    struct CutAfterOtherArrives {
        arrived: Mutex<bool>,
        ready: std::sync::Condvar,
    }
    impl Hooks for CutAfterOtherArrives {
        fn at(&self, _: &str, boundary: &Boundary) -> Step {
            if *boundary == (Boundary::HostArrived { position: 1 }) {
                *self.arrived.lock().unwrap() = true;
                self.ready.notify_all();
            }
            if *boundary == (Boundary::DriverDispatched { position: 0 }) {
                let (arrived, wait) = self
                    .ready
                    .wait_timeout_while(
                        self.arrived.lock().unwrap(),
                        Duration::from_secs(30),
                        |arrived| !*arrived,
                    )
                    .unwrap();
                assert!(
                    *arrived && !wait.timed_out(),
                    "other call must be durable before the crash"
                );
                return Step::Crash;
            }
            Step::Continue
        }
    }
    let mut f = Fixture::with_hooks(
        StoredClass::KeyedMutation,
        vec![success()],
        Arc::new(CutAfterOtherArrives {
            arrived: Mutex::new(false),
            ready: std::sync::Condvar::new(),
        }),
    );
    *f.host.scope_cut.lock().unwrap() = Some(0);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    *f.host.gate.lock().unwrap() = Some(barrier.clone());
    let activation = f.rt().fixture_activation(&f.id).unwrap();
    let issue = |position| match activation
        .issue(HostCall {
            position,
            kind: CallKind::Op {
                module: "test".into(),
                op: "act".into(),
            },
            args: JsonText::new("{}").unwrap(),
        })
        .unwrap()
    {
        Issued::Dispatch(dispatch) => dispatch,
        _ => panic!("dispatch"),
    };
    let cut = issue(0);
    let other = issue(1);
    let cut = std::thread::spawn(move || cut.send());
    let other = std::thread::spawn(move || other.send());
    barrier.wait();
    assert!(other.join().unwrap().is_ok());
    assert!(matches!(
        cut.join().unwrap(),
        Err(basal_core::CoreError::Cut)
    ));
    drop(activation);
    *f.host.gate.lock().unwrap() = None;
    f.rt.take();
    f.store.take();
    let store = Arc::new(Store::open(f.root.join("store.db"), Durability::throwaway()).unwrap());
    let recovered = Runtime::new(
        store.clone(),
        f.host.clone(),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            install_gate: InstallGate::Off,
            auto_resume: false,
            clock: Clock::manual(101),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        recovered.run(&f.id).unwrap().state,
        RunState::NeedsReconcile
    );
    assert!(matches!(
        recovered
            .activate(
                &f.id,
                &mut basal_core::test_support::ScriptedWorker::new(vec![])
            )
            .unwrap(),
        basal_core::ActivationEnd::NotRunnable {
            state: RunState::NeedsReconcile
        }
    ));
    let state = recovered.fixture_state(&f.id).unwrap();
    assert!(state.calls[&0].scope_cut);
    assert_eq!(state.calls[&0].unknown.as_deref(), Some("connection_lost"));
    assert!(!state.calls[&1].scope_cut);
    assert_eq!(
        state.calls[&1].arrival.as_ref().unwrap().value.as_str(),
        "7"
    );
    let rows = recovered.calls(&f.id).unwrap();
    assert!(rows[0].scope_cut);
    assert!(!rows[1].scope_cut);
    assert_eq!(
        f.host.calls.lock().unwrap().len(),
        2,
        "no call may be resent after restart"
    );
    f.rt = Some(recovered);
    f.store = Some(store);
}
