//! Filesystem fixtures shared by core and module integration tests.

/// Create a process-local scratch directory whose suffix is the current time.
/// This only creates the directory; it never removes a pre-existing path.
pub fn timestamp_scratch(prefix: &str, name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "basal-{prefix}-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
