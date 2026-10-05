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
pub mod builtins;
pub mod catalog;
pub mod consent;
pub mod core_consent;
pub mod core_host;
pub mod flow_refusal;
pub mod flow_scope;
pub mod mock;
pub mod routing;
pub mod selector;
pub mod subc_catalog;
pub mod transport;

pub use catalog::{Catalog, EventBody, EventDecl, EventOrigin, MockCatalog, OpDecl, OpKind};
pub use consent::{
    CardDecision, Consent, ConsentError, DecisionAnswer, DecisionCard, DecisionContext,
    DecisionEvent, DecisionKind, DecisionOption, DecisionSink, InstallCard, MockConsent,
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
    /// The journaled dispatch bytes: script arguments for module ops, or a
    /// prepared envelope for core and model calls.
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

/// The op a call goes to, as `module.op`, for an operator to read. A flow's
/// own ops name their module; primitives name the module and op they are
/// carried by: core's sinks and facts, and Broca's `session.send` for model
/// calls, under `broca`, the module id `ck-basal` is configured with for
/// Broca. Local primitives
/// (clock, random, `kv`) never leave basal and keep their own name.
pub fn op_label(kind: &CallKind) -> String {
    use basal_proto::Primitive;
    match kind {
        CallKind::Op { module, op } => format!("{module}.{op}"),
        CallKind::Primitive(p @ (Primitive::SinkDigest | Primitive::SinkStatus)) => {
            format!("{}.{}", subc_catalog::CORE, p.name())
        }
        CallKind::Primitive(Primitive::Facts) => format!("{}.agent.facts", subc_catalog::CORE),
        CallKind::Primitive(Primitive::Llm | Primitive::Classify) => "broca.session.send".into(),
        // Local primitives and the built-ins, which basal carries out itself.
        CallKind::Primitive(p) => p.name().into(),
    }
}

/// Why a call's outcome became unknown: a closed set, recorded with the
/// call at the moment basal stops being able to prove what happened, and
/// shown to the operator on the call's reconcile card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnknownReason {
    /// basal stopped after sending the call and before recording its reply.
    BasalRestarted,
    /// The route closed, or the connection failed, after the call was sent.
    ConnectionLost,
    /// No reply came within the call's deadline.
    ReplyTimeout,
    /// A reply came that could not be decoded, or named a disposition basal
    /// does not know.
    ReplyUnreadable,
    /// A call whose op honours idempotency keys stayed ambiguous through
    /// every retry basal makes inside the call.
    RetriesExhausted,
    /// Broca answered `unknown_run` for a run it had accepted.
    ProviderLostRun,
}

impl UnknownReason {
    pub const ALL: [Self; 6] = [
        Self::BasalRestarted,
        Self::ConnectionLost,
        Self::ReplyTimeout,
        Self::ReplyUnreadable,
        Self::RetriesExhausted,
        Self::ProviderLostRun,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BasalRestarted => "basal_restarted",
            Self::ConnectionLost => "connection_lost",
            Self::ReplyTimeout => "reply_timeout",
            Self::ReplyUnreadable => "reply_unreadable",
            Self::RetriesExhausted => "retries_exhausted",
            Self::ProviderLostRun => "provider_lost_run",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }
}

/// Why the runtime disabled a flow by itself: a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisabledReason {
    /// The runs admitted per rate window hit their limit in the configured
    /// number of windows in a row.
    RunLimitSaturated,
    /// The calls dispatched per rate window hit their limit in the
    /// configured number of windows in a row.
    DispatchLimitSaturated,
}

impl DisabledReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunLimitSaturated => "run_limit_saturated",
            Self::DispatchLimitSaturated => "dispatch_limit_saturated",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::RunLimitSaturated, Self::DispatchLimitSaturated]
            .into_iter()
            .find(|r| r.as_str() == text)
    }
}

/// Whether a send that failed can have reached the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// The transport proves the request never reached the host (for
    /// example the connection was refused before any byte was written).
    Never,
    /// It may have reached the host and taken effect there; the reason says
    /// why its outcome is unknown.
    Maybe(UnknownReason),
}

/// The transport could not carry the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Unavailable { sent: Sent, detail: String },
    Refused(flow_refusal::FlowRefusal),
}

impl TransportError {
    /// A failure the transport proves happened before the request left.
    pub fn unsent(detail: impl Into<String>) -> Self {
        Self::Unavailable {
            sent: Sent::Never,
            detail: detail.into(),
        }
    }

    /// A failure after which the request may have reached the host.
    pub fn maybe_sent(reason: UnknownReason, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            sent: Sent::Maybe(reason),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(
                f,
                "{}: {} {}",
                refusal.reason.as_str(),
                refusal.provider,
                refusal.action
            ),
            Self::Unavailable { sent, detail } => match sent {
                Sent::Never => write!(f, "host unavailable (provably unsent): {detail}"),
                Sent::Maybe(reason) => write!(
                    f,
                    "host unavailable (may have been sent; {}): {detail}",
                    reason.as_str()
                ),
            },
        }
    }
}

impl std::error::Error for TransportError {}

/// What core says about one flow version, from its `flow.install_status`
/// op. Core keeps every version it approved active until the operator
/// revokes it there; it has no notion of a superseded version, so the older
/// approved version a run started on still reads active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStatus {
    /// Core stands behind the version. `code_hash` is the code hash core
    /// approved, as 64 lowercase hex digits.
    Active {
        code_hash: String,
        scope: Option<flow_scope::RegisteredScope>,
    },
    /// The operator revoked the version in core, which now refuses its sink
    /// writes.
    Revoked { code_hash: String },
    /// Core holds no install of the version.
    Unknown,
}

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

/// A long-running call whose outcome the host can no longer establish,
/// although the host accepted it: for example Broca answering that it knows
/// no run under the run id it gave at acceptance. The runtime records the
/// call as unknown and the run waits for the operator in `needs_reconcile`,
/// with `detail` as the run's error so the anomaly can be reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownOutcome {
    pub run_id: String,
    pub position: u64,
    pub handle: String,
    /// Why the host can no longer establish the outcome, recorded with the
    /// call.
    pub reason: UnknownReason,
    pub detail: String,
}

/// Where a host sends completions of long-running calls.
pub trait CompletionSink: Send + Sync {
    fn complete(&self, completion: &Completion) -> Result<CompletionAck, SinkError>;
    /// Reports an accepted call whose outcome cannot be established. Like
    /// a completion, it is redelivered until the runtime records it, and a
    /// redelivery is acknowledged as [`CompletionAck::Duplicate`].
    fn unknown(&self, unknown: &UnknownOutcome) -> Result<CompletionAck, SinkError>;
}

/// The hosts a flow reaches. Implementations must be safe to call from
/// several threads at once: the runtime runs a run's calls concurrently.
pub trait Host: Send + Sync {
    /// Only flow-owned provider calls use scope readiness. Plumbing and local
    /// built-ins keep the default so they cannot accidentally acquire a scope.
    fn provider_ready(
        &self,
        _flow_id: &str,
        _kind: &CallKind,
    ) -> Result<(), flow_refusal::FlowRefusal> {
        Ok(())
    }
    fn refusal_committed(&self, _request: &CallRequest, _refusal: &flow_refusal::FlowRefusal) {}
    /// Production checks core; capture-only and non-gate tests explicitly opt out.
    fn scope_checks(&self, _enabled: bool) {}
    /// The activation gate supplies fresh core authority before any run calls.
    fn configure_flow(
        &self,
        _flow_id: &str,
        _agent_owned: bool,
        _scope: Option<flow_scope::RegisteredScope>,
    ) {
    }
    /// Whether the call only reads, and if not whether it honours
    /// idempotency keys.
    fn classify(&self, kind: &CallKind) -> CallClass;

    /// Sends one call. May block until the host answers; the runtime calls it
    /// off the activation's thread.
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError>;

    /// Dispatches under the retry policy already stored with the intent.
    /// If a catalog changes an operation from query to mutation, refuse it
    /// before sending: the runtime will repeat queries after a lost reply.
    fn dispatch_classified(
        &self,
        request: &CallRequest,
        _class: CallClass,
    ) -> Result<Dispatched, TransportError> {
        self.dispatch(request)
    }

    /// The current time in milliseconds since the Unix epoch, for a clock
    /// read. The runtime keeps the value monotonic within a run.
    /// Invoked after the accepted handle or immediate outcome is durable, so
    /// a fast model completion cannot arrive before its handle is journaled.
    fn dispatch_committed(&self, _request: &CallRequest) {}
    fn now_ms(&self) -> f64;

    /// A sample in `[0, 1)`, for `Math.random()` and `random()`.
    fn random(&self) -> f64;

    /// Tells the host where to send completions. A host that already holds
    /// completions not yet acknowledged delivers them again to the new sink,
    /// which is how a restarted runtime learns outcomes it missed.
    fn attach(&self, sink: Arc<dyn CompletionSink>);

    /// Asks core whether it still stands behind `version` of `flow_id`. The
    /// runtime asks before every activation and activates nothing on an
    /// error, so this default (no answer) keeps every run of a host that
    /// cannot reach core from activating rather than letting it through.
    fn install_status(&self, flow_id: &str, version: u32) -> Result<InstallStatus, TransportError> {
        Err(TransportError::unsent(format!(
            "this host cannot ask core about {flow_id} v{version}"
        )))
    }
}
