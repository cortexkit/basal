use std::{
    sync::{Arc, mpsc},
    time::Instant,
};

use basal_proto::{ActivationRequest, ActivationResult, ParentMessage, Profile, WorkerMessage};

use super::*;
use crate::{
    channel::{ChannelError, WorkerChannel},
    ids::Fingerprint,
    model::RunState,
    runtime::ActivationEnd,
};

struct Frames {
    messages: mpsc::Receiver<std::result::Result<WorkerMessage, ChannelError>>,
    permit: mpsc::Sender<()>,
    in_flight: bool,
    finished: Arc<std::sync::atomic::AtomicBool>,
    reader: std::thread::JoinHandle<()>,
}
impl Frames {
    fn start(worker: &mut dyn WorkerChannel) -> Self {
        let mut receiver = worker.receiver();
        let (send, messages) = mpsc::sync_channel(1);
        let (permit, receive) = mpsc::channel();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ended = finished.clone();
        let reader = std::thread::spawn(move || {
            loop {
                if receive.recv().is_err() {
                    break;
                }
                let message = receiver.recv();
                let done = message
                    .as_ref()
                    .is_ok_and(|m| matches!(m, WorkerMessage::Finished { .. }));
                if done {
                    ended.store(true, std::sync::atomic::Ordering::Release);
                }
                let stop = done || message.is_err();
                if send.send(message).is_err() || stop {
                    break;
                }
            }
        });
        Self {
            messages,
            permit,
            in_flight: false,
            finished,
            reader,
        }
    }
    fn recv(&mut self, timeout: Duration) -> std::result::Result<WorkerMessage, ChannelError> {
        // Read only after the driver finished the previous frame. A timeout
        // leaves that read in flight rather than requesting another read.
        if !self.in_flight {
            self.permit.send(()).map_err(|_| ChannelError::Closed)?;
            self.in_flight = true;
        }
        match self.messages.recv_timeout(timeout) {
            Ok(message) => {
                self.in_flight = false;
                message
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ChannelError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ChannelError::Closed),
        }
    }
    fn stop(self, worker: &mut dyn WorkerChannel) -> Result<()> {
        drop(self.messages);
        drop(self.permit);
        if !self.finished.load(std::sync::atomic::Ordering::Acquire) {
            worker.kill();
        }
        self.reader
            .join()
            .map_err(|_| CoreError::Worker("worker receiver panicked".into()))
    }
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
        let mut frames = None;
        let result = self.drive_inner(worker, &mut request, &effects, timeout, &mut frames);
        if let Some(frames) = frames {
            frames.stop(worker)?;
        }
        let result = match result {
            Err(error)
                if !matches!(
                    error,
                    CoreError::Cut | CoreError::Store(_) | CoreError::OwnerLost { .. }
                ) =>
            {
                worker.kill();
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
        if !matches!(
            result,
            Ok(ActivationEnd::Succeeded { .. }
                | ActivationEnd::Suspended { .. }
                | ActivationEnd::Failed { .. }
                | ActivationEnd::EngineMismatch { .. })
        ) {
            worker.kill();
        }
        result
    }

    fn drive_inner(
        &self,
        worker: &mut dyn WorkerChannel,
        request: &mut ActivationRequest,
        effects: &Arc<dyn TerminalEffects>,
        timeout: Duration,
        frames: &mut Option<Frames>,
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
        if !self.fingerprint(Fingerprint::of_worker(worker.welcome()).canonical())? {
            let detail = "runtime fingerprint changed".to_owned();
            let mut terminal = self.terminal(RunState::EngineMismatch, effects);
            terminal.error_detail = Some(detail.clone());
            self.finish(terminal)?;
            return Ok(ActivationEnd::EngineMismatch { detail });
        }
        let mut pending = Vec::new();
        let spawn = |dispatch: Dispatch| {
            let (tx, rx) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _ = tx.send(dispatch.send());
            });
            rx
        };
        for dispatch in self.recover_dispatches_for_worker()? {
            pending.push(spawn(dispatch));
        }
        let state = self.state()?;
        let unknown: Vec<_> = state
            .calls
            .values()
            .filter(|c| c.unknown.is_some() && c.arrival.is_none())
            .map(|c| c.identity.position)
            .collect();
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
        request.prelude_hash = worker.welcome().prelude_hash;
        request.self_input = crate::model::json(self.live.self_input.to_string(), "self input")?;
        request.params_input =
            crate::model::json(self.live.params_input.to_string(), "params input")?;
        request.prefix = state.prefix();
        self.script_started()?;
        if worker
            .send(&ParentMessage::Activate(Box::new(request.clone())))
            .is_err()
        {
            self.requeue(true)?;
            return Ok(ActivationEnd::Requeued);
        }
        let until = Instant::now() + timeout;
        #[cfg(any(test, feature = "test-support"))]
        let inline = worker.fixture_inline_recv();
        #[cfg(not(any(test, feature = "test-support")))]
        let inline = false;
        if !inline {
            *frames = Some(Frames::start(worker));
        }
        let mut awaiting = Vec::new();
        let mut reported = BTreeSet::new();
        loop {
            for rx in &pending {
                match rx.try_recv() {
                    Ok(result) => result?,
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
                }
            }
            for dispatch in self.recover_dispatches_for_worker()? {
                pending.push(spawn(dispatch));
            }
            let unknown: Vec<_> = self
                .state()?
                .calls
                .values()
                .filter(|c| c.unknown.is_some() && c.arrival.is_none())
                .map(|c| c.identity.position)
                .collect();
            if !unknown.is_empty() {
                return Ok(ActivationEnd::NeedsReconcile { positions: unknown });
            }
            if let Some((kind, detail)) = control_failure(&self.state()?) {
                worker.kill();
                return self.fail(kind, detail, effects);
            }
            if self.deadline_passed()? || Instant::now() >= until {
                worker.kill();
                return self.fail(
                    "deadline",
                    "the run passed its wall-clock deadline".into(),
                    effects,
                );
            }
            if !awaiting.is_empty() {
                if let Some(outcome) = self.release(&awaiting)? {
                    awaiting.clear();
                    if worker.send(&ParentMessage::Deliver(outcome)).is_err() {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                } else {
                    let state = self.state()?;
                    let positions: Vec<_> = awaiting
                        .iter()
                        .copied()
                        .filter(|p| {
                            state
                                .calls
                                .get(p)
                                .is_some_and(|c| c.accepted.is_some() || c.deferred.is_some())
                                && reported.insert(*p)
                        })
                        .collect();
                    if !positions.is_empty()
                        && worker
                            .send(&ParentMessage::LongRunning { positions })
                            .is_err()
                    {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                }
            }
            let wait = until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            let message = match frames
                .as_mut()
                .map_or_else(|| worker.recv(wait), |frames| frames.recv(wait))
            {
                Ok(message) => message,
                Err(ChannelError::Timeout) => continue,
                Err(_) => {
                    self.requeue(true)?;
                    return Ok(ActivationEnd::Requeued);
                }
            };
            match message {
                WorkerMessage::HostCall(call) => match self.issue_for_worker(call)? {
                    Issued::Dispatch(dispatch) => pending.push(spawn(*dispatch)),
                    Issued::Reply(outcome) => {
                        if worker.send(&ParentMessage::Deliver(outcome)).is_err() {
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
                    if activation_id != self.generation {
                        self.requeue(true)?;
                        return Ok(ActivationEnd::Requeued);
                    }
                    if let Some((kind, detail)) = control_failure(&self.state()?) {
                        worker.kill();
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
                            let readiness = self
                                .state()?
                                .calls
                                .get(&position)
                                .and_then(|c| c.arrival.as_ref())
                                .is_some_and(|a| {
                                    a.settlement == Settlement::Rejected
                                        && crate::ops::typed_scope_rejection(a.value.as_str(), true)
                                });
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
                            worker.kill();
                            self.fail("budget_exhausted", format!("{kind:?}"), effects)
                        }
                    };
                }
                other => {
                    worker.kill();
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
