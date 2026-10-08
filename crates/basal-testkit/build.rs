use std::path::PathBuf;
use std::process::Command;

#[path = "src/worker_artifact.rs"]
mod worker_artifact;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    // Cargo's stable artifact dependencies cannot yet provide another package's
    // executable. Build it here, once for this testkit fingerprint, rather than
    // in every test process. A separate target directory avoids waiting on the
    // outer Cargo process's build lock.
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/basal-worker",
        "crates/basal-proto",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let target = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("worker-target");
    let output = Command::new(std::env::var_os("CARGO").unwrap())
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "-p",
            "basal-worker",
            "--bin",
            "ck-basal-worker",
            "--message-format=json",
            "--target-dir",
        ])
        .arg(target)
        .output()
        .expect("launch Cargo to build the test worker");
    assert!(
        output.status.success(),
        "building the test worker failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let binary = worker_artifact::reported_worker(&output.stdout)
        .expect("Cargo did not report a ck-basal-worker executable");
    println!("cargo:rustc-env=BASAL_TEST_WORKER_BIN={binary}");
}
