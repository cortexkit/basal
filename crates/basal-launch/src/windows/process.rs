//! The running child, owned together with its job.

use super::context::Context;
use super::job::{self, JobLimits};
use super::profile::PackageSid;
use super::token::{self, TokenFacts};
use std::fs::File;
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::windows::io::{AsRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, OpenProcessToken, TerminateProcess, WaitForSingleObject,
};

/// The exit code a killed child reports, by analogy with a shell's 128 + 9
/// for SIGKILL. It is distinct from the worker's own refusal exit (70) and
/// from the NTSTATUS codes a confinement fault ends a process with.
pub const KILL_EXIT_CODE: u32 = 137;

/// A child process started by this crate, owned together with the job it
/// was born in and the parent's ends of its standard pipes.
///
/// Dropping it kills the child and waits for it to end, then closes the job,
/// whose kill-on-close limit ends anything else still in it.
pub struct OwnedProcess {
    process: OwnedHandle,
    pid: u32,
    job: OwnedHandle,
    /// Writes to the child's stdin, when it was piped.
    pub stdin: Option<File>,
    /// Reads the child's stdout, when it was piped.
    pub stdout: Option<File>,
    /// Reads the child's stderr, when it was piped.
    pub stderr: Option<File>,
}

impl OwnedProcess {
    pub(crate) fn new(
        process: OwnedHandle,
        pid: u32,
        job: OwnedHandle,
        stdin: Option<File>,
        stdout: Option<File>,
        stderr: Option<File>,
    ) -> Self {
        Self {
            process,
            pid,
            job,
            stdin,
            stdout,
            stderr,
        }
    }

    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Kills every process in the child's job, then the child itself, with
    /// [`KILL_EXIT_CODE`]. Killing a child that has already ended succeeds.
    pub fn kill(&self) -> io::Result<()> {
        let job_killed =
            unsafe { TerminateJobObject(self.job.as_raw_handle(), KILL_EXIT_CODE) } != 0;
        if unsafe { TerminateProcess(self.process.as_raw_handle(), KILL_EXIT_CODE) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // Terminating a process that is already ending fails with access
        // denied. That happens right after the job kill above, which ends the
        // child (a member of the job) whether or not it has finished exiting
        // yet, and for a child that had already exited.
        if job_killed && self.in_owned_job().unwrap_or(false) {
            return Ok(());
        }
        match self.try_wait()? {
            Some(_) => Ok(()),
            None => Err(error),
        }
    }

    /// The exit code, if the child has ended. Never blocks.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Blocks until the child ends, and returns its exit code.
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

    /// Reads the limits of the child's job through the parent's own handle.
    pub fn job_limits(&self) -> Result<JobLimits, String> {
        job::read_limits(self.job.as_raw_handle())
    }

    /// Whether the child is in the job the parent created for it.
    pub fn in_owned_job(&self) -> Result<bool, String> {
        job::contains(self.job.as_raw_handle(), self.process.as_raw_handle())
    }
}

impl AsRawHandle for OwnedProcess {
    /// The process handle.
    fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }
}

impl std::fmt::Debug for OwnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedProcess")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.kill().is_ok() {
            let _ = self.wait();
        }
    }
}

/// A worker started by [`launch`](crate::launch): the [`OwnedProcess`]
/// (reached through `Deref`, so `kill`, `wait`, `try_wait` and the pipes are
/// the same as for any child) plus what only a confined launch has.
///
/// Dropping it kills the worker and waits for it to end, then closes the job
/// and removes the worker's window station, desktop and TEMP directory.
pub struct ConfinedProcess {
    // Fields drop in order: the process (killed and waited for) first, the
    // start-up context last, after the worker has ended.
    process: OwnedProcess,
    package: PackageSid,
    inherited_stdio: [usize; 3],
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
            process: OwnedProcess::new(process, pid, job, Some(stdin), Some(stdout), Some(stderr)),
            package,
            inherited_stdio,
            context,
        }
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

    /// Reads the worker's current primary token.
    pub fn primary_token(&self) -> Result<TokenFacts, String> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(self.process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(super::native::last("OpenProcessToken(worker)"));
        }
        let token = super::native::owned(token, "OpenProcessToken(worker)")?;
        token::read_facts(token.as_raw_handle())
    }
}

impl Deref for ConfinedProcess {
    type Target = OwnedProcess;
    fn deref(&self) -> &OwnedProcess {
        &self.process
    }
}

impl DerefMut for ConfinedProcess {
    fn deref_mut(&mut self) -> &mut OwnedProcess {
        &mut self.process
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
            .field("pid", &self.process.pid)
            .field("package", &self.package)
            .finish_non_exhaustive()
    }
}
