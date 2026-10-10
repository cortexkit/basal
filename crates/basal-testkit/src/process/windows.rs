//! Native worker ownership and process observations for the Windows test parent.

use basal_launch::{ConfinedProcess, LaunchOptions};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ, TerminateProcess,
    WaitForSingleObject,
};

struct ReapState {
    process: Weak<ConfinedProcess>,
    signaled: bool,
}

fn processes() -> &'static Mutex<HashMap<u32, ReapState>> {
    static PROCESSES: OnceLock<Mutex<HashMap<u32, ReapState>>> = OnceLock::new();
    PROCESSES.get_or_init(Mutex::default)
}

pub(super) struct Child {
    process: Option<Arc<ConfinedProcess>>,
    pid: u32,
    status: Option<ExitStatus>,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
}

pub(super) fn spawn_worker(binary: &Path, args: &[&str]) -> io::Result<Child> {
    let mut options = LaunchOptions::new(binary, basal_proto::FLOW_JOB_COMMIT_BYTES);
    options.args = args.iter().map(|arg| (*arg).into()).collect();
    let mut process = basal_launch::launch(&options).map_err(io::Error::other)?;
    let stdin = process.stdin.take();
    let stdout = process.stdout.take();
    let stderr = process.stderr.take();
    let process = Arc::new(process);
    let pid = process.id();
    processes().lock().unwrap().insert(
        pid,
        ReapState {
            process: Arc::downgrade(&process),
            signaled: false,
        },
    );
    Ok(Child {
        process: Some(process),
        pid,
        status: None,
        stdin,
        stdout,
        stderr,
    })
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn kill(&self) -> io::Result<()> {
        self.process
            .as_ref()
            .map_or(Ok(()), |process| process.kill())
    }

    fn release(&mut self, code: u32) -> ExitStatus {
        let process = self.process.take().expect("owned process before release");
        // Observe the real handle before releasing it, rather than treating a
        // cached exit code or the disappearance of a pid as evidence of exit.
        let signaled = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0;
        processes()
            .lock()
            .unwrap()
            .get_mut(&self.pid)
            .unwrap()
            .signaled = signaled;
        drop(process);
        let status = ExitStatus::from_raw(code);
        self.status = Some(status);
        status
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let code = self.process.as_ref().unwrap().try_wait()?;
        Ok(code.map(|code| self.release(code)))
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let code = self.process.as_ref().unwrap().wait()?;
        Ok(self.release(code))
    }
}

pub(super) fn assert_reaped(pid: u32) {
    let state = processes()
        .lock()
        .unwrap()
        .get(&pid)
        .map(|state| (state.process.clone(), state.signaled));
    let (process, signaled) = state.expect("process was launched by the test kit");
    assert!(signaled, "worker {pid}'s process handle was not signaled");
    assert!(
        process.upgrade().is_none(),
        "worker {pid}'s process handle is still owned"
    );
}

pub(super) fn rss_kib(pid: u32) -> Option<u64> {
    // SAFETY: OpenProcess returns a new owned handle, or NULL on failure.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&counters) as u32;
    counters.cb = size;
    // SAFETY: counters is writable for exactly the declared structure size.
    if unsafe { K32GetProcessMemoryInfo(handle.as_raw_handle(), &mut counters, size) } == 0 {
        return None;
    }
    Some(counters.WorkingSetSize as u64 / 1024)
}

pub fn terminate_process(pid: u32) -> io::Result<()> {
    // SAFETY: OpenProcess returns a fresh owned handle, or NULL on failure.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if unsafe { TerminateProcess(handle.as_raw_handle(), basal_launch::KILL_EXIT_CODE) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn output_until(
    command: &basal_launch::PlainCommand,
    timeout: std::time::Duration,
) -> io::Result<std::process::Output> {
    use std::io::Read;
    use std::time::{Duration, Instant};
    let mut child = basal_launch::spawn_plain(command).map_err(io::Error::other)?;
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let mut stderr = child.stderr.take().expect("stderr pipe");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(code)) => break Ok(ExitStatus::from_raw(code)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                timed_out = result.is_ok();
                child.kill()?;
                break child.wait().map(ExitStatus::from_raw);
            }
        }
    };
    // Even a normally exited parent can leave a descendant holding a pipe.
    // End the entire job before joining readers, never only the root process.
    child.kill()?;
    drop(child);
    let stdout = out
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = err
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "subprocess deadline expired; job killed and process released",
        ));
    }
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}
