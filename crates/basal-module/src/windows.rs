//! The module's Windows-only parts: starting the worker through the
//! launcher, killing and reaping it through the launcher's owned wrapper,
//! and which exit codes mean the worker hit its confinement.
//!
//! On macOS and Linux the worker confines itself after `exec`, and a
//! confinement violation kills it with SIGSYS. On Windows most of the
//! confinement is fixed by `basal-launch` when the process is created, and a
//! violation the worker cannot survive ends it with an NTSTATUS exit code.

use std::io;
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

use basal_launch::{ConfinedProcess, LaunchOptions, launch};
use basal_proto::{CODEMODE_JOB_COMMIT_BYTES, FLOW_JOB_COMMIT_BYTES};

use crate::process::SpawnError;

/// A Windows worker must report every startup check, not just its token kind.
pub(crate) fn accepts_report(
    lpac: bool,
    untrusted: bool,
    no_thread_token: bool,
    mitigations: bool,
    handle_table: bool,
) -> bool {
    lpac && untrusted && no_thread_token && mitigations && handle_table
}

/// `STATUS_ACCESS_VIOLATION`: the worker touched memory it may not, for
/// example code the dynamic-code policy refused to make executable.
pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;

/// `STATUS_INVALID_HANDLE`: strict handle checks end the worker when it uses
/// a handle it does not hold, such as one its startup checks closed.
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;

/// Whether a worker's exit code is a confinement fault. The worker's own
/// refusal (exit 70) and the launcher's kill code are not: neither is the
/// confinement stopping the worker in the act.
pub fn is_fault_exit(code: u32) -> bool {
    matches!(code, STATUS_ACCESS_VIOLATION | STATUS_INVALID_HANDLE)
}

/// Starts the worker under the full confinement, with the job commit limit
/// of its profile.
pub(crate) fn start(binary: &Path, codemode: bool) -> Result<ConfinedProcess, SpawnError> {
    let commit_bytes = if codemode {
        CODEMODE_JOB_COMMIT_BYTES
    } else {
        FLOW_JOB_COMMIT_BYTES
    };
    launch(&LaunchOptions::new(binary, commit_bytes))
        .map_err(|error| SpawnError::Exec(format!("{}: {error}", binary.display())))
}

/// The exit status, if the worker has ended. Never blocks.
pub(crate) fn try_wait(child: &ConfinedProcess) -> io::Result<Option<ExitStatus>> {
    Ok(child.try_wait()?.map(ExitStatus::from_raw))
}

/// Kills the worker's job and process and waits for the process to end.
/// Errors are ignored: an ended worker is reaped either way.
pub(crate) fn kill_and_reap(child: &ConfinedProcess) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Kills a worker that is still running; returns whether a kill was issued.
/// One that has already ended, or whose state cannot be read, is left alone.
pub(crate) fn kill_if_running(child: &ConfinedProcess) -> bool {
    if !matches!(child.try_wait(), Ok(None)) {
        return false;
    }
    child.kill().is_ok()
}

/// Ends this process at once with the launcher's kill code, as an outside
/// kill would (see `crate::fatal::kill_self`).
pub(crate) fn terminate_self() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    // SAFETY: the pseudo-handle of the current process is always valid.
    unsafe {
        TerminateProcess(GetCurrentProcess(), basal_launch::KILL_EXIT_CODE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_statuses_count_and_the_refusal_and_kill_codes_do_not() {
        assert!(is_fault_exit(0xc000_0008));
        assert!(is_fault_exit(0xc000_0005));
        assert!(!is_fault_exit(70));
        assert!(!is_fault_exit(basal_launch::KILL_EXIT_CODE));
        assert!(!is_fault_exit(0));
    }
}
