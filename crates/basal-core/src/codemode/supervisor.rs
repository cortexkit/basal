//! One blocking supervisor per codemode run. The worker cannot authorize a
//! call, write its outcome, or decide which scope the provider receives.
//!
//! Blocking dispatch lanes belong to the supervisor, not to an async executor.
//! Only the supervisor records answers and delivers them to the worker, so
//! dropping its event receiver also discards answers that arrive after a kill.
//!
//! Each run's driver sleeps on one event channel until something happens:
//! a worker frame (forwarded by the run's reader thread), a provider answer,
//! a cancel, or a move of a manual clock. Its only timeout is the run's wall
//! deadline, so a run waiting on a slow tool costs no CPU.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

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

/// Everything that can wake a run's driver, in the order it happened.
enum Event {
    /// A frame from the worker, or the error that ended its channel.
    Worker(std::result::Result<WorkerMessage, ChannelError>),
    /// The worker's receiver panicked on the reader thread.
    ReaderPanicked,
    /// A provider answer from a dispatch thread.
    Answer(Answer),
    Cancel(mpsc::SyncSender<Result<()>>),
    /// A manual clock moved, so the wall deadline may have passed without
    /// any real time elapsing.
    ClockMoved,
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
    workers: Arc<dyn WorkerSource>,
    clock: Clock,
    prelude_hash: PreludeHash,
    denylist: ShellDenylist,
    active: Mutex<BTreeMap<String, mpsc::Sender<Event>>>,
    drivers: Mutex<Drivers>,
    dispatch_starts: Arc<DispatchStarts>,
    #[cfg(test)]
    before_entry: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_record: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_exit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_reader_exit: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// How many times a driver has returned from its wait for an event.
    #[cfg(test)]
    wakes: std::sync::atomic::AtomicUsize,
}

/// Owns codemode supervisors only. Supply a fresh-worker source, never a flow
/// pool, and the parent's codemode prelude hash, never the worker's claim.
#[derive(Clone)]
pub struct Supervisor(Arc<Shared>);

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
        denylist: ShellDenylist,
    ) -> Result<Self> {
        let supervisor = Self(Arc::new(Shared {
            store,
            transport,
            catalog,
            workers,
            clock,
            prelude_hash,
            denylist,
            active: Mutex::new(BTreeMap::new()),
            drivers: Mutex::new(Drivers::default()),
            dispatch_starts: Arc::new(DispatchStarts::default()),
            #[cfg(test)]
            before_entry: Mutex::new(None),
            #[cfg(test)]
            before_record: Mutex::new(None),
            #[cfg(test)]
            before_exit: Mutex::new(None),
            #[cfg(test)]
            before_reader_exit: Mutex::new(None),
            #[cfg(test)]
            wakes: std::sync::atomic::AtomicUsize::new(0),
        }));
        supervisor.recover()?;
        Ok(supervisor)
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
        let mut drivers = self.0.drivers.lock().unwrap();
        drivers.stopped = true;
        let ids = self
            .0
            .active
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut error = None;
        for id in ids {
            if let Err(failure) = self.cancel(&id) {
                error.get_or_insert(failure);
            }
        }
        for handle in drivers.threads.drain(..) {
            if handle.join().is_err() {
                error.get_or_insert_with(|| CoreError::Invalid("codemode driver panicked".into()));
            }
        }
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
        self.0.store.read(|conn| {
            let Lookup::Found(run) = store::lookup(conn, id)? else { return Ok(None) };
            let calls = store::calls(conn, id)?.into_iter().map(|call| {
                let mut value = json!({"tool": call.tool, "outcome": call.outcome.as_str()});
                if let Some(code) = call.code { value["code"] = code.into(); }
                if let Some(duration) = call.duration_ms { value["duration_ms"] = duration.into(); }
                value
            }).collect::<Vec<_>>();
            let mut value = json!({
                "status": run.status.as_str(), "output": run.output, "calls": calls,
                "warnings": parse(&run.warnings)?, "catalog_digest": run.catalog_digest,
                "duration_ms": run.duration_ms.unwrap_or_else(|| self.0.clock.now_ms().saturating_sub(run.admitted_at).max(0))
            });
            if let Some(result) = run.value { value["value"] = parse(&result)?; }
            if let Some(error) = run.error { value["error"] = json!({"code": error.code, "message": error.message}); }
            if let Some(description) = run.description { value["description"] = description.into(); }
            Ok(Some(value))
        })
    }

    /// Cancellation is serialized with answer recording by the run's driver.
    /// Its acknowledgement follows scope release and the terminal commit.
    pub fn cancel(&self, id: &str) -> Result<Option<Value>> {
        let sender = self.0.active.lock().unwrap().get(id).cloned();
        if let Some(sender) = sender {
            let (tx, rx) = mpsc::sync_channel(1);
            if sender.send(Event::Cancel(tx)).is_ok() {
                match rx.recv() {
                    Ok(result) => result?,
                    Err(_) => {
                        if let Some(value) = self.result(id)?
                            && value["status"] != "running"
                        {
                            return Ok(Some(value));
                        }
                        return Err(CoreError::Invalid("supervisor lost during cancel".into()));
                    }
                }
            }
        }
        self.result(id)
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
        Ok(())
    }
}

fn parse(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|error| CoreError::Corrupt(error.to_string()))
}

struct Answer {
    call: HostCall,
    result: std::result::Result<Value, WireError>,
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
    invocations: u64,
    delivery_order: u64,
    completion: Option<Terminal>,
    stopping: bool,
    parent_owned_kill: bool,
    output_truncated: bool,
}

impl Driver {
    fn new(
        shared: Arc<Shared>,
        run: RunRecord,
        start: Start,
        event_tx: mpsc::Sender<Event>,
        events: mpsc::Receiver<Event>,
    ) -> Self {
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
            invocations: 0,
            delivery_order: 0,
            completion: None,
            stopping: false,
            parent_owned_kill: false,
            output_truncated: false,
        }
    }

    fn key(&self) -> String {
        format!("codemode:{}", self.run.run_id)
    }

    fn drive(&mut self) -> Result<()> {
        self.watch_clock();
        if self.wall()? {
            return Ok(());
        }
        self.shared
            .transport
            .configure_flow(&self.key(), false, Some(self.start.scope.clone()));
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
            tools: self.start.tools.keys().cloned().collect(),
            prelude_hash: self.shared.prelude_hash,
            script: self.run.program.clone(),
            trigger: JsonText::null(),
            self_input: JsonText::null(),
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
            self.drain()?;
            if self.stopping {
                break;
            }
            if self.in_flight == 0
                && self.queue.is_empty()
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

    /// Sleeps until an event arrives or the wall deadline, measured on the
    /// injected clock, comes due. `None` means the deadline came due.
    fn next_event(&mut self) -> Option<Event> {
        let remaining = self
            .start
            .wall_deadline_ms
            .saturating_sub(self.shared.clock.now_ms())
            .max(0);
        let wait = Duration::from_millis(remaining as u64);
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

    fn answer(&mut self, answer: Answer) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = self.shared.before_record.lock().unwrap().as_ref() {
            hook();
        }
        if self.wall()? {
            return Ok(());
        }
        self.in_flight -= 1;
        let mapped = map_answer(answer.result);
        if self.shared.store.write(|tx| {
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

    fn cancel(&mut self, ack: mpsc::SyncSender<Result<()>>) -> Result<()> {
        let result = self.end(terminal(Status::Cancelled, None, None));
        let failed = result.is_err();
        let _ = ack.send(result);
        if failed {
            return Err(CoreError::Invalid("cancel could not commit".into()));
        }
        Ok(())
    }

    fn wall(&mut self) -> Result<bool> {
        if !self.stopping && self.shared.clock.now_ms() >= self.start.wall_deadline_ms {
            self.end(exhausted(Budget::Wall))?;
        }
        Ok(self.stopping)
    }

    fn message(&mut self, message: WorkerMessage) -> Result<()> {
        match message {
            WorkerMessage::Console { line } => self.console(line),
            WorkerMessage::HostCall(call) if self.completion.is_none() => self.call(call),
            WorkerMessage::HostCall(_) => self.end(failed(
                "engine_error",
                "worker called after reporting completion",
            )),
            WorkerMessage::Blocked { .. } => Ok(()), // Keep this worker alive until a provider reply can settle its promises.
            WorkerMessage::Finished {
                activation_id: 1,
                result,
            } => {
                let terminal = activation_end(result);
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
            self.run.warnings = json!([{"code":"output_truncated", "message":"console output exceeded the byte limit"}]).to_string();
        }
        self.shared.store.write(|tx| {
            tx.execute("UPDATE codemode_runs SET output = ?2, warnings = ?3 WHERE run_id = ?1 AND status = 'running'",
                rusqlite::params![self.run.run_id, self.run.output, self.run.warnings])?;
            Ok(())
        })
    }

    fn call(&mut self, call: HostCall) -> Result<()> {
        let CallKind::Tool { name } = &call.kind else {
            return self.end(failed(
                "profile_violation",
                "codemode accepts only tool calls",
            ));
        };
        self.invocations += 1;
        if self.invocations > self.start.limits.tool_calls {
            return self.end(exhausted(Budget::ToolCalls));
        }
        let Some(tool) = self.start.tools.get(name) else {
            return self.refuse(call, Outcome::Refused, "unknown_tool", true);
        };
        let input: Value = match serde_json::from_str(call.args.as_str()) {
            Ok(value) => value,
            Err(_) => return self.refuse(call, Outcome::Refused, "invalid_input", true),
        };
        if !tool.input_schema.is_valid(&input) {
            return self.refuse(call, Outcome::Refused, "invalid_input", true);
        }
        if self.shared.transport.catalog().is_err() {
            return self.refuse(call, Outcome::ToolUnavailable, "tool_unavailable", true);
        }
        let declaration = self.shared.catalog.op(&tool.module, &tool.op);
        if self.shared.denylist.contains(&tool.module, &tool.op)
            || declaration.as_ref().is_some_and(|op| op.shell_capable)
        {
            return self.refuse(call, Outcome::Refused, "shell_capable", true);
        }
        if declaration.is_none() {
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

    fn dispatch(&mut self, call: HostCall, queued: bool) -> Result<()> {
        if self.wall()? {
            return Ok(());
        }
        let CallKind::Tool { name } = &call.kind else {
            unreachable!()
        };
        let tool = &self.start.tools[name];
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
        if self.wall()? {
            return Ok(());
        }
        if !queued {
            self.insert(&call, CallStart::Queued)?;
        }
        let store = DispatchStore::new(
            self.shared.store.clone(),
            self.shared.dispatch_starts.clone(),
        );
        let id = self.run.run_id.clone();
        let clock = self.shared.clock.clone();
        let deadline = self.start.wall_deadline_ms;
        let key = store::call_key(&self.run.run_id, call.position);
        let route = self.key();
        let transport = self.shared.transport.clone();
        let events = self.event_tx.clone();
        #[cfg(test)]
        let before_entry = self.shared.before_entry.lock().unwrap().clone();
        self.in_flight += 1;
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
                                transport.tool_for_flow(&route, &module, &op, input, &key)
                            }
                            Ok(false) => return,
                            Err(error) => Err(WireError::NeverSent(error.to_string())),
                        }
                    }
                    Ok(false) => return,
                    Err(error) => Err(WireError::NeverSent(error.to_string())),
                };
                let _ = events.send(Event::Answer(Answer { call, result }));
            })
        {
            return self.end(failed("worker_lost", error.to_string()));
        }
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
        let CallKind::Tool { name } = &call.kind else {
            unreachable!()
        };
        let (settlement, value) = if mapped.outcome == Outcome::Ok {
            (Settlement::Fulfilled, mapped.value.unwrap())
        } else {
            let rejection = json!({
                "code":mapped.code, "tool":name, "outcome":mapped.outcome.as_str(),
                "message":mapped.message
            })
            .to_string();
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
        self.stopping = true;
        self.parent_owned_kill = true;
        terminal.output = self.run.output.clone();
        terminal.warnings = self.run.warnings.clone();
        terminate(
            &self.shared,
            &self.run.run_id,
            self.worker.as_deref_mut(),
            terminal,
        )
    }
}

fn terminate(
    shared: &Shared,
    id: &str,
    worker: Option<&mut (dyn WorkerChannel + 'static)>,
    terminal: Terminal,
) -> Result<()> {
    // The driver has stopped accepting calls and marked the worker kill as
    // parent-initiated. Cancel calls not yet sent, mark unanswered attempts as
    // unknown, then release the registered scope before making the run terminal.
    // Admission counts running rows, so the terminal commit frees its slot.
    if let Some(worker) = worker {
        worker.kill();
    }
    let at = shared.clock.now_ms();
    shared
        .store
        .write(|tx| store::settle_pending_calls(tx, id, at))?;
    shared
        .transport
        .configure_flow(&format!("codemode:{id}"), false, None);
    shared
        .store
        .write(|tx| store::commit_terminal(tx, id, &terminal, at))?;
    Ok(())
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

struct Mapped {
    outcome: Outcome,
    code: Option<String>,
    message: String,
    value: Option<JsonText>,
    scope_loss: bool,
}

impl Mapped {
    fn rejected(outcome: Outcome, code: &str, message: &str) -> Self {
        Self {
            outcome,
            code: Some(code.into()),
            message: message.into(),
            value: None,
            scope_loss: false,
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
                ScopeEnded | ScopeNotLive | ScopeNotSynced | ScopeChanged => {
                    (Outcome::Refused, refusal.reason.as_str(), true)
                }
                ConsentUnavailable => (Outcome::ConsentUnavailable, "consent_unavailable", false),
                ScopeNotCarrier
                | ScopeEpochRequired
                | ScopeUnsupported
                | TargetFlowUnsupported
                | ResourceBusy
                | ModuleGrantAbsent
                | AgentGrantAbsent
                | NoFlowScope
                | AgentRetired
                | FlowScopeRequired => (Outcome::ToolUnavailable, "tool_unavailable", false),
            };
            let mut mapped = Mapped::rejected(outcome, code, refusal.message());
            mapped.scope_loss = scope_loss;
            mapped
        }
        Err(WireError::NeverSent(message)) => {
            Mapped::rejected(Outcome::ToolUnavailable, "tool_unavailable", &message)
        }
        Err(WireError::Unknown(message)) => {
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
        ) => Mapped::rejected(Outcome::Error, &code, &message),
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

fn activation_end(result: ActivationResult) -> Terminal {
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
            failed(code, format!("{failure:?}"))
        }
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
