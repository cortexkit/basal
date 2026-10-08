//! basal's store holds flow inputs and outputs, so its directory must not be
//! readable by other users. The shared store crate creates a store's own
//! directory with mode 0700 and narrows an existing one when it opens. These
//! tests pin that basal opens its store through that crate, including over a
//! directory an older build left at 0755.
#![cfg(unix)]

use basal_core::{Durability, Store};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "basal-store-mode-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &PathBuf) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn opening_the_store_creates_its_directory_private() {
    let root = scratch("fresh");
    let _cleanup = Cleanup(root.clone());
    let dir = root.join("basal");
    let _store = Store::open(dir.join("store.db"), Durability::default()).expect("open");
    assert_eq!(mode(&dir), 0o700, "a new store directory must be 0700");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn opening_the_store_narrows_an_existing_open_directory() {
    let root = scratch("existing");
    let _cleanup = Cleanup(root.clone());
    let dir = root.join("basal");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _store = Store::open(dir.join("store.db"), Durability::default()).expect("open");
    assert_eq!(
        mode(&dir),
        0o700,
        "an existing 0755 store directory must be narrowed"
    );
    fs::remove_dir_all(&root).unwrap();
}
