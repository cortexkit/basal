//! Confinement launch deviation modes and controls.

use std::fmt;
use std::str::FromStr;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Confinement deviation requested for a spawn.
///
/// Covers full confinement, LPAC-only control, plain control, and one deviation
/// per worker refusal reason token and per parent pre-resume check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Deviation {
    /// Full production confinement recipe.
    Full,
    /// Weaker LPAC-only positive control: CreateProcessW, zero capabilities,
    /// no restricted primary token, no mitigations, no job.
    LpacOnly,
    /// Weaker plain positive control: CreateProcessW without AppContainer or mitigations.
    Plain,

    // Worker refusal reason token deviations
    /// Retains or injects an impersonation thread token before first frame read.
    ThreadTokenPresent,
    /// Prevents lowering primary token integrity to Untrusted.
    IntegrityLowerFailed,
    /// Omits LPAC attributes (e.g. WIN://NOALLAPPPKG claim or wrong package SID).
    NotLpac,
    /// Injects a non-empty capability list into the token.
    CapabilitiesPresent,
    /// Primary token restricting SIDs are not exactly `{S-1-0-0}`.
    RestrictingSidMismatch,
    /// Retains an access group as enabled rather than deny-only.
    GroupNotDenyOnly,
    /// Retains privileges on the primary token instead of clearing them.
    PrivilegesPresent,
    /// Primary token integrity is not lowered to Untrusted.
    IntegrityNotUntrusted,
    /// Omits required mitigation policy flags or enables forbidden bits.
    MitigationMismatch,
    /// Leaks unpermitted handles or omits handle list restriction.
    HandleNotAllowed,

    // Parent pre-resume check deviations
    /// Intentionally mismatches job limits or flags readback.
    JobLimitsMismatch,
    /// Omits the job list attribute so the process is not in the owned job.
    NotInOwnedJob,
    /// Suspended primary token differs from expected birth token attributes.
    BirthTokenMismatch,
    /// Thread token handle is not closed before resume, or token level differs.
    InitialTokenOpen,
}

impl Deviation {
    /// Returns the canonical kebab-case string name of this deviation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::LpacOnly => "lpac-only",
            Self::Plain => "plain",
            Self::ThreadTokenPresent => "thread-token-present",
            Self::IntegrityLowerFailed => "integrity-lower-failed",
            Self::NotLpac => "not-lpac",
            Self::CapabilitiesPresent => "capabilities-present",
            Self::RestrictingSidMismatch => "restricting-sid-mismatch",
            Self::GroupNotDenyOnly => "group-not-deny-only",
            Self::PrivilegesPresent => "privileges-present",
            Self::IntegrityNotUntrusted => "integrity-not-untrusted",
            Self::MitigationMismatch => "mitigation-mismatch",
            Self::HandleNotAllowed => "handle-not-allowed",
            Self::JobLimitsMismatch => "job-limits-mismatch",
            Self::NotInOwnedJob => "not-in-owned-job",
            Self::BirthTokenMismatch => "birth-token-mismatch",
            Self::InitialTokenOpen => "initial-token-open",
        }
    }

    /// If this deviation targets a worker startup check, returns the expected
    /// exit 70 stderr reason token.
    pub const fn worker_reason(&self) -> Option<&'static str> {
        match self {
            Self::ThreadTokenPresent => Some("thread-token-present"),
            Self::IntegrityLowerFailed => Some("integrity-lower-failed"),
            Self::NotLpac => Some("not-lpac"),
            Self::CapabilitiesPresent => Some("capabilities-present"),
            Self::RestrictingSidMismatch => Some("restricting-sid-mismatch"),
            Self::GroupNotDenyOnly => Some("group-not-deny-only"),
            Self::PrivilegesPresent => Some("privileges-present"),
            Self::IntegrityNotUntrusted => Some("integrity-not-untrusted"),
            Self::MitigationMismatch => Some("mitigation-mismatch"),
            Self::HandleNotAllowed => Some("handle-not-allowed"),
            _ => None,
        }
    }

    /// If this deviation targets a parent pre-resume check, returns the expected
    /// parent pre-resume refusal reason.
    pub const fn parent_reason(&self) -> Option<&'static str> {
        match self {
            Self::JobLimitsMismatch => Some("job-limits-mismatch"),
            Self::NotInOwnedJob => Some("not-in-owned-job"),
            Self::BirthTokenMismatch => Some("birth-token-mismatch"),
            Self::InitialTokenOpen => Some("initial-token-open"),
            _ => None,
        }
    }

    /// Returns true if this is one of the positive control modes (Full, LpacOnly, Plain).
    pub const fn is_control(&self) -> bool {
        matches!(self, Self::Full | Self::LpacOnly | Self::Plain)
    }

    /// Returns true if this deviation targets a parent pre-resume check.
    pub const fn is_parent_check(&self) -> bool {
        self.parent_reason().is_some()
    }

    /// Returns true if this deviation targets a worker startup check.
    pub const fn is_worker_check(&self) -> bool {
        self.worker_reason().is_some()
    }
}

impl fmt::Display for Deviation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for Deviation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Self::Full),
            "lpac-only" => Ok(Self::LpacOnly),
            "plain" => Ok(Self::Plain),
            "thread-token-present" => Ok(Self::ThreadTokenPresent),
            "integrity-lower-failed" => Ok(Self::IntegrityLowerFailed),
            "not-lpac" => Ok(Self::NotLpac),
            "capabilities-present" => Ok(Self::CapabilitiesPresent),
            "restricting-sid-mismatch" => Ok(Self::RestrictingSidMismatch),
            "group-not-deny-only" => Ok(Self::GroupNotDenyOnly),
            "privileges-present" => Ok(Self::PrivilegesPresent),
            "integrity-not-untrusted" => Ok(Self::IntegrityNotUntrusted),
            "mitigation-mismatch" => Ok(Self::MitigationMismatch),
            "handle-not-allowed" => Ok(Self::HandleNotAllowed),
            "job-limits-mismatch" => Ok(Self::JobLimitsMismatch),
            "not-in-owned-job" => Ok(Self::NotInOwnedJob),
            "birth-token-mismatch" => Ok(Self::BirthTokenMismatch),
            "initial-token-open" => Ok(Self::InitialTokenOpen),
            other => Err(format!("unknown deviation: {other}")),
        }
    }
}
