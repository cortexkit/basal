//! Error types for Windows process launching and pre-resume verification.

use std::fmt;

/// Errors produced during profile creation, token construction, process launching,
/// or pre-resume attestation.
#[derive(Debug)]
pub enum LaunchError {
    /// AppContainer profile could not be created or opened.
    AppContainerProfileUnavailable(String),
    /// Pre-resume job limits readback did not match the configured confinement or commit limit.
    JobLimitsMismatch(String),
    /// Pre-resume check found that the suspended child process is not in the owned job.
    NotInOwnedJob(String),
    /// Suspended primary token differed from the expected birth token configuration.
    BirthTokenMismatch(String),
    /// Suspended initial thread token could not be verified or closed before resume.
    InitialTokenOpen(String),
    /// A Win32 API call failed unexpectedly.
    Win32 {
        api: &'static str,
        code: u32,
        detail: String,
    },
    /// An I/O error occurred during pipe creation or process manipulation.
    Io(std::io::Error),
    /// Generic launch error with human-readable description.
    Other(String),
}

impl LaunchError {
    /// Returns the named refusal reason token if this error corresponds to a standard refusal.
    pub fn reason_token(&self) -> Option<&'static str> {
        match self {
            Self::AppContainerProfileUnavailable(_) => Some("appcontainer-profile-unavailable"),
            Self::JobLimitsMismatch(_) => Some("job-limits-mismatch"),
            Self::NotInOwnedJob(_) => Some("not-in-owned-job"),
            Self::BirthTokenMismatch(_) => Some("birth-token-mismatch"),
            Self::InitialTokenOpen(_) => Some("initial-token-open"),
            _ => None,
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppContainerProfileUnavailable(d) => {
                if d.is_empty() {
                    write!(f, "appcontainer-profile-unavailable")
                } else {
                    write!(f, "appcontainer-profile-unavailable: {d}")
                }
            }
            Self::JobLimitsMismatch(d) => {
                if d.is_empty() {
                    write!(f, "job-limits-mismatch")
                } else {
                    write!(f, "job-limits-mismatch: {d}")
                }
            }
            Self::NotInOwnedJob(d) => {
                if d.is_empty() {
                    write!(f, "not-in-owned-job")
                } else {
                    write!(f, "not-in-owned-job: {d}")
                }
            }
            Self::BirthTokenMismatch(d) => {
                if d.is_empty() {
                    write!(f, "birth-token-mismatch")
                } else {
                    write!(f, "birth-token-mismatch: {d}")
                }
            }
            Self::InitialTokenOpen(d) => {
                if d.is_empty() {
                    write!(f, "initial-token-open")
                } else {
                    write!(f, "initial-token-open: {d}")
                }
            }
            Self::Win32 { api, code, detail } => {
                write!(
                    f,
                    "{api} failed with Win32 error {code} (0x{code:08x}): {detail}"
                )
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for LaunchError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
