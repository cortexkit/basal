//! The Broca wire subset used by flow model calls.
//!
//! Mirrored from Broca commit 3fded44a5d9b4ff4487259d4c67c152af272b729:
//! broca-wire/src/lib.rs (97-195, 402-424, 464-586, 669-780) and
//! broca-protocol/src/{caller,content,usage,request}.rs.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Cursor {
    pub wal_seq: u64,
    pub sub_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FromWire {
    Named(String),
    Cursor(Cursor),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<FromWire>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(other)]
    Other,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub message_id: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlUnit {
    RunStarted {
        run_id: String,
        #[serde(default)]
        submission_id: Option<String>,
    },
    AssistantMessage {
        message: AssistantMessage,
    },
    RunFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        reason: RunFinishReason,
        #[serde(flatten)]
        metadata: Terminal,
    },
    #[serde(other)]
    Other,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubscribeEvent {
    Control {
        cursor: Cursor,
        unit: Box<ControlUnit>,
    },
    Display {
        event: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_tools: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_prefix_blocks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadMessage {
    pub ordinal: u64,
    pub mid: String,
    pub message: Message,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadLineageState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadResponse {
    pub messages: Vec<SessionReadMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_from_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_id: Option<String>,
    pub lineage_state: SessionReadLineageState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_run_id: Option<String>,
}
