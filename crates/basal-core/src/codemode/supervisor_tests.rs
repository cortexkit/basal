use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use basal_host::flow_refusal::FlowRefusal;
use basal_host::flow_scope::RegisteredScope;
use basal_host::{MockCatalog, OpDecl, OpKind};
use basal_proto::{Confinement, PROTOCOL_VERSION, Primitive, Welcome};

use super::super::admission::{Limits, Tool, Unavailable};
use super::super::catalog::compile_input_schema;
use super::super::store::tests::{DIGEST, new_run};
use crate::store::Durability;

static NEXT: AtomicUsize = AtomicUsize::new(0);
const TIMEOUT: Duration = Duration::from_secs(10);

// Cleanup may hit the same broken invariant that is already failing a test.
// Run it off the unwinding thread so that a second panic cannot abort libtest,
// and bound the join observation so a stuck event-driven driver fails the test.
fn teardown(action: impl FnOnce() + Send + 'static) {
    let unwinding = thread::panicking();
    let (done, finished) = mpsc::channel();
    let cleanup = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action));
        let _ = done.send(result);
    });
    match finished.recv_timeout(TIMEOUT) {
        Ok(result) => {
            cleanup.join().unwrap();
            if let Err(panic) = result {
                if unwinding {
                    tracing::error!("codemode test cleanup panicked while unwinding");
                } else {
                    std::panic::resume_unwind(panic);
                }
            }
        }
        Err(error) if unwinding => {
            tracing::error!(%error, "codemode test cleanup did not finish while unwinding");
        }
        Err(error) => panic!("codemode test cleanup did not finish: {error}"),
    }
}

/// A helper thread whose join the test bounds. `shutdown` and `cancel` wait
/// for a run's driver to exit, so a driver that never exits would block a bare
/// join forever and hang the whole test binary instead of failing the test.
struct BoundedThread<T> {
    finished: mpsc::Receiver<std::thread::Result<T>>,
    thread: thread::JoinHandle<()>,
}

fn spawn_bounded<T: Send + 'static>(
    action: impl FnOnce() -> T + Send + 'static,
) -> BoundedThread<T> {
    let (done, finished) = mpsc::channel();
    let thread = thread::spawn(move || {
        let _ = done.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            action,
        )));
    });
    BoundedThread { finished, thread }
}

impl<T> BoundedThread<T> {
    /// Returns the thread's value, re-raises its panic, or fails the test
    /// naming `what` when the thread has not finished within `TIMEOUT`.
    fn join(self, what: &str) -> T {
        match self.finished.recv_timeout(TIMEOUT) {
            Ok(result) => {
                self.thread.join().unwrap();
                result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            }
            Err(error) => panic!("{what} did not return within {TIMEOUT:?}: {error}"),
        }
    }
}

struct Attempt {
    flow: String,
    module: String,
    op: String,
    input: Value,
    key: String,
    thread: String,
    reply: mpsc::SyncSender<std::result::Result<Value, WireError>>,
}

type Provider = dyn Fn(&str, Value, &str) -> std::result::Result<Value, WireError> + Send + Sync;

struct CustodyAttempt {
    name: String,
    input: Value,
    reply: mpsc::SyncSender<std::result::Result<Value, WireError>>,
}

struct FakeTransport {
    attempts: mpsc::Sender<Attempt>,
    events: Arc<Mutex<Vec<String>>>,
    registrations: Mutex<Vec<(String, Option<RegisteredScope>)>>,
    catalog_error: AtomicBool,
    broca_opted_in: AtomicBool,
    readiness: Mutex<Option<FlowRefusal>>,
    readiness_clock: Mutex<Option<(Clock, i64)>>,
    store: Arc<Store>,
    provider: Mutex<Option<Arc<Provider>>>,
    scope_end: Mutex<Option<Arc<basal_host::transport::RunScopeEnded>>>,
    custody_controlled: AtomicBool,
    custody_attempts: mpsc::Sender<CustodyAttempt>,
    custody_count: AtomicUsize,
    content_results: AtomicBool,
}

impl Transport for FakeTransport {
    fn role_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        input: Value,
        _: &basal_host::transport::RunToolOptions,
    ) -> std::result::Result<Value, WireError> {
        assert_eq!(run, "codemode:r");
        assert_eq!(module, "notes");
        self.custody_count.fetch_add(1, Ordering::SeqCst);
        if self.custody_controlled.load(Ordering::SeqCst) {
            let (reply, done) = mpsc::sync_channel(1);
            self.custody_attempts
                .send(CustodyAttempt {
                    name: name.into(),
                    input,
                    reply,
                })
                .unwrap();
            return done
                .recv_timeout(TIMEOUT)
                .unwrap_or_else(|_| Err(WireError::Unknown("fake route drained".into())));
        }
        Ok(match name {
            "late_results" => {
                json!({"entries":[],"cursor":{"provider_incarnation":"fake","seq":0},"more":false})
            }
            "late_results.ack" => json!({}),
            "tool.withdraw" => json!({"answer":"unknown_call"}),
            _ => panic!("unexpected custody operation {name}"),
        })
    }
    fn tool_for_run(
        &self,
        run: &str,
        module: &str,
        name: &str,
        input: Value,
        key: &str,
        _: &basal_host::transport::RunToolOptions,
    ) -> std::result::Result<Value, WireError> {
        self.tool_for_flow(run, module, name, input, key)
    }
    fn watch_run_scope(&self, run: &str, ended: Arc<basal_host::transport::RunScopeEnded>) {
        assert_eq!(run, "codemode:r");
        *self.scope_end.lock().unwrap() = Some(ended);
    }
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        self.events.lock().unwrap().push("catalog".into());
        if self.catalog_error.load(Ordering::SeqCst) {
            Err(WireError::NeverSent("offline".into()))
        } else {
            Ok(
                json!({"modules":[{"module_id":"broca","capabilities":{"provides":if self.broca_opted_in.load(Ordering::SeqCst) {vec!["agent-run-scopes/v1"]} else {vec![]}}}]}),
            )
        }
    }
    fn configure_flow(&self, key: &str, scope: Option<RegisteredScope>) {
        self.registrations
            .lock()
            .unwrap()
            .push((key.into(), scope.clone()));
        if scope.is_some() {
            self.events.lock().unwrap().push("register".into());
        } else {
            let id = key.strip_prefix("codemode:").unwrap();
            let calls = self.store.read(|c| store::calls(c, id)).unwrap();
            assert!(
                calls.iter().all(|c| c.outcome != Outcome::Pending),
                "settlement must precede release"
            );
            let Lookup::Found(run) = self.store.read(|c| store::lookup(c, id)).unwrap() else {
                panic!()
            };
            assert_eq!(
                run.status,
                Status::Running,
                "release must precede terminal commit"
            );
            self.events.lock().unwrap().push("release".into());
        }
    }
    fn provider_ready(
        &self,
        flow: &str,
        module: &str,
        op: &str,
    ) -> std::result::Result<(), FlowRefusal> {
        assert_eq!(flow, "codemode:r");
        self.events
            .lock()
            .unwrap()
            .push(format!("ready:{module}:{op}"));
        if let Some((clock, time)) = self.readiness_clock.lock().unwrap().as_ref() {
            clock.set(*time);
        }
        self.readiness.lock().unwrap().clone().map_or(Ok(()), Err)
    }
    fn management(
        &self,
        module: &str,
        op: &str,
        params: Value,
    ) -> std::result::Result<Value, WireError> {
        assert_eq!(module, "prefrontal-core");
        assert_eq!(op, "codemode.run_scope.close");
        assert_eq!(params, json!({"run_id":"r","epoch":7}));
        self.events.lock().unwrap().push("core_close".into());
        Ok(json!({"closed":true,"already_closed":false}))
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> std::result::Result<Value, WireError> {
        panic!("no unscoped calls")
    }
    fn tool_for_flow(
        &self,
        flow: &str,
        module: &str,
        op: &str,
        input: Value,
        key: &str,
    ) -> std::result::Result<Value, WireError> {
        let running: bool = self.store.read(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM codemode_calls WHERE idempotency_key=?1 AND intent_at IS NOT NULL AND entered_at IS NOT NULL AND outcome='pending')",[key],|r|r.get(0))?)).unwrap();
        assert!(
            running,
            "durable intent and entry must precede provider send"
        );
        let provider = self.provider.lock().unwrap().clone();
        if let Some(provider) = provider {
            return provider(flow, input, key);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.events.lock().unwrap().push("send".into());
        self.attempts
            .send(Attempt {
                flow: flow.into(),
                module: module.into(),
                op: op.into(),
                input,
                key: key.into(),
                thread: thread::current().name().unwrap_or("").into(),
                reply: tx,
            })
            .unwrap();
        rx.recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| Err(WireError::Unknown("test reply dropped".into())))
    }
}

/// What the fake worker's queue carries: a frame, or a nudge that makes a
/// blocked receive look at the fake's flags again.
enum Inbox {
    Frame(WorkerMessage),
    Wake,
}

/// The test's end of the fake worker's outgoing frames.
struct Frames(mpsc::Sender<Inbox>);
impl Frames {
    fn send(&self, message: WorkerMessage) -> std::result::Result<(), ChannelError> {
        self.0
            .send(Inbox::Frame(message))
            .map_err(|_| ChannelError::Closed)
    }
}

/// A fake worker condition a test switches on. Setting it wakes a receive
/// blocked on the fake's queue, as a real worker's exit would.
struct Flag {
    set: AtomicBool,
    wake: mpsc::Sender<Inbox>,
}
impl Flag {
    fn new(wake: &mpsc::Sender<Inbox>) -> Arc<Self> {
        Arc::new(Self {
            set: AtomicBool::new(false),
            wake: wake.clone(),
        })
    }
    fn store(&self, value: bool, order: Ordering) {
        self.set.store(value, order);
        let _ = self.wake.send(Inbox::Wake);
    }
    fn get(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
}

/// The fake worker's incoming side, shared by its channel and any receiver
/// split off from it.
#[derive(Clone)]
struct FakeQueue {
    panic_recv: Arc<Flag>,
    exit: Arc<Flag>,
    killed: Arc<AtomicBool>,
    messages: Arc<Mutex<mpsc::Receiver<Inbox>>>,
}
impl FakeQueue {
    /// `None` blocks until a frame or the end of the channel, as a real
    /// worker's receiver does.
    fn next(&self, timeout: Option<Duration>) -> std::result::Result<WorkerMessage, ChannelError> {
        let messages = self.messages.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            assert!(!self.panic_recv.get(), "test channel panic");
            if self.exit.get() || self.killed.load(Ordering::SeqCst) {
                return Err(ChannelError::Closed);
            }
            let next = match timeout {
                Some(timeout) => messages.recv_timeout(timeout).map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => ChannelError::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => ChannelError::Closed,
                }),
                None => messages.recv().map_err(|_| ChannelError::Closed),
            };
            if let Inbox::Frame(message) = next? {
                return Ok(message);
            }
        }
    }
}
impl WorkerReceiver for FakeQueue {
    fn recv(&mut self) -> std::result::Result<WorkerMessage, ChannelError> {
        self.next(None)
    }
}

struct FakeWorker {
    queue: FakeQueue,
    wake: mpsc::Sender<Inbox>,
    welcome: Welcome,
    parent: mpsc::Sender<ParentMessage>,
    events: Arc<Mutex<Vec<String>>>,
}
impl WorkerChannel for FakeWorker {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }
    fn send(&mut self, message: &ParentMessage) -> std::result::Result<(), ChannelError> {
        self.parent
            .send(message.clone())
            .map_err(|_| ChannelError::Closed)
    }
    fn recv(&mut self, timeout: Duration) -> std::result::Result<WorkerMessage, ChannelError> {
        self.queue.next(Some(timeout))
    }
    fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
        Box::new(self.queue.clone())
    }
    fn kill(&mut self) {
        self.events.lock().unwrap().push("kill".into());
        // A killed worker's channel ends, which releases a blocked receive.
        self.queue.killed.store(true, Ordering::SeqCst);
        let _ = self.wake.send(Inbox::Wake);
    }
}
struct FakeSource(Mutex<Option<FakeWorker>>);
impl WorkerSource for FakeSource {
    fn worker(&self) -> std::result::Result<Box<dyn WorkerChannel>, ChannelError> {
        self.0
            .lock()
            .unwrap()
            .take()
            .map(|w| Box::new(w) as Box<dyn WorkerChannel>)
            .ok_or(ChannelError::Closed)
    }
}

struct Fixture {
    worker_panic: Arc<Flag>,
    worker_exit: Arc<Flag>,
    supervisor: Supervisor,
    worker: Frames,
    parent: mpsc::Receiver<ParentMessage>,
    attempts: mpsc::Receiver<Attempt>,
    custody_attempts: mpsc::Receiver<CustodyAttempt>,
    host: Arc<FakeTransport>,
    catalog: Arc<MockCatalog>,
    source: Arc<FakeSource>,
    clock: Clock,
    dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "basal-supervisor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        Self::at(dir)
    }
    fn at(dir: PathBuf) -> Self {
        Self::at_with_provider(dir, None)
    }
    fn at_with_provider(dir: PathBuf, provider: Option<Arc<Provider>>) -> Self {
        let store = Arc::new(Store::open(dir.join("core.db"), Durability::throwaway()).unwrap());
        let events = Arc::new(Mutex::new(Vec::new()));
        let (worker, messages) = mpsc::channel();
        let (parent_tx, parent) = mpsc::channel();
        let (attempt_tx, attempts) = mpsc::channel();
        let (custody_tx, custody_attempts) = mpsc::channel();
        let hash = PreludeHash::of("parent's codemode prelude");
        let worker_panic = Flag::new(&worker);
        let worker_exit = Flag::new(&worker);
        let source = Arc::new(FakeSource(Mutex::new(Some(FakeWorker {
            queue: FakeQueue {
                panic_recv: worker_panic.clone(),
                exit: worker_exit.clone(),
                killed: Arc::new(AtomicBool::new(false)),
                messages: Arc::new(Mutex::new(messages)),
            },
            wake: worker.clone(),
            welcome: Welcome {
                protocol_version: PROTOCOL_VERSION,
                engine: "fake".into(),
                prelude_hash: PreludeHash::of("flow"),
                codemode_prelude_hash: hash,
                confinement: Confinement::None,
            },
            parent: parent_tx,
            events: events.clone(),
        }))));
        let host = Arc::new(FakeTransport {
            attempts: attempt_tx,
            events,
            registrations: Mutex::new(Vec::new()),
            catalog_error: AtomicBool::new(false),
            broca_opted_in: AtomicBool::new(true),
            readiness: Mutex::new(None),
            readiness_clock: Mutex::new(None),
            store: store.clone(),
            provider: Mutex::new(provider),
            scope_end: Mutex::new(None),
            custody_controlled: AtomicBool::new(false),
            custody_attempts: custody_tx,
            custody_count: AtomicUsize::new(0),
            content_results: AtomicBool::new(false),
        });
        let catalog = Arc::new(MockCatalog::new());
        catalog.set_op(
            "notes",
            "read",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        let clock = Clock::manual(100);
        let supervisor = Supervisor::new(
            store,
            host.clone(),
            catalog.clone(),
            source.clone(),
            clock.clone(),
            hash,
            ShellDenylist::default(),
        )
        .unwrap();
        Self {
            worker_panic,
            worker_exit,
            supervisor,
            worker: Frames(worker),
            parent,
            attempts,
            custody_attempts,
            host,
            catalog,
            source,
            clock,
            dir,
        }
    }
    fn start(&self, tools: &[(&str, &str, &str, Value)], limits: Limits, deadline: i64) {
        self.start_with(tools, Default::default(), limits, deadline);
    }
    fn start_with(
        &self,
        tools: &[(&str, &str, &str, Value)],
        unavailable: BTreeMap<String, Unavailable>,
        limits: Limits,
        deadline: i64,
    ) {
        let mut new = new_run("r");
        new.catalog_digest = DIGEST;
        new.deadline_ms = deadline;
        self.host
            .store
            .write(|tx| store::insert_run(tx, &new, None))
            .unwrap();
        let Lookup::Found(run) = self.host.store.read(|c| store::lookup(c, "r")).unwrap() else {
            panic!()
        };
        let scope = RegisteredScope {
            selector: serde_json::from_value(
                json!({"owner":{"kind":"reserved","module_id":"core"}, "ref":"scope-r", "epoch":7}),
            )
            .unwrap(),
            targets: tools.iter().map(|t| t.1.to_string()).collect(),
        };
        let tools = tools
            .iter()
            .map(|(name, module, op, schema)| {
                (
                    name.to_string(),
                    Tool {
                        module: module.to_string(),
                        op: op.to_string(),
                        input_schema: compile_input_schema(schema).unwrap(),
                        result_format: self.host.content_results.load(Ordering::SeqCst).then_some(
                            cortexkit_role_tool_provider::result::ResultFormat::ContentV1,
                        ),
                    },
                )
            })
            .collect();
        self.supervisor
            .start(
                *run,
                Start {
                    scope,
                    preset: None,
                    tools,
                    unavailable,
                    limits,
                    wall_deadline_ms: deadline,
                },
            )
            .unwrap();
    }
    fn standard(&self) {
        self.start(
            &[("read", "notes", "read", json!({}))],
            Limits::default(),
            10_000,
        );
        let ParentMessage::Activate(request) = self.parent.recv_timeout(TIMEOUT).unwrap() else {
            panic!()
        };
        assert_eq!(request.profile, Profile::Codemode);
        assert_eq!(request.tools, ["read"]);
        assert_eq!(
            request.prelude_hash,
            PreludeHash::of("parent's codemode prelude")
        );
        assert_eq!(request.budgets.js_time_micros, 10_000_000);
        assert_eq!(request.budgets.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(request.budgets.max_value_bytes, MAX_VALUE_BYTES as u32);
    }
    fn issue(&self, position: u64, name: &str, input: Value) {
        self.worker
            .send(WorkerMessage::HostCall(HostCall {
                position,
                kind: CallKind::Tool { name: name.into() },
                args: JsonText::new(input.to_string()).unwrap(),
            }))
            .unwrap();
    }
    fn delivery(&self) -> Delivery {
        let ParentMessage::Deliver(value) = self.parent.recv_timeout(TIMEOUT).unwrap() else {
            panic!()
        };
        value
    }
    fn attempt(&self) -> Attempt {
        self.attempts.recv_timeout(TIMEOUT).unwrap()
    }
    fn calls(&self) -> Vec<store::CallRecord> {
        self.host.store.read(|c| store::calls(c, "r")).unwrap()
    }
    fn finish(&self, value: &str) {
        self.worker
            .send(WorkerMessage::Finished {
                activation_id: 1,
                result: ActivationResult::Completed {
                    value: JsonText::new(value).unwrap(),
                },
            })
            .unwrap();
    }
    fn terminal(&self) -> Value {
        let until = Instant::now() + TIMEOUT;
        loop {
            let value = self.supervisor.result("r").unwrap().unwrap();
            if value["status"] != "running" {
                return value;
            }
            assert!(Instant::now() < until, "run did not terminate: {value}");
            thread::yield_now();
        }
    }
    fn wait_calls(&self, count: usize) {
        let until = Instant::now() + TIMEOUT;
        while self.calls().len() < count {
            assert!(Instant::now() < until, "calls did not reach {count}");
            thread::yield_now();
        }
    }
    fn assert_order(&self) {
        let until = Instant::now() + TIMEOUT;
        while !self.supervisor.0.active.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < until,
                "supervisor did not stop after termination"
            );
            thread::yield_now();
        }
        let events = self.host.events.lock().unwrap();
        let kill = events.iter().position(|e| e == "kill").unwrap();
        let release = events.iter().position(|e| e == "release").unwrap();
        assert!(kill < release, "kill must precede release: {events:?}");
        assert_eq!(events.iter().filter(|e| *e == "release").count(), 1);
        assert_eq!(
            self.host
                .store
                .read(|c| store::running_counts(c, "agent"))
                .unwrap(),
            (0, 0)
        );
    }
    fn cleanup(&self, shutdown: bool) {
        if thread::panicking() {
            // Release the fake reader and wake the driver only after an assertion
            // has failed; normal teardown must still prove the real kill and join.
            self.worker_exit.store(true, Ordering::SeqCst);
            self.clock.set(i64::MAX);
        }
        let supervisor = self.supervisor.clone();
        teardown(move || {
            if shutdown {
                supervisor.shutdown().unwrap();
            } else {
                let _ = supervisor.cancel("r");
            }
        });
    }
}

#[path = "restart_tests.rs"]
mod restart;

#[path = "model_tests.rs"]
mod models;

#[test]
fn shutdown_joins_a_driver_after_its_active_slot_is_released() {
    let f = Fixture::new();
    let (entered, at_exit) = mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let exiting = release.clone();
    *f.supervisor.0.before_exit.lock().unwrap() = Some(Arc::new(move || {
        entered.send(()).unwrap();
        exiting.wait();
    }));
    f.standard();
    f.finish("1");
    at_exit.recv_timeout(TIMEOUT).unwrap();
    assert!(f.supervisor.0.active.lock().unwrap().is_empty());
    let supervisor = f.supervisor.clone();
    let (done, finished) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        supervisor.shutdown().unwrap();
        done.send(()).unwrap();
    });
    let prematurely_returned = finished.recv_timeout(Duration::from_secs(2)).is_ok();
    release.wait();
    shutdown.join().unwrap();
    assert!(
        !prematurely_returned,
        "shutdown returned while the driver still held its store"
    );
}

#[test]
fn shutdown_refuses_to_start_a_new_driver() {
    let f = Fixture::new();
    f.supervisor.shutdown().unwrap();
    f.host
        .store
        .write(|tx| store::insert_run(tx, &new_run("after-shutdown"), None))
        .unwrap();
    let Lookup::Found(run) = f
        .host
        .store
        .read(|conn| store::lookup(conn, "after-shutdown"))
        .unwrap()
    else {
        panic!("missing run")
    };
    let error = f.supervisor.start(*run, Start {
        scope: RegisteredScope {
            selector: serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"core"}, "ref":"scope-r", "epoch":7})).unwrap(),
            targets: Default::default(),
        },
        preset: None,
        tools: Default::default(), unavailable: Default::default(), limits: Limits::default(), wall_deadline_ms: 10000,
    }).unwrap_err();
    assert_eq!(
        error.to_string(),
        CoreError::Invalid("codemode supervisor is stopped".into()).to_string()
    );
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cleanup(false);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn teardown_preserves_the_original_panic_when_cleanup_also_panics() {
    const CHILD: &str = "BASAL_CODEMODE_UNWIND_CHILD";
    if std::env::var_os(CHILD).is_some() {
        struct FailingCleanup;
        impl Drop for FailingCleanup {
            fn drop(&mut self) {
                teardown(|| panic!("cleanup failure"));
            }
        }
        let original = std::panic::catch_unwind(|| {
            let _cleanup = FailingCleanup;
            panic!("original failure");
        })
        .unwrap_err();
        assert_eq!(original.downcast_ref::<&str>(), Some(&"original failure"));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "codemode::supervisor::tests::teardown_preserves_the_original_panic_when_cleanup_also_panics",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cleanup aborted the child: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn teardown_still_propagates_cleanup_panics_without_an_existing_panic() {
    let failure = std::panic::catch_unwind(|| teardown(|| panic!("cleanup failure"))).unwrap_err();
    assert_eq!(failure.downcast_ref::<&str>(), Some(&"cleanup failure"));
}

fn rejection(delivery: Delivery, tool: &str, outcome: &str, code: &str) {
    assert_eq!(delivery.settlement, Settlement::Rejected);
    let value: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
    assert_eq!(value["tool"], tool);
    assert_eq!(value["outcome"], outcome);
    assert_eq!(value["code"], code);
    assert!(!value["message"].as_str().unwrap().is_empty());
}

#[test]
fn supervisor_registers_routes_records_before_delivery_and_releases() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({"n":1}));
    let sent = f.attempt();
    assert_eq!(
        (sent.flow.as_str(), sent.module.as_str(), sent.op.as_str()),
        ("codemode:r", "notes", "read")
    );
    assert_eq!(sent.key, store::call_key("r", 0));
    assert_eq!(sent.input, json!({"n":1}));
    assert_eq!(sent.thread, "codemode:r:tool");
    let pending = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(pending["status"], "running");
    assert_eq!(
        pending["calls"][0],
        json!({"tool":"read", "outcome":"pending", "waited_on_person":false})
    );
    f.clock.advance(37);
    sent.reply.send(Ok(json!({"answer":42}))).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    assert_eq!(f.calls()[0].duration_ms, Some(37));
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
    f.finish("42");
    let result = f.terminal();
    assert_eq!(result["value"], 42);
    assert_eq!(result["duration_ms"], 37);
    assert!(result.get("error").is_none());
    assert_eq!(result["description"], "count the notes");
    let registrations = f.host.registrations.lock().unwrap();
    assert_eq!(registrations.len(), 2);
    assert_eq!(registrations[0].0, "codemode:r");
    let scope = registrations[0].1.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(&scope.selector).unwrap(),
        json!({"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r","epoch":7})
    );
    assert!(scope.targets.contains("notes"));
    assert_eq!(registrations[1], ("codemode:r".into(), None));
    f.assert_order();
}

#[test]
fn raw_ops_and_all_primitives_are_profile_violations_without_rows_or_sends() {
    let mut kinds = vec![CallKind::Op {
        module: "notes".into(),
        op: "read".into(),
    }];
    for code in 1..=21 {
        if let Some(p) = Primitive::from_code(code) {
            kinds.push(CallKind::Primitive(p));
        }
    }
    for kind in kinds {
        let f = Fixture::new();
        f.standard();
        f.worker
            .send(WorkerMessage::HostCall(HostCall {
                position: 0,
                kind,
                args: JsonText::null(),
            }))
            .unwrap();
        let result = f.terminal();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"]["code"], "profile_violation");
        assert!(f.calls().is_empty());
        assert!(f.attempts.try_recv().is_err());
        f.assert_order();
    }
}

#[test]
fn per_call_checks_are_ordered_and_never_write_intent_before_readiness() {
    let f = Fixture::new();
    f.start(
        &[
            ("strict", "notes", "read", json!({"type":"integer"})),
            ("shell", "aft", "bash", json!({})),
            ("marked", "notes", "shell", json!({})),
            ("absent", "notes", "missing", json!({})),
        ],
        Limits::default(),
        10_000,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.catalog.set_op(
        "notes",
        "shell",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: true,
        },
    );
    f.host.catalog_error.store(true, Ordering::SeqCst);
    for (position, name, input, outcome, code) in [
        (0, "unknown", json!(null), "refused", "unknown_tool"),
        (1, "strict", json!("bad"), "refused", "invalid_input"),
        (
            2,
            "shell",
            json!(null),
            "tool_unavailable",
            "tool_unavailable",
        ),
    ] {
        f.issue(position, name, input);
        rejection(f.delivery(), name, outcome, code);
    }
    assert!(
        f.host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "catalog")
            .count()
            == 1
    );
    f.host.catalog_error.store(false, Ordering::SeqCst);
    for (position, name, code) in [
        (3, "shell", "not_in_catalog"),
        (5, "absent", "not_in_catalog"),
    ] {
        f.issue(position, name, json!(null));
        rejection(f.delivery(), name, "refused", code);
    }
    f.issue(4, "marked", json!(null));
    f.attempt().reply.send(Ok(json!(true))).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    *f.host.readiness.lock().unwrap() = Some(FlowRefusal::new(
        RefusalReason::NoFlowScope,
        "notes",
        "read",
    ));
    f.issue(6, "strict", json!(1));
    rejection(
        f.delivery(),
        "strict",
        "tool_unavailable",
        "tool_unavailable",
    );
    assert_eq!(f.calls().len(), 7);
    assert!(
        f.calls()
            .iter()
            .filter(|c| c.tool != "marked")
            .all(|c| c.intent_at.is_none() && c.duration_ms.is_none())
    );
    assert!(f.attempts.try_recv().is_err());
    f.finish("true");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn invalid_input_names_schema_errors_and_fields_without_sending_or_storing_detail() {
    let f = Fixture::new();
    f.start(
        &[(
            "read",
            "notes",
            "read",
            json!({"type":"object", "properties":{
                "path":{"type":"string"}, "startLine":{"type":"integer"},
                "endLine":{"type":"integer"}, "offset":{"type":"integer"},
                "limit":{"type":"integer"}}, "required":["path"], "additionalProperties":false}),
        )],
        Limits::default(),
        10_000,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "read", json!({"filePath":"Cargo.toml"}));
    let delivery = f.delivery();
    let value: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
    rejection(delivery, "read", "refused", "invalid_input");
    let message = value["message"].as_str().unwrap();
    assert!(message.starts_with("read: invalid input: "), "{message}");
    assert!(
        message.contains("\"path\" is a required property"),
        "{message}"
    );
    assert!(message.contains("'filePath' was unexpected"), "{message}");
    assert!(
        message.contains("read takes: endLine, limit, offset, path (required), startLine."),
        "{message}"
    );
    assert_eq!(value["detail"]["tool"], "read");
    assert_eq!(
        value["detail"]["properties"],
        json!(["endLine", "limit", "offset", "path", "startLine"])
    );
    assert_eq!(value["detail"]["required"], json!(["path"]));
    let errors = value["detail"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().all(|error| error["path"] == ""));
    assert!(
        errors
            .iter()
            .any(|error| error["message"].as_str().unwrap().contains("\"path\""))
    );
    assert!(
        errors
            .iter()
            .any(|error| error["message"].as_str().unwrap().contains("filePath"))
    );
    assert!(f.attempts.try_recv().is_err());
    assert!(!f.host.events.lock().unwrap().iter().any(|e| e == "catalog"));
    let calls = f.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, Outcome::Refused);
    assert_eq!(calls[0].code.as_deref(), Some("invalid_input"));
    assert!(calls[0].intent_at.is_none());
    f.worker
        .send(WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::ScriptHostRejection {
                position: 0,
                message: "HostError: invalid_input".into(),
            }),
        })
        .unwrap();
    let result = f.terminal();
    assert_eq!(result["error"]["code"], "invalid_input");
    assert_eq!(
        result["error"]["message"],
        format!("read (call 1) failed: {message}")
    );
    assert!(result["calls"][0].get("message").is_none());
    assert!(result["calls"][0].get("detail").is_none());
}

#[test]
fn invalid_input_detail_and_message_are_bounded_for_many_errors_and_unknown_properties() {
    let properties: serde_json::Map<String, Value> = (0..50)
        .map(|i| (format!("field_{i:02}_\n雪"), json!({"type":"integer"})))
        .collect();
    let required: Vec<_> = properties.keys().cloned().collect();
    let schema = compile_input_schema(&json!({"type":"object", "properties":properties,
        "required":required, "additionalProperties":false}))
    .unwrap();
    let unknown: serde_json::Map<String, Value> = (0..50)
        .map(|i| (format!("unknown_{i}_{}", "雪\n".repeat(100)), json!(null)))
        .collect();
    let wrong_types: serde_json::Map<String, Value> = properties
        .keys()
        .map(|name| (name.clone(), json!("wrong type")))
        .collect();
    for input in [json!(unknown), json!(wrong_types)] {
        let mapped = invalid_input(&"r".repeat(128), &schema, Some(&input));
        let detail = mapped.detail.unwrap();
        assert!(mapped.message.len() <= 1024, "{}", mapped.message.len());
        assert!(
            detail.to_string().len() <= 1024,
            "{}",
            detail.to_string().len()
        );
        let errors = detail["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 5);
        for error in errors {
            assert!(error["path"].to_string().len() <= 34);
            assert!(error["message"].to_string().len() <= 82);
        }
        assert!(detail["properties"].as_array().unwrap().len() < 50);
        assert!(detail["required"].as_array().unwrap().len() < 50);
    }
    let schema =
        compile_input_schema(&json!({"properties":{"path":{}}, "additionalProperties":false}))
            .unwrap();
    let detail = invalid_input("read", &schema, Some(&json!(unknown)))
        .detail
        .unwrap();
    assert!(detail.to_string().len() <= 1024);
    assert_eq!(detail["errors"].as_array().unwrap().len(), 1);
    assert!(
        detail["errors"][0]["message"]
            .as_str()
            .unwrap()
            .ends_with('…')
    );
}

#[test]
fn invalid_input_lists_only_own_top_level_properties_and_required() {
    let schema = compile_input_schema(&json!({
        "$defs":{"args":{"properties":{"referenced":{}}, "required":["referenced"]}},
        "$ref":"#/$defs/args",
        "properties":{"own":{"properties":{"nested":{}}}}, "required":["own"]
    }))
    .unwrap();
    let mapped = invalid_input("read", &schema, Some(&json!({})));
    let detail = mapped.detail.unwrap();
    assert_eq!(detail["properties"], json!(["own"]));
    assert_eq!(detail["required"], json!(["own"]));
    assert!(mapped.message.ends_with("read takes: own (required)."));
}

#[test]
fn invalid_input_parse_failure_names_tool_without_sending() {
    let f = Fixture::new();
    f.standard();
    f.worker
        .send(WorkerMessage::HostCall(HostCall {
            position: 0,
            kind: CallKind::Tool {
                name: "read".into(),
            },
            args: JsonText::new("{not JSON").unwrap(),
        }))
        .unwrap();
    let delivery = f.delivery();
    let value: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
    rejection(delivery, "read", "refused", "invalid_input");
    assert_eq!(value["message"], "read: arguments are not valid JSON");
    assert_eq!(
        value["detail"],
        json!({"tool":"read", "errors":[{"path":"", "message":"arguments are not valid JSON"}], "properties":[], "required":[]})
    );
    assert!(f.attempts.try_recv().is_err());
    assert_eq!(f.calls()[0].outcome, Outcome::Refused);
    assert!(f.calls()[0].intent_at.is_none());
}

#[test]
fn valid_schema_input_dispatches_and_delivers_unchanged() {
    let f = Fixture::new();
    f.start(&[("read", "notes", "read", json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"], "additionalProperties":false}))], Limits::default(), 10_000);
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "read", json!({"path":"Cargo.toml"}));
    f.attempt()
        .reply
        .send(Ok(json!({"text":"contents"})))
        .unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(
        serde_json::from_str::<Value>(delivery.value.as_str()).unwrap(),
        json!({"text":"contents"})
    );
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
    f.finish("true");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn two_hundred_refusals_count_and_the_next_invocation_has_no_row() {
    let f = Fixture::new();
    f.standard();
    for n in 0..200 {
        f.issue(n, "absent", json!(null));
        rejection(f.delivery(), "absent", "refused", "unknown_tool");
    }
    assert_eq!(f.calls().len(), 200);
    f.issue(200, "absent", json!(null));
    let result = f.terminal();
    assert_eq!(result["status"], "budget_exhausted:tool_calls");
    assert_eq!(result["error"]["code"], "budget_exhausted:tool_calls");
    assert_eq!(f.calls().len(), 200);
    assert!(f.attempts.try_recv().is_err());
}

#[test]
fn eight_in_flight_queue_fifo_and_completion_drains_unawaited_calls() {
    let f = Fixture::new();
    f.standard();
    let mut held = Vec::new();
    for n in 0..8 {
        f.issue(n, "read", json!(n));
        held.push(f.attempt());
    }
    for n in 8..11 {
        f.issue(n, "read", json!(n));
    }
    f.wait_calls(11);
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running"
    );
    assert!(f.attempts.try_recv().is_err());
    assert!(f.calls()[8..].iter().all(|c| c.intent_at.is_none()));
    f.finish("7");
    for n in 8..11 {
        held.pop().unwrap().reply.send(Ok(json!(true))).unwrap();
        f.delivery();
        let next = f.attempt();
        assert_eq!(next.input, json!(n));
        held.push(next);
    }
    for sent in held {
        sent.reply.send(Ok(json!(true))).unwrap();
        f.delivery();
    }
    let result = f.terminal();
    assert_eq!(result["status"], "completed");
    assert_eq!(f.calls().len(), 11);
    assert!(f.calls().iter().all(|c| c.outcome == Outcome::Ok));
    f.assert_order();
}

#[test]
fn queue_byte_cap_is_inclusive_and_overflow_has_no_row_or_intent() {
    let f = Fixture::new();
    f.standard();
    let mut held = Vec::new();
    for n in 0..8 {
        f.issue(n, "read", json!(n));
        held.push(f.attempt());
    }
    let input = Value::String("x".repeat(MAX_VALUE_BYTES - 2));
    assert_eq!(input.to_string().len(), MAX_VALUE_BYTES);
    for n in 8..12 {
        f.issue(n, "read", input.clone());
    }
    f.wait_calls(12);
    f.issue(12, "read", json!(0));
    rejection(f.delivery(), "read", "refused", "queue_full");
    assert_eq!(f.calls().len(), 12);
    assert!(f.calls()[8..].iter().all(|c| c.intent_at.is_none()));
    assert!(f.attempts.try_recv().is_err());
    let result = f.supervisor.cancel("r").unwrap().unwrap();
    assert_eq!(result["status"], "cancelled");
    drop(held);
}

#[test]
fn input_and_answer_at_one_mib_pass_but_larger_answer_rejects() {
    let f = Fixture::new();
    f.standard();
    let cap = Value::String("x".repeat(MAX_VALUE_BYTES - 2));
    f.issue(0, "read", cap.clone());
    let sent = f.attempt();
    assert_eq!(sent.input.to_string().len(), MAX_VALUE_BYTES);
    sent.reply.send(Ok(cap)).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    f.issue(1, "read", json!(null));
    f.attempt()
        .reply
        .send(Ok(Value::String("x".repeat(MAX_VALUE_BYTES - 1))))
        .unwrap();
    rejection(f.delivery(), "read", "error", "value_too_large");
    assert_eq!(f.calls()[1].outcome, Outcome::Error);
    f.finish("true");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn every_non_scope_wire_error_is_recorded_then_rejected_without_ending_run() {
    let f = Fixture::new();
    f.standard();
    let mut cases = vec![
        (
            WireError::NeverSent("offline".into()),
            "tool_unavailable",
            "tool_unavailable",
        ),
        (
            WireError::Unknown("lost".into()),
            "outcome_unknown",
            "connection_lost",
        ),
        (
            WireError::TimedOut("late".into()),
            "outcome_unknown",
            "reply_timeout",
        ),
        (
            WireError::Unreadable("bad".into()),
            "outcome_unknown",
            "reply_unreadable",
        ),
        (
            WireError::Refused {
                code: "denied".into(),
                message: "no".into(),
            },
            "denied",
            "denied",
        ),
        (
            WireError::RefusedDetails {
                code: "detail_denied".into(),
                message: "no".into(),
                detail: json!({}),
            },
            "error",
            "detail_denied",
        ),
    ];
    use RefusalReason::*;
    for reason in [
        ScopeNotCarrier,
        ScopeEpochRequired,
        ScopeUnsupported,
        TargetFlowUnsupported,
        ResourceBusy,
        ModuleGrantAbsent,
        AgentGrantAbsent,
        NoFlowScope,
        AgentRetired,
        FlowScopeRequired,
        ConsentUnavailable,
    ] {
        let (outcome, code) = if reason == ConsentUnavailable {
            ("refused", "consent_unavailable")
        } else {
            ("tool_unavailable", "tool_unavailable")
        };
        cases.push((
            WireError::Typed(FlowRefusal::new(reason, "notes", "read")),
            outcome,
            code,
        ));
    }
    for (n, (error, outcome, code)) in cases.into_iter().enumerate() {
        f.issue(n as u64, "read", json!(null));
        f.attempt().reply.send(Err(error)).unwrap();
        rejection(f.delivery(), "read", outcome, code);
        let call = &f.calls()[n];
        assert_eq!(call.outcome.as_str(), outcome);
        assert_eq!(call.code.as_deref(), Some(code));
        assert_eq!(
            f.supervisor.result("r").unwrap().unwrap()["status"],
            "running"
        );
    }
    f.finish("1");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn each_scope_loss_records_the_refusal_preserves_earlier_ok_and_interrupts() {
    // ScopeChanged is not here: it leaves the run scope live, so it ends only
    // its call (see the next test).
    for reason in [
        RefusalReason::ScopeEnded,
        RefusalReason::ScopeNotLive,
        RefusalReason::ScopeNotSynced,
    ] {
        let f = Fixture::new();
        f.standard();
        f.issue(0, "read", json!(null));
        f.attempt().reply.send(Ok(json!(1))).unwrap();
        f.delivery();
        f.issue(1, "read", json!(null));
        f.attempt()
            .reply
            .send(Err(WireError::Typed(FlowRefusal::new(
                reason, "notes", "read",
            ))))
            .unwrap();
        let result = f.terminal();
        assert_eq!(result["status"], "interrupted");
        assert_eq!(result["error"]["code"], reason.as_str());
        assert_eq!(f.calls()[0].outcome, Outcome::Ok);
        assert_eq!(f.calls()[1].outcome, Outcome::Refused);
        assert_eq!(f.calls()[1].code.as_deref(), Some(reason.as_str()));
        assert!(f.parent.try_recv().is_err());
        f.assert_order();
    }
}

/// A `scope_changed` that reaches the supervisor came after the transport's
/// one re-open, or from the provider after it received the request. Either way
/// the call is refused, it is not dispatched again, and the run goes on.
#[test]
fn scope_changed_refuses_only_its_call_and_the_run_continues() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!(null));
    f.attempt()
        .reply
        .send(Err(WireError::Typed(FlowRefusal::new(
            RefusalReason::ScopeChanged,
            "notes",
            "read",
        ))))
        .unwrap();
    rejection(f.delivery(), "read", "refused", "scope_changed");
    assert!(
        f.attempts.try_recv().is_err(),
        "the call is not dispatched again"
    );
    assert_eq!(f.calls()[0].outcome, Outcome::Refused);
    assert_eq!(f.calls()[0].code.as_deref(), Some("scope_changed"));
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running"
    );
    f.issue(1, "read", json!(null));
    f.attempt().reply.send(Ok(json!(1))).unwrap();
    f.delivery();
    assert_eq!(f.calls()[1].outcome, Outcome::Ok);
    f.finish("1");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn closed_scope_without_another_dispatch_still_completes() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!(null));
    f.attempt().reply.send(Ok(json!(1))).unwrap();
    f.delivery();
    *f.host.readiness.lock().unwrap() =
        Some(FlowRefusal::new(RefusalReason::ScopeEnded, "notes", "read"));
    f.finish("2");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn cancel_settles_sent_and_queued_calls_never_sends_queue_or_records_late_answer() {
    let f = Fixture::new();
    f.standard();
    let mut held = Vec::new();
    for n in 0..8 {
        f.issue(n, "read", json!(n));
        held.push(f.attempt());
    }
    f.issue(8, "read", json!(8));
    f.wait_calls(9);
    f.clock.advance(19);
    let result = f.supervisor.cancel("r").unwrap().unwrap();
    assert_eq!(result["status"], "cancelled");
    assert!(result.get("error").is_none());
    for call in &f.calls()[..8] {
        assert_eq!(call.outcome, Outcome::OutcomeUnknown);
        assert_eq!(call.code.as_deref(), Some("no_outcome"));
        assert_eq!(call.duration_ms, Some(19));
    }
    assert_eq!(f.calls()[8].outcome, Outcome::Cancelled);
    assert_eq!(f.calls()[8].duration_ms, None);
    let before = f.calls();
    for sent in held {
        sent.reply.send(Ok(json!("late"))).unwrap();
    }
    assert_eq!(f.calls(), before);
    assert!(f.attempts.try_recv().is_err());
    assert_eq!(f.supervisor.cancel("r").unwrap(), Some(result));
    assert_eq!(f.supervisor.cancel("unknown").unwrap(), None);
    f.assert_order();
}

#[test]
fn output_cap_is_inclusive_truncates_whole_lines_once_and_survives_wall_kill() {
    let f = Fixture::new();
    f.standard();
    let kept = format!("{}\n", "x".repeat(65_535));
    f.worker
        .send(WorkerMessage::Console { line: kept.clone() })
        .unwrap();
    f.worker
        .send(WorkerMessage::Console {
            line: "drop\n".into(),
        })
        .unwrap();
    f.worker
        .send(WorkerMessage::Console {
            line: "also drop\n".into(),
        })
        .unwrap();
    f.issue(0, "read", json!(null));
    let held = f.attempt();
    let running = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(running["output"], kept);
    assert_eq!(running["warnings"].as_array().unwrap().len(), 1);
    f.clock.set(10_000);
    let result = f.terminal();
    assert_eq!(result["output"], kept);
    assert_eq!(result["warnings"], running["warnings"]);
    assert_eq!(result["status"], "budget_exhausted:wall");
    drop(held);
    f.assert_order();
}

#[test]
fn manual_clock_fires_omitted_wall_at_deadline_and_wall_one_at_admission_plus_one() {
    for deadline in [1_000, 101] {
        let f = Fixture::new();
        f.start(
            &[("read", "notes", "read", json!({}))],
            Limits::default(),
            deadline,
        );
        f.parent.recv_timeout(TIMEOUT).unwrap();
        f.clock.set(deadline - 1);
        f.issue(0, "read", json!(null));
        let held = f.attempt();
        assert_eq!(
            f.supervisor.result("r").unwrap().unwrap()["status"],
            "running"
        );
        f.clock.set(deadline);
        let result = f.terminal();
        assert_eq!(result["status"], "budget_exhausted:wall");
        assert_eq!(result["duration_ms"], deadline - 100);
        drop(held);
        f.assert_order();
    }
}

#[test]
fn return_boundary_non_json_and_stalled_have_exact_terminal_codes() {
    for (value, status, code) in [
        (format!("\"{}\"", "x".repeat(16_382)), "completed", None),
        (
            format!("\"{}\"", "x".repeat(16_383)),
            "failed",
            Some("result_too_large"),
        ),
        ("undefined".into(), "failed", Some("result_not_json")),
    ] {
        let f = Fixture::new();
        f.standard();
        f.finish(&value);
        let result = f.terminal();
        assert_eq!(result["status"], status);
        if let Some(code) = code {
            assert_eq!(result["error"]["code"], code);
        } else {
            assert_eq!(result["value"].as_str().unwrap().len(), 16_382);
        }
        if code == Some("result_too_large") {
            assert!(
                result["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("16385")
            );
        }
        f.assert_order();
    }
    let f = Fixture::new();
    f.standard();
    f.worker
        .send(WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Stalled,
        })
        .unwrap();
    assert_eq!(f.terminal()["error"]["code"], "stalled");
    f.assert_order();
}

#[test]
fn spawn_failure_and_restart_use_ordered_termination_without_resending() {
    let f = Fixture::new();
    f.source.0.lock().unwrap().take();
    f.start(&[], Limits::default(), 10_000);
    let result = f.terminal();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"]["code"], "worker_lost");
    assert_eq!(*f.host.events.lock().unwrap(), ["register", "release"]);
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| {
            store::insert_run(tx, &new_run("r"), None)?;
            store::insert_call(tx, "r", 0, "read", 4, CallStart::Intent { at: 100 })?;
            store::record_outcome(tx, "r", 0, Outcome::Ok, None, 100)?;
            store::insert_call(tx, "r", 1, "read", 4, CallStart::Intent { at: 100 })?;
            store::insert_call(tx, "r", 2, "read", 4, CallStart::Queued)?;
            Ok(())
        })
        .unwrap();
    f.clock.advance(9);
    f.supervisor.recover().unwrap();
    let result = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["error"]["code"], "basal_restarted");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
    assert_eq!(f.calls()[1].code.as_deref(), Some("no_outcome"));
    assert_eq!(f.calls()[2].outcome, Outcome::Cancelled);
    assert!(f.attempts.try_recv().is_err());
}

#[test]
fn activation_failure_table_and_engine_budgets_are_exhaustive() {
    let hash = PreludeHash::of("hash");
    let cases = [
        (
            Failure::ProfileViolation {
                kind: CallKind::Primitive(Primitive::Sh),
            },
            "profile_violation",
        ),
        (
            Failure::EngineMismatch {
                expected: hash,
                actual: PreludeHash::of("other"),
            },
            "engine_mismatch",
        ),
        (
            Failure::ArgumentsTooLarge {
                bytes: 1_048_577,
                cap: 1_048_576,
            },
            "arguments_too_large",
        ),
        (
            Failure::ResultTooLarge {
                bytes: 1_048_577,
                cap: 1_048_576,
            },
            "result_too_large",
        ),
        (
            Failure::ResultNotSerializable {
                detail: "undefined".into(),
            },
            "result_not_json",
        ),
        (
            Failure::Script {
                message: "throw".into(),
            },
            "script",
        ),
        (
            Failure::ScriptHostRejection {
                position: 0,
                message: "reject".into(),
            },
            "script",
        ),
        (
            Failure::InvalidRequest {
                detail: "bad".into(),
            },
            "engine_error",
        ),
        (
            Failure::InvalidHostValue {
                position: 0,
                detail: "bad".into(),
            },
            "engine_error",
        ),
        (
            Failure::HostLink {
                detail: "bad".into(),
            },
            "engine_error",
        ),
        (
            Failure::Engine {
                detail: "bad".into(),
            },
            "engine_error",
        ),
        (
            Failure::Nondeterminism(basal_proto::Nondeterminism::UnconsumedCall { position: 0 }),
            "engine_error",
        ),
    ];
    for (failure, code) in cases {
        let f = Fixture::new();
        f.standard();
        f.worker
            .send(WorkerMessage::Finished {
                activation_id: 1,
                result: ActivationResult::Failed(failure),
            })
            .unwrap();
        assert_eq!(f.terminal()["error"]["code"], code);
        f.assert_order();
    }
    for (budget, status) in [
        (BudgetKind::JsTime, "budget_exhausted:js_cpu"),
        (BudgetKind::Memory, "budget_exhausted:memory"),
        (BudgetKind::Stack, "budget_exhausted:stack"),
    ] {
        let f = Fixture::new();
        f.standard();
        f.worker
            .send(WorkerMessage::Finished {
                activation_id: 1,
                result: ActivationResult::BudgetExhausted(budget),
            })
            .unwrap();
        let result = f.terminal();
        assert_eq!(result["status"], status);
        assert_eq!(result["error"]["code"], status);
        f.assert_order();
    }
}

#[test]
fn uncaught_tool_rejection_ends_the_run_with_the_provider_code_as_plain_text() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({}));
    f.attempt()
        .reply
        .send(Err(WireError::RefusedDetails {
            code: "no_session".into(),
            message: "No session.\nCall browser.launch, then retry the browser action.".into(),
            detail: json!({"reason":"no browser session is open"}),
        }))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Rejected);
    // The worker reports the rejection it delivered, by position, with the
    // JavaScript text of the error the program left uncaught.
    f.worker
        .send(WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::ScriptHostRejection {
                position: 0,
                message: "HostError: No session.\n    at deliver (codemode_prelude.js:77:5)".into(),
            }),
        })
        .unwrap();
    let result = f.terminal();
    assert_eq!(result["status"], "failed");
    assert_eq!(
        result["error"],
        json!({"code":"no_session","message":"read (call 1) failed: No session. Call browser.launch, then retry the browser action. Reason: no browser session is open."})
    );
    assert_eq!(result["calls"][0]["code"], "no_session");
}

#[test]
fn script_error_is_the_plain_javascript_message() {
    let f = Fixture::new();
    f.standard();
    f.worker
        .send(WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::Script {
                message: "TypeError: tools.work is not available in this codemode run.".into(),
            }),
        })
        .unwrap();
    assert_eq!(
        f.terminal()["error"],
        json!({"code":"script","message":"TypeError: tools.work is not available in this codemode run."})
    );
}

#[test]
fn intent_to_entry_delay_is_not_provider_time_and_unentered_intents_have_no_duration() {
    let f = Fixture::new();
    f.standard();
    let clock = f.clock.clone();
    *f.supervisor.0.before_entry.lock().unwrap() = Some(Arc::new(move || {
        clock.advance(41);
    }));
    f.issue(0, "read", json!(null));
    let sent = f.attempt();
    let call = &f.calls()[0];
    assert_eq!(call.intent_at, Some(100));
    assert_eq!(call.entered_at, Some(141));
    assert_eq!(call.duration_ms, None);
    f.clock.advance(23);
    sent.reply.send(Ok(json!(1))).unwrap();
    f.delivery();
    assert_eq!(f.calls()[0].duration_ms, Some(23));
    f.finish("1");
    assert_eq!(f.terminal()["duration_ms"], 64);
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| {
            store::insert_run(tx, &new_run("r"), None)?;
            store::insert_call(tx, "r", 0, "read", 4, CallStart::Prepared { at: 100 })?;
            assert!(store::enter_call(tx, "r", 0, 130)?);
            assert!(!store::enter_call(tx, "r", 0, 131)?);
            store::insert_call(tx, "r", 1, "read", 4, CallStart::Prepared { at: 100 })?;
            store::insert_call(tx, "r", 2, "read", 4, CallStart::Queued)?;
            assert!(store::prepare_queued(tx, "r", 2, 120)?);
            assert!(!store::prepare_queued(tx, "r", 2, 121)?);
            Ok(())
        })
        .unwrap();
    f.clock.set(160);
    f.supervisor.recover().unwrap();
    assert_eq!(f.calls()[0].duration_ms, Some(30));
    assert_eq!(f.calls()[1].outcome, Outcome::OutcomeUnknown);
    assert_eq!(f.calls()[1].duration_ms, None);
    assert_eq!(f.calls()[2].duration_ms, None);
    f.host
        .store
        .write(|tx| {
            assert!(!store::enter_call(tx, "r", 1, 170)?);
            assert!(!store::prepare_queued(tx, "r", 1, 170)?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn entry_time_migration_preserves_old_calls_and_reinstalls_guards() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    for migration in &crate::schema::MIGRATIONS[..17] {
        conn.execute_batch(migration.statements).unwrap();
    }
    let tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO codemode_runs (run_id,agent_id,program,catalog,catalog_digest,limits,scope,deadline_ms,status,admitted_at)
        VALUES ('r','agent','return 1','[]',?1,'{}','{}',1000,'running',100)",[DIGEST]).unwrap();
    tx.execute("INSERT INTO codemode_calls (run_id,position,tool,idempotency_key,input_bytes,intent_at,outcome,duration_ms)
        VALUES ('r',0,'read','legacy',4,100,'ok',13)",[]).unwrap();
    tx.commit().unwrap();
    conn.execute_batch(crate::schema::MIGRATIONS[17].statements)
        .unwrap();
    let calls = store::calls(&conn, "r").unwrap();
    assert_eq!(calls[0].outcome, Outcome::Ok);
    assert_eq!(calls[0].entered_at, Some(100));
    assert_eq!(calls[0].duration_ms, Some(13));
    assert!(
        conn.execute(
            "UPDATE codemode_calls SET outcome='error',code='bad' WHERE run_id='r'",
            []
        )
        .is_err()
    );
    let tx = conn.transaction().unwrap();
    store::insert_call(&tx, "r", 1, "read", 4, CallStart::Prepared { at: 100 }).unwrap();
    assert!(tx.execute("UPDATE codemode_runs SET status='cancelled',ended_at=100,duration_ms=0 WHERE run_id='r'",[]).is_err());
    store::settle_pending_calls(&tx, "r", 110).unwrap();
    assert_eq!(store::calls(&tx, "r").unwrap()[1].duration_ms, None);
    tx.commit().unwrap();
}

#[test]
fn admission_limits_drive_the_supervisor_deadline() {
    use super::super::admission::{self, Admission, Platform};
    use basal_host::mock::MockHost;
    for (limits, deadline) in [(json!({}), 1000), (json!({"wall_ms":1}), 101)] {
        let f = Fixture::new();
        let host = MockHost::new();
        let owner = subc_protocol::Principal::Reserved {
            module_id: "core".into(),
        };
        let description = serde_json::from_value(json!({"status":"live","scope_epoch":7,
            "daemon_incarnation":"daemon:test","owner_synced":true,"owner_configured":true,
            "scope":{"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r","scope_epoch":7,"kind":"head",
                "attributes":{"agent_id":"agent","run_id":"r"},"owner_authorized":true}})).unwrap();
        host.set_scope_description(owner, "scope-r", Ok(description));
        let request = json!({"run_id":"r","agent_id":"agent","program":"return 1;",
            "catalog":[{"name":"read","module":"notes","op":"read","input_schema":{}}],
            "limits":limits,"scope":{"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r","epoch":7},"deadline_ms":1000});
        let Admission::Admitted {
            run,
            start: Some(start),
        } = admission::admit(&f.host.store, &host, &f.clock, Platform::Unix, &request).unwrap()
        else {
            panic!()
        };
        assert_eq!(start.wall_deadline_ms, deadline);
        f.supervisor.start(*run, *start).unwrap();
        f.parent.recv_timeout(TIMEOUT).unwrap();
        f.clock.set(deadline);
        let result = f.terminal();
        assert_eq!(result["status"], "budget_exhausted:wall");
        assert_eq!(result["duration_ms"], deadline - 100);
        f.assert_order();
    }
}

#[test]
fn live_supervisors_cannot_be_recovered_and_return_unknown_ids_as_absent() {
    let f = Fixture::new();
    f.standard();
    assert!(f.supervisor.recover().is_err());
    assert_eq!(f.supervisor.result("unknown").unwrap(), None);
    f.finish("null");
    f.terminal();
}

#[test]
fn configured_tool_budget_is_enforced_before_the_unknown_tool_check() {
    let f = Fixture::new();
    f.start(
        &[],
        Limits {
            tool_calls: 1,
            ..Limits::default()
        },
        10_000,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "absent", json!(null));
    rejection(f.delivery(), "absent", "refused", "unknown_tool");
    f.issue(1, "absent", json!(null));
    assert_eq!(f.terminal()["status"], "budget_exhausted:tool_calls");
    assert_eq!(f.calls().len(), 1);
    assert!(f.attempts.try_recv().is_err());
}

#[test]
fn a_tool_dropped_at_admission_rejects_typed_and_named_while_others_still_run() {
    let f = Fixture::new();
    let unavailable = [(
        "apply_patch".to_owned(),
        Unavailable {
            module: "aft".into(),
            reason: "catalog_unreadable",
            message: "codemode can't use apply_patch from provider aft: unreadable".into(),
        },
    )]
    .into_iter()
    .collect();
    f.start_with(
        &[("read", "notes", "read", json!({}))],
        unavailable,
        Limits::default(),
        10_000,
    );
    let ParentMessage::Activate(request) = f.parent.recv_timeout(TIMEOUT).unwrap() else {
        panic!()
    };
    // The program sees the dropped name, so calling it reaches the parent.
    assert_eq!(request.tools, ["read", "apply_patch"]);
    f.issue(0, "apply_patch", json!({}));
    let delivery = f.delivery();
    let value: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
    rejection(
        delivery,
        "apply_patch",
        "tool_unavailable",
        "tool_unavailable",
    );
    assert_eq!(
        value["detail"],
        json!({"reason":"catalog_unreadable","module":"aft"})
    );
    assert!(value["message"].as_str().unwrap().contains("provider aft"));
    assert!(f.attempts.try_recv().is_err(), "nothing was sent");
    let call = &f.calls()[0];
    assert_eq!(call.code.as_deref(), Some("tool_unavailable"));
    f.issue(1, "read", json!({"n":1}));
    f.attempt().reply.send(Ok(json!(true))).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    f.finish("null");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn queued_readiness_refusal_has_no_intent_and_wall_expiring_during_readiness_never_sends() {
    let f = Fixture::new();
    f.standard();
    let mut held = Vec::new();
    for n in 0..8 {
        f.issue(n, "read", json!(n));
        held.push(f.attempt());
    }
    f.issue(8, "read", json!(8));
    f.wait_calls(9);
    *f.host.readiness.lock().unwrap() = Some(FlowRefusal::new(
        RefusalReason::NoFlowScope,
        "notes",
        "read",
    ));
    held.pop().unwrap().reply.send(Ok(json!(true))).unwrap();
    f.delivery();
    rejection(f.delivery(), "read", "tool_unavailable", "tool_unavailable");
    let call = &f.calls()[8];
    assert_eq!(call.intent_at, None);
    assert_eq!(call.duration_ms, None);
    assert!(f.attempts.try_recv().is_err());
    f.supervisor.cancel("r").unwrap();
    drop(held);
    let f = Fixture::new();
    f.standard();
    *f.host.readiness_clock.lock().unwrap() = Some((f.clock.clone(), 10_000));
    f.issue(0, "read", json!(null));
    assert_eq!(f.terminal()["status"], "budget_exhausted:wall");
    assert!(f.calls().is_empty());
    assert!(f.attempts.try_recv().is_err());
}

#[test]
fn an_output_overflow_drops_even_later_lines_that_would_fit() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits {
            output_bytes: 10,
            ..Limits::default()
        },
        10_000,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    for line in ["a\n", "0123456789\n", "b\n"] {
        f.worker
            .send(WorkerMessage::Console { line: line.into() })
            .unwrap();
    }
    f.finish("null");
    let result = f.terminal();
    assert_eq!(result["output"], "a\n");
    assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(result["warnings"][0]["code"], "output_truncated");
}

#[test]
fn calls_after_a_completion_frame_are_not_recorded_and_unowned_exit_is_worker_lost() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!(null));
    let held = f.attempt();
    f.finish("1");
    f.issue(1, "read", json!(null));
    assert_eq!(f.terminal()["error"]["code"], "engine_error");
    assert_eq!(f.calls().len(), 1);
    assert_eq!(f.calls()[0].code.as_deref(), Some("no_outcome"));
    drop(held);
    f.assert_order();
    let f = Fixture::new();
    f.standard();
    f.worker
        .send(WorkerMessage::Refused(
            basal_proto::Refusal::UnknownPosition { position: 99 },
        ))
        .unwrap();
    assert_eq!(f.terminal()["error"]["code"], "engine_error");
    f.assert_order();
    let f = Fixture::new();
    f.standard();
    f.worker_exit.store(true, Ordering::SeqCst);
    assert_eq!(f.terminal()["error"]["code"], "worker_lost");
    f.assert_order();
}

#[test]
fn panicking_driver_terminates_and_start_rejects_duplicate_or_terminal_runs() {
    let f = Fixture::new();
    f.standard();
    let make_start = || Start {
        scope: RegisteredScope {
            selector: serde_json::from_value(
                json!({"owner":{"kind":"reserved","module_id":"core"},"ref":"scope-r","epoch":7}),
            )
            .unwrap(),
            targets: Default::default(),
        },
        preset: None,
        tools: Default::default(),
        unavailable: Default::default(),
        limits: Limits::default(),
        wall_deadline_ms: 10_000,
    };
    let Lookup::Found(run) = f.host.store.read(|c| store::lookup(c, "r")).unwrap() else {
        panic!()
    };
    assert!(f.supervisor.start(*run, make_start()).is_err());
    f.worker_panic.store(true, Ordering::SeqCst);
    assert_eq!(f.terminal()["error"]["code"], "engine_error");
    f.assert_order();
    let until = Instant::now() + TIMEOUT;
    while !f.supervisor.0.active.lock().unwrap().is_empty() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    let Lookup::Found(run) = f.host.store.read(|c| store::lookup(c, "r")).unwrap() else {
        panic!()
    };
    assert!(f.supervisor.start(*run, make_start()).is_err());
}

/// A wall deadline so far ahead in real time that a run reaching it inside a
/// test can only have been ended by a move of the manual clock.
const FAR_DEADLINE: i64 = 1_000_000_000;

impl Fixture {
    /// Starts a run with `FAR_DEADLINE`, issues one call and returns the held
    /// provider attempt.
    fn held_far_from_its_deadline(&self) -> Attempt {
        self.start(
            &[("read", "notes", "read", json!({}))],
            Limits::default(),
            FAR_DEADLINE,
        );
        self.parent.recv_timeout(TIMEOUT).unwrap();
        self.issue(0, "read", json!(null));
        self.attempt()
    }

    fn wakes(&self) -> usize {
        self.supervisor.0.wakes.load(Ordering::SeqCst)
    }
}

#[test]
fn a_run_waiting_on_a_held_tool_sleeps_until_the_answer_wakes_it() {
    let f = Fixture::new();
    let held = f.held_far_from_its_deadline();
    // The worker's frames reach the driver one at a time, so once this line
    // is stored the driver has finished with the call and counted the wake
    // that brought the line. Nothing else is due before the answer.
    f.worker
        .send(WorkerMessage::Console {
            line: "idle\n".into(),
        })
        .unwrap();
    let until = Instant::now() + TIMEOUT;
    while f.supervisor.result("r").unwrap().unwrap()["output"] != "idle\n" {
        assert!(Instant::now() < until, "the console line was not stored");
        thread::yield_now();
    }
    let idle = f.wakes();
    // Give a driver that polls on a timer time to wake. The assertion is on
    // the wake count, not on how long anything took.
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        f.wakes(),
        idle,
        "the driver woke while the provider was held"
    );
    held.reply.send(Ok(json!(1))).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    assert_eq!(
        f.clock.now_ms(),
        100,
        "the answer was delivered without moving the clock"
    );
    assert_eq!(f.wakes(), idle + 1, "one answer is one wake");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
}

#[test]
fn a_manual_clock_jump_past_the_deadline_ends_a_sleeping_run() {
    let f = Fixture::new();
    let held = f.held_far_from_its_deadline();
    f.clock.set(FAR_DEADLINE);
    let result = f.terminal();
    assert_eq!(result["status"], "budget_exhausted:wall");
    assert_eq!(result["duration_ms"], FAR_DEADLINE - 100);
    assert_eq!(f.calls()[0].code.as_deref(), Some("no_outcome"));
    drop(held);
    f.assert_order();
}

#[test]
fn shutdown_joins_the_worker_reader_of_a_cancelled_run() {
    let f = Fixture::new();
    let (entered, at_exit) = mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let exiting = release.clone();
    *f.supervisor.0.before_reader_exit.lock().unwrap() = Some(Arc::new(move || {
        entered.send(()).unwrap();
        exiting.wait();
    }));
    let held = f.held_far_from_its_deadline();
    let supervisor = f.supervisor.clone();
    let (done, finished) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        supervisor.shutdown().unwrap();
        done.send(()).unwrap();
    });
    // The cancel killed the worker, which ended the reader's wait.
    at_exit.recv_timeout(TIMEOUT).unwrap();
    let prematurely_returned = finished.recv_timeout(Duration::from_millis(500)).is_ok();
    release.wait();
    // A driver that never exits keeps shutdown joining it forever, and a bare
    // join on the shutdown thread would hang the whole test run instead of
    // failing this test. A disconnect means shutdown panicked; the join below
    // reports that panic.
    if !prematurely_returned
        && let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(TIMEOUT)
    {
        panic!("shutdown did not return after the run's worker reader exited");
    }
    shutdown.join().unwrap();
    assert!(
        !prematurely_returned,
        "shutdown returned while the run's worker reader was still running"
    );
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "cancelled"
    );
    drop(held);
}

fn block_worker(f: &Fixture) {
    let awaiting = f
        .calls()
        .iter()
        .filter(|call| call.outcome == Outcome::Pending)
        .map(|call| call.position)
        .collect();
    f.worker.send(WorkerMessage::Blocked { awaiting }).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while !f.supervisor.0.worker_blocked.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "blocked frame was not processed");
        thread::yield_now();
    }
}

fn save_core_scope(f: &Fixture) {
    f.host.store.write(|tx| {
        tx.execute("INSERT INTO codemode_scopes(run_id,request,opened) VALUES ('r',?1,?2)",rusqlite::params![
            json!({"agent_id":"agent","invoking_scope":{"ref":"agent","epoch":2},"run_id":"r","expires_at_ms":2_460_100}).to_string(),
            json!({"scope":{"ref":"scope-r","epoch":7},"expires_at_ms":2_460_100,"catalog":[]}).to_string()])?;
        Ok(())
    }).unwrap();
}

#[test]
fn person_wait_hook_exhausts_one_accumulated_run_budget() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits::default(),
        1_800_100,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "read", json!({}));
    let sent = f.attempt();
    block_worker(&f);
    f.supervisor.held("r", 0, true).unwrap();
    f.clock.advance(200_000);
    f.supervisor.held("r", 0, false).unwrap();
    sent.reply.send(Ok(json!(1))).unwrap();
    f.delivery();
    f.clock.advance(100);
    f.issue(1, "read", json!({}));
    let second = f.attempt();
    block_worker(&f);
    f.supervisor.held("r", 1, true).unwrap();
    f.clock.advance(400_000);
    let result = f.terminal();
    assert_eq!(result["status"], "budget_exhausted:person_wait");
    assert_eq!(result["person_wait_ms"], 600_000);
    assert_eq!(result["calls"][0]["outcome"], "ok");
    assert_eq!(result["calls"][1]["outcome"], "outcome_unknown");
    assert_eq!(result["calls"][1]["waited_on_person"], true);
    drop(second);
}

#[test]
fn person_time_requires_blocked_worker_and_every_flight_held() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits::default(),
        1_800_100,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "read", json!({}));
    let first = f.attempt();
    f.supervisor.held("r", 0, true).unwrap();
    f.clock.advance(100);
    f.supervisor.held("r", 0, true).unwrap();
    f.issue(1, "read", json!({}));
    let second = f.attempt();
    block_worker(&f);
    f.clock.advance(100);
    f.supervisor.held("r", 1, true).unwrap();
    f.clock.advance(500);
    f.supervisor.held("r", 0, false).unwrap();
    first.reply.send(Ok(json!(1))).unwrap();
    f.delivery();
    second.reply.send(Ok(json!(2))).unwrap();
    f.delivery();
    f.finish("3");
    assert_eq!(f.terminal()["person_wait_ms"], 500);
}

#[test]
fn cancel_closes_core_scope_before_worker_kill() {
    let f = Fixture::new();
    f.standard();
    save_core_scope(&f);
    f.supervisor.cancel("r").unwrap();
    let events = f.host.events.lock().unwrap();
    assert!(
        events.iter().position(|e| e == "core_close").unwrap()
            < events.iter().position(|e| e == "kill").unwrap(),
        "{events:?}"
    );
    assert_eq!(events.iter().filter(|e| *e == "core_close").count(), 1);
}

#[test]
fn restart_closes_leftover_core_scope_without_starting_worker() {
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| store::insert_run(tx, &new_run("r"), None))
        .unwrap();
    save_core_scope(&f);
    f.supervisor.recover().unwrap();
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "interrupted"
    );
    assert_eq!(
        f.host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| *e == "core_close")
            .count(),
        1
    );
    assert!(f.source.0.lock().unwrap().is_some());
}

#[test]
fn tool_provider_migration_preserves_calls_and_schedules_legacy_scope_close() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    for migration in &crate::schema::MIGRATIONS[..19] {
        conn.execute_batch(migration.statements).unwrap();
    }
    let tx = conn.transaction().unwrap();
    let mut run = new_run("r");
    run.scope =
        r#"{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"legacy","epoch":7}"#;
    store::insert_run(&tx, &run, None).unwrap();
    store::insert_call(&tx, "r", 0, "read", 4, CallStart::Intent { at: 100 }).unwrap();
    store::record_outcome(&tx, "r", 0, Outcome::Ok, None, 137).unwrap();
    store::insert_call(&tx, "r", 1, "read", 4, CallStart::Queued).unwrap();
    tx.commit().unwrap();
    conn.execute_batch(crate::schema::MIGRATIONS[19].statements)
        .unwrap();
    let calls = store::calls(&conn, "r").unwrap();
    assert_eq!(calls[0].outcome, Outcome::Ok);
    assert_eq!(calls[0].entered_at, Some(100));
    assert_eq!(calls[0].duration_ms, Some(37));
    assert_eq!(calls[1].outcome, Outcome::Pending);
    assert_eq!(calls[1].intent_at, None);
    let opened: String = conn
        .query_row(
            "SELECT opened FROM codemode_scopes WHERE run_id='r'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let opened: basal_host::run_scope::Opened = serde_json::from_str(&opened).unwrap();
    assert_eq!(
        opened.scope,
        basal_host::run_scope::Scope {
            reference: "legacy".into(),
            epoch: 7
        }
    );
    assert!(
        conn.execute(
            "UPDATE codemode_calls SET outcome='error',code='bad' WHERE run_id='r' AND position=0",
            []
        )
        .is_err()
    );
    let tx = conn.transaction().unwrap();
    store::settle_pending_calls(&tx, "r", 140).unwrap();
    store::commit_terminal(&tx, "r", &exhausted(Budget::PersonWait), 140).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        store::calls(&conn, "r").unwrap()[1].outcome,
        Outcome::Cancelled
    );
    assert!(
        conn.execute(
            "UPDATE codemode_runs SET output='changed' WHERE run_id='r'",
            []
        )
        .is_err()
    );
}

#[test]
fn daemon_scope_end_interrupts_without_waiting_for_another_dispatch() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({}));
    let sent = f.attempt();
    f.host.scope_end.lock().unwrap().as_ref().unwrap()("scope_expired");
    let result = f.terminal();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["error"]["code"], "scope_expired");
    assert_eq!(result["calls"][0]["outcome"], "outcome_unknown");
    drop(sent);
}

#[test]
fn raw_scope_expired_refusal_and_provider_denial_have_distinct_outcomes() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({}));
    f.attempt()
        .reply
        .send(Err(WireError::Refused {
            code: "denied".into(),
            message: "provider policy".into(),
        }))
        .unwrap();
    rejection(f.delivery(), "read", "denied", "denied");
    f.issue(1, "read", json!({}));
    f.attempt()
        .reply
        .send(Err(WireError::RefusedDetails {
            code: "scope_expired".into(),
            message: "expired epoch".into(),
            detail: json!({}),
        }))
        .unwrap();
    let result = f.terminal();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["error"]["code"], "scope_expired");
    assert_eq!(result["calls"][0]["outcome"], "denied");
}

fn held_reply() -> Value {
    json!({"content":[{"type":"text","text":"Waiting for approval"}],"held":{"reason":"consent","detail":{"site":"example.test"}}})
}

fn late_entry(key: &str, event: &str, result: Value) -> Value {
    json!({"kind":"result","owner":"reserved:core","ref":"scope-r","scope_epoch":7,"custodian":"reserved:basal","call_key":key,"event_id":event,"settled_at":100,"result":result})
}

fn late_page(incarnation: &str, seq: u64, entries: Vec<Value>) -> Value {
    json!({"entries":entries,"cursor":{"provider_incarnation":incarnation,"seq":seq},"more":false})
}

impl Fixture {
    fn custody_attempt(&self, name: &str) -> CustodyAttempt {
        let attempt = self.custody_attempts.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(attempt.name, name);
        attempt
    }
    fn provider_held(&self) -> (Attempt, CustodyAttempt) {
        self.host.content_results.store(true, Ordering::SeqCst);
        self.host.custody_controlled.store(true, Ordering::SeqCst);
        let call = self.held_far_from_its_deadline();
        self.worker
            .send(WorkerMessage::Blocked { awaiting: vec![0] })
            .unwrap();
        call.reply.send(Ok(held_reply())).unwrap();
        assert!(
            self.parent.recv_timeout(Duration::from_millis(75)).is_err(),
            "a held reply must not settle the program's await"
        );
        let pull = self.custody_attempt("late_results");
        assert_eq!(pull.input["since"], Value::Null);
        (call, pull)
    }
}

#[test]
fn provider_held_reply_stays_pending_until_late_value_and_counts_person_wait() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    assert!(f.parent.try_recv().is_err(), "held is not the final answer");
    assert_eq!(
        f.supervisor.holds("r"),
        [Hold {
            index: 0,
            tool: "read".into(),
            reason: Some("consent".into()),
        }]
    );
    assert_eq!(f.calls()[0].outcome, Outcome::Pending);
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["calls"][0]["waited_on_person"],
        true
    );
    f.clock.set(1_100);
    pull.reply.send(Ok(late_page("i1", 1, vec![late_entry(&call.key, "e1", json!({"content":[{"type":"text","text":"approved"}],"structuredContent":{"answer":42}}))]))).unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(delivery.value, JsonText::new(r#"{"answer":42}"#).unwrap());
    assert!(
        f.supervisor.holds("r").is_empty(),
        "a settled call is no longer held"
    );
    let ack = f.custody_attempt("late_results.ack");
    assert_eq!(
        ack.input,
        json!({"through":{"provider_incarnation":"i1","seq":1}})
    );
    assert_eq!(f.calls()[0].outcome, Outcome::Ok, "settlement precedes ack");
    ack.reply.send(Ok(json!({}))).unwrap();
    f.finish("42");
    let result = f.terminal();
    assert_eq!(result["person_wait_ms"], 1_000);
    assert!(
        f.custody_attempts
            .recv_timeout(Duration::from_millis(75))
            .is_err()
    );
}

#[test]
fn cancel_while_provider_held_withdraws_before_closing_scope() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    let supervisor = f.supervisor.clone();
    let cancelled = thread::spawn(move || supervisor.cancel("r").unwrap().unwrap());
    let withdraw = f.custody_attempt("tool.withdraw");
    assert_eq!(
        withdraw.input,
        json!({"call_key":call.key,"scope":{"owner":"reserved:core","ref":"scope-r","scope_epoch":7}})
    );
    assert!(
        !f.host
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == "core_close")
    );
    withdraw
        .reply
        .send(Ok(json!({"answer":"withdrawn"})))
        .unwrap();
    assert_eq!(cancelled.join().unwrap()["status"], "cancelled");
    assert_eq!(f.calls()[0].outcome, Outcome::Withdrawn);
    drop(pull);
    assert!(
        f.attempts.try_recv().is_err(),
        "the held tool is never resent"
    );
}

#[test]
fn provider_held_person_budget_withdraws_and_keeps_started_outcome_unknown() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    f.clock.set(600_100);
    let withdraw = f.custody_attempt("tool.withdraw");
    withdraw
        .reply
        .send(Ok(json!({"answer":"already_started","outcome":"unknown"})))
        .unwrap();
    let result = f.terminal();
    assert_eq!(result["status"], "budget_exhausted:person_wait");
    assert_eq!(result["person_wait_ms"], 600_000);
    assert_eq!(f.calls()[0].outcome, Outcome::OutcomeUnknown);
    drop(pull);
}

#[test]
fn scope_end_while_provider_held_is_unknown_and_never_resent() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    f.host.scope_end.lock().unwrap().as_ref().unwrap()("scope_parent_ended");
    let result = f.terminal();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["error"]["code"], "scope_parent_ended");
    assert_eq!(f.calls()[0].outcome, Outcome::OutcomeUnknown);
    assert!(f.custody_attempts.try_recv().is_err());
    assert!(f.attempts.try_recv().is_err());
    drop(pull);
}

#[test]
fn provider_incarnation_rereads_from_null_without_double_settlement() {
    let f = Fixture::new();
    let (first, pull) = f.provider_held();
    f.issue(1, "read", json!({}));
    let second = f.attempt();
    second.reply.send(Ok(held_reply())).unwrap();
    let e1 = late_entry(
        &first.key,
        "e1",
        json!({"content":[{"type":"text","text":"first"}],"structuredContent":{"n":11}}),
    );
    pull.reply
        .send(Ok(late_page("i1", 1, vec![e1.clone()])))
        .unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(delivery.value.as_str(), r#"{"n":11}"#);
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    let stale = f.custody_attempt("late_results");
    assert_eq!(
        stale.input["since"],
        json!({"provider_incarnation":"i1","seq":1})
    );
    stale
        .reply
        .send(Err(WireError::Refused {
            code: "cursor_incarnation_changed".into(),
            message: "restarted".into(),
        }))
        .unwrap();
    let fresh = f.custody_attempt("late_results");
    assert_eq!(fresh.input["since"], Value::Null);
    fresh
        .reply
        .send(Ok(late_page(
            "i2",
            2,
            vec![e1, late_entry(&second.key, "e2", json!({"content":[{"type":"text","text":"second"}],"structuredContent":{"n":22}}))],
        )))
        .unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.position, 1);
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(delivery.value.as_str(), r#"{"n":22}"#);
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.finish("33");
    assert_eq!(f.terminal()["status"], "completed");
    assert!(
        f.parent.try_recv().is_err(),
        "duplicate entry must not settle twice"
    );
    assert_eq!(
        f.calls()
            .iter()
            .filter(|c| c.outcome == Outcome::Ok)
            .count(),
        2
    );
}

#[test]
fn no_late_results_traffic_without_a_provider_held_reply() {
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({}));
    let call = f.attempt();
    // A dispatch slot and a provider route alone must not start the poller.
    f.worker
        .send(WorkerMessage::Blocked { awaiting: vec![0] })
        .unwrap();
    assert!(
        f.custody_attempts
            .recv_timeout(Duration::from_millis(75))
            .is_err()
    );
    call.reply.send(Ok(json!(123))).unwrap();
    f.delivery();
    f.finish("123");
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.host.custody_count.load(Ordering::SeqCst), 0);
}

#[test]
fn onward_content_results_map_structured_text_and_errors_and_reject_malformed_held() {
    for (body, expected) in [
        (
            json!({"content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}),
            Ok(json!("one\ntwo")),
        ),
        (
            json!({"content":[{"type":"text","text":"display"}],"structuredContent":{"machine":7}}),
            Ok(json!({"machine":7})),
        ),
        (
            json!({"content":[{"type":"text","text":"denied"}],"isError":true,"errorCode":"permission_denied"}),
            Err("permission_denied"),
        ),
        (
            json!({"content":[{"type":"text","text":"bad"}],"isError":true,"errorCode":"bad_held","held":{"reason":"consent"}}),
            Err("reply_unreadable"),
        ),
        (
            json!({"content":[{"type":"text","text":"bad"}],"held":{"reason":"Not Snake Case"}}),
            Err("reply_unreadable"),
        ),
    ] {
        let f = Fixture::new();
        f.host.content_results.store(true, Ordering::SeqCst);
        f.standard();
        f.issue(0, "read", json!({}));
        f.attempt().reply.send(Ok(body)).unwrap();
        let delivery = f.delivery();
        match expected {
            Ok(value) => {
                assert_eq!(delivery.settlement, Settlement::Fulfilled);
                assert_eq!(delivery.value, JsonText::new(value.to_string()).unwrap());
            }
            Err(code) => {
                assert_eq!(delivery.settlement, Settlement::Rejected);
                let error: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
                assert_eq!(error["code"], code);
            }
        }
        assert_eq!(f.host.custody_count.load(Ordering::SeqCst), 0);
        f.finish("null");
        assert_eq!(f.terminal()["status"], "completed");
    }
}

#[test]
fn late_not_started_expired_and_error_results_are_terminal_without_retry() {
    for (kind, result, outcome, code) in [
        ("not_started", None, Outcome::Withdrawn, "not_started"),
        ("expired", None, Outcome::OutcomeUnknown, "result_expired"),
        (
            "result",
            Some(
                json!({"content":[{"type":"text","text":"No approval"}],"isError":true,"errorCode":"permission_denied"}),
            ),
            Outcome::Error,
            "permission_denied",
        ),
    ] {
        let f = Fixture::new();
        let (call, pull) = f.provider_held();
        let mut entry = late_entry(&call.key, "event", Value::Null);
        entry["kind"] = json!(kind);
        entry["reason"] = json!("restart_before_dispatch");
        entry.as_object_mut().unwrap().remove("result");
        if let Some(value) = result {
            entry["result"] = value;
        }
        pull.reply
            .send(Ok(late_page("i1", 1, vec![entry])))
            .unwrap();
        let delivery = f.delivery();
        assert_eq!(delivery.settlement, Settlement::Rejected);
        let rejection: Value = serde_json::from_str(delivery.value.as_str()).unwrap();
        assert_eq!(rejection["code"], code);
        assert_eq!(f.calls()[0].outcome, outcome);
        f.custody_attempt("late_results.ack")
            .reply
            .send(Ok(json!({})))
            .unwrap();
        f.finish("null");
        assert_eq!(f.terminal()["status"], "completed");
        assert!(f.attempts.try_recv().is_err());
    }
}

#[test]
fn route_draining_while_held_is_unknown_without_reopening() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    pull.reply
        .send(Err(WireError::ScopeCut("route drained".into())))
        .unwrap();
    assert_eq!(f.terminal()["status"], "interrupted");
    assert_eq!(f.calls()[0].outcome, Outcome::OutcomeUnknown);
    assert!(f.attempts.try_recv().is_err());
    assert!(f.custody_attempts.try_recv().is_err());
}

#[test]
fn wall_deadline_with_a_held_call_withdraws_even_when_worker_is_not_blocked() {
    let f = Fixture::new();
    f.host.custody_controlled.store(true, Ordering::SeqCst);
    f.host.content_results.store(true, Ordering::SeqCst);
    f.standard();
    f.issue(0, "read", json!({}));
    f.attempt().reply.send(Ok(held_reply())).unwrap();
    let pull = f.custody_attempt("late_results");
    f.clock.set(10_000);
    let withdraw = f.custody_attempt("tool.withdraw");
    withdraw
        .reply
        .send(Err(WireError::ScopeCut(
            "deadline drained the route".into(),
        )))
        .unwrap();
    assert_eq!(f.terminal()["status"], "budget_exhausted:wall");
    assert_eq!(f.calls()[0].outcome, Outcome::OutcomeUnknown);
    drop(pull);
}

#[path = "custody_review_tests.rs"]
mod custody_review;

impl Fixture {
    fn sleep(&self, position: u64, args: &str) {
        self.worker
            .send(WorkerMessage::HostCall(HostCall {
                position,
                kind: CallKind::Primitive(Primitive::Sleep),
                args: JsonText::new(args).unwrap(),
            }))
            .unwrap();
    }

    /// Returns once the driver has handled every event sent before this call.
    /// Worker frames reach the driver in order and a manual clock move queues
    /// its event before returning, so a console line stored after them proves
    /// they were handled, without waiting on real time.
    fn settled(&self, marker: &str) {
        let line = format!("{marker}\n");
        self.worker
            .send(WorkerMessage::Console { line: line.clone() })
            .unwrap();
        let until = Instant::now() + TIMEOUT;
        while !self.supervisor.result("r").unwrap().unwrap()["output"]
            .as_str()
            .unwrap()
            .ends_with(&line)
        {
            assert!(Instant::now() < until, "{marker} was not stored");
            thread::yield_now();
        }
    }
}

#[test]
fn a_sleep_completes_when_the_clock_reaches_it_and_is_not_a_tool_call() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits {
            tool_calls: 1,
            ..Limits::default()
        },
        FAR_DEADLINE,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.sleep(0, "250");
    f.settled("issued");
    assert!(
        f.parent.try_recv().is_err(),
        "the sleep ended before its time"
    );
    f.clock.advance(249);
    f.settled("almost");
    assert!(
        f.parent.try_recv().is_err(),
        "the sleep ended a millisecond early"
    );
    f.clock.advance(1);
    let woke = f.delivery();
    assert_eq!(
        (woke.position, woke.settlement, woke.value.as_str()),
        (0, Settlement::Fulfilled, "null")
    );
    // More sleeps still leave the whole one-call budget to the tool call.
    f.sleep(1, "0");
    assert_eq!(f.delivery().position, 1);
    f.sleep(2, "10000");
    f.settled("long");
    f.clock.advance(10_000);
    assert_eq!(f.delivery().position, 2);
    assert!(f.calls().is_empty(), "a sleep wrote a call row");
    f.issue(3, "read", json!({}));
    f.attempt()
        .reply
        .send(Err(WireError::RefusedDetails {
            code: "no_session".into(),
            message: "No session.".into(),
            detail: json!({}),
        }))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Rejected);
    f.worker
        .send(WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::ScriptHostRejection {
                position: 3,
                message: "HostError: No session.".into(),
            }),
        })
        .unwrap();
    let result = f.terminal();
    // The first tool call is call 1, although three sleeps came before it.
    assert_eq!(
        result["error"],
        json!({"code":"no_session","message":"read (call 1) failed: No session."})
    );
    assert_eq!(result["call_count"], 1);
    assert_eq!(f.calls().len(), 1);
    f.assert_order();
}

#[test]
fn a_sleep_outside_0_to_10000_ms_from_the_worker_is_a_profile_violation() {
    for args in ["10001", "-1", "1.5", "null", "\"5\""] {
        let f = Fixture::new();
        f.standard();
        f.sleep(0, args);
        let result = f.terminal();
        assert_eq!(result["status"], "failed", "{args}");
        assert_eq!(result["error"]["code"], "profile_violation", "{args}");
        assert!(f.calls().is_empty());
        f.assert_order();
    }
}

#[test]
fn a_sleep_running_past_the_wall_deadline_ends_the_run_as_wall_exhausted() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits::default(),
        1_000,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.sleep(0, "10000");
    f.settled("sleeping");
    f.clock.set(999);
    f.settled("before the deadline");
    assert!(f.parent.try_recv().is_err());
    // The clock reaches the sleep's own end too, but the deadline came first.
    f.clock.set(10_100);
    let result = f.terminal();
    assert_eq!(result["status"], "budget_exhausted:wall");
    assert_eq!(result["error"]["code"], "budget_exhausted:wall");
    f.assert_order();
    assert!(
        !matches!(f.parent.try_recv(), Ok(ParentMessage::Deliver(_))),
        "the sleep was completed after the deadline"
    );
}

#[test]
fn cancelling_a_run_ends_its_sleep_at_once() {
    let f = Fixture::new();
    f.standard();
    f.sleep(0, "10000");
    f.settled("sleeping");
    let result = f.supervisor.cancel("r").unwrap().unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(f.clock.now_ms(), 100, "the cancel needed no clock move");
    f.assert_order();
    assert!(
        !matches!(f.parent.try_recv(), Ok(ParentMessage::Deliver(_))),
        "a cancelled sleep was completed"
    );
}

#[test]
fn a_pending_sleep_keeps_held_time_off_the_person_wait_budget() {
    let f = Fixture::new();
    f.start(
        &[("read", "notes", "read", json!({}))],
        Limits::default(),
        FAR_DEADLINE,
    );
    f.parent.recv_timeout(TIMEOUT).unwrap();
    f.issue(0, "read", json!({}));
    let held = f.attempt();
    f.supervisor.held("r", 0, true).unwrap();
    f.sleep(1, "10000");
    block_worker(&f);
    // Every tool call is held on a person, but the program is also sleeping,
    // so this time is the program's own wait, not the person's.
    f.clock.advance(5_000);
    f.supervisor.held("r", 0, true).unwrap();
    f.clock.advance(5_000);
    assert_eq!(f.delivery().position, 1);
    f.supervisor.held("r", 0, false).unwrap();
    held.reply.send(Ok(json!(1))).unwrap();
    assert_eq!(f.delivery().position, 0);
    f.finish("1");
    assert_eq!(f.terminal()["person_wait_ms"], 0);
}
