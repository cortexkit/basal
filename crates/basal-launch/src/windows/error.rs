//! Launch failures, and the named refusals the parent's checks produce.

use std::fmt;

/// A named reason the launcher refused to start a worker.
///
/// Every refusal leaves nothing running: a suspended child that fails a check
/// is killed before its first instruction executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The shared AppContainer profile could not be created or opened, so
    /// there is no package identity to confine the worker with.
    AppContainerProfileUnavailable,
    /// The owned job's flags or limits, read back before resume, are not the
    /// ones the confinement requires.
    JobLimitsMismatch,
    /// The suspended child is not a member of the job the parent created.
    NotInOwnedJob,
    /// The suspended child's primary token differs from the token the parent
    /// built.
    BirthTokenMismatch,
    /// The start-up thread token could not be read back from the suspended
    /// thread as required, or the parent's handle to it did not close before
    /// resume.
    InitialTokenOpen,
}

impl Refusal {
    /// The stable reason token, as written in logs and matched by tests.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AppContainerProfileUnavailable => "appcontainer-profile-unavailable",
            Self::JobLimitsMismatch => "job-limits-mismatch",
            Self::NotInOwnedJob => "not-in-owned-job",
            Self::BirthTokenMismatch => "birth-token-mismatch",
            Self::InitialTokenOpen => "initial-token-open",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

/// Why a launch did not produce a running worker.
#[derive(Debug)]
pub enum LaunchError {
    /// A named check refused the launch.
    Refused {
        /// Which check refused.
        refusal: Refusal,
        /// What was observed, for diagnosis.
        detail: String,
    },
    /// A system call needed to build the launch failed. Nothing was started,
    /// or what was started has been killed.
    Failed(String),
}

impl LaunchError {
    pub(crate) fn refused(refusal: Refusal, detail: impl Into<String>) -> Self {
        Self::Refused {
            refusal,
            detail: detail.into(),
        }
    }

    /// The named refusal, when a check refused the launch.
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused { refusal, .. } => Some(*refusal),
            Self::Failed(_) => None,
        }
    }

    /// The reason token of the refusal, when a check refused the launch.
    pub fn reason(&self) -> Option<&'static str> {
        self.refusal().map(Refusal::reason)
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { refusal, detail } => write!(f, "{refusal}: {detail}"),
            Self::Failed(detail) => write!(f, "worker launch failed: {detail}"),
        }
    }
}

impl std::error::Error for LaunchError {}

impl From<String> for LaunchError {
    fn from(detail: String) -> Self {
        Self::Failed(detail)
    }
}
