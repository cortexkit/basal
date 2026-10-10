//! Confinement launch deviation modes and controls.

use std::fmt;
use std::str::FromStr;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Confinement deviation requested for a spawn.
///
/// Under production builds (without the `deviations` feature), only [`Deviation::Full`]
/// is compiled. Non-`Full` deviations are test and development controls gated behind
/// the `deviations` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Deviation {
    /// Full production confinement recipe.
    #[default]
    Full,

    /// Weaker LPAC-only positive control: CreateProcessW, zero capabilities,
    /// no restricted primary token, no mitigations, no job.
    #[cfg(feature = "deviations")]
    LpacOnly,
    /// Weaker plain positive control: CreateProcessW without AppContainer or mitigations.
    #[cfg(feature = "deviations")]
    Plain,

    // Worker refusal reason token deviations
    /// Retains or injects an impersonation thread token before first frame read.
    #[cfg(feature = "deviations")]
    ThreadTokenPresent,
    /// Prevents lowering primary token integrity to Untrusted.
    #[cfg(feature = "deviations")]
    IntegrityLowerFailed,
    /// Omits LPAC attributes (e.g. WIN://NOALLAPPPKG claim or wrong package SID).
    #[cfg(feature = "deviations")]
    NotLpac,
    /// Injects a non-empty capability list into the token.
    #[cfg(feature = "deviations")]
    CapabilitiesPresent,
    /// Primary token restricting SIDs are not exactly `{S-1-0-0}`.
    #[cfg(feature = "deviations")]
    RestrictingSidMismatch,
    /// Retains an access group as enabled rather than deny-only.
    #[cfg(feature = "deviations")]
    GroupNotDenyOnly,
    /// Retains privileges on the primary token instead of clearing them.
    #[cfg(feature = "deviations")]
    PrivilegesPresent,
    /// Primary token integrity is not lowered to Untrusted.
    #[cfg(feature = "deviations")]
    IntegrityNotUntrusted,
    /// Omits required mitigation policy flags or enables forbidden bits.
    #[cfg(feature = "deviations")]
    MitigationMismatch,
    /// Leaks unpermitted handles or omits handle list restriction.
    #[cfg(feature = "deviations")]
    HandleNotAllowed,

    // Parent pre-resume check deviations
    /// Intentionally mismatches job limits or flags readback.
    #[cfg(feature = "deviations")]
    JobLimitsMismatch,
    /// Omits the job list attribute so the process is not in the owned job.
    #[cfg(feature = "deviations")]
    NotInOwnedJob,
    /// Suspended primary token differs from expected birth token attributes.
    #[cfg(feature = "deviations")]
    BirthTokenMismatch,
    /// Thread token handle is not closed before resume, or token level differs.
    #[cfg(feature = "deviations")]
    InitialTokenOpen,
}

impl Deviation {
    /// Returns the canonical kebab-case string name of this deviation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            #[cfg(feature = "deviations")]
            Self::LpacOnly => "lpac-only",
            #[cfg(feature = "deviations")]
            Self::Plain => "plain",
            #[cfg(feature = "deviations")]
            Self::ThreadTokenPresent => "thread-token-present",
            #[cfg(feature = "deviations")]
            Self::IntegrityLowerFailed => "integrity-lower-failed",
            #[cfg(feature = "deviations")]
            Self::NotLpac => "not-lpac",
            #[cfg(feature = "deviations")]
            Self::CapabilitiesPresent => "capabilities-present",
            #[cfg(feature = "deviations")]
            Self::RestrictingSidMismatch => "restricting-sid-mismatch",
            #[cfg(feature = "deviations")]
            Self::GroupNotDenyOnly => "group-not-deny-only",
            #[cfg(feature = "deviations")]
            Self::PrivilegesPresent => "privileges-present",
            #[cfg(feature = "deviations")]
            Self::IntegrityNotUntrusted => "integrity-not-untrusted",
            #[cfg(feature = "deviations")]
            Self::MitigationMismatch => "mitigation-mismatch",
            #[cfg(feature = "deviations")]
            Self::HandleNotAllowed => "handle-not-allowed",
            #[cfg(feature = "deviations")]
            Self::JobLimitsMismatch => "job-limits-mismatch",
            #[cfg(feature = "deviations")]
            Self::NotInOwnedJob => "not-in-owned-job",
            #[cfg(feature = "deviations")]
            Self::BirthTokenMismatch => "birth-token-mismatch",
            #[cfg(feature = "deviations")]
            Self::InitialTokenOpen => "initial-token-open",
        }
    }

    /// If this deviation targets a worker startup check, returns the expected
    /// exit 70 stderr reason token.
    pub const fn worker_reason(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "deviations")]
            Self::ThreadTokenPresent => Some("thread-token-present"),
            #[cfg(feature = "deviations")]
            Self::IntegrityLowerFailed => Some("integrity-lower-failed"),
            #[cfg(feature = "deviations")]
            Self::NotLpac => Some("not-lpac"),
            #[cfg(feature = "deviations")]
            Self::CapabilitiesPresent => Some("capabilities-present"),
            #[cfg(feature = "deviations")]
            Self::RestrictingSidMismatch => Some("restricting-sid-mismatch"),
            #[cfg(feature = "deviations")]
            Self::GroupNotDenyOnly => Some("group-not-deny-only"),
            #[cfg(feature = "deviations")]
            Self::PrivilegesPresent => Some("privileges-present"),
            #[cfg(feature = "deviations")]
            Self::IntegrityNotUntrusted => Some("integrity-not-untrusted"),
            #[cfg(feature = "deviations")]
            Self::MitigationMismatch => Some("mitigation-mismatch"),
            #[cfg(feature = "deviations")]
            Self::HandleNotAllowed => Some("handle-not-allowed"),
            _ => None,
        }
    }

    /// If this deviation targets a parent pre-resume check, returns the expected
    /// parent pre-resume refusal reason.
    pub const fn parent_reason(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "deviations")]
            Self::JobLimitsMismatch => Some("job-limits-mismatch"),
            #[cfg(feature = "deviations")]
            Self::NotInOwnedJob => Some("not-in-owned-job"),
            #[cfg(feature = "deviations")]
            Self::BirthTokenMismatch => Some("birth-token-mismatch"),
            #[cfg(feature = "deviations")]
            Self::InitialTokenOpen => Some("initial-token-open"),
            _ => None,
        }
    }

    /// Returns true if this is one of the positive control modes (Full, LpacOnly, Plain).
    pub const fn is_control(&self) -> bool {
        match self {
            Self::Full => true,
            #[cfg(feature = "deviations")]
            Self::LpacOnly | Self::Plain => true,
            #[cfg(feature = "deviations")]
            _ => false,
        }
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
            #[cfg(feature = "deviations")]
            "lpac-only" => Ok(Self::LpacOnly),
            #[cfg(feature = "deviations")]
            "plain" => Ok(Self::Plain),
            #[cfg(feature = "deviations")]
            "thread-token-present" => Ok(Self::ThreadTokenPresent),
            #[cfg(feature = "deviations")]
            "integrity-lower-failed" => Ok(Self::IntegrityLowerFailed),
            #[cfg(feature = "deviations")]
            "not-lpac" => Ok(Self::NotLpac),
            #[cfg(feature = "deviations")]
            "capabilities-present" => Ok(Self::CapabilitiesPresent),
            #[cfg(feature = "deviations")]
            "restricting-sid-mismatch" => Ok(Self::RestrictingSidMismatch),
            #[cfg(feature = "deviations")]
            "group-not-deny-only" => Ok(Self::GroupNotDenyOnly),
            #[cfg(feature = "deviations")]
            "privileges-present" => Ok(Self::PrivilegesPresent),
            #[cfg(feature = "deviations")]
            "integrity-not-untrusted" => Ok(Self::IntegrityNotUntrusted),
            #[cfg(feature = "deviations")]
            "mitigation-mismatch" => Ok(Self::MitigationMismatch),
            #[cfg(feature = "deviations")]
            "handle-not-allowed" => Ok(Self::HandleNotAllowed),
            #[cfg(feature = "deviations")]
            "job-limits-mismatch" => Ok(Self::JobLimitsMismatch),
            #[cfg(feature = "deviations")]
            "not-in-owned-job" => Ok(Self::NotInOwnedJob),
            #[cfg(feature = "deviations")]
            "birth-token-mismatch" => Ok(Self::BirthTokenMismatch),
            #[cfg(feature = "deviations")]
            "initial-token-open" => Ok(Self::InitialTokenOpen),
            other => Err(format!("unknown deviation: {other}")),
        }
    }
}
