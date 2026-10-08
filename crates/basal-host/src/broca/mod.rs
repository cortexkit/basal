//! Model calls through Broca. The transport opens an authenticated module
//! route for the project, harness and session, and owns the stream lifetime.
//! The host records call identity, reads each accepted call's outcome with
//! `run.result` and delivers it.
//!
//! The session stream is only a wake signal. Whenever it reports a finished
//! run, and on every reconnect or module start, `BrocaHost::poll` asks
//! `run.result` about each accepted call that has no outcome yet, so a missed
//! stream event costs latency, never correctness. `run.result` reads through
//! Broca's archive, so it answers for archived sessions too.

pub mod fake;
pub mod subc;
pub mod wire;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use basal_proto::{ArgsDigest, CallKind, JsonText, Primitive, Settlement};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::selector::{ModelOutcome, ModelSelection, ModelSelector};
use crate::{
    CallClass, CallRequest, Completion, CompletionSink, Dispatched, Host, HostOutcome, TokenUsage,
    TransportError, UnknownOutcome, UnknownReason,
};
use wire::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    pub project_root: String,
    pub harness: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrocaError {
    Flow(crate::flow_refusal::FlowRefusal),
    Invalid(String),
    UnsupportedKind,
    Wire(String),
    Refused { code: String, detail: String },
    Unavailable { proven_unsent: bool, detail: String },
    Store(String),
    Sink(String),
}

impl fmt::Display for BrocaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flow(refusal) => write!(f, "Broca flow refusal: {}", refusal.reason.as_str()),
            Self::Invalid(detail) => write!(f, "invalid Broca arguments: {detail}"),
            Self::UnsupportedKind => f.write_str("unsupported Broca call kind"),
            Self::Wire(detail) => write!(f, "Broca reply: {detail}"),
            Self::Refused { code, detail } => write!(f, "Broca refused {code}: {detail}"),
            Self::Unavailable { detail, .. } => write!(f, "Broca unavailable: {detail}"),
            Self::Store(detail) => write!(f, "Broca store: {detail}"),
            Self::Sink(detail) => write!(f, "Broca completion: {detail}"),
        }
    }
}
impl std::error::Error for BrocaError {}

/// Adapters bind `route` before each op.
pub trait Transport: Send + Sync {
    fn provider_ready(
        &self,
        _flow_id: &str,
        _action: &str,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        Ok(())
    }
    fn refresh_flow(&self, _identity: &FlowIdentity) -> Result<(), BrocaError> {
        Ok(())
    }
    fn configure_flow(
        &self,
        _flow_id: &str,
        _agent_owned: bool,
        _scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
    }
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError>;
    /// Makes sure a subscription attached at the session's live head is
    /// open, and that it wakes `BrocaHost::poll` when the session's run
    /// finishes. Called before each `run.result` read, so a run that
    /// finishes after the read still wakes the host.
    fn watch(&self, route: &Route) -> Result<(), BrocaError>;
    fn result(
        &self,
        route: &Route,
        params: &RunResultParams,
    ) -> Result<RunResultResponse, BrocaError>;
    /// Only for token usage and the final step's finish reason, which
    /// `run.result` does not carry.
    fn status(&self, route: &Route, params: &StatusParams)
    -> Result<RunStatusResponse, BrocaError>;
    /// Closes the session's subscription once its call is settled.
    fn release(&self, _route: &Route) {}
}

/// A save replaces one call's entire snapshot atomically. Core supplies the
/// SQLite implementation so this crate never opens a second writer to its
/// database.
pub trait StateStore: Send + Sync {
    fn identity(&self, _run_id: &str) -> Result<Option<FlowIdentity>, BrocaError> {
        Ok(None)
    }
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError>;
    /// Look up one id without decoding unrelated snapshots. The default keeps
    /// older adapters source compatible; durable stores should use their key.
    fn get(&self, send_id: &str) -> Result<Option<StoredCall>, BrocaError> {
        Ok(self
            .load()?
            .into_iter()
            .find(|call| call.send_id == send_id))
    }
    fn pending_ids(&self) -> Result<Vec<String>, BrocaError> {
        Ok(self
            .load()?
            .into_iter()
            .filter(|call| !call.acknowledged && !call.deferred)
            .map(|call| call.send_id)
            .collect())
    }
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError>;
}

pub struct FlowIdentity {
    pub flow_id: String,
    pub version: u32,
    pub code_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredOutcome {
    pub rejected: bool,
    pub value: String,
    pub usage: Option<TokenUsage>,
}
impl StoredOutcome {
    fn host(&self) -> Result<HostOutcome, BrocaError> {
        Ok(HostOutcome {
            settlement: if self.rejected {
                Settlement::Rejected
            } else {
                Settlement::Fulfilled
            },
            value: JsonText::new(self.value.clone())
                .map_err(|e| BrocaError::Wire(e.to_string()))?,
            usage: self.usage,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCall {
    /// A refused send is retried only by the runtime's journal, not by polling.
    #[serde(default)]
    pub deferred: bool,
    pub route: Route,
    pub basal_run_id: String,
    pub position: u64,
    pub send_id: String,
    #[serde(with = "params_bytes")]
    pub params: Vec<u8>,
    #[serde(deserialize_with = "envelope_fingerprint")]
    pub envelope: String,
    pub labels: Option<Vec<String>>,
    pub handle: Option<String>,
    pub broca_run_id: Option<String>,
    /// The terminal `run.result` state the outcome was read from. It decides
    /// the outcome reported to the model selector for the routing decision.
    #[serde(default)]
    pub state: Option<String>,
    pub outcome: Option<StoredOutcome>,
    /// Set instead of `outcome` when Broca refuses `run.result` with
    /// `unknown_run` for the run it accepted the call as: the detail the
    /// run is moved to `needs_reconcile` with.
    #[serde(default)]
    pub unknown: Option<String>,
    /// The runtime has recorded `outcome` (or, when there is none, the
    /// `unknown` report) through the completion sink, so neither is
    /// delivered again.
    pub acknowledged: bool,
    pub selection: ModelSelection,
    #[serde(default)]
    pub report_attempted: bool,
}

pub struct BrocaHost {
    scope_checks: std::sync::atomic::AtomicBool,
    transport: Arc<dyn Transport>,
    store: Arc<dyn StateStore>,
    project_root: String,
    harness: String,
    selector: Arc<dyn ModelSelector>,
    // Only identical send ids serialize. An unavailable provider must not hold
    // every other model dispatch behind a process-wide network lock.
    gate: Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    /// Per send id, how many consecutive polls found the run terminal in
    /// `run.result` while `run.status` still said active or paused (the
    /// brief window described in `resolve`). Kept in memory only: a restart
    /// starts the count again, which keeps it bounded.
    status_lag: Mutex<HashMap<String, u32>>,
    sink: Mutex<Option<Arc<dyn CompletionSink>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BrocaHost {
    pub fn new(
        transport: Arc<dyn Transport>,
        store: Arc<dyn StateStore>,
        project_root: String,
        harness: String,
        selector: Arc<dyn ModelSelector>,
    ) -> Self {
        Self {
            scope_checks: std::sync::atomic::AtomicBool::new(true),
            transport,
            store,
            project_root,
            harness,
            selector,
            gate: Mutex::new(HashMap::new()),
            status_lag: Mutex::new(HashMap::new()),
            sink: Mutex::new(None),
        }
    }

    fn call_gate(&self, send_id: &str) -> Arc<Mutex<()>> {
        let mut gates = lock(&self.gate);
        if let Some(gate) = gates.get(send_id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = Arc::new(Mutex::new(()));
        gates.insert(send_id.to_owned(), Arc::downgrade(&gate));
        gate
    }

    fn prepare(&self, request: &CallRequest) -> Result<StoredCall, BrocaError> {
        let primitive = match request.kind {
            CallKind::Primitive(p @ (Primitive::Llm | Primitive::Classify)) => p,
            _ => return Err(BrocaError::UnsupportedKind),
        };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope {
            send_id: String,
            work_class: String,
            session: String,
            op: String,
            max_output: u32,
            request: Value,
            selection: ModelSelection,
        }
        let e: Envelope = serde_json::from_str(request.args.as_str())
            .map_err(|e| BrocaError::Invalid(e.to_string()))?;
        let session = format!(
            "basal:flow-{}:{}:{}",
            request.flow_id, request.run_id, request.position
        );
        if e.send_id != request.idempotency_key
            || e.work_class != format!("flow:{}", request.flow_id)
            || e.session != session
            || e.op != primitive.name()
            || e.max_output == 0
        {
            return Err(BrocaError::Invalid(
                "journaled model envelope does not match call identity".into(),
            ));
        }
        if e.request.get("model").is_some() || e.request.get("provider").is_some() {
            return Err(BrocaError::Refused {
                code: "model_not_allowed".into(),
                detail: "scripts cannot select a provider or model".into(),
            });
        }
        if e.selection.provider_id.is_empty()
            || e.selection.model_id.is_empty()
            || e.selection.decision_id.is_empty()
            || e.selection.runner.provider.is_empty()
            || e.selection.runner.model.is_empty()
        {
            return Err(BrocaError::Invalid(
                "journaled model selection is incomplete".into(),
            ));
        }
        let model = e.selection.runner.clone();
        let (prompt, system, labels) = if primitive == Primitive::Classify {
            let text = e
                .request
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| BrocaError::Invalid("classify needs text".into()))?;
            let labels: Vec<String> =
                serde_json::from_value(e.request.get("labels").cloned().unwrap_or(Value::Null))
                    .map_err(|e| BrocaError::Invalid(e.to_string()))?;
            if labels.is_empty() {
                return Err(BrocaError::Invalid("classify needs labels".into()));
            }
            // JSON framing keeps untrusted text separate from the fixed instruction.
            let prompt = json!({"text": text, "labels": labels}).to_string();
            (prompt, Some("Classify the text in the JSON object. Return exactly one label from labels, verbatim, with no added quotes, whitespace, explanation or other text.".into()), Some(labels))
        } else {
            if e.request.get("tools").is_some_and(|v| !v.is_null()) {
                return Err(BrocaError::Invalid("model calls get no tools".into()));
            }
            let prompt = e
                .request
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| BrocaError::Invalid("llm needs prompt".into()))?
                .to_owned();
            let system = e
                .request
                .get("system")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| BrocaError::Invalid("system must be text".into()))
                })
                .transpose()?;
            (prompt, system, None)
        };
        let mut generation = if primitive == Primitive::Llm {
            e.request
                .get("generation")
                .cloned()
                .map(serde_json::from_value::<GenerationConfig>)
                .transpose()
                .map_err(|error| BrocaError::Invalid(error.to_string()))?
                .unwrap_or_default()
        } else {
            GenerationConfig::default()
        };
        generation.max_output_tokens = Some(e.max_output);
        let params = SendParams {
            prompt,
            system,
            send_id: Some(e.send_id.clone()),
            model,
            tools: vec![],
            tool_choice: json!({"type": "none"}),
            generation,
            stop_when: vec![],
            cache: None,
            work_class: Some(e.work_class),
            append_episode: false,
        };
        Ok(StoredCall {
            deferred: false,
            route: Route {
                flow_id: Some(request.flow_id.clone()),
                project_root: self.project_root.clone(),
                harness: self.harness.clone(),
                session,
            },
            basal_run_id: request.run_id.clone(),
            position: request.position,
            send_id: e.send_id,
            params: serde_json::to_vec(&params).map_err(|e| BrocaError::Wire(e.to_string()))?,
            labels,
            envelope: fingerprint(&request.args),
            handle: None,
            broca_run_id: None,
            state: None,
            outcome: None,
            unknown: None,
            acknowledged: false,
            selection: e.selection,
            report_attempted: false,
        })
    }

    /// Sends the frozen request. A first send records the handle; a repeat
    /// with the same bytes is idempotent under the send id, and is how a
    /// queued submission learns the run id it started as. Returns whether
    /// Broca answered that the run already finished.
    fn issue(&self, call: &mut StoredCall) -> Result<bool, BrocaError> {
        let result = match self.transport.send(&call.route, &call.params) {
            Ok(result) => result,
            Err(BrocaError::Refused { code, detail }) => {
                if code == "send_id_reuse" {
                    tracing::error!(send_id = %call.send_id, %detail, "Broca refused a changed payload under an existing send id");
                }
                if call.handle.is_none() {
                    call.handle = Some(call.send_id.clone());
                }
                call.outcome = Some(rejection(&code, &detail, None));
                self.store.save(call)?;
                return Ok(true);
            }
            Err(BrocaError::Flow(refusal)) => {
                if call.handle.is_some() {
                    return Err(BrocaError::Unavailable {
                        proven_unsent: false,
                        detail: format!("accepted model call can no longer be read: {refusal:?}"),
                    });
                }
                call.deferred = true;
                self.store.save(call)?;
                return Err(BrocaError::Flow(refusal));
            }
            Err(e) => return Err(e),
        };
        let finished = matches!(result, SendResult::Finished { .. });
        let handle = match result {
            SendResult::Active { run_id } | SendResult::Finished { run_id, .. } => {
                if call.broca_run_id.as_ref().is_some_and(|id| id != &run_id) {
                    return Err(BrocaError::Wire("send returned another run".into()));
                }
                call.broca_run_id = Some(run_id.clone());
                run_id
            }
            SendResult::Pending { submission_id } => submission_id,
        };
        if handle.is_empty() {
            return Err(BrocaError::Wire("empty Broca handle".into()));
        }
        // A pending submission remains the completion handle after its run
        // starts, since core journals the handle returned at acceptance.
        if call.handle.is_none() {
            call.handle = Some(handle);
        }
        self.store.save(call)?;
        Ok(finished)
    }

    /// Reads the call's run with `run.result` and records its outcome once
    /// the run has ended. A run that has not ended leaves the call pending.
    fn resolve(&self, call: &mut StoredCall) -> Result<(), BrocaError> {
        let Some(run_id) = call.broca_run_id.clone() else {
            return Ok(());
        };
        // Watch before reading, so a run that ends after the read still
        // wakes the host. A session that can no longer be watched (archived,
        // say) matters only if its run has not ended yet.
        let watched = self.transport.watch(&call.route);
        let result = match self.transport.result(
            &call.route,
            &RunResultParams {
                run_id: run_id.clone(),
            },
        ) {
            Ok(result) => result,
            Err(BrocaError::Refused { code, detail }) if code == UNKNOWN_RUN => {
                // Broca accepted this call as this run, so it cannot have
                // forgotten it: an anomaly for its operator, not lost text.
                // The outcome is unknowable, so the run waits for basal's
                // operator instead of being given an invented rejection.
                tracing::error!(send_id = %call.send_id, broca_run_id = %run_id, %detail, "Broca does not know a run it accepted");
                call.unknown = Some(format!(
                    "Broca answered unknown_run for run {run_id}, which it accepted for send id {}: {detail}",
                    call.send_id
                ));
                self.store.save(call)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if result.run_id != run_id {
            return Err(BrocaError::Wire("run.result answered another run".into()));
        }
        let state = result.state.as_str();
        if !TERMINAL_STATES.contains(&state) {
            if !NONTERMINAL_STATES.contains(&state) {
                tracing::warn!(broca_run_id = %run_id, state, "unrecognised Broca run state; waiting");
            }
            return watched;
        }
        // Usage and the provider's finish reason come only from run.status.
        // If run.status has not yet caught up with an ended run, wait for it
        // so the reported usage is recorded, but only for a bounded number
        // of polls. The two reads disagree in one direction only, and only
        // briefly. Per Broca's serve.rs `run_result`, the run task makes its
        // terminal or pause durable in the WAL first, and only then does the
        // session's live actor mark the run ended; run.result starts from
        // run.status and upgrades it from the durable WAL, while run.status
        // deliberately does not. So run.result can be terminal while
        // run.status still says active or paused. That window is normally
        // milliseconds but unbounded in principle, because the actor handles
        // one command at a time. (A session with no live actor cannot
        // disagree, from Broca v0.3.167.) The outcome waits for run.status
        // rather than settle the reservation without the usage the run did
        // report, but for at most STATUS_LAG_POLLS polls, the safety net
        // that keeps a window that never closes from holding the call, and
        // its run, forever.
        let status = self.transport.status(
            &call.route,
            &StatusParams {
                run_id: Some(run_id.clone()),
            },
        )?;
        let metadata = match &status {
            RunStatusResponse::Active | RunStatusResponse::Paused { .. } => {
                let lagged = {
                    let mut lag = lock(&self.status_lag);
                    let polls = lag.entry(call.send_id.clone()).or_insert(0);
                    *polls += 1;
                    *polls
                };
                if lagged < STATUS_LAG_POLLS {
                    return Err(BrocaError::Unavailable {
                        proven_unsent: false,
                        detail: format!(
                            "run.status still reports run {run_id} as active or paused although run.result reports it ended"
                        ),
                    });
                }
                // Usage that is never reported is charged at the reservation.
                tracing::warn!(broca_run_id = %run_id, send_id = %call.send_id, polls = lagged, "run.status still reports a Broca run as active or paused although run.result reports it ended; recording its outcome with usage unreported, so the whole token reservation is charged");
                None
            }
            // A run.status that cannot place the run leaves usage unreported,
            // which charges the whole reservation.
            RunStatusResponse::Unknown => None,
            other => other.terminal().map(|(_, metadata)| metadata),
        };
        let usage = metadata.and_then(|m| m.usage.as_ref()).map(ledger_usage);
        call.outcome = Some(match state {
            "completed" => {
                let Some(message) = &result.final_message else {
                    return Err(BrocaError::Wire(
                        "run.result reported a completed run without its final message".into(),
                    ));
                };
                let text = &message.text;
                if let Some(labels) = &call.labels {
                    if labels.contains(text) {
                        fulfilled(json!(text), usage)
                    } else {
                        rejection(
                            "classify_invalid",
                            "model did not return an exact label",
                            usage,
                        )
                    }
                } else {
                    let mut value = json!({"text": text});
                    if let Some(reason) = metadata.and_then(|m| m.final_step_finish_reason.as_ref())
                    {
                        value["finish_reason"] = json!(reason);
                    }
                    fulfilled(value, usage)
                }
            }
            "error" => match &result.error {
                Some(error) => StoredOutcome {
                    rejected: true,
                    value: json!({"code": "error", "class": error.class, "message": error.message})
                        .to_string(),
                    usage,
                },
                None => rejection("error", "Broca run ended in error", usage),
            },
            other => rejection(other, "Broca run did not complete", usage),
        });
        lock(&self.status_lag).remove(&call.send_id);
        call.state = Some(result.state);
        self.store.save(call)
    }

    fn report_terminal(&self, call: &mut StoredCall) -> Result<(), BrocaError> {
        if !call.report_attempted && (call.outcome.is_some() || call.unknown.is_some()) {
            let outcome = match call.state.as_deref() {
                Some("completed") => ModelOutcome::Completed,
                Some("cancelled") => ModelOutcome::Cancelled,
                Some("interrupted") => ModelOutcome::Interrupted,
                _ => ModelOutcome::Error,
            };
            if let Err(error) = self
                .selector
                .report_outcome(&call.selection.decision_id, outcome)
            {
                tracing::warn!(%error,"model routing outcome report failed");
            }
            call.report_attempted = true;
            self.store.save(call)?;
        }
        Ok(())
    }

    /// Run on stream wakeups, after reconnect and at module start: asks
    /// `run.result` about every accepted call without an outcome and
    /// delivers what is known. Errors leave durable work pending for the
    /// next invocation, including sink failures after an outcome is saved.
    pub fn poll(&self) -> Result<(), BrocaError> {
        let sink = lock(&self.sink).clone();
        let mut first_error = None;
        for send_id in self.store.pending_ids()? {
            let gate = self.call_gate(&send_id);
            // An activation already owns this id; it will poll after commit.
            let Ok(_guard) = gate.try_lock() else {
                continue;
            };
            let mut call = match self.store.get(&send_id) {
                Ok(Some(call)) => call,
                Ok(None) => continue,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if call.acknowledged || call.deferred {
                continue;
            }
            let identity = if self.scope_checks.load(std::sync::atomic::Ordering::SeqCst) {
                match self.store.identity(&call.basal_run_id) {
                    Ok(identity) => identity,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                }
            } else {
                None
            };
            if let Some(identity) = identity {
                call.route.flow_id = Some(identity.flow_id.clone());
                if let Err(error) = self.transport.refresh_flow(&identity) {
                    first_error.get_or_insert(error);
                    continue;
                }
            }
            // One call's failure must not hold back the others.
            if let Err(error) = self.advance(&mut call, sink.as_deref()) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn advance(
        &self,
        call: &mut StoredCall,
        sink: Option<&dyn CompletionSink>,
    ) -> Result<(), BrocaError> {
        if call.outcome.is_none() && call.unknown.is_none() {
            if call.handle.is_none() || call.broca_run_id.is_none() {
                self.issue(call)?;
            }
            if call.outcome.is_none() {
                self.resolve(call)?;
            }
        }
        self.report_terminal(call)?;
        let (Some(handle), Some(sink)) = (&call.handle, sink) else {
            return Ok(());
        };
        if let Some(outcome) = &call.outcome {
            sink.complete(&Completion {
                run_id: call.basal_run_id.clone(),
                position: call.position,
                handle: handle.clone(),
                outcome: outcome.host()?,
            })
            .map_err(|e| BrocaError::Sink(e.to_string()))?;
        } else if let Some(detail) = &call.unknown {
            sink.unknown(&UnknownOutcome {
                run_id: call.basal_run_id.clone(),
                position: call.position,
                handle: handle.clone(),
                // `call.unknown` is set only when Broca answers
                // `unknown_run` for a run it accepted.
                reason: UnknownReason::ProviderLostRun,
                detail: detail.clone(),
            })
            .map_err(|e| BrocaError::Sink(e.to_string()))?;
        } else {
            return Ok(());
        }
        call.acknowledged = true;
        self.store.save(call)?;
        self.transport.release(&call.route);
        Ok(())
    }

    pub fn dispatch_model(&self, request: &CallRequest) -> Result<Dispatched, BrocaError> {
        let gate = self.call_gate(&request.idempotency_key);
        let _guard = lock(&gate);
        let prepared = self.prepare(request)?;
        let existing = self.store.get(&prepared.send_id)?;
        let mut call = match existing {
            Some(mut c) => {
                if c.route.flow_id.is_none() {
                    c.route.flow_id = prepared.route.flow_id.clone();
                }
                if c.route != prepared.route
                    || c.envelope != prepared.envelope
                    || c.basal_run_id != request.run_id
                    || c.position != request.position
                {
                    return Err(BrocaError::Invalid(
                        "reissue changed the journaled model payload".into(),
                    ));
                }
                c
            }
            None => {
                // No send can happen before the initial snapshot is durable.
                self.store
                    .save(&prepared)
                    .map_err(|error| BrocaError::Unavailable {
                        proven_unsent: true,
                        detail: error.to_string(),
                    })?;
                prepared
            }
        };
        if let Some(outcome) = &call.outcome {
            self.transport.release(&call.route);
            return Ok(Dispatched::Completed(outcome.host()?));
        }
        if call.unknown.is_none() {
            call.deferred = false;
            let finished = self.issue(&mut call)?;
            if finished && call.outcome.is_none() {
                self.resolve(&mut call).map_err(|error| match error {
                    BrocaError::Flow(refusal) => BrocaError::Unavailable {
                        proven_unsent: false,
                        detail: format!("accepted model call can no longer be read: {refusal:?}"),
                    },
                    other => other,
                })?;
            }
        }
        if let Some(outcome) = &call.outcome {
            self.transport.release(&call.route);
            return Ok(Dispatched::Completed(outcome.host()?));
        }
        // An unknown run is reported through the sink once the acceptance
        // is journaled, like any later outcome.
        Ok(Dispatched::Accepted {
            handle: call
                .handle
                .clone()
                .ok_or_else(|| BrocaError::Wire("missing handle".into()))?,
        })
    }
}

impl Host for BrocaHost {
    fn provider_ready(
        &self,
        flow_id: &str,
        kind: &CallKind,
    ) -> Result<(), crate::flow_refusal::FlowRefusal> {
        self.transport
            .provider_ready(flow_id, &crate::op_label(kind))
    }
    fn refusal_committed(&self, request: &CallRequest, refusal: &crate::flow_refusal::FlowRefusal) {
        let result = (|| {
            let gate = self.call_gate(&request.idempotency_key);
            let _guard = lock(&gate);
            if let Some(mut call) = self.store.get(&request.idempotency_key)? {
                let outcome = refusal.outcome();
                call.outcome = Some(StoredOutcome {
                    rejected: true,
                    value: outcome.value.into_string(),
                    usage: outcome.usage,
                });
                call.acknowledged = true;
                // Admission refused before any model send. Reporting a model
                // error would penalize a runner that was never invoked.
                call.report_attempted = true;
                self.store.save(&call)?;
            }
            Ok::<_, BrocaError>(())
        })();
        if let Err(error) = result {
            tracing::error!(%error,"recording an unsent model refusal");
        }
    }
    fn scope_checks(&self, enabled: bool) {
        self.scope_checks
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }
    fn configure_flow(
        &self,
        flow_id: &str,
        agent_owned: bool,
        scope: Option<crate::flow_scope::RegisteredScope>,
    ) {
        self.transport.configure_flow(flow_id, agent_owned, scope);
    }
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Mutation {
            honours_idempotency_keys: true,
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        // Every Broca call honours idempotency keys (`classify` above), so
        // the runtime retries an ambiguous one and, when its retries run
        // out, records it as `retries_exhausted` whatever the last attempt's
        // cause was. So each ambiguous attempt reports `retries_exhausted`
        // as its reason.
        let ambiguous =
            |detail| TransportError::maybe_sent(UnknownReason::RetriesExhausted, detail);
        match self.dispatch_model(request) {
            Err(BrocaError::Flow(refusal)) => Err(TransportError::Refused(refusal)),
            Ok(result) => Ok(result),
            Err(BrocaError::Unavailable {
                proven_unsent: true,
                detail,
            }) => Err(TransportError::unsent(detail)),
            Err(BrocaError::Unavailable {
                proven_unsent: false,
                detail,
            }) => Err(ambiguous(detail)),
            Err(BrocaError::Invalid(detail)) => Ok(Dispatched::Completed(HostOutcome::rejected(
                json_text(json!({"code": "invalid_arguments", "message": detail})),
            ))),
            Err(BrocaError::Refused { code, detail }) => Ok(Dispatched::Completed(
                HostOutcome::rejected(json_text(json!({"code":code,"message":detail}))),
            )),
            Err(BrocaError::UnsupportedKind) => Ok(Dispatched::Completed(HostOutcome::rejected(
                json_text(json!({"code": "unsupported_kind"})),
            ))),
            Err(error) => Err(ambiguous(error.to_string())),
        }
    }
    fn dispatch_committed(&self, _: &CallRequest) {
        if let Err(error) = self.poll() {
            tracing::error!(%error,"Broca completion remains pending after dispatch commit");
        }
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.0
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        *lock(&self.sink) = Some(sink);
        if let Err(error) = self.poll() {
            tracing::error!(%error, "Broca recovery remains pending");
        }
    }
}

/// `run.result` states after which a run never changes again, so the call's
/// outcome can be recorded: `completed` carries its final message, `error`
/// its cause, and the others only their state.
const TERMINAL_STATES: [&str; 6] = [
    "completed",
    "error",
    "cancelled",
    "interrupted",
    "max_steps",
    "transform_unavailable",
];
/// How many consecutive polls an ended run's outcome waits for `run.status`
/// to stop saying active or paused before it is recorded with usage
/// unreported (which charges the whole token reservation). Broca's window
/// is brief but unbounded in principle (explained in `resolve`), so this is
/// the safety net. Each waiting poll fails, and the module retries a failed
/// poll after 5 s, so 12 polls take about a minute.
pub const STATUS_LAG_POLLS: u32 = 12;

/// States of a run that has not ended. A paused run can still resume.
const NONTERMINAL_STATES: [&str; 2] = ["active", "paused"];

fn ledger_usage(u: &Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        cache_write_tokens: u.cache_write_tokens,
        output_tokens: u.output_tokens,
        cached_input_tokens: u.cached_input_tokens,
    }
}
fn fulfilled(value: Value, usage: Option<TokenUsage>) -> StoredOutcome {
    StoredOutcome {
        rejected: false,
        value: value.to_string(),
        usage,
    }
}
fn rejection(code: &str, detail: &str, usage: Option<TokenUsage>) -> StoredOutcome {
    StoredOutcome {
        rejected: true,
        value: json!({"code": code, "message": detail}).to_string(),
        usage,
    }
}
fn json_text(value: Value) -> JsonText {
    // This helper only handles small, host-owned errors. Oversized wire values
    // are checked by StoredOutcome::host and never use this fallback.
    JsonText::new(value.to_string()).unwrap_or_else(|_| JsonText::null())
}

mod params_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        // Frozen sends are UTF-8 JSON. A text scalar stores those exact bytes
        // without expanding every byte to a decimal array element.
        let text = std::str::from_utf8(bytes).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(text)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Bytes {
            Text(String),
            Legacy(Vec<u8>),
        }
        Ok(match Bytes::deserialize(deserializer)? {
            Bytes::Text(text) => text.into_bytes(),
            Bytes::Legacy(bytes) => bytes,
        })
    }
}

fn fingerprint(envelope: &JsonText) -> String {
    // The journal retains the original request. A domain-labelled digest in
    // the snapshot detects changed reissues without retaining the prompt twice.
    format!(
        "blake3:{}",
        serde_json::to_string(&ArgsDigest::of(envelope).0).expect("fixed digest")
    )
}
fn envelope_fingerprint<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let envelope = String::deserialize(deserializer)?;
    if envelope.starts_with("blake3:") {
        return Ok(envelope);
    }
    let envelope = JsonText::new(envelope).map_err(serde::de::Error::custom)?;
    Ok(fingerprint(&envelope))
}
