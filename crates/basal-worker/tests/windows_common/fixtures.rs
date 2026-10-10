//! The test harness retains the package namespace, registry key, named section
//! and pipe. Workers must open them afresh; none of those handles is inherited.
//! A separate file is deliberately made inheritable to test the handle-list
//! boundary. Weaker launcher controls prove that the same fixtures can be opened.
#![allow(unsafe_op_in_unsafe_fn)]
use super::windows_common::wide;
use super::{Handle, Probe};
use basal_launch::{Deviation, PROFILE_NAME, create_or_open_profile};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::mem::{size_of, transmute, zeroed};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *mut UnicodeString,
    attributes: u32,
    descriptor: *mut c_void,
    qos: *mut c_void,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryKey(
        key: HANDLE,
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtCreateSection(
        out: *mut HANDLE,
        access: u32,
        attrs: *mut ObjectAttributes,
        size: *mut i64,
        protection: u32,
        allocation: u32,
        file: HANDLE,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    fn GetAppContainerNamedObjectPath(
        token: HANDLE,
        sid: PSID,
        bytes: u32,
        path: *mut u16,
        returned: *mut u32,
    ) -> i32;
}
static SERIAL: AtomicUsize = AtomicUsize::new(0);
fn unique() -> String {
    format!(
        "ckdev-confinement-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}

struct Library(HMODULE);
impl Library {
    unsafe fn load(name: &str) -> Self {
        let module = LoadLibraryExW(
            wide(name).as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        assert!(!module.is_null(), "{name}: {}", GetLastError());
        Self(module)
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}
struct Revert;
impl Drop for Revert {
    fn drop(&mut self) {
        assert_ne!(unsafe { RevertToSelf() }, 0);
    }
}

pub struct Package {
    _holder: Probe,
    folder: String,
    pub hive: String,
    namespace: String,
    key: HKEY,
}
impl Package {
    pub fn new() -> Self {
        let mut holder = Probe::start(Deviation::LpacOnly, "checkpoint", "", 0);
        holder.marker("checkpoint");
        unsafe {
            let api = Library::load("userenv.dll");
            let ole = Library::load("ole32.dll");
            let folder_api: unsafe extern "system" fn(*const u16, *mut *mut u16) -> i32 = transmute(
                GetProcAddress(api.0, c"GetAppContainerFolderPath".as_ptr().cast()).unwrap(),
            );
            let free: unsafe extern "system" fn(*const c_void) =
                transmute(GetProcAddress(ole.0, c"CoTaskMemFree".as_ptr().cast()).unwrap());
            let registry: unsafe extern "system" fn(u32, *mut HKEY) -> i32 = transmute(
                GetProcAddress(api.0, c"GetAppContainerRegistryLocation".as_ptr().cast()).unwrap(),
            );
            let sid = create_or_open_profile().unwrap();
            let mut folder = null_mut();
            assert_eq!(folder_api(wide(sid.as_str()).as_ptr(), &mut folder), 0);
            let mut length = 0;
            while *folder.add(length) != 0 {
                length += 1;
            }
            let folder_text = String::from_utf16_lossy(std::slice::from_raw_parts(folder, length));
            free(folder.cast());
            let mut token = null_mut();
            assert_ne!(
                OpenProcessToken(
                    holder.child.as_raw_handle(),
                    TOKEN_QUERY | TOKEN_DUPLICATE,
                    &mut token
                ),
                0
            );
            let token = Handle(token);
            let mut imp = null_mut();
            assert_ne!(
                DuplicateTokenEx(
                    token.0,
                    TOKEN_QUERY | TOKEN_IMPERSONATE,
                    null(),
                    SecurityImpersonation,
                    TokenImpersonation,
                    &mut imp
                ),
                0
            );
            let imp = Handle(imp);
            assert_ne!(SetThreadToken(null(), imp.0), 0);
            let revert = Revert;
            let mut key = null_mut();
            assert_eq!(
                registry(KEY_QUERY_VALUE | KEY_SET_VALUE, &mut key),
                0,
                "resolve the real profile storage key"
            );
            let mut buffer = vec![0u32; 4096];
            let mut returned = 0;
            assert_eq!(
                NtQueryKey(
                    key,
                    3,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 4) as u32,
                    &mut returned
                ),
                0
            );
            let bytes = buffer[0] as usize;
            assert!(bytes.is_multiple_of(2) && bytes + 4 <= returned as usize);
            let hive = String::from_utf16_lossy(std::slice::from_raw_parts(
                buffer.as_ptr().add(1).cast::<u16>(),
                bytes / 2,
            ));
            assert!(
                hive.ends_with(PROFILE_NAME),
                "storage key is keyed by profile name: {hive}"
            );
            drop(revert);
            let mut path = vec![0u16; 2048];
            let mut returned = 0;
            assert_ne!(
                GetAppContainerNamedObjectPath(
                    token.0,
                    null_mut(),
                    path.len() as u32,
                    path.as_mut_ptr(),
                    &mut returned
                ),
                0,
                "package namespace: {}",
                GetLastError()
            );
            let length = path.iter().position(|c| *c == 0).unwrap();
            let relative = String::from_utf16_lossy(&path[..length]);
            let mut session = 0;
            assert_ne!(ProcessIdToSessionId(std::process::id(), &mut session), 0);
            let namespace = if relative.starts_with('\\') {
                relative
            } else {
                format!("\\Sessions\\{session}\\{relative}")
            };
            Self {
                _holder: holder,
                folder: folder_text,
                hive,
                namespace,
                key,
            }
        }
    }
    pub fn create_path(&self) -> String {
        format!("{}\\{}", self.folder, unique())
    }
    pub fn remove_value(&self) {
        assert_eq!(
            unsafe { RegDeleteValueW(self.key, wide("ckdev-confinement-fixture").as_ptr()) },
            0
        );
    }
    pub fn section(&self) -> (Handle, String) {
        unsafe {
            let mut sd: SECURITY_DESCRIPTOR = zeroed();
            assert_ne!(
                InitializeSecurityDescriptor((&mut sd as *mut SECURITY_DESCRIPTOR).cast(), 1),
                0
            );
            // A NULL DACL makes the successful LPAC open an authority witness, not
            // an accidental denial caused by a fixture's discretionary permissions.
            assert_ne!(
                SetSecurityDescriptorDacl(
                    (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    null(),
                    0
                ),
                0
            );
            let path = format!("{}\\{}", self.namespace, unique());
            let mut text = wide(&path);
            let mut name = UnicodeString {
                length: ((text.len() - 1) * 2) as u16,
                maximum_length: (text.len() * 2) as u16,
                buffer: text.as_mut_ptr(),
            };
            let mut attrs = ObjectAttributes {
                length: size_of::<ObjectAttributes>() as u32,
                root: null_mut(),
                name: &mut name,
                attributes: 0x40,
                descriptor: (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
                qos: null_mut(),
            };
            let mut section = null_mut();
            let mut size = 4096;
            assert_eq!(
                NtCreateSection(
                    &mut section,
                    0xf001f,
                    &mut attrs,
                    &mut size,
                    4,
                    0x8000000,
                    null_mut()
                ),
                0
            );
            (Handle(section), path)
        }
    }
}
impl Drop for Package {
    fn drop(&mut self) {
        unsafe {
            RegDeleteValueW(self.key, wide("ckdev-confinement-fixture").as_ptr());
            RegCloseKey(self.key);
        }
    }
}

pub fn pipe() -> (Handle, String) {
    let path = format!("\\\\.\\pipe\\{}", unique());
    let handle = unsafe {
        CreateNamedPipeW(
            wide(&path).as_ptr(),
            PIPE_ACCESS_OUTBOUND,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            null(),
        )
    };
    assert_ne!(handle, INVALID_HANDLE_VALUE, "named pipe: {}", unsafe {
        GetLastError()
    });
    (Handle(handle), path)
}

pub struct Planted {
    pub file: File,
    path: PathBuf,
}
impl Planted {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(unique());
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    file.as_raw_handle(),
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                )
            },
            0
        );
        Self { file, path }
    }
    pub fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap()
    }
}
impl Drop for Planted {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
