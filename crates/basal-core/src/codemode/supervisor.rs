//! One blocking supervisor per codemode run. The worker cannot authorize a
//! call, write its outcome, or decide which scope the provider receives.
//!
//! Blocking dispatch lanes belong to the supervisor, not to an async executor.
//! Only the supervisor records answers and delivers them to the worker, so
//! dropping its event receiver also discards answers that arrive after a kill.
//!
//! Each run's driver sleeps on one event channel until something happens:
//! a worker frame (forwarded by the run's reader thread), a provider answer,
//! a cancel, or a move of a manual clock. Without held replies its only timeout
//! is the run's wall deadline, so an ordinary slow RPC costs no polling CPU.
//! Held replies additionally schedule bounded late-result reads.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[path = "held.rs"]
mod held;
#[path = "held_time.rs"]
mod held_time;

use basal_host::Catalog;
use basal_host::flow_refusal::RefusalReason;
use basal_host::transport::{Transport, WireError};
use basal_proto::{
    ActivationRequest, ActivationResult, BudgetKind, Budgets, CODEMODE_HEAP_BYTES, CallKind,
    Failure, HostCall, JsonText, MAX_VALUE_BYTES, Outcome as Delivery, ParentMessage, PreludeHash,
    Profile, Settlement, WorkerMessage,
};
use serde_json::{Value, json};

use super::admission::Start;
use super::catalog::InputSchema;
use super::store::{
    self, Budget, CallStart, Lookup, Outcome, RunError, RunRecord, Status, Terminal,
};
use crate::authorize::ShellDenylist;
use crate::channel::{ChannelError, WorkerChannel, WorkerReceiver, WorkerSource};
use crate::clock::{Clock, Watcher};
use crate::error::{CoreError, Result};
use crate::store::Store;

const IN_FLIGHT: usize = 8;
const QUEUED_BYTES: usize = 4 * 1024 * 1024;
const RESULT_BYTES: usize = 16_384;
/// The longest single `sleep` a program may ask for. The worker's prelude
/// refuses longer ones; the parent checks again, so a worker that bypassed
/// the prelude cannot hold a longer pause.
const MAX_SLEEP_MS: u64 = 10_000;

/// Everything that can wake a run's driver, in the order it happened.
enum Event {
    /// A frame from the worker, or the error that ended its channel.
    Worker(std::result::Result<WorkerMessage, ChannelError>),
    /// The worker's receiver panicked on the reader thread.
    ReaderPanicked,
    /// A provider answer from a dispatch thread.
    Answer(Answer),
    LateAnswer {
        call: HostCall,
        entry: std::result::Result<cortexkit_role_tool_provider::late_results::LateEntry, String>,
        ack: mpsc::SyncSender<()>,
    },
    CustodyChanged,
    ModelAnswer {
        position: u64,
        result: std::result::Result<basal_host::HostOutcome, WireError>,
    },
    Cancel(mpsc::SyncSender<Result<()>>),
    Held {
        position: u64,
        held: bool,
        ack: mpsc::SyncSender<()>,
    },
    /// A manual clock moved, so the wall deadline may have passed without
    /// any real time elapsing.
    ClockMoved,
    ScopeEnded(String),
}

#[derive(Default)]
struct Drivers {
    stopped: bool,
    threads: Vec<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct DispatchStarts {
    pending: Mutex<usize>,
    changed: Condvar,
}

struct DispatchStore {
    store: Option<Arc<Store>>,
    starts: Arc<DispatchStarts>,
}

impl DispatchStore {
    fn new(store: Arc<Store>, starts: Arc<DispatchStarts>) -> Self {
        *starts.pending.lock().unwrap() += 1;
        Self {
            store: Some(store),
            starts,
        }
    }
}

impl std::ops::Deref for DispatchStore {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.store.as_deref().unwrap()
    }
}

impl Drop for DispatchStore {
    fn drop(&mut self) {
        // Publish completion only after releasing the writer lease reference.
        self.store.take();
        *self.starts.pending.lock().unwrap() -= 1;
        self.starts.changed.notify_all();
    }
}

struct Shared {
    store: Arc<Store>,
    transport: Arc<dyn Transport>,
    catalog: Arc<dyn Catalog>,
    models: Mutex<Option<Models>>,
    workers: Arc<dyn WorkerSource>,
    clock: Clock,
    prelude_hash: PreludeHash,
    active: Mutex<BTreeMap<String, mpsc::Sender<Event>>>,
    drivers: Mutex<Drivers>,
    dispatch_starts: Arc<DispatchStarts>,
    custody: Arc<held::Custody>,
    /// Calls each live run is awaiting while a person decides, so a reply
    /// sent before the run ends can say what it is waiting on.
    holds: Mutex<BTreeMap<String, BTreeMap<u64, Hold>>>,
    terminal_changed: (Mutex<u64>, Condvar),
    terminal_hook: Mutex<Option<Arc<TerminalHook>>>,
    #[cfg(test)]
    before_entry: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_record: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_exit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_reader_exit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Called by a driver after it handled the worker's `Blocked` frame, so a
    /// test can send frames that must follow that handling.
    #[cfg(test)]
    after_block: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Called by a driver just before it blocks waiting for its next event.
    #[cfg(test)]
    before_wait: Mutex<Option<ParkedHook>>,
    /// How many times a driver has returned from its wait for an event.
    #[cfg(test)]
    wakes: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    worker_blocked: AtomicBool,
}

/// Owns codemode supervisors only. Supply a fresh-worker source, never a flow
/// pool, and the parent's codemode prelude hash, never the worker's claim.
#[derive(Clone)]
pub struct Supervisor(Arc<Shared>);

pub type TerminalHook = dyn Fn(&str, Value) -> Result<()> + Send + Sync;

/// A test's callback for a driver about to block; see [`Parked`].
#[cfg(test)]
type ParkedHook = Arc<dyn Fn(&Parked) + Send + Sync>;

/// What a driver reports to a test just before it blocks for its next event,
/// so the test can wait for the driver to go idle instead of sleeping.
#[cfg(test)]
#[derive(Clone, Copy)]
struct Parked {
    /// The driver's wake count so far; the wake that ends this wait is next.
    wakes: usize,
    /// How long the driver will block unless an event arrives first.
    wait: Duration,
    /// The program has finished, but the run cannot end yet, for example
    /// because a late-result page is still unacknowledged.
    completion_pending: bool,
}

/// A tool call a running program is awaiting while a person decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    /// The call's zero-based index among the run's tool calls, in the order
    /// the program made them. Sleeps are not counted.
    pub index: u64,
    pub tool: String,
    /// The provider's snake_case reason, such as `consent`, when its held
    /// reply named one.
    pub reason: Option<String>,
}

#[derive(Clone)]
struct Models {
    host: Arc<dyn basal_host::Host>,
    selector: Arc<dyn basal_host::selector::ModelSelector>,
}

struct ModelSink(std::sync::Weak<Shared>);
impl basal_host::CompletionSink for ModelSink {
    fn complete(
        &self,
        completion: &basal_host::Completion,
    ) -> std::result::Result<basal_host::CompletionAck, basal_host::SinkError> {
        if let Some(shared) = self.0.upgrade()
            && let Some(sender) = shared.active.lock().unwrap().get(&completion.run_id)
        {
            let _ = sender.send(Event::ModelAnswer {
                position: completion.position,
                result: Ok(completion.outcome.clone()),
            });
        }
        Ok(basal_host::CompletionAck::Accepted)
    }
    fn unknown(
        &self,
        unknown: &basal_host::UnknownOutcome,
    ) -> std::result::Result<basal_host::CompletionAck, basal_host::SinkError> {
        if let Some(shared) = self.0.upgrade()
            && let Some(sender) = shared.active.lock().unwrap().get(&unknown.run_id)
        {
            let _ = sender.send(Event::ModelAnswer {
                position: unknown.position,
                result: Err(WireError::Unknown(unknown.detail.clone())),
            });
        }
        Ok(basal_host::CompletionAck::Accepted)
    }
}

impl Supervisor {
    /// Recover durable runs before exposing any operation. A recovery failure
    /// prevents startup rather than leaving old runs eligible for dispatch.
    pub fn new(
        store: Arc<Store>,
        transport: Arc<dyn Transport>,
        catalog: Arc<dyn Catalog>,
        workers: Arc<dyn WorkerSource>,
        clock: Clock,
        prelude_hash: PreludeHash,
        _denylist: ShellDenylist,
    ) -> Result<Self> {
        let supervisor = Self(Arc::new(Shared {
            store,
            transport,
            catalog,
            models: Mutex::new(None),
            workers,
            clock,
            prelude_hash,
            active: Mutex::new(BTreeMap::new()),
            drivers: Mutex::new(Drivers::default()),
            dispatch_starts: Arc::new(DispatchStarts::default()),
            custody: Arc::new(held::Custody::default()),
            holds: Mutex::new(BTreeMap::new()),
            terminal_changed: (Mutex::new(0), Condvar::new()),
            terminal_hook: Mutex::new(None),
            #[cfg(test)]
            before_entry: Mutex::new(None),
            #[cfg(test)]
            before_record: Mutex::new(None),
            #[cfg(test)]
            before_exit: Mutex::new(None),
            #[cfg(test)]
            before_reader_exit: Mutex::new(None),
            #[cfg(test)]
            after_block: Mutex::new(None),
            #[cfg(test)]
            before_wait: Mutex::new(None),
            #[cfg(test)]
            wakes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            worker_blocked: AtomicBool::new(false),
        }));
        supervisor.recover()?;
        Ok(supervisor)
    }

    pub fn with_models(
        self,
        host: Arc<dyn basal_host::Host>,
        selector: Arc<dyn basal_host::selector::ModelSelector>,
    ) -> Self {
        host.attach_codemode(Arc::new(ModelSink(Arc::downgrade(&self.0))));
        *self.0.models.lock().unwrap() = Some(Models { host, selector });
        self
    }

    /// Consumes the admission winner's token. No worker is acquired on the
    /// caller's thread; even worker startup is performed off the event loop.
    pub fn start(&self, run: RunRecord, start: Start) -> Result<()> {
        let mut drivers = self.0.drivers.lock().unwrap();
        if drivers.stopped {
            return Err(CoreError::Invalid("codemode supervisor is stopped".into()));
        }
        let (events, rx) = mpsc::channel();
        let mut active = self.0.active.lock().unwrap();
        if run.status == Status::Running
            && let Lookup::Found(current) =
                self.0.store.read(|conn| store::lookup(conn, &run.run_id))?
            && current.status.is_terminal()
        {
            return Ok(());
        }
        if active.contains_key(&run.run_id) || run.status != Status::Running {
            return Err(CoreError::Invalid("run cannot be started twice".into()));
        }
        let id = run.run_id.clone();
        active.insert(id.clone(), events.clone());
        let shared = self.0.clone();
        match thread::Builder::new()
            .name(format!("codemode:{id}"))
            .spawn(move || {
                let mut driver = Driver::new(shared.clone(), run, start, events, rx);
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.drive()))
                        .unwrap_or_else(|_| Err(CoreError::Invalid("supervisor panicked".into())));
                if let Err(error) = result {
                    // A broken worker channel or store must not leave a live worker
                    // with authority. Preserve any output collected before the error.
                    let terminal = failed("engine_error", error.to_string());
                    if let Err(error) = driver.end(terminal) {
                        tracing::error!(%error, "codemode termination could not commit");
                    }
                }
                shared.active.lock().unwrap().remove(&driver.run.run_id);
                driver.stop_reader();
                #[cfg(test)]
                if let Some(hook) = shared.before_exit.lock().unwrap().as_ref() {
                    hook();
                }
            }) {
            Ok(handle) => drivers.threads.push(handle),
            Err(error) => {
                active.remove(&id);
                terminate(&self.0, &id, None, failed("worker_lost", error.to_string()))?;
            }
        }
        Ok(())
    }

    /// Stop admitting drivers, cancel every live run and release all driver
    /// store references before returning. Provider calls already in flight may
    /// finish later, but they retain no store and their answer receiver is gone.
    pub fn shutdown(&self) -> Result<()> {
        let mut drivers = {
            let mut registered = self.0.drivers.lock().unwrap();
            registered.stopped = true;
            Drivers {
                stopped: true,
                threads: std::mem::take(&mut registered.threads),
            }
        };
        let ids = self
            .0
            .active
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut error = None;
        let mut cancellations = Vec::new();
        for id in ids {
            if let Some(sender) = self.0.active.lock().unwrap().get(&id).cloned() {
                let (ack, done) = mpsc::sync_channel(1);
                if sender.send(Event::Cancel(ack)).is_ok() {
                    cancellations.push(done);
                }
            }
        }
        for done in cancellations {
            if let Ok(Err(failure)) = done.recv() {
                error.get_or_insert(failure);
            }
        }
        for handle in drivers.threads.drain(..) {
            if handle.join().is_err() {
                error.get_or_insert_with(|| CoreError::Invalid("codemode driver panicked".into()));
            }
        }
        self.0.custody.shutdown();
        // A dispatch scheduled just before cancellation may still be recording
        // its intent. Wait for that store reference, never for the provider.
        let starts = &self.0.dispatch_starts;
        drop(
            starts
                .changed
                .wait_while(starts.pending.lock().unwrap(), |n| *n != 0)
                .unwrap(),
        );
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Unknown and pruned ids return `None`. Running durations use the same
    /// injected clock as deadlines; terminal durations are the stored ones.
    pub fn result(&self, id: &str) -> Result<Option<Value>> {
        Self::result_for(&self.0.store, &self.0.clock, id)
    }

    /// The calls a running program is currently awaiting on a person, in
    /// position order. Empty for unknown and ended runs.
    pub fn holds(&self, id: &str) -> Vec<Hold> {
        self.0
            .holds
            .lock()
            .unwrap()
            .get(id)
            .map(|holds| holds.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn on_terminal(&self, hook: Arc<TerminalHook>) {
        *self.0.terminal_hook.lock().unwrap() = Some(hook);
    }

    pub fn result_for(store: &Store, clock: &Clock, id: &str) -> Result<Option<Value>> {
        store.read(|conn| {
            let Lookup::Found(run) = store::lookup(conn, id)? else { return Ok(None) };
            let calls = store::calls(conn, id)?.into_iter().map(|call| -> Result<Value> {
                let mut value = json!({"tool": call.tool, "outcome": call.outcome.as_str()});
                let waited: bool = conn.query_row("SELECT waited_on_person FROM codemode_calls WHERE run_id=?1 AND position=?2",rusqlite::params![id,call.position],|row| row.get(0))?;
                value["waited_on_person"] = waited.into();
                if let Some(code) = call.code { value["code"] = code.into(); }
                if let Some(duration) = call.duration_ms { value["duration_ms"] = duration.into(); }
                Ok(value)
            }).collect::<Result<Vec<_>>>()?;
            let mut value = json!({
                "status": run.status.as_str(), "output": run.output, "calls": calls,
                "warnings": parse(&run.warnings)?, "catalog_digest": run.catalog_digest,
                "duration_ms": run.duration_ms.unwrap_or_else(|| clock.now_ms().saturating_sub(run.admitted_at).max(0))
            });
            let (keyless,person,count): (bool,i64,u64) = conn.query_row("SELECT keyless,person_wait_ms,tool_call_count FROM codemode_runs WHERE run_id=?1",[id],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            value["keyless"] = keyless.into();
            value["person_wait_ms"] = person.into();
            value["call_count"] = count.into();
            let limits: super::admission::Limits = serde_json::from_str(&run.limits)
                .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            let spent = crate::classify_cost::spent(conn, "program", id)?;
            value["budgets"] = json!({"model_cost_usd": {"limit":limits.model_cost_usd,
                "spent":spent,"remaining":limits.model_cost_usd - spent}});
            if let Some(result) = run.value { value["value"] = parse(&result)?; }
            if let Some(error) = run.error { value["error"] = json!({"code": error.code, "message": error.message}); }
            if let Some(description) = run.description { value["description"] = description.into(); }
            use rusqlite::OptionalExtension;
            let refusal: Option<(Option<String>,bool)> = conn.query_row("SELECT refusal,refused FROM codemode_scopes WHERE run_id=?1",[id],|row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((Some(error),refused)) = refusal {
                let error = parse(&error)?;
                if refused { value["status"] = error["code"].clone(); }
                if let Some(detail) = error.get("detail") { value["detail"] = detail.clone(); }
                value["error"] = error;
            }
            Ok(Some(value))
        })
    }

    /// Sleep on terminal commits, not a polling loop. A caller may leave a
    /// keyed run alive after its foreground wait and collect the late result.
    pub fn wait(&self, id: &str, timeout: Duration) -> Result<Option<Value>> {
        let (generation, changed) = &self.0.terminal_changed;
        let mut generation = generation.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(result) = self.result(id)?
                && result["status"] != "running"
            {
                return Ok(Some(result));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (next, expired) = changed.wait_timeout(generation, remaining).unwrap();
            generation = next;
            if expired.timed_out() {
                return Ok(None);
            }
        }
    }

    /// Cancellation is serialized with answer recording by the run's driver.
    /// Its acknowledgement follows scope release and the terminal commit.
    pub fn cancel(&self, id: &str) -> Result<Option<Value>> {
        self.cancel_with_started(id).map(|(result, _)| result)
    }

    pub fn cancel_with_started(&self, id: &str) -> Result<(Option<Value>, bool)> {
        let active = self.0.active.lock().unwrap();
        let sender = active.get(id).cloned();
        let started = sender.is_some();
        if sender.is_none()
            && let Lookup::Found(run) = self.0.store.read(|conn| store::lookup(conn, id))?
            && run.status == Status::Running
        {
            terminate(&self.0, id, None, terminal(Status::Cancelled, None, None))?;
        }
        drop(active);
        if let Some(sender) = sender {
            let (tx, rx) = mpsc::sync_channel(1);
            if sender.send(Event::Cancel(tx)).is_ok() {
                match rx.recv() {
                    Ok(result) => result?,
                    Err(_) => {
                        if let Some(value) = self.result(id)?
                            && value["status"] != "running"
                        {
                            return Ok((Some(value), started));
                        }
                        return Err(CoreError::Invalid("supervisor lost during cancel".into()));
                    }
                }
            }
        }
        self.result(id).map(|result| (result, started))
    }

    /// A provider may report that a dispatched call is waiting for a person's
    /// approval or decision, then deliver its final answer later as a late result.
    /// Mark that call held: the program's await must remain pending until the
    /// final answer arrives, so the initial held reply does not settle its promise.
    pub fn held(&self, id: &str, position: u64, held: bool) -> Result<()> {
        let sender = self.0.active.lock().unwrap().get(id).cloned();
        if let Some(sender) = sender {
            let (ack, done) = mpsc::sync_channel(1);
            if sender
                .send(Event::Held {
                    position,
                    held,
                    ack,
                })
                .is_ok()
            {
                let _ = done.recv();
            }
        }
        Ok(())
    }

    // Construction is the only production caller: there can be no live driver
    // yet. There is deliberately no call recovery or re-send, even for queries.
    fn recover(&self) -> Result<()> {
        if !self.0.active.lock().unwrap().is_empty() {
            return Err(CoreError::Invalid("cannot recover live supervisors".into()));
        }
        let runs = self.0.store.read(store::running_runs)?;
        for id in runs {
            let Lookup::Found(run) = self.0.store.read(|conn| store::lookup(conn, &id))? else {
                continue;
            };
            let mut terminal =
                interrupted("basal_restarted", "basal restarted before the run ended");
            terminal.output = run.output;
            terminal.warnings = run.warnings;
            terminate(&self.0, &run.run_id, None, terminal)?;
        }
        let leftover = self.0.store.read(|conn| {
            let mut statement =
                conn.prepare("SELECT run_id FROM codemode_scopes WHERE closed=0 AND refused=0")?;
            Ok(statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for id in leftover {
            if let Err(error) = super::scope::close(&self.0.store, self.0.transport.as_ref(), &id) {
                tracing::warn!(%error, "leftover run scope relies on its fixed expiry");
            }
        }
        Ok(())
    }
}

fn parse(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| CoreError::Corrupt(error.to_string()))
}

struct Answer {
    call: HostCall,
    result: std::result::Result<Value, WireError>,
    usage: Option<basal_host::TokenUsage>,
    no_effect: bool,
}

struct Driver {
    shared: Arc<Shared>,
    run: RunRecord,
    start: Start,
    worker: Option<Box<dyn WorkerChannel>>,
    events: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<Event>,
    /// Lets the reader thread read the worker's next frame. One frame at a
    /// time keeps a worker that floods its channel from queueing frames in
    /// memory faster than the driver handles them.
    credit: Option<mpsc::Sender<()>>,
    reader: Option<thread::JoinHandle<()>>,
    /// Kept alive for as long as the driver runs; the clock holds it weakly.
    clock_watch: Option<Watcher>,
    /// Set while a clock move is waiting in the event channel, so repeated
    /// moves queue one event rather than one each.
    clock_moved: Arc<AtomicBool>,
    queue: VecDeque<HostCall>,
    queued_bytes: usize,
    in_flight: usize,
    flight_positions: BTreeSet<u64>,
    held_positions: BTreeSet<u64>,
    tool_calls: BTreeMap<u64, (String, HostCall)>,
    /// The tool, provider code and message of each call that rejected, so a
    /// program ending on an uncaught rejection reports them as plain text.
    rejections: BTreeMap<u64, Rejection>,
    /// The reason each provider gave when it held a call. Only the
    /// tool-provider person-wait reasons mean a person must act; see
    /// `waits_on_person`.
    hold_reasons: BTreeMap<u64, held_time::Reason>,
    person_time: held_time::PersonTime,
    late_entries: BTreeSet<cortexkit_role_tool_provider::late_results::IntakeKey>,
    /// Pending sleeps as (due time on the run's clock, position), earliest
    /// first. The driver's event wait ends at the earliest one.
    sleeps: BTreeSet<(i64, u64)>,
    /// Each tool call's 1-based number among the run's tool calls. Sleeps
    /// take positions too, so messages number tool calls from this map
    /// rather than from positions.
    tool_numbers: BTreeMap<u64, u64>,
    blocked: bool,
    person_wait_ms: i64,
    last_tick_ms: i64,
    invocations: u64,
    delivery_order: u64,
    completion: Option<Terminal>,
    stopping: bool,
    parent_owned_kill: bool,
    output_truncated: bool,
    identity: Option<subc_protocol::BindIdentity>,
    scope_lost: bool,
}

impl Driver {
    fn new(
        shared: Arc<Shared>,
        run: RunRecord,
        start: Start,
        event_tx: mpsc::Sender<Event>,
        events: mpsc::Receiver<Event>,
    ) -> Self {
        let last_tick_ms = run.admitted_at;
        Self {
            shared,
            run,
            start,
            worker: None,
            events,
            event_tx,
            credit: None,
            reader: None,
            clock_watch: None,
            clock_moved: Arc::new(AtomicBool::new(false)),
            queue: VecDeque::new(),
            queued_bytes: 0,
            in_flight: 0,
            flight_positions: BTreeSet::new(),
            held_positions: BTreeSet::new(),
            tool_calls: BTreeMap::new(),
            rejections: BTreeMap::new(),
            hold_reasons: BTreeMap::new(),
            person_time: held_time::PersonTime::default(),
            late_entries: BTreeSet::new(),
            sleeps: BTreeSet::new(),
            tool_numbers: BTreeMap::new(),
            blocked: false,
            person_wait_ms: 0,
            last_tick_ms,
            invocations: 0,
            delivery_order: 0,
            completion: None,
            stopping: false,
            parent_owned_kill: false,
            output_truncated: false,
            identity: None,
            scope_lost: false,
        }
    }

    fn key(&self) -> String {
        format!("codemode:{}", self.run.run_id)
    }

    fn drive(&mut self) -> Result<()> {
        let identity: Option<String> = self.shared.store.read(|conn| {
            Ok(conn.query_row(
                "SELECT bind_identity FROM codemode_runs WHERE run_id=?1",
                [&self.run.run_id],
                |row| row.get(0),
            )?)
        })?;
        self.identity = identity
            .map(|identity| serde_json::from_str(&identity))
            .transpose()
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        self.watch_clock();
        if self.wall()? {
            return Ok(());
        }
        self.shared
            .transport
            .configure_flow(&self.key(), Some(self.start.scope.clone()));
        if let Some(models) = self.shared.models.lock().unwrap().as_ref() {
            models
                .host
                .configure_flow(&self.key(), Some(self.start.scope.clone()));
        }
        let scope_events = self.event_tx.clone();
        self.shared.transport.watch_run_scope(
            &self.key(),
            Arc::new(move |code| {
                let _ = scope_events.send(Event::ScopeEnded(code.into()));
            }),
        );
        self.worker = match self.shared.workers.worker() {
            Ok(worker) => Some(worker),
            Err(error) => {
                self.end(failed("worker_lost", error.to_string()))?;
                return Ok(());
            }
        };
        if self.wall()? {
            return Ok(());
        }
        let request = ActivationRequest {
            activation_id: 1,
            profile: Profile::Codemode,
            // Dropped tools keep their names so a call to one reaches the
            // parent and rejects with a typed reason, not a TypeError.
            tools: self
                .start
                .tools
                .keys()
                .chain(self.start.unavailable.keys())
                .cloned()
                .collect(),
            prelude_hash: self.shared.prelude_hash,
            script: self.run.program.clone(),
            trigger: JsonText::null(),
            self_input: JsonText::null(),
            params_input: JsonText::null(),
            budgets: Budgets {
                js_time_micros: 10_000_000,
                memory_bytes: CODEMODE_HEAP_BYTES,
                stack_bytes: 1024 * 1024,
                max_value_bytes: MAX_VALUE_BYTES as u32,
            },
            prefix: Vec::new(),
        };
        if let Err(error) = self
            .worker
            .as_mut()
            .unwrap()
            .send(&ParentMessage::Activate(Box::new(request)))
        {
            self.end(failed("worker_lost", error.to_string()))?;
            return Ok(());
        }
        self.spawn_reader()?;
        while !self.stopping {
            if self.wall()? {
                break;
            }
            self.wake_sleepers()?;
            if self.stopping {
                break;
            }
            self.drain()?;
            if self.stopping {
                break;
            }
            if self.in_flight == 0
                && self.queue.is_empty()
                && self.shared.custody.is_idle(&self.run.run_id)
                && let Some(terminal) = self.completion.take()
            {
                self.end(terminal)?;
                break;
            }
            match self.next_event() {
                // The loop's first step checks the wall deadline.
                None => {}
                Some(Event::ClockMoved) => self.clock_moved.store(false, Ordering::SeqCst),
                Some(Event::Cancel(ack)) => self.cancel(ack)?,
                Some(Event::Answer(answer)) => self.answer(answer)?,
                Some(Event::LateAnswer { call, entry, ack }) => {
                    self.late_answer(call, entry)?;
                    let _ = ack.send(());
                }
                Some(Event::CustodyChanged) => {}
                Some(Event::ModelAnswer { position, result }) => {
                    self.model_answer(position, result)?
                }
                Some(Event::ScopeEnded(code)) => {
                    self.scope_lost = true;
                    if !self.wall()? {
                        self.end(interrupted(&code, "the daemon ended the run scope"))?;
                    }
                }
                Some(Event::Held {
                    position,
                    held,
                    ack,
                }) => {
                    if self.wall()? {
                        let _ = ack.send(());
                        break;
                    }
                    self.mark_held(position, held)?;
                    if held {
                        self.start_intake(position)?;
                    }
                    let _ = ack.send(());
                }
                Some(Event::Worker(Ok(message))) => {
                    if self.wall()? {
                        break;
                    }
                    self.message(message)?;
                    if let Some(credit) = &self.credit {
                        let _ = credit.send(());
                    }
                }
                Some(Event::Worker(Err(error))) => {
                    if !self.parent_owned_kill {
                        self.end(failed("worker_lost", error.to_string()))?;
                    }
                }
                Some(Event::ReaderPanicked) => {
                    return Err(CoreError::Invalid("worker receiver panicked".into()));
                }
            }
        }
        Ok(())
    }

    /// Sleeps until an event, the applicable deadline (the injected wall
    /// deadline, or the person-wait budget while only people are awaited) or
    /// the earliest program sleep falling due. Provider intake workers own their polling
    /// timers; the driver wakes on their deliveries.
    fn next_event(&mut self) -> Option<Event> {
        let deadline = if self.person_only() {
            basal_host::run_scope::PERSON_WAIT_MS.saturating_sub(self.person_wait_ms)
        } else {
            self.start
                .wall_deadline_ms
                .saturating_add(self.person_wait_ms)
                .saturating_sub(self.shared.clock.now_ms())
        };
        let sleep = self.sleeps.first().map_or(i64::MAX, |(due, _)| {
            due.saturating_sub(self.shared.clock.now_ms())
        });
        let remaining = deadline.min(sleep);
        // A wait of zero means something is due, but a countdown that reaches
        // zero does not: `wall` decides the deadline after adding the
        // person-wait allowance back, so nothing may actually be due. A
        // zero-length receive returns at once without blocking on the events,
        // so the driver needs this floor to keep waiting either way.
        let wait = Duration::from_millis(remaining.max(1) as u64);
        #[cfg(test)]
        {
            let hook = self.shared.before_wait.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook(&Parked {
                    wakes: self.shared.wakes.load(Ordering::SeqCst),
                    wait,
                    completion_pending: self.completion.is_some(),
                });
            }
        }
        let event = self.events.recv_timeout(wait).ok();
        #[cfg(test)]
        self.shared.wakes.fetch_add(1, Ordering::SeqCst);
        event
    }

    /// A manual clock does not move with real time, so a driver sleeping
    /// until its deadline must also wake when the clock is moved.
    fn watch_clock(&mut self) {
        let events = self.event_tx.clone();
        let pending = self.clock_moved.clone();
        let watcher: Watcher = Arc::new(move || {
            if !pending.swap(true, Ordering::SeqCst) {
                let _ = events.send(Event::ClockMoved);
            }
        });
        self.shared.clock.watch(&watcher);
        self.clock_watch = Some(watcher);
    }

    /// Moves the worker's receive side to a thread of its own, which
    /// forwards each frame into the driver's event channel. The driver keeps
    /// the channel's send side, so deliveries and the kill never wait on a
    /// blocked receive.
    fn spawn_reader(&mut self) -> Result<()> {
        let receiver = self.worker.as_mut().unwrap().receiver();
        let events = self.event_tx.clone();
        let (credit, credits) = mpsc::channel();
        #[cfg(test)]
        let before_exit = self.shared.before_reader_exit.lock().unwrap().clone();
        match thread::Builder::new()
            .name(format!("{}:reader", self.key()))
            .spawn(move || {
                read_worker(receiver, events, credits);
                #[cfg(test)]
                if let Some(hook) = before_exit {
                    hook();
                }
            }) {
            Ok(handle) => {
                self.reader = Some(handle);
                self.credit = Some(credit);
                Ok(())
            }
            Err(error) => self.end(failed("worker_lost", error.to_string())),
        }
    }

    /// Waits for the reader thread once the run has ended. The reader is
    /// blocked either on the worker, which every ending kills, or on its next
    /// credit, whose sender is dropped here.
    fn stop_reader(&mut self) {
        self.credit = None;
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            tracing::error!("codemode worker reader panicked");
        }
    }

    fn start_intake(&self, position: u64) -> Result<()> {
        if let Some((module, _)) = self.tool_calls.get(&position) {
            self.shared
                .custody
                .ensure_worker(module, self.shared.transport.clone())?;
        }
        Ok(())
    }

    fn mark_held(&mut self, position: u64, held: bool) -> Result<()> {
        self.account_time();
        let entered = self.shared.store.read(|conn| Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM codemode_calls WHERE run_id=?1 AND position=?2 AND entered_at IS NOT NULL AND outcome='pending')",
            rusqlite::params![self.run.run_id,position],|row| row.get::<_,bool>(0))?))?;
        if self.flight_positions.contains(&position) && entered {
            if let Some((module, _)) = self.tool_calls.get(&position) {
                self.shared
                    .custody
                    .mark_held(module, &self.run.run_id, position, held);
            }
            if held {
                // Every held call stays in custody until its late result
                // arrives and is recorded as held, which is what lets it be
                // withdrawn. Only a person wait is also listed for the
                // person, recorded as one, and able to stop the wall clock.
                self.held_positions.insert(position);
                self.hold_reasons
                    .entry(position)
                    .or_insert_with(|| held_time::Reason {
                        reason: None,
                        since_ms: self.shared.clock.now_ms(),
                    });
                let person = self.waits_on_person(position);
                self.shared.store.write(|tx| {
                    tx.execute("UPDATE codemode_calls SET held=1, waited_on_person=MAX(waited_on_person, ?3) WHERE run_id=?1 AND position=?2 AND outcome='pending'", rusqlite::params![self.run.run_id, position, person])?;
                    Ok(())
                })?;
                if !person {
                    self.forget_hold(position);
                    return Ok(());
                }
                if let Some((_, call)) = self.tool_calls.get(&position)
                    && let CallKind::Tool { name } = &call.kind
                {
                    self.shared
                        .holds
                        .lock()
                        .unwrap()
                        .entry(self.run.run_id.clone())
                        .or_default()
                        .insert(
                            position,
                            Hold {
                                index: self.tool_numbers.get(&position).map_or(position, |n| n - 1),
                                tool: name.clone(),
                                reason: self
                                    .hold_reasons
                                    .get(&position)
                                    .and_then(|r| r.reason.clone()),
                            },
                        );
                }
            } else {
                self.held_positions.remove(&position);
                self.hold_reasons.remove(&position);
                self.forget_hold(position);
            }
        }
        Ok(())
    }

    /// Whether a held call is waiting on a person. A provider's held reason
    /// decides: the tool-provider contract names the reasons that mean a
    /// person must act (`consent`, `login_handoff`, `login_capture`). Any
    /// other reason, including `running`, is treated as a hold without a
    /// person, so the call spends ordinary wall time. A hold with no
    /// recorded reason is a person wait: `Supervisor::held` marks a person's
    /// approval, and a late result that overtakes its held reply proves a
    /// hold without saying why.
    fn waits_on_person(&self, position: u64) -> bool {
        self.hold_reasons
            .get(&position)
            .and_then(|record| record.reason.as_deref())
            .is_none_or(cortexkit_role_tool_provider::result::is_person_wait)
    }

    fn forget_hold(&self, position: u64) {
        let mut holds = self.shared.holds.lock().unwrap();
        if let Some(run) = holds.get_mut(&self.run.run_id) {
            run.remove(&position);
            if run.is_empty() {
                holds.remove(&self.run.run_id);
            }
        }
    }

    fn custody_scope(&self) -> cortexkit_role_tool_provider::scope::ScopeIdentity {
        let scope = &self.start.scope.selector;
        let owner = match &scope.owner {
            subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
            subc_protocol::Principal::Direct => "direct".into(),
            subc_protocol::Principal::Unverified => "unverified".into(),
        };
        cortexkit_role_tool_provider::scope::ScopeIdentity {
            owner,
            scope_ref: scope.scope_ref.clone(),
            scope_epoch: scope.epoch,
        }
    }

    fn custody_options(&self) -> basal_host::transport::RunToolOptions {
        basal_host::transport::RunToolOptions {
            identity: self.identity.clone().unwrap_or_else(|| {
                subc_protocol::BindIdentity::new("/", "basal", format!("basal:flow:{}", self.key()))
            }),
            reply_timeout: Duration::from_secs(2),
            // Custody requests never carry a preset.
            preset: None,
        }
    }

    fn withdraw_held(&mut self) -> Result<()> {
        if self.scope_lost {
            return Ok(());
        }
        let mut calls = Vec::new();
        for position in &self.held_positions {
            let Some((module, call)) = self.tool_calls.get(position) else {
                continue;
            };
            calls.push((*position, module.clone(), self.result_format(call)));
        }
        let results = thread::scope(|threads| {
            let mut pending = Vec::new();
            for (position, module, format) in calls {
                let transport = self.shared.transport.clone();
                let custody_route = self.key();
                let options = self.custody_options();
                let request = cortexkit_role_tool_provider::withdraw::WithdrawArguments::new(
                    store::call_key(&self.run.run_id, position),
                )
                .with_scope(self.custody_scope());
                pending.push((
                    position,
                    threads.spawn(move || {
                        held::withdraw(
                            transport.as_ref(),
                            &custody_route,
                            &module,
                            request,
                            options,
                            format.as_ref(),
                        )
                    }),
                ));
            }
            pending
                .into_iter()
                .map(|(position, reply)| {
                    (
                        position,
                        reply.join().unwrap_or_else(|_| {
                            Mapped::rejected(
                                Outcome::OutcomeUnknown,
                                "no_outcome",
                                "withdrawal worker failed",
                            )
                        }),
                    )
                })
                .collect::<Vec<_>>()
        });
        for (position, mapped) in results {
            self.shared.store.write(|tx| {
                store::record_outcome(
                    tx,
                    &self.run.run_id,
                    position,
                    mapped.outcome,
                    mapped.code.as_deref(),
                    self.shared.clock.now_ms(),
                )
            })?;
        }
        Ok(())
    }

    fn result_format(
        &self,
        call: &HostCall,
    ) -> Option<cortexkit_role_tool_provider::result::ResultFormat> {
        let CallKind::Tool { name } = &call.kind else {
            return None;
        };
        self.start
            .tools
            .get(name)
            .and_then(|tool| tool.result_format.clone())
    }

    fn answer(&mut self, mut answer: Answer) -> Result<()> {
        if !self.flight_positions.contains(&answer.call.position) {
            return Ok(());
        }
        if let Ok(value) = answer.result {
            let format = self.result_format(&answer.call);
            match held::classify(value, format.as_ref()) {
                held::Reply::Held { reason, detail } => {
                    let _ = detail;
                    if self.wall()? {
                        return Ok(());
                    }
                    self.account_time();
                    let record = self
                        .hold_reasons
                        .entry(answer.call.position)
                        .or_insert_with(|| held_time::Reason {
                            reason: None,
                            since_ms: self.shared.clock.now_ms(),
                        });
                    if record.reason.is_none() {
                        record.reason = Some(reason);
                    }
                    self.mark_held(answer.call.position, true)?;
                    self.start_intake(answer.call.position)?;
                    return Ok(());
                }
                held::Reply::Final(result) => answer.result = result,
            }
        }
        self.settle_answer(answer, None)
    }

    fn late_answer(
        &mut self,
        call: HostCall,
        entry: std::result::Result<cortexkit_role_tool_provider::late_results::LateEntry, String>,
    ) -> Result<()> {
        let entry = match entry {
            Ok(entry) => entry,
            Err(message) => return self.record_malformed_entry(&message),
        };
        let Some((provider, _)) = self.tool_calls.get(&call.position) else {
            return Ok(());
        };
        if !self.late_entries.insert(entry.intake_key(provider)) {
            return Ok(());
        }
        let settled_at = i64::try_from(entry.settled_at).unwrap_or(i64::MAX);
        let format = self.result_format(&call);
        match held::map_entry(entry, format.as_ref()) {
            Some(held::Update::Held(held)) => {
                self.mark_held(call.position, true)?;
                self.update_hold_reason(call.position, held.reason, settled_at)?;
            }
            Some(held::Update::Malformed(message)) => self.record_malformed_entry(&message)?,
            Some(held::Update::Final(mapped)) => {
                self.mark_held(call.position, true)?;
                self.mark_held(call.position, false)?;
                self.settle_answer(
                    Answer {
                        call,
                        result: Ok(Value::Null),
                        usage: None,
                        no_effect: mapped.outcome == Outcome::Withdrawn,
                    },
                    Some(mapped),
                )?;
            }
            None => {}
        }
        Ok(())
    }

    fn update_hold_reason(&mut self, position: u64, reason: String, settled_at: i64) -> Result<()> {
        self.account_time();
        let now = self.last_tick_ms;
        let Some(previous) = self.hold_reasons.get(&position) else {
            return Ok(());
        };
        // The provider's late-results cursor orders reason changes. Clamp each
        // timestamp to the current hold's start and the local clock so
        // an update cannot refund earlier holds or future time.
        let since_ms = settled_at.min(now).max(previous.since_ms);
        let person = cortexkit_role_tool_provider::result::is_person_wait(&reason);
        self.person_time.revise(position, since_ms, person);
        self.person_wait_ms = self.person_time.total();
        self.hold_reasons.get_mut(&position).unwrap().reason = Some(reason);
        self.mark_held(position, true)?;
        // Recovery interrupts runs rather than resuming their calls. It only
        // needs the accrued total and the existing held/waited_on_person bits.
        self.shared.store.write(|tx| {
            tx.execute(
                "UPDATE codemode_runs SET person_wait_ms=?2 WHERE run_id=?1 AND status='running'",
                rusqlite::params![self.run.run_id, self.person_wait_ms],
            )?;
            Ok(())
        })
    }

    fn record_malformed_entry(&mut self, message: &str) -> Result<()> {
        let mut warnings: Vec<Value> = serde_json::from_str(&self.run.warnings)
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        if warnings.len() < 256 && self.run.warnings.len() < 65_536 {
            warnings.push(json!({"code":"reply_unreadable","message":format!("skipped malformed late entry: {}", diagnostic_text(message, 1024))}));
            self.run.warnings = serde_json::to_string(&warnings).unwrap();
            self.shared.store.write(|tx| {
                tx.execute(
                    "UPDATE codemode_runs SET warnings=?2 WHERE run_id=?1 AND status='running'",
                    rusqlite::params![self.run.run_id, self.run.warnings],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    fn settle_answer(&mut self, answer: Answer, mapped: Option<Mapped>) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = self.shared.before_record.lock().unwrap().as_ref() {
            hook();
        }
        self.account_time();
        if !self.flight_positions.contains(&answer.call.position) {
            return Ok(());
        }
        self.in_flight -= 1;
        self.flight_positions.remove(&answer.call.position);
        self.held_positions.remove(&answer.call.position);
        self.forget_hold(answer.call.position);
        self.hold_reasons.remove(&answer.call.position);
        if let Some((module, _)) = self.tool_calls.remove(&answer.call.position) {
            self.shared
                .custody
                .remove(&module, &self.run.run_id, answer.call.position);
        }
        let cost_value = answer
            .result
            .as_ref()
            .ok()
            .and_then(|value| value.get("cost_usd"))
            .map(|cost| json!({"cost_usd":cost}));
        let mapped = mapped.unwrap_or_else(|| map_answer(answer.result));
        let cost_value = cost_value.or_else(|| {
            mapped
                .value
                .as_ref()
                .and_then(|value| serde_json::from_str::<Value>(value.as_str()).ok())
        });
        // A provider refusal's spend marker lives in the detail layer that
        // `deliver` adds later, so settle reads it from there rather than from
        // a reply value that has none.
        let cost_value = cost_value.or_else(|| {
            mapped
                .detail
                .as_ref()
                .map(|detail| json!({"detail": detail}))
        });
        if self.shared.store.write(|tx| {
            crate::classify_cost::settle(
                tx,
                "program",
                &self.run.run_id,
                answer.call.position,
                cost_value.as_ref(),
                mapped.code.as_deref(),
            )?;
            super::models::settle(
                tx,
                &self.run.run_id,
                answer.call.position,
                answer.usage,
                answer.no_effect,
            )?;
            store::record_outcome(
                tx,
                &self.run.run_id,
                answer.call.position,
                mapped.outcome,
                mapped.code.as_deref(),
                self.shared.clock.now_ms(),
            )
        })? {
            if mapped.scope_loss {
                self.scope_lost = true;
                return self.end(interrupted(
                    mapped.code.as_deref().unwrap(),
                    &mapped.message,
                ));
            }
            if self.wall()? {
                return Ok(());
            }
            self.deliver(&answer.call, mapped)?;
        }
        Ok(())
    }

    fn model_answer(
        &mut self,
        position: u64,
        result: std::result::Result<basal_host::HostOutcome, WireError>,
    ) -> Result<()> {
        if !self.flight_positions.contains(&position) {
            return Ok(());
        }
        let name: String = self.shared.store.read(|c| {
            Ok(c.query_row(
                "SELECT tool FROM codemode_calls WHERE run_id=?1 AND position=?2",
                rusqlite::params![self.run.run_id, position],
                |r| r.get(0),
            )?)
        })?;
        let (usage, no_effect, result) = match result {
            Ok(outcome) => {
                let value: Value = serde_json::from_str(outcome.value.as_str())
                    .map_err(|e| CoreError::Invalid(e.to_string()))?;
                let result = if outcome.settlement == Settlement::Fulfilled {
                    Ok(value)
                } else {
                    Err(WireError::RefusedDetails {
                        code: value["code"].as_str().unwrap_or("model_error").into(),
                        message: value["message"]
                            .as_str()
                            .unwrap_or("model call failed")
                            .into(),
                        detail: value,
                    })
                };
                (outcome.usage, false, result)
            }
            Err(error) => (None, error.unknown_reason().is_none(), Err(error)),
        };
        self.answer(Answer {
            call: HostCall {
                position,
                kind: CallKind::Tool { name },
                args: JsonText::null(),
            },
            result,
            usage,
            no_effect,
        })
    }

    fn cancel(&mut self, ack: mpsc::SyncSender<Result<()>>) -> Result<()> {
        if self.wall()? {
            let _ = ack.send(Ok(()));
            return Ok(());
        }
        let result = self.end(terminal(Status::Cancelled, None, None));
        let failed = result.is_err();
        let _ = ack.send(result);
        if failed {
            return Err(CoreError::Invalid("cancel could not commit".into()));
        }
        Ok(())
    }

    fn account_time(&mut self) {
        let now = self.shared.clock.now_ms();
        if !self.stopping && self.blocked && self.sleeps.is_empty() && self.in_flight > 0 {
            let people = self
                .held_positions
                .iter()
                .copied()
                .filter(|position| self.waits_on_person(*position))
                .collect();
            self.person_time.tick(
                self.last_tick_ms,
                now,
                self.flight_positions.clone(),
                people,
            );
            self.person_wait_ms = self.person_time.total();
        }
        self.last_tick_ms = now;
    }

    fn wall(&mut self) -> Result<bool> {
        self.account_time();
        let now = self.shared.clock.now_ms();
        if !self.stopping {
            if self.person_wait_ms >= basal_host::run_scope::PERSON_WAIT_MS {
                self.end(exhausted(Budget::PersonWait))?;
            } else if now
                >= self
                    .start
                    .wall_deadline_ms
                    .saturating_add(self.person_wait_ms)
            {
                self.end(exhausted(Budget::Wall))?;
            }
        }
        Ok(self.stopping)
    }

    /// Whether the blocked program is waiting only on people. A pending sleep is the
    /// program's own wait, so it keeps the time on the wall budget.
    fn person_only(&self) -> bool {
        if !self.sleeps.is_empty() {
            return false;
        }
        let person_holds = self
            .held_positions
            .iter()
            .filter(|position| self.waits_on_person(**position))
            .count();
        self.blocked && self.in_flight > 0 && person_holds == self.in_flight
    }

    /// Starts a program sleep. It is not a tool call: it has no call row and
    /// counts against neither the call budget nor the worker's CPU budget,
    /// only the run's wall time, which `wall` checks before any sleep is
    /// woken, so a sleep running past the deadline ends the run there.
    fn sleep(&mut self, call: HostCall) -> Result<()> {
        let ms = serde_json::from_str::<Value>(call.args.as_str())
            .ok()
            .and_then(|value| value.as_u64())
            .filter(|ms| *ms <= MAX_SLEEP_MS);
        let Some(ms) = ms else {
            return self.end(failed(
                "profile_violation",
                format!("sleep takes an integer from 0 to {MAX_SLEEP_MS} milliseconds"),
            ));
        };
        let due = self.shared.clock.now_ms().saturating_add(ms as i64);
        self.sleeps.insert((due, call.position));
        Ok(())
    }

    /// Completes every sleep whose time has come, earliest first.
    fn wake_sleepers(&mut self) -> Result<()> {
        let now = self.shared.clock.now_ms();
        while let Some(&(due, position)) = self.sleeps.first() {
            if due > now || self.stopping {
                break;
            }
            self.sleeps.pop_first();
            self.blocked = false;
            #[cfg(test)]
            self.shared.worker_blocked.store(false, Ordering::SeqCst);
            self.delivery_order += 1;
            if let Err(error) =
                self.worker
                    .as_mut()
                    .unwrap()
                    .send(&ParentMessage::Deliver(Delivery {
                        position,
                        settlement: Settlement::Fulfilled,
                        value: JsonText::null(),
                        delivery_order: self.delivery_order,
                    }))
            {
                self.end(failed("worker_lost", error.to_string()))?;
            }
        }
        Ok(())
    }

    fn message(&mut self, message: WorkerMessage) -> Result<()> {
        self.blocked = matches!(message, WorkerMessage::Blocked { .. });
        #[cfg(test)]
        self.shared
            .worker_blocked
            .store(self.blocked, Ordering::SeqCst);
        match message {
            WorkerMessage::Warning { message } => {
                let mut warnings: Vec<Value> = serde_json::from_str(&self.run.warnings)
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                if warnings.len() < 256 && self.run.warnings.len() < 65_536 {
                    warnings.push(json!({"code":"unhandled_rejection","message":message}));
                    self.run.warnings = serde_json::to_string(&warnings).unwrap();
                    self.shared.store.write(|tx| {
                        tx.execute("UPDATE codemode_runs SET warnings=?2 WHERE run_id=?1 AND status='running'",rusqlite::params![self.run.run_id,self.run.warnings])?;
                        Ok(())
                    })?;
                }
                Ok(())
            }
            WorkerMessage::Console { line } => self.console(line),
            WorkerMessage::HostCall(call) if self.completion.is_none() => self.call(call),
            WorkerMessage::HostCall(_) => self.end(failed(
                "engine_error",
                "worker called after reporting completion",
            )),
            WorkerMessage::Blocked { .. } => {
                #[cfg(test)]
                if let Some(hook) = self.shared.after_block.lock().unwrap().clone() {
                    hook();
                }
                // Keep this worker alive until a provider reply can settle its promises.
                Ok(())
            }
            WorkerMessage::Finished {
                activation_id: 1,
                result,
            } => {
                let terminal = activation_end(result, &self.rejections);
                if terminal.status == Status::Completed {
                    self.completion = Some(terminal);
                    Ok(())
                } else {
                    self.end(terminal)
                }
            }
            _ => self.end(failed("engine_error", "unexpected worker frame")),
        }
    }

    fn console(&mut self, line: String) -> Result<()> {
        if !self.output_truncated
            && self.run.output.len().saturating_add(line.len())
                <= self.start.limits.output_bytes as usize
        {
            self.run.output.push_str(&line);
        } else if !self.output_truncated {
            self.output_truncated = true;
            // Appended, so warnings recorded earlier (such as tools dropped
            // at admission) stay in the result.
            let mut warnings: Vec<Value> = serde_json::from_str(&self.run.warnings)
                .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            warnings.push(json!({"code":"output_truncated", "message":"console output exceeded the byte limit"}));
            self.run.warnings = serde_json::to_string(&warnings).unwrap();
        }
        self.shared.store.write(|tx| {
            tx.execute("UPDATE codemode_runs SET output = ?2, warnings = ?3 WHERE run_id = ?1 AND status = 'running'",
                rusqlite::params![self.run.run_id, self.run.output, self.run.warnings])?;
            Ok(())
        })
    }

    fn call(&mut self, call: HostCall) -> Result<()> {
        if call.kind.is_sleep() {
            return self.sleep(call);
        }
        let CallKind::Tool { name } = &call.kind else {
            return self.end(failed(
                "profile_violation",
                "codemode accepts only tool calls",
            ));
        };
        self.invocations += 1;
        self.tool_numbers.insert(call.position, self.invocations);
        self.shared.store.write(|tx| {
            tx.execute(
                "UPDATE codemode_runs SET tool_call_count=?2 WHERE run_id=?1 AND status='running'",
                rusqlite::params![self.run.run_id, self.invocations],
            )?;
            Ok(())
        })?;
        if self.invocations > self.start.limits.tool_calls {
            return self.end(exhausted(Budget::ToolCalls));
        }
        if let Some(dropped) = self.start.unavailable.get(name) {
            // The daemon refused the provider for this run when the run
            // started: the program sees that refusal with its own code and
            // message, as it would from a call the provider refused.
            if let Some(refusal) = &dropped.refusal {
                let mut mapped =
                    Mapped::rejected(Outcome::Refused, &refusal.code, &refusal.message);
                mapped.detail = Some(json!({"module":dropped.module}));
                self.insert(
                    &call,
                    CallStart::Settled {
                        outcome: Outcome::Refused,
                        code: Some(&refusal.code),
                    },
                )?;
                return self.deliver(&call, mapped);
            }
            let mut mapped = Mapped::rejected(
                Outcome::ToolUnavailable,
                "tool_unavailable",
                &dropped.message,
            );
            mapped.detail = Some(json!({"reason":dropped.reason,"module":dropped.module}));
            self.insert(
                &call,
                CallStart::Settled {
                    outcome: Outcome::ToolUnavailable,
                    code: Some("tool_unavailable"),
                },
            )?;
            return self.deliver(&call, mapped);
        }
        let Some(tool) = self.start.tools.get(name) else {
            return self.refuse(call, Outcome::Refused, "unknown_tool", true);
        };
        let mut input: Value = match serde_json::from_str(call.args.as_str()) {
            Ok(value) => value,
            Err(_) => {
                let mapped = invalid_input(name, &tool.input_schema, None);
                return self.refuse_input(call, mapped);
            }
        };
        if tool.classify_cost && input.is_object() && input.get("max_cost_usd").is_none() {
            input["max_cost_usd"] = 0.into();
        }
        if !tool.input_schema.is_valid(&input) {
            let mapped = invalid_input(name, &tool.input_schema, Some(&input));
            return self.refuse_input(call, mapped);
        }
        if self.shared.transport.catalog().is_err() {
            return self.refuse(call, Outcome::ToolUnavailable, "tool_unavailable", true);
        }
        let declaration = self.shared.catalog.op(&tool.module, &tool.op);
        if !super::models::is_local(&tool.module, &tool.op) && declaration.is_none() {
            return self.refuse(call, Outcome::Refused, "not_in_catalog", true);
        }
        if self.in_flight >= IN_FLIGHT || !self.queue.is_empty() {
            if self.queued_bytes.saturating_add(call.args.as_str().len()) > QUEUED_BYTES {
                return self.refuse(call, Outcome::Refused, "queue_full", false);
            }
            self.insert(&call, CallStart::Queued)?;
            self.queued_bytes += call.args.as_str().len();
            self.queue.push_back(call);
            Ok(())
        } else {
            self.dispatch(call, false)
        }
    }

    fn insert(&self, call: &HostCall, start: CallStart<'_>) -> Result<()> {
        let CallKind::Tool { name } = &call.kind else {
            unreachable!()
        };
        self.shared.store.write(|tx| {
            store::insert_call(
                tx,
                &self.run.run_id,
                call.position,
                name,
                call.args.as_str().len() as u64,
                start,
            )?;
            Ok(())
        })
    }

    fn refuse(&mut self, call: HostCall, outcome: Outcome, code: &str, row: bool) -> Result<()> {
        if row {
            self.insert(
                &call,
                CallStart::Settled {
                    outcome,
                    code: Some(code),
                },
            )?;
        }
        self.deliver(&call, Mapped::rejected(outcome, code, code))
    }

    fn refuse_input(&mut self, call: HostCall, mapped: Mapped) -> Result<()> {
        self.insert(
            &call,
            CallStart::Settled {
                outcome: Outcome::Refused,
                code: Some("invalid_input"),
            },
        )?;
        self.deliver(&call, mapped)
    }

    fn dispatch(&mut self, mut call: HostCall, queued: bool) -> Result<()> {
        if self.wall()? {
            return Ok(());
        }
        let CallKind::Tool { name } = &call.kind else {
            unreachable!()
        };
        let tool = &self.start.tools[name];
        let local_model = super::models::is_local(&tool.module, &tool.op);
        if local_model {
            return self.dispatch_model(call, queued);
        }
        if let Err(error) =
            self.shared
                .transport
                .provider_ready(&self.key(), &tool.module, &tool.op)
        {
            if queued {
                self.shared.store.write(|tx| {
                    store::record_outcome(
                        tx,
                        &self.run.run_id,
                        call.position,
                        Outcome::ToolUnavailable,
                        Some("tool_unavailable"),
                        self.shared.clock.now_ms(),
                    )
                })?;
                return self.deliver(
                    &call,
                    Mapped::rejected(
                        Outcome::ToolUnavailable,
                        "tool_unavailable",
                        error.message(),
                    ),
                );
            }
            return self.refuse(call, Outcome::ToolUnavailable, "tool_unavailable", true);
        }
        let module = tool.module.clone();
        let op = tool.op.clone();
        let classify_cost = tool.classify_cost;
        if self.wall()? {
            return Ok(());
        }
        if !queued {
            self.insert(&call, CallStart::Queued)?;
        }
        if classify_cost {
            let mut input: Value =
                serde_json::from_str(call.args.as_str()).expect("validated input");
            let prepared = self.shared.store.write(|tx| {
                crate::classify_cost::reserve(
                    tx,
                    "program",
                    &self.run.run_id,
                    call.position,
                    None,
                    self.shared.clock.now_ms(),
                    self.start.limits.model_cost_usd,
                    None,
                    &mut input,
                )
            })?;
            if let Err(error) = prepared {
                let mapped = map_answer(Err(error));
                self.shared.store.write(|tx| {
                    store::record_outcome(
                        tx,
                        &self.run.run_id,
                        call.position,
                        mapped.outcome,
                        mapped.code.as_deref(),
                        self.shared.clock.now_ms(),
                    )
                })?;
                return self.deliver(&call, mapped);
            }
            call.args = JsonText::new(input.to_string()).expect("bounded classify input");
        }
        let store = DispatchStore::new(
            self.shared.store.clone(),
            self.shared.dispatch_starts.clone(),
        );
        let id = self.run.run_id.clone();
        let clock = self.shared.clock.clone();
        let deadline = self
            .start
            .wall_deadline_ms
            .saturating_add(self.person_wait_ms);
        let key = store::call_key(&self.run.run_id, call.position);
        let route = self.key();
        let transport = self.shared.transport.clone();
        let options = basal_host::transport::RunToolOptions {
            identity: self.identity.clone().unwrap_or_else(|| {
                subc_protocol::BindIdentity::new("/", "basal", format!("basal:flow:{}", self.key()))
            }),
            preset: self.start.presets.get(&module).cloned(),
            // The ck-basal process supervises the separate ck-basal-worker and
            // enforces 30 minutes of wall time, excluding person waits. A person
            // wait means the worker is blocked and every in-flight provider call
            // awaits a person's approval or decision; these intervals have one
            // cumulative 10-minute cap. The supervisor closes the scope at the
            // first cap, so a slow RPC can wait up to the fixed scope expiry
            // without giving the program more execution time.
            reply_timeout: Duration::from_millis(
                self.run
                    .deadline_ms
                    .saturating_sub(self.shared.clock.now_ms())
                    .max(1) as u64,
            ),
        };
        let events = self.event_tx.clone();
        #[cfg(test)]
        let before_entry = self.shared.before_entry.lock().unwrap().clone();
        self.in_flight += 1;
        self.flight_positions.insert(call.position);
        self.shared.custody.register(
            &module,
            held::Target {
                run: id.clone(),
                scope: self.custody_scope(),
                call: call.clone(),
                events: events.clone(),
                options: options.clone(),
                held: false,
            },
        );
        self.tool_calls
            .insert(call.position, (module.clone(), call.clone()));

        if let Err(error) = thread::Builder::new()
            .name(format!("{route}:tool"))
            .spawn(move || {
                let input = serde_json::from_str(call.args.as_str()).expect("validated input");
                // Record the permission to send on this dispatch thread, rather
                // than when the supervisor schedules it. Cancellation settles rows
                // without that permission as cancelled; the conditional write then
                // prevents a thread scheduled too late from invoking the provider.
                let now = clock.now_ms();
                if now >= deadline {
                    return;
                }
                let prepared = store.write(|tx| store::prepare_queued(tx, &id, call.position, now));
                let result = match prepared {
                    Ok(true) => {
                        #[cfg(test)]
                        if let Some(hook) = before_entry {
                            hook();
                        }
                        let entered = store
                            .write(|tx| store::enter_call(tx, &id, call.position, clock.now_ms()));
                        match entered {
                            Ok(true) => {
                                // A blocked provider must not keep the module's
                                // SQLite writer lease alive after shutdown.
                                drop(store);
                                transport.tool_for_run(&route, &module, &op, input, &key, &options)
                            }
                            Ok(false) => return,
                            Err(error) => Err(WireError::NeverSent(error.to_string())),
                        }
                    }
                    Ok(false) => return,
                    Err(error) => Err(WireError::NeverSent(error.to_string())),
                };
                let no_effect = result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.unknown_reason().is_none());
                let _ = events.send(Event::Answer(Answer {
                    call,
                    result,
                    usage: None,
                    no_effect,
                }));
            })
        {
            return self.end(failed("worker_lost", error.to_string()));
        }
        Ok(())
    }

    fn dispatch_model(&mut self, call: HostCall, queued: bool) -> Result<()> {
        if !queued {
            self.insert(&call, CallStart::Queued)?;
        }
        self.in_flight += 1;
        self.flight_positions.insert(call.position);
        let ready = super::models::provider_ready(self.shared.transport.as_ref());
        let models = self.shared.models.lock().unwrap().clone();
        let models = match (ready, models) {
            (Err(error), _) => return self.model_answer(call.position, Err(error)),
            (_, None) => {
                return self.model_answer(
                    call.position,
                    Err(WireError::NeverSent("model host unavailable".into())),
                );
            }
            (Ok(()), Some(models)) => models,
        };
        let store = DispatchStore::new(
            self.shared.store.clone(),
            self.shared.dispatch_starts.clone(),
        );
        let run = self.run.clone();
        let events = self.event_tx.clone();
        let clock = self.shared.clock.clone();
        thread::Builder::new()
            .name(format!("{}:model", self.key()))
            .spawn(move || {
                let result = (|| {
                    let request =
                        super::models::prepare(&store, models.selector.as_ref(), &run, &call)?;
                    let entered = store
                        .write(|tx| {
                            if !store::prepare_queued(
                                tx,
                                &run.run_id,
                                call.position,
                                clock.now_ms(),
                            )? {
                                return Ok(false);
                            }
                            store::enter_call(tx, &run.run_id, call.position, clock.now_ms())
                        })
                        .map_err(|e| WireError::NeverSent(e.to_string()))?;
                    drop(store);
                    if !entered {
                        return Ok(None);
                    }
                    let result = models.host.dispatch(&request).map_err(|e| match e {
                        basal_host::TransportError::Refused(refusal) => WireError::Typed(refusal),
                        basal_host::TransportError::Unavailable {
                            sent: basal_host::Sent::Never,
                            detail,
                        } => WireError::NeverSent(detail),
                        other => WireError::Unknown(other.to_string()),
                    })?;
                    match result {
                        basal_host::Dispatched::Completed(outcome) => {
                            models.host.dispatch_committed(&request);
                            Ok(Some(outcome))
                        }
                        basal_host::Dispatched::Accepted { .. } => {
                            models.host.dispatch_committed(&request);
                            Ok(None)
                        }
                    }
                })();
                let result = match result {
                    Ok(Some(outcome)) => Ok(outcome),
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                let _ = events.send(Event::ModelAnswer {
                    position: call.position,
                    result,
                });
            })
            .map_err(|e| CoreError::Invalid(e.to_string()))?;
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        while self.in_flight < IN_FLIGHT && !self.queue.is_empty() {
            if self.wall()? {
                break;
            }
            let call = self.queue.pop_front().unwrap();
            self.queued_bytes -= call.args.as_str().len();
            self.dispatch(call, true)?;
        }
        Ok(())
    }

    fn deliver(&mut self, call: &HostCall, mapped: Mapped) -> Result<()> {
        self.blocked = false;
        #[cfg(test)]
        self.shared.worker_blocked.store(false, Ordering::SeqCst);
        let CallKind::Tool { name } = &call.kind else {
            unreachable!()
        };
        let (settlement, value) = if mapped.outcome == Outcome::Ok {
            (Settlement::Fulfilled, mapped.value.unwrap())
        } else {
            let mut rejection = json!({
                "code":mapped.code, "tool":name, "outcome":mapped.outcome.as_str(),
                "message":mapped.message
            });
            self.rejections.insert(
                call.position,
                Rejection {
                    code: mapped.code.clone().unwrap_or_else(|| "tool_error".into()),
                    message: rejection_message(
                        name,
                        self.tool_numbers
                            .get(&call.position)
                            .copied()
                            .unwrap_or(call.position + 1),
                        &mapped.message,
                        mapped.reason.as_deref(),
                    ),
                },
            );
            if let Some(reason) = mapped.reason {
                rejection["reason"] = json!(reason);
            }
            if let Some(detail) = mapped.detail {
                if let Some(budget) = detail.get("budget") {
                    rejection["budget"] = budget.clone();
                }
                rejection["detail"] = detail;
            }
            let rejection = rejection.to_string();
            let value =
                JsonText::new(rejection).map_err(|error| CoreError::Invalid(error.to_string()))?;
            (Settlement::Rejected, value)
        };
        self.delivery_order += 1;
        if let Err(error) = self
            .worker
            .as_mut()
            .unwrap()
            .send(&ParentMessage::Deliver(Delivery {
                position: call.position,
                settlement,
                value,
                delivery_order: self.delivery_order,
            }))
        {
            self.end(failed("worker_lost", error.to_string()))?;
        }
        Ok(())
    }

    fn end(&mut self, mut terminal: Terminal) -> Result<()> {
        self.shared.custody.end(&self.run.run_id);
        self.shared.holds.lock().unwrap().remove(&self.run.run_id);
        self.account_time();
        self.stopping = true;
        self.parent_owned_kill = true;
        terminal.output = self.run.output.clone();
        terminal.warnings = self.run.warnings.clone();
        let accounting = self.shared.store.write(|tx| {
            tx.execute(
                "UPDATE codemode_runs SET person_wait_ms=?2 WHERE run_id=?1 AND status='running'",
                rusqlite::params![self.run.run_id, self.person_wait_ms],
            )?;
            Ok(())
        });
        self.withdraw_held()?;
        let ended = terminate(
            &self.shared,
            &self.run.run_id,
            self.worker.as_deref_mut(),
            terminal,
        );
        ended.and(accounting)
    }
}

fn terminate(
    shared: &Shared,
    id: &str,
    worker: Option<&mut (dyn WorkerChannel + 'static)>,
    mut terminal: Terminal,
) -> Result<()> {
    // The driver has stopped accepting calls and marked the worker kill as
    // parent-initiated. Cancel calls not yet sent, mark unanswered attempts as
    // unknown, then release the registered scope before making the run terminal.
    // Admission counts running rows, so the terminal commit frees its slot.
    if let Err(error) = super::scope::close(&shared.store, shared.transport.as_ref(), id) {
        let mut warnings: Vec<Value> = serde_json::from_str(&terminal.warnings)
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        warnings.push(json!({"code":"scope_close_failed","message":error.to_string()}));
        terminal.warnings = serde_json::to_string(&warnings).unwrap();
    }
    if let Some(worker) = worker {
        worker.kill();
    }
    let at = shared.clock.now_ms();
    shared
        .store
        .write(|tx| store::settle_pending_calls(tx, id, at))?;
    shared
        .transport
        .configure_flow(&format!("codemode:{id}"), None);
    shared
        .store
        .write(|tx| store::commit_terminal(tx, id, &terminal, at))?;
    let hook_result = if let Some(hook) = shared.terminal_hook.lock().unwrap().clone() {
        Supervisor::result_for(&shared.store, &shared.clock, id)?
            .map_or(Ok(()), |result| hook(id, result))
    } else {
        Ok(())
    };
    let (generation, changed) = &shared.terminal_changed;
    *generation.lock().unwrap() += 1;
    changed.notify_all();
    hook_result
}

/// The body of a run's reader thread. It forwards one frame, then waits for
/// the driver's credit before reading the next. It stops at the first channel
/// error, which every kill produces, or once the driver is gone.
fn read_worker(
    mut receiver: Box<dyn WorkerReceiver>,
    events: mpsc::Sender<Event>,
    credits: mpsc::Receiver<()>,
) {
    loop {
        let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receiver.recv()));
        match frame {
            Ok(Ok(message)) => {
                if events.send(Event::Worker(Ok(message))).is_err() || credits.recv().is_err() {
                    return;
                }
            }
            Ok(Err(error)) => {
                let _ = events.send(Event::Worker(Err(error)));
                return;
            }
            Err(_) => {
                let _ = events.send(Event::ReaderPanicked);
                return;
            }
        }
    }
}

// The five errors, two 96-byte name lists, and admission-capped 128-byte tool
// name fit within one KiB, including JSON escaping and structural overhead.
const INPUT_DIAGNOSTIC_BYTES: usize = 1024;
const INPUT_ERRORS: usize = 5;

fn diagnostic_text(text: &str, bytes: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cost = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => c.len_utf8(),
        };
        if used + cost > bytes - '…'.len_utf8() {
            result.push('…');
            return result;
        }
        result.push(c);
        used += cost;
    }
    result
}

fn diagnostic_names(names: &[String]) -> Vec<String> {
    let mut selected = Vec::new();
    for name in names {
        selected.push(name.clone());
        if serde_json::to_string(&selected).unwrap().len() > 96 {
            selected.pop();
            break;
        }
    }
    selected
}

fn invalid_input(name: &str, schema: &InputSchema, input: Option<&Value>) -> Mapped {
    let tool = name;
    let errors: Vec<Value> = match input {
        Some(input) => schema
            .iter_errors(input)
            .take(INPUT_ERRORS)
            .map(|error| {
                json!({
                    "path": diagnostic_text(&error.instance_path().to_string(), 32),
                    "message": diagnostic_text(&error.to_string(), 72),
                })
            })
            .collect(),
        None => vec![json!({"path":"", "message":"arguments are not valid JSON"})],
    };
    let properties = diagnostic_names(&schema.properties);
    let required = diagnostic_names(&schema.required);
    let message = if input.is_some() {
        let errors = errors
            .iter()
            .map(|error| {
                let path = error["path"].as_str().unwrap();
                let message = error["message"].as_str().unwrap();
                if path.is_empty() {
                    message.to_owned()
                } else {
                    format!("{path}: {message}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut names = properties
            .iter()
            .map(|property| {
                if schema.required.contains(property) {
                    format!("{property} (required)")
                } else {
                    property.clone()
                }
            })
            .collect::<Vec<_>>();
        if properties.len() < schema.properties.len() {
            names.push("…".into());
        } else if names.is_empty() {
            names.push("(no top-level properties declared)".into());
        }
        format!(
            "{tool}: invalid input: {errors}. {tool} takes: {}.",
            names.join(", ")
        )
    } else {
        format!("{tool}: arguments are not valid JSON")
    };
    let mut mapped = Mapped::rejected(
        Outcome::Refused,
        "invalid_input",
        &diagnostic_text(&message, INPUT_DIAGNOSTIC_BYTES),
    );
    mapped.detail =
        Some(json!({"tool":tool, "errors":errors, "properties":properties, "required":required}));
    mapped
}

struct Mapped {
    outcome: Outcome,
    code: Option<String>,
    message: String,
    value: Option<JsonText>,
    scope_loss: bool,
    reason: Option<String>,
    detail: Option<Value>,
}

impl Mapped {
    fn rejected(outcome: Outcome, code: &str, message: &str) -> Self {
        Self {
            outcome,
            code: Some(code.into()),
            message: message.into(),
            value: None,
            scope_loss: false,
            reason: None,
            detail: None,
        }
    }
}

fn map_answer(result: std::result::Result<Value, WireError>) -> Mapped {
    match result {
        Ok(value) => match JsonText::new(value.to_string()) {
            Ok(value) => Mapped {
                outcome: Outcome::Ok,
                code: None,
                message: String::new(),
                value: Some(value),
                scope_loss: false,
                reason: None,
                detail: None,
            },
            Err(_) => Mapped::rejected(
                Outcome::Error,
                "value_too_large",
                "provider answer exceeded 1 MiB",
            ),
        },
        Err(WireError::Typed(refusal)) => {
            use RefusalReason::*;
            let (outcome, code, scope_loss) = match refusal.reason {
                ScopeEnded | ScopeNotLive | ScopeNotSynced => {
                    (Outcome::Refused, refusal.reason.as_str(), true)
                }
                // The run scope is still live: its record changed while a bind
                // was in flight. The transport has already re-opened such a
                // bind once, and never resends a request the provider may have
                // received, so this refusal ends only this call.
                ScopeChanged => (Outcome::Refused, refusal.reason.as_str(), false),
                ConsentUnavailable => (Outcome::Refused, "consent_unavailable", false),
                ScopeNotCarrier
                | ScopeEpochRequired
                | ScopeUnsupported
                | TargetFlowUnsupported
                | ResourceBusy
                | ModuleGrantAbsent
                | AgentGrantAbsent
                | NoFlowScope
                | AgentRetired
                | FlowDelegationUnsupported
                | SinkTargetNotInAudience
                | FlowNotApproved
                | SinkAudienceUnavailable
                | SinkStoreFailed
                | SecretVaultUnavailable
                | SinkBoardTargetUnavailable
                | FlowHasNoAudience
                | ScopeExpiryTooFar
                | FlowTargetScopePrincipalRefused
                | FlowScopeRequired => (Outcome::ToolUnavailable, "tool_unavailable", false),
            };
            let mut mapped = Mapped::rejected(outcome, code, refusal.message());
            mapped.scope_loss = scope_loss;
            mapped
        }
        Err(WireError::NeverSent(message)) => {
            Mapped::rejected(Outcome::ToolUnavailable, "tool_unavailable", &message)
        }
        Err(WireError::Unknown(message) | WireError::ScopeCut(message)) => {
            Mapped::rejected(Outcome::OutcomeUnknown, "connection_lost", &message)
        }
        Err(WireError::TimedOut(message)) => {
            Mapped::rejected(Outcome::OutcomeUnknown, "reply_timeout", &message)
        }
        Err(WireError::Unreadable(message)) => {
            Mapped::rejected(Outcome::OutcomeUnknown, "reply_unreadable", &message)
        }
        Err(
            WireError::Refused { code, message } | WireError::RefusedDetails { code, message, .. },
        ) if matches!(
            code.as_str(),
            "scope_ended" | "scope_expired" | "scope_not_synced"
        ) =>
        {
            let mut mapped = Mapped::rejected(Outcome::Refused, &code, &message);
            mapped.scope_loss = true;
            mapped
        }
        Err(WireError::RefusedDetails {
            code,
            message,
            detail,
        }) => {
            // The provider's refusal reaches the program as the provider sent
            // it. A codemode run has no flow, so its refusal must not be turned
            // into basal's flow wording. The whole detail is kept, so a program
            // sees everything the provider said; the run error text still names
            // the payload's own `reason` when it carries one.
            let mut mapped = map_answer(Err(WireError::Refused { code, message }));
            mapped.reason = detail["reason"].as_str().map(str::to_owned);
            mapped.detail = Some(detail);
            mapped
        }
        Err(WireError::Refused { code, message }) => Mapped::rejected(
            if code == "denied" {
                Outcome::Denied
            } else if code == "tool_unavailable" {
                Outcome::ToolUnavailable
            } else if matches!(code.as_str(), "budget_exhausted" | "route_unavailable") {
                Outcome::Refused
            } else {
                Outcome::Error
            },
            &code,
            &message,
        ),
    }
}

fn terminal(status: Status, value: Option<String>, error: Option<RunError>) -> Terminal {
    Terminal {
        status,
        value,
        error,
        output: String::new(),
        warnings: "[]".into(),
    }
}

fn failed(code: &str, message: impl Into<String>) -> Terminal {
    terminal(
        Status::Failed,
        None,
        Some(RunError {
            code: code.into(),
            message: message.into(),
        }),
    )
}

fn interrupted(code: &str, message: impl Into<String>) -> Terminal {
    terminal(
        Status::Interrupted,
        None,
        Some(RunError {
            code: code.into(),
            message: message.into(),
        }),
    )
}

fn exhausted(budget: Budget) -> Terminal {
    let status = Status::BudgetExhausted(budget);
    terminal(
        status,
        None,
        Some(RunError {
            code: status.as_str().into(),
            message: "run budget exhausted".into(),
        }),
    )
}

fn activation_end(result: ActivationResult, rejections: &BTreeMap<u64, Rejection>) -> Terminal {
    match result {
        ActivationResult::Completed { value } => {
            if value.as_str().len() > RESULT_BYTES {
                failed(
                    "result_too_large",
                    format!("return value has {} bytes", value.as_str().len()),
                )
            } else if serde_json::from_str::<Value>(value.as_str()).is_err() {
                failed("result_not_json", "return value is not JSON")
            } else {
                terminal(Status::Completed, Some(value.as_str().into()), None)
            }
        }
        ActivationResult::Stalled => failed("stalled", "program awaits an unsettled promise"),
        ActivationResult::Suspended { .. } => {
            failed("engine_error", "codemode cannot suspend or replay")
        }
        ActivationResult::BudgetExhausted(budget) => exhausted(match budget {
            BudgetKind::JsTime => Budget::JsCpu,
            BudgetKind::Memory => Budget::Memory,
            BudgetKind::Stack => Budget::Stack,
        }),
        ActivationResult::Failed(Failure::ScriptHostRejection { position, .. })
            if rejections.contains_key(&position) =>
        {
            let rejection = &rejections[&position];
            failed(&rejection.code, rejection.message.clone())
        }
        ActivationResult::Failed(failure) => {
            let code = match &failure {
                Failure::ProfileViolation { .. } => "profile_violation",
                Failure::EngineMismatch { .. } => "engine_mismatch",
                Failure::ArgumentsTooLarge { .. } => "arguments_too_large",
                Failure::ResultTooLarge { .. } => "result_too_large",
                Failure::ResultNotSerializable { .. } => "result_not_json",
                Failure::Script { .. } | Failure::ScriptHostRejection { .. } => "script",
                Failure::Nondeterminism(_)
                | Failure::InvalidRequest { .. }
                | Failure::InvalidHostValue { .. }
                | Failure::HostLink { .. }
                | Failure::Engine { .. } => "engine_error",
            };
            failed(code, failure.to_string())
        }
    }
}

/// A tool call's rejection as the program received it.
struct Rejection {
    /// The provider's code, or basal's own when basal refused the call.
    code: String,
    /// One plain-text line naming the tool and the call.
    message: String,
}

/// Provider messages can be long and span lines; the run error keeps one
/// readable line of bounded size.
const REJECTION_MESSAGE_BYTES: usize = 4096;

/// `number` is the call's 1-based number among the run's tool calls.
fn rejection_message(tool: &str, number: u64, message: &str, reason: Option<&str>) -> String {
    let mut text = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(reason) = reason.map(str::trim).filter(|r| !r.is_empty())
        && !text.contains(reason)
    {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str("Reason: ");
        text.push_str(reason);
        if !reason.ends_with(['.', '!', '?']) {
            text.push('.');
        }
    }
    if text.len() > REJECTION_MESSAGE_BYTES {
        let mut end = REJECTION_MESSAGE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    if text.is_empty() {
        format!("{tool} (call {number}) failed.")
    } else {
        format!("{tool} (call {number}) failed: {text}")
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
