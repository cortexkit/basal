use super::*;
use crate::pool::{PoolConfig, Spawn};
use crate::process::{SpawnError, WorkerLaunch, WorkerProcess};
use basal_core::{Config, Durability, NoHooks, Store};
use basal_host::{MockCatalog, MockConsent, mock::MockHost};
use std::sync::atomic::AtomicUsize;

struct RefuseOrPanic(bool);
impl Spawn for RefuseOrPanic {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        assert!(!self.0, "spawn panic");
        Err(SpawnError::Exec("missing worker".into()))
    }
}

fn engine(panic: bool) -> (Engine, std::path::PathBuf) {
    engine_with_spawner(Arc::new(RefuseOrPanic(panic)), EngineConfig::default())
}

fn engine_with_spawner(
    spawner: Arc<dyn Spawn>,
    engine_config: EngineConfig,
) -> (Engine, std::path::PathBuf) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "basal-engine-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.db");
    let clock = basal_core::Clock::manual(0);
    let rt = Runtime::new(
        Arc::new(Store::open(&path, Durability { fullfsync: false }).unwrap()),
        Arc::new(MockHost::new()),
        Arc::new(MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        Config {
            clock: clock.clone(),
            ..Config::default()
        },
    );
    let metrics = Arc::new(Metrics::default());
    let mut config = PoolConfig::new("missing", WorkerLaunch::Plain);
    config.warm_spares = 0;
    let pool = Pool::new(config, spawner, clock, metrics.clone());
    (
        Engine::new(
            rt,
            pool,
            Arc::new(MockConsent::new()),
            engine_config,
            metrics,
            Fatal::new(),
        )
        .unwrap(),
        path,
    )
}

#[test]
fn idle_engine_runs_only_startup_and_fallback_passes() {
    let (engine, path) = engine(false);
    let mut passes = 0;
    let clock = engine.inner.rt.config().clock.clone();
    // Advance the clock by the actual timeout requested by the production
    // loop. Several old 200ms intervals fit before the first fallback.
    engine.run_loop(|_, timeout| {
        passes += 1;
        clock.advance(timeout.as_millis() as i64);
        if clock.now_ms() >= 1_200 {
            engine.stop();
        }
    });
    assert_eq!(
        passes, 1,
        "scheduler and retention were serviced only at startup"
    );
    cleanup(engine, path);
}

struct ObserveSpawn(std::sync::mpsc::Sender<()>);
impl Spawn for ObserveSpawn {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        self.0.send(()).unwrap();
        Err(SpawnError::Exec("missing worker".into()))
    }
}

fn install_flow(rt: &Runtime, id: &str) -> basal_core::Installed {
    let installed = rt.install(&request(id)).unwrap();
    rt.approve(id, 1, &installed.code_hash, "operator").unwrap();
    installed
}

fn request(id: &str) -> basal_core::InstallRequest {
    basal_core::InstallRequest {
        script: "return 1;".into(),
        manifest: serde_json::json!({"id":id,"version":1,"purpose":"Wake test", "trigger":{"events":[{"module":"plexus","name":"pull_request_review","version":1}]}}).to_string(),
        author: "SYNAPSE".into(),
        loop_override: false,
    }
}

/// Wait observers are barriers, not elapsed-time assertions. The fallback is
/// ten minutes, far longer than the unchanged 60s observer hang bound.
fn wake_case(event: impl FnOnce(&Engine), raw_admit: bool) {
    let (started_tx, started) = std::sync::mpsc::channel();
    let (engine, path) = engine_with_spawner(
        Arc::new(ObserveSpawn(started_tx)),
        EngineConfig {
            pass_interval: Duration::from_secs(600),
            ..EngineConfig::default()
        },
    );
    install_flow(&engine.inner.rt, "due");
    engine.inner.rt.install(&request("control")).unwrap();
    let (park_tx, park) = std::sync::mpsc::channel();
    let thread_engine = engine.clone();
    let loop_thread = thread::spawn(move || {
        thread_engine.run_loop(|seen, timeout| {
            let _ = park_tx.send(());
            thread_engine.inner.wake.wait(seen, timeout);
        })
    });
    park.recv_timeout(Duration::from_secs(60)).unwrap();
    if raw_admit {
        let rt = &engine.inner.rt;
        rt.store()
            .write(|tx| {
                basal_core::admission::admit_current(
                    tx,
                    rt.store().store_id(),
                    "due",
                    "raw",
                    basal_proto::JsonText::null(),
                    &basal_core::admission::AdmitContext {
                        now_ms: 0,
                        rate: rt.config().rate,
                        default_deadline_ms: 600_000,
                    },
                )
            })
            .unwrap();
    }
    event(&engine);
    let result = started.recv_timeout(Duration::from_secs(60));
    engine.stop();
    loop_thread.join().unwrap();
    engine.wait_quiet();
    assert!(
        result.is_ok(),
        "the work signal never started the due activation"
    );
    cleanup(engine, path);
}

#[test]
fn admission_wakes_the_sleeping_engine() {
    wake_case(
        |engine| {
            engine
                .inner
                .rt
                .admit_trigger("due", "event", basal_proto::JsonText::null())
                .unwrap();
        },
        false,
    );
}

#[test]
fn install_approval_enable_and_disable_wake_the_engine() {
    wake_case(
        |engine| {
            engine.inner.rt.install(&request("installed")).unwrap();
        },
        true,
    );
    wake_case(
        |engine| {
            let hash = basal_core::ids::code_hash(
                &request("control").script,
                &request("control").manifest,
            );
            engine
                .inner
                .rt
                .approve("control", 1, &hash, "operator")
                .unwrap();
        },
        true,
    );
    wake_case(
        |engine| {
            engine
                .inner
                .rt
                .store()
                .write(|tx| {
                    basal_core::install::disable(
                        tx,
                        "control",
                        &basal_core::Actor::Operator("op".into()),
                        "stop",
                        0,
                    )
                    .map_err(|e| basal_core::CoreError::Invalid(e.to_string()))?;
                    Ok(())
                })
                .unwrap();
            engine.inner.rt.enable_flow("control").unwrap();
        },
        true,
    );
    wake_case(
        |engine| {
            engine
                .inner
                .rt
                .disable_flow("control", &basal_core::Actor::Operator("op".into()), "stop")
                .unwrap();
        },
        true,
    );
}

#[test]
fn shutdown_wakes_a_parked_loop() {
    let (engine, path) = engine(false);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = engine.clone();
    let loop_thread = thread::spawn(move || {
        e.run_loop(|seen, timeout| {
            tx.send(()).unwrap();
            e.inner.wake.wait(seen, timeout);
        })
    });
    rx.recv_timeout(Duration::from_secs(60)).unwrap();
    engine.stop();
    loop_thread.join().unwrap();
    cleanup(engine, path);
}

#[test]
fn a_decision_answer_wakes_the_sleeping_engine() {
    wake_case(
        |engine| {
            let rt = &engine.inner.rt;
            rt.store()
                .write(|tx| {
                    basal_core::install::disable(
                        tx,
                        "control",
                        &basal_core::Actor::Runtime,
                        "run_limit_saturated",
                        0,
                    )
                    .map_err(|e| basal_core::CoreError::Invalid(e.to_string()))?;
                    basal_core::decisions::record_auto_disable(
                        tx,
                        "control",
                        &basal_host::DecisionContext::Reenable {
                            disabled_at_ms: 0,
                            disabled_reason: basal_host::DisabledReason::RunLimitSaturated,
                            limit: 10,
                            window_ms: 60_000,
                            saturated_windows: 3,
                        },
                        0,
                    )
                })
                .unwrap();
            let card = rt.decision_cards().unwrap().pop().unwrap();
            rt.decision_raised(card.seq, card.revision, "answer-id")
                .unwrap();
            rt.answer_decision(&basal_host::DecisionAnswer {
                grant_context: None,
                elicitation_id: "answer-id".into(),
                choice: Some(basal_core::decisions::KEEP.into()),
                dedup_key: Some(card.dedup_key),
                decision: basal_host::DecisionKind::Reenable,
                run_id: None,
                call_key: None,
                flow_id: "control".into(),
                version: 1,
            })
            .unwrap();
        },
        true,
    );
}

#[test]
fn a_run_deadline_wakes_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(3600),
            ..EngineConfig::default()
        },
    );
    install_flow(&engine.inner.rt, "deadline");
    let run = engine
        .inner
        .rt
        .admit_trigger("deadline", "trigger", basal_proto::JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    engine
        .inner
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE runs SET state='suspended', deadline_at=600000 WHERE run_id=?1",
                [&run],
            )?;
            Ok(())
        })
        .unwrap();
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    assert_eq!(clock.now_ms(), 600_000);
    assert_eq!(
        engine.inner.rt.run(&run).unwrap().state,
        basal_core::RunState::Failed
    );
    cleanup(engine, path);
}

#[test]
fn a_schedule_wakes_at_its_due_time_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(600),
            ..EngineConfig::default()
        },
    );
    let mut request = request("scheduled");
    let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
    manifest["trigger"] = serde_json::json!({"schedule":{"interval":"1m"}});
    request.manifest = manifest.to_string();
    let installed = engine.inner.rt.install(&request).unwrap();
    engine
        .inner
        .rt
        .approve("scheduled", 1, &installed.code_hash, "operator")
        .unwrap();
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    engine.wait_quiet();
    let admitted: Vec<i64> = engine
        .inner
        .rt
        .store()
        .read(|c| {
            Ok(c.prepare("SELECT admitted_at FROM runs")
                .unwrap()
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(admitted, [60_000]);
    cleanup(engine, path);
}

#[test]
fn retention_wakes_on_its_own_cadence_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(7200),
            ..EngineConfig::default()
        },
    );
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    assert_eq!(clock.now_ms(), 3_600_000);
    // That retention pass moved the next retention time another hour on.
    assert_eq!(engine.inner.rt.next_wake_at(None).unwrap(), Some(7_200_000));
    cleanup(engine, path);
}

struct GateOnly;
impl basal_core::WorkerChannel for GateOnly {
    fn welcome(&self) -> &basal_proto::Welcome {
        panic!("an unanswered gate must not enter the worker")
    }
    fn send(&mut self, _: &basal_proto::ParentMessage) -> Result<(), basal_core::ChannelError> {
        panic!("worker entered")
    }
    fn recv(
        &mut self,
        _: Duration,
    ) -> Result<basal_proto::WorkerMessage, basal_core::ChannelError> {
        panic!("worker entered")
    }
    fn receiver(&mut self) -> Box<dyn basal_core::channel::WorkerReceiver> {
        panic!("worker entered")
    }
    fn kill(&mut self) {}
}

#[test]
fn an_install_gate_retry_wakes_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(600),
            ..EngineConfig::default()
        },
    );
    install_flow(&engine.inner.rt, "gate");
    let run = engine
        .inner
        .rt
        .admit_trigger("gate", "trigger", basal_proto::JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    let ActivationEnd::Deferred { retry_in, .. } =
        engine.inner.rt.activate(&run, &mut GateOnly).unwrap()
    else {
        panic!("core is unreachable, so the gate must defer")
    };
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    engine.wait_quiet();
    assert_eq!(clock.now_ms(), retry_in.as_millis() as i64);
    cleanup(engine, path);
}

#[test]
fn a_deferred_call_retry_wakes_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(600),
            ..EngineConfig::default()
        },
    );
    install_flow(&engine.inner.rt, "deferred");
    let run = engine
        .inner
        .rt
        .admit_trigger("deferred", "trigger", basal_proto::JsonText::null())
        .unwrap()
        .run_id()
        .unwrap()
        .to_owned();
    engine
        .inner
        .rt
        .store()
        .write(|tx| {
            let lease = basal_core::runs::claim(tx, &run, "owner", 0)?;
            basal_core::journal::insert_call(
                tx,
                &lease,
                "deferred",
                &basal_core::journal::NewCall {
                    position: 0,
                    kind: &basal_proto::CallKind::Op {
                        module: "mock".into(),
                        op: "send".into(),
                    },
                    args: &basal_proto::JsonText::null(),
                    class: basal_core::StoredClass::KeyedMutation,
                    request: None,
                },
            )?;
            basal_core::journal::defer(
                tx,
                &run,
                0,
                &basal_host::flow_refusal::FlowRefusal::new(
                    basal_host::flow_refusal::RefusalReason::ResourceBusy,
                    "mock",
                    "send",
                ),
                10_000,
            )?;
            basal_core::runs::park(tx, &lease, &[0], 0)?;
            Ok(())
        })
        .unwrap();
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    engine.wait_quiet();
    assert_eq!(clock.now_ms(), 10_000);
    assert_eq!(
        engine.inner.rt.run(&run).unwrap().state,
        basal_core::RunState::Pending
    );
    cleanup(engine, path);
}

#[test]
fn worker_retirement_wakes_before_the_fallback() {
    let config = PoolConfig::new(basal_testkit::channel::worker_binary(), WorkerLaunch::Plain);
    let spawner = Arc::new(crate::pool::ProcessSpawner::new(&config));
    let (engine, path) = engine_with_spawner(
        spawner,
        EngineConfig {
            pass_interval: Duration::from_secs(7200),
            ..EngineConfig::default()
        },
    );
    let worker = engine
        .inner
        .pool
        .acquire(Binding::Flow("idle".into()))
        .unwrap();
    drop(worker);
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    assert_eq!(clock.now_ms(), 600_000);
    assert_eq!(
        engine.inner.metrics.workers_retired.load(Ordering::Relaxed),
        1
    );
    assert_eq!(engine.inner.pool.stats().live, 0);
    cleanup(engine, path);
}

#[test]
fn a_pool_spawn_retry_wakes_before_the_fallback() {
    let (engine, path) = engine_with_spawner(
        Arc::new(RefuseOrPanic(false)),
        EngineConfig {
            pass_interval: Duration::from_secs(600),
            ..EngineConfig::default()
        },
    );
    assert!(
        engine
            .inner
            .pool
            .acquire(Binding::Flow("retry".into()))
            .is_err()
    );
    let clock = engine.inner.rt.config().clock.clone();
    let mut passes = 0;
    engine.run_loop(|_, timeout| {
        passes += 1;
        if passes == 2 {
            engine.stop();
        } else {
            clock.advance(timeout.as_millis() as i64);
        }
    });
    assert_eq!(clock.now_ms(), 250);
    cleanup(engine, path);
}

fn cleanup(engine: Engine, path: std::path::PathBuf) {
    engine.inner.pool.stop();
    drop(engine);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn panicking_activation_releases_run_and_flow_slots() {
    let (engine, path) = engine(true);
    engine.active().runs.insert("r".into());
    engine.active().flows.insert("f".into());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.activate("r".into(), "f".into())
    }));
    assert!(result.is_err(), "control reached the panicking spawner");
    assert_eq!(engine.active_count(), 0);
    assert!(engine.active().flows.is_empty());
    assert!(
        engine.inner.fatal.get().is_some(),
        "a panic may leave the run claimed and must trigger recovery"
    );
    engine.wait_quiet();
    cleanup(engine, path);
}

#[test]
fn journal_count_failure_is_fatal_not_zero_replayed_calls() {
    let (engine, path) = engine(false);
    engine.inner.rt.store().cut();
    engine.activate("r".into(), "f".into());
    assert!(engine.inner.fatal.get().is_some());
    cleanup(engine, path);
}

/// Starts a whole module on a fresh store. Its worker spawner panics when
/// `spawn_panics` is set and refuses otherwise. Returns the store path and the
/// consent adapter, which stays attached.
fn module(spawn_panics: bool) -> (crate::module::Module, std::path::PathBuf, Arc<MockConsent>) {
    use crate::dryrun::DryRunConfig;
    use crate::module::{Hosts, Module, ModuleConfig};

    let (old_engine, path) = engine(false);
    cleanup(old_engine, path.clone());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let consent = Arc::new(MockConsent::new());
    let mut pool = PoolConfig::new("missing", WorkerLaunch::Plain);
    pool.warm_spares = 0;
    let module = Module::start(
        ModuleConfig {
            store_path: path.clone(),
            durability: Durability { fullfsync: false },
            runtime: Config {
                clock: basal_core::Clock::manual(0),
                install_gate: basal_core::InstallGate::Off,
                ..Config::default()
            },
            pool,
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(path.parent().unwrap().join("dry")),
        },
        Hosts {
            transport: Arc::new(crate::unconfigured::UnconfiguredTransport),
            host: Arc::new(MockHost::new()),
            catalog: Arc::new(MockCatalog::standard()),
            consent: consent.clone(),
            hooks: Arc::new(NoHooks),
        },
        Arc::new(RefuseOrPanic(spawn_panics)),
    )
    .unwrap();
    (module, path, consent)
}

#[test]
fn module_drop_joins_activation_after_slot_release() {
    use std::sync::{Barrier, mpsc};

    let (module, path, consent) = module(true);
    install_flow(&module.rt, "cut");
    module
        .rt
        .admit_trigger("cut", "event", basal_proto::JsonText::null())
        .unwrap();

    let release = Arc::new(Barrier::new(2));
    let tail_release = release.clone();
    let (parked_tx, parked) = mpsc::channel();
    *module.engine.inner.activation_exit.lock().unwrap() = Some(Arc::new(move || {
        parked_tx.send(()).unwrap();
        tail_release.wait();
    }));
    module.engine.pass().unwrap();
    parked.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(module.engine.active_count(), 0);
    assert!(module.fatal.get().unwrap().contains("panicked"));

    #[derive(Debug, PartialEq)]
    enum Event {
        Joining,
        Dropped,
    }
    let (event_tx, events) = mpsc::channel();
    let joining_tx = event_tx.clone();
    *module.engine.inner.joining.lock().unwrap() = Some(Arc::new(move || {
        joining_tx.send(Event::Joining).unwrap();
    }));
    let dropping = thread::spawn(move || {
        drop(module);
        event_tx.send(Event::Dropped).unwrap();
    });
    // With the join removed, Dropped arrives while the activation still owns
    // the engine. Open before releasing that barrier to expose the lease race
    // without depending on the scheduler or an elapsed-time assertion.
    let first = events.recv_timeout(Duration::from_secs(60)).unwrap();
    if first == Event::Joining {
        release.wait();
        assert_eq!(
            events.recv_timeout(Duration::from_secs(60)).unwrap(),
            Event::Dropped
        );
    }
    let reopened = Store::open(&path, Durability { fullfsync: false });
    if first == Event::Dropped {
        release.wait();
    }
    dropping.join().unwrap();
    assert!(
        reopened.is_ok(),
        "drop returned with the storage lease still held: {:?}",
        reopened.err()
    );
    // The consent adapter is deliberately still alive and attached here.
    drop(consent);
    // On Windows an open database blocks deleting its directory.
    drop(reopened);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

/// Makes the module's shutdown stall inside the engine's thread join until the
/// returned sender is dropped, and shortens how long drop waits for it.
fn stall_shutdown(module: &mut crate::module::Module) -> std::sync::mpsc::Sender<()> {
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    *module.engine.inner.joining.lock().unwrap() = Some(Arc::new(move || {
        let _ = released.lock().unwrap_or_else(|p| p.into_inner()).recv();
    }));
    module.shutdown_grace = Duration::from_millis(100);
    release
}

const UNWINDING_DROP_CHILD: &str =
    "engine::lifecycle_tests::stalled_module_drop_while_unwinding_child";
const UNWINDING_DROP_ENV: &str = "BASAL_STALLED_MODULE_DROP_CHILD";
const UNWINDING_DROP_SURVIVED: &str = "the process survived a stalled module drop while unwinding";

/// Does its work only in the subprocess the next test starts: a panic unwinds
/// past a module whose shutdown stalls, so the module is dropped while the
/// thread is already panicking. A second panic from that drop would abort the
/// process, which only a separate process can observe.
#[test]
fn stalled_module_drop_while_unwinding_child() {
    if std::env::var_os(UNWINDING_DROP_ENV).is_none() {
        return;
    }
    let (mut module, path, consent) = module(false);
    let release = stall_shutdown(&mut module);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _owned = module;
        panic!("a failure while the module is alive");
    }));
    let payload = unwound.expect_err("the closure panics");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"a failure while the module is alive"),
        "the original panic must be the one that reaches the caller"
    );
    println!("{UNWINDING_DROP_SURVIVED}");
    drop(release);
    drop(consent);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_stalled_module_drop_while_unwinding_does_not_abort_the_process() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", UNWINDING_DROP_CHILD, "--nocapture"])
        .env(UNWINDING_DROP_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The marker also proves the child test ran rather than matching nothing.
    assert!(
        output.status.success() && stdout.contains(UNWINDING_DROP_SURVIVED),
        "child exited with {:?}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_stalled_module_drop_panics_when_not_unwinding() {
    let (mut module, path, consent) = module(false);
    let release = stall_shutdown(&mut module);
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(module)));
    let payload = dropped.expect_err("a stalled shutdown must fail loudly");
    let message = payload
        .downcast_ref::<String>()
        .expect("the stall panic carries a formatted message");
    assert!(
        message.contains("module shutdown did not release its threads"),
        "{message}"
    );
    drop(release);
    drop(consent);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
