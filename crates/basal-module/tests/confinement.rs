//! Parent-side acceptance uses the reported policy before a worker sees code.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::time::Duration;

use basal_module::pool::{LandlockPolicy, PoolConfig, ProcessSpawner, Spawn};
use basal_module::process::{SpawnError, WorkerLaunch};
use basal_proto::{
    Confinement, LandlockReport, PROTOCOL_VERSION, PreludeHash, Welcome, WorkerMessage,
    encode_worker_frame,
};

#[test]
fn spawn_refuses_a_stand_in_worker_with_seccomp_false() {
    let dir = common::scratch("bad-welcome");
    let script = dir.join("ckdev-bad-worker");
    let pid_file = dir.join("pid");
    let argv_file = dir.join("argv");
    let frame = encode_worker_frame(&WorkerMessage::Welcome(Welcome {
        protocol_version: PROTOCOL_VERSION,
        engine: "stand-in".into(),
        prelude_hash: PreludeHash([0; 32]),
        codemode_prelude_hash: PreludeHash([0; 32]),
        confinement: Confinement::Linux {
            seccomp: false,
            landlock: Some(LandlockReport {
                runtime_abi: 1,
                applied_abi: 1,
            }),
        },
    }))
    .expect("Welcome frame");
    let octal: String = frame.iter().map(|byte| format!("\\{byte:03o}")).collect();
    // This child sends a well-framed but unacceptable Welcome, then blocks on
    // stdin. Refusing the report must kill and reap it without an activation.
    std::fs::write(&script, format!(
        "#!/bin/sh\necho $$ > '{}'\nprintf '%s\\n' \"$@\" > '{}'\nprintf '{octal}'\nread -r activation\n",
        pid_file.display(), argv_file.display(),
    )).expect("stand-in script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let config = PoolConfig::new(&script, WorkerLaunch::Plain);
    let error = match ProcessSpawner::new(&config).spawn() {
        Err(SpawnError::Handshake(error)) => error,
        Err(other) => panic!("expected handshake refusal, got {other}"),
        Ok(_) => panic!("a worker without seccomp was accepted"),
    };
    assert!(error.contains("confinement"), "{error}");
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .expect("started pid")
        .trim()
        .parse()
        .expect("pid");
    // SAFETY: signal zero observes existence; an unreaped zombie still exists.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "refused child still exists"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let argv = std::fs::read_to_string(argv_file).expect("launch arguments");
    if cfg!(target_os = "linux") {
        assert_eq!(argv, "--landlock=required\n");
    } else {
        assert_eq!(argv, "\n");
    }
    assert_eq!(config.landlock, LandlockPolicy::Required);
    std::fs::remove_dir_all(dir).expect("remove fixture");
}

#[cfg(target_os = "linux")]
#[test]
fn production_spawner_requires_landlock_completes_activation_and_shuts_down() {
    use basal_proto::{
        ActivationRequest, ActivationResult, Budgets, JsonText, LANDLOCK_ABI, ParentMessage,
        Profile,
    };
    use std::time::Instant;

    let production = PoolConfig::beside_current_exe().expect("production config");
    assert!(matches!(production.worker_launch, WorkerLaunch::Plain));
    assert_eq!(production.landlock, LandlockPolicy::Required);
    let config = PoolConfig::new(basal_testkit::worker_binary(), production.worker_launch);
    let mut worker = ProcessSpawner::new(&config)
        .spawn()
        .expect("required Landlock handshake");
    let Confinement::Linux {
        seccomp: true,
        landlock: Some(report),
    } = worker.welcome().confinement
    else {
        panic!("incomplete Linux confinement: {:?}", worker.welcome());
    };
    assert!(report.runtime_abi >= 1);
    assert_eq!(report.applied_abi, report.runtime_abi.min(LANDLOCK_ABI));
    worker
        .send(&ParentMessage::Activate(Box::new(ActivationRequest {
            activation_id: 1,
            profile: Profile::Flow,
            prelude_hash: worker.welcome().prelude_hash,
            tools: vec![],
            script: "return 4;".into(),
            trigger: JsonText::null(),
            self_input: JsonText::null(),
            budgets: Budgets::default(),
            prefix: vec![],
        })))
        .expect("activation");
    assert_eq!(
        worker.recv(Duration::from_secs(60)).expect("finished"),
        WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Completed {
                value: JsonText::new("4").expect("JSON")
            },
        }
    );
    worker.send(&ParentMessage::Shutdown).expect("shutdown");
    let mut exit = None;
    basal_testkit::harness::wait_until(
        Instant::now() + Duration::from_secs(60),
        "normal shutdown",
        || {
            exit = worker.exit_status();
            exit.is_some()
        },
    )
    .expect("exited");
    assert_eq!(exit.expect("exit status").expect("wait").code(), Some(0));
}
