//! Windows implementation of process launching, token construction, and confinement controls.

pub mod deviation;
pub mod error;
pub mod job;
pub mod launch;
pub mod native;
pub mod process;
pub mod profile;
pub mod token;

pub use deviation::Deviation;
pub use error::LaunchError;
pub use job::{JOB_LIMIT_FLAGS, check_job_before_resume, create_confined_job};
pub use launch::{LaunchOptions, MITIGATION_POLICY_FLAGS, launch};
pub use process::{KILL_CODE, OwnedProcess, WORKER_KILL_CODE, is_confinement_fault_code};
pub use profile::{
    AppContainerProfile, AppContainerSid, PROFILE_NAME, create_or_open_profile,
    grant_test_binary_directory,
};
pub use token::{SpawnTokens, check_birth_token, check_initial_token_and_close, construct_tokens};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "deviations")]
    fn deviation_tokens_match_specification() {
        assert_eq!(Deviation::Full.as_str(), "full");
        assert_eq!(Deviation::LpacOnly.as_str(), "lpac-only");
        assert_eq!(Deviation::Plain.as_str(), "plain");

        assert_eq!(
            Deviation::ThreadTokenPresent.worker_reason(),
            Some("thread-token-present")
        );
        assert_eq!(
            Deviation::IntegrityLowerFailed.worker_reason(),
            Some("integrity-lower-failed")
        );
        assert_eq!(Deviation::NotLpac.worker_reason(), Some("not-lpac"));
        assert_eq!(
            Deviation::CapabilitiesPresent.worker_reason(),
            Some("capabilities-present")
        );
        assert_eq!(
            Deviation::RestrictingSidMismatch.worker_reason(),
            Some("restricting-sid-mismatch")
        );
        assert_eq!(
            Deviation::GroupNotDenyOnly.worker_reason(),
            Some("group-not-deny-only")
        );
        assert_eq!(
            Deviation::PrivilegesPresent.worker_reason(),
            Some("privileges-present")
        );
        assert_eq!(
            Deviation::IntegrityNotUntrusted.worker_reason(),
            Some("integrity-not-untrusted")
        );
        assert_eq!(
            Deviation::MitigationMismatch.worker_reason(),
            Some("mitigation-mismatch")
        );
        assert_eq!(
            Deviation::HandleNotAllowed.worker_reason(),
            Some("handle-not-allowed")
        );

        assert_eq!(
            Deviation::JobLimitsMismatch.parent_reason(),
            Some("job-limits-mismatch")
        );
        assert_eq!(
            Deviation::NotInOwnedJob.parent_reason(),
            Some("not-in-owned-job")
        );
        assert_eq!(
            Deviation::BirthTokenMismatch.parent_reason(),
            Some("birth-token-mismatch")
        );
        assert_eq!(
            Deviation::InitialTokenOpen.parent_reason(),
            Some("initial-token-open")
        );
    }

    #[test]
    #[cfg(not(feature = "deviations"))]
    fn deviations_absent_in_production() {
        let d = Deviation::Full;
        match d {
            Deviation::Full => (),
        }
        assert_eq!(d.as_str(), "full");
        assert_eq!(d.worker_reason(), None);
        assert_eq!(d.parent_reason(), None);
    }

    #[test]
    fn fault_accounting_predicate_matches_specification() {
        assert!(is_confinement_fault_code(0xC0000008));
        assert!(is_confinement_fault_code(0xC0000005));
        assert!(!is_confinement_fault_code(70));
        assert!(!is_confinement_fault_code(0));
        assert!(!is_confinement_fault_code(WORKER_KILL_CODE));
    }

    #[test]
    fn launch_error_formatting_matches_refusal_tokens() {
        assert_eq!(
            LaunchError::AppContainerProfileUnavailable(String::new()).to_string(),
            "appcontainer-profile-unavailable"
        );
        assert_eq!(
            LaunchError::JobLimitsMismatch(String::new()).to_string(),
            "job-limits-mismatch"
        );
        assert_eq!(
            LaunchError::NotInOwnedJob(String::new()).to_string(),
            "not-in-owned-job"
        );
        assert_eq!(
            LaunchError::BirthTokenMismatch(String::new()).to_string(),
            "birth-token-mismatch"
        );
        assert_eq!(
            LaunchError::InitialTokenOpen(String::new()).to_string(),
            "initial-token-open"
        );
    }
}
