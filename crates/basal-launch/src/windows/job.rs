//! Parent-owned job object setup, limit enforcement, and pre-resume verification.

use crate::windows::deviation::Deviation;
use crate::windows::error::LaunchError;
use crate::windows::native::{Handle, JOB_OBJECT_UILIMIT_ALL, last_win32_error};
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use windows_sys::Win32::{Foundation::*, System::JobObjects::*};

/// The expected job object limit flags: 0x2508.
/// - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (0x2000)
/// - `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` (0x400)
/// - `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (0x100)
/// - `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (0x008)
pub const JOB_LIMIT_FLAGS: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;

/// Creates and configures the parent-owned job object with confinement limits.
pub fn create_confined_job(commit_limit: u64, deviation: Deviation) -> Result<Handle, LaunchError> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(last_win32_error("CreateJobObjectW"));
        }
        let job = Handle(job);

        // Job handle must be non-inheritable
        SetHandleInformation(job.0, HANDLE_FLAG_INHERIT, 0);

        let flags = JOB_LIMIT_FLAGS;
        #[allow(unused_mut)]
        let mut active_limit = 1;
        let process_memory = commit_limit as usize;

        #[cfg(feature = "deviations")]
        if deviation == Deviation::JobLimitsMismatch {
            // Intentionally mismatch limits to trigger job-limits-mismatch refusal
            active_limit = 2;
        }

        let _ = deviation;

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = flags;
        limits.BasicLimitInformation.ActiveProcessLimit = active_limit;
        limits.ProcessMemoryLimit = process_memory;

        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(last_win32_error("SetInformationJobObject(limits)"));
        }

        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_ALL, // 0xff
        };
        if SetInformationJobObject(
            job.0,
            JobObjectBasicUIRestrictions,
            (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            return Err(last_win32_error("SetInformationJobObject(UI)"));
        }

        Ok(job)
    }
}

/// Reads back the job limits through the owned handle and checks child membership.
///
/// On mismatch, returns `LaunchError::JobLimitsMismatch` or `LaunchError::NotInOwnedJob`.
pub fn check_job_before_resume(
    job: HANDLE,
    process: HANDLE,
    expected_commit_limit: u64,
) -> Result<(), LaunchError> {
    unsafe {
        let mut actual: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        if QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut actual as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        ) == 0
        {
            return Err(LaunchError::JobLimitsMismatch(
                "failed to query job limits through owned handle".to_string(),
            ));
        }

        let mut actual_ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = zeroed();
        if QueryInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            (&mut actual_ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            null_mut(),
        ) == 0
        {
            return Err(LaunchError::JobLimitsMismatch(
                "failed to query job UI restrictions".to_string(),
            ));
        }

        let flags = actual.BasicLimitInformation.LimitFlags;
        let active = actual.BasicLimitInformation.ActiveProcessLimit;
        let mem = actual.ProcessMemoryLimit;
        let ui = actual_ui.UIRestrictionsClass;

        if flags != JOB_LIMIT_FLAGS
            || active != 1
            || mem != (expected_commit_limit as usize)
            || ui != JOB_OBJECT_UILIMIT_ALL
            || (flags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)) != 0
        {
            return Err(LaunchError::JobLimitsMismatch(format!(
                "flags: 0x{flags:08x} (expected 0x{JOB_LIMIT_FLAGS:08x}), active: {active}, mem: {mem}, ui: 0x{ui:02x}"
            )));
        }

        let mut in_job = 0;
        if IsProcessInJob(process, job, &mut in_job) == 0 || in_job == 0 {
            return Err(LaunchError::NotInOwnedJob(
                "child process is not a member of the owned job".to_string(),
            ));
        }

        Ok(())
    }
}
