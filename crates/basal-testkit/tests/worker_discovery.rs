//! Cargo owns the build-time worker path; runtime discovery never invokes it.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[path = "../src/worker_artifact.rs"]
mod worker_artifact;

#[test]
fn worker_discovery_uses_cargos_reported_executable() {
    for profile in ["debug", "release"] {
        let artifact = format!("relative-target/{profile}/ck-basal-worker");
        let message = serde_json::json!({"reason":"compiler-artifact",
            "target":{"name":"ck-basal-worker"},"executable":artifact});
        assert_eq!(
            worker_artifact::reported_worker(message.to_string().as_bytes()),
            Some(artifact)
        );
    }
    assert_eq!(worker_artifact::reported_worker(b"not JSON\n"), None);
}

#[test]
fn every_test_process_uses_the_built_worker_without_invoking_cargo() {
    if std::env::var_os("BASAL_DISCOVERY_PROBE").is_some() {
        let binary = basal_testkit::worker_binary();
        let (mut worker, _) =
            basal_testkit::WorkerProcess::start(&binary, std::time::Duration::from_secs(60))
                .unwrap();
        worker.close_stdin();
        assert!(
            worker
                .wait_exit(std::time::Duration::from_secs(60))
                .is_some()
        );
        return;
    }
    let dir = basal_testkit::harness::scratch("worker-discovery");
    let cargo = dir.join("cargo-fixture");
    let calls = dir.join("calls");
    std::fs::write(
        &cargo,
        "#!/bin/sh\necho build >> \"$BASAL_DISCOVERY_CALLS\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..2 {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "every_test_process_uses_the_built_worker_without_invoking_cargo",
                "--nocapture",
            ])
            .env_remove("BASAL_WORKER_BIN")
            .env("CARGO", &cargo)
            .env("BASAL_DISCOVERY_PROBE", "1")
            .env("BASAL_DISCOVERY_CALLS", &calls)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(
        std::fs::read_to_string(&calls)
            .unwrap_or_default()
            .lines()
            .count(),
        0,
        "test processes must not build the worker"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_worker_override_preserves_the_selected_bytes() {
    if let Some(artifact) = std::env::var_os("BASAL_DISCOVERY_ARTIFACT") {
        assert_eq!(
            std::fs::read(basal_testkit::worker_binary()).unwrap(),
            std::fs::read(artifact).unwrap()
        );
        return;
    }
    let dir = basal_testkit::harness::scratch("worker-override");
    for profile in ["debug", "release"] {
        let artifact = dir.join(profile).join("ckdev-basal-worker");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, format!("{profile} worker")).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "explicit_worker_override_preserves_the_selected_bytes",
                "--nocapture",
            ])
            .env("BASAL_WORKER_BIN", &artifact)
            .env("BASAL_DISCOVERY_ARTIFACT", &artifact)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
