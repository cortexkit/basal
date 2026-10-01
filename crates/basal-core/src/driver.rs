//! The activation driver: one activation of one run on one worker.
//!
//! The order of every step is the crash-safety argument:
//! - a call's row commits before the call is dispatched;
//! - an outcome and its delivery order commit before the `Deliver` frame,
//!   and a synchronous call's row, value and order commit before its reply;
//! - the prefix shipped to the worker holds only outcomes whose order is
//!   committed, and later outcomes are released one at a time once the
//!   worker has replayed it and reports itself blocked;
//! - every one of those writes is fenced by the activation's lease, so a
//!   superseded activation writes nothing and its worker is killed.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use basal_proto::{
    ActivationRequest, ActivationResult, ArgsDigest, CallKind, CallSignature, Failure, HostCall,
    JsonText, Nondeterminism, Outcome, ParentMessage, Primitive, Profile, Settlement,
    WorkerMessage,
};
use serde_json::Number;

use crate::channel::{ChannelError, WorkerChannel};
use crate::error::{CoreError, Result};
use crate::hooks::Boundary;
use crate::ids::{Fingerprint, code_hash};
use crate::journal::{self, NewCall, Source};
use crate::local;
use crate::model::{DispatchState, Run, RunState, StoredClass};
use crate::runs::{self, Exit, Lease, Parked};
use crate::runtime::{ActivationEnd, Runtime};

/// How long a blocked activation sleeps between mailbox checks when nothing
/// signals it (a backstop; arrivals wake it at once).
const WAIT_SLICE: Duration = Duration::from_millis(100);

/// What the driver loop decided.
enum Flow {
    Continue,
    /// The activation is over. `idle` says whether the worker finished its
    /// activation and can be reused; otherwise it is killed.
    Done {
        end: ActivationEnd,
        idle: bool,
    },
}

fn done(end: ActivationEnd, idle: bool) -> Flow {
    Flow::Done { end, idle }
}

/// Formats a number as JSON for a clock read or random sample.
fn number_text(v: f64) -> JsonText {
    let text = if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        format!("{}", v as i64)
    } else {
        Number::from_f64(v)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_owned())
    };
    JsonText::new(text).unwrap_or_else(|_| JsonText::null())
}

struct Activation<'a> {
    rt: &'a Runtime,
    lease: Lease,
    run: Run,
    worker: &'a mut dyn WorkerChannel,
    /// The position the next new call must have.
    next_position: u64,
    /// Long-running positions already reported to the worker.
    long_reported: BTreeSet<u64>,
    /// The run's readiness sequence as of the last mailbox check.
    readiness_seen: u64,
    deadline: Instant,
}

impl Runtime {
    /// Starts an activation of a pending run on `worker` and drives it to
    /// its end.
    pub fn activate(&self, run_id: &str, worker: &mut dyn WorkerChannel) -> Result<ActivationEnd> {
        let lease = match self.claim(run_id)? {
            Ok(lease) => lease,
            Err(state) => return Ok(ActivationEnd::NotRunnable { state }),
        };
        Ok(self.drive(lease, worker))
    }

    /// Starts an activation of a pending run on a worker from the source.
    pub fn resume(&self, run_id: &str) -> Result<ActivationEnd> {
        let Some(source) = self.shared.source.clone() else {
            return Err(CoreError::Invalid("no worker source configured".into()));
        };
        let lease = match self.claim(run_id)? {
            Ok(lease) => lease,
            Err(state) => return Ok(ActivationEnd::NotRunnable { state }),
        };
        let mut worker = match source.worker() {
            Ok(w) => w,
            Err(e) => {
                // No worker: hand the run back untouched.
                self.shared
                    .store
                    .write(|tx| runs::exit(tx, &lease, &Exit::Requeue { broken: false }))?;
                return Err(CoreError::Worker(e.to_string()));
            }
        };
        Ok(self.drive(lease, worker.as_mut()))
    }

    /// Claims a pending run, or reports the state that prevented it.
    fn claim(&self, run_id: &str) -> Result<std::result::Result<Lease, RunState>> {
        let owner = self.config.owner.clone();
        match self
            .shared
            .store
            .write(|tx| runs::claim(tx, run_id, &owner))
        {
            Ok(lease) => {
                self.at(
                    run_id,
                    Boundary::Claimed {
                        generation: lease.generation,
                    },
                )?;
                Ok(Ok(lease))
            }
            Err(CoreError::WrongState { state, .. }) => Ok(Err(state)),
            Err(e) => Err(e),
        }
    }

    /// Drives a claimed activation. Never leaves the worker mid-activation:
    /// unless the worker finished cleanly, it is killed.
    pub fn drive(&self, lease: Lease, worker: &mut dyn WorkerChannel) -> ActivationEnd {
        let run_id = lease.run_id.clone();
        let (end, idle) = match self.store().read(|c| runs::load(c, &run_id)) {
            Err(e) => (Err(e), false),
            Ok(run) => {
                let deadline = Instant::now() + self.config.activation_deadline;
                let mut activation = Activation {
                    rt: self,
                    next_position: 0,
                    long_reported: BTreeSet::new(),
                    readiness_seen: run.readiness,
                    lease,
                    run,
                    worker,
                    deadline,
                };
                match activation.run() {
                    Ok(Flow::Done { end, idle }) => (Ok(end), idle),
                    Ok(Flow::Continue) => (
                        Err(CoreError::Worker("activation loop ended early".into())),
                        false,
                    ),
                    Err(e) => (Err(e), false),
                }
            }
        };
        if !idle {
            worker.kill();
        }
        self.shared.signal.bump();
        match end {
            Ok(ActivationEnd::Requeued) => {
                self.wake(&run_id);
                ActivationEnd::Requeued
            }
            Ok(end) => end,
            Err(CoreError::OwnerLost { .. }) => ActivationEnd::OwnerLost,
            Err(CoreError::Cut) => ActivationEnd::Crashed,
            // Storage failed: fail closed. The worker is already killed and
            // nothing was sent past the failed commit; the run stays as the
            // last successful commit left it, for recovery to pick up.
            Err(e) => ActivationEnd::Failed {
                kind: "store".into(),
                detail: e.to_string(),
            },
        }
    }
}

impl Activation<'_> {
    fn at(&self, boundary: Boundary) -> Result<()> {
        self.rt.at(&self.lease.run_id, boundary)
    }

    fn exit(&self, exit: Exit) -> Result<RunState> {
        let state = self
            .rt
            .store()
            .write(|tx| runs::exit(tx, &self.lease, &exit))?;
        self.at(Boundary::Exited)?;
        Ok(state)
    }

    fn fail(&self, kind: &str, detail: String, idle: bool) -> Result<Flow> {
        self.exit(Exit::Failed {
            kind: kind.into(),
            detail: detail.clone(),
        })?;
        Ok(done(
            ActivationEnd::Failed {
                kind: kind.into(),
                detail,
            },
            idle,
        ))
    }

    fn send(&mut self, message: &ParentMessage) -> std::result::Result<(), ChannelError> {
        self.worker.send(message)
    }

    /// The worker's channel failed: hand the run back for another worker,
    /// or fail it after too many broken activations.
    fn broken(&self, detail: String) -> Result<Flow> {
        if self.run.broken + 1 >= self.rt.config.max_broken_activations {
            return self.fail("worker", detail, false);
        }
        self.exit(Exit::Requeue { broken: true })?;
        Ok(done(ActivationEnd::Requeued, false))
    }

    fn run(&mut self) -> Result<Flow> {
        if let Flow::Done { end, idle } = self.check_identity()? {
            return Ok(done(end, idle));
        }
        if let Flow::Done { end, idle } = self.recover_calls()? {
            return Ok(done(end, idle));
        }
        let prefix = self
            .rt
            .store()
            .read(|c| journal::prefix(c, &self.lease.run_id))?;
        self.next_position = prefix.len() as u64;
        let request = ActivationRequest {
            activation_id: self.lease.generation,
            profile: Profile::Flow,
            prelude_hash: self.worker.welcome().prelude_hash,
            script: self.run.script.clone(),
            trigger: self.run.trigger.clone(),
            budgets: self.rt.config.budgets,
            prefix,
        };
        if let Err(e) = self.send(&ParentMessage::Activate(Box::new(request))) {
            return self.broken(e.to_string());
        }
        self.at(Boundary::ActivateSent {
            generation: self.lease.generation,
        })?;
        self.deadline = Instant::now() + self.rt.config.activation_deadline;
        loop {
            let now = Instant::now();
            if now >= self.deadline {
                return self.fail(
                    "deadline",
                    "the activation ran out of wall time".into(),
                    false,
                );
            }
            let message = match self.worker.recv(self.deadline - now) {
                Ok(m) => m,
                Err(ChannelError::Timeout) => continue,
                Err(e) => return self.broken(e.to_string()),
            };
            let flow = match message {
                WorkerMessage::HostCall(call) => {
                    self.at(Boundary::HostCallReceived {
                        position: call.position,
                    })?;
                    self.on_host_call(call)?
                }
                WorkerMessage::Blocked { awaiting } => {
                    self.at(Boundary::BlockedReceived)?;
                    self.on_blocked(&awaiting)?
                }
                WorkerMessage::Finished {
                    activation_id,
                    result,
                } => {
                    if activation_id != self.lease.generation {
                        return self.broken(format!(
                            "result for activation {activation_id}, expected {}",
                            self.lease.generation
                        ));
                    }
                    self.at(Boundary::EndingReceived)?;
                    self.on_finished(result)?
                }
                WorkerMessage::Refused(refusal) => {
                    return self.broken(format!("the worker refused a frame: {refusal:?}"));
                }
                WorkerMessage::Welcome(_) => {
                    return self.broken("unexpected Welcome during an activation".into());
                }
            };
            if let Flow::Done { .. } = flow {
                return Ok(flow);
            }
        }
    }

    /// Refuses to replay a run whose code or runtime changed, and records
    /// the fingerprint at the first activation.
    fn check_identity(&mut self) -> Result<Flow> {
        if code_hash(&self.run.script, &self.run.manifest) != self.run.code_hash {
            let detail = "the run's code no longer matches its approved code hash".to_owned();
            self.exit(Exit::EngineMismatch {
                detail: detail.clone(),
            })?;
            return Ok(done(ActivationEnd::EngineMismatch { detail }, true));
        }
        let current = Fingerprint::of_worker(self.worker.welcome()).canonical();
        match &self.run.fingerprint {
            Some(recorded) if *recorded != current => {
                let detail = format!("recorded under [{recorded}], worker offers [{current}]");
                self.exit(Exit::EngineMismatch {
                    detail: detail.clone(),
                })?;
                Ok(done(ActivationEnd::EngineMismatch { detail }, true))
            }
            Some(_) => Ok(Flow::Continue),
            None => {
                self.rt
                    .store()
                    .write(|tx| runs::record_fingerprint(tx, &self.lease, &current))?;
                self.at(Boundary::FingerprintRecorded)?;
                self.run.fingerprint = Some(current);
                Ok(Flow::Continue)
            }
        }
    }

    /// Decides, for every call an earlier activation left without an
    /// outcome, how it can still get one:
    /// - a call this process is still dispatching, a long-running call the
    ///   host accepted, or one whose outcome waits in the mailbox: nothing
    ///   to do;
    /// - a clock read or random sample: the worker asks again;
    /// - a query, or a mutation whose op honours idempotency keys, or one an
    ///   operator reconciled as not applied: sent again with the same key;
    /// - any other mutation: its outcome cannot be proven, so the run stops
    ///   in `needs_reconcile` naming it.
    fn recover_calls(&mut self) -> Result<Flow> {
        let run_id = self.lease.run_id.clone();
        let (rows, waiting) = self.rt.store().read(|c| {
            Ok((
                journal::rows(c, &run_id)?,
                journal::mailbox_positions(c, &run_id)?,
            ))
        })?;
        let mut resend = Vec::new();
        let mut unknown = Vec::new();
        for row in rows.iter().filter(|r| r.outcome.is_none()) {
            if waiting.contains(&row.position) || self.rt.is_inflight(&run_id, row.position) {
                continue;
            }
            match (row.class, row.dispatch) {
                (StoredClass::Sync, _) | (_, DispatchState::Accepted) => {}
                (StoredClass::Local, _) => {
                    // A local call's effect commits with its outcome, so a
                    // row without one never took effect; run it now.
                    let flow_id = self.run.flow_id.clone();
                    self.rt.store().write(|tx| {
                        let outcome =
                            local::apply(tx, &flow_id, &row.idempotency_key, &row.kind, &row.args)?;
                        journal::accept_outcome(
                            tx,
                            &run_id,
                            row.position,
                            None,
                            &outcome,
                            Source::Local,
                        )
                    })?;
                    self.at(Boundary::LocalCommitted {
                        position: row.position,
                    })?;
                }
                (_, DispatchState::NotApplied) => resend.push(row.clone()),
                (class, _) if class.safe_to_resend() => resend.push(row.clone()),
                _ => unknown.push(row.position),
            }
        }
        if !unknown.is_empty() {
            self.rt.store().write(|tx| {
                for p in &unknown {
                    journal::record_unknown(tx, &run_id, *p)?;
                }
                runs::exit(
                    tx,
                    &self.lease,
                    &Exit::NeedsReconcile {
                        positions: unknown.clone(),
                    },
                )
            })?;
            self.at(Boundary::Exited)?;
            return Ok(done(
                ActivationEnd::NeedsReconcile { positions: unknown },
                true,
            ));
        }
        for row in resend {
            self.rt.register(&run_id, row.position);
            let attempt = match self
                .rt
                .store()
                .write(|tx| journal::authorize_resend(tx, &self.lease, row.position))
            {
                Ok(a) => a,
                Err(e) => {
                    self.rt.unregister(&run_id, row.position);
                    return Err(e);
                }
            };
            self.at(Boundary::ResendAuthorized {
                position: row.position,
            })?;
            self.rt
                .spawn_dispatch(Runtime::request(&self.run, &row, attempt), row.class);
        }
        Ok(Flow::Continue)
    }

    /// The value of a clock read (kept monotonic within the run) or a random
    /// sample.
    fn sync_value(&self, kind: &CallKind) -> Result<(Settlement, JsonText, Option<f64>)> {
        match kind {
            CallKind::Primitive(Primitive::Now) => {
                let now = self.rt.shared.host.now_ms();
                let last = self
                    .rt
                    .store()
                    .read(|c| journal::last_clock(c, &self.lease.run_id))?;
                let value = last.map_or(now, |last| now.max(last));
                Ok((Settlement::Fulfilled, number_text(value), Some(value)))
            }
            _ => {
                let sample = self.rt.shared.host.random();
                Ok((Settlement::Fulfilled, number_text(sample), None))
            }
        }
    }

    fn reply(&mut self, outcome: Outcome) -> Result<Flow> {
        let position = outcome.position;
        if let Err(e) = self.send(&ParentMessage::Deliver(outcome)) {
            return self.broken(e.to_string());
        }
        self.at(Boundary::SyncReplied { position })?;
        Ok(Flow::Continue)
    }

    fn on_host_call(&mut self, call: HostCall) -> Result<Flow> {
        if call.position < self.next_position {
            return self.on_reissued_sync(call);
        }
        if call.position > self.next_position {
            return self.broken(format!(
                "call at position {}, expected {}",
                call.position, self.next_position
            ));
        }
        // The worker refuses `sh` in the flow profile; the dispatcher refuses
        // it again, so a compromised worker cannot reach a shell either.
        if call.kind.is_shell() {
            return self.fail(
                "profile_violation",
                format!("{} is not available to flows", call.kind),
                false,
            );
        }
        let class = self.rt.class_of(&call.kind);
        let flow_id = self.run.flow_id.clone();
        let new = NewCall {
            position: call.position,
            kind: &call.kind,
            args: &call.args,
            class,
        };
        match class {
            StoredClass::Sync => {
                let (settlement, value, clock) = self.sync_value(&call.kind)?;
                let outcome = self.rt.store().write(|tx| {
                    journal::insert_call(tx, &self.lease, &flow_id, &new)?;
                    // Re-read the latest clock inside the transaction, so
                    // the value is monotonic against everything committed.
                    let clock = match (clock, journal::last_clock(tx, &self.lease.run_id)?) {
                        (Some(c), Some(last)) => Some(c.max(last)),
                        (c, _) => c,
                    };
                    let value = clock.map_or(value.clone(), number_text);
                    journal::record_sync(tx, &self.lease, call.position, settlement, &value, clock)
                })?;
                self.next_position += 1;
                self.at(Boundary::SyncCommitted {
                    position: call.position,
                })?;
                self.reply(outcome)
            }
            StoredClass::Local => {
                let ack = self.rt.store().write(|tx| {
                    let key = journal::insert_call(tx, &self.lease, &flow_id, &new)?;
                    let outcome = local::apply(tx, &flow_id, &key, &call.kind, &call.args)?;
                    journal::accept_outcome(
                        tx,
                        &self.lease.run_id,
                        call.position,
                        None,
                        &outcome,
                        Source::Local,
                    )
                })?;
                let _ = ack;
                self.next_position += 1;
                self.at(Boundary::LocalCommitted {
                    position: call.position,
                })?;
                self.rt.shared.signal.bump();
                Ok(Flow::Continue)
            }
            _ => {
                let run_id = self.lease.run_id.clone();
                self.rt.register(&run_id, call.position);
                let key = match self
                    .rt
                    .store()
                    .write(|tx| journal::insert_call(tx, &self.lease, &flow_id, &new))
                {
                    Ok(key) => key,
                    Err(e) => {
                        self.rt.unregister(&run_id, call.position);
                        return Err(e);
                    }
                };
                self.next_position += 1;
                self.at(Boundary::CallCommitted {
                    position: call.position,
                })?;
                self.rt.spawn_dispatch(
                    basal_host::CallRequest {
                        flow_id,
                        run_id,
                        position: call.position,
                        kind: call.kind,
                        args: call.args,
                        idempotency_key: key,
                        attempt: 1,
                    },
                    class,
                );
                Ok(Flow::Continue)
            }
        }
    }

    /// A synchronous call journaled without its value comes back at its
    /// old position. Anything else at an old position is a divergence or a
    /// broken worker.
    fn on_reissued_sync(&mut self, call: HostCall) -> Result<Flow> {
        let row = self
            .rt
            .store()
            .read(|c| journal::row(c, &self.lease.run_id, call.position))?;
        let Some(row) = row else {
            return self.broken(format!("call at unknown position {}", call.position));
        };
        let observed = CallSignature {
            kind: call.kind.clone(),
            args_digest: ArgsDigest::of(&call.args),
        };
        if row.kind != observed.kind || row.args_digest != observed.args_digest {
            let failure = Nondeterminism::Divergence {
                position: call.position,
                recorded: CallSignature {
                    kind: row.kind.clone(),
                    args_digest: row.args_digest,
                },
                observed,
            };
            return self.fail("nondeterminism", format!("{failure:?}"), false);
        }
        if row.class != StoredClass::Sync || row.outcome.is_some() {
            return self.broken(format!(
                "the worker issued position {} again",
                call.position
            ));
        }
        let (settlement, value, clock) = self.sync_value(&call.kind)?;
        let outcome = self.rt.store().write(|tx| {
            journal::record_sync(tx, &self.lease, call.position, settlement, &value, clock)
        })?;
        self.at(Boundary::SyncCommitted {
            position: call.position,
        })?;
        self.reply(outcome)
    }

    /// The worker can make no progress until one of `awaiting` settles.
    /// Answer with exactly one outcome (committed with its order first), or
    /// with the long-running ones it has not been told about, or wait.
    fn on_blocked(&mut self, awaiting: &[u64]) -> Result<Flow> {
        let run_id = self.lease.run_id.clone();
        loop {
            let seen = self.rt.shared.signal.current();
            let unknown = self
                .rt
                .store()
                .read(|c| journal::unknown_positions(c, &run_id))?;
            if !unknown.is_empty() {
                self.exit(Exit::NeedsReconcile {
                    positions: unknown.clone(),
                })?;
                return Ok(done(
                    ActivationEnd::NeedsReconcile { positions: unknown },
                    false,
                ));
            }
            let (outcome, readiness) = self
                .rt
                .store()
                .write(|tx| journal::release_next(tx, &self.lease, awaiting))?;
            self.readiness_seen = readiness;
            if let Some(outcome) = outcome {
                let position = outcome.position;
                self.at(Boundary::OrderCommitted {
                    position,
                    order: outcome.delivery_order,
                })?;
                if let Err(e) = self.send(&ParentMessage::Deliver(outcome)) {
                    return self.broken(e.to_string());
                }
                self.at(Boundary::Delivered { position })?;
                return Ok(Flow::Continue);
            }
            let rows = self.rt.store().read(|c| journal::rows(c, &run_id))?;
            let long: Vec<u64> = rows
                .iter()
                .filter(|r| {
                    r.outcome.is_none()
                        && r.dispatch == DispatchState::Accepted
                        && awaiting.contains(&r.position)
                        && !self.long_reported.contains(&r.position)
                })
                .map(|r| r.position)
                .collect();
            if !long.is_empty() {
                self.long_reported.extend(long.iter().copied());
                if let Err(e) = self.send(&ParentMessage::LongRunning { positions: long }) {
                    return self.broken(e.to_string());
                }
                self.at(Boundary::LongRunningSent)?;
                return Ok(Flow::Continue);
            }
            let now = Instant::now();
            if now >= self.deadline {
                return self.fail(
                    "deadline",
                    format!("still waiting on {awaiting:?} when the activation ran out of time"),
                    false,
                );
            }
            self.rt
                .shared
                .signal
                .wait(seen, (self.deadline - now).min(WAIT_SLICE));
        }
    }

    fn on_finished(&mut self, result: ActivationResult) -> Result<Flow> {
        match result {
            ActivationResult::Completed { value } => {
                let exit = Exit::Succeeded {
                    value: value.as_str().to_owned(),
                };
                match self
                    .rt
                    .store()
                    .write(|tx| runs::exit(tx, &self.lease, &exit))
                {
                    Ok(_) => {
                        self.at(Boundary::Exited)?;
                        Ok(done(
                            ActivationEnd::Succeeded {
                                value: value.into_string(),
                            },
                            true,
                        ))
                    }
                    Err(CoreError::Invalid(detail)) => self.fail("fault", detail, true),
                    Err(e) => Err(e),
                }
            }
            ActivationResult::Suspended { awaited } => {
                let parked = self
                    .rt
                    .store()
                    .write(|tx| runs::park(tx, &self.lease, &awaited, self.readiness_seen))?;
                self.at(Boundary::Exited)?;
                Ok(done(
                    match parked {
                        Parked::Suspended => ActivationEnd::Suspended { awaited },
                        Parked::Runnable => ActivationEnd::Requeued,
                        Parked::NeedsReconcile => ActivationEnd::NeedsReconcile {
                            positions: self
                                .rt
                                .store()
                                .read(|c| journal::unknown_positions(c, &self.lease.run_id))?,
                        },
                    },
                    true,
                ))
            }
            ActivationResult::Failed(Failure::EngineMismatch { expected, actual }) => {
                let detail = format!("the worker refused prelude {expected:?}; it has {actual:?}");
                self.exit(Exit::EngineMismatch {
                    detail: detail.clone(),
                })?;
                Ok(done(ActivationEnd::EngineMismatch { detail }, true))
            }
            ActivationResult::Failed(Failure::Nondeterminism(n)) => {
                self.fail("nondeterminism", format!("{n:?}"), true)
            }
            ActivationResult::Failed(Failure::Script { message }) => {
                self.fail("script", message, true)
            }
            ActivationResult::Failed(other) => self.fail("activation", format!("{other:?}"), true),
            ActivationResult::Stalled => self.fail(
                "stalled",
                "the script waits on a promise no host call will settle".into(),
                true,
            ),
            ActivationResult::BudgetExhausted(kind) => {
                self.fail("budget_exhausted", format!("{kind:?}"), true)
            }
        }
    }
}
