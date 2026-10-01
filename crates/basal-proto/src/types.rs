//! The data carried between the parent and a worker.

use std::fmt;

use crate::limits::MAX_VALUE_BYTES;

/// JSON text crossing the process boundary.
///
/// Values travel as text and are parsed only inside the VM with `JSON.parse`
/// (or by the parent, which owns them), never evaluated as source. The text
/// is capped at [`MAX_VALUE_BYTES`] when it is constructed or decoded, which
/// is before anything parses it. Its JSON validity is not checked here: the
/// receiver that parses it reports a typed error if it is malformed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct JsonText(String);

/// A value longer than the hard cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueTooLarge {
    pub bytes: usize,
    pub cap: usize,
}

impl fmt::Display for ValueTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "value of {} bytes exceeds the cap of {}",
            self.bytes, self.cap
        )
    }
}

impl std::error::Error for ValueTooLarge {}

impl JsonText {
    /// Wraps text, refusing anything longer than [`MAX_VALUE_BYTES`].
    pub fn new(text: impl Into<String>) -> Result<Self, ValueTooLarge> {
        let text = text.into();
        if text.len() > MAX_VALUE_BYTES {
            return Err(ValueTooLarge {
                bytes: text.len(),
                cap: MAX_VALUE_BYTES,
            });
        }
        Ok(Self(text))
    }

    /// The JSON `null`, used where a value is required but absent.
    pub fn null() -> Self {
        Self("null".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for JsonText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOWN: usize = 120;
        if self.0.len() <= SHOWN {
            write!(f, "JsonText({:?})", self.0)
        } else {
            let mut end = SHOWN;
            while !self.0.is_char_boundary(end) {
                end -= 1;
            }
            write!(
                f,
                "JsonText({:?}... {} bytes)",
                &self.0[..end],
                self.0.len()
            )
        }
    }
}

/// A BLAKE3 digest of a host call's argument text.
///
/// Replay compares this digest, not the arguments themselves, so a recorded
/// prefix does not have to ship every argument back to the worker.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArgsDigest(pub [u8; 32]);

impl ArgsDigest {
    /// The digest of the exact argument text the VM produced with
    /// `JSON.stringify`. Both sides must hash the same bytes, so the parent
    /// hashes the text it received in the host call, never a re-serialised
    /// copy.
    pub fn of(args: &JsonText) -> Self {
        Self(*blake3::hash(args.as_str().as_bytes()).as_bytes())
    }
}

impl fmt::Debug for ArgsDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArgsDigest({})", hex(&self.0[..8]))
    }
}

/// A BLAKE3 digest of the worker's lockdown prelude. The parent records it
/// with each run, and the worker refuses to replay under a different one.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PreludeHash(pub [u8; 32]);

impl PreludeHash {
    pub fn of(source: &str) -> Self {
        Self(*blake3::hash(source.as_bytes()).as_bytes())
    }
}

impl fmt::Debug for PreludeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PreludeHash({})", hex(&self.0[..8]))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Which primitive set a script gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Unattended flows: every host call journaled, and no shell by any route.
    Flow,
    /// Interactive codemode: the same primitives plus `sh`.
    Codemode,
}

/// The host primitives a script can call, other than module ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    /// A clock read (`Date.now()`, `new Date()`, `Date()`, `now()`).
    Now,
    /// A random sample (`Math.random()`, `random()`).
    Random,
    Facts,
    Classify,
    Llm,
    SinkDigest,
    SinkStatus,
    KvGet,
    KvSet,
    KvDelete,
    /// Shell access, which exists only in the codemode profile.
    Sh,
}

impl Primitive {
    pub const ALL: [Primitive; 11] = [
        Self::Now,
        Self::Random,
        Self::Facts,
        Self::Classify,
        Self::Llm,
        Self::SinkDigest,
        Self::SinkStatus,
        Self::KvGet,
        Self::KvSet,
        Self::KvDelete,
        Self::Sh,
    ];

    /// The wire code. Zero is reserved for module ops in [`CallKind`].
    pub fn code(self) -> u8 {
        match self {
            Self::Now => 1,
            Self::Random => 2,
            Self::Facts => 3,
            Self::Classify => 4,
            Self::Llm => 5,
            Self::SinkDigest => 6,
            Self::SinkStatus => 7,
            Self::KvGet => 8,
            Self::KvSet => 9,
            Self::KvDelete => 10,
            Self::Sh => 11,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.code() == code)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::Random => "random",
            Self::Facts => "facts",
            Self::Classify => "classify",
            Self::Llm => "llm",
            Self::SinkDigest => "sink.digest",
            Self::SinkStatus => "sink.status",
            Self::KvGet => "kv.get",
            Self::KvSet => "kv.set",
            Self::KvDelete => "kv.delete",
            Self::Sh => "sh",
        }
    }
}

/// What a host call asks for. Module ops are structured pairs, never a
/// dotted string, so no check ever depends on parsing a name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CallKind {
    Op { module: String, op: String },
    Primitive(Primitive),
}

impl CallKind {
    /// Synchronous calls must be answered before the script continues,
    /// because their JavaScript contract returns a plain value (`Date.now()`
    /// returns a number). The parent answers them immediately with a
    /// delivery for the same position.
    pub fn is_synchronous(&self) -> bool {
        matches!(self, Self::Primitive(Primitive::Now | Primitive::Random))
    }

    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Primitive(Primitive::Sh))
    }
}

impl fmt::Display for CallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Op { module, op } => write!(f, "op ({module}, {op})"),
            Self::Primitive(p) => write!(f, "{}", p.name()),
        }
    }
}

/// How a host call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Settlement {
    Fulfilled,
    /// A final host error (denied, not found, invalid arguments). Rejections
    /// are journaled and replayed in order exactly like fulfilments.
    Rejected,
}

/// Per-activation limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// JavaScript execution time, in microseconds of the worker thread's CPU
    /// time spent inside the engine. Time spent waiting on the parent does
    /// not count. Replay counts, so size it to include replay.
    pub js_time_micros: u64,
    /// The QuickJS heap limit for the activation's runtime.
    pub memory_bytes: u64,
    /// The QuickJS stack limit.
    pub stack_bytes: u64,
    /// The largest argument or result text the activation may produce, at
    /// most [`MAX_VALUE_BYTES`].
    pub max_value_bytes: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            js_time_micros: 1_000_000,
            memory_bytes: 8 * 1024 * 1024,
            // QuickJS frames are large (about 1.5 KiB per JS call level in a
            // release build), so 1 MiB allows recursion a few hundred deep.
            stack_bytes: 1024 * 1024,
            max_value_bytes: 256 * 1024,
        }
    }
}

/// The outcome of a call, as recorded in the journal or newly delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedOutcome {
    pub settlement: Settlement,
    pub value: JsonText,
    /// The order in which the outcome was handed to the VM, across the whole
    /// run. Replay releases outcomes in this order, not in issue order, which
    /// is what keeps `Promise.race` and `Promise.any` choosing the same winner.
    pub delivery_order: u64,
}

/// One journaled call: its signature, and its outcome if one was recorded.
///
/// A call with no outcome was issued before the journal was cut (a long call
/// still running, or one whose result was lost); the parent still owes it an
/// outcome, so the worker treats it as outstanding without issuing it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub position: u64,
    pub kind: CallKind,
    pub args_digest: ArgsDigest,
    pub outcome: Option<RecordedOutcome>,
}

/// Everything an activation needs, in one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRequest {
    /// Chosen by the parent and echoed in the result, so the parent can match
    /// a result to the activation it started.
    pub activation_id: u64,
    pub profile: Profile,
    /// The prelude the parent expects. A worker with a different prelude
    /// refuses the activation rather than replay under changed semantics.
    pub prelude_hash: PreludeHash,
    /// The exact approved script source.
    pub script: String,
    /// The trigger payload, exposed to the script as the frozen `trigger`.
    pub trigger: JsonText,
    pub budgets: Budgets,
    /// The run's recorded calls, positions `0..n` in order.
    pub prefix: Vec<RecordedCall>,
}

/// A call the worker issues to the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCall {
    pub position: u64,
    pub kind: CallKind,
    pub args: JsonText,
}

/// An outcome the parent delivers to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub position: u64,
    pub settlement: Settlement,
    pub value: JsonText,
    pub delivery_order: u64,
}

/// How the worker confined itself before reading its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confinement {
    /// The deny-by-default Seatbelt profile applied through `sandbox_init`.
    Seatbelt,
    /// No OS sandbox. A production parent refuses such a worker.
    None,
}

/// The worker's reply to the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub protocol_version: u32,
    /// The engine and binding versions as one string (for example
    /// "quickjs-ng 0.16.2 via rquickjs 0.14.0"), part of a run's runtime
    /// fingerprint together with the prelude hash.
    pub engine: String,
    pub prelude_hash: PreludeHash,
    pub confinement: Confinement,
}

/// A call's identity for replay comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSignature {
    pub kind: CallKind,
    pub args_digest: ArgsDigest,
}

/// Ways a replay can disagree with the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nondeterminism {
    /// The script issued a different call at a recorded position.
    Divergence {
        position: u64,
        recorded: CallSignature,
        observed: CallSignature,
    },
    /// A recorded outcome was due for release, but the replay had not issued
    /// its call (or had already settled it).
    UnreleasedOutcome { position: u64, delivery_order: u64 },
    /// The script finished without issuing a recorded call.
    UnconsumedCall { position: u64 },
}

/// Why an activation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The script threw or its top-level promise rejected.
    Script {
        message: String,
    },
    Nondeterminism(Nondeterminism),
    /// The script tried to issue a call its profile forbids.
    ProfileViolation {
        kind: CallKind,
    },
    /// The worker's lockdown prelude differs from the one the parent recorded
    /// for the run, so replay would run under changed semantics.
    EngineMismatch {
        expected: PreludeHash,
        actual: PreludeHash,
    },
    /// The activation request was inconsistent (bad prefix, bad budgets).
    InvalidRequest {
        detail: String,
    },
    /// The script passed arguments larger than the value cap.
    ArgumentsTooLarge {
        bytes: u64,
        cap: u64,
    },
    /// The script's result is larger than the value cap.
    ResultTooLarge {
        bytes: u64,
        cap: u64,
    },
    /// The script's result cannot be expressed as JSON.
    ResultNotSerializable {
        detail: String,
    },
    /// A value from the parent could not be parsed as data.
    InvalidHostValue {
        position: u64,
        detail: String,
    },
    /// The channel to the parent broke during the activation.
    HostLink {
        detail: String,
    },
    /// The engine failed in a way no script action explains.
    Engine {
        detail: String,
    },
}

/// Which per-activation limit ended an activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    JsTime,
    Memory,
    Stack,
}

/// How an activation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationResult {
    Completed {
        value: JsonText,
    },
    Failed(Failure),
    /// The script is blocked only on calls the parent reported as
    /// long-running; the VM is dropped and the run resumes by replay.
    Suspended {
        awaited: Vec<u64>,
    },
    /// The script is waiting on a promise that no host call will settle.
    Stalled,
    BudgetExhausted(BudgetKind),
}

/// What the worker was doing when it refused a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    AwaitingHello,
    Idle,
    AwaitingSyncReply,
    Blocked,
}

/// Message kinds, used to name the frame a refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Hello,
    Activate,
    Deliver,
    LongRunning,
    Shutdown,
}

/// Why the worker refused a frame from the parent. A refusal never ends the
/// worker, except [`Refusal::Oversized`], after which the byte stream can no
/// longer be split into frames and the worker exits cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Malformed {
        detail: String,
    },
    Oversized {
        declared: u64,
        max: u64,
    },
    UnexpectedFrame {
        received: MessageKind,
        state: WorkerState,
    },
    VersionMismatch {
        supported: u32,
        requested: u32,
    },
    /// A delivery or long-running report named a position the worker is not
    /// waiting on.
    UnknownPosition {
        position: u64,
    },
    /// A delivery order did not increase.
    DeliveryOrderRegression {
        last: u64,
        received: u64,
    },
}

/// Frames the parent sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentMessage {
    Hello {
        protocol_version: u32,
    },
    Activate(Box<ActivationRequest>),
    Deliver(Outcome),
    /// In reply to [`WorkerMessage::Blocked`]: these awaited calls are
    /// long-running and will not settle soon.
    LongRunning {
        positions: Vec<u64>,
    },
    Shutdown,
}

/// Frames the worker sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerMessage {
    Welcome(Welcome),
    HostCall(HostCall),
    /// The script can make no progress until one of these calls settles. The
    /// parent answers with exactly one [`ParentMessage::Deliver`] for one of
    /// them, or a [`ParentMessage::LongRunning`] naming the ones that are.
    Blocked {
        awaiting: Vec<u64>,
    },
    Finished {
        activation_id: u64,
        result: ActivationResult,
    },
    Refused(Refusal),
}

impl ParentMessage {
    pub fn kind(&self) -> MessageKind {
        match self {
            Self::Hello { .. } => MessageKind::Hello,
            Self::Activate(_) => MessageKind::Activate,
            Self::Deliver(_) => MessageKind::Deliver,
            Self::LongRunning { .. } => MessageKind::LongRunning,
            Self::Shutdown => MessageKind::Shutdown,
        }
    }
}
