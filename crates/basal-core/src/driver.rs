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

use basal_host::UnknownReason;
use basal_proto::{
    ActivationRequest, ActivationResult, ArgsDigest, CallKind, CallSignature, Failure, HostCall,
    JsonText, Nondeterminism, Outcome, ParentMessage, Primitive, Profile, Settlement,
    WorkerMessage,
};
use rusqlite::Transaction;
use serde_json::{Number, Value};

use crate::audit;
use crate::authorize::{self, Refusal, codes};
use crate::channel::{ChannelError, WorkerChannel};
use crate::error::{CoreError, Result};
use crate::gate::Gate;
use crate::hooks::Boundary;
use crate::ids::{Fingerprint, code_hash, idempotency_key};
use crate::install;
use crate::journal::{self, NewCall, Source};
use crate::kv;
use crate::manifest::Manifest;
use crate::model::{DispatchState, Run, RunState, StoredClass};
use crate::rate::{self, Take};
use crate::runs::{self, Exit, Lease, Parked};
use crate::runtime::{ActivationEnd, Runtime};
use crate::tokens::{self, Reservation};

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
    deadline: Option<Instant>,
    input: Value,
    trigger: Value,
    /// The run's approved manifest, decoded once per activation; every new
    /// call is authorized against it.
    manifest: Option<Manifest>,
}

/// A call that passed the manifest check, ready to journal.
#[derive(Debug, Default)]
struct Prepared {
    /// The exact bytes to send when they are not the script's arguments: a
    /// model call's clamped request.
    request: Option<JsonText>,
    /// Tokens to reserve for a model call.
    tokens: Option<u64>,
    args: Option<Value>,
}

impl Runtime {
    /// Starts an activation of a pending run on `worker` and drives it to
    /// its end.
    pub fn activate(&self, run_id: &str, worker: &mut dyn WorkerChannel) -> Result<ActivationEnd> {
        let lease = match self.claim(run_id)? {
            Ok(lease) => lease,
            Err(end) => return Ok(end),
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
            Err(end) => return Ok(end),
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

    /// Claims a pending run, or reports why no activation started: the run
    /// is not pending (a pending run already past its deadline is failed
    /// here instead), or an earlier run of its flow holds the slot.
    fn claim(&self, run_id: &str) -> Result<std::result::Result<Lease, ActivationEnd>> {
        if let Some(retry_in) = self.gate_wait(run_id) {
            // Core gave this run's install gate no answer a moment ago; it
            // is not asked again until the backoff passes.
            return Ok(Err(ActivationEnd::Deferred {
                detail: "waiting to ask core for the install status again".into(),
                retry_in,
            }));
        }
        let owner = self.config.owner.clone();
        let now = self.config.clock.now_ms();
        let claimed = self.shared.store.write(|tx| {
            if !runs::expire(tx, now, Some(run_id))?.is_empty() {
                return Ok(Err(ActivationEnd::NotRunnable {
                    state: RunState::Failed,
                }));
            }
            journal::wake_deferred(tx, now, Some(run_id))?;
            match runs::claim(tx, run_id, &owner, now) {
                Ok(lease) => Ok(Ok(lease)),
                Err(CoreError::SlotBusy { holder, .. }) => {
                    Ok(Err(ActivationEnd::Waiting { holder }))
                }
                Err(e) => Err(e),
            }
        });
        let claimed = match claimed {
            Ok(Err(end)) => {
                self.flush_refusals()?;
                if let ActivationEnd::NotRunnable { .. } = end
                    && let Ok(run) = self.run(run_id)
                {
                    // It expired just now: the slot it held is free.
                    self.shared.signal.bump();
                    self.wake_flow(&run.flow_id);
                }
                return Ok(Err(end));
            }
            Ok(Ok(lease)) => Ok(lease),
            Err(e) => Err(e),
        };
        match claimed {
            Ok(lease) => {
                self.at(
                    run_id,
                    Boundary::Claimed {
                        generation: lease.generation,
                    },
                )?;
                Ok(Ok(lease))
            }
            Err(CoreError::WrongState { state, .. }) => {
                Ok(Err(ActivationEnd::NotRunnable { state }))
            }
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
                let mut activation = Activation {
                    rt: self,
                    next_position: 0,
                    long_reported: BTreeSet::new(),
                    readiness_seen: run.readiness,
                    lease: lease.clone(),
                    run,
                    worker,
                    deadline: None,
                    manifest: None,
                    input: Value::Null,
                    trigger: Value::Null,
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
        // A domain error (anything but a store failure, a cut, or a lost
        // lease) concerns only this run; the store is still usable. Mark the
        // run failed, but only while this activation still owns it (same
        // owner and lease generation, state still `running`). Ending the run
        // releases its flow's concurrency slot, so the next run of the flow
        // is not stuck behind an activation that has already stopped.
        let end = match end {
            Err(e)
                if !matches!(
                    e,
                    CoreError::Store(_) | CoreError::Cut | CoreError::OwnerLost { .. }
                ) =>
            {
                let detail = e.to_string();
                match self.store().write(|tx| {
                    runs::exit(
                        tx,
                        &lease,
                        &Exit::Failed {
                            kind: "core".into(),
                            detail: detail.clone(),
                        },
                    )
                }) {
                    Ok(_) => Ok(ActivationEnd::Failed {
                        kind: "core".into(),
                        detail,
                    }),
                    Err(error) => Err(error),
                }
            }
            other => other,
        };
        let end = self.flush_refusals().and(end);
        if !idle {
            worker.kill();
        }
        self.shared.signal.bump();
        match end {
            Ok(ActivationEnd::Requeued) => {
                self.wake(&run_id);
                ActivationEnd::Requeued
            }
            Ok(end) => {
                // A run that reached a terminal state frees its flow's slot.
                if matches!(
                    end,
                    ActivationEnd::Succeeded { .. }
                        | ActivationEnd::Failed { .. }
                        | ActivationEnd::EngineMismatch { .. }
                        | ActivationEnd::Revoked { .. }
                ) && let Ok(run) = self.run(&run_id)
                {
                    self.wake_flow(&run.flow_id);
                }
                end
            }
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

/// The failure detail of a run that passed its wall-clock deadline.
const RUN_DEADLINE: &str = "the run passed its wall-clock deadline";

/// What a disabled flow's new calls are refused with.
fn disabled_refusal() -> Refusal {
    Refusal::new(
        codes::FLOW_DISABLED,
        "the flow is disabled; it makes no new calls",
    )
}

/// The audit outcome of a call the runtime served itself: allowed, or the
/// code it was refused with (a `kv` limit, invalid arguments).
fn outcome_label(outcome: &basal_host::HostOutcome) -> String {
    match outcome.settlement {
        Settlement::Fulfilled => audit::ALLOWED.to_owned(),
        Settlement::Rejected => serde_json::from_str::<Value>(outcome.value.as_str())
            .ok()
            .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "rejected".to_owned()),
    }
}

impl Activation<'_> {
    fn at(&self, boundary: Boundary) -> Result<()> {
        self.rt.at(&self.lease.run_id, boundary)
    }

    /// Whether the run's wall-clock deadline, fixed at its first claim, has
    /// passed on the runtime's clock.
    fn run_deadline_passed(&self) -> bool {
        self.run
            .deadline_at
            .is_some_and(|d| self.rt.config.clock.now_ms() >= d)
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
        // The install gate (`crate::gate`): the lease is held and nothing
        // has reached the worker or the host yet, not even a resend of a
        // call an earlier activation left open. A revoke that lands after
        // this check is deliberately not looked for again during this
        // activation. Core itself refuses a revoked version's sink writes
        // (digest items and status lines, the only way a flow reaches an
        // agent), and the next activation asks again, so what a revoked
        // version can still do is bounded by this one activation. The
        // worker was handed nothing, so it stays reusable.
        if let Gate::Closed(end) = self.rt.check_install(&self.lease, &self.run)? {
            return Ok(done(end, true));
        }
        if let Flow::Done { end, idle } = self.check_identity()? {
            return Ok(done(end, idle));
        }
        match Manifest::parse(&self.run.manifest) {
            Ok(mut manifest) => {
                self.input = serde_json::from_str(self.run.self_input.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                self.trigger = serde_json::from_str(self.run.trigger.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                if let Some(agent) = self.input["agent_id"].as_str() {
                    authorize::resolve_self(&mut manifest, agent);
                }
                self.manifest = Some(manifest);
            }
            Err(e) => {
                return self.fail(
                    "manifest",
                    format!("the run's approved manifest does not decode: {e}"),
                    true,
                );
            }
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
            self_input: self.run.self_input.clone(),
            budgets: self.rt.config.budgets,
            prefix,
        };
        if let Err(e) = self.send(&ParentMessage::Activate(Box::new(request))) {
            return self.broken(e.to_string());
        }
        self.at(Boundary::ActivateSent {
            generation: self.lease.generation,
        })?;
        let deadline = Instant::now() + self.rt.config.activation_deadline;
        self.deadline = Some(deadline);
        loop {
            if self.run_deadline_passed() {
                return self.fail("deadline", RUN_DEADLINE.into(), false);
            }
            let now = Instant::now();
            if now >= deadline {
                return self.fail(
                    "deadline",
                    "the activation ran out of wall time".into(),
                    false,
                );
            }
            let message = match self.worker.recv((deadline - now).min(WAIT_SLICE)) {
                Ok(m) => m,
                Err(ChannelError::Timeout) => {
                    // Between frames, notice a run failed or taken from
                    // outside (its deadline enforced, a cancel), so a worker
                    // busy in a loop is killed rather than left running.
                    let lease = self.lease.clone();
                    if !self.rt.store().read(|c| lease.holds(c))? {
                        return Err(self.lease.lost());
                    }
                    continue;
                }
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
                (_, DispatchState::Deferred) => {
                    let retry = self
                        .rt
                        .store()
                        .read(|c| journal::deferred_until(c, &run_id, row.position))?
                        .ok_or_else(|| {
                            CoreError::Corrupt("deferred call has no retry time".into())
                        })?;
                    if retry <= self.rt.config.clock.now_ms() {
                        resend.push(row.clone());
                    }
                }
                (StoredClass::Sync, _) | (_, DispatchState::Accepted) => {}
                (StoredClass::Local, _) => {
                    // Normally a local call and its outcome commit together.
                    // If only the row remains, recovery completes that intent.
                    let flow_id = self.run.flow_id.clone();
                    let limits = self.rt.config.kv;
                    self.rt.store().write(|tx| {
                        let outcome = kv::apply(tx, &flow_id, &row.kind, &row.args, &limits)?;
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
                    // No thread of this process is sending the call, so
                    // the process that sent it stopped before recording a
                    // reply.
                    journal::record_unknown(tx, &run_id, *p, UnknownReason::BasalRestarted)?;
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
            self.rt.spawn_dispatch(
                Runtime::request(&self.run, &row, attempt),
                row.class,
                matches!(
                    row.dispatch,
                    DispatchState::Deferred | DispatchState::NotApplied
                ),
            );
        }
        Ok(Flow::Continue)
    }

    /// The value of a clock read (kept monotonic within the run) or a random
    /// sample.
    fn sync_value(&self, kind: &CallKind) -> Result<(Settlement, JsonText, Option<f64>)> {
        match kind {
            CallKind::Primitive(Primitive::Now) => {
                let now = self.rt.shared.host.now_ms();
                Ok((Settlement::Fulfilled, number_text(now), Some(now)))
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
        let flow_id = self.run.flow_id.clone();
        // Clock reads and random samples need no grant. Every other call is
        // checked against the approved manifest first; a refusal is still
        // journaled, as a rejection, below.
        let prepared = if call.kind.is_synchronous() {
            Ok(Prepared::default())
        } else {
            self.prepare(&call)
        };
        let request = prepared.as_ref().ok().and_then(|p| p.request.as_ref());
        if let Some(flow) = self.check_run_limits(&call, request)? {
            return Ok(flow);
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => return self.refuse(&call, &flow_id, &refusal),
        };
        let class = self.rt.class_of(&call.kind, prepared.args.as_ref());
        if matches!(
            call.kind,
            CallKind::Primitive(Primitive::Llm | Primitive::Classify)
        ) {
            self.at(Boundary::ModelSelected {
                position: call.position,
            })?;
        }
        let new = NewCall {
            position: call.position,
            kind: &call.kind,
            args: &call.args,
            class,
            request: prepared.request.as_ref(),
        };
        let audited_at = self.rt.config.clock.now_ms();
        match class {
            StoredClass::Sync => {
                let (settlement, value, clock) = self.sync_value(&call.kind)?;
                let outcome = self.rt.store().write(|tx| {
                    journal::insert_call(tx, &self.lease, &flow_id, &new)?;
                    audit::record(
                        tx,
                        &flow_id,
                        &self.lease.run_id,
                        call.position,
                        &call.kind,
                        &ArgsDigest::of(&call.args),
                        audit::ALLOWED,
                        audited_at,
                    )?;
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
                let limits = self.rt.config.kv;
                let refused = self.rt.store().write(|tx| {
                    if install::is_disabled(tx, &flow_id)? {
                        self.record_refusal(tx, &flow_id, &call, &disabled_refusal())?;
                        return Ok(true);
                    }
                    journal::insert_call(tx, &self.lease, &flow_id, &new)?;
                    // The effect, its audit row and its outcome commit with
                    // the journal row: all of them or none.
                    let outcome = kv::apply(tx, &flow_id, &call.kind, &call.args, &limits)?;
                    audit::record(
                        tx,
                        &flow_id,
                        &self.lease.run_id,
                        call.position,
                        &call.kind,
                        &ArgsDigest::of(&call.args),
                        &outcome_label(&outcome),
                        audited_at,
                    )?;
                    journal::accept_outcome(
                        tx,
                        &self.lease.run_id,
                        call.position,
                        None,
                        &outcome,
                        Source::Local,
                    )?;
                    Ok(false)
                })?;
                self.next_position += 1;
                self.at(if refused {
                    Boundary::RefusalCommitted {
                        position: call.position,
                    }
                } else {
                    Boundary::LocalCommitted {
                        position: call.position,
                    }
                })?;
                self.rt.shared.signal.bump();
                Ok(Flow::Continue)
            }
            _ => {
                let run_id = self.lease.run_id.clone();
                self.rt.register(&run_id, call.position);
                let committed = self.rt.store().write(|tx| {
                    self.commit_remote(tx, &flow_id, &call, &new, &prepared, audited_at)
                });
                let key = match committed {
                    Ok(Some(key)) => key,
                    Ok(None) => {
                        // Refused inside the transaction (disabled flow,
                        // dispatch budget, token cap); nothing is sent.
                        self.rt.unregister(&run_id, call.position);
                        self.next_position += 1;
                        self.at(Boundary::RefusalCommitted {
                            position: call.position,
                        })?;
                        self.rt.shared.signal.bump();
                        return Ok(Flow::Continue);
                    }
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
                        args: prepared.request.unwrap_or(call.args),
                        idempotency_key: key,
                        attempt: 1,
                    },
                    class,
                    true,
                );
                Ok(Flow::Continue)
            }
        }
    }

    /// Checks a new call's argument size and its grant in the approved
    /// manifest, then selects a model and builds its clamped request. The
    /// selector runs before the intent transaction, so refusal sends no model work.
    fn prepare(&self, call: &HostCall) -> std::result::Result<Prepared, Refusal> {
        let cap = self.rt.config.limits.max_arg_bytes;
        if call.args.len() > cap {
            return Err(Refusal::new(
                codes::TOO_LARGE,
                format!(
                    "arguments of {} bytes exceed the cap of {cap}",
                    call.args.len()
                ),
            ));
        }
        let mut args: Value = serde_json::from_str(call.args.as_str())
            .map_err(|_| Refusal::new(codes::INVALID_ARGUMENTS, "arguments are not JSON"))?;
        if matches!(
            call.kind,
            CallKind::Primitive(Primitive::Llm | Primitive::Classify)
        ) && (args.get("model").is_some() || args.get("provider").is_some())
        {
            return Err(Refusal::new(
                "model_not_allowed",
                "scripts cannot select a provider or model",
            ));
        }
        let Some(manifest) = &self.manifest else {
            return Err(Refusal::denied("the run has no approved manifest"));
        };
        if matches!(
            call.kind,
            CallKind::Primitive(Primitive::SinkDigest | Primitive::SinkStatus | Primitive::Facts)
        ) && args["agent"] == "$self"
            && let Some(agent) = self.input["agent_id"].as_str()
        {
            args["agent"] = Value::String(agent.to_owned());
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("action").is_none_or(Value::is_null)
            && let Some(cap) = args
                .get("agent")
                .and_then(Value::as_str)
                .and_then(|agent| manifest.digest_cap(agent))
        {
            args["action"] = serde_json::to_value(cap)
                .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
        }
        authorize::check(
            manifest,
            &self.rt.config.shell_denylist,
            self.rt.catalog(),
            &call.kind,
            &args,
        )?;
        self.rt
            .shared
            .host
            .provider_ready(&self.run.flow_id, &call.kind)
            .map_err(|refusal| {
                let mut rejected = Refusal::new(refusal.reason.as_str(), refusal.message());
                rejected.module = Some(refusal.provider);
                rejected
            })?;
        let prepared: std::result::Result<Prepared, Refusal> = match &call.kind {
            CallKind::Primitive(p @ (Primitive::Llm | Primitive::Classify)) => {
                let Some(grant) = &manifest.llm else {
                    return Err(Refusal::denied("the manifest grants no model calls"));
                };
                let send_id = idempotency_key(&self.run.flow_id, &self.lease.run_id, call.position);
                let clamped = tokens::clamp(
                    *p,
                    &call.args,
                    &args,
                    grant,
                    tokens::Identity {
                        flow_id: &self.run.flow_id,
                        run_id: &self.lease.run_id,
                        send_id: &send_id,
                        position: call.position,
                    },
                )?;
                let selection_request = basal_host::selector::SelectionRequest {
                    iq: grant.iq,
                    eq: grant.eq,
                    flow_id: self.run.flow_id.clone(),
                    run_id: self.lease.run_id.clone(),
                    send_id,
                };
                let mut retries = 0;
                let selection = loop {
                    match self.rt.config.selector.select(&selection_request) {
                        Ok(selection) => break selection,
                        Err(basal_host::selector::SelectionError::Refused { code, detail }) => {
                            return Err(Refusal::new(code, detail));
                        }
                        Err(basal_host::selector::SelectionError::Unavailable { detail }) => {
                            if retries >= self.rt.config.unavailable_retries {
                                return Err(Refusal::new("route_unavailable", detail));
                            }
                            retries += 1;
                            std::thread::sleep(self.rt.config.retry_backoff);
                        }
                    }
                };
                let mut envelope: Value = serde_json::from_str(clamped.request.as_str())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                envelope["selection"] = serde_json::to_value(selection)
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                let request = JsonText::new(envelope.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: Some(clamped.reserve),
                    args: None,
                })
            }
            CallKind::Primitive(
                p @ (Primitive::SinkDigest | Primitive::SinkStatus | Primitive::Facts),
            ) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                let due_at = match self.trigger.get("due").and_then(Value::as_str) {
                    Some(due) => due
                        .parse::<jiff::Timestamp>()
                        .map_err(|_| {
                            Refusal::new(codes::INVALID_ARGUMENTS, "invalid schedule due time")
                        })?
                        .as_millisecond(),
                    None => self.run.admitted_at,
                };
                // Store the entire core request with the intent so a restart
                // reuses its timestamps instead of sampling a new clock.
                let value = basal_host::core_host::intent(
                    *p,
                    &args,
                    basal_host::core_host::IntentContext {
                        flow_id: &self.run.flow_id,
                        version,
                        run_id: &self.lease.run_id,
                        position: call.position,
                        due_at,
                        created_at: self.rt.config.clock.now_ms(),
                    },
                );
                let request = JsonText::new(value.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: None,
                    args: None,
                })
            }
            CallKind::Primitive(p) if p.is_builtin() => {
                // The scope travels with the call, so the host checks the
                // same scope again at the moment it acts, and a resend after
                // a crash carries the scope the call was approved under.
                let grant = authorize::builtin_grant(manifest, *p, &args)?;
                let request =
                    JsonText::new(basal_host::builtins::envelope(&args, &grant).to_string())
                        .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: None,
                    args: None,
                })
            }
            _ => Ok(Prepared::default()),
        };
        Ok(Prepared {
            args: Some(args),
            ..prepared?
        })
    }

    /// Checks the run's host-call and journal-size limits. Passing either
    /// fails the run before anything about the call is written, refusal
    /// included: a refusal would itself add a journal row.
    fn check_run_limits(
        &mut self,
        call: &HostCall,
        request: Option<&JsonText>,
    ) -> Result<Option<Flow>> {
        let limits = self.rt.config.limits;
        let run_id = self.lease.run_id.clone();
        let (calls, bytes) = self.rt.store().read(|c| journal::run_size(c, &run_id))?;
        if calls >= limits.max_calls {
            return self
                .fail(
                    "limit",
                    format!(
                        "the run reached its limit of {} host calls",
                        limits.max_calls
                    ),
                    false,
                )
                .map(Some);
        }
        let incoming = (call.args.len() + request.map_or(0, JsonText::len)) as u64;
        if bytes.saturating_add(incoming) > limits.max_journal_bytes {
            return self
                .fail(
                    "limit",
                    format!(
                        "the run's journal would pass its limit of {} bytes",
                        limits.max_journal_bytes
                    ),
                    false,
                )
                .map(Some);
        }
        Ok(None)
    }

    /// Journals a refused call: its row, its audit row and its rejection,
    /// in the caller's transaction. The row is stored as served locally: it
    /// was never sent, and its outcome is already known.
    fn record_refusal(
        &self,
        tx: &Transaction,
        flow_id: &str,
        call: &HostCall,
        refusal: &Refusal,
    ) -> Result<()> {
        let new = NewCall {
            position: call.position,
            kind: &call.kind,
            args: &call.args,
            class: StoredClass::Local,
            request: None,
        };
        journal::insert_call(tx, &self.lease, flow_id, &new)?;
        if let Some(module) = &refusal.module {
            let reason = match refusal.code.as_str() {
                "no_flow_scope" => Some(basal_host::flow_refusal::RefusalReason::NoFlowScope),
                "target_flow_unsupported" => {
                    Some(basal_host::flow_refusal::RefusalReason::TargetFlowUnsupported)
                }
                _ => None,
            };
            if let Some(reason) = reason {
                crate::flow_scope::health(
                    tx,
                    flow_id,
                    &basal_host::flow_refusal::FlowRefusal::new(
                        reason,
                        module,
                        &basal_host::op_label(&call.kind),
                    ),
                )?;
            }
        }
        audit::record(
            tx,
            flow_id,
            &self.lease.run_id,
            call.position,
            &call.kind,
            &ArgsDigest::of(&call.args),
            &refusal.code,
            self.rt.config.clock.now_ms(),
        )?;
        journal::accept_outcome(
            tx,
            &self.lease.run_id,
            call.position,
            None,
            &refusal.outcome(),
            Source::Local,
        )?;
        Ok(())
    }

    /// Journals a call refused by a check that needs no store (argument
    /// size, valid JSON, the manifest), in its own transaction.
    fn refuse(&mut self, call: &HostCall, flow_id: &str, refusal: &Refusal) -> Result<Flow> {
        self.rt
            .store()
            .write(|tx| self.record_refusal(tx, flow_id, call, refusal))?;
        self.next_position += 1;
        self.at(Boundary::RefusalCommitted {
            position: call.position,
        })?;
        self.rt.shared.signal.bump();
        Ok(Flow::Continue)
    }

    /// In one transaction, the checks that need the store and the journal
    /// row of a remote call: a disabled flow and the dispatch budget refuse
    /// it; a model call reserves its tokens or is refused. Returns the
    /// call's idempotency key when it may be dispatched, or `None` when it
    /// was journaled as refused.
    fn commit_remote(
        &self,
        tx: &Transaction,
        flow_id: &str,
        call: &HostCall,
        new: &NewCall<'_>,
        prepared: &Prepared,
        now_ms: i64,
    ) -> Result<Option<String>> {
        let rate = self.rt.config.rate;
        if install::is_disabled(tx, flow_id)? {
            self.record_refusal(tx, flow_id, call, &disabled_refusal())?;
            return Ok(None);
        }
        if let Take::Refused { .. } = rate::check(tx, flow_id, rate::Kind::Dispatch, now_ms, &rate)?
        {
            let refusal = Refusal::new(
                codes::DISPATCH_BUDGET,
                format!(
                    "the flow's budget of {} dispatches per window is spent",
                    rate.max_dispatches
                ),
            );
            self.record_refusal(tx, flow_id, call, &refusal)?;
            return Ok(None);
        }
        let key = idempotency_key(flow_id, &self.lease.run_id, call.position);
        if let Some(amount) = prepared.tokens {
            let Some(manifest) = &self.manifest else {
                return Err(CoreError::Invalid("a model call without a manifest".into()));
            };
            let (Some(window_ms), Some(grant)) = (manifest.token_window_ms(), &manifest.llm) else {
                return Err(CoreError::Invalid(
                    "a model call without a token cap".into(),
                ));
            };
            let reservation = Reservation {
                send_id: &key,
                flow_id,
                run_id: &self.lease.run_id,
                position: call.position,
                window_ms,
                cap: grant.token_cap.tokens,
                amount,
                now_ms,
            };
            if let Err(refusal) = tokens::reserve(tx, &reservation)? {
                self.record_refusal(tx, flow_id, call, &refusal)?;
                return Ok(None);
            }
        }
        rate::count(tx, flow_id, rate::Kind::Dispatch, now_ms, &rate)?;
        let inserted = journal::insert_call(tx, &self.lease, flow_id, new)?;
        audit::record(
            tx,
            flow_id,
            &self.lease.run_id,
            call.position,
            &call.kind,
            &ArgsDigest::of(&call.args),
            audit::ALLOWED,
            now_ms,
        )?;
        Ok(Some(inserted))
    }

    /// The worker issued a call at a position the journal already holds.
    /// That is legitimate only for a synchronous call whose row holds no
    /// recorded value: the value is produced, recorded and answered at that
    /// position, as for a new call.
    /// A call of a different kind or with different arguments at that
    /// position means the replayed worker diverged from the journal, and the
    /// run fails as nondeterministic; any other repeat breaks the protocol.
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
            // Recovery must use the same monotonic clock rule as a new call.
            let clock = match clock {
                Some(sample) => {
                    Some(sample.max(journal::last_clock(tx, &self.lease.run_id)?.unwrap_or(sample)))
                }
                None => None,
            };
            let value = clock.map_or(value.clone(), number_text);
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
        let deadline = self
            .deadline
            .ok_or_else(|| CoreError::Corrupt("blocked before activation".into()))?;
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
            // Read the acceptances and the readiness sequence together (the
            // store's one connection serialises every writer), so an
            // acceptance committed after the release attempt above does not
            // later look like an unseen arrival and requeue the run for
            // nothing.
            let (accepted, readiness) = self.rt.store().read(|c| {
                Ok((
                    journal::accepted_positions(c, &run_id)?,
                    runs::readiness(c, &run_id)?,
                ))
            })?;
            self.readiness_seen = readiness;
            let long: Vec<u64> = accepted
                .into_iter()
                .filter(|p| awaiting.contains(p) && !self.long_reported.contains(p))
                .collect();
            if !long.is_empty() {
                self.long_reported.extend(long.iter().copied());
                if let Err(e) = self.send(&ParentMessage::LongRunning { positions: long }) {
                    return self.broken(e.to_string());
                }
                self.at(Boundary::LongRunningSent)?;
                return Ok(Flow::Continue);
            }
            if self.run_deadline_passed() {
                return self.fail("deadline", RUN_DEADLINE.into(), false);
            }
            let now = Instant::now();
            if now >= deadline {
                return self.fail(
                    "deadline",
                    format!("still waiting on {awaiting:?} when the activation ran out of time"),
                    false,
                );
            }
            self.rt
                .shared
                .signal
                .wait(seen, (deadline - now).min(WAIT_SLICE));
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
            ActivationResult::Failed(Failure::ScriptHostRejection { position, message }) => {
                let readiness = self.rt.store().read(|c| {
                    Ok(
                        journal::row(c, &self.lease.run_id, position)?.is_some_and(|row| {
                            row.outcome.as_ref().is_some_and(|outcome| {
                                outcome.settlement == Settlement::Rejected
                                    && crate::ops::typed_scope_rejection(
                                        outcome.value.as_str(),
                                        true,
                                    )
                            })
                        }),
                    )
                })?;
                self.fail(
                    if readiness {
                        "readiness_rejection"
                    } else {
                        "script"
                    },
                    message,
                    true,
                )
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
