//! Model calls through Broca. The transport opens an authenticated module
//! route for the project, harness and session, and owns the stream lifetime.
//! The host records call identity, buffers final text and delivers outcomes.

pub mod fake;
pub mod wire;

use std::fmt;
use std::sync::{Arc, Mutex};

use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    CallClass, CallRequest, Completion, CompletionSink, Dispatched, Host, HostOutcome, TokenUsage,
    TransportError,
};
use wire::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    pub project_root: String,
    pub harness: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrocaError {
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
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BrocaError {}

/// Adapters bind `route` before each op. `subscribe` returns ordered durable
/// events available at the current head, not display deltas as result text.
/// A live adapter may wake `BrocaHost::poll` when its subscription receives data.
pub trait Transport: Send + Sync {
    fn send(&self, route: &Route, params: &[u8]) -> Result<SendResult, BrocaError>;
    fn subscribe(
        &self,
        route: &Route,
        params: &SubscribeParams,
    ) -> Result<Vec<SubscribeEvent>, BrocaError>;
    fn status(&self, route: &Route, params: &StatusParams)
    -> Result<RunStatusResponse, BrocaError>;
    fn read(&self, route: &Route, params: &ReadParams) -> Result<SessionReadResponse, BrocaError>;
}

/// A save replaces one call's entire snapshot atomically. In particular text,
/// finish metadata and cursor must commit together. Core supplies the SQLite
/// implementation so this crate never opens a second writer to its database.
pub trait StateStore: Send + Sync {
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError>;
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError>;
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
    pub route: Route,
    pub basal_run_id: String,
    pub position: u64,
    pub send_id: String,
    pub params: Vec<u8>,
    pub envelope: String,
    pub labels: Option<Vec<String>>,
    pub handle: Option<String>,
    pub broca_run_id: Option<String>,
    pub cursor: Option<Cursor>,
    /// The final assembled assistant message, not reasoning or display deltas.
    pub text: Option<String>,
    pub finish: Option<(RunFinishReason, Terminal)>,
    pub outcome: Option<StoredOutcome>,
    pub acknowledged: bool,
}

pub struct BrocaHost {
    transport: Arc<dyn Transport>,
    store: Arc<dyn StateStore>,
    project_root: String,
    harness: String,
    model: ModelParams,
    gate: Mutex<()>,
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
        model: ModelParams,
    ) -> Self {
        Self {
            transport,
            store,
            project_root,
            harness,
            model,
            gate: Mutex::new(()),
            sink: Mutex::new(None),
        }
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
        let mut model = self.model.clone();
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
            if let Some(m) = e.request.get("model") {
                model = serde_json::from_value(m.clone())
                    .map_err(|e| BrocaError::Invalid(e.to_string()))?;
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
                .unwrap_or(GenerationConfig {
                    max_output_tokens: None,
                    temperature: None,
                    top_p: None,
                    stop_sequences: vec![],
                })
        } else {
            GenerationConfig {
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                stop_sequences: vec![],
            }
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
            route: Route {
                project_root: self.project_root.clone(),
                harness: self.harness.clone(),
                session,
            },
            basal_run_id: request.run_id.clone(),
            position: request.position,
            send_id: e.send_id,
            params: serde_json::to_vec(&params).map_err(|e| BrocaError::Wire(e.to_string()))?,
            labels,
            envelope: request.args.as_str().to_owned(),
            handle: None,
            broca_run_id: None,
            cursor: None,
            text: None,
            finish: None,
            outcome: None,
            acknowledged: false,
        })
    }

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
            Err(e) => return Err(e),
        };
        let finished = matches!(result, SendResult::Finished { .. });
        if let SendResult::Finished { reason, .. } = &result {
            call.finish = Some((*reason, Terminal::default()));
        }
        let handle = match result {
            SendResult::Active { run_id } | SendResult::Finished { run_id, .. } => {
                call.broca_run_id = Some(run_id.clone());
                run_id
            }
            SendResult::Pending { submission_id } => submission_id,
        };
        if handle.is_empty() {
            return Err(BrocaError::Wire("empty Broca handle".into()));
        }
        // A pending submission remains the completion handle even after RunStarted
        // assigns its run id, since core journals the handle returned at acceptance.
        if call.handle.is_none() {
            call.handle = Some(handle);
        }
        self.store.save(call)?;
        Ok(finished)
    }

    fn read_text(&self, call: &mut StoredCall) -> Result<(), BrocaError> {
        let page = self.transport.read(
            &call.route,
            &ReadParams {
                from_ordinal: None,
                limit: Some(1),
                include_tools: false,
            },
        )?;
        if let Some(id) = &page.lineage_state.last_run_id {
            if call
                .broca_run_id
                .as_ref()
                .is_some_and(|expected| expected != id)
            {
                return Err(BrocaError::Wire("read returned another run".into()));
            }
            call.broca_run_id = Some(id.clone());
        }
        // The session is private to one call. The newest assistant message is
        // therefore its final answer, even after the session has been archived.
        if let Some(message) = page
            .messages
            .iter()
            .rev()
            .find(|m| m.message.role == "assistant")
        {
            call.text = Some(text_parts(&message.message.content));
        }
        Ok(())
    }

    fn collect(&self, call: &mut StoredCall) -> Result<(), BrocaError> {
        let missing_cursor = call.cursor.is_none();
        let params = SubscribeParams {
            from: Some(
                call.cursor
                    .map(FromWire::Cursor)
                    .unwrap_or_else(|| FromWire::Named("start".into())),
            ),
        };
        let expired = match self.transport.subscribe(&call.route, &params) {
            Ok(events) => {
                for event in events {
                    let SubscribeEvent::Control { cursor, unit } = event else {
                        continue;
                    };
                    if call.cursor.is_some_and(|previous| cursor <= previous) {
                        continue;
                    }
                    match *unit {
                        ControlUnit::RunStarted {
                            run_id,
                            submission_id,
                        } => {
                            if call.broca_run_id.as_ref().is_some_and(|id| id != &run_id) {
                                return Err(BrocaError::Wire("stream returned another run".into()));
                            }
                            if call.broca_run_id.is_none()
                                && submission_id.as_ref() != call.handle.as_ref()
                            {
                                return Err(BrocaError::Wire(
                                    "stream returned another submission".into(),
                                ));
                            }
                            call.broca_run_id = Some(run_id);
                        }
                        ControlUnit::AssistantMessage { message } => {
                            call.text = Some(text_parts(&message.content))
                        }
                        ControlUnit::RunFinished {
                            run_id,
                            reason,
                            metadata,
                        } => {
                            if let Some(id) = run_id {
                                if call
                                    .broca_run_id
                                    .as_ref()
                                    .is_some_and(|expected| expected != &id)
                                {
                                    return Err(BrocaError::Wire(
                                        "finish returned another run".into(),
                                    ));
                                }
                                call.broca_run_id = Some(id);
                            }
                            call.finish = Some((reason, metadata));
                        }
                        ControlUnit::Other => {}
                    }
                    call.cursor = Some(cursor);
                    // No cursor is acknowledged independently of the text and
                    // terminal metadata it passed. A failed save replays the event.
                    self.store.save(call)?;
                }
                false
            }
            Err(BrocaError::Refused { code, .. }) if code == "cursor_expired" => true,
            Err(e) => return Err(e),
        };
        if let Some(run_id) = &call.broca_run_id {
            let status = self.transport.status(
                &call.route,
                &StatusParams {
                    run_id: Some(run_id.clone()),
                },
            )?;
            if let Some((reason, metadata)) = status.terminal() {
                if call.finish.as_ref().is_none_or(|(_, t)| t.usage.is_none()) {
                    call.finish = Some((reason, metadata.clone()));
                }
            }
        }
        if missing_cursor || expired || (call.finish.is_some() && call.text.is_none()) {
            if let Err(error) = self.read_text(call) {
                match error {
                    BrocaError::Refused { code, detail } if call.finish.is_some() => {
                        let usage = call
                            .finish
                            .as_ref()
                            .and_then(|(_, t)| t.usage.as_ref())
                            .map(ledger_usage);
                        call.outcome = Some(rejection(&code, &detail, usage));
                    }
                    other => return Err(other),
                }
            }
        }
        // A queued call may first learn its run id from archived history.
        // Status must then be requested with that id, not the submission id.
        if call.finish.is_none() {
            if let Some(run_id) = &call.broca_run_id {
                let status = self.transport.status(
                    &call.route,
                    &StatusParams {
                        run_id: Some(run_id.clone()),
                    },
                )?;
                if let Some((reason, metadata)) = status.terminal() {
                    call.finish = Some((reason, metadata.clone()));
                }
            }
        }
        if call.outcome.is_none() {
            if let Some((reason, metadata)) = &call.finish {
                let usage = metadata.usage.as_ref().map(ledger_usage);
                call.outcome = Some(if *reason != RunFinishReason::Completed {
                    rejection(
                        reason_code(*reason),
                        "Broca run did not complete successfully",
                        usage,
                    )
                } else if let Some(text) = &call.text {
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
                        if let Some(reason) = &metadata.final_step_finish_reason {
                            value["finish_reason"] = json!(reason);
                        }
                        fulfilled(value, usage)
                    }
                } else {
                    return Err(BrocaError::Wire(
                        "completed run has no assistant message in durable history".into(),
                    ));
                });
            }
        }
        self.store.save(call)
    }

    /// Run on stream wakeups and after reconnect. Errors leave durable work
    /// pending for the next invocation, including sink failures after a finish.
    pub fn poll(&self) -> Result<(), BrocaError> {
        let _guard = lock(&self.gate);
        let sink = lock(&self.sink).clone();
        for mut call in self.store.load()? {
            if call.acknowledged {
                continue;
            }
            if call.handle.is_none() && call.outcome.is_none() {
                self.issue(&mut call)?;
            }
            if call.outcome.is_none() {
                self.collect(&mut call)?;
            }
            if let (Some(outcome), Some(handle), Some(sink)) = (&call.outcome, &call.handle, &sink)
            {
                sink.complete(&Completion {
                    run_id: call.basal_run_id.clone(),
                    position: call.position,
                    handle: handle.clone(),
                    outcome: outcome.host()?,
                })
                .map_err(|e| BrocaError::Sink(e.to_string()))?;
                call.acknowledged = true;
                self.store.save(&call)?;
            }
        }
        Ok(())
    }

    pub fn dispatch_model(&self, request: &CallRequest) -> Result<Dispatched, BrocaError> {
        let _guard = lock(&self.gate);
        let prepared = self.prepare(request)?;
        let existing = self
            .store
            .load()?
            .into_iter()
            .find(|c| c.send_id == prepared.send_id);
        let mut call = match existing {
            Some(c) => {
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
                self.store.save(&prepared)?;
                prepared
            }
        };
        if let Some(outcome) = &call.outcome {
            return Ok(Dispatched::Completed(outcome.host()?));
        }
        let finished = self.issue(&mut call)?;
        if finished && call.outcome.is_none() {
            self.collect(&mut call)?;
        }
        if let Some(outcome) = &call.outcome {
            return Ok(Dispatched::Completed(outcome.host()?));
        }
        Ok(Dispatched::Accepted {
            handle: call
                .handle
                .clone()
                .ok_or_else(|| BrocaError::Wire("missing handle".into()))?,
        })
    }
}

impl Host for BrocaHost {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Mutation {
            honours_idempotency_keys: true,
        }
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        match self.dispatch_model(request) {
            Ok(result) => Ok(result),
            Err(BrocaError::Unavailable {
                proven_unsent,
                detail,
            }) => Err(TransportError::Unavailable {
                proven_unsent,
                detail,
            }),
            Err(BrocaError::Invalid(detail)) => Ok(Dispatched::Completed(HostOutcome::rejected(
                json_text(json!({"code": "invalid_arguments", "message": detail})),
            ))),
            Err(BrocaError::UnsupportedKind) => Ok(Dispatched::Completed(HostOutcome::rejected(
                json_text(json!({"code": "unsupported_kind"})),
            ))),
            Err(error) => Err(TransportError::Unavailable {
                proven_unsent: false,
                detail: error.to_string(),
            }),
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

fn text_parts(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
fn ledger_usage(u: &Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        cache_write_tokens: u.cache_write_tokens,
        output_tokens: u.output_tokens,
        cached_input_tokens: u.cached_input_tokens,
    }
}
fn reason_code(reason: RunFinishReason) -> &'static str {
    match reason {
        RunFinishReason::Completed => "completed",
        RunFinishReason::MaxSteps => "max_steps",
        RunFinishReason::Cancelled => "cancelled",
        RunFinishReason::Interrupted => "interrupted",
        RunFinishReason::Error => "error",
        RunFinishReason::TransformUnavailable => "transform_unavailable",
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
