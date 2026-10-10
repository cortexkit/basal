//! Unconfined children: every Windows process basal starts that is not a
//! confined worker (test fixtures, helper tools) goes through here, so that
//! it is created under the same spawn lock, inherits only its own standard
//! handles, and is born in a job that ends it with its owner.

use super::error::{LaunchError, Refusal};
use super::job;
use super::launch::{AttributeList, pipe, push_argument};
use super::native::{Result, check, owned, wide};
use super::process::{KILL_EXIT_CODE, OwnedProcess};
use super::spawn_lock::{make_inheritable, spawn_lock};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, WaitForSingleObject,
};

/// The longest command line `CreateProcessW` accepts, in UTF-16 units,
/// including the terminating NUL.
const MAX_COMMAND_LINE: usize = 32_767;

/// Where one of the child's standard handles goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainStdio {
    /// A new anonymous pipe; the parent's end is on the [`OwnedProcess`].
    Piped,
    /// The `NUL` device: reads see end of file, writes are discarded.
    Null,
}

/// An unconfined child to start with [`spawn_plain`].
#[derive(Debug, Clone)]
pub struct PlainCommand {
    /// The absolute path of the image; a relative path is refused.
    pub program: PathBuf,
    /// Arguments after the image name, encoded so the C runtime's parser
    /// reads each back unchanged.
    pub args: Vec<OsString>,
    /// The child's whole environment. Nothing of the parent's is added.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory; the parent's when unset.
    pub cwd: Option<PathBuf>,
    /// The child's stdin.
    pub stdin: PlainStdio,
    /// The child's stdout.
    pub stdout: PlainStdio,
    /// The child's stderr.
    pub stderr: PlainStdio,
}

impl PlainCommand {
    /// `program` with no arguments, an empty environment, the parent's
    /// working directory and every standard handle on `NUL`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            stdin: PlainStdio::Null,
            stdout: PlainStdio::Null,
            stderr: PlainStdio::Null,
        }
    }
}

/// Starts an unconfined child: no restricted token and no mitigations, but
/// the same handling of handles and lifetime as a confined worker.
///
/// - Its standard handles are made inheritable only under the spawn lock,
///   and they are the only entries of its inherited-handle list, so it
///   inherits nothing else the parent holds.
/// - It is created suspended in a fresh job whose only limit is
///   kill-on-close (so no breakaway), its membership of exactly that job is
///   verified, and only then is it resumed. A child that is not in the job
///   is killed before its first instruction.
/// - The returned [`OwnedProcess`] owns the process and the job: `kill`
///   ends the whole job, and dropping it kills the child.
pub fn spawn_plain(command: &PlainCommand) -> std::result::Result<OwnedProcess, LaunchError> {
    if !command.program.is_absolute() {
        return Err(LaunchError::Failed(format!(
            "the image path must be absolute: {}",
            command.program.display()
        )));
    }
    let application = wide_checked(&command.program, "image path")?;
    let mut command_line = command_line(&command.program, &command.args)?;
    let environment = environment_block(&command.env)?;
    let cwd = command
        .cwd
        .as_ref()
        .map(|cwd| wide_checked(cwd, "working directory"))
        .transpose()?;
    let job = job::create_kill_on_close()?;

    // Declared before the handles so that on an early return they close
    // before the lock is released.
    let spawn_lock = spawn_lock();
    let stdin = Stdio::open(command.stdin, Direction::ToChild)?;
    let stdout = Stdio::open(command.stdout, Direction::FromChild)?;
    let stderr = Stdio::open(command.stderr, Direction::FromChild)?;
    let inherited: [HANDLE; 3] = [
        stdin.child.as_raw_handle(),
        stdout.child.as_raw_handle(),
        stderr.child.as_raw_handle(),
    ];
    make_inheritable(&spawn_lock, &inherited)
        .map_err(|error| format!("SetHandleInformation(child stdio): {error}"))?;
    let jobs: [HANDLE; 1] = [job.as_raw_handle()];
    let mut attributes = AttributeList::new(2)?;
    attributes.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)?;
    attributes.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.as_ptr();
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    check(
        unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATION_FLAGS,
                environment.as_ptr().cast::<c_void>(),
                cwd.as_ref().map_or(null(), |cwd| cwd.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        },
        "CreateProcessW",
    )?;
    let created = Created {
        process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
        thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
    };
    #[cfg(test)]
    tests::observe(&created, &inherited);
    // The child holds its own copies now. Closing the parent's before the
    // lock is released ends the window in which another spawn could copy
    // them, and lets the pipes report end of file when the child exits.
    drop(attributes);
    let [stdin, stdout, stderr] = [stdin, stdout, stderr].map(Stdio::close_child);
    drop(spawn_lock);

    match job::contains(job.as_raw_handle(), created.process.as_raw_handle()) {
        Ok(true) => {}
        Ok(false) => {
            created.kill();
            return Err(LaunchError::refused(
                Refusal::NotInOwnedJob,
                "the suspended child is not in the job the parent created",
            ));
        }
        Err(error) => {
            created.kill();
            return Err(LaunchError::refused(Refusal::NotInOwnedJob, error));
        }
    }
    // A previous suspend count other than 1 means the thread was not
    // suspended exactly once by the creation, and may already have run or
    // may stay suspended.
    let previous = unsafe { ResumeThread(created.thread.as_raw_handle()) };
    if previous != 1 {
        let error = super::native::last("ResumeThread");
        created.kill();
        return Err(LaunchError::Failed(format!(
            "{error}; previous suspend count {previous}"
        )));
    }
    Ok(OwnedProcess::new(
        created.process,
        info.dwProcessId,
        job,
        stdin,
        stdout,
        stderr,
    ))
}

/// Suspended, with extended startup information for the attribute list, no
/// console window, and a UTF-16 environment block.
const CREATION_FLAGS: u32 =
    EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;

/// A created child before it is handed out.
struct Created {
    process: OwnedHandle,
    thread: OwnedHandle,
}

impl Created {
    fn kill(&self) {
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), KILL_EXIT_CODE);
            WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    ToChild,
    FromChild,
}

/// One standard handle: the child's end, and the parent's end of a pipe.
struct Stdio {
    child: OwnedHandle,
    parent: Option<File>,
}

impl Stdio {
    /// Closes the parent's copy of the child's end, keeping the parent's end.
    fn close_child(self) -> Option<File> {
        drop(self.child);
        self.parent
    }

    /// Neither end is inheritable when opened.
    fn open(kind: PlainStdio, direction: Direction) -> Result<Self> {
        match kind {
            PlainStdio::Piped => {
                let (read, write) = pipe()?;
                let (child, parent) = match direction {
                    Direction::ToChild => (read, write),
                    Direction::FromChild => (write, read),
                };
                Ok(Self {
                    child,
                    parent: Some(File::from(parent)),
                })
            }
            PlainStdio::Null => {
                let device = wide("NUL");
                let handle = unsafe {
                    CreateFileW(
                        device.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        null(),
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        null_mut(),
                    )
                };
                Ok(Self {
                    child: owned(handle, "CreateFileW(NUL)")?,
                    parent: None,
                })
            }
        }
    }
}

/// A NUL-terminated UTF-16 copy of a value that must not contain NUL.
fn wide_checked(value: impl AsRef<OsStr>, what: &str) -> Result<Vec<u16>> {
    let value = value.as_ref();
    if value.encode_wide().any(|unit| unit == 0) {
        return Err(format!("the {what} contains NUL"));
    }
    Ok(wide(value))
}

/// The quoted image path, then each argument encoded for the C runtime's
/// parser. The image path is quoted as is: the parser takes the first
/// argument up to the closing quote, and a path cannot contain a quote.
fn command_line(program: &Path, args: &[OsString]) -> Result<Vec<u16>> {
    let mut line: Vec<u16> = Vec::new();
    line.push(u16::from(b'"'));
    line.extend(program.as_os_str().encode_wide());
    line.push(u16::from(b'"'));
    for arg in args {
        if arg.encode_wide().any(|unit| unit == 0) {
            return Err("an argument contains NUL".to_owned());
        }
        line.push(u16::from(b' '));
        push_argument(&mut line, arg);
    }
    line.push(0);
    if line.len() > MAX_COMMAND_LINE {
        return Err(format!(
            "the command line is {} UTF-16 units, over the limit of {MAX_COMMAND_LINE}",
            line.len()
        ));
    }
    Ok(line)
}

/// The environment block: `NAME=value` strings sorted by upper-cased name,
/// as Windows keeps them, each NUL-terminated, then one more NUL. Names
/// compare case-insensitively, and a later entry replaces an earlier one
/// with the same name.
fn environment_block(env: &[(OsString, OsString)]) -> Result<Vec<u16>> {
    let mut sorted: BTreeMap<Vec<u16>, (Vec<u16>, Vec<u16>)> = BTreeMap::new();
    for (name, value) in env {
        let name: Vec<u16> = name.encode_wide().collect();
        let value: Vec<u16> = value.encode_wide().collect();
        // Windows itself keeps a few names that start with `=` (the
        // per-drive working directories), so only a later `=` is refused.
        if name.is_empty()
            || name.iter().skip(1).any(|&unit| unit == u16::from(b'='))
            || name.contains(&0)
            || value.contains(&0)
        {
            return Err(format!(
                "invalid environment entry {:?}",
                String::from_utf16_lossy(&name)
            ));
        }
        let key = name
            .iter()
            .map(|&unit| match u8::try_from(unit) {
                Ok(byte) => u16::from(byte.to_ascii_uppercase()),
                Err(_) => unit,
            })
            .collect();
        sorted.insert(key, (name, value));
    }
    let mut block = Vec::new();
    for (name, value) in sorted.into_values() {
        block.extend(name);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    // An empty block is two NULs: one for the absent first string, one to
    // end the block.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests;
