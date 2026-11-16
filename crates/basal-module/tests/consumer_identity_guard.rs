//! Basal never names a consumer identity on a daemon call.
//!
//! Once the daemon hands basal a registration channel, every consumer
//! connection is bound to basal by a claim, and the SDK refuses a call that
//! sets `CallOptions::consumer_identity` (or the same field on
//! `CloseRouteOptions`) before sending it. Calls carry basal's own attested
//! identity instead. This guard scans the workspace's Rust sources so a new
//! override fails here rather than in production after the channel is on.

use std::path::{Path, PathBuf};

/// Lines that set a consumer identity: a struct-literal field or an
/// assignment. Reading a `consumer_identity` member of wire JSON
/// (`open["consumer_identity"]`) is not a setting and does not match.
fn sites(source: &str) -> Vec<usize> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split("//").next().unwrap_or("");
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            compact.contains("consumer_identity:")
                || compact.contains(".consumer_identity=")
                || compact.contains("with_consumer_identity(")
        })
        .map(|(index, _)| index + 1)
        .collect()
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn no_call_sets_a_consumer_identity() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let this = Path::new(file!()).file_name().unwrap();
    let mut files = Vec::new();
    rust_files(crates, &mut files);
    assert!(
        files.len() > 100,
        "the scan must cover the workspace's sources, found {}",
        files.len()
    );
    let mut violations = Vec::new();
    for file in files {
        if file.file_name() == Some(this) {
            continue;
        }
        let source = std::fs::read_to_string(&file).unwrap();
        for line in sites(&source) {
            violations.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "a registration-channel connection refuses explicit consumer identities: {violations:?}"
    );
}

#[test]
fn the_guard_finds_each_way_of_setting_one() {
    let planted = r#"
        let options = CallOptions { consumer_identity: Some(identity), ..Default::default() };
        let mut close = CloseRouteOptions::default();
        close.consumer_identity = Some(identity.clone());
        let options = CallOptions::default().with_consumer_identity(identity);
        // consumer_identity: only a comment
        let principal = open["consumer_identity"]["module_id"].as_str();
    "#;
    assert_eq!(sites(planted), vec![2, 4, 5]);
}
