//! The systems a flow's host calls reach, seen from basal's runtime.
//!
//! basal's parent process journals every host call before it leaves and
//! records every outcome when it comes back. What happens in between belongs
//! to a [`Host`]: module ops, facts, llm and classify, sinks. This crate
//! defines that boundary only as wide as the runtime needs today, and a
//! deterministic [`mock::MockHost`] that records every effect so tests can
//! prove a crash never repeats one. [`catalog`] holds what modules declare
//! (events, ops and their markers, agents), which manifests are validated
//! against. [`consent`] is where install cards are raised and their
//! decisions come back from.
//!
//! `llm` and `classify` reach Broca. Their request is an envelope basal
//! builds and journals before the first send: `send_id` (the call's
//! idempotency key), `work_class`, `session`, the clamped `max_output` and
//! the script's own `request`. A fulfilled outcome carries the run's token
//! usage as metadata, in Broca's canonical fields (see [`USAGE_FIELDS`]),
//! separate from the value delivered to the script.
//!
//! A dispatched call ends in one of three ways:
//! - it completes now, fulfilled or rejected ([`Dispatched::Completed`]);
//! - the host accepts it as long-running and completes it later through the
//!   [`CompletionSink`] the runtime attached ([`Dispatched::Accepted`]);
//! - the transport fails ([`TransportError::Unavailable`]), saying whether
//!   the request provably never left. That distinction decides whether the
//!   runtime may retry a mutation: a request that left may have taken effect
//!   even though its reply was lost.

pub mod broca;
pub mod catalog;
pub mod consent;
pub mod mock;

pub use catalog::{Catalog, EventBody, EventDecl, EventOrigin, MockCatalog, OpDecl, OpKind};
pub use consent::{
    CardDecision, Consent, ConsentError, DecisionEvent, DecisionSink, InstallCard, MockConsent,
};

/// The usage fields of a Broca outcome, in Broca's canonical names: fresh
/// input, cache write, output (reasoning is already inside it) and cached
/// input.
pub const USAGE_FIELDS: [&str; 4] = [
    "input_tokens",
    "cache_write_tokens",
    "output_tokens",
    "cached_input_tokens",
];

use std::fmt;
use std::sync::Arc;

use basal_proto::{CallKind, JsonText, Settlement};

/// A call's final outcome as the host reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostOutcome {
    pub settlement: Settlement,
    pub value: JsonText,
    /// Call metadata, never part of the script-visible value.
    pub usage: Option<TokenUsage>,
}

impl HostOutcome {
    pub fn fulfilled(value: JsonText) -> Self {
        Self {
            settlement: Settlement::Fulfilled,
            value,
            usage: None,
        }
    }

    pub fn rejected(value: JsonText) -> Self {
        Self {
            settlement: Settlement::Rejected,
            value,
            usage: None,
        }
    }
}

/// Canonical token measurements. Missing fields are unreported, not zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning is already included in output and must not be added again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
}

/// What repeating a call could do, which decides what the runtime may do
/// with a call whose outcome it never learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallClass {
    /// Reads only. Sending it again costs time and nothing else.
    Query,
    /// Changes something outside basal.
    Mutation {
        /// The op deduplicates on the idempotency key basal sends, so a
        /// second send with the same key returns the first result instead of
        /// acting twice. In production this comes from an operator-approved
        /// list, because a false claim turns a crash re-issue into a
        /// duplicate effect.
        honours_idempotency_keys: bool,
    },
}

impl CallClass {
    /// Whether sending the call again can never produce a second effect.
    pub fn safe_to_repeat(self) -> bool {
        match self {
            Self::Query => true,
            Self::Mutation {
                honours_idempotency_keys,
            } => honours_idempotency_keys,
        }
    }
}

/// One call on its way to a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRequest {
    pub flow_id: String,
    pub run_id: String,
    pub position: u64,
    pub kind: CallKind,
    /// The exact argument text the script produced.
    pub args: JsonText,
    /// Derived from (flow id, run id, position), so every send of this call,
    /// across crashes and re-issues, carries the same key.
    pub idempotency_key: String,
    /// 1 for the first send, counting every later send of the same call.
    pub attempt: u32,
}

/// How a host took a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatched {
    /// The call finished while dispatch waited.
    Completed(HostOutcome),
    /// The call will take a while. Its outcome arrives later through the
    /// attached [`CompletionSink`], quoting `handle`.
    Accepted { handle: String },
}

/// The transport could not carry the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Unavailable {
        /// True only when the transport can prove the request never reached
        /// the host (for example the connection was refused before any byte
        /// was written). Anything else may have taken effect remotely.
        proven_unsent: bool,
        detail: String,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                proven_unsent,
                detail,
            } => write!(
                f,
                "host unavailable ({}): {detail}",
                if *proven_unsent {
                    "provably unsent"
                } else {
                    "may have been sent"
                }
            ),
        }
    }
}

impl std::error::Error for TransportError {}

/// A long-running call's outcome, delivered after dispatch returned.
///
/// A completion names the call it settles by run, position and the handle
/// the host gave when it accepted the call. The runtime accepts it whichever
/// activation is current, because it is a fact about a call already sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub run_id: String,
    pub position: u64,
    pub handle: String,
    pub outcome: HostOutcome,
}

/// What the runtime did with a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionAck {
    /// Recorded durably; the call is settled.
    Accepted,
    /// The same outcome was already recorded; nothing changed.
    Duplicate,
    /// It contradicts the recorded outcome or the call's identity, so it was
    /// set aside and never applied.
    Quarantined,
    /// The run was cancelled; the completion was logged and not applied.
    Refused,
}

/// The runtime could not record a completion (its store failed). The host
/// must keep the completion and deliver it again later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError(pub String);

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SinkError {}

/// Where a host sends completions of long-running calls.
pub trait CompletionSink: Send + Sync {
    fn complete(&self, completion: &Completion) -> Result<CompletionAck, SinkError>;
}

/// The hosts a flow reaches. Implementations must be safe to call from
/// several threads at once: the runtime runs a run's calls concurrently.
pub trait Host: Send + Sync {
    /// Whether the call only reads, and if not whether it honours
    /// idempotency keys.
    fn classify(&self, kind: &CallKind) -> CallClass;

    /// Sends one call. May block until the host answers; the runtime calls it
    /// off the activation's thread.
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError>;

    /// The current time in milliseconds since the Unix epoch, for a clock
    /// read. The runtime keeps the value monotonic within a run.
    fn now_ms(&self) -> f64;

    /// A sample in `[0, 1)`, for `Math.random()` and `random()`.
    fn random(&self) -> f64;

    /// Tells the host where to send completions. A host that already holds
    /// completions not yet acknowledged delivers them again to the new sink,
    /// which is how a restarted runtime learns outcomes it missed.
    fn attach(&self, sink: Arc<dyn CompletionSink>);
}
