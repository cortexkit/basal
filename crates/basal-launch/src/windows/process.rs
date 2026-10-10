//! The running worker, owned together with its job.

use super::context::Context;
use super::job::{self, JobLimits};
use super::profile::PackageSid;
use super::token::{self, TokenFacts};
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, OpenProcessToken, TerminateProcess, WaitForSingleObject,
};

/// The exit code a killed worker reports, by analogy with a shell's 128 + 9
/// for SIGKILL. It is distinct from the worker's own refusal exit (70) and
/// from the NTSTATUS codes a confinement fault ends a process with.
pub const KILL_EXIT_CODE: u32 = 137;

/// A worker started by [`launch`](crate::launch), with its job and the
/// parent's ends of its standard pipes.
///
/// Dropping it kills the worker and waits for it to end, then closes the job
/// (whose kill-on-close limit ends anything else in it) and removes the
/// worker's window station, desktop and TEMP directory.
pub struct ConfinedProcess {
    process: OwnedHandle,
    pid: u32,
    job: OwnedHandle,
    package: PackageSid,
    inherited_stdio: [usize; 3],
    /// Writes to the worker's stdin.
    pub stdin: Option<File>,
    /// Reads the worker's stdout.
    pub stdout: Option<File>,
    /// Reads the worker's stderr.
    pub stderr: Option<File>,
    // Dropped last, after the worker has ended.
    context: Context,
}

impl ConfinedProcess {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        process: OwnedHandle,
        pid: u32,
        job: OwnedHandle,
        package: PackageSid,
        inherited_stdio: [usize; 3],
        stdin: File,
        stdout: File,
        stderr: File,
        context: Context,
    ) -> Self {
        Self {
            process,
            pid,
            job,
            package,
            inherited_stdio,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            context,
        }
    }

    /// The worker's process id.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// The package SID the worker runs under.
    pub fn package_sid(&self) -> &PackageSid {
        &self.package
    }

    /// The handle values the worker was given as stdin, stdout and stderr.
    /// An inherited handle keeps its value in the child, so these are also
    /// the values the worker sees.
    pub fn inherited_stdio(&self) -> [usize; 3] {
        self.inherited_stdio
    }

    /// The worker's private TEMP directory.
    pub fn temp_dir(&self) -> &Path {
        &self.context.temp
    }

    /// Kills every process in the worker's job, then the worker itself,
    /// with [`KILL_EXIT_CODE`]. Killing a worker that has already ended
    /// succeeds.
    pub fn kill(&self) -> io::Result<()> {
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), KILL_EXIT_CODE);
            if TerminateProcess(self.process.as_raw_handle(), KILL_EXIT_CODE) != 0 {
                return Ok(());
            }
        }
        let error = io::Error::last_os_error();
        // Terminating a process that has already ended fails with access
        // denied; that worker is as killed as it will get.
        match self.try_wait()? {
            Some(_) => Ok(()),
            None => Err(error),
        }
    }

    /// The exit code, if the worker has ended. Never blocks.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Blocks until the worker ends, and returns its exit code.
    pub fn wait(&self) -> io::Result<u32> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 => self.exit_code(),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn exit_code(&self) -> io::Result<u32> {
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }

    /// Reads the worker's current primary token.
    pub fn primary_token(&self) -> Result<TokenFacts, String> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(self.process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(super::native::last("OpenProcessToken(worker)"));
        }
        let token = super::native::owned(token, "OpenProcessToken(worker)")?;
        token::read_facts(token.as_raw_handle())
    }

    /// Reads the limits of the worker's job through the parent's own handle.
    pub fn job_limits(&self) -> Result<JobLimits, String> {
        job::read_limits(self.job.as_raw_handle())
    }

    /// Whether the worker is in the job the parent created for it.
    pub fn in_owned_job(&self) -> Result<bool, String> {
        job::contains(self.job.as_raw_handle(), self.process.as_raw_handle())
    }
}

impl AsRawHandle for ConfinedProcess {
    /// The process handle.
    fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }
}

impl std::fmt::Debug for ConfinedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedProcess")
            .field("pid", &self.pid)
            .field("package", &self.package)
            .finish_non_exhaustive()
    }
}

impl Drop for ConfinedProcess {
    fn drop(&mut self) {
        if self.kill().is_ok() {
            let _ = self.wait();
        }
    }
}
