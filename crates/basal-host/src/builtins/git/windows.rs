//! Windows git reads use a private empty global config, explicit executable
//! paths, and a non-breakaway kill-on-close job. The child remains suspended
//! until membership in that exact job is verified.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Command;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        LocalFree, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadFile,
    },
    System::{
        JobObjects::{
            CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Pipes::{CreatePipe, PeekNamedPipe},
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

use super::{MAX_OUTPUT_BYTES, MAX_TAGS, Ran, STDERR_BYTES, TIMEOUT, codes};
use crate::builtins::{
    Denial,
    fs::windows::{PinnedDirectory, pin_directory},
};

fn native_error(what: &str) -> Denial {
    Denial::new(
        codes::GIT,
        format!("{what}: {}", std::io::Error::last_os_error()),
    )
}

fn wide(s: impl AsRef<OsStr>) -> Result<Vec<u16>, Denial> {
    let mut value: Vec<u16> = s.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(Denial::invalid("NUL in Windows process input"));
    }
    value.push(0);
    Ok(value)
}

/// Only a verbatim drive path can be converted to an ordinary drive path.
/// UNC, device, volume and drive-relative names are not supported by the fs policy.
fn drive_path(path: &Path) -> Result<PathBuf, Denial> {
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir)) => match prefix.kind() {
            Prefix::Disk(_) => Ok(path.to_path_buf()),
            Prefix::VerbatimDisk(_) => {
                let units: Vec<u16> = path.as_os_str().encode_wide().skip(4).collect();
                Ok(PathBuf::from(OsString::from_wide(&units)))
            }
            _ => Err(Denial::invalid("only absolute drive paths are supported")),
        },
        _ => Err(Denial::invalid("only absolute drive paths are supported")),
    }
}

fn canonical_image(path: &Path) -> Result<PathBuf, Denial> {
    let path = drive_path(path)?;
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        || !path.is_file()
    {
        return Err(Denial::new(
            codes::GIT,
            "the image must be an existing .exe",
        ));
    }
    drive_path(&std::fs::canonicalize(path).map_err(|e| Denial::new(codes::GIT, e.to_string()))?)
}

/// PATH is operator-controlled; only absolute directories participate. This
/// establishes a stable image path, not a signature/trust check of the binary.
fn resolve_git_binary(program: &OsStr, path_env: Option<&OsStr>) -> Result<PathBuf, Denial> {
    let p = Path::new(program);
    if p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
    {
        return Err(Denial::new(codes::GIT, "git scripts are refused"));
    }
    if program != "git" && program != "git.exe" {
        return canonical_image(p);
    }
    if let Some(path_env) = path_env {
        for directory in std::env::split_paths(path_env) {
            let Ok(directory) = drive_path(&directory) else {
                continue;
            };
            let image = directory.join("git.exe");
            if image.is_file() {
                return canonical_image(&image);
            }
            if directory.join("git.cmd").is_file() || directory.join("git.bat").is_file() {
                return Err(Denial::new(codes::GIT, "git PATH entry is a script"));
            }
        }
    }
    Err(Denial::new(
        codes::GIT,
        "git.exe was not found in absolute PATH directories",
    ))
}

fn resolve_git() -> Result<&'static Path, Denial> {
    static IMAGE: OnceLock<Result<PathBuf, Denial>> = OnceLock::new();
    match IMAGE
        .get_or_init(|| resolve_git_binary(OsStr::new("git"), std::env::var_os("PATH").as_deref()))
    {
        Ok(image) => Ok(image),
        Err(error) => Err(error.clone()),
    }
}

struct OwnedHandle(HANDLE);
// SAFETY: kernel handles may move across threads. Each owner closes once.
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: all constructors check for invalid handles before taking ownership.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct SecurityDescriptor(*mut std::ffi::c_void);
impl SecurityDescriptor {
    fn private() -> Result<Self, Denial> {
        let sddl = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)")?;
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the null-terminated SDDL and output pointer live through the call.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(native_error("private DACL"));
        }
        Ok(Self(descriptor))
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the conversion API allocated this descriptor with LocalAlloc.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// Guards precede cleanup: the directory cannot be replaced and the zero-byte
/// file cannot be written/deleted while git reads it. Each call gets a new DACL
/// protected directory, created exclusively using a cryptographically random name.
struct Isolation {
    directory: PathBuf,
    pins: Vec<PinnedDirectory>,
    config: Option<OwnedHandle>,
}
impl Isolation {
    fn new() -> Result<Self, Denial> {
        let base = drive_path(&std::env::temp_dir())?;
        let base = base
            .to_str()
            .ok_or_else(|| Denial::invalid("temp directory is not Unicode"))?;
        let base_pin = pin_directory(base)?;
        let descriptor = SecurityDescriptor::private()?;
        let attributes = descriptor.attributes();
        let mut random = [0u8; 16];
        // SAFETY: random is writable for exactly the requested byte count.
        if unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                random.as_mut_ptr(),
                random.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        } < 0
        {
            return Err(Denial::new(
                codes::GIT,
                "private directory randomness failed",
            ));
        }
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let directory = base_pin.path().join(format!("basal-git-{name}"));
        let path = wide(&directory)?;
        // SAFETY: path and private security descriptor live through the call.
        // CreateDirectory refuses an existing name; no existing contents are trusted.
        if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
            return Err(native_error("create private git directory"));
        }
        let mut isolation = Self {
            directory,
            pins: vec![base_pin],
            config: None,
        };
        isolation.pins.push(pin_directory(
            isolation
                .directory
                .to_str()
                .ok_or_else(|| Denial::invalid("private directory is not Unicode"))?,
        )?);
        let hooks = isolation.directory.join("hooks");
        if unsafe { CreateDirectoryW(wide(&hooks)?.as_ptr(), &attributes) } == 0 {
            return Err(native_error("create private hooks directory"));
        }
        isolation.pins.push(pin_directory(
            hooks
                .to_str()
                .ok_or_else(|| Denial::invalid("hooks directory is not Unicode"))?,
        )?);
        let config = wide(isolation.directory.join("global.config"))?;
        // SAFETY: CREATE_NEW is exclusive, OPEN_REPARSE_POINT never follows a link,
        // and read sharing only prevents both writes and deletion for the call.
        let handle = unsafe {
            CreateFileW(
                config.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(native_error("create empty global config"));
        }
        isolation.config = Some(OwnedHandle(handle));
        Ok(isolation)
    }
}
impl Drop for Isolation {
    fn drop(&mut self) {
        self.config.take();
        self.pins.clear();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub struct HardenedCommand {
    command: Command,
    _isolation: Isolation,
}
impl std::ops::Deref for HardenedCommand {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.command
    }
}
impl std::ops::DerefMut for HardenedCommand {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

/// The resources selected by this recipe are owned until the child tree exits.
pub fn hardened_command(repo: &Path) -> Result<HardenedCommand, Denial> {
    let isolation = Isolation::new()?;
    let repo = drive_path(repo)?;
    let mut command = Command::new(resolve_git()?);
    command.env_clear();
    for key in ["PATH", "SystemRoot", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            isolation.directory.join("global.config"),
        )
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    if let Some(parent) = repo.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(isolation.directory.join("hooks"));
    command
        .arg("-C")
        .arg(repo)
        .args([
            "--no-optional-locks",
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
        ])
        .arg(hooks)
        .args([
            "-c",
            "core.pager=cat",
            "-c",
            "log.showSignature=false",
            "-c",
            "protocol.allow=never",
        ]);
    Ok(HardenedCommand {
        command,
        _isolation: isolation,
    })
}

/// Microsoft CRT encoding over UTF-16 units (including unpaired surrogates).
/// Backslashes before quotes and before the closing quote must be doubled.
fn append_windows_arg(command: &mut Vec<u16>, argument: &OsStr) -> Result<(), Denial> {
    let units: Vec<u16> = argument.encode_wide().collect();
    if units.contains(&0) {
        return Err(Denial::invalid("NUL in process argument"));
    }
    command.push(b'"' as u16);
    let mut slashes = 0;
    for unit in units {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let count = if unit == b'"' as u16 {
            2 * slashes + 1
        } else {
            slashes
        };
        command.extend(std::iter::repeat_n(b'\\' as u16, count));
        command.push(unit);
        slashes = 0;
    }
    command.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes));
    command.push(b'"' as u16);
    Ok(())
}

struct AttributeList {
    buffer: Vec<usize>,
    jobs: Box<[HANDLE; 1]>,
    handles: Box<[HANDLE; 3]>,
}
impl AttributeList {
    fn new(job: HANDLE, handles: [HANDLE; 3]) -> Result<Self, Denial> {
        let mut size = 0;
        // SAFETY: the first call queries storage size; no attribute list exists yet.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size);
        }
        if size == 0 {
            return Err(native_error("attribute list size"));
        }
        let mut buffer = vec![0; size.div_ceil(std::mem::size_of::<usize>())];
        if unsafe { InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), 2, 0, &mut size) }
            == 0
        {
            return Err(native_error("initialize attribute list"));
        }
        let mut list = Self {
            buffer,
            jobs: Box::new([job]),
            handles: Box::new(handles),
        };
        // SAFETY: both payloads have stable boxed addresses. Drop deletes the
        // attribute list before Rust drops either payload or its backing storage.
        for (attribute, payload, length) in [
            (
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                list.jobs.as_mut_ptr(),
                std::mem::size_of_val(list.jobs.as_ref()),
            ),
            (
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list.handles.as_mut_ptr(),
                std::mem::size_of_val(list.handles.as_ref()),
            ),
        ] {
            if unsafe {
                UpdateProcThreadAttribute(
                    list.pointer(),
                    0,
                    attribute,
                    payload.cast(),
                    length,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(native_error("set process attribute"));
            }
        }
        Ok(list)
    }
    fn pointer(&mut self) -> *mut std::ffi::c_void {
        self.buffer.as_mut_ptr().cast()
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialization succeeded and backing/payload storage is still live.
        unsafe {
            DeleteProcThreadAttributeList(self.pointer());
        }
    }
}

struct ProcessJob {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
    terminated: bool,
    #[cfg(test)]
    fail_termination_once: bool,
}
impl ProcessJob {
    fn terminate(&mut self) -> Result<(), Denial> {
        if self.terminated {
            return Ok(());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_termination_once) {
            return Err(Denial::new(codes::GIT, "injected job termination failure"));
        }
        if let Some(job) = &self.job {
            // SAFETY: the job is exclusively owned and cannot close during this call.
            if unsafe { TerminateJobObject(job.0, 1) } == 0 {
                return Err(native_error("terminate git job"));
            }
        }
        self.terminated = true;
        Ok(())
    }
    fn stop_tree(&mut self) {
        if self.terminate().is_err() {
            let _ = self.terminate();
        }
        // Last-handle close is the kill-on-close fallback, before any joins.
        self.job.take();
    }
}
impl Drop for ProcessJob {
    fn drop(&mut self) {
        self.stop_tree();
    }
}

fn pipe(attributes: &SECURITY_ATTRIBUTES) -> Result<(OwnedHandle, OwnedHandle), Denial> {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: output pointers and security attributes are valid for this call.
    if unsafe { CreatePipe(&mut read, &mut write, attributes, 0) } == 0 {
        return Err(native_error("create git pipe"));
    }
    let read = OwnedHandle(read);
    let write = OwnedHandle(write);
    if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(native_error("clear pipe inheritance"));
    }
    Ok((read, write))
}

pub struct CommandInput {
    command: Command,
    _isolation: Option<Isolation>,
}
impl From<HardenedCommand> for CommandInput {
    fn from(value: HardenedCommand) -> Self {
        Self {
            command: value.command,
            _isolation: Some(value._isolation),
        }
    }
}
impl From<Command> for CommandInput {
    fn from(command: Command) -> Self {
        Self {
            command,
            _isolation: None,
        }
    }
}

fn environment(command: &Command, hardened: bool) -> Result<Vec<u16>, Denial> {
    fn key(value: &OsStr) -> Vec<u16> {
        value
            .encode_wide()
            .map(|c| {
                if (b'a' as u16..=b'z' as u16).contains(&c) {
                    c - 32
                } else {
                    c
                }
            })
            .collect()
    }
    let mut entries = BTreeMap::new();
    if !hardened {
        for (name, value) in std::env::vars_os() {
            entries.insert(key(&name), (name, value));
        }
    }
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            entries.insert(key(name), (name.to_owned(), value.to_owned()));
        } else {
            entries.remove(&key(name));
        }
    }
    let mut block = Vec::new();
    for (_, (name, value)) in entries {
        let name = wide(name)?;
        block.extend_from_slice(&name[..name.len() - 1]);
        block.push(b'=' as u16);
        block.extend(wide(value)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// The optional observer runs while the primary thread is still suspended. It
/// is used by native tests to inspect the exact job, not just any inherited job.
fn spawn_job_command(
    input: &CommandInput,
    observer: impl FnOnce(&ProcessJob) -> Result<(), Denial>,
) -> Result<(ProcessJob, OwnedHandle, OwnedHandle), Denial> {
    spawn_with_argv0(input, observer, None)
}

fn spawn_with_argv0(
    input: &CommandInput,
    observer: impl FnOnce(&ProcessJob) -> Result<(), Denial>,
    argv0: Option<&OsStr>,
) -> Result<(ProcessJob, OwnedHandle, OwnedHandle), Denial> {
    let command = &input.command;
    let program = command.get_program();
    let image = if program == "git" || program == "git.exe" {
        resolve_git()?.to_path_buf()
    } else {
        canonical_image(Path::new(program))?
    };
    let application = wide(&image)?;
    let mut command_line = Vec::new();
    append_windows_arg(&mut command_line, argv0.unwrap_or(image.as_os_str()))?;
    for argument in command.get_args() {
        command_line.push(b' ' as u16);
        append_windows_arg(&mut command_line, argument)?;
    }
    command_line.push(0);
    if command_line.len() > 32767 {
        return Err(Denial::invalid("Windows command line is too long"));
    }
    let env = environment(command, input._isolation.is_some())?;
    let cwd = command
        .get_current_dir()
        .map(|p| drive_path(p).and_then(wide))
        .transpose()?;
    let cwd_pointer = cwd.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(native_error("create git job"));
    }
    let job = OwnedHandle(job);
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(native_error("git job limits"));
    }
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // NUL is a device, never a reopen of the trusted global configuration file.
    let nul = wide("NUL")?;
    let stdin = unsafe {
        CreateFileW(
            nul.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &inherit,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if stdin == INVALID_HANDLE_VALUE {
        return Err(native_error("open inert stdin"));
    }
    let stdin = OwnedHandle(stdin);
    let (stdout, stdout_writer) = pipe(&inherit)?;
    let (stderr, stderr_writer) = pipe(&inherit)?;
    let mut attributes = AttributeList::new(job.0, [stdin.0, stdout_writer.0, stderr_writer.0])?;
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = stdout_writer.0;
    startup.StartupInfo.hStdError = stderr_writer.0;
    startup.lpAttributeList = attributes.pointer();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: buffers, owned attribute payloads, handles and startup structure
    // remain live through creation; lpApplicationName is the canonical image.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_SUSPENDED,
            env.as_ptr().cast(),
            cwd_pointer,
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(native_error("start git"));
    }
    let primary_thread = OwnedHandle(process.hThread);
    let guard = ProcessJob {
        process: OwnedHandle(process.hProcess),
        job: Some(job),
        terminated: false,
        #[cfg(test)]
        fail_termination_once: false,
    };
    let mut member = 0;
    // Verify the exact job before the child can perform any work. A failed
    // verification drops the job and kills the still-suspended process.
    if unsafe { IsProcessInJob(guard.process.0, guard.job.as_ref().unwrap().0, &mut member) } == 0
        || member == 0
    {
        // If assignment ever fails, job closure cannot kill an unassigned child.
        // It is still suspended and its primary thread has never done work.
        unsafe {
            TerminateProcess(guard.process.0, 1);
        }
        return Err(Denial::new(
            codes::GIT,
            "git did not join its containment job",
        ));
    }
    observer(&guard)?;
    if unsafe { ResumeThread(primary_thread.0) } != 1 {
        return Err(Denial::new(
            codes::GIT,
            "git primary thread was not suspended exactly once",
        ));
    }
    Ok((guard, stdout, stderr))
}

struct PipeOutput {
    bytes: Vec<u8>,
    limit_reached: bool,
}

fn drain_pipe(
    handle: HANDLE,
    cap: usize,
    lines: Option<usize>,
    cancelled: &AtomicBool,
) -> Result<PipeOutput, Denial> {
    let mut output = PipeOutput {
        bytes: Vec::new(),
        limit_reached: false,
    };
    let mut count = 0;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(output);
        }
        let mut available = 0;
        // Peek avoids an uncancellable synchronous read when exceptional cleanup
        // cannot terminate a process. Only this thread reads this pipe.
        if unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(output);
            }
            return Err(native_error("peek git pipe"));
        }
        if available == 0 {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        let mut chunk = [0u8; STDERR_BYTES];
        let mut read = 0;
        if unsafe {
            ReadFile(
                handle,
                chunk.as_mut_ptr(),
                available.min(chunk.len() as u32),
                &mut read,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(native_error("read git pipe"));
        }
        for byte in &chunk[..read as usize] {
            if output.bytes.len() < cap {
                output.bytes.push(*byte);
            }
            if cap > STDERR_BYTES && output.bytes.len() > MAX_OUTPUT_BYTES {
                return Err(Denial::new(
                    codes::TOO_LARGE,
                    "git output exceeds the byte cap",
                ));
            }
            if *byte == b'\n' {
                count += 1;
                if lines.is_some_and(|limit| count >= limit) {
                    output.limit_reached = true;
                    return Ok(output);
                }
            }
        }
    }
}

struct ScopeCleanup<'a> {
    guard: &'a mut ProcessJob,
    cancelled: &'a AtomicBool,
}
impl Drop for ScopeCleanup<'_> {
    fn drop(&mut self) {
        self.guard.stop_tree();
        self.cancelled.store(true, Ordering::Release);
    }
}

enum Completion {
    Exit(Result<u32, Denial>),
    Stdout(Result<PipeOutput, Denial>),
    Stderr(Result<PipeOutput, Denial>),
}
#[derive(Default)]
struct Faults {
    #[cfg(test)]
    fail_termination_once: bool,
    #[cfg(test)]
    panic_after_waiter: bool,
}

fn run_until(
    input: CommandInput,
    lines: Option<usize>,
    timeout: Duration,
    _faults: Faults,
) -> Result<Ran, Denial> {
    let (mut guard, stdout, stderr) = spawn_job_command(&input, |_| Ok(()))?;
    #[cfg(test)]
    {
        guard.fail_termination_once = _faults.fail_termination_once;
    }
    let out = stdout.0 as usize;
    let err = stderr.0 as usize;
    let process = guard.process.0 as usize;
    let deadline = Instant::now() + timeout;
    let cancelled = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    thread::scope(|scope| {
        // This guard drops on an early return or a panic during thread startup,
        // BEFORE scope joins workers. The outer process/pipe owners outlive joins.
        let cleanup = ScopeCleanup {
            guard: &mut guard,
            cancelled: &cancelled,
        };
        let exit_sender = sender.clone();
        let cancel = &cancelled;
        scope.spawn(move || {
            let result = loop {
                let wait = unsafe { WaitForSingleObject(process as HANDLE, 10) };
                if wait == WAIT_OBJECT_0 {
                    let mut code = 0;
                    break if unsafe { GetExitCodeProcess(process as HANDLE, &mut code) } == 0 {
                        Err(native_error("git exit code"))
                    } else {
                        Ok(code)
                    };
                }
                if wait != WAIT_TIMEOUT {
                    break Err(native_error("wait for git"));
                }
                if cancel.load(Ordering::Acquire) {
                    break Err(Denial::new(codes::GIT, "git wait cancelled"));
                }
            };
            let _ = exit_sender.send(Completion::Exit(result));
        });
        #[cfg(test)]
        if _faults.panic_after_waiter {
            panic!("injected thread-start failure");
        }
        let out_sender = sender.clone();
        scope.spawn(move || {
            let _ = out_sender.send(Completion::Stdout(drain_pipe(
                out as HANDLE,
                MAX_OUTPUT_BYTES + 1,
                lines,
                cancel,
            )));
        });
        scope.spawn(move || {
            let _ = sender.send(Completion::Stderr(drain_pipe(
                err as HANDLE,
                STDERR_BYTES,
                None,
                cancel,
            )));
        });
        let mut code = None;
        let mut output = None;
        let mut errors = None;
        let mut limited = false;
        while code.is_none() || output.is_none() || errors.is_none() {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| {
                    Denial::new(
                        codes::TIMEOUT,
                        format!("git ran longer than {} s", timeout.as_secs()),
                    )
                })?;
            match event {
                Completion::Exit(result) => {
                    code = Some(result?);
                    cleanup.guard.stop_tree();
                }
                Completion::Stdout(result) => {
                    let result = result?;
                    limited = result.limit_reached;
                    if limited {
                        cleanup.guard.stop_tree();
                    }
                    output = Some(result.bytes);
                }
                Completion::Stderr(result) => {
                    errors = Some(result?.bytes);
                }
            }
        }
        Ok(Ran {
            success: limited || code == Some(0),
            code: Some(if limited { 0 } else { code.unwrap() as i32 }),
            stdout: output.unwrap(),
            stderr: String::from_utf8_lossy(&errors.unwrap()).into_owned(),
        })
    })
}

pub fn run_command(command: impl Into<CommandInput>) -> Result<Ran, Denial> {
    run_until(command.into(), None, TIMEOUT, Faults::default())
}
pub fn run_tags_command(command: impl Into<CommandInput>) -> Result<Ran, Denial> {
    run_until(command.into(), Some(MAX_TAGS), TIMEOUT, Faults::default())
}

#[cfg(test)]
mod tests;
