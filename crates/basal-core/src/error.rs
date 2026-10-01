//! Errors from the runtime core.

use std::fmt;

use crate::model::RunState;

/// Everything that can stop a core operation.
///
/// Storage failures are never retried silently: every caller that is about
/// to dispatch a call or acknowledge something stops when the store fails,
/// so nothing leaves basal that the journal did not record first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// The database refused or failed an operation.
    Store(String),
    /// The store was cut on purpose (a simulated crash); every later
    /// operation through it fails.
    Cut,
    /// The run is no longer owned by this activation: another activation
    /// took it over, or it was cancelled. Nothing more may be written for
    /// the old activation.
    OwnerLost {
        run_id: String,
        generation: u64,
    },
    NoSuchRun(String),
    /// The run is not in a state that allows the operation.
    WrongState {
        run_id: String,
        state: RunState,
        operation: &'static str,
    },
    /// A stored value failed to decode. The store holds something this
    /// binary did not write, so it refuses to act on it.
    Corrupt(String),
    /// The worker broke the protocol or its channel failed.
    Worker(String),
    /// A request to the core was malformed.
    Invalid(String),
    /// The run cannot start yet: an earlier run of its flow has not reached
    /// a terminal state and holds the flow's concurrency slot.
    SlotBusy {
        run_id: String,
        holder: String,
    },
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Cut => write!(f, "the store was cut"),
            Self::OwnerLost { run_id, generation } => write!(
                f,
                "activation generation {generation} no longer owns run {run_id}"
            ),
            Self::NoSuchRun(r) => write!(f, "no run {r}"),
            Self::WrongState {
                run_id,
                state,
                operation,
            } => write!(f, "run {run_id} is {}; cannot {operation}", state.as_str()),
            Self::Corrupt(e) => write!(f, "corrupt store value: {e}"),
            Self::Worker(e) => write!(f, "worker: {e}"),
            Self::Invalid(e) => write!(f, "invalid request: {e}"),
            Self::SlotBusy { run_id, holder } => {
                write!(
                    f,
                    "run {run_id} waits for run {holder}, which holds its flow's slot"
                )
            }
        }
    }
}

impl std::error::Error for CoreError {}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
