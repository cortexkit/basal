//! Cargo, not an assumed target directory or profile, owns the worker path.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn worker_discovery_uses_cargos_reported_executable() {
    if let Some(artifact) = std::env::var_os("BASAL_DISCOVERY_ARTIFACT") {
        let binary = basal_testkit::worker_binary();
        assert_eq!(
            std::fs::read(binary).unwrap(),
            std::fs::read(artifact).unwrap()
        );
        return;
    }
    let dir = basal_testkit::harness::scratch("worker-discovery");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let cargo = dir.join("cargo-fixture");
    std::fs::write(
        &cargo,
        "#!/bin/sh\nprintf '%s\\n' \"$BASAL_DISCOVERY_JSON\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    for profile in ["debug", "release"] {
        let artifact = dir.join(profile).join("ckdev-basal-worker");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, format!("{profile} worker")).unwrap();
        let message = serde_json::json!({"reason":"compiler-artifact",
            "target":{"name":"ck-basal-worker"},"executable":artifact});
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker_discovery_uses_cargos_reported_executable",
                "--nocapture",
            ])
            .env_remove("BASAL_WORKER_BIN")
            .env("CARGO", &cargo)
            .env("CARGO_TARGET_DIR", "relative-target")
            .env("BASAL_DISCOVERY_ARTIFACT", &artifact)
            .env("BASAL_DISCOVERY_JSON", message.to_string())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
