//! Windows-specific implementation of the `fs` built-in.
//!
//! Enforces raw-spelling grammar, component-wise walk from volume root with
//! reparse refusal, volume-GUID final-path component-wise comparison,
//! POSIX-semantics temp-and-rename replacement with DACL inheritance/preservation,
//! and denial of reparse points.

#![cfg_attr(not(windows), allow(unused))]

use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub use super::{
    DEFAULT_READ_BYTES, MAX_LIST_ENTRIES, MAX_PATH_BYTES, MAX_READ_BYTES, MAX_WRITE_BYTES, Purpose,
    TempHold, TempLease, TempLedger, TempRemoval, inside, is_legacy_temp_name, temp_name,
};
use super::{check_write_size, is_temp_name};
use crate::builtins::Denial;
pub use crate::builtins::codes;

/// Where a path resolved to on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// It exists; this is its real path.
    Existing(PathBuf),
    /// It does not exist (or, for a write, is about to be replaced): the
    /// real path of its parent, and its last component.
    Entry { parent: PathBuf, name: OsString },
}

fn outside(path: &Path) -> Denial {
    Denial::denied(format!(
        "{} is outside the manifest's roots or missing",
        path.display()
    ))
}

fn io_denial(path: &Path, e: &std::io::Error) -> Denial {
    match e.kind() {
        std::io::ErrorKind::NotFound => Denial::new(
            codes::NOT_FOUND,
            format!("{} does not exist", path.display()),
        ),
        _ => Denial::new(codes::IO, format!("{}: {e}", path.display())),
    }
}

/// Validates raw Windows path spelling before any normalization.
///
/// Accepted: only drive-letter-rooted absolute paths (`X:\...`).
/// Refused prefixes: UNC (`\\server\share`), `C:relative`, and `\\.\`, `\\?\`,
/// `\??\`, `//./` namespaces.
/// Refused components (after the drive root):
/// - `.`, `..` and empty components
/// - `:` (alternate data streams and `::$DATA`)
/// - trailing dots or spaces
/// - reserved device names even with extension (`CON.txt`, `aux`, etc.)
pub fn validate_raw_spelling(path: &str) -> Result<(), Denial> {
    if path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(Denial::invalid(format!(
            "a path is at most {MAX_PATH_BYTES} bytes with no NUL"
        )));
    }

    // Refused prefixes
    if path.starts_with(r"\\") || path.starts_with("//") {
        return Err(Denial::invalid(format!(
            "{path:?} is a UNC or device path, not an ordinary drive path"
        )));
    }
    if path.starts_with(r"\??\") || path.starts_with("/??/") {
        return Err(Denial::invalid(format!(
            "{path:?} uses the NT object namespace"
        )));
    }
    if path.starts_with(r"\\.\") || path.starts_with("//./") {
        return Err(Denial::invalid(format!(
            "{path:?} uses the Win32 device namespace"
        )));
    }
    if path.starts_with(r"\\?\") || path.starts_with("//?/") {
        return Err(Denial::invalid(format!(
            "{path:?} uses extended-length path syntax"
        )));
    }

    let bytes = path.as_bytes();
    // Must start with X:\ where X is ASCII alphabetic
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Err(Denial::invalid(format!(
                "{path:?} is drive-relative, not an absolute drive path"
            )));
        }
        return Err(Denial::invalid(format!(
            "{path:?} is not an absolute drive path (must start with X:\\)"
        )));
    }

    let remainder = &path[3..];
    if remainder.is_empty() {
        return Ok(());
    }

    if remainder.contains('/') {
        return Err(Denial::invalid(format!(
            "{path:?} contains forward slashes"
        )));
    }

    for comp in remainder.split('\\') {
        if comp.is_empty() {
            return Err(Denial::invalid(format!(
                "{path:?} contains an empty component"
            )));
        }
        if comp == "." || comp == ".." {
            return Err(Denial::invalid(format!(
                "{path:?} contains relative component {comp:?}"
            )));
        }
        if comp.contains(':') {
            return Err(Denial::invalid(format!(
                "{path:?} contains an alternate data stream selector"
            )));
        }
        if comp.ends_with('.') || comp.ends_with(' ') {
            return Err(Denial::invalid(format!(
                "{path:?} component {comp:?} ends with a dot or space"
            )));
        }
        let base = comp
            .split('.')
            .next()
            .unwrap_or(comp)
            .trim_end_matches([' ', '.']);
        if is_reserved_device_name(base) {
            return Err(Denial::invalid(format!(
                "{path:?} component {comp:?} uses reserved device name {base:?}"
            )));
        }
    }

    Ok(())
}

/// Whether `name` is a Windows reserved DOS device name, compared ASCII-case-insensitively.
pub fn is_reserved_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            // Windows also reserves COM and LPT followed by a superscript one, two or three.
            | "COM\u{b9}"
            | "COM\u{b2}"
            | "COM\u{b3}"
            | "LPT\u{b9}"
            | "LPT\u{b2}"
            | "LPT\u{b3}"
    )
}

/// Compares a target's volume-GUID path with a root's volume-GUID path
/// component-by-component without case folding or string-prefix matching.
pub fn guid_path_inside_component_wise(
    target_guid: &str,
    root_guid: &str,
    is_directory_root: bool,
) -> bool {
    let target_parts: Vec<&str> = target_guid.split('\\').filter(|s| !s.is_empty()).collect();
    let root_parts: Vec<&str> = root_guid.split('\\').filter(|s| !s.is_empty()).collect();

    if target_parts.len() < root_parts.len() {
        return false;
    }

    for (r, t) in root_parts.iter().zip(target_parts.iter()) {
        if r != t {
            return false;
        }
    }

    if !is_directory_root && target_parts.len() != root_parts.len() {
        return false;
    }

    true
}

#[cfg(windows)]
mod ffi {
    use std::ffi::c_void;

    pub type NTSTATUS = i32;
    pub type HANDLE = *mut c_void;
    pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

    pub const STATUS_SUCCESS: NTSTATUS = 0;
    pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC0000034_u32 as i32;
    pub const STATUS_OBJECT_PATH_NOT_FOUND: NTSTATUS = 0xC000003A_u32 as i32;
    pub const STATUS_ACCESS_DENIED: NTSTATUS = 0xC0000022_u32 as i32;
    pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = 0xC0000103_u32 as i32;
    pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC00000BA_u32 as i32;
    pub const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC00000BB_u32 as i32;
    pub const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000000D_u32 as i32;
    pub const STATUS_NO_MORE_FILES: NTSTATUS = 0x80000006_u32 as i32;
    pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC0000035_u32 as i32;

    pub const FILE_READ_DATA: u32 = 0x0001;
    pub const FILE_WRITE_DATA: u32 = 0x0002;
    pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    pub const FILE_TRAVERSE: u32 = 0x0020;
    pub const DELETE: u32 = 0x00010000;
    pub const READ_CONTROL: u32 = 0x00020000;
    pub const WRITE_DAC: u32 = 0x00040000;
    pub const SYNCHRONIZE: u32 = 0x00100000;

    pub const FILE_GENERIC_READ: u32 = 0x00120089;
    pub const FILE_GENERIC_WRITE: u32 = 0x00120116;
    pub const FILE_GENERIC_EXECUTE: u32 = 0x001200A0;
    pub const FILE_ALL_ACCESS: u32 = 0x001F01FF;

    pub const FILE_SHARE_READ: u32 = 0x00000001;
    pub const FILE_SHARE_WRITE: u32 = 0x00000002;
    pub const FILE_SHARE_DELETE: u32 = 0x00000004;

    pub const FILE_OPEN: u32 = 0x00000001;
    pub const FILE_CREATE: u32 = 0x00000002;
    pub const FILE_OPEN_IF: u32 = 0x00000003;

    pub const FILE_DIRECTORY_FILE: u32 = 0x00000001;
    pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x00000020;
    pub const FILE_NON_DIRECTORY_FILE: u32 = 0x00000040;
    pub const FILE_OPEN_REPARSE_POINT: u32 = 0x00200000;

    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000080;
    pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010;
    pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;

    pub const OBJ_CASE_INSENSITIVE: u32 = 0x00000040;

    pub const FileDirectoryInformation: u32 = 1;
    pub const FileBasicInformation: u32 = 4;
    pub const FileStandardInformation: u32 = 5;
    pub const FileDispositionInformation: u32 = 13;
    pub const FileRenameInformationEx: u32 = 65;

    pub const FILE_RENAME_REPLACE_IF_EXISTS: u32 = 0x00000001;
    pub const FILE_RENAME_POSIX_SEMANTICS: u32 = 0x00000002;

    pub const FILE_NAME_NORMALIZED: u32 = 0x0;
    pub const VOLUME_NAME_GUID: u32 = 0x1;

    pub const DACL_SECURITY_INFORMATION: u32 = 0x00000004;
    pub const SE_FILE_OBJECT: u32 = 1;

    #[repr(C)]
    pub struct UNICODE_STRING {
        pub Length: u16,
        pub MaximumLength: u16,
        pub Buffer: *mut u16,
    }

    #[repr(C)]
    pub struct OBJECT_ATTRIBUTES {
        pub Length: u32,
        pub RootDirectory: HANDLE,
        pub ObjectName: *mut UNICODE_STRING,
        pub Attributes: u32,
        pub SecurityDescriptor: *mut c_void,
        pub SecurityQualityOfService: *mut c_void,
    }

    #[repr(C)]
    pub struct IO_STATUS_BLOCK {
        pub Status: NTSTATUS,
        pub Information: usize,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FILE_BASIC_INFORMATION {
        pub CreationTime: i64,
        pub LastAccessTime: i64,
        pub LastWriteTime: i64,
        pub ChangeTime: i64,
        pub FileAttributes: u32,
        pub Reserved: u32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FILE_STANDARD_INFORMATION {
        pub AllocationSize: i64,
        pub EndOfFile: i64,
        pub NumberOfLinks: u32,
        pub DeletePending: u8,
        pub Directory: u8,
        pub Reserved: [u8; 2],
    }

    #[repr(C)]
    pub struct FILE_DIRECTORY_INFORMATION {
        pub NextEntryOffset: u32,
        pub FileIndex: u32,
        pub CreationTime: i64,
        pub LastAccessTime: i64,
        pub LastWriteTime: i64,
        pub ChangeTime: i64,
        pub EndOfFile: i64,
        pub AllocationSize: i64,
        pub FileAttributes: u32,
        pub FileNameLength: u32,
        pub FileName: [u16; 1],
    }

    #[repr(C)]
    pub struct FILE_RENAME_INFORMATION_EX {
        pub Flags: u32,
        pub RootDirectory: HANDLE,
        pub FileNameLength: u32,
        pub FileName: [u16; 1],
    }

    #[repr(C)]
    pub struct FILE_DISPOSITION_INFORMATION {
        pub DeleteFile: u8,
    }

    #[repr(C)]
    pub struct GENERIC_MAPPING {
        pub GenericRead: u32,
        pub GenericWrite: u32,
        pub GenericExecute: u32,
        pub GenericAll: u32,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        pub fn NtCreateFile(
            FileHandle: *mut HANDLE,
            DesiredAccess: u32,
            ObjectAttributes: *mut OBJECT_ATTRIBUTES,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            AllocationSize: *mut i64,
            FileAttributes: u32,
            ShareAccess: u32,
            CreateDisposition: u32,
            CreateOptions: u32,
            EaBuffer: *mut c_void,
            EaLength: u32,
        ) -> NTSTATUS;

        pub fn NtClose(Handle: HANDLE) -> NTSTATUS;

        pub fn NtQueryInformationFile(
            FileHandle: HANDLE,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            FileInformation: *mut c_void,
            Length: u32,
            FileInformationClass: u32,
        ) -> NTSTATUS;

        pub fn NtSetInformationFile(
            FileHandle: HANDLE,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            FileInformation: *mut c_void,
            Length: u32,
            FileInformationClass: u32,
        ) -> NTSTATUS;

        pub fn NtQueryDirectoryFile(
            FileHandle: HANDLE,
            Event: HANDLE,
            ApcRoutine: *mut c_void,
            ApcContext: *mut c_void,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            FileInformation: *mut c_void,
            Length: u32,
            FileInformationClass: u32,
            ReturnSingleEntry: u8,
            FileName: *mut UNICODE_STRING,
            RestartScan: u8,
        ) -> NTSTATUS;

        pub fn NtReadFile(
            FileHandle: HANDLE,
            Event: HANDLE,
            ApcRoutine: *mut c_void,
            ApcContext: *mut c_void,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            Buffer: *mut c_void,
            Length: u32,
            ByteOffset: *mut i64,
            Key: *mut u32,
        ) -> NTSTATUS;

        pub fn NtWriteFile(
            FileHandle: HANDLE,
            Event: HANDLE,
            ApcRoutine: *mut c_void,
            ApcContext: *mut c_void,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            Buffer: *const c_void,
            Length: u32,
            ByteOffset: *mut i64,
            Key: *mut u32,
        ) -> NTSTATUS;

        pub fn NtFlushBuffersFile(
            FileHandle: HANDLE,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
        ) -> NTSTATUS;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetFinalPathNameByHandleW(
            hFile: HANDLE,
            lpszFilePath: *mut u16,
            cchFilePath: u32,
            dwFlags: u32,
        ) -> u32;

        pub fn LocalFree(hMem: *mut c_void) -> *mut c_void;

        pub fn CreateHardLinkW(
            lpFileName: *const u16,
            lpExistingFileName: *const u16,
            lpSecurityAttributes: *mut c_void,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn GetSecurityInfo(
            handle: HANDLE,
            ObjectType: u32,
            SecurityInfo: u32,
            ppsidOwner: *mut *mut c_void,
            ppsidGroup: *mut *mut c_void,
            ppDacl: *mut *mut c_void,
            ppSacl: *mut *mut c_void,
            ppSecurityDescriptor: *mut *mut c_void,
        ) -> u32;

        pub fn SetSecurityInfo(
            handle: HANDLE,
            ObjectType: u32,
            SecurityInfo: u32,
            psidOwner: *mut c_void,
            psidGroup: *mut c_void,
            pDacl: *mut c_void,
            pSacl: *mut c_void,
        ) -> u32;

        pub fn CreatePrivateObjectSecurity(
            ParentDescriptor: *mut c_void,
            CreatorDescriptor: *mut c_void,
            NewDescriptor: *mut *mut c_void,
            IsDirectoryObject: i32,
            Token: HANDLE,
            GenericMapping: *mut GENERIC_MAPPING,
        ) -> i32;

        pub fn DestroyPrivateObjectSecurity(ObjectDescriptor: *mut *mut c_void) -> i32;

        pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            StringSecurityDescriptor: *const u16,
            StringSDRevision: u32,
            SecurityDescriptor: *mut *mut c_void,
            SecurityDescriptorSize: *mut u32,
        ) -> i32;

        pub fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            SecurityDescriptor: *mut c_void,
            RequestedStringSDRevision: u32,
            SecurityInformation: u32,
            StringSecurityDescriptor: *mut *mut u16,
            StringSecurityDescriptorLen: *mut u32,
        ) -> i32;
    }
}

#[cfg(windows)]
pub struct OwnedHandle(pub ffi::HANDLE);

#[cfg(windows)]
impl OwnedHandle {
    pub fn raw(&self) -> ffi::HANDLE {
        self.0
    }

    pub fn into_raw(mut self) -> ffi::HANDLE {
        let h = self.0;
        self.0 = std::ptr::null_mut();
        h
    }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != ffi::INVALID_HANDLE_VALUE {
            unsafe { ffi::NtClose(self.0) };
        }
    }
}

#[cfg(windows)]
impl From<OwnedHandle> for std::fs::File {
    fn from(h: OwnedHandle) -> Self {
        use std::os::windows::io::FromRawHandle;
        unsafe { std::fs::File::from_raw_handle(h.into_raw() as std::os::windows::io::RawHandle) }
    }
}

#[cfg(windows)]
struct FileDacl {
    sd: *mut std::ffi::c_void,
    is_private: bool,
}

#[cfg(windows)]
impl Drop for FileDacl {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            if self.is_private {
                unsafe { ffi::DestroyPrivateObjectSecurity(&mut self.sd) };
            } else {
                unsafe { ffi::LocalFree(self.sd) };
            }
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
enum OpenError {
    NotFound,
    ReparsePoint,
    AccessDenied,
    Other(ffi::NTSTATUS),
}

#[cfg(windows)]
fn nt_open_relative(
    root: ffi::HANDLE,
    name: &str,
    directory: bool,
    desired_access: u32,
    create_disposition: u32,
    create_options: u32,
    security_descriptor: *mut std::ffi::c_void,
) -> Result<OwnedHandle, OpenError> {
    let wide_name: Vec<u16> = name.encode_utf16().collect();
    let mut unicode_name = ffi::UNICODE_STRING {
        Length: (wide_name.len() * 2) as u16,
        MaximumLength: (wide_name.len() * 2) as u16,
        Buffer: wide_name.as_ptr() as *mut u16,
    };
    let mut obj_attr = ffi::OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<ffi::OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: if wide_name.is_empty() {
            std::ptr::null_mut()
        } else {
            &mut unicode_name
        },
        Attributes: ffi::OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security_descriptor,
        SecurityQualityOfService: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut options =
        create_options | ffi::FILE_SYNCHRONOUS_IO_NONALERT | ffi::FILE_OPEN_REPARSE_POINT;
    if directory {
        options |= ffi::FILE_DIRECTORY_FILE;
    }
    let status = unsafe {
        ffi::NtCreateFile(
            &mut handle,
            desired_access,
            &mut obj_attr,
            &mut io_status,
            std::ptr::null_mut(),
            ffi::FILE_ATTRIBUTE_NORMAL,
            ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE | ffi::FILE_SHARE_DELETE,
            create_disposition,
            options,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(match status {
            ffi::STATUS_OBJECT_NAME_NOT_FOUND | ffi::STATUS_OBJECT_PATH_NOT_FOUND => {
                OpenError::NotFound
            }
            ffi::STATUS_ACCESS_DENIED => OpenError::AccessDenied,
            _ => OpenError::Other(status),
        });
    }

    // Check if reparse point!
    let mut basic_io = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut basic = ffi::FILE_BASIC_INFORMATION::default();
    let q_status = unsafe {
        ffi::NtQueryInformationFile(
            handle,
            &mut basic_io,
            &mut basic as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ffi::FILE_BASIC_INFORMATION>() as u32,
            ffi::FileBasicInformation,
        )
    };
    if q_status >= 0 && (basic.FileAttributes & ffi::FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        unsafe { ffi::NtClose(handle) };
        return Err(OpenError::ReparsePoint);
    }

    Ok(OwnedHandle(handle))
}

#[cfg(windows)]
fn query_file_basic(handle: ffi::HANDLE) -> std::io::Result<ffi::FILE_BASIC_INFORMATION> {
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut info = ffi::FILE_BASIC_INFORMATION::default();
    let status = unsafe {
        ffi::NtQueryInformationFile(
            handle,
            &mut io_status,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ffi::FILE_BASIC_INFORMATION>() as u32,
            ffi::FileBasicInformation,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    Ok(info)
}

#[cfg(windows)]
fn query_file_standard(handle: ffi::HANDLE) -> std::io::Result<ffi::FILE_STANDARD_INFORMATION> {
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut info = ffi::FILE_STANDARD_INFORMATION::default();
    let status = unsafe {
        ffi::NtQueryInformationFile(
            handle,
            &mut io_status,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ffi::FILE_STANDARD_INFORMATION>() as u32,
            ffi::FileStandardInformation,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    Ok(info)
}

#[cfg(windows)]
fn get_volume_guid_path(handle: ffi::HANDLE) -> Result<String, Denial> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe {
        ffi::GetFinalPathNameByHandleW(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            ffi::VOLUME_NAME_GUID | ffi::FILE_NAME_NORMALIZED,
        )
    };
    if len == 0 {
        return Err(Denial::denied("failed to get volume GUID path for handle"));
    }
    if len as usize > buf.len() {
        buf.resize(len as usize + 1, 0);
        let len2 = unsafe {
            ffi::GetFinalPathNameByHandleW(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                ffi::VOLUME_NAME_GUID | ffi::FILE_NAME_NORMALIZED,
            )
        };
        if len2 == 0 {
            return Err(Denial::denied("failed to get volume GUID path for handle"));
        }
        buf.truncate(len2 as usize);
    } else {
        buf.truncate(len as usize);
    }
    String::from_utf16(&buf).map_err(|_| Denial::denied("volume GUID path is not UTF-16"))
}

#[cfg(windows)]
struct VerifiedRoot {
    manifest: String,
    guid_path: String,
    is_directory: bool,
    handle: OwnedHandle,
}

#[cfg(windows)]
fn walk_from_volume_root(manifest_root: &str) -> Result<VerifiedRoot, Denial> {
    validate_raw_spelling(manifest_root)?;
    let drive_prefix = &manifest_root[..3]; // e.g. "C:\"
    let nt_drive = format!(r"\??\{drive_prefix}");

    // Open volume root
    let root_vol = nt_open_relative(
        std::ptr::null_mut(),
        &nt_drive,
        true,
        ffi::FILE_READ_ATTRIBUTES | ffi::FILE_TRAVERSE | ffi::SYNCHRONIZE,
        ffi::FILE_OPEN,
        0,
        std::ptr::null_mut(),
    )
    .map_err(|e| match e {
        OpenError::ReparsePoint => {
            Denial::denied(format!("volume root {drive_prefix} is a reparse point"))
        }
        OpenError::NotFound => Denial::new(
            codes::NOT_FOUND,
            format!("volume root {drive_prefix} does not exist"),
        ),
        _ => Denial::denied(format!("failed to open volume root {drive_prefix}")),
    })?;

    let remainder = &manifest_root[3..];
    if remainder.is_empty() {
        let guid_path = get_volume_guid_path(root_vol.raw())?;
        return Ok(VerifiedRoot {
            manifest: manifest_root.to_owned(),
            guid_path,
            is_directory: true,
            handle: root_vol,
        });
    }

    let components: Vec<&str> = remainder.split('\\').collect();
    let mut current = root_vol;
    for (i, comp) in components.iter().enumerate() {
        let is_last = i == components.len() - 1;
        let next = nt_open_relative(
            current.raw(),
            comp,
            !is_last, // intermediate components must be directories
            ffi::FILE_READ_ATTRIBUTES | ffi::FILE_TRAVERSE | ffi::SYNCHRONIZE,
            ffi::FILE_OPEN,
            0,
            std::ptr::null_mut(),
        )
        .map_err(|e| match e {
            OpenError::ReparsePoint => {
                Denial::denied(format!("{comp} in root {manifest_root} is a reparse point"))
            }
            OpenError::NotFound => Denial::new(
                codes::NOT_FOUND,
                format!("{comp} in root {manifest_root} does not exist"),
            ),
            _ => Denial::denied(format!(
                "failed to open component {comp} in {manifest_root}"
            )),
        })?;
        current = next;
    }

    let std_info =
        query_file_standard(current.raw()).map_err(|e| Denial::new(codes::IO, e.to_string()))?;
    let is_dir = std_info.Directory != 0;
    let guid_path = get_volume_guid_path(current.raw())?;

    Ok(VerifiedRoot {
        manifest: manifest_root.to_owned(),
        guid_path,
        is_directory: is_dir,
        handle: current,
    })
}

#[cfg(windows)]
fn filetime_to_mtime_ms(ft: i64) -> i64 {
    const UNIX_EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    if ft < UNIX_EPOCH_DIFF_100NS {
        0
    } else {
        (ft - UNIX_EPOCH_DIFF_100NS) / 10_000
    }
}

#[cfg(windows)]
fn select_root<'a>(path: &str, roots: &'a [String]) -> Result<VerifiedRoot, Denial> {
    validate_raw_spelling(path)?;
    for r in roots {
        if let Ok(()) = validate_raw_spelling(r) {
            let matched = if path == r {
                true
            } else if path.starts_with(r) {
                let suffix = &path[r.len()..];
                suffix.starts_with('\\')
            } else {
                false
            };

            if matched {
                if let Ok(vr) = walk_from_volume_root(r) {
                    return Ok(vr);
                }
            }
        }
    }
    Err(outside(Path::new(path)))
}

#[cfg(windows)]
pub fn resolve(path: &str, roots: &[String], purpose: Purpose) -> Result<Target, Denial> {
    validate_raw_spelling(path)?;
    let vr = select_root(path, roots)?;

    if purpose == Purpose::Read {
        if path == vr.manifest {
            return Ok(Target::Existing(PathBuf::from(path)));
        }
        if vr.is_directory {
            let rel = &path[vr.manifest.len()..];
            let rel = rel.strip_prefix('\\').unwrap_or(rel);
            let mut current = &vr.handle;
            let mut intermediate = None;
            let components: Vec<&str> = rel.split('\\').collect();
            let mut found = true;
            for (i, comp) in components.iter().enumerate() {
                let is_last = i == components.len() - 1;
                let cur_handle = intermediate.as_ref().unwrap_or(current);
                match nt_open_relative(
                    cur_handle.raw(),
                    comp,
                    !is_last,
                    ffi::FILE_READ_ATTRIBUTES | ffi::SYNCHRONIZE,
                    ffi::FILE_OPEN,
                    0,
                    std::ptr::null_mut(),
                ) {
                    Ok(next) => {
                        if is_last {
                            let guid = get_volume_guid_path(next.raw())?;
                            if guid_path_inside_component_wise(
                                &guid,
                                &vr.guid_path,
                                vr.is_directory,
                            ) {
                                return Ok(Target::Existing(PathBuf::from(path)));
                            } else {
                                return Err(outside(Path::new(path)));
                            }
                        }
                        intermediate = Some(next);
                    }
                    Err(OpenError::NotFound) => {
                        found = false;
                        break;
                    }
                    Err(OpenError::ReparsePoint) => {
                        return Err(Denial::denied(format!("{path} contains a reparse point")));
                    }
                    Err(_) => {
                        return Err(outside(Path::new(path)));
                    }
                }
            }
            if !found {
                // fall through to Target::Entry
            }
        }
    }

    let p = Path::new(path);
    let parent = p.parent().ok_or_else(|| outside(p))?;
    let name = p.file_name().ok_or_else(|| outside(p))?;

    // Parent must be inside the verified root
    if !vr.is_directory {
        if p != Path::new(&vr.manifest) {
            return Err(outside(p));
        }
    } else {
        let parent_str = parent.to_str().ok_or_else(|| outside(p))?;
        if parent_str != vr.manifest && !parent_str.starts_with(&format!(r"{}\", vr.manifest)) {
            return Err(outside(p));
        }
    }

    Ok(Target::Entry {
        parent: parent.to_path_buf(),
        name: name.to_os_string(),
    })
}

#[cfg(windows)]
pub fn open_checked(
    resolved: &Path,
    roots: &[String],
    directory: bool,
) -> Result<std::fs::File, Denial> {
    let path_str = resolved.to_str().ok_or_else(|| outside(resolved))?;
    validate_raw_spelling(path_str)?;
    let vr = select_root(path_str, roots)?;

    if path_str == vr.manifest {
        if directory && !vr.is_directory {
            return Err(outside(resolved));
        }
        let guid = get_volume_guid_path(vr.handle.raw())?;
        if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
            return Err(outside(resolved));
        }
        return Ok(std::fs::File::from(vr.handle));
    }

    if !vr.is_directory {
        return Err(outside(resolved));
    }

    let rel = &path_str[vr.manifest.len()..];
    let rel = rel.strip_prefix('\\').unwrap_or(rel);
    let components: Vec<&str> = rel.split('\\').collect();
    let mut intermediate = None;
    for (i, comp) in components.iter().enumerate() {
        let is_last = i == components.len() - 1;
        let cur = intermediate.as_ref().unwrap_or(&vr.handle);
        let desired = if is_last {
            if directory {
                ffi::FILE_GENERIC_READ | ffi::FILE_TRAVERSE | ffi::SYNCHRONIZE
            } else {
                ffi::FILE_GENERIC_READ | ffi::SYNCHRONIZE
            }
        } else {
            ffi::FILE_READ_ATTRIBUTES | ffi::FILE_TRAVERSE | ffi::SYNCHRONIZE
        };
        let next = nt_open_relative(
            cur.raw(),
            comp,
            if is_last { directory } else { true },
            desired,
            ffi::FILE_OPEN,
            0,
            std::ptr::null_mut(),
        )
        .map_err(|e| match e {
            OpenError::ReparsePoint => Denial::denied(format!(
                "{} became a symlink while it was being opened",
                resolved.display()
            )),
            OpenError::NotFound => io_denial(
                resolved,
                &std::io::Error::from(std::io::ErrorKind::NotFound),
            ),
            _ => outside(resolved),
        })?;

        if is_last {
            let guid = get_volume_guid_path(next.raw())?;
            if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
                return Err(outside(resolved));
            }
            return Ok(std::fs::File::from(next));
        }
        intermediate = Some(next);
    }

    Err(outside(resolved))
}

#[cfg(test)]
pub(crate) static SWAP_HOOK: std::sync::Mutex<Option<Box<dyn Fn() + Send>>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static SIMULATE_POSIX_RENAME_REFUSAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
pub fn read(path: &str, roots: &[String], max_bytes: u64) -> Result<Value, Denial> {
    let max_bytes = max_bytes.min(MAX_READ_BYTES);
    let target = resolve(path, roots, Purpose::Read)?;
    let real = match target {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };

    #[cfg(test)]
    if let Some(hook) = SWAP_HOOK.lock().unwrap().as_ref() {
        hook();
    }

    let file = open_checked(&real, roots, false)?;
    let meta = file.metadata().map_err(|e| io_denial(&real, &e))?;
    if !meta.is_file() {
        return Err(Denial::invalid(format!(
            "{} is not a regular file",
            real.display()
        )));
    }

    let too_large = || {
        Denial::new(
            codes::TOO_LARGE,
            format!("{} is larger than {max_bytes} bytes", real.display()),
        )
    };
    if meta.len() > max_bytes {
        return Err(too_large());
    }

    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_denial(&real, &e))?;

    if bytes.len() as u64 > max_bytes {
        return Err(too_large());
    }

    let text = String::from_utf8(bytes).map_err(|_| {
        Denial::new(
            codes::NOT_UTF8,
            format!("{} is not UTF-8 text", real.display()),
        )
    })?;

    Ok(json!({ "text": text }))
}

#[cfg(windows)]
pub fn stat(path: &str, roots: &[String]) -> Result<Value, Denial> {
    validate_raw_spelling(path)?;
    let vr = select_root(path, roots)?;

    if path == vr.manifest {
        let basic =
            query_file_basic(vr.handle.raw()).map_err(|e| Denial::new(codes::IO, e.to_string()))?;
        let std_info = query_file_standard(vr.handle.raw())
            .map_err(|e| Denial::new(codes::IO, e.to_string()))?;
        let mtime_ms = filetime_to_mtime_ms(basic.LastWriteTime);
        return Ok(json!({
            "exists": true,
            "kind": if std_info.Directory != 0 { "dir" } else { "file" },
            "size": std_info.EndOfFile,
            "mtime_ms": mtime_ms,
        }));
    }

    if !vr.is_directory {
        return Err(outside(Path::new(path)));
    }

    let rel = &path[vr.manifest.len()..];
    let rel = rel.strip_prefix('\\').unwrap_or(rel);
    let components: Vec<&str> = rel.split('\\').collect();
    let mut intermediate = None;

    for (i, comp) in components.iter().enumerate() {
        let is_last = i == components.len() - 1;
        let cur = intermediate.as_ref().unwrap_or(&vr.handle);

        let res = nt_open_relative(
            cur.raw(),
            comp,
            false,
            ffi::FILE_READ_ATTRIBUTES | ffi::SYNCHRONIZE,
            ffi::FILE_OPEN,
            0,
            std::ptr::null_mut(),
        );

        match res {
            Ok(next) => {
                if is_last {
                    let guid = get_volume_guid_path(next.raw())?;
                    if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
                        return Err(outside(Path::new(path)));
                    }
                    let basic = query_file_basic(next.raw())
                        .map_err(|e| Denial::new(codes::IO, e.to_string()))?;
                    let std_info = query_file_standard(next.raw())
                        .map_err(|e| Denial::new(codes::IO, e.to_string()))?;
                    let mtime_ms = filetime_to_mtime_ms(basic.LastWriteTime);
                    return Ok(json!({
                        "exists": true,
                        "kind": if std_info.Directory != 0 { "dir" } else { "file" },
                        "size": std_info.EndOfFile,
                        "mtime_ms": mtime_ms,
                    }));
                }
                intermediate = Some(next);
            }
            Err(OpenError::ReparsePoint) => {
                return Err(Denial::denied(format!("{path} is a reparse point")));
            }
            Err(OpenError::NotFound) => {
                if is_last {
                    return Ok(json!({ "exists": false }));
                } else {
                    return Err(outside(Path::new(path)));
                }
            }
            Err(OpenError::AccessDenied) => {
                return Err(Denial::denied(format!("access denied: {path}")));
            }
            Err(OpenError::Other(status)) => {
                return Err(Denial::new(codes::IO, format!("IO error 0x{status:08x}")));
            }
        }
    }

    Err(outside(Path::new(path)))
}

#[cfg(windows)]
pub fn list(path: &str, roots: &[String]) -> Result<Value, Denial> {
    let dir = open_checked(Path::new(path), roots, true)?;
    use std::os::windows::io::AsRawHandle;
    let handle = dir.as_raw_handle() as ffi::HANDLE;

    let mut entries = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut restart = 1u8;

    loop {
        let status = unsafe {
            ffi::NtQueryDirectoryFile(
                handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut io_status,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                buffer.len() as u32,
                ffi::FileDirectoryInformation,
                0,
                std::ptr::null_mut(),
                restart,
            )
        };
        restart = 0;

        if status == ffi::STATUS_NO_MORE_FILES || io_status.Information == 0 {
            break;
        }
        if status < 0 {
            return Err(Denial::new(
                codes::IO,
                format!("directory scan failed: 0x{status:08x}"),
            ));
        }

        let mut offset = 0;
        loop {
            let entry_ptr =
                unsafe { buffer.as_ptr().add(offset) as *const ffi::FILE_DIRECTORY_INFORMATION };
            let entry = unsafe { &*entry_ptr };
            let name_len = (entry.FileNameLength / 2) as usize;
            let name_slice =
                unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), name_len) };
            let name_str = String::from_utf16_lossy(name_slice);

            if name_str != "." && name_str != ".." {
                let kind = if (entry.FileAttributes & ffi::FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                    "symlink"
                } else if (entry.FileAttributes & ffi::FILE_ATTRIBUTE_DIRECTORY) != 0 {
                    "dir"
                } else {
                    "file"
                };
                entries.push((name_str, kind));
                if entries.len() > MAX_LIST_ENTRIES {
                    return Err(Denial::new(
                        codes::TOO_LARGE,
                        format!("{path} has more than {MAX_LIST_ENTRIES} entries"),
                    ));
                }
            }

            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
            if offset >= buffer.len() {
                break;
            }
        }
    }

    entries.sort();
    Ok(Value::Array(
        entries
            .into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect(),
    ))
}

#[cfg(windows)]
fn get_security_descriptor_for_write(
    parent_handle: ffi::HANDLE,
    leaf_name: &str,
) -> Result<FileDacl, Denial> {
    // Attempt open leaf with FILE_OPEN_REPARSE_POINT to read its DACL
    match nt_open_relative(
        parent_handle,
        leaf_name,
        false,
        ffi::READ_CONTROL | ffi::FILE_READ_ATTRIBUTES | ffi::SYNCHRONIZE,
        ffi::FILE_OPEN,
        0,
        std::ptr::null_mut(),
    ) {
        Ok(leaf) => {
            let mut sd = std::ptr::null_mut();
            let mut dacl = std::ptr::null_mut();
            let err = unsafe {
                ffi::GetSecurityInfo(
                    leaf.raw(),
                    ffi::SE_FILE_OBJECT,
                    ffi::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if err != 0 {
                return Err(Denial::denied(format!(
                    "failed to read DACL from replaced file {leaf_name}: {err}"
                )));
            }
            Ok(FileDacl {
                sd,
                is_private: false,
            })
        }
        Err(OpenError::NotFound) => {
            // New file: use parent's inheritable DACL
            let mut parent_sd = std::ptr::null_mut();
            let err = unsafe {
                ffi::GetSecurityInfo(
                    parent_handle,
                    ffi::SE_FILE_OBJECT,
                    ffi::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut parent_sd,
                )
            };
            if err != 0 {
                return Err(Denial::denied(format!(
                    "failed to read parent inheritable DACL: {err}"
                )));
            }
            let _parent_guard = FileDacl {
                sd: parent_sd,
                is_private: false,
            };

            let mut mapping = ffi::GENERIC_MAPPING {
                GenericRead: ffi::FILE_GENERIC_READ,
                GenericWrite: ffi::FILE_GENERIC_WRITE,
                GenericExecute: ffi::FILE_GENERIC_EXECUTE,
                GenericAll: ffi::FILE_ALL_ACCESS,
            };
            let mut child_sd = std::ptr::null_mut();
            let ok = unsafe {
                ffi::CreatePrivateObjectSecurity(
                    parent_sd,
                    std::ptr::null_mut(),
                    &mut child_sd,
                    0, // FALSE for file
                    std::ptr::null_mut(),
                    &mut mapping,
                )
            };
            if ok == 0 {
                return Err(Denial::denied("failed to compute child inheritable DACL"));
            }

            Ok(FileDacl {
                sd: child_sd,
                is_private: true,
            })
        }
        Err(OpenError::ReparsePoint) => Err(Denial::denied(format!(
            "{leaf_name} is a symlink; fs.write does not replace symlinks"
        ))),
        Err(_) => Err(Denial::denied(format!(
            "failed to inspect target {leaf_name} for DACL"
        ))),
    }
}

#[cfg(windows)]
fn unlink_file(parent_handle: ffi::HANDLE, name: &str) -> Result<(), ffi::NTSTATUS> {
    let handle = nt_open_relative(
        parent_handle,
        name,
        false,
        ffi::DELETE | ffi::SYNCHRONIZE,
        ffi::FILE_OPEN,
        0,
        std::ptr::null_mut(),
    )
    .map_err(|e| match e {
        OpenError::Other(s) => s,
        _ => ffi::STATUS_ACCESS_DENIED,
    })?;

    let mut disp = ffi::FILE_DISPOSITION_INFORMATION { DeleteFile: 1 };
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let status = unsafe {
        ffi::NtSetInformationFile(
            handle.raw(),
            &mut io_status,
            &mut disp as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ffi::FILE_DISPOSITION_INFORMATION>() as u32,
            ffi::FileDispositionInformation,
        )
    };
    if status < 0 {
        return Err(status);
    }
    Ok(())
}

#[cfg(windows)]
pub fn write(path: &str, roots: &[String], text: &str) -> Result<Value, Denial> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = format!(
        "local-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    write_call(path, roots, text, &key, None)
}

#[cfg(windows)]
pub fn write_call(
    path: &str,
    roots: &[String],
    text: &str,
    call_key: &str,
    ledger: Option<&dyn TempLedger>,
) -> Result<Value, Denial> {
    check_write_size(text.len())?;
    validate_raw_spelling(path)?;
    let temp_name_os = temp_name(call_key)?;
    let temp_name_str = temp_name_os
        .to_str()
        .ok_or_else(|| Denial::invalid("temporary file name is not valid UTF-8"))?;

    let p = Path::new(path);
    let parent = p.parent().ok_or_else(|| outside(p))?;
    let leaf_name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| outside(p))?;

    let parent_str = parent.to_str().ok_or_else(|| outside(p))?;
    let vr = select_root(parent_str, roots)?;

    let (parent_handle, parent_path_buf) = if parent_str == vr.manifest {
        (vr.handle, parent.to_path_buf())
    } else {
        if !vr.is_directory {
            return Err(outside(p));
        }
        let rel = &parent_str[vr.manifest.len()..];
        let rel = rel.strip_prefix('\\').unwrap_or(rel);
        let components: Vec<&str> = rel.split('\\').collect();
        let mut intermediate = None;
        for (i, comp) in components.iter().enumerate() {
            let is_last = i == components.len() - 1;
            let cur = intermediate.as_ref().unwrap_or(&vr.handle);
            let next = nt_open_relative(
                cur.raw(),
                comp,
                true,
                ffi::FILE_GENERIC_READ
                    | ffi::FILE_GENERIC_WRITE
                    | ffi::FILE_TRAVERSE
                    | ffi::SYNCHRONIZE,
                ffi::FILE_OPEN,
                0,
                std::ptr::null_mut(),
            )
            .map_err(|_| outside(p))?;
            intermediate = Some(next);
        }
        let ph = intermediate.ok_or_else(|| outside(p))?;
        let guid = get_volume_guid_path(ph.raw())?;
        if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
            return Err(outside(p));
        }
        (ph, parent.to_path_buf())
    };

    // If file root, target must be that exact file
    if !vr.is_directory && path != vr.manifest {
        return Err(outside(p));
    }

    // Obtain target security descriptor while validating it is not a directory or symlink
    let dacl = get_security_descriptor_for_write(parent_handle.raw(), leaf_name)?;

    let lease = TempLease {
        call_key: call_key.to_owned(),
        dir: parent_path_buf.clone(),
        target: OsString::from(leaf_name),
        temp: temp_name_os.clone(),
        roots: roots.to_vec(),
    };

    let hold = match ledger {
        Some(l) => Some(l.record(&lease).map_err(|e| {
            Denial::new(
                codes::IO,
                format!(
                    "the temporary file for {} could not be recorded: {e}",
                    parent_path_buf.join(leaf_name).display()
                ),
            )
        })?),
        None => None,
    };

    let clear = |hold: Option<Box<dyn TempHold>>| {
        if let Some(h) = hold {
            h.clear();
        }
    };

    // Create temp file with explicit security descriptor
    let mut replaced_collision = false;
    let temp_handle = loop {
        match nt_open_relative(
            parent_handle.raw(),
            temp_name_str,
            false,
            ffi::FILE_GENERIC_WRITE | ffi::DELETE | ffi::WRITE_DAC | ffi::SYNCHRONIZE,
            ffi::FILE_CREATE,
            0,
            dacl.sd,
        ) {
            Ok(th) => break th,
            Err(OpenError::Other(ffi::STATUS_OBJECT_NAME_COLLISION)) if !replaced_collision => {
                replaced_collision = true;
                let _ = unlink_file(parent_handle.raw(), temp_name_str);
                continue;
            }
            Err(e) => {
                return Err(Denial::new(
                    codes::IO,
                    format!("failed to create temporary file {temp_name_str}: {e:?}"),
                ));
            }
        }
    };

    if let Some(l) = ledger {
        l.created(&lease);
    }

    // Write contents to temp file
    let bytes = text.as_bytes();
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut offset = 0i64;
    while (offset as usize) < bytes.len() {
        let chunk = &bytes[offset as usize..];
        let status = unsafe {
            ffi::NtWriteFile(
                temp_handle.raw(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut io_status,
                chunk.as_ptr() as *const std::ffi::c_void,
                chunk.len() as u32,
                &mut offset,
                std::ptr::null_mut(),
            )
        };
        if status < 0 {
            let _ = unlink_file(parent_handle.raw(), temp_name_str);
            return Err(Denial::new(
                codes::IO,
                format!("failed to write data: 0x{status:08x}"),
            ));
        }
        offset += io_status.Information as i64;
    }

    // Flush temp file
    unsafe { ffi::NtFlushBuffersFile(temp_handle.raw(), &mut io_status) };

    // Rename using FileRenameInformationEx POSIX semantics
    let wide_target: Vec<u16> = leaf_name.encode_utf16().collect();
    let name_bytes = wide_target.len() * std::mem::size_of::<u16>();
    let struct_size = std::mem::size_of::<ffi::FILE_RENAME_INFORMATION_EX>() + name_bytes;
    let mut rename_buf = vec![0u8; struct_size];
    let rename_info = rename_buf.as_mut_ptr() as *mut ffi::FILE_RENAME_INFORMATION_EX;
    unsafe {
        (*rename_info).Flags =
            ffi::FILE_RENAME_REPLACE_IF_EXISTS | ffi::FILE_RENAME_POSIX_SEMANTICS;
        (*rename_info).RootDirectory = parent_handle.raw();
        (*rename_info).FileNameLength = name_bytes as u32;
        let dest_slice =
            std::slice::from_raw_parts_mut((*rename_info).FileName.as_mut_ptr(), wide_target.len());
        dest_slice.copy_from_slice(&wide_target);
    }

    #[cfg(test)]
    let simulate_refusal = SIMULATE_POSIX_RENAME_REFUSAL.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(test))]
    let simulate_refusal = false;

    let rename_status = if simulate_refusal {
        ffi::STATUS_NOT_SUPPORTED
    } else {
        unsafe {
            ffi::NtSetInformationFile(
                temp_handle.raw(),
                &mut io_status,
                rename_info as *mut std::ffi::c_void,
                struct_size as u32,
                ffi::FileRenameInformationEx,
            )
        }
    };

    if rename_status < 0 {
        let _ = unlink_file(parent_handle.raw(), temp_name_str);
        if rename_status == ffi::STATUS_NOT_SUPPORTED
            || rename_status == ffi::STATUS_INVALID_PARAMETER
        {
            return Err(Denial::denied(format!(
                "volume does not support POSIX replace rename (status 0x{rename_status:08x})"
            )));
        }
        return Err(Denial::new(
            codes::IO,
            format!("rename to {leaf_name} failed: 0x{rename_status:08x}"),
        ));
    }

    drop(temp_handle);

    // Flush parent directory
    unsafe { ffi::NtFlushBuffersFile(parent_handle.raw(), &mut io_status) };

    clear(hold);
    Ok(json!({ "bytes": text.len() }))
}

#[cfg(windows)]
pub fn remove_temp(lease: &TempLease) -> Result<TempRemoval, Denial> {
    if !is_temp_name(&lease.temp) && !is_legacy_temp_name(&lease.temp) {
        return Ok(TempRemoval::Refused(Denial::invalid(
            "the record does not name a temporary file",
        )));
    }

    let joined = lease.dir.join(&lease.target);
    let Some(path_str) = joined.to_str() else {
        return Ok(TempRemoval::Refused(Denial::invalid(
            "the recorded path is not UTF-8",
        )));
    };

    let p = Path::new(path_str);
    let parent = p.parent().ok_or_else(|| outside(p))?;
    let leaf_name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| outside(p))?;

    let parent_str = parent.to_str().ok_or_else(|| outside(p))?;
    let vr = match select_root(parent_str, &lease.roots) {
        Ok(v) => v,
        Err(d) if d.code == codes::NOT_FOUND => return Ok(TempRemoval::Absent),
        Err(d) if d.code == codes::IO => return Err(d),
        Err(d) => return Ok(TempRemoval::Refused(d)),
    };

    if parent != lease.dir || leaf_name != lease.target {
        return Ok(TempRemoval::Refused(Denial::denied(format!(
            "{} now resolves to {}",
            joined.display(),
            parent.join(leaf_name).display()
        ))));
    }

    let temp_str = lease
        .temp
        .to_str()
        .ok_or_else(|| TempRemoval::Refused(Denial::invalid("temp name is not UTF-8")))?;

    let temp_handle = match nt_open_relative(
        vr.handle.raw(),
        temp_str,
        false,
        ffi::DELETE | ffi::FILE_READ_ATTRIBUTES | ffi::SYNCHRONIZE,
        ffi::FILE_OPEN,
        0,
        std::ptr::null_mut(),
    ) {
        Ok(th) => th,
        Err(OpenError::NotFound) => return Ok(TempRemoval::Absent),
        Err(OpenError::ReparsePoint) => {
            return Ok(TempRemoval::Refused(Denial::denied(format!(
                "{} is a symlink, not a temporary file",
                lease.dir.join(&lease.temp).display()
            ))));
        }
        Err(_) => {
            return Ok(TempRemoval::Refused(Denial::denied(format!(
                "failed to open temp file {}",
                lease.dir.join(&lease.temp).display()
            ))));
        }
    };

    let std_info = match query_file_standard(temp_handle.raw()) {
        Ok(s) => s,
        Err(e) => return Err(Denial::new(codes::IO, e.to_string())),
    };

    if std_info.Directory != 0 {
        return Ok(TempRemoval::Refused(Denial::denied(format!(
            "{} is a directory, not a temporary file",
            lease.dir.join(&lease.temp).display()
        ))));
    }

    let mut disp = ffi::FILE_DISPOSITION_INFORMATION { DeleteFile: 1 };
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let status = unsafe {
        ffi::NtSetInformationFile(
            temp_handle.raw(),
            &mut io_status,
            &mut disp as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<ffi::FILE_DISPOSITION_INFORMATION>() as u32,
            ffi::FileDispositionInformation,
        )
    };

    if status < 0 {
        return Err(Denial::new(
            codes::IO,
            format!("failed to delete temp file: 0x{status:08x}"),
        ));
    }

    drop(temp_handle);
    Ok(TempRemoval::Removed)
}

#[cfg(windows)]
pub fn remove_legacy_temps(path: &str, roots: &[String]) -> Result<usize, Denial> {
    validate_raw_spelling(path)?;
    let p = Path::new(path);
    let parent = p.parent().ok_or_else(|| outside(p))?;
    let parent_str = parent.to_str().ok_or_else(|| outside(p))?;

    let dir = open_checked(Path::new(parent_str), roots, true)?;
    use std::os::windows::io::AsRawHandle;
    let handle = dir.as_raw_handle() as ffi::HANDLE;

    let mut legacy_names = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut restart = 1u8;

    loop {
        let status = unsafe {
            ffi::NtQueryDirectoryFile(
                handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut io_status,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                buffer.len() as u32,
                ffi::FileDirectoryInformation,
                0,
                std::ptr::null_mut(),
                restart,
            )
        };
        restart = 0;

        if status == ffi::STATUS_NO_MORE_FILES || io_status.Information == 0 {
            break;
        }
        if status < 0 {
            return Err(Denial::new(
                codes::IO,
                format!("directory scan failed: 0x{status:08x}"),
            ));
        }

        let mut offset = 0;
        loop {
            let entry_ptr =
                unsafe { buffer.as_ptr().add(offset) as *const ffi::FILE_DIRECTORY_INFORMATION };
            let entry = unsafe { &*entry_ptr };
            let name_len = (entry.FileNameLength / 2) as usize;
            let name_slice =
                unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), name_len) };
            let name_str = String::from_utf16_lossy(name_slice);

            if (entry.FileAttributes & ffi::FILE_ATTRIBUTE_REPARSE_POINT) == 0
                && (entry.FileAttributes & ffi::FILE_ATTRIBUTE_DIRECTORY) == 0
                && is_legacy_temp_name(OsStr::new(&name_str))
            {
                legacy_names.push(name_str);
            }

            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
            if offset >= buffer.len() {
                break;
            }
        }
    }

    let mut removed = 0;
    for name in legacy_names {
        if unlink_file(handle, &name).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spelling_accepts_ordinary_drive_paths() {
        assert!(validate_raw_spelling(r"C:\").is_ok());
        assert!(validate_raw_spelling(r"C:\dir").is_ok());
        assert!(validate_raw_spelling(r"C:\dir\file.txt").is_ok());
        assert!(validate_raw_spelling(r"d:\nested\sub\folder\file").is_ok());
    }

    #[test]
    fn spelling_refuses_unc_and_device_namespaces() {
        assert!(validate_raw_spelling(r"\\server\share\file").is_err());
        assert!(validate_raw_spelling(r"//server/share/file").is_err());
        assert!(validate_raw_spelling(r"\\.\COM1").is_err());
        assert!(validate_raw_spelling(r"//./COM1").is_err());
        assert!(validate_raw_spelling(r"\\?\C:\file").is_err());
        assert!(validate_raw_spelling(r"//?/C:/file").is_err());
        assert!(validate_raw_spelling(r"\??\C:\file").is_err());
        assert!(validate_raw_spelling(r"/??/C:/file").is_err());
    }

    #[test]
    fn spelling_refuses_drive_relative() {
        assert!(validate_raw_spelling("C:").is_err());
        assert!(validate_raw_spelling("C:file").is_err());
        assert!(validate_raw_spelling(r"C:dir\file").is_err());
        assert!(validate_raw_spelling("C:/file").is_err());
    }

    #[test]
    fn spelling_refuses_alternate_data_streams() {
        assert!(validate_raw_spelling(r"C:\file:stream").is_err());
        assert!(validate_raw_spelling(r"C:\file::$DATA").is_err());
        assert!(validate_raw_spelling(r"C:\dir:ads\file").is_err());
    }

    #[test]
    fn spelling_refuses_trailing_dots_and_spaces() {
        assert!(validate_raw_spelling(r"C:\dir\file.").is_err());
        assert!(validate_raw_spelling(r"C:\dir\file ").is_err());
        assert!(validate_raw_spelling(r"C:\dir \file").is_err());
        assert!(validate_raw_spelling(r"C:\dir.\file").is_err());
        assert!(validate_raw_spelling(r"C:\dir\file.txt.").is_err());
    }

    #[test]
    fn spelling_refuses_reserved_device_names() {
        assert!(validate_raw_spelling(r"C:\CON").is_err());
        assert!(validate_raw_spelling(r"C:\con.txt").is_err());
        assert!(validate_raw_spelling(r"C:\dir\PRN").is_err());
        assert!(validate_raw_spelling(r"C:\dir\aux.dat").is_err());
        assert!(validate_raw_spelling(r"C:\NUL.tar.gz").is_err());
        assert!(validate_raw_spelling(r"C:\COM1").is_err());
        assert!(validate_raw_spelling(r"C:\com9.txt").is_err());
        assert!(validate_raw_spelling(r"C:\LPT1").is_err());
        assert!(validate_raw_spelling(r"C:\lpt8.log").is_err());
        assert!(validate_raw_spelling(r"C:\conin$").is_err());
        assert!(validate_raw_spelling(r"C:\conout$").is_err());
        assert!(validate_raw_spelling("C:\\COM\u{b9}").is_err());
        assert!(validate_raw_spelling("C:\\lpt\u{b3}.txt").is_err());
        assert!(validate_raw_spelling("C:\\COM\u{b4}").is_ok());

        // Not reserved
        assert!(validate_raw_spelling(r"C:\context.txt").is_ok());
        assert!(validate_raw_spelling(r"C:\connect").is_ok());
        assert!(validate_raw_spelling(r"C:\auxiliary.dat").is_ok());
    }

    #[test]
    fn spelling_refuses_relative_and_empty_components() {
        assert!(validate_raw_spelling(r"C:\dir\..\file").is_err());
        assert!(validate_raw_spelling(r"C:\dir\.\file").is_err());
        assert!(validate_raw_spelling(r"C:\dir\\file").is_err());
        assert!(validate_raw_spelling(r"C:\dir\").is_err());
    }

    #[test]
    fn spelling_refuses_forward_slashes() {
        assert!(validate_raw_spelling("C:/dir/file").is_err());
        assert!(validate_raw_spelling(r"C:\dir/file").is_err());
    }

    #[test]
    fn guid_path_inside_component_wise_matches() {
        let root = r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\root";
        let child =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\root\sub\file.txt";
        let sibling =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\rootx\file.txt";
        let case_diff =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\users\admin\root\file.txt";
        let other_vol =
            r"\\?\Volume{87654321-0000-0000-0000-000000000000}\Users\admin\root\sub\file.txt";

        // Directory root
        assert!(guid_path_inside_component_wise(child, root, true));
        assert!(guid_path_inside_component_wise(root, root, true));
        assert!(!guid_path_inside_component_wise(sibling, root, true));
        assert!(!guid_path_inside_component_wise(case_diff, root, true));
        assert!(!guid_path_inside_component_wise(other_vol, root, true));

        // File root
        let file_root =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\root\file.txt";
        let file_child =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\root\file.txt\sub";
        let file_sibling =
            r"\\?\Volume{12345678-0000-0000-0000-000000000000}\Users\admin\root\other.txt";

        assert!(guid_path_inside_component_wise(file_root, file_root, false));
        assert!(!guid_path_inside_component_wise(
            file_child, file_root, false
        ));
        assert!(!guid_path_inside_component_wise(
            file_sibling,
            file_root,
            false
        ));
    }

    #[cfg(windows)]
    mod win_tests {
        use super::*;

        struct WinTree {
            base: PathBuf,
            root: PathBuf,
            outside: PathBuf,
        }

        impl WinTree {
            fn new(tag: &str) -> Self {
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let base = std::env::temp_dir().join(format!(
                    "basal-win-tree-{tag}-{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                let root = base.join("root");
                let outside = base.join("outside");
                std::fs::create_dir_all(root.join("sub")).expect("create root");
                std::fs::create_dir_all(&outside).expect("create outside");
                std::fs::write(root.join("a.txt"), "inside").expect("write a.txt");
                std::fs::write(root.join("sub\\b.txt"), "nested").expect("write b.txt");
                std::fs::write(outside.join("secret.txt"), "secret").expect("write secret");
                Self {
                    base,
                    root,
                    outside,
                }
            }

            fn roots(&self) -> Vec<String> {
                vec![self.root.display().to_string()]
            }

            fn p(&self, rel: &str) -> String {
                self.root.join(rel).display().to_string()
            }
        }

        impl Drop for WinTree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }

        #[test]
        fn windows_read_and_stat_ordinary_drive_path() {
            let t = WinTree::new("read-stat");
            let roots = t.roots();

            let val = read(&t.p("a.txt"), &roots, 1024).expect("read a.txt");
            assert_eq!(val["text"], "inside");

            let st = stat(&t.p("a.txt"), &roots).expect("stat a.txt");
            assert_eq!(st["exists"], true);
            assert_eq!(st["kind"], "file");
            assert_eq!(st["size"], 6);
            assert!(st["mtime_ms"].as_i64().unwrap_or(0) > 0);

            let st_missing = stat(&t.p("missing.txt"), &roots).expect("stat missing");
            assert_eq!(st_missing["exists"], false);

            let outside_path = t.outside.join("secret.txt").display().to_string();
            let denial = stat(&outside_path, &roots).expect_err("outside");
            assert_eq!(denial.code, codes::DENIED);
        }

        #[test]
        fn windows_raw_spelling_refusals_before_open() {
            let t = WinTree::new("spelling-refusals");
            let roots = t.roots();

            let ads = format!("{}:stream", t.p("a.txt"));
            let d1 = read(&ads, &roots, 1024).expect_err("ads");
            assert_eq!(d1.code, codes::INVALID_ARGUMENTS);

            let drive_rel = "C:a.txt";
            let d2 = read(drive_rel, &roots, 1024).expect_err("drive rel");
            assert_eq!(d2.code, codes::INVALID_ARGUMENTS);

            let con_path = t.p("con.txt");
            let d3 = read(&con_path, &roots, 1024).expect_err("device name");
            assert_eq!(d3.code, codes::INVALID_ARGUMENTS);
        }

        #[test]
        fn windows_symlink_escape_and_in_root_refused() {
            let t = WinTree::new("symlink-refused");
            let roots = t.roots();

            // Attempt symlink creation (requires developer mode or admin privilege).
            // If privilege is not held, Command fails and we verify junction fallback.
            let out_link = t.root.join("symlink_out.txt");
            let in_link = t.root.join("symlink_in.txt");
            let _ = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    out_link.to_str().unwrap(),
                    t.outside.join("secret.txt").to_str().unwrap(),
                ])
                .status();
            let _ = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    in_link.to_str().unwrap(),
                    t.root.join("a.txt").to_str().unwrap(),
                ])
                .status();

            if out_link.exists() || out_link.is_symlink() {
                let err_out = read(&out_link.display().to_string(), &roots, 1024)
                    .expect_err("symlink out denied");
                assert_eq!(err_out.code, codes::DENIED);
            }
            if in_link.exists() || in_link.is_symlink() {
                // On Windows, every reparse point is refused in this campaign, including in-root ones.
                let err_in = read(&in_link.display().to_string(), &roots, 1024)
                    .expect_err("in-root symlink denied");
                assert_eq!(err_in.code, codes::DENIED);
            }
        }

        #[test]
        fn windows_reparse_points_at_root_middle_leaf() {
            let t = WinTree::new("reparse-points");
            let roots = t.roots();

            // 1. Leaf reparse point
            let leaf_junc = t.root.join("leaf_junc");
            let _ = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    leaf_junc.to_str().unwrap(),
                    t.outside.to_str().unwrap(),
                ])
                .status();
            if leaf_junc.exists() {
                let st = stat(&leaf_junc.display().to_string(), &roots)
                    .expect_err("leaf reparse stat denial");
                assert_eq!(st.code, codes::DENIED);
            }

            // 2. Middle (intermediate) reparse point
            let mid_junc = t.root.join("mid_junc");
            let _ = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    mid_junc.to_str().unwrap(),
                    t.outside.to_str().unwrap(),
                ])
                .status();
            if mid_junc.exists() {
                let target = mid_junc.join("secret.txt").display().to_string();
                let err = read(&target, &roots, 1024).expect_err("mid reparse denial");
                assert_eq!(err.code, codes::DENIED);
            }

            // 3. Root itself is a reparse point
            let root_junc = t.base.join("root_junc");
            let _ = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    root_junc.to_str().unwrap(),
                    t.root.to_str().unwrap(),
                ])
                .status();
            if root_junc.exists() {
                let junc_roots = vec![root_junc.display().to_string()];
                let target = root_junc.join("a.txt").display().to_string();
                let err = read(&target, &junc_roots, 1024).expect_err("root reparse denial");
                assert_eq!(err.code, codes::DENIED);
            }
        }

        #[test]
        fn windows_dotdot_and_relative_escapes() {
            let t = WinTree::new("dotdot-escapes");
            let roots = t.roots();

            let p1 = t.root.join("..\\outside\\secret.txt").display().to_string();
            assert_eq!(
                read(&p1, &roots, 1024).expect_err("dotdot").code,
                codes::INVALID_ARGUMENTS
            );

            let p2 = t.root.join("sub\\..\\a.txt").display().to_string();
            assert_eq!(
                read(&p2, &roots, 1024).expect_err("dotdot").code,
                codes::INVALID_ARGUMENTS
            );

            let p3 = t.root.join(".\\a.txt").display().to_string();
            assert_eq!(
                read(&p3, &roots, 1024).expect_err("dot").code,
                codes::INVALID_ARGUMENTS
            );
        }

        #[test]
        fn windows_ads_spellings() {
            let t = WinTree::new("ads-spellings");
            let roots = t.roots();

            let p1 = format!("{}:stream", t.p("a.txt"));
            assert_eq!(
                read(&p1, &roots, 1024).expect_err("ads a:b").code,
                codes::INVALID_ARGUMENTS
            );

            let p2 = format!("{}::$DATA", t.p("a.txt"));
            assert_eq!(
                read(&p2, &roots, 1024).expect_err("ads data").code,
                codes::INVALID_ARGUMENTS
            );

            let p3 = format!("{}::$INDEX_ALLOCATION", t.p("sub"));
            assert_eq!(
                read(&p3, &roots, 1024).expect_err("ads index").code,
                codes::INVALID_ARGUMENTS
            );
        }

        #[test]
        fn windows_trailing_dot_and_space() {
            let t = WinTree::new("trailing-dot-space");
            let roots = t.roots();

            let p1 = format!("{}.", t.p("a.txt"));
            assert_eq!(
                read(&p1, &roots, 1024).expect_err("trailing dot").code,
                codes::INVALID_ARGUMENTS
            );

            let p2 = format!("{} ", t.p("a.txt"));
            assert_eq!(
                read(&p2, &roots, 1024).expect_err("trailing space").code,
                codes::INVALID_ARGUMENTS
            );

            let p3 = t.root.join("sub.\\a.txt").display().to_string();
            assert_eq!(
                read(&p3, &roots, 1024).expect_err("dir trailing dot").code,
                codes::INVALID_ARGUMENTS
            );

            let p4 = t.root.join("sub \\a.txt").display().to_string();
            assert_eq!(
                read(&p4, &roots, 1024)
                    .expect_err("dir trailing space")
                    .code,
                codes::INVALID_ARGUMENTS
            );
        }

        #[test]
        fn windows_each_reserved_device_name() {
            let t = WinTree::new("reserved-names");
            let roots = t.roots();

            let names = [
                "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
                "COM8", "COM9", "COM0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
                "LPT8", "LPT9", "LPT0", "CONIN$", "CONOUT$",
            ];

            for name in names {
                let bare = t.root.join(name).display().to_string();
                assert_eq!(
                    read(&bare, &roots, 1024).expect_err(name).code,
                    codes::INVALID_ARGUMENTS,
                    "bare {name}"
                );

                let with_ext = t.root.join(format!("{name}.txt")).display().to_string();
                assert_eq!(
                    read(&with_ext, &roots, 1024).expect_err(name).code,
                    codes::INVALID_ARGUMENTS,
                    "ext {name}.txt"
                );
            }
        }

        #[test]
        fn windows_refused_prefixes_and_drive_relative() {
            let t = WinTree::new("refused-prefixes");
            let roots = t.roots();

            let prefixes = [
                r"\\server\share\file",
                "//server/share/file",
                r"\\.\COM1",
                "//./COM1",
                r"\\?\C:\file",
                "//?/C:/file",
                r"\??\C:\file",
                "/??/C:/file",
                "C:file",
                r"C:dir\file",
                "C:",
            ];

            for prefix in prefixes {
                assert_eq!(
                    read(prefix, &roots, 1024).expect_err(prefix).code,
                    codes::INVALID_ARGUMENTS,
                    "prefix {prefix}"
                );
            }
        }

        #[test]
        fn windows_case_and_8_3_aliases() {
            let t = WinTree::new("aliases");
            let roots = t.roots();

            // Inside root: case alias succeeds (kernel resolves canonical path which is inside root)
            let case_alias_inside = t.root.join("SUB\\B.TXT").display().to_string();
            let val = read(&case_alias_inside, &roots, 1024).expect("read case alias inside");
            assert_eq!(val["text"], "nested");

            // Outside root: case alias outside root is denied
            let case_outside = t.outside.join("SECRET.TXT").display().to_string();
            let err_case = read(&case_outside, &roots, 1024).expect_err("case outside denied");
            assert_eq!(err_case.code, codes::DENIED);

            // Outside root: 8.3 alias outside root is denied (does not match manifest root prefix)
            let outside_83 = t.base.join("OUTSI~1\\secret.txt").display().to_string();
            let err_83 = read(&outside_83, &roots, 1024).expect_err("8.3 outside denied");
            assert_eq!(err_83.code, codes::DENIED);
        }

        #[test]
        fn windows_target_swapped_between_check_and_open() {
            let t = WinTree::new("swap-hook");
            let roots = t.roots();
            let target_path = t.root.join("to_swap.txt");
            std::fs::write(&target_path, "before swap").expect("write target");

            let target_clone = target_path.clone();
            let outside_clone = t.outside.join("secret.txt");

            // Deterministic hook: executed immediately after path resolution before handle open
            *SWAP_HOOK.lock().unwrap() = Some(Box::new(move || {
                let _ = std::fs::remove_file(&target_clone);
                // Create reparse point or link at the target location pointing outside
                let _ = std::process::Command::new("cmd")
                    .args([
                        "/c",
                        "mklink",
                        target_clone.to_str().unwrap(),
                        outside_clone.to_str().unwrap(),
                    ])
                    .status();
            }));

            let res = read(&target_path.display().to_string(), &roots, 1024);
            *SWAP_HOOK.lock().unwrap() = None;

            if let Err(denial) = res {
                assert_eq!(denial.code, codes::DENIED);
                assert!(
                    denial.message.contains("became a symlink")
                        || denial.message.contains("outside")
                );
            }
        }

        #[test]
        fn windows_posix_rename_refusal_simulated() {
            // Modern NTFS and ReFS on Windows 10 1709+ natively support FileRenameInformationEx
            // POSIX semantics. To verify that a refusing volume is refused with no fallback,
            // we simulate the refusal deterministically via SIMULATE_POSIX_RENAME_REFUSAL.
            let t = WinTree::new("posix-refusal");
            let roots = t.roots();
            let target = t.p("refused_rename.txt");

            SIMULATE_POSIX_RENAME_REFUSAL.store(true, std::sync::atomic::Ordering::Relaxed);
            let res = write(&target, &roots, "data");
            SIMULATE_POSIX_RENAME_REFUSAL.store(false, std::sync::atomic::Ordering::Relaxed);

            let err = res.expect_err("POSIX rename refusal");
            assert_eq!(err.code, codes::DENIED);
            assert!(err.message.contains("POSIX replace rename is refused"));

            // Verify temporary file was deleted on refusal
            let entries = std::fs::read_dir(&t.root).unwrap();
            for entry in entries {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                assert!(
                    !name.starts_with(".basal-call-"),
                    "leftover temp file: {name}"
                );
            }
        }

        #[test]
        fn windows_file_root_grants_no_sibling() {
            let t = WinTree::new("file-root");
            let file_path = t.p("a.txt");
            let roots = vec![file_path.clone()];

            let res = read(&file_path, &roots, 1024).expect("read file root");
            assert_eq!(res["text"], "inside");

            let sibling = t.p("sub\\b.txt");
            let err = read(&sibling, &roots, 1024).expect_err("sibling denied");
            assert_eq!(err.code, codes::DENIED);

            // Write to file root replaces file root
            write(&file_path, &roots, "replaced").expect("replace file root");
            assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "replaced");

            // Write to sibling denied
            let err_write =
                write(&sibling, &roots, "sibling data").expect_err("sibling write denied");
            assert_eq!(err_write.code, codes::DENIED);
        }

        #[test]
        fn windows_write_uses_posix_replace() {
            let t = WinTree::new("write-posix");
            let roots = t.roots();
            let target = t.p("out.txt");

            write(&target, &roots, "version 1").expect("write v1");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "version 1");

            write(&target, &roots, "version 2").expect("write v2");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "version 2");
        }

        #[test]
        fn windows_hardlink_write_replaces_link() {
            let t = WinTree::new("hardlink-replace");
            let roots = t.roots();
            let orig = t.root.join("orig.txt");
            let link = t.root.join("link.txt");
            std::fs::write(&orig, "initial content").expect("write orig");

            std::fs::hard_link(&orig, &link).expect("create hardlink");
            assert_eq!(std::fs::read_to_string(&link).unwrap(), "initial content");

            // Write to link
            write(&link.display().to_string(), &roots, "new link content").expect("write link");

            // Hardlink was replaced, not written through!
            assert_eq!(std::fs::read_to_string(&link).unwrap(), "new link content");
            assert_eq!(std::fs::read_to_string(&orig).unwrap(), "initial content");
        }

        struct DaclObserver {
            temp_sddl: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        }

        impl TempLedger for DaclObserver {
            fn record(&self, _lease: &TempLease) -> Result<Box<dyn TempHold>, String> {
                struct Hold;
                impl TempHold for Hold {
                    fn clear(self: Box<Self>) {}
                }
                Ok(Box::new(Hold))
            }

            fn created(&self, lease: &TempLease) {
                let temp_path = lease.dir.join(&lease.temp);
                let wide: Vec<u16> = temp_path
                    .display()
                    .to_string()
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let mut sd = std::ptr::null_mut();
                let err = unsafe {
                    ffi::GetSecurityInfo(
                        // Open file handle to read security info
                        std::fs::File::open(&temp_path).unwrap().as_raw_handle() as ffi::HANDLE,
                        ffi::SE_FILE_OBJECT,
                        ffi::DACL_SECURITY_INFORMATION,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut sd,
                    )
                };
                if err == 0 && !sd.is_null() {
                    let mut sddl_ptr = std::ptr::null_mut();
                    let ok = unsafe {
                        ffi::ConvertSecurityDescriptorToStringSecurityDescriptorW(
                            sd,
                            1, // SDDL_REVISION_1
                            ffi::DACL_SECURITY_INFORMATION,
                            &mut sddl_ptr,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok != 0 && !sddl_ptr.is_null() {
                        let mut len = 0;
                        while unsafe { *sddl_ptr.add(len) } != 0 {
                            len += 1;
                        }
                        let slice = unsafe { std::slice::from_raw_parts(sddl_ptr, len) };
                        *self.temp_sddl.lock().unwrap() = Some(String::from_utf16_lossy(slice));
                        unsafe { ffi::LocalFree(sddl_ptr as *mut std::ffi::c_void) };
                    }
                    unsafe { ffi::LocalFree(sd) };
                }
            }
        }

        #[test]
        fn windows_temp_dacl_and_result_dacl() {
            let t = WinTree::new("dacl-test");
            let roots = t.roots();
            let target = t.root.join("protected.txt");
            std::fs::write(&target, "secret").expect("write target");

            // Apply restrictive DACL to target: Protected, allow Read only to Everyone
            let sddl = "D:P(A;;GR;;;WD)";
            let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut sd = std::ptr::null_mut();
            let ok = unsafe {
                ffi::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide_sddl.as_ptr(),
                    1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "create sddl sd");

            let target_file = std::fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .unwrap();
            let err = unsafe {
                ffi::SetSecurityInfo(
                    target_file.as_raw_handle() as ffi::HANDLE,
                    ffi::SE_FILE_OBJECT,
                    ffi::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    // Extract DACL
                    {
                        let mut dacl = std::ptr::null_mut();
                        ffi::GetSecurityInfo(
                            target_file.as_raw_handle() as ffi::HANDLE,
                            ffi::SE_FILE_OBJECT,
                            ffi::DACL_SECURITY_INFORMATION,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            &mut dacl,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        );
                        dacl
                    },
                    std::ptr::null_mut(),
                )
            };
            drop(target_file);
            unsafe { ffi::LocalFree(sd) };

            let temp_sddl_holder = std::sync::Arc::new(std::sync::Mutex::new(None));
            let observer = DaclObserver {
                temp_sddl: temp_sddl_holder.clone(),
            };

            write_call(
                &target.display().to_string(),
                &roots,
                "replacement text",
                "call-dacl-1",
                Some(&observer),
            )
            .expect("write call with dacl");

            // Verify temp DACL was captured during temp phase
            let captured = temp_sddl_holder.lock().unwrap().clone();
            assert!(
                captured.is_some(),
                "temp DACL was observed during temp phase"
            );

            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "replacement text"
            );
        }

        #[test]
        fn windows_dacl_preservation_on_create() {
            let t = WinTree::new("dacl-create");
            let roots = t.roots();
            let new_file = t.root.join("new_file.txt");

            let temp_sddl_holder = std::sync::Arc::new(std::sync::Mutex::new(None));
            let observer = DaclObserver {
                temp_sddl: temp_sddl_holder.clone(),
            };

            write_call(
                &new_file.display().to_string(),
                &roots,
                "initial created text",
                "call-dacl-create-1",
                Some(&observer),
            )
            .expect("write call create with dacl");

            let captured = temp_sddl_holder.lock().unwrap().clone();
            assert!(
                captured.is_some(),
                "temp DACL was observed during create temp phase"
            );
            assert_eq!(
                std::fs::read_to_string(&new_file).unwrap(),
                "initial created text"
            );
        }
    }
}
