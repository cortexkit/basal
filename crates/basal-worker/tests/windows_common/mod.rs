//! Shared by the Windows worker tests.
//!
//! Test processes never run under a production executable name (`ck-*`), so
//! a test process can't be mistaken for the placed production fleet. The
//! worker is copied to a `ckdev-` name before it is run or inspected, as
//! basal-testkit's `dev_binary` does on the other systems.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A `ckdev-` named copy of the built worker, in a directory of its own so
/// that granting the worker's package access changes no other ACL. A build
/// with the `deviations` feature gets its own directory, so the two test runs
/// never replace each other's image.
pub fn dev_binary(built: &str) -> &'static Path {
    static COPY: OnceLock<PathBuf> = OnceLock::new();
    COPY.get_or_init(|| {
        let built = Path::new(built);
        let directory = built.parent().expect("the worker has a directory").join(
            if cfg!(feature = "deviations") {
                "ckdev-placed-deviations"
            } else {
                "ckdev-placed"
            },
        );
        std::fs::create_dir_all(&directory).expect("create the placement directory");
        let copy = directory.join("ckdev-basal-worker.exe");
        std::fs::copy(built, &copy).expect("copy the worker");
        copy
    })
}
