//! basal's store holds flow inputs and outputs, so no other account may read
//! it. On Unix the shared store crate makes the directory 0700 and the files
//! 0600 (pinned by basal-core's `store_dir_mode` tests). On Windows it gives
//! the directory and the database files a protected DACL (nothing inherited
//! from the parent) with a single entry: full access for the user the
//! process runs as. These tests read that DACL back after basal opens its
//! store, including over a directory that already existed with the
//! permissive DACL it inherited.
#![cfg(windows)]

use basal_core::{Durability, Store};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorControl, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "basal-store-dacl-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// The `S-1-...` form of a SID.
fn sid_text(sid: PSID) -> String {
    let mut text = null_mut();
    assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    value
}

/// The SID of the user this test runs as.
fn current_user() -> String {
    let mut token: HANDLE = null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut bytes = 0;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut bytes) };
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        },
        0
    );
    unsafe { CloseHandle(token) };
    sid_text(unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

/// One entry of a DACL.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    ace_type: u8,
    flags: u8,
    mask: u32,
    sid: String,
}

/// Whether the DACL is protected from inheritance, and its entries.
fn dacl(path: &Path) -> (bool, Vec<Entry>) {
    let mut acl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(error, 0, "GetNamedSecurityInfoW({})", path.display());
    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert!(!acl.is_null(), "{} has no DACL at all", path.display());
    let count = unsafe { (*acl).AceCount };
    let mut entries = Vec::new();
    for index in 0..u32::from(count) {
        let mut ace = null_mut();
        assert_ne!(unsafe { GetAce(acl, index, &mut ace) }, 0);
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        let (mask, sid) = if u32::from(header.AceType) == ACCESS_ALLOWED_ACE_TYPE {
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = unsafe { std::ptr::addr_of!((*allowed).SidStart) } as PSID;
            (unsafe { (*allowed).Mask }, sid_text(sid))
        } else {
            (0, String::new())
        };
        entries.push(Entry {
            ace_type: header.AceType,
            flags: header.AceFlags,
            mask,
            sid,
        });
    }
    unsafe { LocalFree(descriptor) };
    (control & SE_DACL_PROTECTED != 0, entries)
}

/// Owner-only: protected, one allow entry for the current user with full
/// access. A directory's entry is inherited by what is created in it.
fn assert_owner_only(path: &Path, directory: bool) {
    let (protected, entries) = dacl(path);
    println!("{}: protected {protected}, {entries:?}", path.display());
    assert!(protected, "{} inherits its parent's DACL", path.display());
    let inherit = if directory {
        // OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        0x1 | 0x2
    } else {
        0
    };
    assert_eq!(
        entries,
        [Entry {
            ace_type: ACCESS_ALLOWED_ACE_TYPE as u8,
            flags: inherit,
            mask: FILE_ALL_ACCESS,
            sid: current_user(),
        }],
        "{}",
        path.display()
    );
}

#[test]
fn opening_the_store_makes_its_directory_and_database_owner_only() {
    let root = scratch("fresh");
    let _cleanup = Cleanup(root.clone());
    let dir = root.join("basal");
    let store = Store::open(dir.join("store.db"), Durability::default()).expect("open");
    assert_owner_only(&dir, true);
    assert_owner_only(&dir.join("store.db"), false);
    drop(store);
}

/// The scratch root inherits the temp directory's DACL, which grants more
/// than the current user, so the positive control shows the check can fail.
#[test]
fn opening_the_store_narrows_an_existing_inherited_directory() {
    let root = scratch("existing");
    let _cleanup = Cleanup(root.clone());
    let dir = root.join("basal");
    std::fs::create_dir(&dir).unwrap();
    let (protected, entries) = dacl(&dir);
    assert!(
        !protected && entries.len() > 1,
        "the control directory should inherit a broader DACL: {entries:?}"
    );
    let store = Store::open(dir.join("store.db"), Durability::default()).expect("open");
    assert_owner_only(&dir, true);
    assert_owner_only(&dir.join("store.db"), false);
    drop(store);
}
