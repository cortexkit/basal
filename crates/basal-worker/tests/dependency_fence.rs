//! The worker links the protocol and the engine only. A compromised worker
//! must have no code path to a store, to subc, or to basal-core, so none of
//! them may appear anywhere in its dependency tree.

use std::process::Command;

#[test]
fn worker_dependency_tree_has_no_store_no_subc_and_no_core() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--manifest-path",
            manifest,
            "--package",
            "basal-worker",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
            "--offline",
        ])
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    let packages: Vec<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();

    // Positive check first: the worker and what it must link appear, which
    // proves the cargo tree output was parsed. Without it, an empty output or
    // a failed parse would pass the forbidden-package check below vacuously.
    for required in ["basal-worker", "basal-proto", "rquickjs"] {
        assert!(
            packages.contains(&required),
            "{required} missing from:\n{tree}"
        );
    }
    let forbidden: Vec<&&str> = packages
        .iter()
        .filter(|name| {
            name.contains("sqlite")
                || name.starts_with("subc")
                || **name == "basal-core"
                || name.starts_with("cortexkit-store")
        })
        .collect();
    assert!(
        forbidden.is_empty(),
        "forbidden dependencies: {forbidden:?}\n{tree}"
    );
}
