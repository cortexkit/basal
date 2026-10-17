//! End to end with the real module process (`ck-basal-harness`: the module
//! against wire-shaped fake providers or mock hosts over stdio), the real
//! worker, and real kills.
//!
//! A schedule flow is installed and its card approved through the mock
//! consent; the clock jumps over a sleep gap so a catch-up fires; the
//! worker is killed mid-activation; the module process is killed with
//! SIGKILL between a sink write's effect and the commit of its outcome; and
//! after a restart the flow's sink writes have happened exactly once each.
//! A second test cuts the store under a running activation: the process
//! exits non-zero and the restart recovers the run.
//! On macOS a separate case drives the production `ck-basal` startup against
//! a wire daemon and measures a real worker's responsible-process identity.

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use basal_module::fatal::EXIT_STORE_FAILURE;
use basal_testkit::channel::worker_binary;
#[cfg(feature = "rig-kill-hook")]
use basal_testkit::git::git_command;
use serde_json::{Value, json};

/// 2026-05-01T00:00:00Z, the harness's starting clock.
const T0: i64 = 1_777_593_600_000;
const HOUR: i64 = 3_600_000;

#[cfg(unix)]
#[path = "common/binary_staging.rs"]
mod binary_staging;

#[cfg(unix)]
const PROBE_MODULE: &str = "BASAL_E2E_PRIVACY_MODULE_BINARY";
#[cfg(unix)]
const PROBE_WORKER: &str = "BASAL_E2E_PRIVACY_WORKER_BINARY";

#[cfg(unix)]
fn placed_probe_binaries() -> (PathBuf, PathBuf, PathBuf) {
    let module = basal_testkit::dev_binary(env!("CARGO_BIN_EXE_ck-basal"));
    let worker = worker_binary();
    let (module, worker) = binary_staging::stage_module_worker(&module, &worker);
    let executable = std::env::current_exe().expect("test executable");
    // The probe is also an executable, so stage it before giving it its own
    // macOS privacy identity. Its build output may live on a removable volume.
    let probe = cortexkit_test_support::ckdev_binary(executable);
    (probe, module, worker)
}

#[cfg(unix)]
fn probe_fixture_binaries() -> (PathBuf, PathBuf) {
    // These are placed paths supplied by the parent, not paths rediscovered
    // from Cargo or from the copied test executable's directory.
    let module = std::env::var_os(PROBE_MODULE).expect("parent supplied copied module");
    let worker = std::env::var_os(PROBE_WORKER).expect("parent supplied copied worker");
    (PathBuf::from(module), PathBuf::from(worker))
}

// Poll process completion only; responsibility observations are never retried.
// Retain the Child so expiry can kill and reap it instead of detaching a wait.
fn wait_for_exit(child: &mut Child, deadline: Instant) -> std::io::Result<ExitStatus> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            child.kill()?;
            child.wait()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "child exit deadline elapsed; child killed and reaped",
            ));
        };
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

// A descendant can keep a pipe open after the direct child exits. Bound reader
// cleanup as well, rather than letting join hide a stalled process indefinitely.
fn join_reader_until<T>(
    reader: std::thread::JoinHandle<T>,
    deadline: Instant,
) -> Result<T, &'static str> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err("reader deadline elapsed after child exit");
        };
        if reader.is_finished() {
            return reader.join().map_err(|_| "reader thread panicked");
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(target_os = "macos")]
mod privacy {
    use super::*;
    use std::time::Duration;
    use subc_protocol::{Flags, Frame, FrameType, PROTOCOL_VERSION, Priority};
    use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
    use subc_transport::{authenticate_server, read_frame, write_frame};

    struct ReapedChild(Child);

    impl Drop for ReapedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct Directory(PathBuf);

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn responsible_pid(pid: u32) -> u32 {
        // SAFETY: Darwin exports this fixed private ABI from libSystem. The
        // function accepts a pid, retains no pointers and stays loaded for the
        // process lifetime. Measuring live children avoids stale pid reuse.
        unsafe {
            let address = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            );
            assert!(!address.is_null(), "responsibility probe unavailable");
            let probe: unsafe extern "C" fn(libc::pid_t) -> libc::pid_t =
                std::mem::transmute(address);
            let result = probe(pid as libc::pid_t);
            assert!(
                result > 0,
                "responsibility lookup failed for {pid}: {result}"
            );
            result as u32
        }
    }

    fn runs_image(pid: u32, image: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer remains live and its size is given to libproc.
        let len =
            unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
        len > 0
            && std::ffi::CStr::from_bytes_until_nul(&path)
                .ok()
                .and_then(|p| p.to_str().ok())
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.dev() == image.dev() && m.ino() == image.ino())
    }

    async fn cat_exec_barrier(control: &mut ReapedChild) {
        // Spawn success is not an application-readiness acknowledgment. An echo
        // proves that cat has reached userspace after exec and initialization;
        // matching the image also excludes a launcher answering in its place.
        control
            .0
            .stdin
            .as_mut()
            .expect("control stdin")
            .write_all(b"R")
            .expect("request control exec acknowledgment");
        let mut stdout = control.0.stdout.take().expect("control stdout");
        let stdout = tokio::time::timeout(
            Duration::from_secs(300),
            tokio::task::spawn_blocking(move || {
                let mut echoed = [0u8; 1];
                stdout
                    .read_exact(&mut echoed)
                    .expect("read control exec acknowledgment");
                assert_eq!(&echoed, b"R", "control exec acknowledgment");
                stdout
            }),
        )
        .await
        .expect("control exec acknowledgment deadline")
        .expect("control exec acknowledgment reader");
        control.0.stdout = Some(stdout);
        assert!(
            runs_image(
                control.0.id(),
                &std::fs::metadata("/bin/cat").expect("cat image")
            ),
            "control must be running the cat image after its acknowledgment"
        );
    }

    #[tokio::test]
    async fn plain_control_exec_barrier_drains_its_ack() {
        let mut control = ReapedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("plain control child"),
        );
        cat_exec_barrier(&mut control).await;
        // The next echo must be the new request, not an unread acknowledgment.
        // This observes the real pipe rather than a helper's claimed readiness.
        control
            .0
            .stdin
            .as_mut()
            .expect("control stdin")
            .write_all(b"P")
            .expect("probe request");
        let mut stdout = control.0.stdout.take().expect("control stdout");
        let echoed = tokio::time::timeout(
            Duration::from_secs(300),
            tokio::task::spawn_blocking(move || {
                let mut echoed = [0u8; 1];
                stdout.read_exact(&mut echoed).expect("probe echo");
                echoed
            }),
        )
        .await
        .expect("probe echo deadline")
        .expect("probe echo reader");
        assert_eq!(
            &echoed, b"P",
            "exec barrier must consume the acknowledgment before a responsibility probe"
        );
    }

    /// The module's direct children whose running image is the worker file,
    /// compared by device and inode. Stat both the expected worker and the image
    /// path reported for each child so matching is based on the executable file,
    /// not on a possibly aliased path string.
    fn worker_pids(parent: u32, worker: &Path) -> Vec<u32> {
        let target = std::fs::metadata(worker).expect("worker file");
        // libproc.h's parent-pid filter is not named by the libc crate.
        const PROC_PPID_ONLY: u32 = 6;
        let mut pids = [0i32; 32];
        // SAFETY: libproc writes at most the stated byte size into the live
        // buffer. PROC_PPID_ONLY selects only this module's direct children.
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_PPID_ONLY,
                parent,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as i32,
            )
        };
        assert!(bytes >= 0, "list module children");
        pids.into_iter()
            .take(bytes as usize / std::mem::size_of::<i32>())
            .filter(|pid| {
                // Verify the exec image, not just a trampoline pid:
                // until the trampoline execs the worker, the child is ck-basal.
                runs_image(*pid as u32, &target)
            })
            .map(|pid| pid as u32)
            .collect()
    }

    #[tokio::test]
    async fn production_worker_is_its_own_responsible_process() {
        // A self result alone does not establish a live responsibility originator:
        // after an inherited originator exits, both parent and plain child can
        // report themselves. Give the probe an explicit, live identity instead.
        // Keep it alive through all observations so a plain worker must inherit
        // this identity and cannot masquerade as a disclaimed worker.
        let (probe_binary, binary, worker) = placed_probe_binaries();
        let dir = Directory(scratch("privacy-originator"));
        std::fs::create_dir_all(&dir.0).expect("probe log directory");
        let log_path = dir.0.join("probe.log");
        let log = std::fs::File::create(&log_path).expect("probe log");
        let mut builder = subc_os::privacy_identity::DisclaimedCommand::new(&binary, &probe_binary);
        builder
            .args([
                "--exact",
                "privacy::worker_privacy_probe_with_live_originator",
                "--ignored",
                "--nocapture",
            ])
            .env(PROBE_MODULE, &binary)
            .env(PROBE_WORKER, &worker)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("probe stdout"))
            .stderr(log);
        let (mut command, confirmation) = builder.into_command().expect("disclaimed probe");
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut probe = ReapedChild(command.spawn().expect("start responsibility probe"));
        drop(command);
        let confirmed = confirmation
            .confirm(deadline)
            .map_err(|error| error.to_string());
        let outcome = confirmed.and_then(|()| {
            wait_for_exit(&mut probe.0, deadline).map_err(|error| error.to_string())
        });
        if outcome.is_err() {
            let _ = probe.0.kill();
            let _ = probe.0.wait();
        }
        let output = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|error| format!("cannot read probe log: {error}"));
        let diagnostics = if output.is_empty() {
            "anchored privacy probe: <empty log>".to_owned()
        } else {
            output
                .lines()
                .map(|line| format!("anchored privacy probe: {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        eprintln!("{diagnostics}");
        assert!(
            outcome.as_ref().is_ok_and(|status| status.success()),
            "responsibility probe {} failed: {outcome:?}; log {}:\n{diagnostics}",
            probe_binary.display(),
            log_path.display()
        );
    }

    #[tokio::test]
    #[ignore = "run by production_worker_is_its_own_responsible_process with an explicit originator"]
    async fn worker_privacy_probe_with_live_originator() {
        assert_eq!(responsible_pid(std::process::id()), std::process::id());
        let (binary, worker) = probe_fixture_binaries();
        let worker = worker.canonicalize().expect("worker path");
        // A control: a plainly launched child, held alive by its open stdin
        // rather than a timer. It must report its parent's responsible process,
        // so the measurement below can tell a disclaimed worker from a plain one.
        let mut control = ReapedChild(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("plain control child"),
        );
        cat_exec_barrier(&mut control).await;
        let parent_responsible = responsible_pid(std::process::id());
        let control_responsible = responsible_pid(control.0.id());
        assert_eq!(
            control_responsible,
            parent_responsible,
            "plain control inheritance: parent={} child={}",
            std::process::id(),
            control.0.id()
        );
        assert_ne!(responsible_pid(control.0.id()), control.0.id());

        let dir = Directory(scratch("privacy"));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listen");
        let address = listener.local_addr().expect("address");
        let file = dir.0.join("connection.json");
        let key = [0x5a; 32];
        let daemon_id = [0x2b; 16];
        write_atomic(
            &file,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: None,
                endpoints: vec![Endpoint {
                    host: address.ip().to_string(),
                    port: address.port(),
                }],
                key: key.to_vec(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "privacy-test".into(),
            },
        )
        .expect("connection file");
        let module = ReapedChild(
            Command::new(&binary)
                .args(["--subc".as_ref(), file.as_os_str()])
                .env_clear()
                .env("SUBC_MODULE_ID", "basal")
                .env("SUBC_CONNECTION_FILE", &file)
                .env("SUBC_LAUNCH_NONCE", "private-test-nonce")
                .env("XDG_DATA_HOME", &dir.0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("start real ck-basal"),
        );
        // Bound all socket operations and startup waits only to prevent hangs;
        // there is no assertion about how quickly startup completes.
        tokio::time::timeout(Duration::from_secs(300), async {
            let (mut consumer, _) = listener.accept().await.expect("host connection");
            authenticate_server(
                &mut consumer,
                &key,
                &daemon_id,
                "privacy-test",
                Duration::from_secs(30),
            )
            .await
            .expect("authenticate hosts");
            let (mut stream, _) = listener.accept().await.expect("module connection");
            authenticate_server(
                &mut stream,
                &key,
                &daemon_id,
                "privacy-test",
                Duration::from_secs(30),
            )
            .await
            .expect("authenticate module");
            let hello = read_frame(&mut stream)
                .await
                .expect("read hello")
                .expect("hello");
            assert_eq!(hello.header.ty, FrameType::Hello);
            let storage = cortexkit_store_types::StorageDescriptor {
                module_id: "basal".into(),
                storage_namespace: "core".into(),
                isolation: cortexkit_store_types::Isolation::Module,
                backend: cortexkit_store_types::StorageBackend::Sqlite {
                    path: dir.0.join("store.db").to_string_lossy().into_owned(),
                },
            };
            let ack = Frame::build(
                FrameType::HelloAck,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&json!({
                    "negotiated_ver": PROTOCOL_VERSION,
                    "subc_ops": [], "subc_capabilities": [],
                    "storage": storage, "machine_id": null
                }))
                .expect("encode ack"),
            )
            .expect("ack");
            write_frame(&mut stream, &ack).await.expect("write ack");
            let pids = loop {
                let pids = worker_pids(module.0.id(), &worker);
                if !pids.is_empty() {
                    break pids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            for pid in pids {
                let responsible = responsible_pid(pid);
                assert_eq!(
                    responsible, pid,
                    "worker must be its own responsible process"
                );
                assert_ne!(
                    responsible,
                    module.0.id(),
                    "worker must not borrow ck-basal's grants"
                );
                assert_ne!(
                    responsible,
                    responsible_pid(module.0.id()),
                    "worker must not borrow the module's responsible identity"
                );
            }
        })
        .await
        .expect("production worker startup deadline");
    }
}

struct Harness {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: mpsc::Receiver<String>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl Harness {
    fn start(dir: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(basal_testkit::dev_binary(env!(
            "CARGO_BIN_EXE_ck-basal-harness"
        )))
        .arg("--dir")
        .arg(dir)
        .arg("--worker")
        .arg(worker_binary())
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the harness");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let (tx, incoming) = mpsc::channel();
        let stdout_reader = std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // Drain the child continuously, but print its diagnostics from the
        // test thread so libtest captures them instead of corrupting status
        // lines consumed by mutation reports.
        let mut stderr = child.stderr.take().expect("stderr");
        let stderr = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).expect("harness diagnostics");
            bytes
        });
        Self {
            child,
            stdin: Some(stdin),
            stdout: incoming,
            stdout_reader: Some(stdout_reader),
            stderr: Some(stderr),
        }
    }

    /// Sends a command; `None` when the process died before answering.
    fn send(&mut self, command: Value) -> Option<Value> {
        let stdin = self.stdin.as_mut()?;
        writeln!(stdin, "{command}").ok()?;
        stdin.flush().ok()?;
        let line = self.stdout.recv_timeout(Duration::from_secs(60)).ok()?;
        serde_json::from_str(&line).ok()
    }

    fn ok(&mut self, command: Value) -> Value {
        let reply = self.send(command.clone()).expect("the harness answered");
        assert_eq!(reply["ok"], true, "{command} -> {reply:#}");
        reply["result"].clone()
    }

    fn wait(mut self) -> ExitStatus {
        drop(self.stdin.take());
        let status = wait_for_exit(&mut self.child, Instant::now() + Duration::from_secs(60))
            .expect("harness exit deadline");
        let stderr = join_reader_until(
            self.stderr.take().unwrap(),
            Instant::now() + Duration::from_secs(60),
        )
        .expect("harness diagnostics thread");
        if !stderr.is_empty() {
            eprintln!("{}", String::from_utf8_lossy(&stderr));
        }
        status
    }

    fn quit(mut self) {
        self.ok(json!({ "cmd": "quit" }));
        let status = self.wait();
        assert!(status.success(), "{status:?}");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut errors = Vec::new();
        if let Some(reader) = self.stdout_reader.take()
            && let Err(error) = join_reader_until(reader, deadline)
        {
            errors.push(format!("stdout: {error}"));
        }
        if let Some(reader) = self.stderr.take()
            && let Err(error) = join_reader_until(reader, deadline)
        {
            errors.push(format!("stderr: {error}"));
        }
        if !errors.is_empty() {
            if std::thread::panicking() {
                eprintln!("harness reader cleanup: {errors:?}");
            } else {
                panic!("harness reader cleanup: {errors:?}");
            }
        }
    }
}

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> PathBuf {
    basal_testkit::harness::scratch(&format!("e2e-{tag}"))
}

#[cfg(unix)]
#[test]
fn copied_test_executable_accepts_exact_ignored_arguments() {
    let original = std::env::current_exe().expect("test executable");
    let (executable, module, worker) = placed_probe_binaries();
    let expected_name = format!("ckdev-{}", original.file_name().unwrap().to_string_lossy());
    assert_eq!(
        executable.file_name().unwrap().to_string_lossy(),
        expected_name
    );
    assert_ne!(executable, original);
    let dir = Directory(scratch("copied-libtest"));
    let log_path = dir.0.join("libtest.log");
    let log = std::fs::File::create(&log_path).expect("copied libtest log");
    let mut child = Command::new(&executable)
        .args([
            "--exact",
            "copied_test_executable_entrypoint",
            "--ignored",
            "--nocapture",
        ])
        .env(
            "BASAL_E2E_EXPECTED_PROBE",
            executable.canonicalize().unwrap(),
        )
        .env(PROBE_MODULE, module)
        .env(PROBE_WORKER, worker)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("copied libtest executable");
    let outcome = wait_for_exit(&mut child, Instant::now() + Duration::from_secs(300));
    let output = std::fs::read_to_string(log_path).expect("copied libtest output");
    assert!(
        outcome.as_ref().is_ok_and(|status| status.success()),
        "copied test {} failed: {outcome:?}; log:\n{output}",
        executable.display()
    );
    assert!(output.contains("copied e2e entrypoint reached"), "{output}");
}

#[cfg(unix)]
#[test]
#[ignore = "selected by copied_test_executable_accepts_exact_ignored_arguments"]
fn copied_test_executable_entrypoint() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let expected = std::env::var_os("BASAL_E2E_EXPECTED_PROBE").expect("expected copy path");
    assert_eq!(executable, PathBuf::from(expected));
    let (module, worker) = probe_fixture_binaries();
    assert_eq!(
        module.parent(),
        worker.parent(),
        "the production module locates its worker beside its own executable"
    );
    for binary in [module, worker] {
        let binary = binary.canonicalize().expect("placed fixture binary");
        assert_ne!(binary, executable);
        assert!(
            binary
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ckdev-")
        );
        assert!(binary.metadata().unwrap().len() > 0);
    }
    println!("copied e2e entrypoint reached");
}

#[cfg(unix)]
#[test]
fn child_exit_deadline_kills_and_reaps_a_stalled_child() {
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("stalled child");
    let pid = child.id() as libc::pid_t;
    let error = wait_for_exit(&mut child, Instant::now()).expect_err("live child must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let mut status = 0;
    // SAFETY: this test owns the child and the status buffer is valid.
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn reader_deadline_does_not_wait_for_an_open_pipe() {
    let (mut read, write) = std::io::pipe().expect("reader pipe");
    let reader = std::thread::spawn(move || read.read(&mut [0u8; 1]).expect("pipe read"));
    let error = join_reader_until(reader, Instant::now()).expect_err("reader must time out");
    drop(write);
    assert_eq!(error, "reader deadline elapsed after child exit");
}

/// Whether the module ended the way an outside kill ends it. On Unix that is
/// SIGKILL either way. On Windows a module that killed itself
/// (`fatal::kill_self`) exits with the launcher's kill code, and one killed
/// with `Child::kill` exits with 1, the code the standard library
/// terminates with.
fn assert_killed(status: ExitStatus, killed_itself: bool) {
    #[cfg(unix)]
    {
        let _ = killed_itself;
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }
    #[cfg(windows)]
    {
        let code = if killed_itself {
            basal_launch::KILL_EXIT_CODE as i32
        } else {
            1
        };
        assert_eq!(status.code(), Some(code), "{status:?}");
    }
}

// Reaping is a POSIX notion: a dropped child that was not waited for stays
// a zombie. A Windows process handle is simply closed.
#[cfg(unix)]
#[test]
fn e2e_fixture_drop_reaps_its_child() {
    let dir = scratch("drop-reaps");
    let _directory = Directory(dir.clone());
    let h = Harness::start(&dir, &[]);
    let pid: i32 = h.child.id().try_into().unwrap();
    drop(h);
    let mut status = 0;
    // SAFETY: this child was created by this test; the status pointer is valid.
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if waited == 0 {
        // The child is still running, so dropping the fixture did not reap
        // it. Kill and reap it here, so the failing assertion below does not
        // leave a stray process behind.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, &mut status, 0);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(waited, -1, "dropping the fixture left an unreaped child");
}

const SCRIPT: &str = "const before = (await kv.get('fires')) ?? 0;\n\
const echo = await ops.call('mock', 'echo', { due: trigger.due });\n\
await sink.digest('SYNAPSE', { title: 'fire', body: trigger.due }, 'piggyback');\n\
await kv.set('fires', before + 1);\n\
return { due: echo.due, before, missed: trigger.catch_up ? trigger.catch_up.missed_count : 0 };";

fn manifest(id: &str, schedule: Value) -> String {
    json!({
        "id": id,
        "version": 1,
        "purpose": "Tell SYNAPSE every hour.",
        "trigger": { "schedule": schedule },
        "sinks": [ { "agent": "SYNAPSE", "digest_max": "piggyback" } ],
        "ops": [ { "module": "mock", "op": "echo" } ]
    })
    .to_string()
}

/// Installs as SYNAPSE and approves the card as the operator.
fn install_and_approve(h: &mut Harness, id: &str, schedule: Value) {
    let reply = h.ok(json!({
        "cmd": "op",
        "as": { "agent": "SYNAPSE" },
        "method": "flow.install",
        "params": { "script": SCRIPT, "manifest": manifest(id, schedule) },
    }));
    assert_eq!(reply["state"], "pending");
    let cards = h.ok(json!({ "cmd": "cards" }));
    assert_eq!(cards[0]["card_id"], reply["card_id"]);
    h.ok(
        json!({ "cmd": "decide", "card_id": reply["card_id"], "approve": true, "by": "operator" }),
    );
    let health = h.ok(json!({ "cmd": "op", "as": "operator", "method": "flow.health" }));
    assert_eq!(health["flows"][0]["approved_version"], 1, "{health:#}");
}

fn digests(effects: &Value) -> Vec<Value> {
    effects
        .as_array()
        .expect("effects")
        .iter()
        .filter(|e| e["op"] == "sink.digest")
        .cloned()
        .collect()
}

/// Builds a fixture repository with one commit.
#[cfg(feature = "rig-kill-hook")]
fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    std::fs::write(dir.join("CHANGELOG.md"), "v1\n").expect("file");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "CHANGELOG.md"],
        &["commit", "-q", "-m", "release v1"],
    ] {
        let mut command = git_command();
        command.current_dir(dir).args(args);
        let output = basal_testkit::process::output_until(&mut command, Duration::from_secs(60))
            .expect("git fixture command deadline");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// A schedule-triggered flow, through the real module process and worker,
/// reads a file and a git log, fetches from a local HTTPS server and writes
/// a digest of all three through the mock core.
// Local HTTPS fixtures require network exemptions excluded from production.
#[cfg(feature = "rig-kill-hook")]
#[test]
fn a_schedule_flow_reads_a_file_and_a_git_log_fetches_and_writes_a_digest() {
    use basal_testkit::https::{Reply, TestServer};

    let dir = scratch("builtins");
    let _directory = Directory(dir.clone());
    let files = dir.join("files");
    std::fs::create_dir_all(&files).expect("files");
    std::fs::write(files.join("watch.txt"), "watched text").expect("file");
    let repo = dir.join("repo");
    git_repo(&repo);
    let server = TestServer::start(|_| {
        [("/release".to_owned(), Reply::text(200, "release 1.2.3"))]
            .into_iter()
            .collect()
    });
    let trust = dir.join("trust.der");
    std::fs::write(&trust, server.trust_der()).expect("trust");
    let resolve = format!("api.test={}", server.addr());
    let harness_args = [
        "--routing-fake",
        "--builtins-trust-der",
        trust.to_str().expect("utf-8"),
        "--builtins-resolve",
        &resolve,
    ];

    let script = format!(
        "const file = await fs.read({file:?});\n\
         const log = await git.log({repo:?}, {{ maxCount: 1 }});\n\
         const page = await net.fetch({url:?});\n\
         const body = file.text + ' | ' + log[0].subject + ' | ' + page.body;\n\
         await sink.digest('SYNAPSE', {{ title: 'builtins', body }}, 'piggyback');\n\
         return {{ body, status: page.status }};",
        file = files.join("watch.txt").display().to_string(),
        repo = repo.display().to_string(),
        // The test's resolver (`--builtins-resolve`) maps the approved
        // HTTPS URL's host, api.test on port 443, to the local test server.
        url = "https://api.test/release",
    );
    let manifest = json!({
        "id": "flow-builtins",
        "version": 1,
        "purpose": "Digest a file, a repository and a release page.",
        "trigger": { "schedule": { "cron": "0 * * * *", "missed": "once" } },
        "sinks": [ { "agent": "SYNAPSE", "digest_max": "piggyback" } ],
        "fs": { "read": [files.display().to_string()] },
        "git": { "read": [repo.display().to_string()] },
        "net": { "fetch": [ { "host": "api.test" } ] }
    })
    .to_string();

    let mut h = Harness::start(&dir.join("module"), &harness_args);
    let reply = h.ok(json!({
        "cmd": "op",
        "as": { "agent": "SYNAPSE" },
        "method": "flow.install",
        "params": { "script": script, "manifest": manifest },
    }));
    assert_eq!(reply["state"], "pending", "{reply:#}");
    let cards = h.ok(json!({ "cmd": "cards" }));
    let card = &cards[0];
    assert_eq!(card["card_id"], reply["card_id"]);
    // The card shows each built-in line, the default methods spelt out.
    assert_eq!(
        card["fields"]["fs"],
        json!({ "read": [files.display().to_string()], "write": [] })
    );
    assert_eq!(
        card["fields"]["git"],
        json!({ "read": [repo.display().to_string()] })
    );
    assert_eq!(
        card["fields"]["net"],
        json!({ "fetch": [{ "host": "api.test", "methods": ["GET", "HEAD"] }] })
    );
    h.ok(
        json!({ "cmd": "decide", "card_id": reply["card_id"], "approve": true, "by": "operator" }),
    );
    h.ok(json!({ "cmd": "clock", "set_ms": T0 + HOUR + 10_000 }));
    h.ok(json!({ "cmd": "pump" }));

    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(runs.as_array().map(Vec::len), Some(1), "{runs:#}");
    assert_eq!(runs[0]["state"], "succeeded", "{runs:#}");
    let result: Value =
        serde_json::from_str(runs[0]["result"].as_str().expect("result")).expect("json");
    let body = "watched text | release v1 | release 1.2.3";
    assert_eq!(result, json!({ "body": body, "status": 200 }));
    let effects = h.ok(json!({ "cmd": "effects" }));
    let writes = digests(&effects);
    assert_eq!(writes.len(), 1, "{effects:#}");
    assert!(
        writes[0]["args"].as_str().unwrap_or("").contains(body),
        "{effects:#}"
    );
    assert_eq!(server.seen().len(), 1, "fetched once");
    h.quit();
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_schedule_flow_survives_a_catch_up_a_worker_kill_and_a_module_kill_with_each_write_once() {
    let dir = scratch("catch-up");
    let _directory = Directory(dir.clone());

    // Install, approve, and sleep through five due times.
    let mut h = Harness::start(&dir, &["--routing-fake"]);
    install_and_approve(
        &mut h,
        "flow-hourly",
        json!({ "cron": "0 * * * *", "missed": "once" }),
    );
    h.ok(json!({ "cmd": "pump" }));
    assert_eq!(
        h.ok(json!({ "cmd": "runs" })),
        json!([]),
        "nothing due before 01:00"
    );
    h.ok(json!({ "cmd": "clock", "set_ms": T0 + 5 * HOUR + 10_000 }));
    h.quit();

    // The catch-up runs. Its worker is killed while the script waits on its
    // echo; on another worker it writes the digest, and the module is killed
    // after the sink applied the write and before its outcome is committed.
    let mut h = Harness::start(
        &dir,
        &[
            "--routing-fake",
            "--kill-worker-at",
            "CallCommitted { position: 1 }#1",
            "--kill-self-at",
            "HostAnswered { position: 2 }#1",
        ],
    );
    assert!(
        h.send(json!({ "cmd": "pump" })).is_none(),
        "the module died mid-run"
    );
    assert_killed(h.wait(), true);

    // The restart recovers the run and finishes both fires.
    let mut h = Harness::start(&dir, &["--routing-fake"]);
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(runs.as_array().map(Vec::len), Some(2), "{runs:#}");
    assert_eq!(
        runs[0]["state"], "pending",
        "recovered from running: {runs:#}"
    );
    h.ok(json!({ "cmd": "pump" }));
    let runs = h.ok(json!({ "cmd": "runs" }));
    let ids: Vec<&str> = runs
        .as_array()
        .expect("runs")
        .iter()
        .map(|r| r["trigger_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ids,
        [
            "schedule:2026-05-01T04:00:00Z",
            "schedule:2026-05-01T05:00:00Z"
        ],
        "one catch-up fire for 01:00 to 04:00, then the on-time 05:00"
    );
    for run in runs.as_array().expect("runs") {
        assert_eq!(run["state"], "succeeded", "{runs:#}");
    }
    assert_eq!(runs[0]["broken"], 1, "the first run lost one worker");
    let first: Value =
        serde_json::from_str(runs[0]["result"].as_str().expect("result")).expect("json");
    let second: Value =
        serde_json::from_str(runs[1]["result"].as_str().expect("result")).expect("json");
    assert_eq!(
        first,
        json!({ "due": "2026-05-01T04:00:00Z", "before": 0, "missed": 4 })
    );
    assert_eq!(
        second,
        json!({ "due": "2026-05-01T05:00:00Z", "before": 1, "missed": 0 })
    );

    let effects = h.ok(json!({ "cmd": "effects" }));
    let writes = digests(&effects);
    assert_eq!(writes.len(), 2, "one digest write per fire: {effects:#}");
    assert_ne!(writes[0]["key"], writes[1]["key"]);
    assert!(
        writes[0]["args"]
            .as_str()
            .unwrap_or("")
            .contains("2026-05-01T04:00:00Z")
    );
    assert!(
        writes[1]["args"]
            .as_str()
            .unwrap_or("")
            .contains("2026-05-01T05:00:00Z")
    );
    // The write whose outcome the kill lost was sent again under its key
    // and applied once.
    assert_eq!(writes[0]["sends"], 2, "{effects:#}");
    assert_eq!(writes[1]["sends"], 1);
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_store_error_ends_the_process_non_zero_and_the_restart_recovers() {
    let dir = scratch("store-error");
    let _directory = Directory(dir.clone());
    let mut h = Harness::start(&dir, &["--cut-store-at", "CallCommitted { position: 1 }#1"]);
    install_and_approve(&mut h, "flow-cut", json!({ "interval": "1h" }));
    h.ok(json!({ "cmd": "clock", "set_ms": T0 + HOUR + 10_000 }));
    // The store fails under the run's second call: the module exits rather
    // than serve on with the run stuck in running.
    let _ = h.send(json!({ "cmd": "pump" }));
    let status = h.wait();
    assert_eq!(status.code(), Some(EXIT_STORE_FAILURE), "{status:?}");

    let mut h = Harness::start(&dir, &[]);
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(
        runs[0]["state"], "pending",
        "recovery put the run back: {runs:#}"
    );
    h.ok(json!({ "cmd": "pump" }));
    let runs = h.ok(json!({ "cmd": "runs" }));
    assert_eq!(runs.as_array().map(Vec::len), Some(1));
    assert_eq!(runs[0]["state"], "succeeded", "{runs:#}");
    let effects = h.ok(json!({ "cmd": "effects" }));
    assert_eq!(digests(&effects).len(), 1, "{effects:#}");
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_broca_llm_suspends_survives_module_kill_and_settles_after_restart() {
    let dir = scratch("broca-restart");
    let _directory = Directory(dir.clone());
    let mut h = Harness::start(&dir, &["--broca"]);
    let manifest = json!({
        "id": "flow-model", "version": 1, "purpose": "Obtain one model answer.",
        "trigger": {"schedule": {"cron": "0 * * * *", "missed": "once"}},
        "llm": {"iq": 0, "token_cap": {"tokens": 10000, "window": "1h"}, "max_output": 32}
    })
    .to_string();
    let installed = h.ok(json!({"cmd":"op","as":{"agent":"SYNAPSE"},"method":"flow.install","params":{"manifest":manifest,"script":"return await llm({prompt:'hello',max_output:999});"}}));
    h.ok(json!({"cmd":"decide","card_id":installed["card_id"],"approve":true,"by":"operator"}));
    h.ok(json!({"cmd":"clock","set_ms":T0+HOUR}));
    h.ok(json!({"cmd":"pump"}));
    let before = h.ok(json!({"cmd":"runs"}));
    assert_eq!(before[0]["state"], "suspended", "{before:#}");
    let sends = h.ok(json!({"cmd":"broca_sends"}));
    assert_eq!(sends.as_array().unwrap().len(), 1);
    assert_eq!(sends[0]["params"]["tools"], json!([]));
    assert_eq!(sends[0]["params"]["generation"]["max_output_tokens"], 32);
    h.child.kill().expect("kill the module");
    assert_killed(h.wait(), false);

    let mut h = Harness::start(&dir, &["--broca"]);
    assert_eq!(h.ok(json!({"cmd":"runs"}))[0]["state"], "suspended");
    h.ok(json!({"cmd":"broca_finish","text":"after restart","usage":{"input_tokens":7,"cache_write_tokens":2,"output_tokens":5,"cached_input_tokens":100,"reasoning_tokens":3}}));
    h.ok(json!({"cmd":"pump"}));
    let runs = h.ok(json!({"cmd":"runs"}));
    assert_eq!(runs[0]["state"], "succeeded", "{runs:#}");
    let result: Value = serde_json::from_str(runs[0]["result"].as_str().unwrap()).unwrap();
    assert_eq!(result, json!({"text":"after restart"}));
    assert_eq!(
        h.ok(json!({"cmd":"broca_sends"})).as_array().unwrap().len(),
        1
    );
    h.quit();
    let c = rusqlite::Connection::open(dir.join("basal.db")).unwrap();
    let row: (String,i64,i64,i64,i64) = c.query_row("SELECT state,input_tokens,cache_write_tokens,output_tokens,cached_input_tokens FROM token_ledger",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(row, ("settled".into(), 7, 2, 5, 100));
    let snapshot: String = c
        .query_row("SELECT snapshot FROM broca_calls", [], |r| r.get(0))
        .unwrap();
    let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(snapshot["acknowledged"], true);
    assert_eq!(snapshot["state"], "completed");
    assert!(snapshot.get("cursor").is_none() && snapshot.get("text").is_none());
    // The restarted module read the outcome with run.result.
    let fake: Value =
        serde_json::from_slice(&std::fs::read(dir.join("broca.json")).unwrap()).unwrap();
    assert!(fake["result_calls"].as_u64().unwrap() >= 1, "{fake:#}");
}
