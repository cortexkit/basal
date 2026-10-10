//! The job a confined worker is born in, and its read-back.

use super::native::{Result, check, owned};
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};

/// The job's limit flags, `0x2508`: kill every member when the last job
/// handle closes, end a member on an unhandled exception instead of showing
/// an error dialog, bound each member's committed memory, and bound the
/// number of live members. No breakaway flag is set, so a member can never
/// leave the job.
pub const JOB_LIMIT_FLAGS: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
    | JOB_OBJECT_LIMIT_PROCESS_MEMORY
    | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;

/// The UI restrictions, `0xff`: no USER handles from outside the job, no
/// clipboard reads or writes, no system-parameter or display-settings
/// changes, no global atoms, no desktop switching and no exit-Windows.
/// Newer SDKs also define an input-method restriction, `0x100`, and fold it
/// into `JOB_OBJECT_UILIMIT_ALL`. The confinement was validated with `0xff`,
/// so that is the value set and required on read-back.
pub const JOB_UI_RESTRICTIONS: u32 = 0xff;

/// The limits of a job, as read back from the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// `LimitFlags` of the extended limit information.
    pub flags: u32,
    /// The most processes the job may hold at once.
    pub active_process_limit: u32,
    /// The commit limit of each process, in bytes.
    pub process_memory_limit: usize,
    /// The commit limit of the whole job, in bytes; unset is 0.
    pub job_memory_limit: usize,
    /// The UI restriction class.
    pub ui_restrictions: u32,
}

impl JobLimits {
    /// The limits a confined worker's job must have, for the given commit
    /// limit: one live process, no breakaway, no whole-job memory limit, and
    /// every UI restriction.
    pub fn confined(commit_bytes: usize) -> Self {
        Self {
            flags: JOB_LIMIT_FLAGS,
            active_process_limit: 1,
            process_memory_limit: commit_bytes,
            job_memory_limit: 0,
            ui_restrictions: JOB_UI_RESTRICTIONS,
        }
    }
}

/// Creates the confined worker's job. Its handle is not inheritable, so it
/// never reaches the worker.
pub(crate) fn create_confined(
    commit_bytes: usize,
    active_process_limit: u32,
) -> Result<OwnedHandle> {
    let job = owned(
        unsafe { CreateJobObjectW(null(), null()) },
        "CreateJobObjectW",
    )?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_LIMIT_FLAGS;
    limits.BasicLimitInformation.ActiveProcessLimit = active_process_limit;
    limits.ProcessMemoryLimit = commit_bytes;
    set_limits(job.as_raw_handle(), &limits)?;
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_UI_RESTRICTIONS,
    };
    check(
        unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        },
        "SetInformationJobObject(UI restrictions)",
    )?;
    Ok(job)
}

/// Creates a job whose only limit is killing its members when the last
/// handle closes. The positive controls and the token source process are
/// put in one, so a parent that dies never leaves them behind.
pub(crate) fn create_kill_on_close() -> Result<OwnedHandle> {
    let job = owned(
        unsafe { CreateJobObjectW(null(), null()) },
        "CreateJobObjectW",
    )?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    set_limits(job.as_raw_handle(), &limits)?;
    Ok(job)
}

fn set_limits(job: HANDLE, limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> Result<()> {
    check(
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        },
        "SetInformationJobObject(limits)",
    )
}

/// Reads a job's limits back through the given job handle.
///
/// The handle matters: a query with a NULL handle describes whichever job
/// the caller itself is in, such as a CI runner's, not the worker's.
pub(crate) fn read_limits(job: HANDLE) -> Result<JobLimits> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { zeroed() };
    check(
        unsafe {
            QueryInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        },
        "QueryInformationJobObject(limits)",
    )?;
    check(
        unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicUIRestrictions,
                (&mut ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                null_mut(),
            )
        },
        "QueryInformationJobObject(UI restrictions)",
    )?;
    Ok(JobLimits {
        flags: limits.BasicLimitInformation.LimitFlags,
        active_process_limit: limits.BasicLimitInformation.ActiveProcessLimit,
        process_memory_limit: limits.ProcessMemoryLimit,
        job_memory_limit: limits.JobMemoryLimit,
        ui_restrictions: ui.UIRestrictionsClass,
    })
}

/// Whether `process` is a member of `job`.
pub(crate) fn contains(job: HANDLE, process: HANDLE) -> Result<bool> {
    let mut member = 0;
    check(
        unsafe { IsProcessInJob(process, job, &mut member) },
        "IsProcessInJob",
    )?;
    Ok(member != 0)
}
