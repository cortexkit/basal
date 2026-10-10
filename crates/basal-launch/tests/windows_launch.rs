#![cfg(windows)]

#[test]
fn test_deviations_feature_contract() {
    #[cfg(not(feature = "deviations"))]
    {
        let d = basal_launch::Deviation::Full;
        match d {
            basal_launch::Deviation::Full => (),
        }
        assert_eq!(d.as_str(), "full");
    }

    #[cfg(feature = "deviations")]
    {
        let all = [
            basal_launch::Deviation::Full,
            basal_launch::Deviation::LpacOnly,
            basal_launch::Deviation::Plain,
            basal_launch::Deviation::ThreadTokenPresent,
            basal_launch::Deviation::IntegrityLowerFailed,
            basal_launch::Deviation::NotLpac,
            basal_launch::Deviation::CapabilitiesPresent,
            basal_launch::Deviation::RestrictingSidMismatch,
            basal_launch::Deviation::GroupNotDenyOnly,
            basal_launch::Deviation::PrivilegesPresent,
            basal_launch::Deviation::IntegrityNotUntrusted,
            basal_launch::Deviation::MitigationMismatch,
            basal_launch::Deviation::HandleNotAllowed,
            basal_launch::Deviation::JobLimitsMismatch,
            basal_launch::Deviation::NotInOwnedJob,
            basal_launch::Deviation::BirthTokenMismatch,
            basal_launch::Deviation::InitialTokenOpen,
        ];
        assert_eq!(all.len(), 17);
        for d in all {
            assert!(!d.as_str().is_empty());
        }
    }
}

#[cfg(all(windows, feature = "deviations"))]
mod windows_tests {
    use basal_launch::*;
    use std::path::PathBuf;
    use windows_sys::Win32::{
        Foundation::*, Security::*, System::JobObjects::*, System::Threading::*,
    };

    fn find_test_helper() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe");
        path.pop(); // remove test binary name
        if path.file_name().and_then(|s| s.to_str()) == Some("deps") {
            path.pop(); // remove deps
        }
        let helper = path.join("basal-launch-test-helper.exe");
        assert!(
            helper.exists(),
            "test helper binary not found at {}",
            helper.display()
        );
        helper
    }

    #[test]
    fn test_full_confinement_spawn_attestation_and_kill() {
        let helper = find_test_helper();
        let profile = create_or_open_profile(PROFILE_NAME).expect("create_or_open_profile");

        // Grant test binary directory access to the AppContainer package SID
        grant_test_binary_directory(helper.parent().unwrap(), profile.sid())
            .expect("grant_test_binary_directory");

        const COMMIT_LIMIT: u64 = 512 * 1024 * 1024;
        let mut options = LaunchOptions::new(&helper, COMMIT_LIMIT);
        options.with_deviation(Deviation::Full);

        let mut child = launch(&options).expect("launch full confinement");
        assert!(child.id() > 0, "child pid must be non-zero");

        unsafe {
            // Parent side verification: check process is in owned job
            let mut in_job = 0;
            let ok = IsProcessInJob(child.process_handle(), child.job_handle(), &mut in_job);
            assert_ne!(ok, 0, "IsProcessInJob API must succeed");
            assert_ne!(in_job, 0, "child must be in the owned job");

            // Parent side verification: query primary token
            let mut token = std::ptr::null_mut();
            let ok = OpenProcessToken(child.process_handle(), TOKEN_QUERY, &mut token);
            assert_ne!(ok, 0, "OpenProcessToken must succeed from parent");

            // 1. Token type
            let mut token_type = 0u32;
            let mut bytes = 0;
            let ok = GetTokenInformation(
                token,
                TokenType,
                (&mut token_type as *mut u32).cast(),
                4,
                &mut bytes,
            );
            assert_ne!(ok, 0, "GetTokenInformation(TokenType)");
            assert_eq!(token_type, TokenPrimary as u32);

            // 2. Low integrity
            let mut label_buf = [0usize; 64];
            let ok = GetTokenInformation(
                token,
                TokenIntegrityLevel,
                label_buf.as_mut_ptr().cast(),
                (label_buf.len() * std::mem::size_of::<usize>()) as u32,
                &mut bytes,
            );
            assert_ne!(ok, 0, "GetTokenInformation(TokenIntegrityLevel)");
            let label = &*label_buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
            let sub_count = *GetSidSubAuthorityCount(label.Label.Sid);
            assert!(sub_count > 0);
            let last_sub = *GetSidSubAuthority(label.Label.Sid, (sub_count - 1) as u32);
            assert_eq!(last_sub, 0x1000, "integrity must be Low (0x1000)");

            // 3. AppContainer SID matches
            let mut app_buf = [0usize; 64];
            let ok = GetTokenInformation(
                token,
                TokenAppContainerSid,
                app_buf.as_mut_ptr().cast(),
                (app_buf.len() * std::mem::size_of::<usize>()) as u32,
                &mut bytes,
            );
            assert_ne!(ok, 0, "GetTokenInformation(TokenAppContainerSid)");
            let app_sid =
                (*app_buf.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer;
            assert!(!app_sid.is_null());
            assert!(profile.sid().matches(app_sid));

            // 4. Zero capabilities
            let mut cap_buf = [0usize; 64];
            let ok = GetTokenInformation(
                token,
                TokenCapabilities,
                cap_buf.as_mut_ptr().cast(),
                (cap_buf.len() * std::mem::size_of::<usize>()) as u32,
                &mut bytes,
            );
            assert_ne!(ok, 0, "GetTokenInformation(TokenCapabilities)");
            let cap_count = (*cap_buf.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount;
            assert_eq!(cap_count, 0, "capabilities must be zero");

            CloseHandle(token);
        }

        // Kill child and verify exit code
        child.kill().expect("child.kill");
        let exit_code = child.wait().expect("child.wait");
        assert_eq!(
            exit_code, WORKER_KILL_CODE,
            "exit code must match WORKER_KILL_CODE"
        );
    }

    #[test]
    fn test_parent_check_job_limits_mismatch_refused() {
        let helper = find_test_helper();
        let mut options = LaunchOptions::new(&helper, 512 * 1024 * 1024);
        options.with_deviation(Deviation::JobLimitsMismatch);

        let err = launch(&options).expect_err("JobLimitsMismatch must be refused");
        assert_eq!(
            err.reason_token(),
            Some("job-limits-mismatch"),
            "expected job-limits-mismatch, got: {err}"
        );
    }

    #[test]
    fn test_parent_check_not_in_owned_job_refused() {
        let helper = find_test_helper();
        let mut options = LaunchOptions::new(&helper, 512 * 1024 * 1024);
        options.with_deviation(Deviation::NotInOwnedJob);

        let err = launch(&options).expect_err("NotInOwnedJob must be refused");
        assert_eq!(
            err.reason_token(),
            Some("not-in-owned-job"),
            "expected not-in-owned-job, got: {err}"
        );
    }

    #[test]
    fn test_parent_check_birth_token_mismatch_refused() {
        let helper = find_test_helper();
        let mut options = LaunchOptions::new(&helper, 512 * 1024 * 1024);
        options.with_deviation(Deviation::BirthTokenMismatch);

        let err = launch(&options).expect_err("BirthTokenMismatch must be refused");
        assert_eq!(
            err.reason_token(),
            Some("birth-token-mismatch"),
            "expected birth-token-mismatch, got: {err}"
        );
    }

    #[test]
    fn test_parent_check_initial_token_open_refused() {
        let helper = find_test_helper();
        let mut options = LaunchOptions::new(&helper, 512 * 1024 * 1024);
        options.with_deviation(Deviation::InitialTokenOpen);

        let err = launch(&options).expect_err("InitialTokenOpen must be refused");
        assert_eq!(
            err.reason_token(),
            Some("initial-token-open"),
            "expected initial-token-open, got: {err}"
        );
    }
}
