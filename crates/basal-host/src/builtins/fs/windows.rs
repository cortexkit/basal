//! Windows-specific implementation of the `fs` built-in.
//!
//! A path's spelling is checked before anything is opened
//! ([`validate_raw_spelling`]). Every root and every path is then walked one
//! component at a time from `\??\X:\`, each component opened relative to the
//! handle of the one before, so Windows never rewrites a path, and a reparse
//! point (symlink, junction, mount point or any other) is refused wherever
//! it appears. The handle finally acted on is checked once more: its
//! volume-GUID path must lie under the root's, component by component.
//!
//! `fs.write` holds every directory from the volume root down to the
//! target's parent without `FILE_SHARE_DELETE` until its rename returns, so
//! none of them can be moved out of the root while it runs. It writes a
//! temporary file, flushes it, and renames it over the target with POSIX
//! semantics, failing rather than falling back to any other rename. A
//! replaced file keeps its DACL; a new file takes what NTFS inherits.

#![cfg_attr(not(windows), allow(unused))]

use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub use super::{
    DEFAULT_READ_BYTES, MAX_LIST_ENTRIES, MAX_PATH_BYTES, MAX_READ_BYTES, MAX_WRITE_BYTES, Purpose,
    TempHold, TempLease, TempLedger, TempRemoval, inside, is_legacy_temp_name, temp_name,
};
use super::{TEMP_SEQ, check_write_size, io_denial, is_temp_name, outside};
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
#[allow(non_snake_case, non_upper_case_globals, clippy::upper_case_acronyms)]
mod ffi {
    use std::ffi::c_void;

    pub type NTSTATUS = i32;
    pub type HANDLE = *mut c_void;

    pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC0000034_u32 as i32;
    pub const STATUS_OBJECT_PATH_NOT_FOUND: NTSTATUS = 0xC000003A_u32 as i32;
    pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC0000035_u32 as i32;
    pub const STATUS_ACCESS_DENIED: NTSTATUS = 0xC0000022_u32 as i32;
    pub const STATUS_NOT_A_DIRECTORY: NTSTATUS = 0xC0000103_u32 as i32;
    pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC00000BA_u32 as i32;
    pub const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC00000BB_u32 as i32;
    pub const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000000D_u32 as i32;
    pub const STATUS_NAME_TOO_LONG: NTSTATUS = 0xC0000106_u32 as i32;
    pub const STATUS_FILE_CORRUPT_ERROR: NTSTATUS = 0xC0000102_u32 as i32;
    pub const STATUS_NO_MORE_FILES: NTSTATUS = 0x80000006_u32 as i32;
    #[cfg(test)]
    pub const STATUS_IO_DEVICE_ERROR: NTSTATUS = 0xC0000185_u32 as i32;

    pub const FILE_LIST_DIRECTORY: u32 = 0x0001;
    pub const FILE_WRITE_DATA: u32 = 0x0002;
    pub const FILE_TRAVERSE: u32 = 0x0020;
    pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    pub const DELETE: u32 = 0x00010000;
    pub const READ_CONTROL: u32 = 0x00020000;
    pub const SYNCHRONIZE: u32 = 0x00100000;
    pub const FILE_GENERIC_READ: u32 = 0x00120089;
    pub const FILE_GENERIC_WRITE: u32 = 0x00120116;

    pub const FILE_SHARE_READ: u32 = 0x00000001;
    pub const FILE_SHARE_WRITE: u32 = 0x00000002;
    pub const FILE_SHARE_DELETE: u32 = 0x00000004;

    pub const FILE_OPEN: u32 = 0x00000001;
    pub const FILE_CREATE: u32 = 0x00000002;

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
    pub const FILE_RENAME_IGNORE_READONLY_ATTRIBUTE: u32 = 0x00000040;

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

    /// One entry of an `NtQueryDirectoryFile` reply. Entries are never read
    /// through this type: the reply is parsed as bytes at the offsets
    /// `offset_of!` gives for these fields, so a misaligned or truncated
    /// reply cannot make a reference to it.
    #[allow(dead_code)]
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

    /// The header of a rename request. It is written field by field through
    /// a raw pointer into an 8-byte-aligned buffer that also holds the name,
    /// never built as a value.
    #[allow(dead_code)]
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
    }
}

/// Every sharing mode: what reads, stats and listings open with.
#[cfg(windows)]
const SHARE_ALL: u32 = ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE | ffi::FILE_SHARE_DELETE;

/// The sharing mode of the directories a write holds while it runs. Without
/// `FILE_SHARE_DELETE` nobody else can open them for `DELETE`, which a
/// rename or a delete of the directory needs, so none of them can be moved
/// out of the root before the write's rename returns.
#[cfg(windows)]
const SHARE_NO_DELETE: u32 = ffi::FILE_SHARE_READ | ffi::FILE_SHARE_WRITE;

/// The access a directory on a walked path is opened with. `FILE_TRAVERSE`
/// is there for sharing, not for the walk itself: the kernel records a
/// handle's sharing mode only when the handle has read, write, execute
/// (`FILE_TRAVERSE` is the execute bit) or delete access. A held directory
/// opened with attribute access alone would not stop anyone renaming it.
#[cfg(windows)]
const DIR_WALK: u32 = ffi::FILE_READ_ATTRIBUTES | ffi::FILE_TRAVERSE | ffi::SYNCHRONIZE;

/// A handle this module opened and closes. The field is private and the
/// only constructor is [`nt_open_relative`], so no code can hand an
/// arbitrary pointer to `NtClose`.
#[cfg(windows)]
struct OwnedHandle(ffi::HANDLE);

#[cfg(windows)]
impl OwnedHandle {
    fn raw(&self) -> ffi::HANDLE {
        self.0
    }

    fn into_file(self) -> std::fs::File {
        use std::os::windows::io::FromRawHandle;
        let raw = self.0;
        std::mem::forget(self);
        // SAFETY: `raw` is an open handle this value owned; forgetting the
        // value hands that ownership to the File exactly once.
        unsafe { std::fs::File::from_raw_handle(raw) }
    }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful NtCreateFile and is
        // closed exactly once, here.
        unsafe { ffi::NtClose(self.0) };
    }
}

/// A security descriptor allocated by `GetSecurityInfo`, freed on drop.
#[cfg(windows)]
struct FileDacl {
    sd: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl Drop for FileDacl {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            // SAFETY: `sd` came from GetSecurityInfo, whose descriptors are
            // documented to be released with LocalFree, and is freed once.
            unsafe { ffi::LocalFree(self.sd) };
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenError {
    NotFound,
    ReparsePoint,
    AccessDenied,
    NotADirectory,
    IsADirectory,
    Collision,
    Other(ffi::NTSTATUS),
}

#[cfg(windows)]
impl OpenError {
    fn status(self) -> ffi::NTSTATUS {
        match self {
            Self::NotFound => ffi::STATUS_OBJECT_NAME_NOT_FOUND,
            // Not an NT status of its own: the open succeeded and the entry
            // was refused for what it is.
            Self::ReparsePoint => ffi::STATUS_ACCESS_DENIED,
            Self::AccessDenied => ffi::STATUS_ACCESS_DENIED,
            Self::NotADirectory => ffi::STATUS_NOT_A_DIRECTORY,
            Self::IsADirectory => ffi::STATUS_FILE_IS_A_DIRECTORY,
            Self::Collision => ffi::STATUS_OBJECT_NAME_COLLISION,
            Self::Other(status) => status,
        }
    }
}

/// How [`nt_open_relative`] opens a name.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct Open {
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    security_descriptor: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl Open {
    /// Opens an existing entry of any type, sharing everything.
    fn existing(access: u32) -> Self {
        Self {
            access,
            share: SHARE_ALL,
            disposition: ffi::FILE_OPEN,
            options: 0,
            security_descriptor: std::ptr::null_mut(),
        }
    }

    fn directory(mut self) -> Self {
        self.options |= ffi::FILE_DIRECTORY_FILE;
        self
    }

    fn non_directory(mut self) -> Self {
        self.options |= ffi::FILE_NON_DIRECTORY_FILE;
        self
    }

    fn share(mut self, share: u32) -> Self {
        self.share = share;
        self
    }
}

/// Opens `name`, one component, relative to the directory `root` (or, with
/// a null `root`, the NT path `name`). An empty `name` opens `root` itself
/// again, with the access `open` asks for.
///
/// No reparse point is ever followed (`FILE_OPEN_REPARSE_POINT`), and one
/// that was opened is refused: the handle is asked for its attributes, and
/// an entry that is a reparse point, or whose attributes cannot be read, is
/// closed and refused. `FILE_READ_ATTRIBUTES` is added to every request so
/// that check always has the access it needs.
#[cfg(windows)]
fn nt_open_relative(root: ffi::HANDLE, name: &str, open: Open) -> Result<OwnedHandle, OpenError> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    // A UNICODE_STRING counts bytes in a u16; a longer name would be cut to
    // a prefix, which can name a different entry.
    let length =
        u16::try_from(wide.len() * 2).map_err(|_| OpenError::Other(ffi::STATUS_NAME_TOO_LONG))?;
    let maximum = length
        .checked_add(2)
        .ok_or(OpenError::Other(ffi::STATUS_NAME_TOO_LONG))?;
    wide.push(0);
    let mut unicode_name = ffi::UNICODE_STRING {
        Length: length,
        MaximumLength: maximum,
        Buffer: wide.as_mut_ptr(),
    };
    let mut obj_attr = ffi::OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<ffi::OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &mut unicode_name,
        Attributes: ffi::OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: open.security_descriptor,
        SecurityQualityOfService: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    // SAFETY: every pointer refers to a live local that outlives the call:
    // the name buffer `wide`, the `unicode_name` describing it, the object
    // attributes, the status block, and `handle`, which receives the new
    // handle on success.
    let status = unsafe {
        ffi::NtCreateFile(
            &mut handle,
            open.access | ffi::FILE_READ_ATTRIBUTES,
            &mut obj_attr,
            &mut io_status,
            std::ptr::null_mut(),
            ffi::FILE_ATTRIBUTE_NORMAL,
            open.share,
            open.disposition,
            open.options | ffi::FILE_SYNCHRONOUS_IO_NONALERT | ffi::FILE_OPEN_REPARSE_POINT,
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
            ffi::STATUS_NOT_A_DIRECTORY => OpenError::NotADirectory,
            ffi::STATUS_FILE_IS_A_DIRECTORY => OpenError::IsADirectory,
            ffi::STATUS_OBJECT_NAME_COLLISION => OpenError::Collision,
            _ => OpenError::Other(status),
        });
    }
    let handle = OwnedHandle(handle);
    // Fail closed: an entry whose attributes cannot be read is not known
    // not to be a reparse point.
    let basic = query_file_basic(handle.raw()).map_err(OpenError::Other)?;
    if basic.FileAttributes & ffi::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(OpenError::ReparsePoint);
    }
    Ok(handle)
}

/// Opens the file or directory behind `handle` again, with the access
/// `open` asks for. An NT open of an empty name relative to a handle opens
/// that handle's own file object (this is how `ReOpenFile` works), so the
/// new handle is for the object already checked, not for whatever a path
/// names now.
#[cfg(windows)]
fn nt_reopen(handle: &OwnedHandle, open: Open) -> Result<OwnedHandle, OpenError> {
    nt_open_relative(handle.raw(), "", open)
}

/// Opens each of `components` beneath `start` as a directory, in order, and
/// returns every handle (the last is the deepest).
#[cfg(windows)]
fn walk_dirs(
    start: &OwnedHandle,
    components: &[&str],
    open: Open,
) -> Result<Vec<OwnedHandle>, OpenError> {
    let mut held: Vec<OwnedHandle> = Vec::with_capacity(components.len());
    for comp in components {
        let current = held.last().unwrap_or(start);
        let next = nt_open_relative(current.raw(), comp, open.directory())?;
        held.push(next);
    }
    Ok(held)
}

#[cfg(windows)]
fn query_file_basic(handle: ffi::HANDLE) -> Result<ffi::FILE_BASIC_INFORMATION, ffi::NTSTATUS> {
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut info = ffi::FILE_BASIC_INFORMATION::default();
    // SAFETY: the output buffer is `info`, and the length passed is its size.
    let status = unsafe {
        ffi::NtQueryInformationFile(
            handle,
            &mut io_status,
            (&mut info as *mut ffi::FILE_BASIC_INFORMATION).cast(),
            std::mem::size_of::<ffi::FILE_BASIC_INFORMATION>() as u32,
            ffi::FileBasicInformation,
        )
    };
    if status < 0 {
        return Err(status);
    }
    Ok(info)
}

#[cfg(windows)]
fn query_file_standard(
    handle: ffi::HANDLE,
) -> Result<ffi::FILE_STANDARD_INFORMATION, ffi::NTSTATUS> {
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut info = ffi::FILE_STANDARD_INFORMATION::default();
    // SAFETY: the output buffer is `info`, and the length passed is its size.
    let status = unsafe {
        ffi::NtQueryInformationFile(
            handle,
            &mut io_status,
            (&mut info as *mut ffi::FILE_STANDARD_INFORMATION).cast(),
            std::mem::size_of::<ffi::FILE_STANDARD_INFORMATION>() as u32,
            ffi::FileStandardInformation,
        )
    };
    if status < 0 {
        return Err(status);
    }
    Ok(info)
}

/// A denial with code `IO` for an NT call on `path` that failed with
/// `status`, naming both.
#[cfg(windows)]
fn nt_io(path: &Path, what: &str, status: ffi::NTSTATUS) -> Denial {
    Denial::new(
        codes::IO,
        format!(
            "{}: {what} failed (NT status 0x{:08x})",
            path.display(),
            status as u32
        ),
    )
}

#[cfg(windows)]
fn get_volume_guid_path(handle: ffi::HANDLE) -> Result<String, Denial> {
    let mut buf = vec![0u16; 1024];
    // SAFETY: `buf` is writable for the `buf.len()` UTF-16 units passed as
    // the capacity.
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
        // SAFETY: `buf` was grown to the length the first call asked for and
        // is writable for the `buf.len()` units passed as the capacity.
        let len2 = unsafe {
            ffi::GetFinalPathNameByHandleW(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                ffi::VOLUME_NAME_GUID | ffi::FILE_NAME_NORMALIZED,
            )
        };
        if len2 == 0 || len2 as usize > buf.len() {
            return Err(Denial::denied("failed to get volume GUID path for handle"));
        }
        buf.truncate(len2 as usize);
    } else {
        buf.truncate(len as usize);
    }
    String::from_utf16(&buf).map_err(|_| Denial::denied("volume GUID path is not UTF-16"))
}

/// Whether `parent_guid` is the directory that holds the file root whose
/// volume-GUID path is `file_root_guid`, compared component by component.
#[cfg(windows)]
fn guid_is_parent_of(parent_guid: &str, file_root_guid: &str) -> bool {
    let parent: Vec<&str> = parent_guid.split('\\').filter(|s| !s.is_empty()).collect();
    let root: Vec<&str> = file_root_guid
        .split('\\')
        .filter(|s| !s.is_empty())
        .collect();
    root.split_last()
        .is_some_and(|(_, head)| head == parent.as_slice())
}

/// A manifest root after [`walk_from_volume_root`] opened it from its
/// volume's root directory, refusing reparse points, with its volume-GUID
/// path for checking where later handles really are.
#[cfg(windows)]
struct VerifiedRoot {
    manifest: String,
    guid_path: String,
    is_directory: bool,
    /// The root itself: [`walk_from_volume_root`] opens a directory with
    /// [`DIR_WALK`] and a file with attribute access only. Operations that need more open it again with
    /// [`nt_reopen`].
    handle: OwnedHandle,
    /// The directories above the root, from the volume root down; the last
    /// is the root's parent. Empty for a volume root.
    ancestors: Vec<OwnedHandle>,
}

/// A directory and every ancestor held against rename or replacement while a
/// reader such as git opens the directory by path. Omitting delete sharing
/// prevents another process from moving any of those directories.
#[cfg(windows)]
pub(crate) struct PinnedDirectory(VerifiedRoot);

#[cfg(windows)]
impl PinnedDirectory {
    pub(crate) fn path(&self) -> &Path {
        Path::new(&self.0.manifest)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        guid_path_inside_component_wise(&self.0.guid_path, &other.0.guid_path, false)
    }
}

#[cfg(windows)]
impl std::ops::Deref for PinnedDirectory {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

/// Uses the filesystem builtin's walk, which refuses reparse points, retaining
/// every directory handle until the returned guard is dropped. The volume-GUID identity is
/// obtained from the opened directory, never from canonicalized input text.
#[cfg(windows)]
pub(crate) fn pin_directory(path: &str) -> Result<PinnedDirectory, Denial> {
    let root = walk_from_volume_root(path, SHARE_NO_DELETE)?;
    if !root.is_directory {
        return Err(Denial::denied("the pinned path is not a directory"));
    }
    Ok(PinnedDirectory(root))
}

/// The denial for a failed open of `comp` while walking to `manifest_root`.
#[cfg(windows)]
fn root_walk_denial(e: OpenError, comp: &str, manifest_root: &str) -> Denial {
    match e {
        OpenError::ReparsePoint => Denial::denied(format!(
            "{comp} in root {manifest_root} is a reparse point; fs refuses every reparse point"
        )),
        OpenError::NotFound => Denial::new(
            codes::NOT_FOUND,
            format!("root {manifest_root} does not exist"),
        ),
        OpenError::AccessDenied => Denial::denied(format!(
            "access to {comp} in root {manifest_root} is denied"
        )),
        OpenError::NotADirectory => {
            Denial::denied(format!("{comp} in root {manifest_root} is not a directory"))
        }
        other => Denial::new(
            codes::IO,
            format!(
                "opening {comp} in root {manifest_root} failed (NT status 0x{:08x})",
                other.status() as u32
            ),
        ),
    }
}

/// Walks `manifest_root` from its volume's root directory, one component at
/// a time, refusing a reparse point anywhere. Every handle is opened with
/// `share`.
#[cfg(windows)]
fn walk_from_volume_root(manifest_root: &str, share: u32) -> Result<VerifiedRoot, Denial> {
    validate_raw_spelling(manifest_root)?;
    let drive = &manifest_root[..3]; // e.g. "C:\"
    let volume = nt_open_relative(
        std::ptr::null_mut(),
        &format!(r"\??\{drive}"),
        Open::existing(DIR_WALK).directory().share(share),
    )
    .map_err(|e| root_walk_denial(e, drive, manifest_root))?;

    let rest = &manifest_root[3..];
    if rest.is_empty() {
        let guid_path = get_volume_guid_path(volume.raw())?;
        return Ok(VerifiedRoot {
            manifest: manifest_root.to_owned(),
            guid_path,
            is_directory: true,
            handle: volume,
            ancestors: Vec::new(),
        });
    }

    let components: Vec<&str> = rest.split('\\').collect();
    let Some((last, middle)) = components.split_last() else {
        return Err(outside(Path::new(manifest_root)));
    };
    let mut ancestors = vec![volume];
    for comp in middle {
        let current = ancestors.last().expect("the volume root is held");
        let next = nt_open_relative(
            current.raw(),
            comp,
            Open::existing(DIR_WALK).directory().share(share),
        )
        .map_err(|e| root_walk_denial(e, comp, manifest_root))?;
        ancestors.push(next);
    }
    let parent = ancestors.last().expect("the volume root is held");
    // The root's type is not known before it is open, so it is opened first
    // with attribute access, which any type grants. A directory is then
    // opened again with FILE_TRAVERSE, which a held directory needs to take
    // part in sharing checks; a file is not, because on a file that bit is
    // the execute right and may not be granted. A file root keeps the
    // first handle, which shares everything: it is the very file a write
    // to the root replaces, so it must never stand in the way of that.
    let first = nt_open_relative(parent.raw(), last, Open::existing(ffi::SYNCHRONIZE))
        .map_err(|e| root_walk_denial(e, last, manifest_root))?;
    let std_info =
        query_file_standard(first.raw()).map_err(|s| nt_io(Path::new(manifest_root), "stat", s))?;
    let is_directory = std_info.Directory != 0;
    let handle = if is_directory {
        nt_reopen(&first, Open::existing(DIR_WALK).directory().share(share))
            .map_err(|e| root_walk_denial(e, last, manifest_root))?
    } else {
        first
    };
    let guid_path = get_volume_guid_path(handle.raw())?;
    Ok(VerifiedRoot {
        manifest: manifest_root.to_owned(),
        guid_path,
        is_directory,
        handle,
        ancestors,
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

/// Whether `path` is `root` or lies beneath it, by spelling. A volume root
/// (`X:\`, the only root spelling that ends in a separator) covers every
/// path on its drive.
fn path_under(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| root.ends_with('\\') || rest.starts_with('\\'))
}

/// The components of `path` below `root`, which it must lie under
/// ([`path_under`]); empty when it is the root.
fn components_below<'a>(path: &'a str, root: &str) -> Vec<&'a str> {
    let rest = &path[root.len()..];
    let rest = rest.strip_prefix('\\').unwrap_or(rest);
    if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('\\').collect()
    }
}

/// How bad a root's failure is, for choosing which one to report: a
/// failure that may pass beats a missing root, which beats a refusal.
#[cfg(windows)]
fn failure_rank(d: &Denial) -> u8 {
    if d.code == codes::IO {
        2
    } else if d.code == codes::NOT_FOUND {
        1
    } else {
        0
    }
}

/// Resolve the profile of the process token, not a caller-controlled
/// environment variable. A missing or unreadable profile grants no root.
#[cfg(windows)]
fn process_profile_directory() -> Result<String, Denial> {
    basal_launch::process_profile_directory().map_err(|error| Denial::new(codes::IO, error))
}

#[cfg(windows)]
fn expand_root(root: &str) -> Result<std::borrow::Cow<'_, str>, Denial> {
    let suffix = if root == "~" {
        ""
    } else if let Some(suffix) = root.strip_prefix("~/") {
        suffix
    } else {
        return Ok(std::borrow::Cow::Borrowed(root));
    };
    let mut profile = process_profile_directory()?;
    validate_raw_spelling(&profile)?;
    if !suffix.is_empty() {
        if !profile.ends_with('\\') {
            profile.push('\\');
        }
        // Preserve components, including refused spellings, until validation.
        profile.push_str(&suffix.replace('/', "\\"));
    }
    validate_raw_spelling(&profile)?;
    Ok(std::borrow::Cow::Owned(profile))
}

/// Finds a manifest root that `path` lies under and walks it
/// ([`walk_from_volume_root`]). A file root grants only its own path. When
/// every root `path` lies under fails to walk, the most telling failure is
/// returned: an `IO` error (which may succeed if tried again), then
/// `NOT_FOUND` (the root is gone), then a refusal. `remove_temp` relies on
/// that order to keep a record after a transient failure and to drop one
/// whose directory no longer exists.
#[cfg(windows)]
fn select_root(path: &str, roots: &[String], share: u32) -> Result<VerifiedRoot, Denial> {
    validate_raw_spelling(path)?;
    let mut failure: Option<Denial> = None;
    for r in roots {
        let r = expand_root(r)?;
        if validate_raw_spelling(&r).is_err() || !path_under(path, &r) {
            continue;
        }
        match walk_from_volume_root(&r, share) {
            Ok(vr) if vr.is_directory || path == vr.manifest => return Ok(vr),
            Ok(_) => {}
            Err(d) => {
                if failure
                    .as_ref()
                    .is_none_or(|f| failure_rank(&d) > failure_rank(f))
                {
                    failure = Some(d);
                }
            }
        }
    }
    Err(failure.unwrap_or_else(|| outside(Path::new(path))))
}

/// The denial for a failed open of a directory between a root and the
/// entry `path` names: a reparse point is refused as one, an unexpected NT
/// failure is `IO`, and a missing, unreadable or non-directory component
/// is "outside the manifest's roots or missing", the answer the Unix
/// implementation gives when resolving such a path fails.
#[cfg(windows)]
fn intermediate_denial(e: OpenError, path: &Path) -> Denial {
    match e {
        OpenError::ReparsePoint => Denial::denied(format!(
            "{} contains a reparse point; fs refuses every reparse point",
            path.display()
        )),
        OpenError::Other(status) => nt_io(path, "opening a directory", status),
        _ => outside(path),
    }
}

#[cfg(windows)]
pub fn resolve(path: &str, roots: &[String], purpose: Purpose) -> Result<Target, Denial> {
    validate_raw_spelling(path)?;
    let vr = select_root(path, roots, SHARE_ALL)?;
    let p = Path::new(path);

    if purpose == Purpose::Read {
        if path == vr.manifest {
            return Ok(Target::Existing(PathBuf::from(path)));
        }
        let components = components_below(path, &vr.manifest);
        let Some((last, middle)) = components.split_last() else {
            return Err(outside(p));
        };
        let dirs = walk_dirs(&vr.handle, middle, Open::existing(ffi::SYNCHRONIZE))
            .map_err(|e| intermediate_denial(e, p))?;
        let parent = dirs.last().unwrap_or(&vr.handle);
        match nt_open_relative(parent.raw(), last, Open::existing(ffi::SYNCHRONIZE)) {
            Ok(leaf) => {
                let guid = get_volume_guid_path(leaf.raw())?;
                return if guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
                    Ok(Target::Existing(PathBuf::from(path)))
                } else {
                    Err(outside(p))
                };
            }
            // A missing entry: answered below as the name in its parent
            // directory, which must lie under the root.
            Err(OpenError::NotFound) => {}
            Err(OpenError::ReparsePoint) => {
                return Err(Denial::denied(format!(
                    "{path} contains a reparse point; fs refuses every reparse point"
                )));
            }
            Err(_) => return Err(outside(p)),
        }
    }

    let parent = p.parent().ok_or_else(|| outside(p))?;
    let name = p.file_name().ok_or_else(|| outside(p))?;

    if vr.is_directory {
        // The directory root itself is not an entry a write can replace.
        let parent_str = parent.to_str().ok_or_else(|| outside(p))?;
        if path == vr.manifest || !path_under(parent_str, &vr.manifest) {
            return Err(outside(p));
        }
    } else if path != vr.manifest {
        return Err(outside(p));
    }

    Ok(Target::Entry {
        parent: parent.to_path_buf(),
        name: name.to_os_string(),
    })
}

/// The denial for a failed open of the entry `path` itself.
#[cfg(windows)]
fn leaf_denial(e: OpenError, path: &Path) -> Denial {
    match e {
        OpenError::ReparsePoint => Denial::denied(format!(
            "{} became a symlink while it was being opened",
            path.display()
        )),
        OpenError::NotFound => io_denial(path, &std::io::Error::from(std::io::ErrorKind::NotFound)),
        OpenError::NotADirectory => {
            Denial::new(codes::IO, format!("{} is not a directory", path.display()))
        }
        OpenError::AccessDenied => {
            Denial::denied(format!("access to {} is denied", path.display()))
        }
        other => nt_io(path, "opening", other.status()),
    }
}

#[cfg(windows)]
pub fn open_checked(
    resolved: &Path,
    roots: &[String],
    directory: bool,
) -> Result<std::fs::File, Denial> {
    let path_str = resolved.to_str().ok_or_else(|| outside(resolved))?;
    validate_raw_spelling(path_str)?;
    let vr = select_root(path_str, roots, SHARE_ALL)?;
    let leaf_open = if directory {
        Open::existing(ffi::FILE_GENERIC_READ | ffi::FILE_TRAVERSE).directory()
    } else {
        Open::existing(ffi::FILE_GENERIC_READ)
    };

    let opened = if path_str == vr.manifest {
        if directory && !vr.is_directory {
            return Err(outside(resolved));
        }
        // The walk opened the root with attribute access only; reading or
        // listing it needs a handle with the access for that.
        nt_reopen(&vr.handle, leaf_open).map_err(|e| leaf_denial(e, resolved))?
    } else {
        let components = components_below(path_str, &vr.manifest);
        let Some((last, middle)) = components.split_last() else {
            return Err(outside(resolved));
        };
        let dirs = walk_dirs(&vr.handle, middle, Open::existing(DIR_WALK))
            .map_err(|e| intermediate_denial(e, resolved))?;
        let parent = dirs.last().unwrap_or(&vr.handle);
        nt_open_relative(parent.raw(), last, leaf_open).map_err(|e| leaf_denial(e, resolved))?
    };

    let guid = get_volume_guid_path(opened.raw())?;
    if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
        return Err(outside(resolved));
    }
    Ok(opened.into_file())
}

/// Test-only hooks. They are per thread, so a test that sets one affects
/// only the calls it makes itself, never a test running beside it.
#[cfg(all(test, windows))]
mod hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// Runs in `read` between resolving the path and opening it.
        pub(super) static BEFORE_READ_OPEN: RefCell<Option<Box<dyn Fn()>>> =
            const { RefCell::new(None) };
        /// Makes `write_call` treat the rename as refused by the volume.
        pub(super) static REFUSE_POSIX_RENAME: Cell<bool> = const { Cell::new(false) };
        /// Makes `write_call` treat flushing its temporary file as failed.
        pub(super) static FAIL_TEMP_FLUSH: Cell<bool> = const { Cell::new(false) };
    }
}

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
    hooks::BEFORE_READ_OPEN.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });

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
    // The file can grow between the size check and the read.
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

/// `fs.stat`'s answer for the open entry behind `handle`.
#[cfg(windows)]
fn stat_value(handle: &OwnedHandle, path: &Path) -> Result<Value, Denial> {
    let basic = query_file_basic(handle.raw()).map_err(|s| nt_io(path, "stat", s))?;
    let std_info = query_file_standard(handle.raw()).map_err(|s| nt_io(path, "stat", s))?;
    Ok(json!({
        "exists": true,
        "kind": if std_info.Directory != 0 { "dir" } else { "file" },
        "size": std_info.EndOfFile,
        "mtime_ms": filetime_to_mtime_ms(basic.LastWriteTime),
    }))
}

#[cfg(windows)]
pub fn stat(path: &str, roots: &[String]) -> Result<Value, Denial> {
    validate_raw_spelling(path)?;
    let vr = select_root(path, roots, SHARE_ALL)?;
    let p = Path::new(path);

    if path == vr.manifest {
        return stat_value(&vr.handle, p);
    }

    let components = components_below(path, &vr.manifest);
    let Some((last, middle)) = components.split_last() else {
        return Err(outside(p));
    };
    let dirs = walk_dirs(&vr.handle, middle, Open::existing(ffi::SYNCHRONIZE))
        .map_err(|e| intermediate_denial(e, p))?;
    let parent = dirs.last().unwrap_or(&vr.handle);
    match nt_open_relative(parent.raw(), last, Open::existing(ffi::SYNCHRONIZE)) {
        Ok(leaf) => {
            let guid = get_volume_guid_path(leaf.raw())?;
            if !guid_path_inside_component_wise(&guid, &vr.guid_path, vr.is_directory) {
                return Err(outside(p));
            }
            stat_value(&leaf, p)
        }
        Err(OpenError::NotFound) => Ok(json!({ "exists": false })),
        Err(OpenError::ReparsePoint) => Err(Denial::denied(format!(
            "{path} is a reparse point; fs refuses every reparse point"
        ))),
        Err(OpenError::AccessDenied) => Err(Denial::denied(format!("access to {path} is denied"))),
        Err(e) => Err(nt_io(p, "opening", e.status())),
    }
}

/// Byte offsets of the `FILE_DIRECTORY_INFORMATION` fields a scan reads.
#[cfg(windows)]
const DIR_NEXT_OFFSET: usize =
    std::mem::offset_of!(ffi::FILE_DIRECTORY_INFORMATION, NextEntryOffset);
#[cfg(windows)]
const DIR_ATTRIBUTES: usize = std::mem::offset_of!(ffi::FILE_DIRECTORY_INFORMATION, FileAttributes);
#[cfg(windows)]
const DIR_NAME_LENGTH: usize =
    std::mem::offset_of!(ffi::FILE_DIRECTORY_INFORMATION, FileNameLength);
#[cfg(windows)]
const DIR_NAME: usize = std::mem::offset_of!(ffi::FILE_DIRECTORY_INFORMATION, FileName);

#[cfg(windows)]
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Hands each entry of the directory behind `handle` (not `.` or `..`) to
/// `visit` as its UTF-16 name and attributes, until `visit` answers false.
/// The handle needs `FILE_LIST_DIRECTORY`. Any failed query is an error,
/// the first one included: an unreadable directory is never reported as
/// an empty one. The reply is parsed as bytes and every entry is checked
/// against the length the kernel reported, so a malformed reply is an
/// error rather than a read past it.
#[cfg(windows)]
fn scan_directory(
    handle: ffi::HANDLE,
    mut visit: impl FnMut(&[u16], u32) -> bool,
) -> Result<(), ffi::NTSTATUS> {
    // u64 storage: the kernel requires an 8-byte-aligned buffer.
    let mut buffer = vec![0u64; 8 * 1024];
    let buffer_bytes = buffer.len() * std::mem::size_of::<u64>();
    let mut restart = 1u8;
    loop {
        let mut io_status = ffi::IO_STATUS_BLOCK {
            Status: 0,
            Information: 0,
        };
        // SAFETY: the buffer is `buffer_bytes` long and outlives the call.
        let status = unsafe {
            ffi::NtQueryDirectoryFile(
                handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut io_status,
                buffer.as_mut_ptr().cast(),
                buffer_bytes as u32,
                ffi::FileDirectoryInformation,
                0,
                std::ptr::null_mut(),
                restart,
            )
        };
        restart = 0;
        if status == ffi::STATUS_NO_MORE_FILES {
            return Ok(());
        }
        if status < 0 {
            return Err(status);
        }
        let filled = io_status.Information;
        if filled == 0 {
            return Ok(());
        }
        if filled > buffer_bytes {
            return Err(ffi::STATUS_FILE_CORRUPT_ERROR);
        }
        // SAFETY: the first `filled` bytes of the buffer are initialised
        // (it was zeroed) and lie within it.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), filled) };
        let mut offset = 0usize;
        loop {
            let entry = &bytes[offset..];
            if entry.len() < DIR_NAME {
                return Err(ffi::STATUS_FILE_CORRUPT_ERROR);
            }
            let next = u32_at(entry, DIR_NEXT_OFFSET) as usize;
            let attributes = u32_at(entry, DIR_ATTRIBUTES);
            let name_bytes = u32_at(entry, DIR_NAME_LENGTH) as usize;
            let Some(raw_name) = entry.get(DIR_NAME..DIR_NAME + name_bytes) else {
                return Err(ffi::STATUS_FILE_CORRUPT_ERROR);
            };
            if !name_bytes.is_multiple_of(2) {
                return Err(ffi::STATUS_FILE_CORRUPT_ERROR);
            }
            let name: Vec<u16> = raw_name
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes(*unit))
                .collect();
            let dot = u16::from(b'.');
            let is_dot = name == [dot] || name == [dot, dot];
            if !is_dot && !visit(&name, attributes) {
                return Ok(());
            }
            if next == 0 {
                break;
            }
            offset = match offset.checked_add(next) {
                Some(o) if o < filled => o,
                _ => return Err(ffi::STATUS_FILE_CORRUPT_ERROR),
            };
        }
    }
}

#[cfg(windows)]
pub fn list(path: &str, roots: &[String]) -> Result<Value, Denial> {
    use std::os::windows::io::AsRawHandle;
    let real = match resolve(path, roots, Purpose::Read)? {
        Target::Existing(real) => real,
        Target::Entry { parent, name } => {
            return Err(Denial::new(
                codes::NOT_FOUND,
                format!("{} does not exist", parent.join(name).display()),
            ));
        }
    };
    let dir = open_checked(&real, roots, true)?;

    let mut names: Vec<(Vec<u16>, u32)> = Vec::new();
    scan_directory(dir.as_raw_handle(), |name, attributes| {
        names.push((name.to_vec(), attributes));
        names.len() <= MAX_LIST_ENTRIES
    })
    .map_err(|s| nt_io(&real, "listing", s))?;
    if names.len() > MAX_LIST_ENTRIES {
        return Err(Denial::new(
            codes::TOO_LARGE,
            format!(
                "{} has more than {MAX_LIST_ENTRIES} entries",
                real.display()
            ),
        ));
    }

    let mut entries = Vec::with_capacity(names.len());
    for (name, attributes) in names {
        let kind = if attributes & ffi::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            "symlink"
        } else if attributes & ffi::FILE_ATTRIBUTE_DIRECTORY != 0 {
            "dir"
        } else {
            "file"
        };
        let Ok(text) = String::from_utf16(&name) else {
            return Err(Denial::new(
                codes::NOT_UTF8,
                format!("{} holds a name that is not UTF-8", real.display()),
            ));
        };
        entries.push((text, kind));
    }
    entries.sort();
    Ok(Value::Array(
        entries
            .into_iter()
            .map(|(name, kind)| json!({ "name": name, "kind": kind }))
            .collect(),
    ))
}

/// The DACL of the file a write is about to replace, or `None` when there
/// is no such file and the new one takes what NTFS inherits for it.
/// Refuses a reparse point and anything that is not a regular file, as the
/// Unix write refuses a symlink and a non-regular file.
#[cfg(windows)]
fn existing_target_dacl(
    parent: &OwnedHandle,
    leaf: &str,
    target: &Path,
) -> Result<Option<FileDacl>, Denial> {
    let file = match nt_open_relative(
        parent.raw(),
        leaf,
        Open::existing(ffi::READ_CONTROL | ffi::SYNCHRONIZE).non_directory(),
    ) {
        Ok(file) => file,
        Err(OpenError::NotFound) => return Ok(None),
        Err(OpenError::ReparsePoint) => {
            return Err(Denial::denied(format!(
                "{} is a symlink; fs.write does not replace symlinks",
                target.display()
            )));
        }
        Err(OpenError::IsADirectory) => {
            return Err(Denial::invalid(format!(
                "{} is not a regular file",
                target.display()
            )));
        }
        Err(OpenError::AccessDenied) => {
            return Err(Denial::denied(format!(
                "the permissions of {} cannot be read",
                target.display()
            )));
        }
        Err(e) => return Err(nt_io(target, "opening", e.status())),
    };
    let mut sd = std::ptr::null_mut();
    // SAFETY: `file` is open with READ_CONTROL; on success `sd` receives a
    // LocalAlloc'd descriptor that FileDacl frees.
    let err = unsafe {
        ffi::GetSecurityInfo(
            file.raw(),
            ffi::SE_FILE_OBJECT,
            ffi::DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(Denial::new(
            codes::IO,
            format!(
                "{}: reading its permissions failed (error {err})",
                target.display()
            ),
        ));
    }
    Ok(Some(FileDacl { sd }))
}

/// Marks the open file behind `handle` for deletion; it goes when the last
/// handle to it closes.
#[cfg(windows)]
fn delete_by_handle(handle: &OwnedHandle) -> Result<(), ffi::NTSTATUS> {
    let mut disp = ffi::FILE_DISPOSITION_INFORMATION { DeleteFile: 1 };
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    // SAFETY: the buffer passed is `disp`, and the length passed is its
    // size.
    let status = unsafe {
        ffi::NtSetInformationFile(
            handle.raw(),
            &mut io_status,
            (&mut disp as *mut ffi::FILE_DISPOSITION_INFORMATION).cast(),
            std::mem::size_of::<ffi::FILE_DISPOSITION_INFORMATION>() as u32,
            ffi::FileDispositionInformation,
        )
    };
    if status < 0 { Err(status) } else { Ok(()) }
}

/// What [`unlink_file`] found under a name.
#[cfg(windows)]
enum Unlinked {
    Removed,
    Absent,
    /// Something other than a regular file, left in place: what it is.
    NotRegular(&'static str),
}

/// Removes `name` from the directory behind `parent` only if it is a
/// regular file, as the Unix `unlink_regular` does. A directory or a
/// reparse point under the name is left alone, and a link is never
/// followed, so nothing but that one entry can be removed.
#[cfg(windows)]
fn unlink_file(parent: ffi::HANDLE, name: &str) -> Result<Unlinked, ffi::NTSTATUS> {
    let file = match nt_open_relative(
        parent,
        name,
        Open::existing(ffi::DELETE | ffi::SYNCHRONIZE).non_directory(),
    ) {
        Ok(file) => file,
        Err(OpenError::NotFound) => return Ok(Unlinked::Absent),
        Err(OpenError::ReparsePoint) => return Ok(Unlinked::NotRegular("reparse point")),
        Err(OpenError::IsADirectory) => return Ok(Unlinked::NotRegular("directory")),
        Err(e) => return Err(e.status()),
    };
    delete_by_handle(&file)?;
    Ok(Unlinked::Removed)
}

/// Flushes the file or directory behind `handle` to disk.
#[cfg(windows)]
fn flush(handle: &OwnedHandle) -> ffi::NTSTATUS {
    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    // SAFETY: `handle` is open; the status block is a live local.
    unsafe { ffi::NtFlushBuffersFile(handle.raw(), &mut io_status) }
}

#[cfg(windows)]
pub fn write(path: &str, roots: &[String], text: &str) -> Result<Value, Denial> {
    let key = format!(
        "local-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    write_call(path, roots, text, &key, None)
}

/// The directory a write to a path acts in, held open from the volume root
/// down, and the name the write replaces in it.
#[cfg(windows)]
struct WriteParent {
    root: VerifiedRoot,
    /// The directories between a directory root and the parent, the parent
    /// last. Empty when the parent is the root, or for a file root.
    below: Vec<OwnedHandle>,
    /// The parent directory, as spelled in the path.
    dir: PathBuf,
    /// The last component of the path.
    leaf: String,
}

#[cfg(windows)]
impl WriteParent {
    /// The handle on the directory the write acts in.
    fn parent(&self) -> &OwnedHandle {
        if let Some(dir) = self.below.last() {
            dir
        } else if self.root.is_directory {
            &self.root.handle
        } else {
            self.root
                .ancestors
                .last()
                .expect("a file root has a parent directory")
        }
    }

    /// Checks where the parent directory really is, against the root.
    fn check_inside(&self) -> Result<(), Denial> {
        let guid = get_volume_guid_path(self.parent().raw())?;
        let inside = if self.root.is_directory {
            guid_path_inside_component_wise(&guid, &self.root.guid_path, true)
        } else {
            guid_is_parent_of(&guid, &self.root.guid_path)
        };
        if inside {
            Ok(())
        } else {
            Err(outside(&self.dir.join(&self.leaf)))
        }
    }
}

/// Selects the root for `path` by the whole path (so a file root grants a
/// write to itself), walks to the directory that holds it and checks that
/// directory. Every directory from the volume root down to that one is
/// opened with the sharing mode `share` and stays open as long as the
/// returned value lives, so a caller passing [`SHARE_NO_DELETE`] keeps them
/// all from being moved until it drops the value.
#[cfg(windows)]
fn open_write_parent(path: &str, roots: &[String], share: u32) -> Result<WriteParent, Denial> {
    validate_raw_spelling(path)?;
    let p = Path::new(path);
    let (Some(parent), Some(leaf)) = (p.parent(), p.file_name().and_then(OsStr::to_str)) else {
        return Err(outside(p));
    };
    let parent_str = parent.to_str().ok_or_else(|| outside(p))?;
    let root = select_root(path, roots, share)?;
    let below = if root.is_directory {
        // The directory root itself is not an entry a write can replace.
        if path == root.manifest || !path_under(parent_str, &root.manifest) {
            return Err(outside(p));
        }
        let components = components_below(parent_str, &root.manifest);
        walk_dirs(
            &root.handle,
            &components,
            Open::existing(DIR_WALK).share(share),
        )
        .map_err(|e| intermediate_denial(e, p))?
    } else {
        // select_root grants a file root only to its own path. Its parent
        // is the last of the root's ancestors, which walk_from_volume_root
        // opened and keeps in `root.ancestors`; WriteParent::parent uses it.
        Vec::new()
    };
    let wp = WriteParent {
        root,
        below,
        dir: parent.to_path_buf(),
        leaf: leaf.to_owned(),
    };
    wp.check_inside()?;
    Ok(wp)
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
    let temp_str = temp_name_os
        .to_str()
        .ok_or_else(|| Denial::invalid("temporary file name is not valid UTF-8"))?;

    // `wp` holds every directory from the volume root down to the parent,
    // opened without FILE_SHARE_DELETE, until write_call returns. Renaming
    // or deleting a directory needs it opened for DELETE, which those
    // handles refuse, so none can be moved out of the root before the
    // rename below lands where the check said it would.
    let wp = open_write_parent(path, roots, SHARE_NO_DELETE)?;
    let target_path = wp.dir.join(&wp.leaf);
    let temp_path = wp.dir.join(temp_str);

    // A replaced file keeps its DACL. A new file gets no explicit
    // descriptor, so NTFS gives it what the folder's inheritable entries
    // grant, marked inherited, and later changes to the folder reach it.
    let target_dacl = existing_target_dacl(wp.parent(), &wp.leaf, &target_path)?;

    let lease = TempLease {
        call_key: call_key.to_owned(),
        dir: wp.dir.clone(),
        target: OsString::from(&wp.leaf),
        temp: temp_name_os.clone(),
        roots: roots.to_vec(),
    };
    // With a ledger, the record is durable before the file exists, so no
    // crash can leave a temporary file that nothing has recorded.
    let hold = match ledger {
        Some(l) => Some(l.record(&lease).map_err(|e| {
            Denial::new(
                codes::IO,
                format!(
                    "the temporary file for {} could not be recorded: {e}",
                    target_path.display()
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

    let create = Open {
        access: ffi::FILE_GENERIC_WRITE | ffi::DELETE | ffi::SYNCHRONIZE,
        share: SHARE_ALL,
        disposition: ffi::FILE_CREATE,
        options: ffi::FILE_NON_DIRECTORY_FILE,
        security_descriptor: target_dacl.as_ref().map_or(std::ptr::null_mut(), |d| d.sd),
    };
    // On a failed create the record stays: a file an earlier send left
    // under this name may still be there, and cleanup checks for it.
    let mut replaced = false;
    let temp = loop {
        match nt_open_relative(wp.parent().raw(), temp_str, create) {
            Ok(file) => break file,
            // The name belongs to this call alone, so a regular file already
            // under it is what an earlier send of the same call left. It is
            // replaced once; a directory or link there is an error.
            Err(OpenError::Collision)
                if !replaced
                    && matches!(
                        unlink_file(wp.parent().raw(), temp_str),
                        Ok(Unlinked::Removed)
                    ) =>
            {
                replaced = true;
            }
            Err(e) => return Err(nt_io(&temp_path, "creating", e.status())),
        }
    };
    if let Some(l) = ledger {
        l.created(&lease);
    }

    // On a failure from here on, the temporary file is removed; the record
    // is cleared only when that worked, otherwise cleanup tries again.
    let abandon = |temp: OwnedHandle, hold: Option<Box<dyn TempHold>>| {
        if delete_by_handle(&temp).is_ok() {
            drop(temp);
            clear(hold);
        }
    };

    let bytes = text.as_bytes();
    let mut written = 0usize;
    while written < bytes.len() {
        let chunk = &bytes[written..];
        let mut io_status = ffi::IO_STATUS_BLOCK {
            Status: 0,
            Information: 0,
        };
        let mut offset = written as i64;
        // SAFETY: `chunk` is live for the call; its length is at most
        // MAX_WRITE_BYTES, so it fits in a u32.
        let status = unsafe {
            ffi::NtWriteFile(
                temp.raw(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut io_status,
                chunk.as_ptr().cast(),
                chunk.len() as u32,
                &mut offset,
                std::ptr::null_mut(),
            )
        };
        if status < 0 || io_status.Information == 0 {
            abandon(temp, hold);
            return Err(nt_io(&temp_path, "writing", status));
        }
        written += io_status.Information;
    }

    // The data must be on disk before the rename makes it the target: the
    // rename is journaled, the data is not, so renaming unflushed data can
    // leave an empty or partial file after a crash. A failed flush stops
    // the write, as a failed sync does on Unix.
    #[cfg(test)]
    let flush_status = if hooks::FAIL_TEMP_FLUSH.with(std::cell::Cell::get) {
        ffi::STATUS_IO_DEVICE_ERROR
    } else {
        flush(&temp)
    };
    #[cfg(not(test))]
    let flush_status = flush(&temp);
    if flush_status < 0 {
        abandon(temp, hold);
        return Err(nt_io(
            &target_path,
            "flushing the new contents",
            flush_status,
        ));
    }

    // The parent is held, but where it is gets checked again before the
    // rename so the write does not rely on the hold alone.
    if let Err(d) = wp.check_inside() {
        abandon(temp, hold);
        return Err(d);
    }

    // FileRenameInformationEx with POSIX semantics replaces the target even
    // while it is open. If the volume refuses it, the write fails: there is
    // no fallback to a non-POSIX rename or a copy.
    // A read-only target is replaced, as renameat ignores a target's mode.
    let wide_target: Vec<u16> = wp.leaf.encode_utf16().collect();
    let name_bytes = wide_target.len() * std::mem::size_of::<u16>();
    let name_offset = std::mem::offset_of!(ffi::FILE_RENAME_INFORMATION_EX, FileName);
    let struct_size =
        (name_offset + name_bytes).max(std::mem::size_of::<ffi::FILE_RENAME_INFORMATION_EX>());
    // u64 storage gives the 8-byte alignment the structure needs.
    let mut rename_buf = vec![0u64; struct_size.div_ceil(std::mem::size_of::<u64>())];
    let rename_info = rename_buf
        .as_mut_ptr()
        .cast::<ffi::FILE_RENAME_INFORMATION_EX>();
    // SAFETY: the buffer is aligned for the structure and holds its header
    // plus `name_bytes` of name; fields are written through the raw pointer
    // (no reference is made), and the name is copied through a pointer
    // derived from it, so it may run past the declared one-element array.
    unsafe {
        std::ptr::addr_of_mut!((*rename_info).Flags).write(
            ffi::FILE_RENAME_REPLACE_IF_EXISTS
                | ffi::FILE_RENAME_POSIX_SEMANTICS
                | ffi::FILE_RENAME_IGNORE_READONLY_ATTRIBUTE,
        );
        std::ptr::addr_of_mut!((*rename_info).RootDirectory).write(wp.parent().raw());
        std::ptr::addr_of_mut!((*rename_info).FileNameLength).write(name_bytes as u32);
        std::ptr::copy_nonoverlapping(
            wide_target.as_ptr(),
            std::ptr::addr_of_mut!((*rename_info).FileName).cast::<u16>(),
            wide_target.len(),
        );
    }

    let mut io_status = ffi::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let rename = |io_status: &mut ffi::IO_STATUS_BLOCK| {
        // SAFETY: `rename_info` points into `rename_buf`, which is
        // `struct_size` bytes or more and outlives the call.
        unsafe {
            ffi::NtSetInformationFile(
                temp.raw(),
                io_status,
                rename_info.cast(),
                struct_size as u32,
                ffi::FileRenameInformationEx,
            )
        }
    };
    #[cfg(test)]
    let rename_status = if hooks::REFUSE_POSIX_RENAME.with(std::cell::Cell::get) {
        ffi::STATUS_NOT_SUPPORTED
    } else {
        rename(&mut io_status)
    };
    #[cfg(not(test))]
    let rename_status = rename(&mut io_status);

    if rename_status < 0 {
        abandon(temp, hold);
        if rename_status == ffi::STATUS_NOT_SUPPORTED
            || rename_status == ffi::STATUS_INVALID_PARAMETER
        {
            return Err(Denial::denied(format!(
                "volume does not support POSIX replace rename (status 0x{:08x})",
                rename_status as u32
            )));
        }
        return Err(nt_io(
            &target_path,
            "renaming the new contents into place",
            rename_status,
        ));
    }
    drop(temp);

    // Make the rename itself durable. Best effort, as on Unix: a failure
    // here leaves the new file in place. Flushing a directory needs write
    // access, which the held handle does not carry.
    if let Ok(dir) = nt_reopen(
        wp.parent(),
        Open::existing(ffi::FILE_WRITE_DATA | ffi::SYNCHRONIZE).directory(),
    ) {
        let _ = flush(&dir);
    }

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
    let Some(temp_str) = lease.temp.to_str() else {
        return Ok(TempRemoval::Refused(Denial::invalid(
            "the recorded temporary file name is not UTF-8",
        )));
    };
    let joined = lease.dir.join(&lease.target);
    let Some(path) = joined.to_str() else {
        return Ok(TempRemoval::Refused(Denial::invalid(
            "the recorded path is not UTF-8",
        )));
    };

    // The lease's directory is reached exactly as write_call reached it:
    // the root chosen by the whole path, then a walk down to `lease.dir`.
    let wp = match open_write_parent(path, &lease.roots, SHARE_ALL) {
        Ok(wp) => wp,
        Err(d) if d.code == codes::NOT_FOUND => return Ok(TempRemoval::Absent),
        Err(d) if d.code == codes::IO => return Err(d),
        Err(d) => return Ok(TempRemoval::Refused(d)),
    };
    // The walk follows the spelling of `lease.dir` and refuses every reparse
    // point, so it cannot end anywhere that spelling does not name. What
    // this check catches is a `lease.target` that is not one plain name: one
    // holding a separator makes the parent of `dir\target` a deeper
    // directory than `lease.dir`.
    if wp.dir != lease.dir || OsStr::new(&wp.leaf) != lease.target {
        return Ok(TempRemoval::Refused(Denial::denied(format!(
            "{} now resolves to {}",
            joined.display(),
            wp.dir.join(&wp.leaf).display()
        ))));
    }

    let temp_path = wp.dir.join(temp_str);
    Ok(match unlink_file(wp.parent().raw(), temp_str) {
        Ok(Unlinked::Removed) => TempRemoval::Removed,
        Ok(Unlinked::Absent) => TempRemoval::Absent,
        Ok(Unlinked::NotRegular(kind)) => TempRemoval::Refused(Denial::denied(format!(
            "{} is a {kind}, not a temporary file",
            temp_path.display()
        ))),
        Err(status) => return Err(nt_io(&temp_path, "removing", status)),
    })
}

#[cfg(windows)]
pub fn remove_legacy_temps(path: &str, roots: &[String]) -> Result<usize, Denial> {
    let wp = open_write_parent(path, roots, SHARE_ALL)?;
    let lister = nt_reopen(
        wp.parent(),
        Open::existing(ffi::FILE_LIST_DIRECTORY | ffi::SYNCHRONIZE).directory(),
    )
    .map_err(|e| nt_io(&wp.dir, "opening the directory to list", e.status()))?;
    let mut names = Vec::new();
    scan_directory(lister.raw(), |name, _| {
        if let Ok(name) = String::from_utf16(name)
            && is_legacy_temp_name(OsStr::new(&name))
        {
            names.push(name);
        }
        true
    })
    .map_err(|s| nt_io(&wp.dir, "listing", s))?;
    let mut removed = 0;
    for name in names {
        // Each name is examined again as it is removed: only a regular
        // file goes, whatever the listing said it was.
        match unlink_file(wp.parent().raw(), &name) {
            Ok(Unlinked::Removed) => removed += 1,
            Ok(_) => {}
            Err(status) => return Err(nt_io(&wp.dir.join(&name), "removing", status)),
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

    #[test]
    fn path_under_matches_by_component_and_volume_roots_cover_their_drive() {
        assert!(path_under(r"C:\root", r"C:\root"));
        assert!(path_under(r"C:\root\a\b.txt", r"C:\root"));
        assert!(!path_under(r"C:\rootx\a.txt", r"C:\root"));
        assert!(!path_under(r"C:\other", r"C:\root"));
        assert!(path_under(r"C:\", r"C:\"));
        assert!(path_under(r"C:\x", r"C:\"));
        assert!(path_under(r"C:\x\y.txt", r"C:\"));
        assert!(!path_under(r"D:\x", r"C:\"));
    }

    #[test]
    fn components_below_handles_directory_and_volume_roots() {
        assert!(components_below(r"C:\root", r"C:\root").is_empty());
        assert_eq!(components_below(r"C:\root\a\b", r"C:\root"), ["a", "b"]);
        assert!(components_below(r"C:\", r"C:\").is_empty());
        assert_eq!(components_below(r"C:\x\y", r"C:\"), ["x", "y"]);
    }

    #[cfg(windows)]
    mod win_tests {
        use super::*;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use std::sync::Mutex;

        /// Win32 calls only the tests make.
        #[allow(non_snake_case)]
        mod test_ffi {
            use std::ffi::c_void;

            pub const SDDL_REVISION_1: u32 = 1;
            pub const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
            pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            pub const ERROR_SHARING_VIOLATION: i32 = 32;

            #[link(name = "advapi32")]
            unsafe extern "system" {
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

                pub fn SetFileSecurityW(
                    lpFileName: *const u16,
                    SecurityInformation: u32,
                    pSecurityDescriptor: *mut c_void,
                ) -> i32;
            }

            #[link(name = "kernel32")]
            unsafe extern "system" {
                pub fn GetVolumeNameForVolumeMountPointW(
                    lpszVolumeMountPoint: *const u16,
                    lpszVolumeName: *mut u16,
                    cchBufferLength: u32,
                ) -> i32;

                pub fn SetVolumeMountPointW(
                    lpszVolumeMountPoint: *const u16,
                    lpszVolumeName: *const u16,
                ) -> i32;

                pub fn DeleteVolumeMountPointW(lpszVolumeMountPoint: *const u16) -> i32;
            }
        }

        struct WinTree {
            base: PathBuf,
            root: PathBuf,
            outside: PathBuf,
        }

        impl WinTree {
            fn new(tag: &str) -> Self {
                Self::new_under(tag, &std::env::temp_dir())
            }

            fn new_under(tag: &str, parent: &Path) -> Self {
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let base = parent.join(format!(
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

            /// The names in the root directory that are temporary files.
            fn temps_in(&self, rel: &str) -> Vec<String> {
                std::fs::read_dir(self.root.join(rel))
                    .expect("read dir")
                    .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(".basal-"))
                    .collect()
            }
        }

        impl Drop for WinTree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }

        #[test]
        fn tilde_root_reads_below_the_process_tokens_profile() {
            let profile = process_profile_directory().expect("process token profile");
            let tree = WinTree::new_under("profile", Path::new(&profile));
            let name = tree.base.file_name().unwrap().to_str().unwrap();
            let roots = vec![format!("~/{name}/root")];
            assert_eq!(
                read(&tree.p("sub\\b.txt"), &roots, 100).unwrap()["text"],
                "nested"
            );
            for root in [
                format!("~/{name}/root/.."),
                format!("~/{name}//root"),
                format!("~/{name}/root:stream"),
            ] {
                assert!(read(&tree.p("sub\\b.txt"), &[root], 100).is_err());
            }
        }

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(Some(0)).collect()
        }

        /// Fails the test unless `path` is a symlink, junction or mount point.
        fn assert_link(path: &Path) {
            let meta = std::fs::symlink_metadata(path)
                .unwrap_or_else(|e| panic!("{} was not created: {e}", path.display()));
            assert!(
                meta.file_type().is_symlink(),
                "{} is not a link",
                path.display()
            );
        }

        /// Creates a file symlink. Needs SeCreateSymbolicLinkPrivilege, which
        /// an elevated administrator (as on GitHub's Windows runners) holds.
        fn file_symlink(link: &Path, target: &Path) {
            std::os::windows::fs::symlink_file(target, link).unwrap_or_else(|e| {
                panic!(
                    "creating symlink {} failed (needs SeCreateSymbolicLinkPrivilege): {e}",
                    link.display()
                )
            });
            assert_link(link);
        }

        /// Creates a directory junction; needs no privilege.
        fn junction(link: &Path, target: &Path) {
            // Subprocess text must not interrupt libtest's result lines.
            // Keep it for diagnostics only when fixture creation fails.
            let output = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    link.to_str().unwrap(),
                    target.to_str().unwrap(),
                ])
                .output()
                .expect("run mklink /J");
            assert!(
                output.status.success(),
                "mklink /J {} failed: {} {}",
                link.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_link(link);
        }

        /// Sets the DACL of `path` from an SDDL string, including its
        /// protection flag.
        fn set_dacl(path: &Path, sddl: &str) {
            let wide_sddl = wide(sddl);
            let mut sd = std::ptr::null_mut();
            let ok = unsafe {
                test_ffi::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide_sddl.as_ptr(),
                    test_ffi::SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "convert {sddl}: {}", std::io::Error::last_os_error());
            let mut info = ffi::DACL_SECURITY_INFORMATION;
            if sddl.starts_with("D:P") {
                info |= test_ffi::PROTECTED_DACL_SECURITY_INFORMATION;
            }
            let wide_path = wide(path.to_str().unwrap());
            let ok = unsafe { test_ffi::SetFileSecurityW(wide_path.as_ptr(), info, sd) };
            let err = std::io::Error::last_os_error();
            unsafe { ffi::LocalFree(sd) };
            assert_ne!(ok, 0, "set the DACL of {}: {err}", path.display());
        }

        /// The DACL of `path` as SDDL, read without following a link.
        fn dacl_of(path: &Path) -> Result<String, String> {
            let file = std::fs::OpenOptions::new()
                .access_mode(ffi::READ_CONTROL)
                .custom_flags(
                    test_ffi::FILE_FLAG_BACKUP_SEMANTICS | test_ffi::FILE_FLAG_OPEN_REPARSE_POINT,
                )
                .open(path)
                .map_err(|e| format!("open {}: {e}", path.display()))?;
            let mut sd = std::ptr::null_mut();
            let err = unsafe {
                ffi::GetSecurityInfo(
                    file.as_raw_handle(),
                    ffi::SE_FILE_OBJECT,
                    ffi::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if err != 0 {
                return Err(format!("GetSecurityInfo {}: {err}", path.display()));
            }
            let mut text = std::ptr::null_mut();
            let ok = unsafe {
                test_ffi::ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    sd,
                    test_ffi::SDDL_REVISION_1,
                    ffi::DACL_SECURITY_INFORMATION,
                    &mut text,
                    std::ptr::null_mut(),
                )
            };
            let result = if ok == 0 || text.is_null() {
                Err(format!("convert the DACL of {} to SDDL", path.display()))
            } else {
                let mut len = 0;
                while unsafe { *text.add(len) } != 0 {
                    len += 1;
                }
                let units = unsafe { std::slice::from_raw_parts(text, len) };
                let sddl = String::from_utf16_lossy(units);
                unsafe { ffi::LocalFree(text.cast()) };
                Ok(sddl)
            };
            unsafe { ffi::LocalFree(sd) };
            result
        }

        /// Splits `D:<flags>(<ace>)(<ace>)...` into its flags and ACEs.
        fn dacl_parts(sddl: &str) -> (String, Vec<String>) {
            let rest = sddl
                .strip_prefix("D:")
                .unwrap_or_else(|| panic!("not a DACL: {sddl}"));
            let (flags, aces) = rest.split_at(rest.find('(').unwrap_or(rest.len()));
            let aces = aces
                .split(')')
                .filter(|a| !a.is_empty())
                .map(|a| a.trim_start_matches('(').to_owned())
                .collect();
            (flags.to_owned(), aces)
        }

        /// A ledger that records the temporary file's DACL once it exists.
        #[derive(Default)]
        struct DaclObserver {
            temp_dacl: Mutex<Option<Result<String, String>>>,
        }

        struct NoHold;
        impl TempHold for NoHold {
            fn clear(self: Box<Self>) {}
        }

        impl TempLedger for DaclObserver {
            fn record(&self, _lease: &TempLease) -> Result<Box<dyn TempHold>, String> {
                Ok(Box::new(NoHold))
            }

            fn created(&self, lease: &TempLease) {
                *self.temp_dacl.lock().unwrap() = Some(dacl_of(&lease.dir.join(&lease.temp)));
            }
        }

        impl DaclObserver {
            fn temp_dacl(&self) -> String {
                self.temp_dacl
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("the ledger saw the temporary file")
                    .expect("the temporary file's DACL was read")
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

            let st_root = stat(&t.root.display().to_string(), &roots).expect("stat root");
            assert_eq!(st_root["kind"], "dir");

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

            let out_link = t.root.join("symlink_out.txt");
            let in_link = t.root.join("symlink_in.txt");
            file_symlink(&out_link, &t.outside.join("secret.txt"));
            file_symlink(&in_link, &t.root.join("a.txt"));

            let err_out = read(&out_link.display().to_string(), &roots, 1024)
                .expect_err("symlink out denied");
            assert_eq!(err_out.code, codes::DENIED);
            // Every reparse point is refused, in-root ones included.
            let err_in = read(&in_link.display().to_string(), &roots, 1024)
                .expect_err("in-root symlink denied");
            assert_eq!(err_in.code, codes::DENIED);
            assert!(
                err_in.message.contains("reparse point"),
                "{}",
                err_in.message
            );

            let err_write =
                write(&out_link.display().to_string(), &roots, "x").expect_err("write link");
            assert_eq!(err_write.code, codes::DENIED);
            assert_eq!(
                std::fs::read_to_string(t.outside.join("secret.txt")).unwrap(),
                "secret"
            );
        }

        #[test]
        fn windows_reparse_points_at_root_middle_leaf() {
            let t = WinTree::new("reparse-points");
            let roots = t.roots();

            // A junction as the last component.
            let leaf_junc = t.root.join("leaf_junc");
            junction(&leaf_junc, &t.outside);
            let st = stat(&leaf_junc.display().to_string(), &roots)
                .expect_err("leaf reparse stat denial");
            assert_eq!(st.code, codes::DENIED);
            assert!(st.message.contains("reparse point"), "{}", st.message);

            // A junction in the middle of the path.
            let mid_junc = t.root.join("mid_junc");
            junction(&mid_junc, &t.outside);
            let target = mid_junc.join("secret.txt").display().to_string();
            let err = read(&target, &roots, 1024).expect_err("mid reparse denial");
            assert_eq!(err.code, codes::DENIED);
            assert!(err.message.contains("reparse point"), "{}", err.message);

            // The root itself is a junction: refused for being one, not
            // for some other failure of the walk.
            let root_junc = t.base.join("root_junc");
            junction(&root_junc, &t.root);
            let junc_roots = vec![root_junc.display().to_string()];
            let target = root_junc.join("a.txt").display().to_string();
            let err = read(&target, &junc_roots, 1024).expect_err("root reparse denial");
            assert_eq!(err.code, codes::DENIED);
            assert!(err.message.contains("reparse point"), "{}", err.message);
        }

        /// Removes a volume mount point when the test ends, however it ends.
        struct MountPoint(Vec<u16>);

        impl Drop for MountPoint {
            fn drop(&mut self) {
                unsafe { test_ffi::DeleteVolumeMountPointW(self.0.as_ptr()) };
            }
        }

        #[test]
        fn windows_mount_point_refused() {
            let t = WinTree::new("mount-point");
            let roots = t.roots();
            let mnt = t.root.join("mnt");
            std::fs::create_dir(&mnt).expect("create mount directory");

            // Mount the volume the tree is on at root\mnt. Needs an
            // administrator, as on GitHub's Windows runners.
            let drive = &t.root.to_str().unwrap()[..3];
            let mut volume = vec![0u16; 64];
            let ok = unsafe {
                test_ffi::GetVolumeNameForVolumeMountPointW(
                    wide(drive).as_ptr(),
                    volume.as_mut_ptr(),
                    volume.len() as u32,
                )
            };
            assert_ne!(
                ok,
                0,
                "volume name of {drive}: {}",
                std::io::Error::last_os_error()
            );
            let mount_at = wide(&format!("{}\\", mnt.display()));
            let ok = unsafe { test_ffi::SetVolumeMountPointW(mount_at.as_ptr(), volume.as_ptr()) };
            assert_ne!(
                ok,
                0,
                "mount {drive} at {} (needs an administrator): {}",
                mnt.display(),
                std::io::Error::last_os_error()
            );
            let _mounted = MountPoint(mount_at);
            assert_link(&mnt);

            let st = stat(&mnt.display().to_string(), &roots).expect_err("stat mount point");
            assert_eq!(st.code, codes::DENIED);
            assert!(st.message.contains("reparse point"), "{}", st.message);

            let through = mnt.join("Windows").display().to_string();
            let err = list(&through, &roots).expect_err("list through mount point");
            assert_eq!(err.code, codes::DENIED);
            assert!(err.message.contains("reparse point"), "{}", err.message);

            let mnt_roots = vec![mnt.display().to_string()];
            let err = list(&mnt.display().to_string(), &mnt_roots).expect_err("mount point root");
            assert_eq!(err.code, codes::DENIED);
            assert!(err.message.contains("reparse point"), "{}", err.message);
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

            // A case alias inside the root opens the same entry.
            let case_alias_inside = t.root.join("SUB\\B.TXT").display().to_string();
            let val = read(&case_alias_inside, &roots, 1024).expect("read case alias inside");
            assert_eq!(val["text"], "nested");

            // Outside the root: refused because the path's spelling does not
            // lie under the root's, before anything is opened.
            let case_outside = t.outside.join("SECRET.TXT").display().to_string();
            let err_case = read(&case_outside, &roots, 1024).expect_err("case outside denied");
            assert_eq!(err_case.code, codes::DENIED);

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
            // Runs on this thread only, after resolution and before the open.
            hooks::BEFORE_READ_OPEN.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    std::fs::remove_file(&target_clone).expect("remove the checked file");
                    file_symlink(&target_clone, &outside_clone);
                }));
            });
            let res = read(&target_path.display().to_string(), &roots, 1024);
            hooks::BEFORE_READ_OPEN.with(|h| *h.borrow_mut() = None);

            let denial = res.expect_err("a symlink swapped in after the check is refused");
            assert_eq!(denial.code, codes::DENIED);
            assert!(
                denial.message.contains("became a symlink"),
                "{}",
                denial.message
            );
        }

        #[test]
        fn windows_posix_rename_refusal_simulated() {
            // NTFS and ReFS on Windows 10 1709 and later support the POSIX
            // rename, so a test hook (on this thread only) makes the rename
            // fail as a refusing volume would, to show the write is refused
            // with no fallback.
            let t = WinTree::new("posix-refusal");
            let roots = t.roots();
            let target = t.p("refused_rename.txt");

            hooks::REFUSE_POSIX_RENAME.with(|f| f.set(true));
            let res = write(&target, &roots, "data");
            hooks::REFUSE_POSIX_RENAME.with(|f| f.set(false));

            let err = res.expect_err("POSIX rename refusal");
            assert_eq!(err.code, codes::DENIED);
            assert!(
                err.message
                    .contains("volume does not support POSIX replace rename"),
                "{}",
                err.message
            );
            assert!(
                !Path::new(&target).exists(),
                "nothing was renamed into place"
            );
            assert!(t.temps_in("").is_empty(), "leftover temp files");
        }

        #[test]
        fn windows_failed_flush_stops_the_replace() {
            let t = WinTree::new("flush-failure");
            let roots = t.roots();

            hooks::FAIL_TEMP_FLUSH.with(|f| f.set(true));
            let res = write(&t.p("a.txt"), &roots, "unflushed");
            hooks::FAIL_TEMP_FLUSH.with(|f| f.set(false));

            let err = res.expect_err("a failed flush refuses the rename");
            assert_eq!(err.code, codes::IO);
            assert_eq!(std::fs::read_to_string(t.p("a.txt")).unwrap(), "inside");
            assert!(t.temps_in("").is_empty(), "leftover temp files");
        }

        #[test]
        fn windows_file_root_grants_no_sibling() {
            let t = WinTree::new("file-root");
            let file_path = t.p("a.txt");
            let roots = vec![file_path.clone()];

            let res = read(&file_path, &roots, 1024).expect("read file root");
            assert_eq!(res["text"], "inside");
            let st = stat(&file_path, &roots).expect("stat file root");
            assert_eq!(st["kind"], "file");

            let sibling = t.p("sub\\b.txt");
            let err = read(&sibling, &roots, 1024).expect_err("sibling denied");
            assert_eq!(err.code, codes::DENIED);
            let err = list(&file_path, &roots).expect_err("a file root is not listed");
            assert_eq!(err.code, codes::DENIED);

            // Writing to the granted file's own path replaces its contents.
            resolve(&file_path, &roots, Purpose::Write).expect("authorized");
            write(&file_path, &roots, "replaced").expect("replace file root");
            assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "replaced");

            let err_write =
                write(&sibling, &roots, "sibling data").expect_err("sibling write denied");
            assert_eq!(err_write.code, codes::DENIED);
            let err_write = write(&t.p("new.txt"), &roots, "new").expect_err("new sibling denied");
            assert_eq!(err_write.code, codes::DENIED);
            assert!(!Path::new(&t.p("new.txt")).exists());
        }

        #[test]
        fn windows_file_root_is_read_without_execute_access() {
            let t = WinTree::new("file-root-no-execute");
            let file_path = t.p("a.txt");
            // Read for everyone, and no execute right for anyone.
            set_dacl(Path::new(&file_path), "D:P(A;;FR;;;WD)");
            let roots = vec![file_path.clone()];

            let res = read(&file_path, &roots, 1024).expect("read file root");
            assert_eq!(res["text"], "inside");
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
            assert!(t.temps_in("").is_empty(), "leftover temp files");
        }

        #[test]
        fn windows_write_creates_file_in_subdirectory() {
            let t = WinTree::new("write-subdir");
            let roots = t.roots();
            let target = t.p("sub\\created.txt");

            write(&target, &roots, "created").expect("create in subdirectory");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "created");
            assert!(t.temps_in("sub").is_empty(), "leftover temp files");

            let missing_parent = t.p("nowhere\\x.txt");
            let err = write(&missing_parent, &roots, "x").expect_err("missing parent");
            assert_eq!(err.code, codes::DENIED);

            let dir_target = write(&t.p("sub"), &roots, "x").expect_err("directory target");
            assert_eq!(dir_target.code, codes::INVALID_ARGUMENTS);
        }

        #[test]
        fn windows_write_replaces_read_only_file() {
            let t = WinTree::new("write-read-only");
            let roots = t.roots();
            let target = t.root.join("a.txt");
            let mut perms = std::fs::metadata(&target).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&target, perms).expect("set read-only");

            write(&target.display().to_string(), &roots, "replaced").expect("replace");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "replaced");
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

            write(&link.display().to_string(), &roots, "new link content").expect("write link");

            // The name was replaced; the other link keeps the old contents.
            assert_eq!(std::fs::read_to_string(&link).unwrap(), "new link content");
            assert_eq!(std::fs::read_to_string(&orig).unwrap(), "initial content");
        }

        #[test]
        fn windows_replace_keeps_the_protected_dacl() {
            let t = WinTree::new("dacl-replace");
            let roots = t.roots();
            let target = t.root.join("protected.txt");
            std::fs::write(&target, "secret").expect("write target");
            set_dacl(&target, "D:P(A;;FA;;;WD)");
            let (flags, aces) = dacl_parts(&dacl_of(&target).expect("target DACL"));
            assert!(flags.contains('P'), "the DACL was applied: {flags}");
            assert_eq!(aces, ["A;;FA;;;WD"], "the DACL was applied");

            let observer = DaclObserver::default();
            write_call(
                &target.display().to_string(),
                &roots,
                "replacement text",
                "call-dacl-1",
                Some(&observer),
            )
            .expect("write call with dacl");

            for (what, sddl) in [
                ("temporary file", observer.temp_dacl()),
                ("result", dacl_of(&target).expect("result DACL")),
            ] {
                let (flags, aces) = dacl_parts(&sddl);
                assert!(flags.contains('P'), "the {what} is protected: {sddl}");
                assert_eq!(aces, ["A;;FA;;;WD"], "the {what} keeps the DACL: {sddl}");
            }
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "replacement text"
            );
        }

        #[test]
        fn windows_create_inherits_the_folder_dacl() {
            let t = WinTree::new("dacl-create");
            let roots = t.roots();
            let sub = t.root.join("sub");
            set_dacl(&sub, "D:P(A;OICI;FA;;;WD)");
            let new_file = sub.join("new_file.txt");

            let observer = DaclObserver::default();
            write_call(
                &new_file.display().to_string(),
                &roots,
                "initial created text",
                "call-dacl-create-1",
                Some(&observer),
            )
            .expect("write call create with dacl");

            for (what, sddl) in [
                ("temporary file", observer.temp_dacl()),
                ("result", dacl_of(&new_file).expect("result DACL")),
            ] {
                let (flags, aces) = dacl_parts(&sddl);
                assert!(!flags.contains('P'), "the {what} is not protected: {sddl}");
                assert_eq!(
                    aces,
                    ["A;ID;FA;;;WD"],
                    "the {what} carries the folder's entry, inherited: {sddl}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&new_file).unwrap(),
                "initial created text"
            );
        }

        /// A test `TempLedger` that, when the write records its temporary
        /// file, tries to move a directory out of the root.
        struct MoveDuringWrite {
            from: PathBuf,
            to: PathBuf,
            result: Mutex<Option<std::io::Result<()>>>,
        }

        impl TempLedger for MoveDuringWrite {
            fn record(&self, _lease: &TempLease) -> Result<Box<dyn TempHold>, String> {
                // No file beneath `from` is open yet, so the write's own
                // handle on that directory is the only thing that can stop
                // the move.
                *self.result.lock().unwrap() = Some(std::fs::rename(&self.from, &self.to));
                Ok(Box::new(NoHold))
            }
        }

        #[test]
        fn windows_write_parent_cannot_be_moved_out_during_the_write() {
            let t = WinTree::new("parent-move");
            let roots = t.roots();
            let moved = t.outside.join("sub-moved");
            let ledger = MoveDuringWrite {
                from: t.root.join("sub"),
                to: moved.clone(),
                result: Mutex::new(None),
            };

            write_call(
                &t.p("sub\\new.txt"),
                &roots,
                "data",
                "call-move-1",
                Some(&ledger),
            )
            .expect("write");

            let attempt = ledger
                .result
                .lock()
                .unwrap()
                .take()
                .expect("move attempted");
            let err = attempt.expect_err("the held parent cannot be moved");
            assert_eq!(
                err.raw_os_error(),
                Some(test_ffi::ERROR_SHARING_VIOLATION),
                "{err}"
            );
            assert!(!moved.exists(), "nothing was moved outside the root");
            assert_eq!(
                std::fs::read_to_string(t.p("sub\\new.txt")).unwrap(),
                "data"
            );
        }

        #[test]
        fn windows_list_directory_root_and_subdirectory() {
            let t = WinTree::new("list");
            let roots = t.roots();

            let root_list = list(&t.root.display().to_string(), &roots).expect("list root");
            assert_eq!(
                root_list,
                json!([
                    { "name": "a.txt", "kind": "file" },
                    { "name": "sub", "kind": "dir" },
                ])
            );
            let sub_list = list(&t.p("sub"), &roots).expect("list sub");
            assert_eq!(sub_list, json!([{ "name": "b.txt", "kind": "file" }]));

            let missing = list(&t.p("missing"), &roots).expect_err("missing");
            assert_eq!(missing.code, codes::NOT_FOUND);
            let file = list(&t.p("a.txt"), &roots).expect_err("a file");
            assert_eq!(file.code, codes::IO);
        }

        #[test]
        fn windows_failed_directory_query_is_an_error() {
            let t = WinTree::new("list-failure");
            // The walk's handle on a directory root has no FILE_LIST_DIRECTORY,
            // so querying it fails on the first call.
            let vr =
                walk_from_volume_root(&t.root.display().to_string(), SHARE_ALL).expect("walk root");
            let mut seen = 0;
            let res = scan_directory(vr.handle.raw(), |_, _| {
                seen += 1;
                true
            });
            assert_eq!(res, Err(ffi::STATUS_ACCESS_DENIED));
            assert_eq!(seen, 0);
        }

        fn lease(dir: &Path, target: &str, temp: &str, roots: Vec<String>) -> TempLease {
            TempLease {
                call_key: "k".to_owned(),
                dir: dir.to_path_buf(),
                target: OsString::from(target),
                temp: OsString::from(temp),
                roots,
            }
        }

        #[test]
        fn windows_remove_temp_acts_in_the_recorded_directory() {
            let t = WinTree::new("remove-temp-subdir");
            let temp = ".basal-call-k1.tmp";
            std::fs::write(t.root.join("sub").join(temp), "partial").unwrap();
            // The same name in the root is not the recorded file.
            std::fs::write(t.root.join(temp), "decoy").unwrap();
            let l = lease(&t.root.join("sub"), "x.txt", temp, t.roots());

            assert_eq!(remove_temp(&l), Ok(TempRemoval::Removed));
            assert!(!t.root.join("sub").join(temp).exists());
            assert!(t.root.join(temp).exists(), "the root's file is untouched");
            assert_eq!(remove_temp(&l), Ok(TempRemoval::Absent));
        }

        #[test]
        fn windows_remove_temp_under_a_file_root() {
            let t = WinTree::new("remove-temp-file-root");
            let temp = ".basal-call-k2.tmp";
            std::fs::write(t.root.join(temp), "partial").unwrap();
            let l = lease(&t.root, "a.txt", temp, vec![t.p("a.txt")]);

            assert_eq!(remove_temp(&l), Ok(TempRemoval::Removed));
            assert!(!t.root.join(temp).exists());
        }

        #[test]
        fn windows_remove_temp_refuses_anything_but_a_regular_file() {
            let t = WinTree::new("remove-temp-dir");
            let temp = ".basal-call-k3.tmp";
            std::fs::create_dir(t.root.join(temp)).unwrap();
            let l = lease(&t.root, "x.txt", temp, t.roots());

            let Ok(TempRemoval::Refused(why)) = remove_temp(&l) else {
                panic!("a directory under the temporary name is refused");
            };
            assert_eq!(why.code, codes::DENIED);
            assert!(t.root.join(temp).is_dir(), "the directory is untouched");
        }

        #[test]
        fn windows_remove_temp_with_a_missing_root_is_absent() {
            let t = WinTree::new("remove-temp-gone");
            let gone = t.base.join("gone");
            let l = lease(
                &gone,
                "x.txt",
                ".basal-call-k4.tmp",
                vec![gone.display().to_string()],
            );
            assert_eq!(remove_temp(&l), Ok(TempRemoval::Absent));
        }

        #[test]
        fn windows_remove_legacy_temps_removes_only_regular_files() {
            let t = WinTree::new("legacy-temps");
            std::fs::write(t.root.join(".basal-12-3.tmp"), "old").unwrap();
            std::fs::create_dir(t.root.join(".basal-45-6.tmp")).unwrap();
            std::fs::write(t.root.join(".basal-x-1.tmp"), "not legacy").unwrap();

            assert_eq!(remove_legacy_temps(&t.p("a.txt"), &t.roots()), Ok(1));
            assert!(!t.root.join(".basal-12-3.tmp").exists());
            assert!(t.root.join(".basal-45-6.tmp").is_dir());
            assert!(t.root.join(".basal-x-1.tmp").exists());

            // Also beside a file root: the directory a write to that file
            // root acts in.
            std::fs::write(t.root.join(".basal-7-8.tmp"), "old").unwrap();
            assert_eq!(remove_legacy_temps(&t.p("a.txt"), &[t.p("a.txt")]), Ok(1));
            assert!(!t.root.join(".basal-7-8.tmp").exists());
        }

        #[test]
        fn windows_temp_name_holding_a_directory_is_not_removed() {
            let t = WinTree::new("temp-collision-dir");
            let roots = t.roots();
            let temp_dir = t.root.join(".basal-call-collide.tmp");
            std::fs::create_dir(&temp_dir).unwrap();

            let err = write_call(&t.p("x.txt"), &roots, "data", "collide", None)
                .expect_err("a directory under the temporary name stops the write");
            assert_eq!(err.code, codes::IO);
            assert!(temp_dir.is_dir(), "the directory is untouched");
            assert!(!Path::new(&t.p("x.txt")).exists());
        }

        #[test]
        fn windows_temp_name_holding_a_regular_file_is_replaced() {
            let t = WinTree::new("temp-collision-file");
            let roots = t.roots();
            std::fs::write(t.root.join(".basal-call-again.tmp"), "earlier send").unwrap();

            write_call(&t.p("x.txt"), &roots, "data", "again", None).expect("write");
            assert_eq!(std::fs::read_to_string(t.p("x.txt")).unwrap(), "data");
            assert!(t.temps_in("").is_empty(), "leftover temp files");
        }

        #[test]
        fn windows_volume_root_grants_its_children() {
            let t = WinTree::new("volume-root");
            let drive = t.root.to_str().unwrap()[..3].to_owned();
            let roots = vec![drive];

            resolve(&t.p("a.txt"), &roots, Purpose::Read).expect("resolve");
            let val = read(&t.p("a.txt"), &roots, 1024).expect("read under a volume root");
            assert_eq!(val["text"], "inside");
            let st = stat(&t.p("sub\\b.txt"), &roots).expect("stat under a volume root");
            assert_eq!(st["size"], 6);
            write(&t.p("v.txt"), &roots, "volume").expect("write under a volume root");
            assert_eq!(std::fs::read_to_string(t.p("v.txt")).unwrap(), "volume");
        }
    }
}
