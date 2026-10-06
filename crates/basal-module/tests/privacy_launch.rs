#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use basal_module::pool::PoolConfig;
use basal_module::process::{SpawnError, WorkerLaunch, WorkerProcess};

#[test]
fn real_trampoline_confirms_and_worker_completes_seatbelt_handshake() {
    let trampoline = PathBuf::from(env!("CARGO_BIN_EXE_ck-basal"));
    subc_os::privacy_identity::probe(&trampoline, Instant::now() + Duration::from_secs(180))
        .expect("production trampoline probe before runtime startup");
    let worker = WorkerProcess::start(
        &basal_testkit::channel::worker_binary(),
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
    // The fixture writes a valid refusal and closes its ack writer, but stays
    // alive on stdin. Only the caller's kill-and-reap path can remove it.
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
