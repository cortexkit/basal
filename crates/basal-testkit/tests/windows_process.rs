#![cfg(windows)]

use basal_testkit::command::Command;
use basal_testkit::harness::scratch;
use basal_testkit::process::{assert_reaped, output_until};
use basal_testkit::{WorkerProcess, worker_binary};
use std::path::Path;
use std::time::Duration;

fn pipe_tree(dir: &Path, hold_parent: bool) -> Command {
    let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let pid_file = dir.join("descendant.pid");
    let quoted_path = pid_file.display().to_string().replace('\'', "''");
    // Start-Process without redirection inherits the parent's standard pipes.
    // The child publishes its pid before waiting, so a missing job kill would
    // leave both a live process and a pipe reader that cannot finish.
    let script = format!(
        "$p = Start-Process -PassThru -NoNewWindow -FilePath '{}' -ArgumentList '-NoProfile','-Command','\"[IO.File]::WriteAllText(''{}'', [string]$PID); Start-Sleep -Seconds 600\"'; while (!(Test-Path '{}')) {{ Start-Sleep -Milliseconds 10 }}; {}",
        powershell.display(),
        quoted_path,
        quoted_path,
        if hold_parent {
            "Start-Sleep -Seconds 600"
        } else {
            "exit 0"
        },
    );
    let mut command = Command::new(powershell);
    command
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(script);
    command
}

fn assert_descendant_dead(dir: &Path) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };
    let pid: u32 = std::fs::read_to_string(dir.join("descendant.pid"))
        .expect("descendant readiness")
        .trim()
        .parse()
        .unwrap();
    // SYNCHRONIZE is a standard access right, not a process-specific flag.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid) };
    if handle.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(87),
            "only an exited, released process may disappear"
        );
        return;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    assert_eq!(
        unsafe { WaitForSingleObject(handle.as_raw_handle(), 30_000) },
        WAIT_OBJECT_0,
        "descendant survived job cleanup"
    );
}

#[test]
fn subprocess_deadline_reaps_descendants_holding_output_pipes() {
    let dir = scratch("windows-pipe-deadline");
    let mut command = pipe_tree(&dir, true);
    let error = output_until(&mut command, Duration::from_secs(10)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_descendant_dead(&dir);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn subprocess_exit_reaps_descendants_holding_output_pipes() {
    let dir = scratch("windows-pipe-exit");
    let output = output_until(&mut pipe_tree(&dir, false), Duration::from_secs(30)).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_descendant_dead(&dir);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn worker_memory_sampling_and_reaping_observe_native_process_state() {
    let (mut worker, _) = WorkerProcess::start(&worker_binary(), Duration::from_secs(60)).unwrap();
    let pid = worker.pid();
    assert!(worker.rss_kib().is_some_and(|kib| kib > 0));
    assert!(
        std::panic::catch_unwind(|| assert_reaped(pid)).is_err(),
        "a running worker is not reaped"
    );
    worker.close_stdin();
    assert!(worker.wait_exit(Duration::from_secs(60)).unwrap().success());
    assert_reaped(pid);
    drop(worker);
    assert_reaped(pid);
}

#[test]
fn discovered_worker_launches_with_full_confinement_without_an_extra_acl_grant() {
    use basal_proto::{
        Confinement, PROTOCOL_VERSION, ParentMessage, WorkerMessage, encode_parent_frame,
        read_worker_message,
    };
    use std::io::Write;
    let binary = worker_binary();
    assert_eq!(binary, basal_testkit::channel::worker_binary());
    assert!(
        binary
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ckdev-")
    );
    let options =
        basal_launch::LaunchOptions::new(&binary, basal_proto::limits::FLOW_JOB_COMMIT_BYTES);
    let mut child = basal_launch::launch(&options).unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            &encode_parent_frame(&ParentMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
            .unwrap(),
        )
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = tx.send(read_worker_message(&mut stdout));
    });
    let frame = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("handshake before hang deadline")
        .unwrap();
    let WorkerMessage::Welcome(welcome) = frame else {
        panic!("expected welcome, got {frame:?}");
    };
    assert_eq!(
        welcome.confinement,
        Confinement::Windows {
            lpac: true,
            untrusted: true,
            no_thread_token: true,
            mitigations: true,
            handle_table: true
        }
    );
    child.stdin.take();
    assert_eq!(child.wait().unwrap(), 0);
    reader.join().unwrap();
}
