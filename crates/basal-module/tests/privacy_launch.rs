#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use basal_module::pool::PoolConfig;
use basal_module::process::{SpawnError, WorkerLaunch, WorkerProcess};

#[test]
fn real_trampoline_confirms_and_worker_completes_seatbelt_handshake() {
    let module = basal_testkit::dev_binary(env!("CARGO_BIN_EXE_ck-basal"));
    let worker = basal_testkit::worker_binary();
    let binaries = basal_testkit::DevBinaries::new(&[&module, &worker]).expect("development pair");
    let trampoline = binaries.path(&module).expect("development trampoline");
    subc_os::privacy_identity::probe(&trampoline, Instant::now() + Duration::from_secs(180))
        .expect("production trampoline probe before runtime startup");
    let worker = WorkerProcess::start(
        &binaries.path(&worker).expect("development worker"),
        &WorkerLaunch::Disclaimed { trampoline },
        Duration::from_secs(180),
    )
    .unwrap_or_else(|error| panic!("disclaimed handshake: {error}"));
    assert_eq!(
        worker.welcome().confinement,
        basal_proto::Confinement::Seatbelt
    );
}

#[test]
fn disclaimed_worker_closes_extra_inherited_descriptors_at_startup() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use subc_os::privacy_identity::DisclaimedCommand;

    let mut pipe = [-1; 2];
    // SAFETY: the live buffer has room for both returned descriptors.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    // SAFETY: a successful pipe returned two newly owned descriptors.
    let (_read, write) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let write_fd = write.as_raw_fd();
    let module = basal_testkit::dev_binary(env!("CARGO_BIN_EXE_ck-basal"));
    let worker = basal_testkit::worker_binary();
    let binaries = basal_testkit::DevBinaries::new(&[&module, &worker]).expect("development pair");
    let mut builder = DisclaimedCommand::new(
        binaries.path(&module).expect("development trampoline"),
        binaries.path(&worker).expect("development worker"),
    );
    builder
        .args(["--confinement-probe", "--no-sandbox"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut command, confirmation) = builder.into_command().expect("disclaimed command");
    // The trampoline replaces itself with the worker in place (posix_spawn with
    // SETEXEC), which keeps every descriptor not marked close-on-exec. Plant an
    // open pipe at fd 3 and fd 64 to stand for a handle the worker must not
    // keep, such as the daemon's launch-nonce pipe: the worker's own startup,
    // not the trampoline, has to close both.
    // SAFETY: this fork hook uses only async-signal-safe descriptor operations.
    unsafe {
        command.pre_exec(move || {
            for target in [3, 64] {
                if libc::dup2(write_fd, target) < 0 || libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("disclaimed descriptor probe");
    drop(command);
    if let Err(error) = confirmation.confirm(Instant::now() + Duration::from_secs(180)) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("descriptor probe confirmation: {error}");
    }
    let output = child.wait_with_output().expect("descriptor probe output");
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("probe JSON");
    assert_eq!(report["open_descriptors"], serde_json::json!([0, 1, 2]));
}

#[test]
fn production_pool_constructor_requires_its_own_trampoline() {
    let config = PoolConfig::beside_current_exe().expect("production pool config");
    let WorkerLaunch::Disclaimed { trampoline } = config.worker_launch else {
        panic!("production constructor must disclaim");
    };
    assert_eq!(
        trampoline,
        std::env::current_exe().expect("current executable")
    );
}

#[test]
fn disclaim_refusal_is_named_and_child_is_killed_and_reaped() {
    let dir = std::env::temp_dir().join(format!("basal-disclaim-refusal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let script = dir.join("trampoline");
    let pid_file = dir.join("child.pid");
    // This stand-in trampoline reports a refusal on its confirmation pipe and
    // closes it, but then keeps running, blocked on stdin. It only goes away if
    // the caller kills and reaps it after reading the refusal, which is what
    // this test checks.
    std::fs::write(
        &script,
        "#!/bin/sh\necho $$ > \"$3\"\neval 'printf \"SUBC_PRIVACY_REFUSAL_V1 privacy identity trampoline setup failed: fixture refusal\\n\" >&'\"$2\"\neval \"exec $2>&-\"\nread -r line\n",
    )
    .expect("refusing trampoline");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("executable fixture");
    let result = WorkerProcess::start(
        &pid_file,
        &WorkerLaunch::Disclaimed { trampoline: script },
        Duration::from_secs(180),
    );
    let error = match result {
        Err(SpawnError::Disclaim(error)) => error,
        Err(other) => panic!("expected disclaim refusal, got {other}"),
        Ok(_) => panic!("a refusing trampoline must not launch a worker"),
    };
    assert!(error.contains("fixture refusal"), "{error}");
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .expect("fixture started")
        .trim()
        .parse()
        .expect("fixture pid");
    // SAFETY: signal zero only observes the child's existence. An unreaped
    // zombie still exists, so ESRCH proves both termination and reaping.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child still exists");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    std::fs::remove_dir_all(dir).expect("remove fixture");
}
