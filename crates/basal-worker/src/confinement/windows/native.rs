//! The Win32 and native calls behind the worker's startup checks.
//!
//! Each reader reports what it found and leaves judging it to the pure
//! checks in [`super::attest`], [`super::mitigation`] and
//! [`super::allowlist`]. Native structure layouts are those of 64-bit
//! Windows, the only Windows target basal builds for.

use super::allowlist::{HandleEntry, Stdio};
use super::attest::{Claim, Group, LPAC_CLAIM, PrimaryFacts};
use super::mitigation::MitigationWords;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetLengthSid, GetTokenInformation, PSID, SID_AND_ATTRIBUTES,
    SetTokenInformation, TOKEN_APPCONTAINER_INFORMATION, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TokenAppContainerSid, TokenCapabilities, TokenGroups,
    TokenIntegrityLevel, TokenIsAppContainer, TokenPrivileges, TokenRestrictedSids,
    TokenSecurityAttributes, WinUntrustedLabelSid,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, PROCESS_MITIGATION_POLICY,
    ProcessChildProcessPolicy, ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy,
    ProcessImageLoadPolicy, ProcessSignaturePolicy, ProcessStrictHandleCheckPolicy,
    ProcessSystemCallDisablePolicy,
};

pub(super) type Result<T> = std::result::Result<T, String>;

/// The pseudo-handle for this process's primary token
/// (`GetCurrentProcessToken`). Querying through it opens no handle, so the
/// token check leaves nothing behind in the handle table.
pub(super) const CURRENT_PROCESS_TOKEN: HANDLE = -4isize as HANDLE;

/// `SE_GROUP_INTEGRITY`, the attribute of the integrity label in a token's
/// group list.
const SE_GROUP_INTEGRITY: u32 = 0x20;
/// `OBJ_INHERIT` in a handle-table entry's attributes.
pub(super) const OBJ_INHERIT: u32 = 0x2;
/// `STATUS_INFO_LENGTH_MISMATCH`: the buffer was too small.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xc000_0004_u32 as i32;
/// `STATUS_BUFFER_OVERFLOW`: the buffer was too small for the whole result.
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
/// `ProcessHandleInformation`, the process's own handle table.
const PROCESS_HANDLE_INFORMATION: u32 = 51;
/// `ObjectNameInformation`.
const OBJECT_NAME_INFORMATION: u32 = 1;
/// `ObjectTypesInformation`: every object type and its index.
const OBJECT_TYPES_INFORMATION: u32 = 3;
/// `TOKEN_SECURITY_ATTRIBUTE_TYPE_UINT64`.
const ATTRIBUTE_TYPE_UINT64: u16 = 2;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryInformationToken(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// The calling thread's last Win32 error, named after the call that failed.
pub(super) fn last(api: &str) -> String {
    let code = unsafe { GetLastError() };
    format!(
        "{api}: Win32 {code} ({})",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

fn status(api: &str, status: i32) -> String {
    format!("{api}: NTSTATUS {:#010x}", status as u32)
}

/// Closes a handle this process owns and reports a failure.
pub(super) fn close(handle: HANDLE, what: &str) -> Result<()> {
    if unsafe { CloseHandle(handle) } == 0 {
        Err(last(&format!("CloseHandle({what})")))
    } else {
        Ok(())
    }
}

/// The package SID from `--package-sid=`, parsed by the system's own SID
/// parser.
pub struct PackageSid(PSID);

impl PackageSid {
    /// Parses SID text. Fails for text the system does not accept as a SID.
    pub fn parse(text: &str) -> Result<Self> {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut sid: PSID = null_mut();
        if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) } == 0 {
            return Err(last(&format!("ConvertStringSidToSidW({text})")));
        }
        Ok(Self(sid))
    }
}

impl Drop for PackageSid {
    fn drop(&mut self) {
        // The SID was allocated by ConvertStringSidToSidW with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

/// A SID as text.
///
/// # Safety
///
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: PSID) -> Result<String> {
    unsafe {
        let mut text = null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err(last("ConvertSidToStringSidW"));
        }
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(result)
    }
}

/// Reads one variable-length token information class into an aligned
/// buffer.
fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    unsafe {
        let mut bytes = 0;
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last(&format!("GetTokenInformation(class {class}, size)")));
        }
        // A list with no entries can be shorter than the C declaration, which
        // reserves one entry. Keep room for it so a reference to the
        // declaration never reaches past the allocation.
        let allocation = (bytes as usize)
            .max(size_of::<TOKEN_GROUPS>())
            .max(size_of::<TOKEN_PRIVILEGES>());
        let mut data = vec![0usize; allocation.div_ceil(size_of::<usize>())];
        if GetTokenInformation(token, class, data.as_mut_ptr().cast(), bytes, &mut bytes) == 0 {
            return Err(last(&format!("GetTokenInformation(class {class})")));
        }
        Ok(data)
    }
}

/// The entries of a `TOKEN_GROUPS` buffer.
///
/// # Safety
///
/// `data` must hold a `TOKEN_GROUPS` structure returned by the system.
unsafe fn groups(data: &[usize]) -> &[SID_AND_ATTRIBUTES] {
    unsafe {
        let list = &*data.as_ptr().cast::<TOKEN_GROUPS>();
        std::slice::from_raw_parts(list.Groups.as_ptr(), list.GroupCount as usize)
    }
}

fn sid_list(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<String>> {
    let data = token_buffer(token, class)?;
    unsafe { groups(&data) }
        .iter()
        .map(|entry| unsafe { sid_string(entry.Sid) })
        .collect()
}

/// Reads every property the token check judges, each on its own.
pub(super) fn primary_facts(token: HANDLE, package: &PackageSid) -> PrimaryFacts {
    PrimaryFacts {
        lpac_claim: lpac_claim(token),
        package: appcontainer(token, package),
        capabilities: sid_list(token, TokenCapabilities),
        restricting_sids: sid_list(token, TokenRestrictedSids),
        groups: token_buffer(token, TokenGroups).and_then(|data| {
            unsafe { groups(&data) }
                .iter()
                .map(|group| {
                    Ok(Group {
                        sid: unsafe { sid_string(group.Sid) }?,
                        attributes: group.Attributes,
                    })
                })
                .collect()
        }),
        privileges: token_buffer(token, TokenPrivileges).map(|data| unsafe {
            (*data.as_ptr().cast::<TOKEN_PRIVILEGES>()).PrivilegeCount as usize
        }),
        integrity: token_buffer(token, TokenIntegrityLevel).and_then(|data| unsafe {
            sid_string((*data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid)
        }),
    }
}

/// The token's AppContainer SID, and whether it equals the package SID.
fn appcontainer(token: HANDLE, package: &PackageSid) -> Result<Option<(String, bool)>> {
    let is_app = token_buffer(token, TokenIsAppContainer)?;
    if unsafe { *is_app.as_ptr().cast::<u32>() } == 0 {
        return Ok(None);
    }
    let info = token_buffer(token, TokenAppContainerSid)?;
    let sid =
        unsafe { (*info.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer };
    if sid.is_null() {
        return Ok(None);
    }
    let equal = unsafe { EqualSid(sid, package.0) } != 0;
    Ok(Some((unsafe { sid_string(sid) }?, equal)))
}

// Token security attributes have no Win32 reader, so the claim is read with
// the native call. The layouts are those of
// TOKEN_SECURITY_ATTRIBUTES_INFORMATION and TOKEN_SECURITY_ATTRIBUTE_V1.
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

impl UnicodeString {
    /// # Safety
    ///
    /// `buffer` must hold `length` bytes of UTF-16, or be null.
    unsafe fn text(&self) -> String {
        if self.buffer.is_null() {
            return String::new();
        }
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(self.buffer, usize::from(self.length) / 2)
        })
    }
}

#[repr(C)]
struct SecurityAttribute {
    name: UnicodeString,
    value_type: u16,
    reserved: u16,
    flags: u32,
    value_count: u32,
    values: *mut c_void,
}

#[repr(C)]
struct SecurityAttributes {
    version: u16,
    reserved: u16,
    count: u32,
    attributes: *mut SecurityAttribute,
}

/// The `WIN://NOALLAPPPKG` attribute. The dedicated information class,
/// `TokenIsLessPrivilegedAppContainer`, fails with "invalid information
/// class" on Windows 11 and Server 2022, so the claim is read instead.
fn lpac_claim(token: HANDLE) -> Result<Claim> {
    unsafe {
        let mut bytes = 0;
        let size_status =
            NtQueryInformationToken(token, TokenSecurityAttributes, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(status(
                "NtQueryInformationToken(TokenSecurityAttributes, size)",
                size_status,
            ));
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let read = NtQueryInformationToken(
            token,
            TokenSecurityAttributes,
            data.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        );
        if read != 0 {
            return Err(status(
                "NtQueryInformationToken(TokenSecurityAttributes)",
                read,
            ));
        }
        let header = &*data.as_ptr().cast::<SecurityAttributes>();
        if header.version != 1 {
            return Err(format!(
                "unsupported token security attributes version {}",
                header.version
            ));
        }
        if header.count == 0 {
            return Ok(Claim::Absent);
        }
        let entries = std::slice::from_raw_parts(header.attributes, header.count as usize);
        for entry in entries {
            if entry.name.text() != LPAC_CLAIM {
                continue;
            }
            if entry.value_type != ATTRIBUTE_TYPE_UINT64 {
                return Ok(Claim::OtherType(entry.value_type));
            }
            let values = if entry.value_count == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(entry.values.cast::<u64>(), entry.value_count as usize)
                    .to_vec()
            };
            return Ok(Claim::Unsigned(values));
        }
        Ok(Claim::Absent)
    }
}

/// Sets the token's integrity label to Untrusted, `S-1-16-0`. The handle
/// needs `TOKEN_ADJUST_DEFAULT`.
pub(super) fn set_untrusted(token: HANDLE) -> Result<()> {
    unsafe {
        // SECURITY_MAX_SID_SIZE is 68 bytes.
        let mut sid = [0usize; 68usize.div_ceil(size_of::<usize>())];
        let mut bytes = size_of_val(&sid) as u32;
        if CreateWellKnownSid(
            WinUntrustedLabelSid,
            null_mut(),
            sid.as_mut_ptr().cast(),
            &mut bytes,
        ) == 0
        {
            return Err(last("CreateWellKnownSid(Untrusted)"));
        }
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        let length = size_of::<TOKEN_MANDATORY_LABEL>() + GetLengthSid(label.Label.Sid) as usize;
        if SetTokenInformation(
            token,
            TokenIntegrityLevel,
            (&label as *const TOKEN_MANDATORY_LABEL).cast(),
            length as u32,
        ) == 0
        {
            return Err(last("SetTokenInformation(TokenIntegrityLevel)"));
        }
        Ok(())
    }
}

/// Reads this process's seven mitigation policy words.
pub(super) fn mitigation_words() -> Result<MitigationWords> {
    let read = |policy: PROCESS_MITIGATION_POLICY, name: &str| -> Result<u32> {
        let mut word = 0u32;
        let ok = unsafe {
            GetProcessMitigationPolicy(
                GetCurrentProcess(),
                policy,
                (&mut word as *mut u32).cast(),
                size_of::<u32>(),
            )
        };
        if ok == 0 {
            return Err(last(&format!("GetProcessMitigationPolicy({name})")));
        }
        Ok(word)
    };
    Ok(MitigationWords {
        dynamic_code: read(ProcessDynamicCodePolicy, "dynamic code")?,
        signature: read(ProcessSignaturePolicy, "signature")?,
        image_load: read(ProcessImageLoadPolicy, "image load")?,
        system_call: read(ProcessSystemCallDisablePolicy, "system call disable")?,
        strict_handle: read(ProcessStrictHandleCheckPolicy, "strict handle check")?,
        extension_point: read(ProcessExtensionPointDisablePolicy, "extension point")?,
        child_process: read(ProcessChildProcessPolicy, "child process")?,
    })
}

/// The values of this process's standard input, output and error handles.
pub(super) fn standard_handles() -> [usize; 3] {
    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .map(|which| unsafe { GetStdHandle(which) } as usize)
}

/// One entry of `PROCESS_HANDLE_SNAPSHOT_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SnapshotEntry {
    pub(super) handle: HANDLE,
    handle_count: usize,
    pointer_count: usize,
    pub(super) access: u32,
    pub(super) type_index: u32,
    pub(super) attributes: u32,
    reserved: u32,
}

/// Lists this process's handle table. Only the values, types and flags are
/// read: no snapshot handle is used as a query target, because another
/// thread can close it meanwhile, and strict handle checks make a call on a
/// closed handle end the process.
pub(super) fn handle_snapshot() -> Result<Vec<SnapshotEntry>> {
    let mut bytes = 64 * 1024u32;
    for _ in 0..8 {
        let mut buffer = vec![0usize; bytes as usize / size_of::<usize>()];
        let mut returned = 0;
        let result = unsafe {
            NtQueryInformationProcess(
                GetCurrentProcess(),
                PROCESS_HANDLE_INFORMATION,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut returned,
            )
        };
        if result == STATUS_INFO_LENGTH_MISMATCH {
            // Handles can be created between the two calls; leave room.
            bytes = returned.max(bytes).saturating_mul(2);
            continue;
        }
        if result < 0 {
            return Err(status(
                "NtQueryInformationProcess(ProcessHandleInformation)",
                result,
            ));
        }
        // The header is two pointer-sized fields: the count and a reserved
        // field. The entries follow.
        let count = buffer[0];
        let capacity = (bytes as usize - 2 * size_of::<usize>()) / size_of::<SnapshotEntry>();
        if count > capacity {
            return Err(format!("handle snapshot count {count} exceeds its buffer"));
        }
        let entries = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<SnapshotEntry>(), count)
        };
        return Ok(entries.to_vec());
    }
    Err("the handle snapshot kept growing".into())
}

/// One entry of `OBJECT_TYPES_INFORMATION`, the fixed part of
/// `OBJECT_TYPE_INFORMATION`. The type's name follows it.
#[repr(C)]
struct ObjectTypeInformation {
    name: UnicodeString,
    counters: [u32; 13],
    generic_mapping: [u32; 4],
    valid_access_mask: u32,
    security_required: u8,
    maintain_handle_count: u8,
    type_index: u8,
    reserved: u8,
    pool: [u32; 3],
}

/// Every object type's name by its index, the index handle tables use.
pub(super) fn object_types() -> Result<BTreeMap<u32, String>> {
    let mut bytes = 64 * 1024u32;
    for _ in 0..8 {
        let mut buffer = vec![0usize; bytes as usize / size_of::<usize>()];
        let mut returned = 0;
        let result = unsafe {
            NtQueryObject(
                null_mut(),
                OBJECT_TYPES_INFORMATION,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut returned,
            )
        };
        if result == STATUS_INFO_LENGTH_MISMATCH {
            bytes = returned.max(bytes).saturating_mul(2);
            continue;
        }
        if result < 0 {
            return Err(status("NtQueryObject(ObjectTypesInformation)", result));
        }
        let base = buffer.as_ptr().cast::<u8>();
        let end = (returned as usize).min(bytes as usize);
        let count = unsafe { *base.cast::<u32>() };
        // The first entry is aligned to a pointer after the 4-byte count; each
        // later one follows the previous entry's name, aligned the same way.
        let mut offset = size_of::<usize>();
        let mut types = BTreeMap::new();
        for _ in 0..count {
            if offset + size_of::<ObjectTypeInformation>() > end {
                return Err("truncated ObjectTypesInformation".into());
            }
            let info = unsafe { &*base.add(offset).cast::<ObjectTypeInformation>() };
            types.insert(u32::from(info.type_index), unsafe { info.name.text() });
            offset = (offset
                + size_of::<ObjectTypeInformation>()
                + usize::from(info.name.maximum_length))
            .next_multiple_of(size_of::<usize>());
        }
        return Ok(types);
    }
    Err("the object type list kept growing".into())
}

/// The object name behind a handle, read through a duplicate so that the
/// table's own handle is never a query target.
pub(super) fn object_name_via_duplicate(handle: HANDLE) -> Result<String> {
    unsafe {
        let process = GetCurrentProcess();
        let mut duplicate = null_mut();
        if DuplicateHandle(
            process,
            handle,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(last("DuplicateHandle(directory)"));
        }
        let name = object_name(duplicate);
        close(duplicate, "directory duplicate")?;
        name
    }
}

fn object_name(handle: HANDLE) -> Result<String> {
    let mut bytes = 1024u32;
    for _ in 0..2 {
        let mut buffer = vec![0usize; bytes as usize / size_of::<usize>()];
        let mut returned = 0;
        let result = unsafe {
            NtQueryObject(
                handle,
                OBJECT_NAME_INFORMATION,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut returned,
            )
        };
        if (result == STATUS_INFO_LENGTH_MISMATCH || result == STATUS_BUFFER_OVERFLOW)
            && returned > bytes
        {
            bytes = returned.next_multiple_of(size_of::<usize>() as u32);
            continue;
        }
        if result < 0 {
            return Err(status("NtQueryObject(ObjectNameInformation)", result));
        }
        // The name's characters are in the same buffer, after the header.
        let name = unsafe { &*buffer.as_ptr().cast::<UnicodeString>() };
        let start = name.buffer as usize;
        let base = buffer.as_ptr() as usize;
        if !name.buffer.is_null()
            && (start < base || start + usize::from(name.length) > base + bytes as usize)
        {
            return Err("object name outside its buffer".into());
        }
        return Ok(unsafe { name.text() });
    }
    Err("the object name kept growing".into())
}

/// Which standard handle `value` is.
pub(super) fn stdio_slot(value: usize, standard: &[usize; 3]) -> Option<Stdio> {
    [Stdio::Input, Stdio::Output, Stdio::Error]
        .into_iter()
        .zip(standard)
        .find(|(_, handle)| **handle == value)
        .map(|(slot, _)| slot)
}

/// The handle table as the allowlist judges it.
pub(super) fn handle_table() -> Result<Vec<HandleEntry>> {
    let types = object_types()?;
    let standard = standard_handles();
    let snapshot = handle_snapshot()?;
    let mut table: Vec<HandleEntry> = snapshot
        .iter()
        .map(|entry| HandleEntry {
            value: entry.handle as usize,
            type_name: types
                .get(&entry.type_index)
                .cloned()
                .unwrap_or_else(|| format!("unknown type {}", entry.type_index)),
            access: entry.access,
            inheritable: entry.attributes & OBJ_INHERIT != 0,
            stdio: stdio_slot(entry.handle as usize, &standard),
            name: None,
        })
        .collect();
    // Only a single Directory handle is named. With two or more the table is
    // refused anyway, and nothing is queried.
    let directories: Vec<usize> = (0..table.len())
        .filter(|&index| table[index].type_name == "Directory")
        .collect();
    if let [index] = directories[..] {
        table[index].name = Some(object_name_via_duplicate(snapshot[index].handle)?);
    }
    Ok(table)
}
