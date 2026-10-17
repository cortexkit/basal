//! Basal's worker staging adapter and Windows-only AppContainer grant.

use std::path::{Path, PathBuf};

/// Stage a test executable through the fleet's shared content-addressed copier.
///
/// The repository's executable-name check requires Cargo-provided `ck-*`
/// paths to pass through this adapter. Storage, symlink handling, cache keys,
/// and atomic publication belong to `cortexkit-test-support`.
pub fn dev_binary(binary: impl AsRef<Path>) -> PathBuf {
    cortexkit_test_support::ckdev_binary(binary)
}

/// Places a test worker and grants its directory to the Windows AppContainer.
/// Production installation never uses this test-only ACL grant.
pub(crate) fn dev_worker_binary(binary: impl AsRef<Path>) -> PathBuf {
    let binary = dev_binary(binary);
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        use std::sync::{Mutex, OnceLock};

        static GRANTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
        let directory = binary.parent().expect("worker directory").to_path_buf();
        let mut granted = GRANTED.get_or_init(Mutex::default).lock().unwrap();
        if !granted.contains(&directory) {
            let package = basal_launch::create_or_open_profile().expect("test worker package SID");
            basal_launch::grant_test_binary_directory(&binary, &package)
                .expect("test worker directory ACL");
            granted.insert(directory);
        }
    }
    binary
}
