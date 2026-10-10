use super::*;
use crate::windows::spawn_lock::spawn_lock_held;
use std::cell::RefCell;
use std::io::Read;
use std::sync::mpsc;
use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INVALID_HANDLE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, STARTUPINFOW, SuspendThread};

type Observer = Box<dyn FnMut(&Created, &[HANDLE; 3])>;

thread_local! {
    /// Called by `spawn_plain` on this thread while the child is suspended
    /// and the parent still holds its copies of the child's handles.
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

pub(super) fn observe(created: &Created, inherited: &[HANDLE; 3]) {
    let observer = OBSERVER.with(|slot| slot.borrow_mut().take());
    if let Some(mut observer) = observer {
        observer(created, inherited);
    }
}

fn cmd() -> PathBuf {
    let root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
    Path::new(&root).join("System32").join("cmd.exe")
}

fn cmd_command(args: &[&str]) -> PlainCommand {
    let mut command = PlainCommand::new(cmd());
    command.args = args.iter().map(OsString::from).collect();
    command.env = vec![(
        "SystemRoot".into(),
        std::env::var_os("SystemRoot").expect("SystemRoot is set"),
    )];
    command
}

/// A `cmd.exe` that waits for a line on its piped stdin, so it stays alive
/// until it is killed.
fn waiting_cmd() -> PlainCommand {
    let mut command = cmd_command(&["/d", "/q", "/k"]);
    command.stdin = PlainStdio::Piped;
    command
}

/// Whether `process` holds the object the parent's `original` handle names.
/// An inherited handle keeps its value in the child, so the value is
/// duplicated out of the child and compared as an object, which also tells
/// apart an unrelated handle that happens to have the same value.
fn process_has_handle(process: HANDLE, original: HANDLE) -> bool {
    let mut duplicate = null_mut();
    if unsafe {
        DuplicateHandle(
            process,
            original,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_HANDLE as i32),
            "could not inspect the child's handle table"
        );
        return false;
    }
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    unsafe { CompareObjectHandles(original, duplicate.as_raw_handle()) != 0 }
}

/// Starts `cmd.exe` suspended the broad way: every inheritable handle, no
/// list. The caller kills it.
fn unlisted_suspended_cmd() -> Created {
    let application = wide(cmd());
    let mut line = wide(format!("\"{}\"", cmd().display()));
    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    check(
        unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                null(),
                null(),
                &startup,
                &mut info,
            )
        },
        "CreateProcessW(unlisted control)",
    )
    .unwrap();
    Created {
        process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
        thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    lock_held: bool,
    previous_suspend_count: u32,
    holds_listed: [bool; 3],
    holds_stray: bool,
}

/// While the child exists but has not run, the spawn lock is held, the main
/// thread is suspended exactly once, and the child holds its three listed
/// standard handles but not an unrelated inheritable handle of the parent.
/// The unlisted control shows that same handle really is inheritable.
#[test]
fn the_child_is_created_suspended_under_the_lock_inheriting_only_its_stdio() {
    let (_stray_read, stray_write) = pipe().unwrap();
    {
        // The control is a broad spawn, so it runs under the lock like any
        // other: it must copy only the handle it is testing.
        let lock = spawn_lock();
        make_inheritable(&lock, &[stray_write.as_raw_handle()]).unwrap();
        let control = unlisted_suspended_cmd();
        let inherited =
            process_has_handle(control.process.as_raw_handle(), stray_write.as_raw_handle());
        control.kill();
        assert!(inherited, "the unlisted control did not inherit the handle");
    }
    // The stray handle stays inheritable, as one opened by code that does
    // not take the lock would be.
    let stray = stray_write.as_raw_handle() as usize;
    let (sender, receiver) = mpsc::channel();
    OBSERVER.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |created: &Created, inherited| {
            let thread = created.thread.as_raw_handle();
            let previous_suspend_count = unsafe { SuspendThread(thread) };
            unsafe { ResumeThread(thread) };
            let process = created.process.as_raw_handle();
            let _ = sender.send(Observed {
                lock_held: spawn_lock_held(),
                previous_suspend_count,
                holds_listed: inherited.map(|handle| process_has_handle(process, handle)),
                holds_stray: process_has_handle(process, stray as HANDLE),
            });
        }));
    });
    let child = spawn_plain(&waiting_cmd()).expect("spawn cmd.exe");
    let observed = receiver.try_recv().expect("the observer ran");
    assert_eq!(
        observed,
        Observed {
            lock_held: true,
            previous_suspend_count: 1,
            holds_listed: [true; 3],
            holds_stray: false,
        }
    );
    assert!(!spawn_lock_held(), "the lock is released on return");
    assert!(
        !process_has_handle(child.as_raw_handle(), stray_write.as_raw_handle()),
        "the running child holds the unrelated handle"
    );
    child.kill().unwrap();
}

/// The child is in its own job, whose only limit is kill-on-close: no
/// breakaway flag, so the child cannot leave it.
#[test]
fn the_child_is_in_a_fresh_kill_on_close_job_without_breakaway() {
    let first = spawn_plain(&waiting_cmd()).expect("spawn cmd.exe");
    let second = spawn_plain(&waiting_cmd()).expect("spawn cmd.exe");
    assert!(first.in_owned_job().unwrap());
    let limits = first.job_limits().unwrap();
    assert_eq!(limits.flags, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    assert_eq!(limits.active_process_limit, 0);
    // Each spawn has a job of its own: killing the first one's job leaves
    // the second running.
    first.kill().unwrap();
    assert_eq!(first.wait().unwrap(), KILL_EXIT_CODE);
    assert_eq!(second.try_wait().unwrap(), None);
    assert!(second.in_owned_job().unwrap());
}

#[test]
fn kill_wait_and_try_wait_report_the_kill_code() {
    let child = spawn_plain(&waiting_cmd()).expect("spawn cmd.exe");
    assert_eq!(child.try_wait().unwrap(), None, "cmd.exe waits on stdin");
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap(), KILL_EXIT_CODE);
    assert_eq!(child.try_wait().unwrap(), Some(KILL_EXIT_CODE));
    child.kill().expect("killing an ended child succeeds");
}

#[test]
fn dropping_the_process_kills_the_child() {
    let child = spawn_plain(&waiting_cmd()).expect("spawn cmd.exe");
    let mut process = null_mut();
    check(
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                child.as_raw_handle(),
                GetCurrentProcess(),
                &mut process,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        },
        "DuplicateHandle(child)",
    )
    .unwrap();
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    assert_eq!(child.try_wait().unwrap(), None);
    drop(child);
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) },
        WAIT_OBJECT_0,
        "the child was still running after its owner was dropped"
    );
}

#[test]
fn piped_output_and_the_exit_code_reach_the_parent() {
    let mut command = cmd_command(&["/d", "/c", "echo", "hello&exit", "7"]);
    command.stdout = PlainStdio::Piped;
    let mut child = spawn_plain(&command).expect("spawn cmd.exe");
    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output.trim_end(), "hello");
    assert_eq!(child.wait().unwrap(), 7);
    assert!(child.stdin.is_none() && child.stderr.is_none());
}

/// The child sees the given variables and none of the parent's.
#[test]
fn the_environment_replaces_the_parents() {
    assert!(
        std::env::var_os("COMPUTERNAME").is_some(),
        "the parent's own variable this test looks for is set"
    );
    let mut command = cmd_command(&["/d", "/c", "set"]);
    command.env.push(("BASAL_PLAIN_PROBE".into(), "x y".into()));
    command.stdout = PlainStdio::Piped;
    let mut child = spawn_plain(&command).expect("spawn cmd.exe");
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(child.wait().unwrap(), 0);
    let lines: Vec<&str> = output.lines().collect();
    assert!(lines.contains(&"BASAL_PLAIN_PROBE=x y"), "{output}");
    assert!(
        !lines.iter().any(|line| line.starts_with("COMPUTERNAME=")),
        "{output}"
    );
}

#[test]
fn a_relative_image_path_or_a_nul_is_refused_before_anything_starts() {
    let error = spawn_plain(&PlainCommand::new("cmd.exe")).unwrap_err();
    assert!(error.to_string().contains("must be absolute"), "{error}");
    let mut command = cmd_command(&["/c", "exit"]);
    command.args.push(OsString::from("a\0b"));
    assert!(spawn_plain(&command).is_err());
    let mut command = cmd_command(&["/c", "exit"]);
    command.env.push(("A=B".into(), "c".into()));
    assert!(spawn_plain(&command).is_err());
}

#[test]
fn the_environment_block_is_sorted_and_later_names_replace_earlier_ones() {
    let block = environment_block(&[
        ("b".into(), "2".into()),
        ("A".into(), "1".into()),
        ("a".into(), "3".into()),
        ("=C:".into(), "C:\\".into()),
    ])
    .unwrap();
    assert_eq!(
        String::from_utf16(&block).unwrap(),
        "=C:=C:\\\0a=3\0b=2\0\0"
    );
    assert_eq!(environment_block(&[]).unwrap(), [0, 0]);
}

#[test]
fn the_command_line_quotes_the_image_and_encodes_each_argument() {
    let line = command_line(
        Path::new("C:\\Program Files\\x.exe"),
        &["a b".into(), "c".into(), "".into()],
    )
    .unwrap();
    assert_eq!(
        String::from_utf16(&line).unwrap(),
        "\"C:\\Program Files\\x.exe\" \"a b\" c \"\"\0"
    );
    let long = vec![OsString::from("x".repeat(MAX_COMMAND_LINE))];
    assert!(command_line(Path::new("C:\\x.exe"), &long).is_err());
}
