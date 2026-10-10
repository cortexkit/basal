//! Owned process wrapper providing lifecycle management, stdio pipes, and kill termination.

use std::fs::File;
use std::io;
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::*, System::JobObjects::TerminateJobObject, System::Threading::*,
};

/// Named exit code passed when terminating a confined worker process and its job.
///
/// Distinct from exit 70, STATUS_INVALID_HANDLE (0xc0000008), and STATUS_ACCESS_VIOLATION (0xc0000005).
pub const WORKER_KILL_CODE: u32 = 0xC000002B;

/// Alias for `WORKER_KILL_CODE`.
pub const KILL_CODE: u32 = WORKER_KILL_CODE;

/// Owned process wrapper holding the native process handle, PID, stdio pipes, and job object.
///
/// Dropping the wrapper terminates the process and job.
pub struct OwnedProcess {
    process: HANDLE,
    pid: u32,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    job: HANDLE,
    killed: bool,
}

unsafe impl Send for OwnedProcess {}
unsafe impl Sync for OwnedProcess {}

impl std::fmt::Debug for OwnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedProcess")
            .field("pid", &self.pid)
            .field("killed", &self.killed)
            .finish()
    }
}

impl OwnedProcess {
    /// Creates a new `OwnedProcess` wrapper from raw handles and optional stdio streams.
    pub fn new(
        process: HANDLE,
        pid: u32,
        stdin: Option<File>,
        stdout: Option<File>,
        stderr: Option<File>,
        job: HANDLE,
    ) -> Self {
        Self {
            process,
            pid,
            stdin,
            stdout,
            stderr,
            job,
            killed: false,
        }
    }

    /// Returns the OS process ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Returns the raw process handle.
    pub fn process_handle(&self) -> HANDLE {
        self.process
    }

    /// Returns the raw job object handle.
    pub fn job_handle(&self) -> HANDLE {
        self.job
    }

    /// Terminates the job object and the process with [`WORKER_KILL_CODE`].
    pub fn kill(&mut self) -> io::Result<()> {
        self.killed = true;
        unsafe {
            if !self.job.is_null() && self.job != INVALID_HANDLE_VALUE {
                let _ = TerminateJobObject(self.job, WORKER_KILL_CODE);
            }
            if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                let _ = TerminateProcess(self.process, WORKER_KILL_CODE);
            }
        }
        Ok(())
    }

    /// Terminates the job object and process using a shared reference.
    pub fn kill_shared(&self) -> io::Result<()> {
        unsafe {
            if !self.job.is_null() && self.job != INVALID_HANDLE_VALUE {
                let _ = TerminateJobObject(self.job, WORKER_KILL_CODE);
            }
            if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                let _ = TerminateProcess(self.process, WORKER_KILL_CODE);
            }
        }
        Ok(())
    }

    /// Checks if the process has exited without blocking.
    ///
    /// Returns `Ok(Some(exit_code))` if the process has terminated,
    /// `Ok(None)` if it is still running, or an `io::Error` on failure.
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        unsafe {
            let res = WaitForSingleObject(self.process, 0);
            if res == WAIT_OBJECT_0 {
                let mut code = 0u32;
                if GetExitCodeProcess(self.process, &mut code) == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(code))
            } else if res == WAIT_TIMEOUT {
                Ok(None)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    /// Waits indefinitely for the process to exit and returns its exit code.
    pub fn wait(&mut self) -> io::Result<u32> {
        unsafe {
            let res = WaitForSingleObject(self.process, INFINITE);
            if res == WAIT_OBJECT_0 {
                let mut code = 0u32;
                if GetExitCodeProcess(self.process, &mut code) == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(code)
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.kill();
        unsafe {
            if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                CloseHandle(self.process);
                self.process = null_mut();
            }
            if !self.job.is_null() && self.job != INVALID_HANDLE_VALUE {
                CloseHandle(self.job);
                self.job = null_mut();
            }
        }
    }
}

/// Evaluates whether a process exit status indicates a confinement fault.
///
/// Returns true for `0xc0000008` (invalid handle) and `0xc0000005` (access violation).
/// Returns false for exit 70, standard clean exit (0), and [`WORKER_KILL_CODE`].
pub fn is_confinement_fault_code(code: u32) -> bool {
    code == 0xC0000008 || code == 0xC0000005
}
