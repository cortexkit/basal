use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use basal_proto::{ActivationRequest, ActivationResult, ParentMessage, Profile, WorkerMessage};

use super::*;
use crate::{
    channel::{ChannelError, WorkerChannel, WorkerWriter},
    ids::Fingerprint,
    model::RunState,
    runtime::ActivationEnd,
};

/// Longest wait for a worker frame before the activation loop rechecks its
/// deadline and control state. Completed dispatches do not wait for it: they
/// wake the loop through [`Wake::Settled`].
pub(super) const WORKER_POLL: Duration = Duration::from_millis(100);

/// The reader, writer and dispatch threads share one wake queue so a worker
/// frame, failed write or recorded host reply can wake the loop immediately.
enum Wake {
    Frame(std::result::Result<WorkerMessage, ChannelError>),
    /// A dispatch thread has recorded its reply; an awaited outcome may now be
    /// ready to release.
    Settled,
    /// The frame reader thread exited without delivering a final frame, for
    /// example by panicking. The worker channel is treated as closed.
    ReaderEnded,
    WriteFailed(ChannelError),
}

/// What one wait of the activation loop produced.
enum Received {
    Frame(WorkerMessage),
    /// Nothing from the worker; recheck dispatches and pending releases.
    Recheck,
}

/// Sends `ReaderEnded` when the frame reader thread exits for any reason,
/// including a panic, so the activation loop never waits on a reader that is
/// gone. A wake sent after a final frame is never read.
struct ReaderExit(mpsc::Sender<Wake>);
impl Drop for ReaderExit {
    fn drop(&mut self) {
        let _ = self.0.send(Wake::ReaderEnded);
    }
}

struct Frames {
    messages: mpsc::Receiver<Wake>,
    permit: mpsc::Sender<()>,
    in_flight: bool,
    reader: JoinHandle<()>,
}
impl Frames {
    /// `wake` and `messages` are the two ends of the loop's wake queue, which
    /// dispatch threads may already have used before the worker started.
    fn start(
        worker: &mut dyn WorkerChannel,
        wake: mpsc::Sender<Wake>,
        messages: mpsc::Receiver<Wake>,
    ) -> std::result::Result<Self, ChannelError> {
        let mut receiver = worker.receiver();
        let (permit, receive) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("flow-reader".into())
            .spawn(move || {
                let send = wake.clone();
                let _exit = ReaderExit(wake);
                loop {
                    if receive.recv().is_err() {
                        break;
                    }
                    let message = receiver.recv();
                    let done = message
                        .as_ref()
                        .is_ok_and(|m| matches!(m, WorkerMessage::Finished { .. }));
                    let stop = done || message.is_err();
                    if send.send(Wake::Frame(message)).is_err() || stop {
                        break;
                    }
                }
            })
            .map_err(|e| ChannelError::Broken(e.to_string()))?;
        Ok(Self {
            messages,
            permit,
            in_flight: false,
            reader,
        })
    }
    fn recv(&mut self, timeout: Duration) -> std::result::Result<Received, ChannelError> {
        // Read only after the driver finished the previous frame. A timeout
        // or a dispatch wake leaves that read in flight rather than requesting
        // another read.
        if !self.in_flight {
            self.permit.send(()).map_err(|_| ChannelError::Closed)?;
            self.in_flight = true;
        }
        match self.messages.recv_timeout(timeout) {
            Ok(Wake::Frame(message)) => {
                self.in_flight = false;
                message.map(Received::Frame)
            }
            Ok(Wake::Settled) => Ok(Received::Recheck),
            Ok(Wake::WriteFailed(error)) => Err(error),
            Ok(Wake::ReaderEnded) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(ChannelError::Closed)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ChannelError::Timeout),
        }
    }
    fn stop(self) -> Result<()> {
        drop(self.messages);
        drop(self.permit);
        self.reader
            .join()
            .map_err(|_| CoreError::Worker("worker receiver panicked".into()))
    }
}

struct Writer {
    queue: mpsc::SyncSender<ParentMessage>,
    outstanding: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    thread: JoinHandle<Option<Box<dyn WorkerWriter>>>,
}

impl Writer {
    fn start(
        mut writer: Box<dyn WorkerWriter>,
        wake: mpsc::Sender<Wake>,
    ) -> std::result::Result<Self, ChannelError> {
        // After Activate, a conforming worker waits for one delivery per
        // Blocked or synchronous HostCall. LongRunning notifications can also
        // arrive while it waits, but it reads them and stays well below 64
        // queued frames. A full queue means the worker stopped reading; end
        // the activation rather than blocking the driver or growing the queue.
        let (queue, frames) = mpsc::sync_channel(64);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let unwritten = outstanding.clone();
        let write_failed = failed.clone();
        let thread = thread::Builder::new()
            .name("flow-writer".into())
            .spawn(move || {
                while let Ok(frame) = frames.recv() {
                    let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        writer.send(&frame)
                    }));
                    let error = match sent {
                        Ok(Ok(())) => {
                            unwritten.fetch_sub(1, Ordering::Release);
                            continue;
                        }
                        Ok(Err(error)) => error,
                        Err(_) => ChannelError::Broken("worker writer panicked".into()),
                    };
                    write_failed.store(true, Ordering::Release);
                    let _ = wake.send(Wake::WriteFailed(error));
                    return None;
                }
                Some(writer)
            })
            .map_err(|e| ChannelError::Broken(e.to_string()))?;
        Ok(Self {
            queue,
            outstanding,
            failed,
            thread,
        })
    }

    fn send(&self, frame: ParentMessage) -> std::result::Result<(), ChannelError> {
        self.outstanding.fetch_add(1, Ordering::Relaxed);
        self.queue.try_send(frame).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                ChannelError::Broken("worker writer queue is full".into())
            }
            mpsc::TrySendError::Disconnected(_) => ChannelError::Closed,
        })
    }

    fn idle(&self) -> bool {
        // Only the writer retires a frame, once send has returned successfully.
        // Zero means there is neither a queued frame nor a write to wait for.
        self.outstanding.load(Ordering::Acquire) == 0
    }

    fn stop(self) -> Result<Option<Box<dyn WorkerWriter>>> {
        drop(self.queue);
        self.thread
            .join()
            .map_err(|_| CoreError::Worker("worker writer panicked".into()))
    }
}

struct WorkerIo<'a> {
    worker: &'a mut dyn WorkerChannel,
    frames: Option<Frames>,
    writer: Option<Writer>,
    idle_at_finished: bool,
    killed: bool,
}

impl WorkerIo<'_> {
    fn start(
        &mut self,
        wake: mpsc::Sender<Wake>,
        woken: mpsc::Receiver<Wake>,
    ) -> std::result::Result<(), ChannelError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.worker.fixture_inline_recv() {
            return Ok(());
        }
        let writer = self.worker.writer().ok_or(ChannelError::Closed)?;
        self.writer = Some(Writer::start(writer, wake.clone())?);
        self.frames = Some(Frames::start(self.worker, wake, woken)?);
        Ok(())
    }

    fn send(&mut self, frame: ParentMessage) -> std::result::Result<(), ChannelError> {
        if let Some(writer) = &self.writer {
            return writer.send(frame);
        }
        #[cfg(any(test, feature = "test-support"))]
        if self.worker.fixture_inline_recv() {
            return self.worker.send(&frame);
        }
        Err(ChannelError::Closed)
    }

    fn recv(&mut self, wait: Duration) -> std::result::Result<Received, ChannelError> {
        if let Some(frames) = &mut self.frames {
            return frames.recv(wait);
        }
        #[cfg(any(test, feature = "test-support"))]
        if self.worker.fixture_inline_recv() {
            return self.worker.recv(wait).map(Received::Frame);
        }
        Err(ChannelError::Closed)
    }

    fn write_failed(&self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|w| w.failed.load(Ordering::Acquire))
    }

    fn kill(&mut self) {
        if !self.killed {
            self.worker.kill();
            self.killed = true;
        }
    }

    fn stop(&mut self, reusable_end: bool) -> Result<()> {
        if self.writer.is_none() && self.frames.is_none() {
            return Ok(());
        }
        let keep = reusable_end && self.idle_at_finished && !self.killed;
        // A worker can claim Finished without reading its input. If any write
        // was still pending at Finished, joining first could wedge the driver.
        if !keep {
            self.kill();
        }
        let reader = self.frames.take().map(Frames::stop).transpose();
        let writer = self.writer.take().map(Writer::stop).transpose();
        match (reader, writer) {
            (Ok(_), Ok(writer)) => {
                if keep
                    && !writer
                        .flatten()
                        .is_some_and(|w| self.worker.restore_writer(w).is_some())
                {
                    self.kill();
                }
                Ok(())
            }
            (Err(error), _) | (_, Err(error)) => {
                self.kill();
                Err(error)
            }
        }
    }
}

impl Drop for WorkerIo<'_> {
    fn drop(&mut self) {
        let _ = self.stop(false);
    }
}

fn reusable_end(result: &Result<ActivationEnd>) -> bool {
    matches!(
        result,
        Ok(ActivationEnd::Succeeded { .. }
            | ActivationEnd::Suspended { .. }
            | ActivationEnd::Failed { .. }
            | ActivationEnd::EngineMismatch { .. })
    )
}

/// Calls whose possible send is unresolved and still need reconciliation.
fn unresolved_positions(state: &crate::run_records::State) -> Vec<u64> {
    state
        .calls
        .values()
        .filter(|c| c.unknown.is_some() && c.arrival.is_none())
        .map(|c| c.identity.position)
        .collect()
}

impl Activation {
    fn terminal(&self, state: RunState, effects: &Arc<dyn TerminalEffects>) -> Terminal {
        Terminal::new(
            self.live.clone(),
            state,
            self.driver.services.now_ms(),
            self.driver.budgets.clone(),
            effects.clone(),
        )
    }

    fn fail(
        &self,
        kind: &str,
        detail: String,
        effects: &Arc<dyn TerminalEffects>,
    ) -> Result<ActivationEnd> {
        let mut terminal = self.terminal(RunState::Failed, effects);
        terminal.error_kind = Some(kind.into());
        terminal.error_detail = Some(detail.clone());
        self.finish(terminal)?;
        Ok(ActivationEnd::Failed {
            kind: kind.into(),
            detail,
        })
    }

    /// Drives an already gated activation. The embedding runtime supplies the
    /// event preamble and worker budgets; the script is loaded from the snapshot
    /// selected by run_identities. Activation id, inputs and replay prefix are
    /// always replaced here.
    pub fn drive(
        &self,
        worker: &mut dyn WorkerChannel,
        mut request: ActivationRequest,
        effects: Arc<dyn TerminalEffects>,
        timeout: Duration,
    ) -> Result<ActivationEnd> {
        let mut io = WorkerIo {
            worker,
            frames: None,
            writer: None,
            idle_at_finished: false,
            killed: false,
        };
        let result = self.drive_inner(&mut io, &mut request, &effects, timeout);
        io.stop(reusable_end(&result))?;
        let result = match result {
            Err(error)
                if !matches!(
                    error,
                    CoreError::Cut | CoreError::Store(_) | CoreError::OwnerLost { .. }
                ) =>
            {
                io.kill();
                let kind = if matches!(&error,CoreError::Worker(detail) if detail.starts_with("run_limit:"))
                {
                    "limit"
                } else if matches!(&error,CoreError::Worker(detail) if detail=="call is not available to the flow profile")
                {
                    "profile_violation"
                } else if matches!(&error,CoreError::Worker(detail) if detail.starts_with("agent_retired:"))
                {
                    "agent_retired"
                } else if matches!(&error,CoreError::Worker(detail) if detail.starts_with("version_revoked:"))
                {
                    "version_revoked"
                } else {
                    "core"
                };
                self.fail(kind, error.to_string(), &effects)
            }
            other => other,
        };
        if !reusable_end(&result) {
            io.kill();
        }
        result
    }

    fn drive_inner(
        &self,
        io: &mut WorkerIo<'_>,
        request: &mut ActivationRequest,
        effects: &Arc<dyn TerminalEffects>,
        timeout: Duration,
    ) -> Result<ActivationEnd> {
        if self.deadline_passed()? {
            return self.fail(
                "deadline",
                "the run passed its wall-clock deadline".into(),
                effects,
            );
        }
        let (script, manifest, identity) = self.driver.store.read(|conn| {
            let identity = crate::run_store_sql::identity(conn, &self.live.run_id)?
                .ok_or_else(|| CoreError::NoSuchRun(self.live.run_id.clone()))?;
            let (script, manifest): (String, String) = conn.query_row(
                "SELECT script,manifest FROM run_snapshots WHERE snapshot_id=?1",
                [identity.snapshot_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((script, manifest, identity))
        })?;
        if identity.flow_id != self.live.flow_id
            || identity.flow_version != self.live.flow_version
            || identity.code_hash != crate::ids::code_hash(&script, &manifest)
        {
            let detail = "run snapshot identity changed".to_owned();
            let mut terminal = self.terminal(RunState::EngineMismatch, effects);
            terminal.error_detail = Some(detail.clone());
            self.finish(terminal)?;
            return Ok(ActivationEnd::EngineMismatch { detail });
        }
        request.script = script;
        if !self.fingerprint(Fingerprint::of_worker(io.worker.welcome()).canonical())? {
            let detail = "runtime fingerprint changed".to_owned();
            let mut terminal = self.terminal(RunState::EngineMismatch, effects);
            terminal.error_detail = Some(detail.clone());
            self.finish(terminal)?;
            return Ok(ActivationEnd::EngineMismatch { detail });
        }
        self.driver.at(
            &self.live.run_id,
            Boundary::DriverActivated {
                generation: self.generation,
            },
        )?;
        let mut pending = Vec::new();
        let mut reported = BTreeSet::new();
        // A host reply can wake the loop as soon as it is recorded.
        let (wake, woken) = mpsc::channel();
        let spawn = |dispatch: Dispatch| {
            let (tx, rx) = mpsc::sync_channel(1);
            let wake = wake.clone();
            let services = self.driver.services.clone();
            self.driver.services.spawn_dispatch(Box::new(move || {
                let reply = dispatch.send();
                let recorded = reply.is_ok();
                let _ = tx.send(reply);
                // Dispatch::send returns only after the reply is appended, and
                // after an outcome that needs it is synced. If the activation
                // has exited, wake the engine to scan the new state instead.
                if wake.send(Wake::Settled).is_err() && recorded {
                    services.wake_after_dispatch();
                }
            }))?;
            Ok::<_, CoreError>(rx)
        };
        for dispatch in self.recover_dispatches_for_worker(&reported)? {
            pending.push(spawn(dispatch)?);
        }
        let (unknown, prefix) =
            self.with_state(|state| (unresolved_positions(state), state.prefix()))?;
        if !unknown.is_empty() {
            return Ok(ActivationEnd::NeedsReconcile { positions: unknown });
        }
        if self.deadline_passed()? {
            return self.fail(
                "deadline",
                "the run passed its wall-clock deadline".into(),
                effects,
            );
        }
        request.activation_id = self.generation;
        request.profile = Profile::Flow;
        request.tools.clear();
        request.prelude_hash = io.worker.welcome().prelude_hash;
        request.self_input = crate::model::json(self.live.self_input.to_string(), "self input")?;
        request.params_input =
            crate::model::json(self.live.params_input.to_string(), "params input")?;
        request.prefix = prefix;
        self.script_started()?;
        let until = Instant::now() + timeout;
        if io
            .start(wake.clone(), woken)
            .and_then(|()| io.send(ParentMessage::Activate(Box::new(request.clone()))))
            .is_err()
        {
            self.requeue(true)?;
            return Ok(ActivationEnd::Requeued);
        }
        let mut awaiting = Vec::new();
        loop {
            if io.write_failed() {
                self.requeue(true)?;
                return Ok(ActivationEnd::Requeued);
            }
            // Drop finished dispatches so each pass checks only those still
            // running; the first recorded error ends the activation as before.
            let mut failed = None;
            pending.retain(|rx: &mpsc::Receiver<Result<()>>| match rx.try_recv() {
                Ok(Ok(())) | Err(mpsc::TryRecvError::Disconnected) => false,
                Ok(Err(error)) => {
                    failed.get_or_insert(error);
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
            });
            if let Some(error) = failed {
                return Err(error);
            }
            for dispatch in self.recover_dispatches_for_worker(&reported)? {
                pending.push(spawn(dispatch)?);
            }
            // One borrowed read under the actor instead of copying the whole
            // call history on every pass of this loop.
            let (unknown, control) =
                self.with_state(|state| (unresolved_positions(state), control_failure(state)))?;
            if !unknown.is_empty() {
                return Ok(ActivationEnd::NeedsReconcile { positions: unknown });
            }
            if let Some((kind, detail)) = control {
                io.kill();
                return self.fail(kind, detail, effects);
            }
            if self.deadline_passed()? || Instant::now() >= until {
                io.kill();
                return self.fail(
                    "deadline",
                    "the run passed its wall-clock deadline".into(),
                    effects,
                );
            }
            if !awaiting.is_empty() {
                #[cfg(test)]
                self.driver.services.before_worker_release();
                if let Some(outcome) = self.release(&awaiting)? {
                    awaiting.clear();
                    if io.send(ParentMessage::Deliver(outcome)).is_err() {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                } else {
                    let positions: Vec<_> =
                        self.with_state(|state| {
                            awaiting
                                .iter()
                                .copied()
                                .filter(|p| {
                                    state.calls.get(p).is_some_and(|c| {
                                        c.accepted.is_some() || c.deferred.is_some()
                                    }) && reported.insert(*p)
                                })
                                .collect()
                        })?;
                    if !positions.is_empty()
                        && io.send(ParentMessage::LongRunning { positions }).is_err()
                    {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                }
            }
            #[cfg(not(test))]
            let poll = WORKER_POLL;
            #[cfg(test)]
            let poll = self.driver.services.worker_poll();
            let wait = until.saturating_duration_since(Instant::now()).min(poll);
            let message = match io.recv(wait) {
                Ok(Received::Frame(message)) => message,
                Ok(Received::Recheck) | Err(ChannelError::Timeout) => continue,
                Err(_) => {
                    self.requeue(true)?;
                    return Ok(ActivationEnd::Requeued);
                }
            };
            match message {
                WorkerMessage::HostCall(call) => match self.issue_for_worker(call)? {
                    Issued::Dispatch(dispatch) => pending.push(spawn(*dispatch)?),
                    Issued::Reply(outcome) => {
                        if io.send(ParentMessage::Deliver(outcome)).is_err() {
                            self.requeue(true)?;
                            return Ok(ActivationEnd::Requeued);
                        }
                    }
                    Issued::Pending => {}
                },
                WorkerMessage::Blocked {
                    awaiting: positions,
                } => {
                    awaiting = positions;
                }
                WorkerMessage::Finished {
                    activation_id,
                    result,
                } => {
                    if activation_id != self.generation || io.write_failed() {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                    io.idle_at_finished = io.writer.as_ref().is_some_and(Writer::idle);
                    if let Some((kind, detail)) = self.with_state(control_failure)? {
                        io.kill();
                        return self.fail(kind, detail, effects);
                    }
                    return match result {
                        ActivationResult::Completed { value } => {
                            let mut terminal = self.terminal(RunState::Succeeded, effects);
                            terminal.result = Some(
                                serde_json::from_str(value.as_str())
                                    .map_err(|e| CoreError::Worker(e.to_string()))?,
                            );
                            self.finish(terminal)?;
                            Ok(ActivationEnd::Succeeded {
                                value: value.as_str().into(),
                            })
                        }
                        ActivationResult::Suspended { awaited } => {
                            if self.park(&awaited)? {
                                Ok(ActivationEnd::Suspended { awaited })
                            } else {
                                self.requeue(false)?;
                                Ok(ActivationEnd::Requeued)
                            }
                        }
                        ActivationResult::Failed(basal_proto::Failure::EngineMismatch {
                            expected,
                            actual,
                        }) => {
                            let detail = format!(
                                "the worker refused prelude {expected:?}; it has {actual:?}"
                            );
                            let mut terminal = self.terminal(RunState::EngineMismatch, effects);
                            terminal.error_detail = Some(detail.clone());
                            self.finish(terminal)?;
                            Ok(ActivationEnd::EngineMismatch { detail })
                        }
                        ActivationResult::Failed(basal_proto::Failure::Nondeterminism(detail)) => {
                            self.fail("nondeterminism", format!("{detail:?}"), effects)
                        }
                        ActivationResult::Failed(basal_proto::Failure::Script { message }) => {
                            self.fail("script", message, effects)
                        }
                        ActivationResult::Failed(basal_proto::Failure::ScriptHostRejection {
                            position,
                            message,
                        }) => {
                            let readiness = self.with_state(|state| {
                                state
                                    .calls
                                    .get(&position)
                                    .and_then(|c| c.arrival.as_ref())
                                    .is_some_and(|a| {
                                        a.settlement == Settlement::Rejected
                                            && crate::ops::typed_scope_rejection(
                                                a.value.as_str(),
                                                true,
                                            )
                                    })
                            })?;
                            self.fail(
                                if readiness {
                                    "readiness_rejection"
                                } else {
                                    "script"
                                },
                                message,
                                effects,
                            )
                        }
                        ActivationResult::Failed(other) => {
                            self.fail("activation", format!("{other:?}"), effects)
                        }
                        ActivationResult::Stalled => self.fail(
                            "stalled",
                            "the script waits on a promise no host call will settle".into(),
                            effects,
                        ),
                        ActivationResult::BudgetExhausted(kind) => {
                            io.kill();
                            self.fail("budget_exhausted", format!("{kind:?}"), effects)
                        }
                    };
                }
                other => {
                    io.kill();
                    return self.fail(
                        "profile_violation",
                        format!("unexpected flow frame: {other:?}"),
                        effects,
                    );
                }
            }
        }
    }
}
