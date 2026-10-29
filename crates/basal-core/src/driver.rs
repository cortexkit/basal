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
    tool_policies: crate::classify_cost::ToolPolicies,
}

/// A call ready to journal, including a board call waiting for its registry read.
#[derive(Debug, Default)]
struct Prepared {
    /// Prepared dispatch bytes, rather than the script's arguments. Absent
    /// while a board or targeted call's agent registry read is deferred.
    request: Option<JsonText>,
    /// Tokens to reserve for a model call.
    tokens: Option<u64>,
    classify_cost: bool,
    args: Option<Value>,
    /// A failed agent registry read is retried without dispatching the call.
    deferred: Option<basal_host::flow_refusal::FlowRefusal>,
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
            if self.is_stopping() {
                return Ok(Err(ActivationEnd::OwnerLost));
            }
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
                    if run.state.is_terminal() {
                        self.clear_event_scope_warning(run_id);
                        self.release_unused_flow_routes_for(&run.flow_id)?;
                    }
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
                // The first claim arms the run's wall-clock deadline. The
                // engine may already be asleep on an older timer snapshot.
                self.shared.signal.bump();
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
                    tool_policies: Default::default(),
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
                    if self.is_stopping() {
                        return Err(lease.lost());
                    }
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
                let terminal = matches!(
                    end,
                    ActivationEnd::Succeeded { .. }
                        | ActivationEnd::Failed { .. }
                        | ActivationEnd::EngineMismatch { .. }
                        | ActivationEnd::Revoked { .. }
                );
                if terminal {
                    self.clear_event_scope_warning(&run_id);
                    if let Ok(run) = self.run(&run_id) {
                        let _ = self.release_unused_flow_routes_for(&run.flow_id);
                        self.wake_flow(&run.flow_id);
                    }
                }
                end
            }
            Err(CoreError::OwnerLost { .. }) => ActivationEnd::OwnerLost,
            Err(CoreError::Unavailable(_)) if self.is_stopping() => ActivationEnd::OwnerLost,
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
    fn write<T>(&self, write: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        self.rt.store().write(|tx| {
            // Shutdown revokes execution in memory, not by changing the durable
            // run or settling a call. Recovery must see the last committed state.
            if self.rt.is_stopping() {
                return Err(self.lease.lost());
            }
            write(tx)
        })
    }

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
        let state = self.write(|tx| runs::exit(tx, &self.lease, &exit))?;
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

    fn end_revoked(&self, detail: String) -> Result<Flow> {
        let unknown = self.write(|tx| {
            let unknown = journal::unknown_positions(tx, &self.lease.run_id)?;
            let exit = if unknown.is_empty() {
                Exit::Failed {
                    kind: "version_revoked".into(),
                    detail: detail.clone(),
                }
            } else {
                Exit::NeedsReconcile {
                    positions: unknown.clone(),
                }
            };
            runs::exit(tx, &self.lease, &exit)?;
            Ok(unknown)
        })?;
        self.at(Boundary::Exited)?;
        if !unknown.is_empty() {
            return Ok(done(
                ActivationEnd::NeedsReconcile { positions: unknown },
                false,
            ));
        }
        Ok(done(
            ActivationEnd::Failed {
                kind: "version_revoked".into(),
                detail,
            },
            false,
        ))
    }

    fn finish_revoked_call(&mut self, call: &HostCall, reason: String) -> Result<Flow> {
        let (outcome, _) =
            self.write(|tx| journal::release_next(tx, &self.lease, &[call.position]))?;
        if let Some(outcome) = outcome {
            // Commit the typed refusal before ending the run. A script catching
            // this reply cannot dispatch again because its worker is retired.
            let _ = self.send(&ParentMessage::Deliver(outcome));
        }
        self.end_revoked(reason)
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
        if self.rt.is_stopping() {
            return Err(self.lease.lost());
        }
        // The install gate (`crate::gate`): the lease is held and nothing
        // has reached the worker or the host yet, not even a resend of a
        // call an earlier activation left open. Verified pushes can record
        // revocation afterward; each dispatch attempt checks that durable
        // record without another network read. The worker was handed nothing
        // when this gate closes, so it stays reusable.
        if let Gate::Closed(end) = self.rt.check_install(&self.lease, &self.run)? {
            return Ok(done(end, true));
        }
        if let Flow::Done { end, idle } = self.check_identity()? {
            return Ok(done(end, idle));
        }
        match Manifest::parse_stored(&self.run.manifest) {
            Ok(mut manifest) => {
                self.input = serde_json::from_str(self.run.self_input.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                self.trigger = serde_json::from_str(self.run.trigger.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                if let Some(agent) = self.input["agent_id"].as_str() {
                    authorize::resolve_self(&mut manifest, agent);
                }
                if manifest.params.is_some() {
                    let params = serde_json::from_str(self.run.params_input.as_str())
                        .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    let guards: Vec<basal_host::builtins::TemplateRoot> =
                        serde_json::from_str(self.run.grant_guards.as_str())
                            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    crate::templates::effective_instance(&mut manifest, &params, &guards);
                }
                self.manifest = Some(manifest);
                if let Some(version) = self.run.flow_version {
                    self.tool_policies = self.rt.store().read(|conn| {
                        crate::classify_cost::load(conn, &self.run.flow_id, version)
                    })?;
                }
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
        let trigger = match self
            .rt
            .event_preamble(&self.run, self.manifest.as_ref().expect("parsed manifest"))?
        {
            crate::events::Preamble::Ready(trigger) => trigger,
            crate::events::Preamble::Retry => {
                self.exit(Exit::Requeue { broken: false })?;
                return Ok(done(ActivationEnd::Requeued, true));
            }
            crate::events::Preamble::Failed { kind, detail } => {
                return self.fail(&kind, detail, true);
            }
        };
        self.trigger = serde_json::from_str(trigger.as_str())
            .map_err(|e| CoreError::Corrupt(e.to_string()))?;
        let prefix = self
            .rt
            .store()
            .read(|c| journal::prefix(c, &self.lease.run_id))?;
        self.next_position = prefix.len() as u64;
        if self.run_deadline_passed() {
            return self.fail("deadline", RUN_DEADLINE.into(), true);
        }
        let request = ActivationRequest {
            activation_id: self.lease.generation,
            profile: Profile::Flow,
            tools: vec![],
            prelude_hash: self.worker.welcome().prelude_hash,
            script: self.run.script.clone(),
            trigger,
            self_input: self.run.self_input.clone(),
            params_input: self.run.params_input.clone(),
            budgets: self.rt.config.budgets,
            prefix,
        };
        self.write(|tx| {
            if !self.lease.holds(tx)? {
                return Err(self.lease.lost());
            }
            tx.execute(
                "UPDATE runs SET script_started=1 WHERE run_id=?1",
                [&self.lease.run_id],
            )?;
            Ok(())
        })?;
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
                WorkerMessage::Console { .. } => {
                    return self.fail(
                        "profile_violation",
                        "console output is not available to flows".into(),
                        false,
                    );
                }
                WorkerMessage::Warning { .. } => {
                    return self.fail(
                        "profile_violation",
                        "rejection warnings are not available to flows".into(),
                        false,
                    );
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
                self.write(|tx| runs::record_fingerprint(tx, &self.lease, &current))?;
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
    fn recover_calls(&self) -> Result<Flow> {
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
                    let revoked = self.write(|tx| {
                        if let Some(reason) = crate::revocation::dispatch_check(tx, &run_id)? {
                            return Ok(Some(reason));
                        }
                        let outcome =
                            if matches!(row.kind, CallKind::Primitive(Primitive::InstanceEnd)) {
                                crate::templates::request_end(tx, &run_id, &row.args)?
                            } else {
                                kv::apply(tx, &flow_id, &row.kind, &row.args, &limits)?
                            };
                        journal::accept_outcome(
                            tx,
                            &run_id,
                            row.position,
                            None,
                            &outcome,
                            Source::Local,
                        )?;
                        Ok(None)
                    })?;
                    if let Some(reason) = revoked {
                        return self.end_revoked(reason);
                    }
                    self.at(Boundary::LocalCommitted {
                        position: row.position,
                    })?;
                }
                (_, DispatchState::NotApplied) => resend.push(row.clone()),
                (class, _) if row.scope_cut && class != StoredClass::Query => {
                    unknown.push(row.position)
                }
                (class, _) if class.safe_to_resend() => resend.push(row.clone()),
                _ => unknown.push(row.position),
            }
        }
        if !unknown.is_empty() {
            self.write(|tx| {
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
        for mut row in resend {
            self.rt.register(&self.run, row.position);
            let attempt =
                match self.write(|tx| journal::authorize_resend(tx, &self.lease, row.position)) {
                    Ok(a) => a,
                    Err(e) => {
                        self.rt.unregister(&run_id, row.position);
                        return Err(e);
                    }
                };
            self.at(Boundary::ResendAuthorized {
                position: row.position,
            })?;
            // A board or targeted call deferred on its agent registry read has
            // no frozen request yet: prepare it again now that its retry is due.
            let pending_board = row.request.is_none()
                && matches!(
                    row.kind,
                    CallKind::Primitive(Primitive::SinkBoard) | CallKind::OpFor { .. }
                );
            if pending_board {
                match self.retry_board_preparation(&mut row, attempt) {
                    Ok(None) => {}
                    result => {
                        self.rt.unregister(&run_id, row.position);
                        match result? {
                            Some(Flow::Continue) => continue,
                            Some(done) => return Ok(done),
                            None => unreachable!(),
                        }
                    }
                }
            }
            let mut request = Runtime::request(&self.run, &row, attempt);
            if matches!(request.kind, CallKind::OpFor { .. }) {
                let mut envelope: Value = serde_json::from_str(request.args.as_str())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
                envelope["replay"] = Value::Bool(true);
                request.args = JsonText::new(envelope.to_string())
                    .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            }
            self.rt.spawn_dispatch(
                request,
                row.class,
                pending_board
                    || matches!(
                        row.dispatch,
                        DispatchState::Deferred | DispatchState::NotApplied
                    ),
            );
        }
        Ok(Flow::Continue)
    }

    /// A board or targeted call with no prepared request has never been sent.
    /// Resolve it again when its durable retry is due, then freeze the stable
    /// target id before dispatch so later retries need no registry read.
    fn retry_board_preparation(
        &self,
        row: &mut crate::model::CallRow,
        attempt: u32,
    ) -> Result<Option<Flow>> {
        let call = HostCall {
            position: row.position,
            kind: row.kind.clone(),
            args: row.args.clone(),
        };
        let prepared = match self.prepare(&call) {
            Ok(prepared) => prepared,
            Err(refusal) => {
                self.rt.accept(
                    &self.lease.run_id,
                    row.position,
                    None,
                    &refusal.outcome(),
                    Source::Local,
                )?;
                return Ok(Some(Flow::Continue));
            }
        };
        if let Some(refusal) = prepared.deferred {
            self.rt
                .defer_dispatch_refusal(&Runtime::request(&self.run, row, attempt), &refusal)?;
            return Ok(Some(Flow::Continue));
        }
        let request = prepared
            .request
            .ok_or_else(|| CoreError::Corrupt("board preparation produced no request".into()))?;
        let fits = self.write(|tx| {
            journal::freeze_request(
                tx,
                &self.lease,
                row.position,
                &request,
                self.rt.config.limits.max_journal_bytes,
            )
        })?;
        if !fits {
            return self
                .fail(
                    "limit",
                    format!(
                        "the run's journal would pass its limit of {} bytes",
                        self.rt.config.limits.max_journal_bytes
                    ),
                    false,
                )
                .map(Some);
        }
        row.request = Some(request);
        Ok(None)
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
        if let Some(reason) = self
            .rt
            .store()
            .read(|c| crate::revocation::dispatch_check(c, &self.lease.run_id))?
        {
            if call.position == self.next_position {
                self.refuse(
                    &call,
                    &self.run.flow_id.clone(),
                    &Refusal::new("version_revoked", &reason),
                )?;
            } else if call.position < self.next_position {
                self.write(|tx| {
                    if journal::row(tx, &self.lease.run_id, call.position)?
                        .is_some_and(|r| r.class == StoredClass::Sync && r.outcome.is_none())
                    {
                        journal::accept_outcome(
                            tx,
                            &self.lease.run_id,
                            call.position,
                            None,
                            &kv::rejection("version_revoked", &reason),
                            Source::Local,
                        )?;
                    }
                    Ok(())
                })?;
            }
            return self.finish_revoked_call(&call, reason);
        }
        if matches!(call.kind, CallKind::Tool { .. }) {
            return self.fail(
                "profile_violation",
                format!("{} is not available to flows", call.kind),
                false,
            );
        }
        // Codemode's sleep has no place in a flow: a flow activation is
        // journaled and replayed, and the flow prelude never offers it, so a
        // worker that sends one is compromised. It is stopped before anything
        // is journaled.
        if call.kind.is_sleep() {
            return self.fail(
                "profile_violation",
                format!("{} is not available to flows", call.kind),
                false,
            );
        }
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
        // A classifier can spend money even when its provider declares a query.
        // An unknown send is reconciled, never automatically dispatched again.
        let class = if prepared.classify_cost {
            StoredClass::Mutation
        } else {
            self.rt.class_of(&call.kind, prepared.args.as_ref())
        };
        self.commit_host_call(call, flow_id, prepared, class)
    }

    fn commit_host_call(
        &mut self,
        call: HostCall,
        flow_id: String,
        mut prepared: Prepared,
        class: StoredClass,
    ) -> Result<Flow> {
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
            StoredClass::Sync => self.commit_sync_call(&call, &flow_id, &new, audited_at),
            StoredClass::Local => self.commit_local_call(&call, &flow_id, &new, audited_at),
            _ => {
                let run_id = self.lease.run_id.clone();
                self.rt.register(&self.run, call.position);
                let committed = self.write(|tx| {
                    self.commit_remote(tx, &flow_id, &call, class, &mut prepared, audited_at)
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
                        if let Some(reason) = self
                            .rt
                            .store()
                            .read(|c| crate::revocation::dispatch_check(c, &run_id))?
                        {
                            return self.finish_revoked_call(&call, reason);
                        }
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
                let request = basal_host::CallRequest {
                    flow_version: self.run.flow_version,
                    flow_id,
                    run_id,
                    position: call.position,
                    kind: call.kind,
                    args: prepared.request.unwrap_or(call.args),
                    idempotency_key: key,
                    attempt: 1,
                };
                if let Some(refusal) = prepared.deferred {
                    let result = self.rt.defer_dispatch_refusal(&request, &refusal);
                    self.rt.unregister(&request.run_id, request.position);
                    result?;
                } else {
                    self.rt.spawn_dispatch(request, class, true);
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn commit_local_call(
        &mut self,
        call: &HostCall,
        flow_id: &str,
        new: &NewCall<'_>,
        audited_at: i64,
    ) -> Result<Flow> {
        let limits = self.rt.config.kv;
        let refused = self.write(|tx| {
            if let Some(reason) = crate::revocation::dispatch_check(tx, &self.lease.run_id)? {
                self.record_refusal(tx, flow_id, call, &Refusal::new("version_revoked", reason))?;
                return Ok(true);
            }
            if install::is_disabled(tx, flow_id)? {
                self.record_refusal(tx, flow_id, call, &disabled_refusal())?;
                return Ok(true);
            }
            journal::insert_call(tx, &self.lease, flow_id, new)?;
            // The effect, its audit row and its outcome commit with
            // the journal row: all of them or none.
            let outcome = if matches!(call.kind, CallKind::Primitive(Primitive::InstanceEnd)) {
                crate::templates::request_end(tx, &self.lease.run_id, &call.args)?
            } else {
                kv::apply(tx, flow_id, &call.kind, &call.args, &limits)?
            };
            audit::record(
                tx,
                flow_id,
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
        if let Some(reason) = self
            .rt
            .store()
            .read(|c| crate::revocation::dispatch_check(c, &self.lease.run_id))?
        {
            return self.finish_revoked_call(call, reason);
        }
        Ok(Flow::Continue)
    }

    fn commit_sync_call(
        &mut self,
        call: &HostCall,
        flow_id: &str,
        new: &NewCall<'_>,
        audited_at: i64,
    ) -> Result<Flow> {
        let outcome = self.write(|tx| {
            if let Some(reason) = crate::revocation::dispatch_check(tx, &self.lease.run_id)? {
                self.record_refusal(tx, flow_id, call, &Refusal::new("version_revoked", &reason))?;
                return Ok(Err(reason));
            }
            let (settlement, value, clock) = self.sync_value(&call.kind)?;
            journal::insert_call(tx, &self.lease, flow_id, new)?;
            audit::record(
                tx,
                flow_id,
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
            journal::record_sync(tx, &self.lease, call.position, settlement, &value, clock).map(Ok)
        })?;
        self.next_position += 1;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => return self.finish_revoked_call(call, reason),
        };
        self.at(Boundary::SyncCommitted {
            position: call.position,
        })?;
        self.reply(outcome)
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
            CallKind::Primitive(
                Primitive::SinkDigest
                    | Primitive::SinkStatus
                    | Primitive::SinkBoard
                    | Primitive::Facts
            )
        ) && args["agent"] == "$self"
            && let Some(agent) = self.input["agent_id"].as_str()
        {
            args["agent"] = Value::String(agent.to_owned());
        }
        // Before the board target is looked up in the registry, so a
        // placeholder is refused for what it is rather than as an unknown agent.
        authorize::placeholder_target(manifest, &call.kind, &args)?;
        let resolved_manifest = if matches!(call.kind, CallKind::Primitive(Primitive::SinkBoard))
            && !manifest.board.is_empty()
        {
            let target = args["agent"].as_str().ok_or_else(|| {
                Refusal::new(
                    codes::INVALID_ARGUMENTS,
                    "sink.board needs an agent reference",
                )
            })?;
            let mut refs = vec![target];
            if manifest.audience.is_none() {
                refs.extend(manifest.board.iter().map(|grant| grant.agent.as_str()));
            }
            // Resolve the target and every grant from the same registry read,
            // so aliases are compared as stable ids without a second lookup.
            let resolved = match self.rt.catalog().resolve_agents(&refs) {
                Ok(ids) if ids.len() == refs.len() => ids,
                _ => {
                    return Ok(Prepared {
                        deferred: Some(basal_host::flow_refusal::FlowRefusal::new(
                            basal_host::flow_refusal::RefusalReason::SinkBoardTargetUnavailable,
                            basal_host::subc_catalog::CORE,
                            "agent.list",
                        )),
                        args: Some(args),
                        ..Prepared::default()
                    });
                }
            };
            let agent = resolved[0].as_ref().ok_or_else(|| {
                Refusal::new(
                    "sink_board_target_not_granted",
                    "board target is not in the agent registry",
                )
            })?;
            args["agent"] = Value::String(agent.clone());
            let mut resolved_manifest = manifest.clone();
            if manifest.audience.is_none() {
                resolved_manifest.board = manifest
                    .board
                    .iter()
                    .zip(&resolved[1..])
                    .filter_map(|(grant, id)| {
                        id.as_ref().map(|id| {
                            let mut grant = grant.clone();
                            grant.agent = id.clone();
                            grant
                        })
                    })
                    .collect();
            }
            Some(resolved_manifest)
        } else {
            None
        };
        let manifest = resolved_manifest.as_ref().unwrap_or(manifest);
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) != Some(true)
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
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkWaitResolve))
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_WAIT_RESOLVE_V1)
        {
            return Err(Refusal::new(
                "wait_resolve_unsupported",
                "prefrontal-core does not currently advertise flow-wait-resolve/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkRoomPost))
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_ROOM_POST_V1)
        {
            return Err(Refusal::new(
                codes::ROOM_POST_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-room-post/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) != Some(true)
            && args
                .get("options")
                .and_then(|options| options.get("replace_key"))
                .is_some()
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_DIGEST_REPLACE_KEY_V1)
        {
            return Err(Refusal::new(
                codes::DIGEST_REPLACE_KEY_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-digest-replace-key/v1",
            ));
        }
        if matches!(call.kind, CallKind::Primitive(Primitive::SinkDigest))
            && args.get("withdraw").and_then(Value::as_bool) == Some(true)
            && !self
                .rt
                .shared
                .host
                .supports_core_capability(basal_host::core_host::FLOW_DIGEST_WITHDRAW_V1)
        {
            return Err(Refusal::new(
                codes::DIGEST_WITHDRAW_UNSUPPORTED,
                "prefrontal-core does not currently advertise flow-digest-withdraw/v1",
            ));
        }
        if !matches!(call.kind, CallKind::OpFor { .. }) {
            self.rt
                .shared
                .host
                .provider_ready(
                    &basal_host::flow_scope::route_key(&self.run.flow_id, self.run.flow_version),
                    &call.kind,
                )
                .map_err(|refusal| {
                    let mut rejected = Refusal::new(refusal.reason.as_str(), refusal.message());
                    rejected.module = Some(refusal.provider);
                    rejected
                })?;
        }
        let classify_cost = match &call.kind {
            CallKind::Op { module, op } => {
                match self.tool_policies.get(&(module.clone(), op.clone())) {
                    Some(crate::classify_cost::ToolPolicy::Classify) => true,
                    Some(
                        crate::classify_cost::ToolPolicy::Ordinary
                        | crate::classify_cost::ToolPolicy::LegacyOrdinary,
                    ) => false,
                    None => {
                        return Err(Refusal::new(
                            "tool_policy_missing",
                            "the approved version has no saved policy for this op",
                        ));
                    }
                }
            }
            _ => false,
        };
        let prepared: std::result::Result<Prepared, Refusal> = match &call.kind {
            CallKind::Op { .. } if classify_cost => {
                if manifest.model_cost_usd.is_none() {
                    return Err(Refusal::new(
                        "model_cost_cap_required",
                        "the manifest grants no classifier dollar cap",
                    ));
                }
                Ok(Prepared {
                    classify_cost: true,
                    ..Prepared::default()
                })
            }
            CallKind::Op { module, op }
                if basal_host::core_host::is_core_carrier_op(module, op) =>
            {
                let object = args.as_object_mut().ok_or_else(|| {
                    Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "core carrier arguments must be an object",
                    )
                })?;
                if ["stamp", "flow_id", "flow_version", "code_hash"]
                    .iter()
                    .any(|field| object.contains_key(*field))
                {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "scripts cannot supply an approved-install stamp",
                    ));
                }
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Journal the trusted stamp with the request so a replay uses
                // the same approved install, even after a newer version exists.
                object.insert(
                    "stamp".into(),
                    serde_json::json!({
                        "flow_id": self.run.flow_id,
                        "flow_version": version,
                        "code_hash": crate::ids::hex(&self.run.code_hash),
                    }),
                );
                let request = JsonText::new(args.to_string())
                    .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    ..Prepared::default()
                })
            }
            CallKind::OpFor { .. } => {
                if let CallKind::OpFor { module, op } = &call.kind
                    && basal_host::core_host::is_core_carrier_op(module, op)
                {
                    return Err(Refusal::new(
                        codes::INVALID_ARGUMENTS,
                        "core carrier ops select their audience in arguments, not the for option",
                    ));
                }
                let name = args
                    .get("for")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Refusal::new(codes::INVALID_ARGUMENTS, "for must name an agent")
                    })?;
                let agent = match self.rt.catalog().agent_id(name) {
                    Ok(Some(agent)) => agent,
                    Ok(None) => {
                        return Err(Refusal::new(
                            "sink_audience_unavailable",
                            "target agent is not in the agent registry",
                        ));
                    }
                    // A failed registry read is not evidence that the target is
                    // unknown. Defer the provably unsent call with the same
                    // durable backoff, bounded by the run deadline, that core's
                    // own transient audience refusal uses.
                    Err(_) => {
                        return Ok(Prepared {
                            deferred: Some(basal_host::flow_refusal::FlowRefusal::new(
                                basal_host::flow_refusal::RefusalReason::SinkAudienceUnavailable,
                                basal_host::subc_catalog::CORE,
                                "agent.list",
                            )),
                            args: Some(args),
                            ..Prepared::default()
                        });
                    }
                };
                let targets = self.rt.store().read(|c| {
                    let mut stmt = c.prepare("SELECT DISTINCT json_extract(request,'$.agent') FROM journal WHERE run_id=?1 AND targeted=1 AND request IS NOT NULL")?;
                    Ok(stmt.query_map([&self.run.run_id], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
                }).map_err(|e| Refusal::new("store_failure", e.to_string()))?;
                if !targets.contains(&agent) && targets.len() >= manifest.max_targets as usize {
                    return Err(Refusal::new(
                        "too_many_targets",
                        "distinct target limit reached",
                    ));
                }
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("target calls require an installed version"))?;
                let expires_at_ms = self
                    .run
                    .deadline_at
                    .ok_or_else(|| Refusal::denied("target calls require a run deadline"))?;
                let request = JsonText::new(serde_json::json!({"agent":agent,"version":version,"expires_at_ms":expires_at_ms,"args":args.get("args").unwrap_or(&Value::Null)}).to_string())
                    .map_err(|e| Refusal::new(codes::TOO_LARGE,e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    ..Prepared::default()
                })
            }
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
                    caller_class: "flow".into(),
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
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkWaitResolve) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Freeze the approved identity and outcome before sending. A
                // lost reply must reuse the same payload and receipt on retry.
                let mut value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "outcome":args["outcome"],
                });
                if let Some(note) = args.get("note") {
                    value["note"] = note.clone();
                }
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkRoomPost) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Freeze the approved identity, room, text and mentions before
                // sending, so a retry after a lost reply posts the same
                // message under the same receipt instead of a newer install's.
                // Core requires a list, so a post that names no one (options
                // omitted, or `mentions: []`) sends an empty one.
                let mentions = match args.get("mentions") {
                    Some(list @ Value::Array(_)) => list.clone(),
                    _ => Value::Array(Vec::new()),
                };
                let value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "room_id":args["room_id"], "text":args["text"], "mentions":mentions,
                });
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(Primitive::SinkBoard) => {
                let version = self
                    .run
                    .flow_version
                    .ok_or_else(|| Refusal::denied("core calls require an installed version"))?;
                // Record the flow version, code hash and board operation before
                // sending, so recovery cannot switch to a newer install. The
                // host adds receipt_key from this call's journal idempotency key
                // so core can return the original result after a lost reply.
                let value = serde_json::json!({
                    "stamp": {"flow_id":self.run.flow_id, "flow_version":version,
                        "code_hash":crate::ids::hex(&self.run.code_hash)},
                    "agent":args["agent"], "operation":args["operation"],
                });
                Ok(Prepared {
                    request: Some(
                        JsonText::new(value.to_string())
                            .map_err(|e| Refusal::new(codes::TOO_LARGE, e.to_string()))?,
                    ),
                    ..Prepared::default()
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
                    ..Prepared::default()
                })
            }
            CallKind::Primitive(p) if p.is_builtin() => {
                // The scope travels with the call, so the host checks the
                // same scope again at the moment it acts, and a resend after
                // a crash carries the scope the call was approved under.
                let grant = authorize::builtin_grant(manifest, *p, &args)?;
                let mut envelope = basal_host::builtins::envelope(&args, &grant);
                if *p == Primitive::NetFetch && args["options"].get("secret").is_some() {
                    envelope["flow_code_hash"] =
                        serde_json::json!(crate::ids::hex(&self.run.code_hash));
                    envelope["credential"] =
                        serde_json::json!({"name":args["options"]["secret"],"value":"[secret]"});
                }
                let request = JsonText::new(envelope.to_string())
                    .map_err(|e| Refusal::new(codes::INVALID_ARGUMENTS, e.to_string()))?;
                Ok(Prepared {
                    request: Some(request),
                    tokens: None,
                    args: None,
                    ..Prepared::default()
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
        &self,
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
        self.write(|tx| self.record_refusal(tx, flow_id, call, refusal))?;
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
        class: StoredClass,
        prepared: &mut Prepared,
        now_ms: i64,
    ) -> Result<Option<String>> {
        if let Some(reason) = crate::revocation::dispatch_check(tx, &self.lease.run_id)? {
            self.record_refusal(tx, flow_id, call, &Refusal::new("version_revoked", reason))?;
            return Ok(None);
        }
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
        if prepared.classify_cost {
            let cap = self
                .manifest
                .as_ref()
                .and_then(|m| m.model_cost_usd.as_ref())
                .ok_or_else(|| {
                    CoreError::Invalid("a classifier call without dollar caps".into())
                })?;
            let mut input: Value = serde_json::from_str(call.args.as_str())
                .map_err(|e| CoreError::Invalid(e.to_string()))?;
            if let Err(error) = crate::classify_cost::reserve(
                tx,
                "flow",
                &self.lease.run_id,
                call.position,
                Some(flow_id),
                now_ms,
                cap.per_run,
                Some((
                    crate::manifest::duration_ms(&cap.window).expect("validated window"),
                    cap.usd,
                )),
                &mut input,
            )? {
                let code = match &error {
                    basal_host::transport::WireError::Refused { code, .. }
                    | basal_host::transport::WireError::RefusedDetails { code, .. } => {
                        code.as_str()
                    }
                    _ => "budget_exhausted",
                };
                self.record_refusal(tx, flow_id, call, &Refusal::new(code, format!("{error:?}")))?;
                return Ok(None);
            }
            prepared.request = Some(
                JsonText::new(input.to_string()).map_err(|e| CoreError::Invalid(e.to_string()))?,
            );
        }
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
        let new = NewCall {
            position: call.position,
            kind: &call.kind,
            args: &call.args,
            class,
            request: prepared.request.as_ref(),
        };
        let inserted = journal::insert_call(tx, &self.lease, flow_id, &new)?;
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
        let outcome = self.write(|tx| {
            if let Some(reason) = crate::revocation::dispatch_check(tx, &self.lease.run_id)? {
                journal::accept_outcome(
                    tx,
                    &self.lease.run_id,
                    call.position,
                    None,
                    &kv::rejection("version_revoked", &reason),
                    Source::Local,
                )?;
                return Ok(Err(reason));
            }
            let (settlement, value, clock) = self.sync_value(&call.kind)?;
            // Recovery must use the same monotonic clock rule as a new call.
            let clock = match clock {
                Some(sample) => {
                    Some(sample.max(journal::last_clock(tx, &self.lease.run_id)?.unwrap_or(sample)))
                }
                None => None,
            };
            let value = clock.map_or(value.clone(), number_text);
            journal::record_sync(tx, &self.lease, call.position, settlement, &value, clock).map(Ok)
        })?;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => return self.finish_revoked_call(&call, reason),
        };
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
            let (outcome, readiness) =
                self.write(|tx| journal::release_next(tx, &self.lease, awaiting))?;
            self.readiness_seen = readiness;
            if let Some(outcome) = outcome {
                let position = outcome.position;
                if outcome.settlement == Settlement::Rejected
                    && serde_json::from_str::<Value>(outcome.value.as_str())
                        .ok()
                        .is_some_and(|v| v["code"] == "version_revoked")
                {
                    let _ = self.send(&ParentMessage::Deliver(outcome));
                    return self.end_revoked("version revoked before dispatch".into());
                }
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

    fn on_finished(&self, result: ActivationResult) -> Result<Flow> {
        if let Some(reason) = self
            .rt
            .store()
            .read(|c| crate::revocation::dispatch_check(c, &self.lease.run_id))?
        {
            return self.end_revoked(reason);
        }
        match result {
            ActivationResult::Completed { value } => {
                let exit = Exit::Succeeded {
                    value: value.as_str().to_owned(),
                };
                match self.write(|tx| runs::exit(tx, &self.lease, &exit)) {
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
                let parked =
                    self.write(|tx| runs::park(tx, &self.lease, &awaited, self.readiness_seen))?;
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
                // Retire workers that report exhausting their CPU, memory or
                // stack budget rather than reuse potentially unhealthy engine
                // state. A caught memory or stack error ending in Completed
                // does not enter this path.
                self.fail("budget_exhausted", format!("{kind:?}"), false)
            }
        }
    }
}
