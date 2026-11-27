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
    output_until_inner(command, timeout, None, || Ok(true))
}

pub(crate) fn output_until_ready<F>(
    command: &basal_launch::PlainCommand,
    timeout: std::time::Duration,
    readiness_timeout: std::time::Duration,
    ready: F,
) -> io::Result<std::process::Output>
where
    F: FnMut() -> io::Result<bool>,
{
    output_until_inner(command, timeout, Some(readiness_timeout), ready)
}

fn output_until_inner<F>(
    command: &basal_launch::PlainCommand,
    timeout: std::time::Duration,
    readiness_timeout: Option<std::time::Duration>,
    mut ready: F,
) -> io::Result<std::process::Output>
where
    F: FnMut() -> io::Result<bool>,
{
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

    if let Some(readiness_timeout) = readiness_timeout {
        let readiness_deadline = Instant::now() + readiness_timeout;
        let readiness_error = loop {
            match ready() {
                Ok(true) if Instant::now() <= readiness_deadline => break None,
                Ok(false) if Instant::now() < readiness_deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(_) => {
                    break Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "readiness condition was not satisfied within {readiness_timeout:?}"
                        ),
                    ));
                }
                Err(error) => break Some(error),
            }
        };
        if let Some(error) = readiness_error {
            child.kill()?;
            child.wait()?;
            drop(child);
            out.join()
                .map_err(|_| io::Error::other("stdout reader panicked"))??;
            err.join()
                .map_err(|_| io::Error::other("stderr reader panicked"))??;
            return Err(error);
        }
    }

    // The output deadline starts only after the optional readiness condition holds.
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

/// TEMPORARY, with the worker's `crash_report` module: symbolizes the frames
/// a crashed worker printed as `exe+0x…` offsets, using `binary`, the
/// worker as Cargo built it, and the PDB its debug directory names. The
/// confined worker cannot read its PDB, so the parent does this. Returns one
/// line per resolved frame, or a line saying why nothing resolved.
pub(super) fn symbolize_crash_report(binary: &Path, stderr: &str) -> String {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        IMAGEHLP_LINEW64, SYMBOL_INFOW, SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_LOAD_LINES,
        SYMOPT_UNDNAME, SymCleanup, SymFromAddrW, SymGetLineFromAddrW64, SymInitializeW,
        SymLoadModuleExW, SymSetOptions,
    };

    const MARKER: &str = "basal-crash-report";
    let offsets: Vec<(String, u64)> = stderr
        .lines()
        .filter(|line| line.starts_with(MARKER))
        .filter_map(|line| {
            let start = line.find("exe+0x")? + "exe+0x".len();
            let digits: String = line[start..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            let offset = u64::from_str_radix(&digits, 16).ok()?;
            Some((
                line[MARKER.len()..]
                    .trim_start_matches(':')
                    .trim()
                    .to_owned(),
                offset,
            ))
        })
        .collect();
    if offsets.is_empty() {
        return String::new();
    }
    // dbghelp is single-threaded; tests run on many threads.
    static DBGHELP: Mutex<()> = Mutex::new(());
    let _guard = DBGHELP.lock().unwrap_or_else(|poison| poison.into_inner());
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<u16>>()
    };
    // Any unique value names a symbol session that does not inspect a live
    // process.
    let session = 0x5EED_usize as HANDLE;
    let image = wide(binary);
    let search = wide(binary.parent().unwrap_or(Path::new(".")));
    let base_hint = 0x1_4000_0000_u64;
    let mut out = String::new();
    // SAFETY: plain dbghelp calls on a private session; every buffer passed
    // is live and sized as declared, and the session is cleaned up below.
    unsafe {
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_LOAD_LINES | SYMOPT_FAIL_CRITICAL_ERRORS);
        if SymInitializeW(session, search.as_ptr(), 0) == 0 {
            return format!(
                "symbolization: SymInitializeW failed: {}",
                io::Error::last_os_error()
            );
        }
        let base = SymLoadModuleExW(
            session,
            std::ptr::null_mut(),
            image.as_ptr(),
            std::ptr::null(),
            base_hint,
            0,
            std::ptr::null(),
            0,
        );
        if base == 0 {
            out = format!(
                "symbolization: SymLoadModuleExW({}) failed: {}",
                binary.display(),
                io::Error::last_os_error()
            );
        } else {
            const NAME_UNITS: usize = 512;
            for (label, offset) in offsets {
                let address = base + offset;
                // SYMBOL_INFOW ends in a variable-length name.
                let mut buffer =
                    vec![0u64; (std::mem::size_of::<SYMBOL_INFOW>() + NAME_UNITS * 2) / 8 + 1];
                let symbol = buffer.as_mut_ptr().cast::<SYMBOL_INFOW>();
                (*symbol).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
                (*symbol).MaxNameLen = NAME_UNITS as u32;
                let mut displacement = 0u64;
                let name = if SymFromAddrW(session, address, &mut displacement, symbol) != 0 {
                    let units = std::slice::from_raw_parts(
                        std::ptr::addr_of!((*symbol).Name).cast::<u16>(),
                        ((*symbol).NameLen as usize).min(NAME_UNITS),
                    );
                    format!("{}+{displacement:#x}", String::from_utf16_lossy(units))
                } else {
                    format!("<no symbol: {}>", io::Error::last_os_error())
                };
                let mut line: IMAGEHLP_LINEW64 = std::mem::zeroed();
                line.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINEW64>() as u32;
                let mut column = 0u32;
                let place = if SymGetLineFromAddrW64(session, address, &mut column, &mut line) != 0
                {
                    let mut length = 0;
                    while *line.FileName.add(length) != 0 {
                        length += 1;
                    }
                    let file =
                        String::from_utf16_lossy(std::slice::from_raw_parts(line.FileName, length));
                    format!(" at {file}:{}", line.LineNumber)
                } else {
                    String::new()
                };
                out.push_str(&format!("\n  {label} => {name}{place}"));
            }
        }
        SymCleanup(session);
    }
    out
}
