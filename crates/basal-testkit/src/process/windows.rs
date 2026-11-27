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
/// a crashed worker printed as `exe+0x…` or `<system dll>+0x…` offsets.
/// Worker offsets use `binary`, the worker as Cargo built it, and the PDB
/// (debug symbols) its debug directory names; the confined worker cannot
/// read it, so the parent does this. Offsets into `ntdll.dll`,
/// `KERNEL32.DLL` and `KERNELBASE.dll` use this machine's copy of the DLL,
/// the same file the worker ran, and its PDB fetched from Microsoft's public
/// symbol server with `curl.exe`. Returns one line per frame, or lines
/// saying why a module did not resolve.
pub(super) fn symbolize_crash_report(binary: &Path, stderr: &str) -> String {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        IMAGEHLP_LINEW64, SYMBOL_INFOW, SYMOPT_FAIL_CRITICAL_ERRORS, SYMOPT_LOAD_LINES,
        SYMOPT_UNDNAME, SymCleanup, SymFromAddrW, SymGetLineFromAddrW64, SymInitializeW,
        SymLoadModuleExW, SymSetOptions,
    };

    const MARKER: &str = "basal-crash-report";
    const SYSTEM: [&str; 3] = ["ntdll.dll", "kernel32.dll", "kernelbase.dll"];
    // (label, module key: "exe" or a lower-case system DLL name, offset)
    let mut frames: Vec<(String, String, u64)> = Vec::new();
    for line in stderr.lines().filter(|line| line.starts_with(MARKER)) {
        let label = line[MARKER.len()..]
            .trim_start_matches(':')
            .trim()
            .to_owned();
        let lower = line.to_ascii_lowercase();
        for key in std::iter::once("exe").chain(SYSTEM) {
            let token = format!("{key}+0x");
            let Some(at) = lower.find(&token) else {
                continue;
            };
            // `exe+0x` must not match the tail of another name.
            if key == "exe" && at > 0 && !lower[..at].ends_with(' ') {
                continue;
            }
            let digits: String = lower[at + token.len()..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if let Ok(offset) = u64::from_str_radix(&digits, 16) {
                frames.push((label.clone(), key.to_owned(), offset));
                break;
            }
        }
    }
    if frames.is_empty() {
        return String::new();
    }
    // dbghelp is single-threaded; tests run on many threads.
    static DBGHELP: Mutex<()> = Mutex::new(());
    let _guard = DBGHELP.lock().unwrap_or_else(|poison| poison.into_inner());
    let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let mut out = String::new();
    // Each module's key, image path, PDB directory and image size (0: read
    // it from the image).
    let mut modules: Vec<(String, std::path::PathBuf, std::path::PathBuf, u32)> = Vec::new();
    modules.push((
        "exe".into(),
        binary.to_path_buf(),
        binary.parent().unwrap_or(Path::new(".")).to_path_buf(),
        0,
    ));
    for key in SYSTEM {
        if frames.iter().any(|(_, module, _)| module == key) {
            match system_pdb(key) {
                Ok(module) => {
                    out.push_str(&format!("\n  {key}: PDB {}", module.pdb));
                    modules.push((key.into(), module.image, module.directory, module.size));
                }
                Err(error) => out.push_str(&format!("\n  symbolization of {key}: {error}")),
            }
        }
    }
    const NAME_UNITS: usize = 512;
    // One private symbol session per module, so modules cannot collide.
    for (index, (key, image, directory, size)) in modules.iter().enumerate() {
        // Any unique value names a symbol session that does not inspect a
        // live process.
        let session = (0x5EED_0000_usize + index) as HANDLE;
        // SAFETY: plain dbghelp calls on a private session; every buffer
        // passed is live and sized as declared, and the session is cleaned
        // up below.
        unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_LOAD_LINES | SYMOPT_FAIL_CRITICAL_ERRORS);
            if SymInitializeW(session, wide(directory.as_os_str()).as_ptr(), 0) == 0 {
                out.push_str(&format!(
                    "\n  symbolization of {key}: SymInitializeW failed: {}",
                    io::Error::last_os_error()
                ));
                continue;
            }
            let base = SymLoadModuleExW(
                session,
                std::ptr::null_mut(),
                wide(image.as_os_str()).as_ptr(),
                std::ptr::null(),
                0x1_0000_0000,
                *size,
                std::ptr::null(),
                0,
            );
            if base == 0 {
                out.push_str(&format!(
                    "\n  symbolization: SymLoadModuleExW({}) failed: {}",
                    image.display(),
                    io::Error::last_os_error()
                ));
                SymCleanup(session);
                continue;
            }
            for (label, _, offset) in frames.iter().filter(|(_, module, _)| module == key) {
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
                out.push_str(&format!("\n  {label} => {key}!{name}{place}"));
            }
            SymCleanup(session);
        }
    }
    out
}

/// A System32 DLL, its image size, and the directory holding its PDB.
struct SystemModule {
    image: std::path::PathBuf,
    directory: std::path::PathBuf,
    size: u32,
    /// `<pdb name>/<key>`, the PDB's path on the symbol server, so the
    /// frames can also be symbolized elsewhere when loading fails here.
    pdb: String,
}

/// This machine's copy of a System32 DLL and its PDB, downloaded once per
/// process from Microsoft's public symbol server.
fn system_pdb(module: &str) -> Result<SystemModule, String> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let image = Path::new(&root).join("System32").join(module);
    let bytes = std::fs::read(&image).map_err(|e| format!("reading {}: {e}", image.display()))?;
    let (key, pdb) = codeview(&bytes).ok_or("no CodeView record in its debug directory")?;
    let size = image_size(&bytes).unwrap_or(0);
    let directory = std::env::temp_dir()
        .join("basal-crash-symbols")
        .join(format!("{module}-{key}"));
    let target = directory.join(&pdb);
    if !target.exists() {
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let url = format!("https://msdl.microsoft.com/download/symbols/{pdb}/{key}/{pdb}");
        let status = std::process::Command::new("curl.exe")
            .args(["-sSfL", "--max-time", "180", "-o"])
            .arg(&target)
            .arg(&url)
            .status()
            .map_err(|e| format!("running curl.exe: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&target);
            return Err(format!("PDB {pdb}/{key}: curl.exe {url}: {status}"));
        }
    }
    Ok(SystemModule {
        image,
        directory,
        size,
        pdb: format!("{pdb}/{key}"),
    })
}

/// `SizeOfImage` from a PE32+ optional header.
fn image_size(bytes: &[u8]) -> Option<u32> {
    let pe = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    let at = pe + 24 + 56;
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The symbol server key (GUID and age) and PDB file name from a PE image's
/// CodeView (`RSDS`) debug record.
fn codeview(bytes: &[u8]) -> Option<(String, String)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    let sections = u16_at(pe + 6)? as usize;
    let optional_size = u16_at(pe + 20)? as usize;
    let optional = pe + 24;
    // PE32+ data directories start 112 bytes in; the debug directory is entry 6.
    let debug_rva = u32_at(optional + 112 + 6 * 8)? as usize;
    let debug_size = u32_at(optional + 112 + 6 * 8 + 4)? as usize;
    let table = optional + optional_size;
    let file_offset = |rva: usize| {
        (0..sections).find_map(|index| {
            let section = table + index * 40;
            let virtual_size = u32_at(section + 8)? as usize;
            let virtual_address = u32_at(section + 12)? as usize;
            let raw = u32_at(section + 20)? as usize;
            (rva >= virtual_address && rva < virtual_address + virtual_size)
                .then(|| raw + rva - virtual_address)
        })
    };
    let directory = file_offset(debug_rva)?;
    for entry in (0..debug_size / 28).map(|index| directory + index * 28) {
        if u32_at(entry + 12)? != 2 {
            continue;
        }
        let data = u32_at(entry + 24)? as usize;
        if bytes.get(data..data + 4)? != b"RSDS" {
            continue;
        }
        let guid = bytes.get(data + 4..data + 20)?;
        let age = u32_at(data + 20)?;
        let name_bytes = bytes.get(data + 24..)?;
        let end = name_bytes.iter().position(|b| *b == 0)?;
        let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        let name = name.rsplit(['\\', '/']).next()?.to_owned();
        let key = format!(
            "{:08X}{:04X}{:04X}{}{age:X}",
            u32::from_le_bytes(guid[0..4].try_into().ok()?),
            u16::from_le_bytes(guid[4..6].try_into().ok()?),
            u16::from_le_bytes(guid[6..8].try_into().ok()?),
            guid[8..16]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>()
        );
        return Some((key, name));
    }
    None
}
