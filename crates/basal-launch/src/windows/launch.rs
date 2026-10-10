//! Building the launch: creation attributes, the suspended child, the
//! parent's checks before resume, and resume.

use super::context;
use super::deviation::{Deviation, Shape};
use super::error::{LaunchError, Refusal};
use super::job::{self, JobLimits};
use super::native::{Result, SidBuf, check, last, owned, wide};
use super::process::{ConfinedProcess, KILL_EXIT_CODE};
use super::profile::{PackageSid, create_or_open_profile};
use super::spawn_lock::{make_inheritable, spawn_lock};
use super::token::{self, BirthExpectation};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_QUERY,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    OpenThreadToken, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, SetThreadToken, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::{
    PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT, PROCESS_CREATION_CHILD_PROCESS_RESTRICTED,
};

// Process-creation mitigation policy bits, the `..._ALWAYS_ON` values of
// `PROCESS_CREATION_MITIGATION_POLICY_*` in winnt.h.

/// Using an invalid handle raises an exception instead of returning an
/// error, and the setting cannot be turned off.
pub const MITIGATION_STRICT_HANDLE_CHECKS: u64 = 1 << 24;
/// No system calls into the Win32k (GUI) kernel component.
pub const MITIGATION_WIN32K_SYSTEM_CALL_DISABLE: u64 = 1 << 28;
/// No legacy extension points (AppInit DLLs, IMEs, window hooks and the
/// like) load into the process.
pub const MITIGATION_EXTENSION_POINT_DISABLE: u64 = 1 << 32;
/// No executable memory can be created or made executable after load.
pub const MITIGATION_PROHIBIT_DYNAMIC_CODE: u64 = 1 << 36;
/// Only Microsoft-signed DLLs load.
pub const MITIGATION_MICROSOFT_SIGNED_ONLY: u64 = 1 << 44;
/// No images load from remote (network) locations.
pub const MITIGATION_NO_REMOTE_IMAGES: u64 = 1 << 52;
/// No images with a Low mandatory label load.
pub const MITIGATION_NO_LOW_LABEL_IMAGES: u64 = 1 << 56;
/// DLLs are looked up in System32 before the application directory.
pub const MITIGATION_PREFER_SYSTEM32_IMAGES: u64 = 1 << 60;

/// The mitigation policy every confined worker is created with.
pub const MITIGATION_POLICY: u64 = MITIGATION_STRICT_HANDLE_CHECKS
    | MITIGATION_WIN32K_SYSTEM_CALL_DISABLE
    | MITIGATION_EXTENSION_POINT_DISABLE
    | MITIGATION_PROHIBIT_DYNAMIC_CODE
    | MITIGATION_MICROSOFT_SIGNED_ONLY
    | MITIGATION_NO_REMOTE_IMAGES
    | MITIGATION_NO_LOW_LABEL_IMAGES
    | MITIGATION_PREFER_SYSTEM32_IMAGES;

/// The capability granted only by the `capabilities-present` test variant:
/// `internetClient`.
#[cfg(feature = "deviations")]
const TEST_CAPABILITY: &str = "S-1-15-3-1";

/// What to start, and how.
#[cfg_attr(
    not(feature = "deviations"),
    doc = "```compile_fail\nuse basal_launch::LaunchOptions;\nlet mut options = LaunchOptions::new(\"C:\\\\ckdev-worker.exe\", 512 << 20);\noptions.before_resume = None;\n```"
)]
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// The absolute path of the image.
    pub program: PathBuf,
    /// Arguments after the image name. The launcher appends
    /// `--package-sid=<SID>` after them on every launch.
    pub args: Vec<OsString>,
    /// The commit limit of the worker's job, in bytes. Callers pass the
    /// limit of the worker's profile from `basal_proto::limits`.
    pub job_commit_bytes: u64,
    /// The recipe. Production builds have only the full confinement.
    pub deviation: Deviation,
    /// A test-only observer called while the child is still suspended. It can
    /// enable handle tracing before loader initialization creates any objects.
    #[cfg(feature = "deviations")]
    pub before_resume: Option<fn(HANDLE, u32) -> Result<()>>,
}

impl LaunchOptions {
    /// The full confinement for `program`, with no arguments.
    pub fn new(program: impl Into<PathBuf>, job_commit_bytes: u64) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            job_commit_bytes,
            deviation: Deviation::Full,
            #[cfg(feature = "deviations")]
            before_resume: None,
        }
    }

    /// Adds one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Selects the recipe.
    pub fn deviation(mut self, deviation: Deviation) -> Self {
        self.deviation = deviation;
        self
    }
}

/// Starts a worker. On return it is running with stdin, stdout and stderr
/// connected to the parent through three pipes, and nothing else inherited.
///
/// Under the full confinement the child is created suspended and is only
/// resumed after the parent has read back, through its own handles, the
/// job's limits and membership, the child's primary token and its start-up
/// thread token. Any difference kills the child before its first
/// instruction and returns the named refusal.
pub fn launch(options: &LaunchOptions) -> std::result::Result<ConfinedProcess, LaunchError> {
    if !options.program.is_absolute() {
        return Err(LaunchError::Failed(format!(
            "the worker image path must be absolute: {}",
            options.program.display()
        )));
    }
    let commit_bytes = usize::try_from(options.job_commit_bytes)
        .map_err(|_| format!("commit limit {} does not fit", options.job_commit_bytes))?;
    let package = create_or_open_profile()?;
    let parent_token = token::own_token()?;
    let context = context::create(&options.program, parent_token.as_raw_handle(), &package)?;

    // The child's ends are made inheritable only under the spawn lock, which
    // is released below once they are closed. It is taken before the pipes
    // exist so that on an early return the ends, dropped in reverse order of
    // declaration, are closed before the lock is released.
    let spawn_lock = spawn_lock();
    let (child_stdin, parent_stdin) = pipe()?;
    let (parent_stdout, child_stdout) = pipe()?;
    let (parent_stderr, child_stderr) = pipe()?;
    // Only the child's three ends are inheritable, and only they are named
    // in the inherited-handle list.
    make_inheritable(
        &spawn_lock,
        &[
            child_stdin.as_raw_handle(),
            child_stdout.as_raw_handle(),
            child_stderr.as_raw_handle(),
        ],
    )
    .map_err(|error| format!("SetHandleInformation(child pipe end): {error}"))?;
    let inherited: [HANDLE; 3] = [
        child_stdin.as_raw_handle(),
        child_stdout.as_raw_handle(),
        child_stderr.as_raw_handle(),
    ];
    let mut command = command_line(&options.program, &options.args, &package, options.deviation);
    let deviation = options.deviation;

    let spawned = match deviation.shape() {
        Shape::Confined => spawn_confined(
            options,
            commit_bytes,
            &package,
            parent_token.as_raw_handle(),
            &context,
            &inherited,
            &mut command,
        )?,
        #[cfg(feature = "deviations")]
        Shape::LpacOnly | Shape::Plain => spawn_control(
            options,
            &package,
            &context,
            &inherited,
            &mut command,
            deviation.shape() == Shape::LpacOnly,
        )?,
    };

    // The child has inherited its own copies of its three pipe ends; the
    // parent's copies would keep the pipes open after the child exits.
    drop((child_stdin, child_stdout, child_stderr));
    drop(spawn_lock);
    Ok(ConfinedProcess::new(
        spawned.process,
        spawned.pid,
        spawned.job,
        package,
        inherited.map(|handle| handle as usize),
        File::from(parent_stdin),
        File::from(parent_stdout),
        File::from(parent_stderr),
        context,
    ))
}

struct Spawned {
    process: OwnedHandle,
    pid: u32,
    job: OwnedHandle,
}

/// The full confinement, or one of the check variants built from it.
fn spawn_confined(
    options: &LaunchOptions,
    commit_bytes: usize,
    package: &PackageSid,
    parent_token: HANDLE,
    context: &context::Context,
    inherited: &[HANDLE; 3],
    command: &mut [u16],
) -> std::result::Result<Spawned, LaunchError> {
    let deviation = options.deviation;
    let source = TokenSource::start(
        &options.program,
        package,
        deviation.less_privileged(),
        context,
    )?;
    let (primary, mut expected) = token::build_primary(parent_token, package.as_str(), deviation)?;
    let initial = token::build_initial(source.token.as_raw_handle())?;
    let job = job::create_confined(commit_bytes, deviation.job_active_process_limit())?;

    // Every value an attribute points at must live until the process exists.
    let capability_sids = capability_sids(deviation)?;
    let capabilities: Vec<SID_AND_ATTRIBUTES> = capability_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: SE_GROUP_ENABLED as u32,
        })
        .collect();
    for sid in &capability_sids {
        expected.capabilities.push(sid.to_text()?);
    }
    let security = security_capabilities(package, &capabilities);
    let opt_out = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
    let jobs: [HANDLE; 1] = [job.as_raw_handle()];
    let mitigation = deviation.mitigation_policy();
    let child_policy = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;

    let mut attributes = AttributeList::new(6)?;
    if deviation.restricts_inherited_handles() {
        attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited)?;
    }
    attributes.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
    if deviation.less_privileged() {
        attributes.add(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &opt_out,
        )?;
    }
    if deviation.joins_job() {
        attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    }
    attributes.add(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigation)?;
    attributes.add(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &child_policy)?;

    let startup = startup_info(context, inherited, &mut attributes);
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    check(
        unsafe {
            CreateProcessAsUserW(
                primary.as_raw_handle(),
                wide(&options.program).as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATION_FLAGS,
                context.environment.as_ptr().cast::<c_void>(),
                context.cwd.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        },
        "CreateProcessAsUserW",
    )?;
    let suspended = Suspended::adopt(&info)?;
    drop(attributes);

    if let Err(refusal) = check_before_resume(
        &suspended,
        &job,
        commit_bytes,
        &expected,
        initial,
        deviation,
    ) {
        suspended.kill();
        return Err(refusal);
    }
    #[cfg(feature = "deviations")]
    if let Some(observer) = options.before_resume {
        observer(suspended.process.as_raw_handle(), info.dwProcessId)?;
    }
    suspended.resume()?;
    drop(source);
    Ok(Spawned {
        process: suspended.process,
        pid: info.dwProcessId,
        job,
    })
}

/// The parent's checks on the suspended child. On success the start-up
/// thread token is set on the main thread and the parent's handle to it is
/// closed.
fn check_before_resume(
    child: &Suspended,
    job: &OwnedHandle,
    commit_bytes: usize,
    expected: &BirthExpectation,
    initial: OwnedHandle,
    deviation: Deviation,
) -> std::result::Result<(), LaunchError> {
    let limits = job::read_limits(job.as_raw_handle())
        .map_err(|error| LaunchError::refused(Refusal::JobLimitsMismatch, error))?;
    let required = JobLimits::confined(commit_bytes);
    if limits != required {
        return Err(LaunchError::refused(
            Refusal::JobLimitsMismatch,
            format!("read back {limits:?}, required {required:?}"),
        ));
    }
    let member = job::contains(job.as_raw_handle(), child.process.as_raw_handle())
        .map_err(|error| LaunchError::refused(Refusal::NotInOwnedJob, error))?;
    if !member {
        return Err(LaunchError::refused(
            Refusal::NotInOwnedJob,
            "the suspended child is not in the job the parent created",
        ));
    }

    let birth = process_token(child.process.as_raw_handle())
        .and_then(|token| token::read_facts(token.as_raw_handle()))
        .map_err(|error| LaunchError::refused(Refusal::BirthTokenMismatch, error))?;
    token::check_birth(&birth, expected)
        .map_err(|error| LaunchError::refused(Refusal::BirthTokenMismatch, error))?;

    if deviation.sets_initial_token() {
        let thread = child.thread.as_raw_handle();
        check(
            unsafe { SetThreadToken(&thread, initial.as_raw_handle()) },
            "SetThreadToken(suspended main thread)",
        )
        .map_err(|error| LaunchError::refused(Refusal::InitialTokenOpen, error))?;
    }
    let assigned = thread_token(child.thread.as_raw_handle())
        .and_then(|token| token::read_facts(token.as_raw_handle()))
        .map_err(|error| LaunchError::refused(Refusal::InitialTokenOpen, error))?;
    token::check_initial(&assigned, &expected.package)
        .map_err(|error| LaunchError::refused(Refusal::InitialTokenOpen, error))?;
    // The handle was never inheritable and is not in the handle list; close
    // it now so the parent holds no reference to a token the child uses.
    if unsafe { CloseHandle(initial.into_raw_handle()) } == 0 {
        return Err(LaunchError::refused(
            Refusal::InitialTokenOpen,
            last("CloseHandle(initial token)"),
        ));
    }
    Ok(())
}

/// The two positive controls: no restricted primary, no start-up token, no
/// mitigations, created with `CreateProcessW` in a job that only kills on
/// close.
#[cfg(feature = "deviations")]
fn spawn_control(
    options: &LaunchOptions,
    package: &PackageSid,
    context: &context::Context,
    inherited: &[HANDLE; 3],
    command: &mut [u16],
    appcontainer: bool,
) -> std::result::Result<Spawned, LaunchError> {
    let job = job::create_kill_on_close()?;
    let no_capabilities: [SID_AND_ATTRIBUTES; 0] = [];
    let security = security_capabilities(package, &no_capabilities);
    let opt_out = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
    let jobs: [HANDLE; 1] = [job.as_raw_handle()];
    let mut attributes = AttributeList::new(4)?;
    if options.deviation.restricts_inherited_handles() {
        attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited)?;
    }
    if appcontainer {
        attributes.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
        attributes.add(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &opt_out,
        )?;
    }
    attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    let startup = startup_info(context, inherited, &mut attributes);
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    check(
        unsafe {
            CreateProcessW(
                wide(&options.program).as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATION_FLAGS,
                context.environment.as_ptr().cast::<c_void>(),
                context.cwd.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        },
        "CreateProcessW(control)",
    )?;
    let suspended = Suspended::adopt(&info)?;
    #[cfg(feature = "deviations")]
    if let Some(observer) = options.before_resume {
        observer(suspended.process.as_raw_handle(), info.dwProcessId)?;
    }
    suspended.resume()?;
    Ok(Spawned {
        process: suspended.process,
        pid: info.dwProcessId,
        job,
    })
}

/// Suspended, so the parent can check the child before it runs; extended
/// startup information, for the attribute list; no console window; and a
/// UTF-16 environment block.
const CREATION_FLAGS: u32 =
    EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;

fn capability_sids(deviation: Deviation) -> Result<Vec<SidBuf>> {
    #[cfg(feature = "deviations")]
    if deviation.grants_capability() {
        return Ok(vec![SidBuf::parse(TEST_CAPABILITY)?]);
    }
    let _ = deviation;
    Ok(Vec::new())
}

/// The AppContainer creation attribute. The capability list pointer is
/// never NULL, even when the list is empty.
fn security_capabilities(
    package: &PackageSid,
    capabilities: &[SID_AND_ATTRIBUTES],
) -> SECURITY_CAPABILITIES {
    SECURITY_CAPABILITIES {
        AppContainerSid: package.as_psid(),
        Capabilities: capabilities.as_ptr().cast_mut(),
        CapabilityCount: capabilities.len() as u32,
        Reserved: 0,
    }
}

fn startup_info(
    context: &context::Context,
    inherited: &[HANDLE; 3],
    attributes: &mut AttributeList,
) -> STARTUPINFOEXW {
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    // The system only reads the desktop name.
    startup.StartupInfo.lpDesktop = context.desktop_name.as_ptr().cast_mut();
    startup.lpAttributeList = attributes.as_ptr();
    startup
}

/// The command line: the quoted image path, the caller's arguments, the
/// package SID and, for a worker check the parent cannot set up itself, the
/// request to the worker.
fn command_line(
    program: &Path,
    args: &[OsString],
    package: &PackageSid,
    deviation: Deviation,
) -> Vec<u16> {
    let mut command: Vec<u16> = Vec::new();
    command.push(u16::from(b'"'));
    command.extend(program.as_os_str().encode_wide());
    command.push(u16::from(b'"'));
    let mut all: Vec<OsString> = args.to_vec();
    all.push(format!("--package-sid={}", package.as_str()).into());
    if let Some(argument) = deviation.child_argument() {
        all.push(argument.into());
    }
    for arg in &all {
        command.push(u16::from(b' '));
        push_argument(&mut command, arg);
    }
    command.push(0);
    command
}

/// Appends one argument quoted so the C runtime's parser reads it back
/// unchanged: backslashes are literal except before a double quote, where
/// they and the quote are escaped.
pub(crate) fn push_argument(command: &mut Vec<u16>, arg: &OsStr) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&unit| unit == u16::from(b' ') || unit == u16::from(b'\t') || unit == QUOTE);
    if !needs_quotes {
        command.extend(units);
        return;
    }
    command.push(QUOTE);
    let mut backslashes = 0;
    for unit in units {
        if unit == BACKSLASH {
            backslashes += 1;
        } else {
            if unit == QUOTE {
                command.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        command.push(unit);
    }
    command.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    command.push(QUOTE);
}

/// An anonymous pipe as (read end, write end). Neither end is inheritable.
pub(crate) fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    check(
        unsafe { CreatePipe(&mut read, &mut write, null(), 0) },
        "CreatePipe",
    )?;
    Ok((owned(read, "CreatePipe")?, owned(write, "CreatePipe")?))
}

fn process_token(process: HANDLE) -> Result<OwnedHandle> {
    let mut token = null_mut();
    check(
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) },
        "OpenProcessToken(child)",
    )?;
    owned(token, "OpenProcessToken(child)")
}

/// The thread's impersonation token, opened with the parent's own identity
/// rather than the thread's.
fn thread_token(thread: HANDLE) -> Result<OwnedHandle> {
    let mut token = null_mut();
    check(
        unsafe { OpenThreadToken(thread, TOKEN_QUERY, 1, &mut token) },
        "OpenThreadToken(suspended main thread)",
    )?;
    owned(token, "OpenThreadToken(suspended main thread)")
}

/// A created, not yet resumed child. Dropping it without resuming kills it.
struct Suspended {
    process: OwnedHandle,
    thread: OwnedHandle,
}

impl Suspended {
    fn adopt(info: &PROCESS_INFORMATION) -> Result<Self> {
        Ok(Self {
            process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
            thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
        })
    }

    fn kill(&self) {
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), KILL_EXIT_CODE);
            WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
        }
    }

    fn resume(&self) -> Result<()> {
        if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
            let error = last("ResumeThread");
            self.kill();
            return Err(error);
        }
        Ok(())
    }
}

/// A process born into the worker's AppContainer and never resumed, whose
/// token is the source of the start-up thread token. Dropping it kills it.
struct TokenSource {
    process: OwnedHandle,
    token: OwnedHandle,
    _job: OwnedHandle,
}

impl TokenSource {
    fn start(
        program: &Path,
        package: &PackageSid,
        less_privileged: bool,
        context: &context::Context,
    ) -> Result<Self> {
        // The source runs no code, but it is still put in a kill-on-close
        // job so a parent that dies cannot leave it suspended forever.
        let job = job::create_kill_on_close()?;
        let no_capabilities: [SID_AND_ATTRIBUTES; 0] = [];
        let security = security_capabilities(package, &no_capabilities);
        let opt_out = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        let jobs: [HANDLE; 1] = [job.as_raw_handle()];
        let mut attributes = AttributeList::new(3)?;
        attributes.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
        if less_privileged {
            attributes.add(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                &opt_out,
            )?;
        }
        attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes.as_ptr();
        let mut command = wide(format!("\"{}\"", program.display()));
        let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
        check(
            unsafe {
                CreateProcessW(
                    wide(program).as_ptr(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    CREATION_FLAGS,
                    context.environment.as_ptr().cast::<c_void>(),
                    context.cwd.as_ptr(),
                    &startup.StartupInfo,
                    &mut info,
                )
            },
            "CreateProcessW(token source)",
        )?;
        let suspended = Suspended::adopt(&info)?;
        let mut token = null_mut();
        let opened = check(
            unsafe {
                OpenProcessToken(
                    suspended.process.as_raw_handle(),
                    TOKEN_ALL_ACCESS,
                    &mut token,
                )
            },
            "OpenProcessToken(token source)",
        );
        if let Err(error) = opened {
            suspended.kill();
            return Err(error);
        }
        let token = match owned(token, "OpenProcessToken(token source)") {
            Ok(token) => token,
            Err(error) => {
                suspended.kill();
                return Err(error);
            }
        };
        Ok(Self {
            process: suspended.process,
            token,
            _job: job,
        })
    }
}

impl Drop for TokenSource {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), KILL_EXIT_CODE);
            WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
        }
    }
}

/// A process/thread attribute list. The values added must outlive every
/// use of the list, since the list stores pointers to them.
pub(crate) struct AttributeList {
    buffer: Vec<usize>,
}

impl AttributeList {
    pub(crate) fn new(capacity: u32) -> Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), capacity, 0, &mut bytes) };
        if bytes == 0 {
            return Err(last("InitializeProcThreadAttributeList(size)"));
        }
        let mut list = Self {
            buffer: vec![0; bytes.div_ceil(size_of::<usize>())],
        };
        check(
            unsafe { InitializeProcThreadAttributeList(list.as_ptr(), capacity, 0, &mut bytes) },
            "InitializeProcThreadAttributeList",
        )?;
        Ok(list)
    }

    pub(crate) fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    pub(crate) fn add<T: ?Sized>(&mut self, attribute: u32, value: &T) -> Result<()> {
        check(
            unsafe {
                UpdateProcThreadAttribute(
                    self.as_ptr(),
                    0,
                    attribute as usize,
                    (value as *const T).cast(),
                    size_of_val(value),
                    null_mut(),
                    null(),
                )
            },
            &format!("UpdateProcThreadAttribute({attribute:#x})"),
        )
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(arg: &str) -> String {
        let mut command = Vec::new();
        push_argument(&mut command, OsStr::new(arg));
        String::from_utf16(&command).unwrap()
    }

    #[test]
    fn arguments_are_quoted_for_the_c_runtime_parser() {
        assert_eq!(
            quoted("--package-sid=S-1-15-2-1"),
            "--package-sid=S-1-15-2-1"
        );
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("a b"), "\"a b\"");
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        assert_eq!(quoted("C:\\dir name\\"), "\"C:\\dir name\\\\\"");
        assert_eq!(quoted("C:\\dir\\x"), "C:\\dir\\x");
    }

    #[test]
    fn the_environment_holds_only_the_named_variables_sorted() {
        let block = context::environment_block("C:\\Windows", "C:\\T\\w");
        let text = String::from_utf16(&block).unwrap();
        let variables: Vec<&str> = text.trim_end_matches('\0').split('\0').collect();
        assert_eq!(
            variables,
            [
                "LOCALAPPDATA=C:\\T\\w",
                "PATH=C:\\Windows\\System32",
                "SYSTEMDRIVE=C:",
                "SYSTEMROOT=C:\\Windows",
                "TEMP=C:\\T\\w",
                "TMP=C:\\T\\w",
                "windir=C:\\Windows",
            ]
        );
        assert!(block.ends_with(&[0, 0]));
    }

    #[test]
    fn the_mitigation_policy_is_the_eight_always_on_bits() {
        assert_eq!(MITIGATION_POLICY, 0x1110_1011_1100_0000);
    }
}
