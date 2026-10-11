//! The Windows launcher.

mod context;
mod deviation;
mod error;
mod job;
mod launch;
mod native;
mod plain;
mod process;
mod profile;
mod spawn_lock;
mod token;

pub use deviation::Deviation;
pub use error::{LaunchError, Refusal};
pub use job::{JOB_LIMIT_FLAGS, JOB_UI_RESTRICTIONS, JobLimits};
pub use launch::{
    LaunchOptions, MITIGATION_EXTENSION_POINT_DISABLE, MITIGATION_MICROSOFT_SIGNED_ONLY,
    MITIGATION_NO_LOW_LABEL_IMAGES, MITIGATION_NO_REMOTE_IMAGES, MITIGATION_POLICY,
    MITIGATION_PREFER_SYSTEM32_IMAGES, MITIGATION_PROHIBIT_DYNAMIC_CODE,
    MITIGATION_STRICT_HANDLE_CHECKS, MITIGATION_WIN32K_SYSTEM_CALL_DISABLE, launch,
};
pub use plain::{PlainCommand, PlainStdio, spawn_plain};
pub use process::{ConfinedProcess, KILL_EXIT_CODE, OwnedProcess};
pub use profile::{
    PROFILE_NAME, PackageSid, create_or_open_profile, grant_test_binary_directory,
    process_profile_directory,
};
pub use spawn_lock::{SpawnLock, make_inheritable, spawn_lock, spawn_lock_held};
pub use token::{
    IMPERSONATION_LEVEL, LOW_INTEGRITY, NULL_SID, TOKEN_IMPERSONATION, TOKEN_PRIMARY, TokenFacts,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_reasons_are_the_named_tokens() {
        let all = [
            (
                Refusal::AppContainerProfileUnavailable,
                "appcontainer-profile-unavailable",
            ),
            (Refusal::JobLimitsMismatch, "job-limits-mismatch"),
            (Refusal::NotInOwnedJob, "not-in-owned-job"),
            (Refusal::BirthTokenMismatch, "birth-token-mismatch"),
            (Refusal::InitialTokenOpen, "initial-token-open"),
        ];
        for (refusal, reason) in all {
            assert_eq!(refusal.reason(), reason);
            let error = LaunchError::refused(refusal, "detail");
            assert_eq!(error.reason(), Some(reason));
            assert_eq!(error.to_string(), format!("{reason}: detail"));
        }
        assert_eq!(LaunchError::Failed("x".into()).reason(), None);
    }

    #[test]
    fn the_job_flags_are_0x2508() {
        assert_eq!(JOB_LIMIT_FLAGS, 0x2508);
        let limits = JobLimits::confined(512 << 20);
        assert_eq!(limits.active_process_limit, 1);
        assert_eq!(limits.job_memory_limit, 0);
        assert_eq!(limits.ui_restrictions, 0xff);
    }

    /// A profile name the system rejects (longer than 64 characters) is
    /// refused as `appcontainer-profile-unavailable`, never launched without.
    #[test]
    fn an_unusable_profile_is_refused_by_name() {
        let name = "x".repeat(65);
        let error = profile::create_or_open_named(&name)
            .map_err(profile::unavailable)
            .expect_err("a 65-character profile name must be rejected");
        assert_eq!(
            error.refusal(),
            Some(Refusal::AppContainerProfileUnavailable)
        );
    }
}
