//! The Broca wire subset used by flow model calls.
//!
//! Send and status shapes are mirrored from Broca commit
//! 3fded44a5d9b4ff4487259d4c67c152af272b729: broca-wire/src/lib.rs
//! (97-195, 402-424, 669-780) and broca-protocol/src/{caller,usage,request}.rs.
//! The `run.result` shapes are mirrored from Broca commit
//! f31d371032254e79dfa12337bbc80360e31e9575 (release v0.3.166):
//! broca-wire/src/role.rs (19, 36, 128-158) and the provider error of
//! broca-protocol/src/{caller.rs:126,error.rs:82}.
//! Optional provider metadata is retained as JSON rather than interpreted here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelParams {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

/// Flow sends deliberately expose no tool or provider credential surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendParams {
    pub prompt: String,
    pub system: Option<String>,
    pub send_id: Option<String>,
    pub model: ModelParams,
    pub tools: Vec<Value>,
    pub tool_choice: Value,
    pub generation: GenerationConfig,
    pub stop_when: Vec<Value>,
    pub cache: Option<Value>,
    pub work_class: Option<String>,
    pub append_episode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SendResult {
    Active {
        run_id: String,
    },
    Pending {
        submission_id: String,
    },
    Finished {
        run_id: String,
        reason: RunFinishReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFinishReason {
    Completed,
    MaxSteps,
    Cancelled,
    Interrupted,
    Error,
    TransformUnavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Metadata not used for flow settlement remains available to adapters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub retries_used: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_step_finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indeterminate_tool_calls: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunStatusResponse {
    Active,
    Paused {
        #[serde(flatten)]
        metadata: Terminal,
    },
    Unknown,
    Completed {
        #[serde(flatten)]
        metadata: Terminal,
    },
    MaxSteps {
        #[serde(flatten)]
        metadata: Terminal,
    },
    Cancelled {
        #[serde(flatten)]
        metadata: Terminal,
    },
    Interrupted {
        #[serde(flatten)]
        metadata: Terminal,
    },
    Error {
        #[serde(flatten)]
        metadata: Terminal,
    },
    TransformUnavailable {
        #[serde(flatten)]
        metadata: Terminal,
    },
}
impl RunStatusResponse {
    pub fn terminal(&self) -> Option<(RunFinishReason, &Terminal)> {
        use RunFinishReason as R;
        match self {
            Self::Completed { metadata } => Some((R::Completed, metadata)),
            Self::MaxSteps { metadata } => Some((R::MaxSteps, metadata)),
            Self::Cancelled { metadata } => Some((R::Cancelled, metadata)),
            Self::Interrupted { metadata } => Some((R::Interrupted, metadata)),
            Self::Error { metadata } => Some((R::Error, metadata)),
            Self::TransformUnavailable { metadata } => Some((R::TransformUnavailable, metadata)),
            _ => None,
        }
    }
}

pub const OP_RUN_RESULT: &str = "run.result";
pub const OP_RUN_STATUS: &str = "run.status";
pub const OP_SESSION_SEND: &str = "session.send";
/// Reads a run event's body by its event key. Broca answers it only on a
/// route bound to the event's own session (the bind the send used); on any
/// other route the event is `absent`.
pub const OP_RUN_EVENT_GET: &str = "run.event.get";
/// The refusal code `run.result` answers when the session has no run with
/// the requested run id.
pub const UNKNOWN_RUN: &str = "unknown_run";

/// `run.result` params: the run id to report on. Unknown members are
/// refused, as Broca refuses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResultParams {
    pub run_id: String,
}

/// The final assistant message of a completed run: its text parts joined in
/// order with nothing inserted, reasoning excluded. A final message with no
/// text part answers `text: ""`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalMessage {
    pub ordinal: u64,
    pub mid: String,
    pub text: String,
}

/// A provider error on a caller-facing surface. `class` is one of the shared
/// classes (`transient`, `permanent`, `auth_required`, `context_overflow`),
/// decoded as an open string. Members basal does not interpret (status,
/// retry hints, provider code) are kept as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderError {
    pub class: String,
    pub message: String,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, Value>,
}

/// `run.result`: a run's state and, once it completed, its final message.
/// `state` is one of `active`, `paused`, `completed`, `max_steps`,
/// `cancelled`, `error`, `interrupted` or Broca's own
/// `transform_unavailable`, decoded as an open string. A run that has not
/// completed carries no `final_message`; `error` carries the cause of an
/// `error` state and `reason` the reason a run is paused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResultResponse {
    pub run_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProviderError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<FinalMessage>,
}

/// `run.event.get` params. Unknown members are refused, as Broca refuses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEventGetParams {
    pub event_key: String,
}

/// `run.event.get`, inside the management `result`. `found` carries the
/// event key and `jcs`, the exact body bytes the bus digest covers. Broca
/// answers `absent` for an unknown key, a key from another session, or a
/// body pruned after its 24 hours. `state` is decoded as an open string.
/// Shapes from Broca's captured replies (test-vectors/broca-run-events).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventReply {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jcs: Option<String>,
}

/// Reports what became of an accepted send, by its send id, on the call's
/// own route, including its run id once started, without submitting the
/// send again. The answer has the same shape as `session.send`'s, plus
/// `delivered` once the send was delivered.
pub const OP_SEND_STATUS: &str = "send.status";
/// The refusal code `send.status` answers for a send id Broca has no record
/// of for this caller. Terminal.
pub const UNKNOWN_SEND: &str = "unknown_send";

/// `send.status` params. Unknown members are refused, as Broca refuses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendStatusParams {
    pub send_id: String,
}

/// What basal takes from a `send.status` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendStatus {
    /// Still queued behind another run.
    Pending,
    /// Started as (or joined) the run `run_id`; it may be active, paused or
    /// finished, which `run.result` then reports.
    Started { run_id: String },
}

/// Decodes a `send.status` answer (inside the management `result`):
/// `{"state":"pending","submission_id"}`, or `active`, `paused` or
/// `finished` with a `run_id`. A finished send's `reason` is an open string
/// and `delivered` is tolerated; neither is needed, since `run.result`
/// reports the outcome. Shapes from Broca's pinned goldens. Anything else
/// is a decoding error: the host leaves the call pending and reads it again
/// after its own backoff.
pub fn decode_send_status(reply: &Value) -> Result<SendStatus, String> {
    let text = |field: &str| {
        reply
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    match reply.get("state").and_then(Value::as_str) {
        Some("pending") => text("submission_id")
            .map(|_| SendStatus::Pending)
            .ok_or_else(|| "send.status pending without a submission_id".into()),
        Some("active" | "paused" | "finished") => text("run_id")
            .map(|run_id| SendStatus::Started {
                run_id: run_id.to_owned(),
            })
            .ok_or_else(|| "send.status reported a started send without a run_id".into()),
        other => Err(format!("unrecognised send.status state {other:?}")),
    }
}
