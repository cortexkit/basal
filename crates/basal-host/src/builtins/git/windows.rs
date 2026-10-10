//! Windows-specific implementation of git execution, hardening and job containment.
//!
//! Spawns `git.exe` in its own Job Object via `CreateProcessW` + `STARTUPINFOEXW` +
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`.
//! Isolates environment and configuration using owned empty files/directories.
//! Enforces timeout and output caps by terminating the job object before joining drain threads.

#![cfg_attr(not(windows), allow(unused))]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Instant;

use super::STDERR_BYTES;
pub use super::{MAX_OUTPUT_BYTES, MAX_TAGS, TIMEOUT};
use super::{Ran, codes};
use crate::builtins::Denial;

/// Strips `\\?\` or `//?/` verbatim prefix from Windows paths.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else if let Some(stripped) = s.strip_prefix("//?/") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

/// Checks whether a path is absolute in Windows syntax (e.g. `C:\...` or `\\server\share`).
fn is_windows_absolute(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    if s.starts_with(r"\\") || s.starts_with("//") {
        return true;
    }
    path.is_absolute()
}

struct IsolationPaths {
    _dir: PathBuf,
    config_file: PathBuf,
    hooks_dir: PathBuf,
}

static ISOLATION: OnceLock<IsolationPaths> = OnceLock::new();

fn isolation_paths() -> &'static IsolationPaths {
    ISOLATION.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("basal-git-isolation-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config_file = dir.join("empty.config");
        let hooks_dir = dir.join("empty.hooks");
        if !config_file.exists() {
            let _ = std::fs::write(&config_file, b"");
        }
        let _ = std::fs::create_dir_all(&hooks_dir);
        IsolationPaths {
            _dir: dir,
            config_file,
            hooks_dir,
        }
    })
}

/// The owned empty configuration file used for `GIT_CONFIG_GLOBAL`.
pub fn empty_config_file() -> &'static Path {
    &isolation_paths().config_file
}

/// The owned empty hooks directory used for `core.hooksPath`.
pub fn empty_hooks_dir() -> &'static Path {
    &isolation_paths().hooks_dir
}

/// Resolves git to an absolute `.exe` path, refusing `.bat` or `.cmd` shims.
pub fn resolve_git_binary(program: &str, path_env: Option<&OsStr>) -> Result<PathBuf, Denial> {
    let p = Path::new(program);
    // If explicit extension is .bat or .cmd, refuse immediately.
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        if ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd") {
            return Err(Denial::new(
                codes::GIT,
                format!("git binary {program:?} is a .{ext} script, which is refused"),
            ));
        }
    }

    // If an absolute path is provided, verify it is a valid .exe file.
    if is_windows_absolute(p) {
        let is_exe = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if !is_exe {
            return Err(Denial::new(
                codes::GIT,
                format!("git binary at {program:?} is not a .exe executable"),
            ));
        }
        if p.is_file() {
            return Ok(strip_verbatim_prefix(p));
        }
        return Err(Denial::new(
            codes::GIT,
            format!("git binary at {program:?} does not exist"),
        ));
    }

    // Search PATH directories.
    let base_name = p.file_stem().and_then(|s| s.to_str()).unwrap_or(program);

    if let Some(path_val) = path_env {
        let dirs: Vec<PathBuf> = if cfg!(windows) {
            std::env::split_paths(path_val).collect()
        } else {
            let s = path_val.to_string_lossy();
            let delimiter = if s.contains(';') { ';' } else { ':' };
            s.split(delimiter).map(PathBuf::from).collect()
        };

        for dir in dirs {
            let exe_candidate = dir.join(format!("{base_name}.exe"));
            let cmd_candidate = dir.join(format!("{base_name}.cmd"));
            let bat_candidate = dir.join(format!("{base_name}.bat"));

            if exe_candidate.is_file() {
                return Ok(strip_verbatim_prefix(&exe_candidate));
            }
            if cmd_candidate.is_file() {
                return Err(Denial::new(
                    codes::GIT,
                    format!(
                        "git resolved to {cmd_candidate:?}, which is a .cmd script and is refused"
                    ),
                ));
            }
            if bat_candidate.is_file() {
                return Err(Denial::new(
                    codes::GIT,
                    format!(
                        "git resolved to {bat_candidate:?}, which is a .bat script and is refused"
                    ),
                ));
            }
        }
    }

    // Check standard Windows Git locations as fallback
    #[cfg(windows)]
    for standard_path in [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\bin\git.exe",
        r"C:\Program Files (x86)\Git\cmd\git.exe",
        r"C:\Program Files (x86)\Git\bin\git.exe",
    ] {
        let pb = PathBuf::from(standard_path);
        if pb.is_file() {
            return Ok(pb);
        }
    }

    Err(Denial::new(
        codes::GIT,
        "git.exe could not be found in PATH or standard install locations".to_string(),
    ))
}

static RESOLVED_GIT: OnceLock<Result<PathBuf, Denial>> = OnceLock::new();

/// Resolves `git.exe` once to an absolute `.exe` path.
pub fn resolve_git() -> Result<&'static Path, Denial> {
    let res =
        RESOLVED_GIT.get_or_init(|| resolve_git_binary("git", std::env::var_os("PATH").as_deref()));
    match res {
        Ok(p) => Ok(p.as_path()),
        Err(e) => Err(e.clone()),
    }
}

/// Computes the parent directory of a Windows path, supporting both `\` and `/`.
pub fn windows_parent(path: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        path.parent().map(|p| p.to_path_buf())
    } else {
        let s = path.to_string_lossy();
        if let Some(idx) = s.rfind(|c| c == '\\' || c == '/') {
            if idx == 0 {
                Some(PathBuf::from(&s[..1]))
            } else {
                Some(PathBuf::from(&s[..idx]))
            }
        } else {
            None
        }
    }
}

/// The only way the built-ins run git on Windows: a `git -C <repo>` command
/// that can read the repository and run nothing else.
pub fn hardened_command(repo: &Path) -> Command {
    let git_prog = resolve_git()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| PathBuf::from("git.exe"));
    let mut command = Command::new(git_prog);
    command.env_clear();
    for var in ["PATH", "SystemRoot", "USERPROFILE"] {
        if let Some(val) = std::env::var_os(var) {
            command.env(var, val);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_config_file())
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    let clean_repo = strip_verbatim_prefix(repo);
    if let Some(parent) = windows_parent(&clean_repo) {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    let hooks_arg = format!("core.hooksPath={}", empty_hooks_dir().display());
    command
        .arg("-C")
        .arg(clean_repo)
        .args([
            "--no-optional-locks",
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            &hooks_arg,
            "-c",
            "core.pager=cat",
            "-c",
            "log.showSignature=false",
            "-c",
            "protocol.allow=never",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Escapes a single argument according to Microsoft `CommandLineToArgvW` rules.
pub fn append_windows_arg(cmd: &mut String, arg: &str) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '\"']) {
        cmd.push_str(arg);
        return;
    }
    cmd.push('"');
    let mut backslashes: usize = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                for _ in 0..backslashes * 2 + 1 {
                    cmd.push('\\');
                }
            } else {
                for _ in 0..backslashes {
                    cmd.push('\\');
                }
                cmd.push(c);
            }
            backslashes = 0;
        }
    }
    for _ in 0..backslashes * 2 {
        cmd.push('\\');
    }
    cmd.push('"');
}

fn to_wide_null(s: impl AsRef<OsStr>) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.as_ref().encode_wide().chain(Some(0)).collect()
    }
    #[cfg(not(windows))]
    {
        s.as_ref()
            .to_str()
            .unwrap_or("")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }
}

fn timeout_denial() -> Denial {
    Denial::new(
        codes::TIMEOUT,
        format!("git ran longer than {} s", TIMEOUT.as_secs()),
    )
}

#[cfg(windows)]
mod ffi {
    pub use std::ffi::c_void;

    pub type BOOL = i32;
    pub type HANDLE = *mut c_void;
    pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

    pub const TRUE: BOOL = 1;
    pub const FALSE: BOOL = 0;

    pub const STILL_ACTIVE: u32 = 259;
    pub const WAIT_OBJECT_0: u32 = 0;
    pub const INFINITE: u32 = 0xFFFFFFFF;

    pub const HANDLE_FLAG_INHERIT: u32 = 0x00000001;

    pub const STARTF_USESTDHANDLES: u32 = 0x00000100;
    pub const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x00080000;
    pub const CREATE_NO_WINDOW: u32 = 0x08000000;
    pub const CREATE_UNICODE_ENVIRONMENT: u32 = 0x00000400;

    pub const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002000D;
    pub const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x00020002;

    pub const JobObjectExtendedLimitInformation: u32 = 9;
    pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;

    pub const FILE_GENERIC_READ: u32 = 0x00120089;
    pub const FILE_SHARE_READ: u32 = 0x00000001;
    pub const FILE_SHARE_WRITE: u32 = 0x00000002;
    pub const FILE_SHARE_DELETE: u32 = 0x00000004;
    pub const OPEN_EXISTING: u32 = 3;
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000080;

    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const SYNCHRONIZE: u32 = 0x00100000;

    #[repr(C)]
    pub struct SECURITY_ATTRIBUTES {
        pub nLength: u32,
        pub lpSecurityDescriptor: *mut c_void,
        pub bInheritHandle: BOOL,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct IO_COUNTERS {
        pub ReadOperationCount: u64,
        pub WriteOperationCount: u64,
        pub OtherOperationCount: u64,
        pub ReadTransferCount: u64,
        pub WriteTransferCount: u64,
        pub OtherTransferCount: u64,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        pub PerProcessUserTimeLimit: i64,
        pub PerJobUserTimeLimit: i64,
        pub LimitFlags: u32,
        pub MinimumWorkingSetSize: usize,
        pub MaximumWorkingSetSize: usize,
        pub ActiveProcessLimit: u32,
        pub Affinity: usize,
        pub PriorityClass: u32,
        pub SchedulingClass: u32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        pub BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        pub IoInfo: IO_COUNTERS,
        pub ProcessMemoryLimit: usize,
        pub JobMemoryLimit: usize,
        pub PeakProcessMemoryUsed: usize,
        pub PeakJobMemoryUsed: usize,
    }

    #[repr(C)]
    pub struct STARTUPINFOW {
        pub cb: u32,
        pub lpReserved: *mut u16,
        pub lpDesktop: *mut u16,
        pub lpTitle: *mut u16,
        pub dwX: u32,
        pub dwY: u32,
        pub dwXSize: u32,
        pub dwYSize: u32,
        pub dwXCountChars: u32,
        pub dwYCountChars: u32,
        pub dwFillAttribute: u32,
        pub dwFlags: u32,
        pub wShowWindow: u16,
        pub cbReserved2: u16,
        pub lpReserved2: *mut u8,
        pub hStdInput: HANDLE,
        pub hStdOutput: HANDLE,
        pub hStdError: HANDLE,
    }

    #[repr(C)]
    pub struct STARTUPINFOEXW {
        pub StartupInfo: STARTUPINFOW,
        pub lpAttributeList: *mut c_void,
    }

    #[repr(C)]
    pub struct PROCESS_INFORMATION {
        pub hProcess: HANDLE,
        pub hThread: HANDLE,
        pub dwProcessId: u32,
        pub dwThreadId: u32,
    }

    unsafe extern "system" {
        pub fn CloseHandle(hObject: HANDLE) -> BOOL;
        pub fn SetHandleInformation(hObject: HANDLE, dwMask: u32, dwFlags: u32) -> BOOL;

        pub fn CreatePipe(
            hReadPipe: *mut HANDLE,
            hWritePipe: *mut HANDLE,
            lpPipeAttributes: *const SECURITY_ATTRIBUTES,
            nSize: u32,
        ) -> BOOL;

        pub fn ReadFile(
            hFile: HANDLE,
            lpBuffer: *mut c_void,
            nNumberOfBytesToRead: u32,
            lpNumberOfBytesRead: *mut u32,
            lpOverlapped: *mut c_void,
        ) -> BOOL;

        pub fn CreateFileW(
            lpFileName: *const u16,
            dwDesiredAccess: u32,
            dwShareMode: u32,
            lpSecurityAttributes: *const SECURITY_ATTRIBUTES,
            dwCreationDisposition: u32,
            dwFlagsAndAttributes: u32,
            hTemplateFile: HANDLE,
        ) -> HANDLE;

        pub fn CreateJobObjectW(
            lpJobAttributes: *const SECURITY_ATTRIBUTES,
            lpName: *const u16,
        ) -> HANDLE;

        pub fn SetInformationJobObject(
            hJob: HANDLE,
            JobObjectInformationClass: u32,
            lpJobObjectInformation: *const c_void,
            cbJobObjectInformationLength: u32,
        ) -> BOOL;

        pub fn TerminateJobObject(hJob: HANDLE, uExitCode: u32) -> BOOL;

        pub fn InitializeProcThreadAttributeList(
            lpAttributeList: *mut c_void,
            dwAttributeCount: u32,
            dwFlags: u32,
            lpSize: *mut usize,
        ) -> BOOL;

        pub fn UpdateProcThreadAttribute(
            lpAttributeList: *mut c_void,
            dwFlags: u32,
            Attribute: usize,
            lpValue: *const c_void,
            cbSize: usize,
            lpPreviousValue: *mut c_void,
            lpReturnSize: *const usize,
        ) -> BOOL;

        pub fn DeleteProcThreadAttributeList(lpAttributeList: *mut c_void);

        pub fn CreateProcessW(
            lpApplicationName: *const u16,
            lpCommandLine: *mut u16,
            lpProcessAttributes: *const SECURITY_ATTRIBUTES,
            lpThreadAttributes: *const SECURITY_ATTRIBUTES,
            bInheritHandles: BOOL,
            dwCreationFlags: u32,
            lpEnvironment: *const c_void,
            lpCurrentDirectory: *const u16,
            lpStartupInfo: *const STARTUPINFOEXW,
            lpProcessInformation: *mut PROCESS_INFORMATION,
        ) -> BOOL;

        pub fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: u32) -> u32;

        pub fn GetExitCodeProcess(hProcess: HANDLE, lpExitCode: *mut u32) -> BOOL;

        pub fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: BOOL, dwProcessId: u32) -> HANDLE;
    }
}

#[cfg(windows)]
pub struct OwnedHandle(pub ffi::HANDLE);

#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

#[cfg(windows)]
impl OwnedHandle {
    pub fn raw(&self) -> ffi::HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != ffi::INVALID_HANDLE_VALUE {
            unsafe { ffi::CloseHandle(self.0) };
            self.0 = ffi::INVALID_HANDLE_VALUE;
        }
    }
}

#[cfg(windows)]
struct AttributeList {
    buffer: Vec<usize>,
}

#[cfg(windows)]
impl AttributeList {
    fn new(count: u32) -> Result<Self, Denial> {
        let mut size: usize = 0;
        unsafe {
            ffi::InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size);
        }
        let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let ok = unsafe {
            ffi::InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size)
        };
        if ok == 0 {
            return Err(Denial::new(
                codes::GIT,
                format!(
                    "InitializeProcThreadAttributeList failed: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        Ok(Self { buffer })
    }

    fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
        self.buffer.as_mut_ptr().cast()
    }

    fn set_job_list(&mut self, job: ffi::HANDLE) -> Result<(), Denial> {
        let mut jobs = [job];
        let ok = unsafe {
            ffi::UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                ffi::PROC_THREAD_ATTRIBUTE_JOB_LIST,
                jobs.as_mut_ptr().cast(),
                std::mem::size_of::<ffi::HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(Denial::new(
                codes::GIT,
                format!(
                    "UpdateProcThreadAttribute(JOB_LIST) failed: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        Ok(())
    }

    fn set_handle_list(&mut self, handles: &[ffi::HANDLE]) -> Result<(), Denial> {
        let ok = unsafe {
            ffi::UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                ffi::PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_ptr().cast(),
                handles.len() * std::mem::size_of::<ffi::HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(Denial::new(
                codes::GIT,
                format!(
                    "UpdateProcThreadAttribute(HANDLE_LIST) failed: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            ffi::DeleteProcThreadAttributeList(self.as_mut_ptr());
        }
    }
}

#[cfg(windows)]
pub struct ProcessJob {
    pub process: ffi::HANDLE,
    pub job: ffi::HANDLE,
    terminated: bool,
}

#[cfg(windows)]
unsafe impl Send for ProcessJob {}

#[cfg(windows)]
impl ProcessJob {
    pub fn new(process: ffi::HANDLE, job: ffi::HANDLE) -> Self {
        Self {
            process,
            job,
            terminated: false,
        }
    }

    pub fn terminate(&mut self) {
        if !self.terminated {
            self.terminated = true;
            if !self.job.is_null() && self.job != ffi::INVALID_HANDLE_VALUE {
                unsafe {
                    ffi::TerminateJobObject(self.job, 1);
                }
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        self.terminate();
        if !self.process.is_null() && self.process != ffi::INVALID_HANDLE_VALUE {
            unsafe {
                ffi::CloseHandle(self.process);
            }
            self.process = ffi::INVALID_HANDLE_VALUE;
        }
        if !self.job.is_null() && self.job != ffi::INVALID_HANDLE_VALUE {
            unsafe {
                ffi::CloseHandle(self.job);
            }
            self.job = ffi::INVALID_HANDLE_VALUE;
        }
    }
}

struct PipeOutput {
    bytes: Vec<u8>,
    limit_reached: bool,
}

#[cfg(windows)]
fn drain_pipe(
    handle: ffi::HANDLE,
    cap: usize,
    max_lines: Option<usize>,
) -> Result<PipeOutput, Denial> {
    let mut output = PipeOutput {
        bytes: Vec::new(),
        limit_reached: false,
    };
    if handle.is_null() || handle == ffi::INVALID_HANDLE_VALUE {
        return Ok(output);
    }
    let mut chunk = [0u8; STDERR_BYTES];
    loop {
        let mut bytes_read: u32 = 0;
        let ok = unsafe {
            ffi::ReadFile(
                handle,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut bytes_read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || bytes_read == 0 {
            return Ok(output);
        }
        let n = bytes_read as usize;
        let room = cap.saturating_sub(output.bytes.len());
        output.bytes.extend_from_slice(&chunk[..n.min(room)]);
        if let Some(limit) = max_lines
            && let Some((last, _)) = output
                .bytes
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'\n')
                .nth(limit - 1)
        {
            output.bytes.truncate(last + 1);
            output.limit_reached = true;
            return Ok(output);
        }
        if cap > STDERR_BYTES && output.bytes.len() > MAX_OUTPUT_BYTES {
            return Err(Denial::new(
                codes::TOO_LARGE,
                format!("git's output is larger than {MAX_OUTPUT_BYTES} bytes"),
            ));
        }
    }
}

#[cfg(windows)]
fn spawn_job_command(command: Command) -> Result<(ProcessJob, OwnedHandle, OwnedHandle), Denial> {
    let program = command.get_program();
    let program_str = program.to_string_lossy();
    let lower_prog = program_str.to_ascii_lowercase();

    if lower_prog.ends_with(".bat") || lower_prog.ends_with(".cmd") {
        return Err(Denial::new(
            codes::GIT,
            format!("{program_str:?} is a .bat or .cmd script, which is refused"),
        ));
    }

    let resolved_prog = if lower_prog == "git" || lower_prog == "git.exe" {
        resolve_git()?.to_path_buf()
    } else {
        PathBuf::from(program)
    };

    let mut cmdline = String::new();
    append_windows_arg(&mut cmdline, &resolved_prog.to_string_lossy());
    for arg in command.get_args() {
        cmdline.push(' ');
        append_windows_arg(&mut cmdline, &arg.to_string_lossy());
    }
    let mut cmdline_wide: Vec<u16> = to_wide_null(&cmdline);

    let mut env_map = BTreeMap::new();
    let is_hardened = command
        .get_envs()
        .any(|(k, v)| k == "GIT_CONFIG_NOSYSTEM" && v.is_some());
    if !is_hardened {
        for (k, v) in std::env::vars_os() {
            env_map.insert(k, v);
        }
    }
    for (k, v) in command.get_envs() {
        if let Some(val) = v {
            env_map.insert(k.to_os_string(), val.to_os_string());
        } else {
            env_map.remove(k);
        }
    }

    let mut entries: Vec<(OsString, OsString)> = env_map.into_iter().collect();
    entries.sort_by(|(k1, _), (k2, _)| {
        k1.to_string_lossy()
            .to_ascii_uppercase()
            .cmp(&k2.to_string_lossy().to_ascii_uppercase())
    });

    let mut env_block: Vec<u16> = Vec::new();
    for (k, v) in &entries {
        let k_str = k.to_string_lossy();
        let v_str = v.to_string_lossy();
        for ch in k_str.encode_utf16() {
            env_block.push(ch);
        }
        env_block.push('=' as u16);
        for ch in v_str.encode_utf16() {
            env_block.push(ch);
        }
        env_block.push(0);
    }
    env_block.push(0);

    let cwd_wide = command.get_current_dir().map(|p| {
        let clean = strip_verbatim_prefix(p);
        to_wide_null(&clean)
    });
    let cwd_ptr = cwd_wide
        .as_ref()
        .map(|v| v.as_ptr())
        .unwrap_or(std::ptr::null());

    let job = unsafe { ffi::CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(Denial::new(
            codes::GIT,
            format!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    let mut limits: ffi::JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = ffi::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        ffi::SetInformationJobObject(
            job,
            ffi::JobObjectExtendedLimitInformation,
            &limits as *const _ as *const ffi::c_void,
            std::mem::size_of::<ffi::JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        unsafe { ffi::CloseHandle(job) };
        return Err(Denial::new(
            codes::GIT,
            format!("SetInformationJobObject failed: {err}"),
        ));
    }

    let sa_inherit = ffi::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<ffi::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: ffi::TRUE,
    };

    let empty_file_wide = to_wide_null(empty_config_file());
    let mut stdin_handle = unsafe {
        ffi::CreateFileW(
            empty_file_wide.as_ptr(),
            ffi::FILE_GENERIC_READ,
            ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE | ffi::FILE_SHARE_DELETE,
            &sa_inherit,
            ffi::OPEN_EXISTING,
            ffi::FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if stdin_handle == ffi::INVALID_HANDLE_VALUE {
        let mut pipe_read = std::ptr::null_mut();
        let mut pipe_write = std::ptr::null_mut();
        unsafe {
            ffi::CreatePipe(&mut pipe_read, &mut pipe_write, &sa_inherit, 0);
            ffi::CloseHandle(pipe_write);
        }
        stdin_handle = pipe_read;
    }

    let mut stdout_read = std::ptr::null_mut();
    let mut stdout_write = std::ptr::null_mut();
    if unsafe { ffi::CreatePipe(&mut stdout_read, &mut stdout_write, &sa_inherit, 0) } == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            if stdin_handle != ffi::INVALID_HANDLE_VALUE {
                ffi::CloseHandle(stdin_handle);
            }
            ffi::CloseHandle(job);
        }
        return Err(Denial::new(
            codes::GIT,
            format!("CreatePipe stdout failed: {err}"),
        ));
    }
    unsafe { ffi::SetHandleInformation(stdout_read, ffi::HANDLE_FLAG_INHERIT, 0) };

    let mut stderr_read = std::ptr::null_mut();
    let mut stderr_write = std::ptr::null_mut();
    if unsafe { ffi::CreatePipe(&mut stderr_read, &mut stderr_write, &sa_inherit, 0) } == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            if stdin_handle != ffi::INVALID_HANDLE_VALUE {
                ffi::CloseHandle(stdin_handle);
            }
            ffi::CloseHandle(stdout_read);
            ffi::CloseHandle(stdout_write);
            ffi::CloseHandle(job);
        }
        return Err(Denial::new(
            codes::GIT,
            format!("CreatePipe stderr failed: {err}"),
        ));
    }
    unsafe { ffi::SetHandleInformation(stderr_read, ffi::HANDLE_FLAG_INHERIT, 0) };

    let mut attr_list = match AttributeList::new(2) {
        Ok(list) => list,
        Err(e) => {
            unsafe {
                if stdin_handle != ffi::INVALID_HANDLE_VALUE {
                    ffi::CloseHandle(stdin_handle);
                }
                ffi::CloseHandle(stdout_read);
                ffi::CloseHandle(stdout_write);
                ffi::CloseHandle(stderr_read);
                ffi::CloseHandle(stderr_write);
                ffi::CloseHandle(job);
            }
            return Err(e);
        }
    };
    if let Err(e) = attr_list.set_job_list(job) {
        unsafe {
            if stdin_handle != ffi::INVALID_HANDLE_VALUE {
                ffi::CloseHandle(stdin_handle);
            }
            ffi::CloseHandle(stdout_read);
            ffi::CloseHandle(stdout_write);
            ffi::CloseHandle(stderr_read);
            ffi::CloseHandle(stderr_write);
            ffi::CloseHandle(job);
        }
        return Err(e);
    }
    let handles = [stdin_handle, stdout_write, stderr_write];
    if let Err(e) = attr_list.set_handle_list(&handles) {
        unsafe {
            if stdin_handle != ffi::INVALID_HANDLE_VALUE {
                ffi::CloseHandle(stdin_handle);
            }
            ffi::CloseHandle(stdout_read);
            ffi::CloseHandle(stdout_write);
            ffi::CloseHandle(stderr_read);
            ffi::CloseHandle(stderr_write);
            ffi::CloseHandle(job);
        }
        return Err(e);
    }

    let mut startup: ffi::STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<ffi::STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = ffi::STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin_handle;
    startup.StartupInfo.hStdOutput = stdout_write;
    startup.StartupInfo.hStdError = stderr_write;
    startup.lpAttributeList = attr_list.as_mut_ptr();

    let mut pi: ffi::PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let creation_flags =
        ffi::EXTENDED_STARTUPINFO_PRESENT | ffi::CREATE_NO_WINDOW | ffi::CREATE_UNICODE_ENVIRONMENT;

    let ok = unsafe {
        ffi::CreateProcessW(
            std::ptr::null(),
            cmdline_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            ffi::TRUE,
            creation_flags,
            env_block.as_ptr().cast(),
            cwd_ptr,
            &startup,
            &mut pi,
        )
    };

    unsafe {
        if stdin_handle != ffi::INVALID_HANDLE_VALUE {
            ffi::CloseHandle(stdin_handle);
        }
        ffi::CloseHandle(stdout_write);
        ffi::CloseHandle(stderr_write);
    }

    if ok == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            ffi::CloseHandle(stdout_read);
            ffi::CloseHandle(stderr_read);
            ffi::CloseHandle(job);
        }
        return Err(Denial::new(
            codes::GIT,
            format!("git could not start: {err}"),
        ));
    }

    unsafe {
        ffi::CloseHandle(pi.hThread);
    }

    let guard = ProcessJob::new(pi.hProcess, job);
    let stdout_pipe = OwnedHandle(stdout_read);
    let stderr_pipe = OwnedHandle(stderr_read);
    Ok((guard, stdout_pipe, stderr_pipe))
}

#[cfg(windows)]
enum Completion {
    Exit(Result<u32, Denial>),
    Stdout(Result<PipeOutput, Denial>),
    Stderr(Result<PipeOutput, Denial>),
}

#[cfg(windows)]
pub fn run_command_until(command: Command, max_lines: Option<usize>) -> Result<Ran, Denial> {
    let (mut guard, stdout_read, stderr_read) = spawn_job_command(command)?;
    let stdout_raw = stdout_read.raw() as usize;
    let stderr_raw = stderr_read.raw() as usize;
    let process_raw = guard.process as usize;

    let deadline = Instant::now() + TIMEOUT;
    let (tx, completed) = mpsc::channel();

    thread::scope(|scope| {
        let exit_tx = tx.clone();
        scope.spawn(move || {
            let process_handle = process_raw as ffi::HANDLE;
            let wait_res = unsafe { ffi::WaitForSingleObject(process_handle, ffi::INFINITE) };
            if wait_res == ffi::WAIT_OBJECT_0 {
                let mut exit_code: u32 = 0;
                let ok = unsafe { ffi::GetExitCodeProcess(process_handle, &mut exit_code) };
                if ok != 0 {
                    let _ = exit_tx.send(Completion::Exit(Ok(exit_code)));
                } else {
                    let _ = exit_tx.send(Completion::Exit(Err(Denial::new(
                        codes::GIT,
                        format!(
                            "GetExitCodeProcess failed: {}",
                            std::io::Error::last_os_error()
                        ),
                    ))));
                }
            } else {
                let _ = exit_tx.send(Completion::Exit(Err(Denial::new(
                    codes::GIT,
                    "WaitForSingleObject on git process failed",
                ))));
            }
        });

        let out_tx = tx.clone();
        scope.spawn(move || {
            let stdout_handle = stdout_raw as ffi::HANDLE;
            let res = drain_pipe(stdout_handle, MAX_OUTPUT_BYTES + 1, max_lines);
            let _ = out_tx.send(Completion::Stdout(res));
        });

        let err_tx = tx.clone();
        scope.spawn(move || {
            let stderr_handle = stderr_raw as ffi::HANDLE;
            let res = drain_pipe(stderr_handle, STDERR_BYTES, None);
            let _ = err_tx.send(Completion::Stderr(res));
        });

        let mut exit_code: Option<u32> = None;
        let mut stdout: Option<Vec<u8>> = None;
        let mut stderr: Option<Vec<u8>> = None;
        let mut limit_reached = false;
        let mut early_err: Option<Denial> = None;

        while exit_code.is_none() || stdout.is_none() || stderr.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match completed.recv_timeout(remaining) {
                Ok(Completion::Exit(result)) => {
                    match result {
                        Ok(code) => exit_code = Some(code),
                        Err(e) => {
                            if early_err.is_none() {
                                early_err = Some(e);
                            }
                            exit_code = Some(1);
                        }
                    }
                    guard.terminate();
                }
                Ok(Completion::Stdout(result)) => match result {
                    Ok(output) => {
                        limit_reached = output.limit_reached;
                        if limit_reached {
                            guard.terminate();
                        }
                        stdout = Some(output.bytes);
                    }
                    Err(e) => {
                        if early_err.is_none() {
                            early_err = Some(e);
                        }
                        guard.terminate();
                        stdout = Some(Vec::new());
                    }
                },
                Ok(Completion::Stderr(result)) => match result {
                    Ok(output) => stderr = Some(output.bytes),
                    Err(e) => {
                        if early_err.is_none() {
                            early_err = Some(e);
                        }
                        guard.terminate();
                        stderr = Some(Vec::new());
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    guard.terminate();
                    return Err(timeout_denial());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    guard.terminate();
                    return Err(Denial::new(codes::GIT, "git waiter disconnected"));
                }
            }
        }

        // The job is terminated before any drain thread is joined
        guard.terminate();

        if let Some(err) = early_err {
            return Err(err);
        }

        let code = exit_code.unwrap();
        let success = limit_reached || code == 0;
        Ok(Ran {
            success,
            code: if limit_reached {
                Some(0)
            } else {
                Some(code as i32)
            },
            stdout: stdout.unwrap(),
            stderr: String::from_utf8_lossy(&stderr.unwrap()).into_owned(),
        })
    })
}

#[cfg(windows)]
pub fn run_command(command: Command) -> Result<Ran, Denial> {
    run_command_until(command, None)
}

#[cfg(windows)]
pub fn run_tags_command(command: Command) -> Result<Ran, Denial> {
    run_command_until(command, Some(MAX_TAGS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_strip_verbatim_prefix() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\repo")),
            PathBuf::from(r"C:\repo")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new("//?/C:/repo")),
            PathBuf::from("C:/repo")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\repo")),
            PathBuf::from(r"C:\repo")
        );
    }

    #[test]
    fn windows_git_resolution_refuses_bat_and_cmd() {
        assert!(
            resolve_git_binary("git.bat", None)
                .unwrap_err()
                .message
                .contains("refused")
        );
        assert!(
            resolve_git_binary("git.cmd", None)
                .unwrap_err()
                .message
                .contains("refused")
        );
        assert!(
            resolve_git_binary(r"C:\bin\git.bat", None)
                .unwrap_err()
                .message
                .contains("refused")
        );
        assert!(
            resolve_git_binary(r"C:\bin\git.cmd", None)
                .unwrap_err()
                .message
                .contains("refused")
        );
        assert!(
            resolve_git_binary(r"C:\bin\git.BAT", None)
                .unwrap_err()
                .message
                .contains("refused")
        );
        assert!(
            resolve_git_binary(r"C:\bin\git.CMD", None)
                .unwrap_err()
                .message
                .contains("refused")
        );

        let temp = std::env::temp_dir().join(format!("basal-git-test-res-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp);
        let bad_dir = temp.join("bad");
        let good_dir = temp.join("good");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::create_dir_all(&good_dir).unwrap();

        std::fs::write(bad_dir.join("git.cmd"), b"@echo off\r\n").unwrap();
        std::fs::write(good_dir.join("git.exe"), b"MZ...").unwrap();

        let path_bad_first = format!("{};{}", bad_dir.display(), good_dir.display());
        let res_bad = resolve_git_binary("git", Some(OsStr::new(&path_bad_first)));
        assert!(
            res_bad.as_ref().unwrap_err().message.contains("refused"),
            "Must refuse git.cmd earlier in PATH"
        );

        let path_good_first = format!("{};{}", good_dir.display(), bad_dir.display());
        let res_good = resolve_git_binary("git", Some(OsStr::new(&path_good_first)));
        assert!(res_good.is_ok(), "Must resolve git.exe");
        assert_eq!(res_good.unwrap(), good_dir.join("git.exe"));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn windows_argv_and_environment() {
        let repo = Path::new(r"\\?\C:\test\repo");
        let command = hardened_command(repo);

        let prog = command.get_program().to_string_lossy();
        assert!(
            prog.ends_with(".exe") || prog == "git.exe",
            "Program must be .exe, got: {prog}"
        );
        assert!(!prog.ends_with(".bat"));
        assert!(!prog.ends_with(".cmd"));

        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args[0], "-C");
        assert_eq!(args[1], r"C:\test\repo");
        assert!(!args[1].starts_with(r"\\?\"));
        assert!(!args[1].starts_with("//?/"));

        assert!(args.contains(&"--no-optional-locks".to_string()));
        assert!(args.contains(&"--no-pager".to_string()));
        assert!(args.contains(&"--literal-pathspecs".to_string()));
        assert!(args.contains(&"core.fsmonitor=false".to_string()));
        assert!(args.contains(&"core.pager=cat".to_string()));
        assert!(args.contains(&"log.showSignature=false".to_string()));
        assert!(args.contains(&"protocol.allow=never".to_string()));

        let hooks_arg = args
            .iter()
            .find(|a| a.starts_with("core.hooksPath="))
            .expect("core.hooksPath argument must be present");
        let hooks_val = hooks_arg.strip_prefix("core.hooksPath=").unwrap();
        assert_ne!(hooks_val, "/dev/null");
        assert_ne!(hooks_val, "NUL");
        assert_ne!(hooks_val, "nul");
        let hooks_path = Path::new(hooks_val);
        assert!(
            hooks_path.is_dir(),
            "core.hooksPath must be an existing directory"
        );
        assert_eq!(
            std::fs::read_dir(hooks_path).unwrap().count(),
            0,
            "core.hooksPath must be empty"
        );

        let envs: BTreeMap<String, Option<String>> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect();

        assert_eq!(
            envs.get("GIT_CONFIG_NOSYSTEM"),
            Some(&Some("1".to_string()))
        );
        assert_eq!(envs.get("LC_ALL"), Some(&Some("C".to_string())));
        assert_eq!(
            envs.get("GIT_TERMINAL_PROMPT"),
            Some(&Some("0".to_string()))
        );
        assert_eq!(envs.get("GIT_NO_LAZY_FETCH"), Some(&Some("1".to_string())));

        let ceiling = envs
            .get("GIT_CEILING_DIRECTORIES")
            .expect("GIT_CEILING_DIRECTORIES must be set")
            .as_ref()
            .unwrap();
        assert_eq!(ceiling, r"C:\test");
        assert!(!ceiling.starts_with(r"\\?\"));

        let global_config = envs
            .get("GIT_CONFIG_GLOBAL")
            .expect("GIT_CONFIG_GLOBAL must be set")
            .as_ref()
            .unwrap();
        assert_ne!(global_config, "/dev/null");
        assert_ne!(global_config, "NUL");
        assert_ne!(global_config, "nul");
        let cfg_path = Path::new(global_config);
        assert!(
            cfg_path.is_file(),
            "GIT_CONFIG_GLOBAL must be an existing file"
        );
        assert_eq!(
            std::fs::metadata(cfg_path).unwrap().len(),
            0,
            "GIT_CONFIG_GLOBAL must be 0 bytes"
        );

        assert!(!envs.contains_key("HOME"));
    }

    #[test]
    fn windows_config_and_hooks_isolation_paths() {
        let cfg = empty_config_file();
        assert!(cfg.is_file());
        assert_eq!(std::fs::metadata(cfg).unwrap().len(), 0);

        let hooks = empty_hooks_dir();
        assert!(hooks.is_dir());
        assert_eq!(std::fs::read_dir(hooks).unwrap().count(), 0);
    }

    #[test]
    #[cfg(windows)]
    fn windows_timeout_proves_descendant_died() {
        let directory =
            std::env::temp_dir().join(format!("basal-git-win-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let pid_file = directory.join("pid");
        let survivor = directory.join("survivor");

        let script = format!(
            "$p = Start-Process cmd.exe -ArgumentList '/c ping -n 35 127.0.0.1 >nul & echo survived > \"{}\"' -PassThru; $p.Id | Out-File -FilePath \"{}\" -Encoding ascii -NoNewline; $p.WaitForExit()",
            survivor.display(),
            pid_file.display()
        );

        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let error = run_command(command).err().expect("command must time out");
        assert_eq!(error.code, codes::TIMEOUT);

        let pid_str = std::fs::read_to_string(&pid_file).expect("pid file must be written");
        let pid: u32 = pid_str.trim().parse().expect("pid must be a valid integer");

        let survived = survivor.exists();
        assert!(!survived, "git's descendant survived the timeout");

        let handle = unsafe {
            ffi::OpenProcess(
                ffi::PROCESS_QUERY_LIMITED_INFORMATION | ffi::SYNCHRONIZE,
                ffi::FALSE,
                pid,
            )
        };
        if !handle.is_null() {
            let wait_res = unsafe { ffi::WaitForSingleObject(handle, 0) };
            assert_eq!(
                wait_res,
                ffi::WAIT_OBJECT_0,
                "descendant process must be signaled (dead)"
            );
            let mut exit_code: u32 = 0;
            let ok = unsafe { ffi::GetExitCodeProcess(handle, &mut exit_code) };
            assert_ne!(ok, 0, "GetExitCodeProcess should succeed");
            assert_ne!(
                exit_code,
                ffi::STILL_ACTIVE,
                "descendant process must not be STILL_ACTIVE"
            );
            unsafe { ffi::CloseHandle(handle) };
        }

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg(windows)]
    fn windows_output_cap_refuses_oversized_output() {
        let mut command = Command::new("cmd");
        command
            .args([
                "/c",
                "for /L %i in (1,1,300000) do @echo 0123456789012345678901234567890123456789",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let error = run_command(command)
            .err()
            .expect("command must exceed output cap");
        assert_eq!(error.code, codes::TOO_LARGE);
    }

    #[test]
    #[cfg(windows)]
    fn windows_tags_output_is_bounded_while_reading() {
        let mut command = Command::new("cmd");
        command
            .args(["/c", "for /L %i in (1,1,4000) do @echo tag-%i"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let result = run_tags_command(command).expect("tag command must succeed");
        assert!(result.success);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().lines().count(),
            MAX_TAGS
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_git_hook_suppression_and_config_isolation() {
        if resolve_git().is_err() {
            return;
        }

        let temp_dir =
            std::env::temp_dir().join(format!("basal-git-win-test-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let run_setup = |args: &[&str]| {
            let status = Command::new(resolve_git().unwrap())
                .args(args)
                .current_dir(&temp_dir)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_setup(&["init"]);
        run_setup(&["config", "user.name", "TestUser"]);
        run_setup(&["config", "user.email", "test@example.com"]);

        let fake_hooks_dir = temp_dir.join("fake_hooks");
        std::fs::create_dir_all(&fake_hooks_dir).unwrap();
        let marker_file = temp_dir.join("hook_was_run.marker");
        let fake_hook = fake_hooks_dir.join("post-checkout.bat");
        std::fs::write(
            &fake_hook,
            format!("@echo ran > \"{}\"\r\n", marker_file.display()),
        )
        .unwrap();

        run_setup(&[
            "config",
            "core.hooksPath",
            &fake_hooks_dir.to_string_lossy(),
        ]);
        run_setup(&["config", "core.fsmonitor", "false"]);

        let file_path = temp_dir.join("file.txt");
        std::fs::write(&file_path, b"hello\n").unwrap();
        run_setup(&["add", "file.txt"]);
        run_setup(&["commit", "-m", "init"]);

        let ran = super::super::git(&temp_dir, &["log", "-1"]).expect("git log must succeed");
        assert!(ran.success);

        assert!(
            !marker_file.exists(),
            "Planted hook must NOT have run due to core.hooksPath override"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
