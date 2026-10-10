//! Launch options and the primary `launch` entry point for Windows processes.

use crate::windows::deviation::Deviation;
use crate::windows::error::LaunchError;
use crate::windows::job::{check_job_before_resume, create_confined_job};
use crate::windows::native::*;
use crate::windows::process::{OwnedProcess, WORKER_KILL_CODE};
use crate::windows::profile::{AppContainerSid, PROFILE_NAME, create_or_open_profile};
use crate::windows::token::{check_birth_token, check_initial_token_and_close, construct_tokens};
use std::mem::{size_of, zeroed};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::ptr::null;
use windows_sys::Win32::{
    Foundation::*, Security::*, System::JobObjects::TerminateJobObject, System::Threading::*,
};

/// Default mitigation policy flags:
/// - ProhibitDynamicCode
/// - MicrosoftSignedOnly
/// - NoRemoteImages
/// - NoLowMandatoryLabelImages
/// - PreferSystem32Images
/// - DisallowWin32kSystemCalls
/// - StrictHandleChecks
/// - DisableExtensionPoints
pub const MITIGATION_POLICY_FLAGS: u64 = (1 << 24) | // StrictHandleChecks
    (1 << 28) | // DisallowWin32kSystemCalls
    (1 << 32) | // DisableExtensionPoints
    (1 << 36) | // ProhibitDynamicCode
    (1 << 44) | // MicrosoftSignedOnly
    (1 << 52) | // NoRemoteImages
    (1 << 56) | // NoLowMandatoryLabelImages
    (1 << 60); // PreferSystem32Images

/// Options configuring process launch under Windows confinement.
///
/// In production builds (without the `deviations` feature), only the full confinement
/// recipe can be launched.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Path to the worker binary to execute.
    pub binary: PathBuf,
    /// Arguments to pass to the process.
    pub args: Vec<String>,
    /// Explicit package SID. If None, derived from [`PROFILE_NAME`].
    pub package_sid: Option<String>,
    /// Commit limit in bytes for the job object.
    pub commit_limit: u64,
    /// Confinement deviation mode (available only with the `deviations` feature).
    #[cfg(feature = "deviations")]
    pub deviation: Deviation,
    /// Optional environment overrides (defaults to retaining only `SystemRoot`).
    pub environment: Option<Vec<(String, String)>>,
}

impl LaunchOptions {
    /// Creates default launch options with full confinement for the given binary and commit limit.
    pub fn new(binary: impl Into<PathBuf>, commit_limit: u64) -> Self {
        Self {
            binary: binary.into(),
            args: Vec::new(),
            package_sid: None,
            commit_limit,
            #[cfg(feature = "deviations")]
            deviation: Deviation::Full,
            environment: None,
        }
    }

    /// Appends an argument.
    pub fn arg(&mut self, arg: impl Into<String>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Appends multiple arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for a in args {
            self.args.push(a.into());
        }
        self
    }

    /// Sets the confinement deviation mode. Available only with the `deviations` feature.
    #[cfg(feature = "deviations")]
    pub fn with_deviation(&mut self, deviation: Deviation) -> &mut Self {
        self.deviation = deviation;
        self
    }

    /// Overrides the package SID.
    pub fn with_package_sid(&mut self, sid: impl Into<String>) -> &mut Self {
        self.package_sid = Some(sid.into());
        self
    }
}

/// Launches a child worker process under Windows confinement according to [`LaunchOptions`].
pub fn launch(options: &LaunchOptions) -> Result<OwnedProcess, LaunchError> {
    unsafe {
        #[cfg(feature = "deviations")]
        let deviation = options.deviation;
        #[cfg(not(feature = "deviations"))]
        let deviation = Deviation::Full;

        // Resolve package profile and SID
        #[allow(unused_mut)]
        let mut need_package_sid = true;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::Plain {
            need_package_sid = false;
        }

        let package_sid = if !need_package_sid {
            None
        } else if let Some(sid_str) = &options.package_sid {
            Some(AppContainerSid::from_string_sid(sid_str)?)
        } else {
            let profile = create_or_open_profile(PROFILE_NAME)?;
            Some(profile.sid().clone())
        };

        // Construct command line
        let mut cmd = format!("\"{}\"", options.binary.display());
        if let Some(sid) = &package_sid {
            if !options.args.iter().any(|a| a.starts_with("--package-sid=")) {
                cmd.push_str(&format!(" \"--package-sid={}\"", sid.as_str()));
            }
        }
        for arg in &options.args {
            cmd.push_str(&format!(" \"{arg}\""));
        }
        let mut cmd_wide = wide(&cmd);

        // Create standard I/O pipes
        let (child_stdin, parent_stdin) = create_pipe(true)?;
        let (parent_stdout, child_stdout) = create_pipe(true)?;
        let (parent_stderr, child_stderr) = create_pipe(true)?;

        SetHandleInformation(parent_stdin.0, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(parent_stdout.0, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(parent_stderr.0, HANDLE_FLAG_INHERIT, 0);

        // Prepare environment block: cleared except SystemRoot (or custom)
        let env_block: Vec<u16> = if let Some(env_vars) = &options.environment {
            let mut block = Vec::new();
            for (k, v) in env_vars {
                block.extend(format!("{k}={v}\0").encode_utf16());
            }
            block.push(0);
            block
        } else {
            let sys_root =
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
            format!("SystemRoot={sys_root}\0\0")
                .encode_utf16()
                .collect()
        };

        // Handle plain control launch
        #[cfg(feature = "deviations")]
        if deviation == Deviation::Plain {
            let mut si: STARTUPINFOW = zeroed();
            si.cb = size_of::<STARTUPINFOW>() as u32;
            si.dwFlags = STARTF_USESTDHANDLES;
            si.hStdInput = child_stdin.0;
            si.hStdOutput = child_stdout.0;
            si.hStdError = child_stderr.0;

            let mut pi: PROCESS_INFORMATION = zeroed();
            let ok = CreateProcessW(
                wide(&options.binary.to_string_lossy()).as_ptr(),
                cmd_wide.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                env_block.as_ptr().cast(),
                null(),
                &si,
                &mut pi,
            );

            if ok == 0 {
                return Err(last_win32_error("CreateProcessW(plain)"));
            }

            let process = Handle(pi.hProcess);
            let thread = Handle(pi.hThread);

            drop(child_stdin);
            drop(child_stdout);
            drop(child_stderr);

            let stdin_file = std::fs::File::from_raw_handle(parent_stdin.into_raw() as _);
            let stdout_file = std::fs::File::from_raw_handle(parent_stdout.into_raw() as _);
            let stderr_file = std::fs::File::from_raw_handle(parent_stderr.into_raw() as _);

            if ResumeThread(thread.0) == u32::MAX {
                TerminateProcess(process.0, WORKER_KILL_CODE);
                return Err(last_win32_error("ResumeThread(plain)"));
            }

            return Ok(OwnedProcess::new(
                process.into_raw(),
                pi.dwProcessId,
                Some(stdin_file),
                Some(stdout_file),
                Some(stderr_file),
                std::ptr::null_mut(),
            ));
        }

        let package_sid = package_sid.unwrap();

        // Handle LPAC-only control launch
        #[cfg(feature = "deviations")]
        if deviation == Deviation::LpacOnly {
            let mut attrs = AttributeList::new(3)?;
            let handles = [child_stdin.0, child_stdout.0, child_stderr.0];
            attrs.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;

            let mut dummy_cap: SID_AND_ATTRIBUTES = zeroed();
            let security = SECURITY_CAPABILITIES {
                AppContainerSid: package_sid.as_psid(),
                Capabilities: &mut dummy_cap as *mut _,
                CapabilityCount: 0,
                Reserved: 0,
            };
            attrs.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
            attrs.add(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                &PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
            )?;

            let mut siex: STARTUPINFOEXW = zeroed();
            siex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            siex.StartupInfo.hStdInput = child_stdin.0;
            siex.StartupInfo.hStdOutput = child_stdout.0;
            siex.StartupInfo.hStdError = child_stderr.0;
            siex.lpAttributeList = attrs.ptr();

            let mut pi: PROCESS_INFORMATION = zeroed();
            let ok = CreateProcessW(
                wide(&options.binary.to_string_lossy()).as_ptr(),
                cmd_wide.as_mut_ptr(),
                null(),
                null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT,
                env_block.as_ptr().cast(),
                null(),
                &siex.StartupInfo,
                &mut pi,
            );

            if ok == 0 {
                return Err(last_win32_error("CreateProcessW(lpac-only)"));
            }

            let process = Handle(pi.hProcess);
            let thread = Handle(pi.hThread);

            drop(child_stdin);
            drop(child_stdout);
            drop(child_stderr);

            let stdin_file = std::fs::File::from_raw_handle(parent_stdin.into_raw() as _);
            let stdout_file = std::fs::File::from_raw_handle(parent_stdout.into_raw() as _);
            let stderr_file = std::fs::File::from_raw_handle(parent_stderr.into_raw() as _);

            if ResumeThread(thread.0) == u32::MAX {
                TerminateProcess(process.0, WORKER_KILL_CODE);
                return Err(last_win32_error("ResumeThread(lpac-only)"));
            }

            return Ok(OwnedProcess::new(
                process.into_raw(),
                pi.dwProcessId,
                Some(stdin_file),
                Some(stdout_file),
                Some(stderr_file),
                std::ptr::null_mut(),
            ));
        }

        // Full confinement or deviation launch
        let job = create_confined_job(options.commit_limit, deviation)?;
        let mut tokens = construct_tokens(&package_sid, deviation)?;

        let mut attrs = AttributeList::new(6)?;

        // Handle list attribute
        #[allow(unused_mut)]
        let mut planted_handle: Option<Handle> = None;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::HandleNotAllowed {
            let (extra_r, _extra_w) = create_pipe(true)?;
            planted_handle = Some(extra_r);
        }

        if let Some(extra) = &planted_handle {
            let handles = [child_stdin.0, child_stdout.0, child_stderr.0, extra.0];
            attrs.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        } else {
            let handles = [child_stdin.0, child_stdout.0, child_stderr.0];
            attrs.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        }

        // Security capabilities attribute
        let mut dummy_cap: SID_AND_ATTRIBUTES = zeroed();
        #[allow(unused_mut)]
        let mut cap_count = 0;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::CapabilitiesPresent {
            cap_count = 1;
        }

        let security = SECURITY_CAPABILITIES {
            AppContainerSid: package_sid.as_psid(),
            Capabilities: &mut dummy_cap as *mut _,
            CapabilityCount: cap_count,
            Reserved: 0,
        };
        attrs.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;

        // All application packages opt-out policy
        #[allow(unused_mut)]
        let mut add_opt_out = true;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::NotLpac {
            add_opt_out = false;
        }

        if add_opt_out {
            attrs.add(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                &PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
            )?;
        }

        // Job list attribute
        #[allow(unused_mut)]
        let mut add_job = true;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::NotInOwnedJob {
            add_job = false;
        }

        if add_job {
            let job_handles = [job.0];
            attrs.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &job_handles)?;
        }

        // Mitigation policy
        #[allow(unused_mut)]
        let mut mitigations = MITIGATION_POLICY_FLAGS;
        #[cfg(feature = "deviations")]
        if deviation == Deviation::MitigationMismatch {
            // Omit ProhibitDynamicCode (bit 36)
            mitigations &= !(1 << 36);
        }
        attrs.add(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigations)?;

        // Child process policy: restrict child processes
        attrs.add(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
            &PROCESS_CREATION_CHILD_PROCESS_RESTRICTED,
        )?;

        let mut siex: STARTUPINFOEXW = zeroed();
        siex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        siex.StartupInfo.hStdInput = child_stdin.0;
        siex.StartupInfo.hStdOutput = child_stdout.0;
        siex.StartupInfo.hStdError = child_stderr.0;
        siex.lpAttributeList = attrs.ptr();

        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessAsUserW(
            tokens.primary.0,
            wide(&options.binary.to_string_lossy()).as_ptr(),
            cmd_wide.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr().cast(),
            null(),
            &siex.StartupInfo,
            &mut pi,
        );

        if ok == 0 {
            return Err(last_win32_error("CreateProcessAsUserW"));
        }

        let process = Handle(pi.hProcess);
        let main_thread = Handle(pi.hThread);

        drop(child_stdin);
        drop(child_stdout);
        drop(child_stderr);

        let stdin_file = std::fs::File::from_raw_handle(parent_stdin.into_raw() as _);
        let stdout_file = std::fs::File::from_raw_handle(parent_stdout.into_raw() as _);
        let stderr_file = std::fs::File::from_raw_handle(parent_stderr.into_raw() as _);

        // Assign initial thread token to the suspended main thread
        if let Some(initial) = &tokens.initial {
            if SetThreadToken(&main_thread.0, initial.0) == 0 {
                TerminateProcess(process.0, WORKER_KILL_CODE);
                TerminateJobObject(job.0, WORKER_KILL_CODE);
                return Err(last_win32_error("SetThreadToken(suspended main thread)"));
            }
        }

        // Pre-resume check 1: Job limits and membership
        if let Err(e) = check_job_before_resume(job.0, process.0, options.commit_limit) {
            TerminateProcess(process.0, WORKER_KILL_CODE);
            TerminateJobObject(job.0, WORKER_KILL_CODE);
            return Err(e);
        }

        // Pre-resume check 2: Birth primary token
        if let Err(e) = check_birth_token(process.0, &package_sid) {
            TerminateProcess(process.0, WORKER_KILL_CODE);
            TerminateJobObject(job.0, WORKER_KILL_CODE);
            return Err(e);
        }

        // Pre-resume check 3: Initial thread token and close
        if let Err(e) =
            check_initial_token_and_close(main_thread.0, &mut tokens, &package_sid, deviation)
        {
            TerminateProcess(process.0, WORKER_KILL_CODE);
            TerminateJobObject(job.0, WORKER_KILL_CODE);
            return Err(e);
        }

        // Resume thread
        if ResumeThread(main_thread.0) == u32::MAX {
            TerminateProcess(process.0, WORKER_KILL_CODE);
            TerminateJobObject(job.0, WORKER_KILL_CODE);
            return Err(last_win32_error("ResumeThread"));
        }

        Ok(OwnedProcess::new(
            process.into_raw(),
            pi.dwProcessId,
            Some(stdin_file),
            Some(stdout_file),
            Some(stderr_file),
            job.into_raw(),
        ))
    }
}
