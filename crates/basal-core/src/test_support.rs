//! Filesystem fixtures shared by core and module integration tests.

use std::sync::atomic::{AtomicU64, Ordering};

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a process-local scratch directory with a time and sequence suffix.
/// This only creates the directory; it never removes a pre-existing path.
/// Each call returns a directory no other call has.
pub fn timestamp_scratch(prefix: &str, name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    create_scratch_dir(prefix, name, nanos, &SCRATCH_COUNTER)
}

fn scratch_path(prefix: &str, name: &str, nanos: u128, sequence: u64) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "basal-{prefix}-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

fn create_scratch_dir(
    prefix: &str,
    name: &str,
    nanos: u128,
    counter: &AtomicU64,
) -> std::path::PathBuf {
    loop {
        let sequence = counter.fetch_add(1, Ordering::Relaxed);
        let dir = scratch_path(prefix, name, nanos, sequence);
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create scratch directory {}: {error}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{create_scratch_dir, scratch_path, timestamp_scratch};
    use std::collections::HashSet;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    #[test]
    fn concurrent_calls_create_distinct_directories() {
        let dirs = (0..64)
            .map(|_| thread::spawn(|| timestamp_scratch("test-support", "concurrent")))
            .map(|thread| thread.join().expect("scratch directory thread"))
            .collect::<Vec<_>>();

        assert!(dirs.iter().all(|dir| dir.is_dir()));
        assert_eq!(dirs.iter().collect::<HashSet<_>>().len(), dirs.len());

        for dir in dirs {
            std::fs::remove_dir_all(dir).expect("remove scratch directory");
        }
    }

    #[test]
    fn existing_candidates_are_skipped() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after Unix epoch")
            .as_nanos();
        let counter = AtomicU64::new(0);
        let existing = scratch_path("test-support", "pre-existing", nanos, 0);
        std::fs::create_dir(&existing).expect("create pre-existing candidate");

        let created = create_scratch_dir("test-support", "pre-existing", nanos, &counter);

        assert_ne!(created, existing);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(existing.is_dir());
        assert!(created.is_dir());
        std::fs::remove_dir_all(existing).expect("remove pre-existing candidate");
        std::fs::remove_dir_all(created).expect("remove created directory");
    }

    #[test]
    fn scratch_names_include_the_sequence_counter() {
        assert_ne!(
            scratch_path("test-support", "counter", 0, 0),
            scratch_path("test-support", "counter", 0, 1)
        );
    }
}
