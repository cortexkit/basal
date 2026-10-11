//! The worker's own confinement checks on Windows.
//!
//! On Windows the parent fixes most of the confinement when it creates the
//! worker (see the `basal-launch` crate): a Less Privileged AppContainer
//! token with no capabilities, every group deny-only, no privileges and the
//! NULL SID as the only restricting SID; a job; mitigation policies; and an
//! explicit list of the three inherited pipes. The loader then runs on the
//! main thread under a start-up impersonation token, because the restricted
//! primary allows too little for it to map system DLLs.
//!
//! Before reading its first frame the worker finishes the job, in this order,
//! and refuses to run (exit 70, the reason on stderr) at the first failure:
//!
//! 1. it drops the start-up token and checks the thread has none left
//!    (`thread-token-present`);
//! 2. it lowers its primary token from Low to Untrusted integrity and closes
//!    the handle it used (`integrity-lower-failed`);
//! 3. it closes the connection to the session's client/server runtime port
//!    and any device handle the loader left that it did not inherit;
//! 4. it reads its actual primary token back and checks every property
//!    (`not-lpac`, `capabilities-present`, `restricting-sid-mismatch`,
//!    `group-not-deny-only`, `privileges-present`, `integrity-not-untrusted`);
//! 5. it reads its mitigation policies back (`mitigation-mismatch`);
//! 6. it checks its whole handle table against an allowlist
//!    (`handle-not-allowed`).
//!
//! The checks themselves ([`attest`], [`mitigation`], [`allowlist`]) are pure
//! functions over what was read, so their accept and reject cases are tested
//! on every system. The reads and the startup sequence are Windows-only.

pub mod allowlist;
pub mod attest;
pub mod mitigation;

#[cfg(windows)]
mod native;
#[cfg(windows)]
mod startup;

#[cfg(windows)]
pub use native::PackageSid;
#[cfg(windows)]
pub use startup::enter;

/// The reason tokens the worker exits with. Each names the check that
/// failed; tests and the parent match on them.
pub mod reason {
    /// No `--package-sid=<SID>` argument was given.
    pub const PACKAGE_SID_ARGUMENT_MISSING: &str = "package-sid-argument-missing";
    /// The main thread still has an impersonation token after reverting.
    pub const THREAD_TOKEN_PRESENT: &str = "thread-token-present";
    /// The primary token could not be lowered to Untrusted integrity, or the
    /// handle used for it could not be closed.
    pub const INTEGRITY_LOWER_FAILED: &str = "integrity-lower-failed";
    /// The primary is not a Less Privileged AppContainer token of the
    /// expected package.
    pub const NOT_LPAC: &str = "not-lpac";
    /// The primary holds a capability.
    pub const CAPABILITIES_PRESENT: &str = "capabilities-present";
    /// The primary's restricting SIDs are not exactly the NULL SID.
    pub const RESTRICTING_SID_MISMATCH: &str = "restricting-sid-mismatch";
    /// An access group of the primary is not deny-only.
    pub const GROUP_NOT_DENY_ONLY: &str = "group-not-deny-only";
    /// The primary holds a privilege.
    pub const PRIVILEGES_PRESENT: &str = "privileges-present";
    /// The primary's integrity is not Untrusted.
    pub const INTEGRITY_NOT_UNTRUSTED: &str = "integrity-not-untrusted";
    /// A required mitigation policy is off, or a forbidden opt-out is on.
    pub const MITIGATION_MISMATCH: &str = "mitigation-mismatch";
    /// The handle table holds a handle outside the allowlist, or the table
    /// could not be read or cleaned.
    pub const HANDLE_NOT_ALLOWED: &str = "handle-not-allowed";
}

/// A failed check: its reason token and what was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub detail: String,
}

impl Refusal {
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.detail)
    }
}

/// The package SID argument the launcher passes on every Windows spawn.
pub const PACKAGE_SID_ARGUMENT: &str = "--package-sid=";

/// The argument basal-launch passes for a worker check it cannot break from
/// the parent. Only a worker built with the `deviations` feature accepts it.
pub const DEVIATION_ARGUMENT: &str = "--confinement-deviation=";

/// A startup check the parent asks a test worker to break.
#[cfg(feature = "deviations")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deviation {
    /// Keep the start-up thread token: step 1 must refuse.
    ThreadTokenPresent,
    /// Open the primary without the right to adjust it, so lowering fails:
    /// step 2 must refuse.
    IntegrityLowerFailed,
    /// Skip lowering the primary: step 4 must refuse it as still Low.
    IntegrityNotUntrusted,
}

#[cfg(feature = "deviations")]
impl Deviation {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "thread-token-present" => Some(Self::ThreadTokenPresent),
            "integrity-lower-failed" => Some(Self::IntegrityLowerFailed),
            "integrity-not-untrusted" => Some(Self::IntegrityNotUntrusted),
            _ => None,
        }
    }
}

/// The engine-mode command line on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arguments {
    /// The package SID, as text. It is parsed with the system's SID parser
    /// before confinement starts.
    pub package_sid: String,
    /// The startup check to break, in a test worker.
    #[cfg(feature = "deviations")]
    pub deviation: Option<Deviation>,
}

impl Arguments {
    /// Whether step 1 leaves the start-up token in place.
    #[cfg(windows)]
    fn keeps_thread_token(&self) -> bool {
        #[cfg(feature = "deviations")]
        if self.deviation == Some(Deviation::ThreadTokenPresent) {
            return true;
        }
        false
    }

    /// Whether step 2 opens the primary without the right to adjust it.
    #[cfg(windows)]
    fn opens_primary_read_only(&self) -> bool {
        #[cfg(feature = "deviations")]
        if self.deviation == Some(Deviation::IntegrityLowerFailed) {
            return true;
        }
        false
    }

    /// Whether step 2 leaves the primary at Low, the integrity the process
    /// was created with.
    #[cfg(windows)]
    fn skips_lowering(&self) -> bool {
        #[cfg(feature = "deviations")]
        if self.deviation == Some(Deviation::IntegrityNotUntrusted) {
            return true;
        }
        false
    }
}

/// Why the engine-mode command line was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentError {
    /// No `--package-sid=` argument: exit 70 `package-sid-argument-missing`.
    Missing,
    /// An unknown, repeated or malformed argument: exit 64.
    Usage(String),
}

/// Parses the engine-mode arguments: exactly one `--package-sid=<SID>`, and
/// in a test worker at most one `--confinement-deviation=<name>`. Anything
/// else is a usage error, checked before a missing SID is reported.
pub fn engine_arguments(args: &[String]) -> Result<Arguments, ArgumentError> {
    let mut package_sid: Option<&str> = None;
    #[cfg(feature = "deviations")]
    let mut deviation: Option<Deviation> = None;
    for arg in args {
        if let Some(sid) = arg.strip_prefix(PACKAGE_SID_ARGUMENT) {
            if package_sid.replace(sid).is_some() {
                return Err(ArgumentError::Usage(format!("repeated argument {arg}")));
            }
            continue;
        }
        #[cfg(feature = "deviations")]
        if let Some(name) = arg.strip_prefix(DEVIATION_ARGUMENT) {
            let parsed = Deviation::parse(name)
                .ok_or_else(|| ArgumentError::Usage(format!("unknown deviation {name}")))?;
            if deviation.replace(parsed).is_some() {
                return Err(ArgumentError::Usage(format!("repeated argument {arg}")));
            }
            continue;
        }
        return Err(ArgumentError::Usage(format!("unknown argument {arg}")));
    }
    let package_sid = package_sid.ok_or(ArgumentError::Missing)?;
    if package_sid.is_empty() {
        return Err(ArgumentError::Usage("empty --package-sid".into()));
    }
    Ok(Arguments {
        package_sid: package_sid.to_owned(),
        #[cfg(feature = "deviations")]
        deviation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    const SID: &str = "--package-sid=S-1-15-2-1-2-3-4-5-6-7";

    #[test]
    fn exactly_one_package_sid_is_accepted() {
        let parsed = engine_arguments(&args(&[SID])).expect("one package SID");
        assert_eq!(parsed.package_sid, "S-1-15-2-1-2-3-4-5-6-7");
    }

    #[test]
    fn a_missing_package_sid_is_its_own_error() {
        assert_eq!(engine_arguments(&[]), Err(ArgumentError::Missing));
    }

    #[test]
    fn other_command_lines_are_usage_errors() {
        for line in [
            &[SID, SID][..],
            &["--package-sid="],
            &[SID, "--landlock=required"],
            &["--bogus"],
            &["--bogus", SID],
            &["--package-sid"],
        ] {
            assert!(
                matches!(engine_arguments(&args(line)), Err(ArgumentError::Usage(_))),
                "{line:?}"
            );
        }
    }

    /// A worker built without the `deviations` feature has no way to skip a
    /// startup check: the request is an unknown argument like any other.
    #[cfg(not(feature = "deviations"))]
    #[test]
    fn a_production_worker_refuses_every_deviation_request() {
        for name in [
            "thread-token-present",
            "integrity-lower-failed",
            "integrity-not-untrusted",
        ] {
            let request = format!("{DEVIATION_ARGUMENT}{name}");
            assert_eq!(
                engine_arguments(&args(&[SID, &request])),
                Err(ArgumentError::Usage(format!("unknown argument {request}")))
            );
        }
    }

    #[cfg(feature = "deviations")]
    #[test]
    fn a_test_worker_accepts_the_three_deviations_once() {
        for (name, deviation) in [
            ("thread-token-present", Deviation::ThreadTokenPresent),
            ("integrity-lower-failed", Deviation::IntegrityLowerFailed),
            ("integrity-not-untrusted", Deviation::IntegrityNotUntrusted),
        ] {
            let request = format!("{DEVIATION_ARGUMENT}{name}");
            let parsed = engine_arguments(&args(&[SID, &request])).expect("accepted");
            assert_eq!(parsed.deviation, Some(deviation));
            assert!(engine_arguments(&args(&[SID, &request, &request])).is_err());
        }
        let other = format!("{DEVIATION_ARGUMENT}not-lpac");
        assert!(engine_arguments(&args(&[SID, &other])).is_err());
    }
}
