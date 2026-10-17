//! Shared by the Windows worker tests.
//!
//! Test processes never run under a production executable name (`ck-*`), so
//! a test process can't be mistaken for the placed production fleet. Shared
//! staging puts content-addressed `ckdev-` copies outside Cargo's build folder.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A `ckdev-` named shared copy of the built worker. Feature builds get
/// different content-addressed directories when their executable bytes differ.
pub fn dev_binary(built: &str) -> &'static Path {
    static COPY: OnceLock<PathBuf> = OnceLock::new();
    COPY.get_or_init(|| cortexkit_test_support::ckdev_binary(Path::new(built)))
}

use std::os::windows::io::{AsRawHandle, RawHandle};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}
impl AsRawHandle for Handle {
    fn as_raw_handle(&self) -> RawHandle {
        self.0
    }
}
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn placed_worker(worker: &'static Path) -> &'static Path {
    static GRANT: OnceLock<()> = OnceLock::new();
    GRANT.get_or_init(|| {
        let package = basal_launch::create_or_open_profile().expect("worker profile");
        basal_launch::grant_test_binary_directory(worker, &package).expect("grant test placement");
    });
    worker
}
