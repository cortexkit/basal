use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::null_mut,
};
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, System::Threading::*,
};

pub type Result<T> = std::result::Result<T, String>;
pub const SE_GROUP_INTEGRITY: u32 = 32;
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 16;
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn last(api: &str) -> String {
    unsafe {
        format!(
            "{api}: Win32 {} ({})",
            GetLastError(),
            std::io::Error::last_os_error()
        )
    }
}
pub fn check(ok: i32, api: &str) -> Result<()> {
    if ok == 0 { Err(last(api)) } else { Ok(()) }
}
pub fn hex(v: u32) -> String {
    format!("0x{v:08x}")
}

pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
impl Handle {
    pub fn new(h: HANDLE, api: &str) -> Result<Self> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            Err(last(api))
        } else {
            Ok(Self(h))
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Target {
    pub kind: String,
    pub name: String,
    pub access: String,
}
impl Target {
    pub fn new(kind: &str, name: impl Into<String>, access: &str) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            access: access.into(),
        }
    }
}
#[derive(Serialize, Deserialize)]
pub struct Input {
    pub mode: String,
    pub parent_pid: u32,
    pub targets: Vec<Target>,
    pub temp: String,
    pub package: String,
    pub binary_dir: String,
    pub package_registry: String,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub leaked_handle: usize,
    pub leaked_name: String,
}
#[derive(Serialize, Deserialize)]
pub struct Probe {
    pub kind: String,
    pub target: String,
    pub access: String,
    pub success: bool,
    pub result: Value,
}
impl Probe {
    pub fn win(t: &Target, ok: bool, error: u32) -> Self {
        Self {
            kind: t.kind.clone(),
            target: t.name.clone(),
            access: t.access.clone(),
            success: ok,
            result: json!({"domain":"Win32", "code":if ok {0} else {error}}),
        }
    }
    pub fn nt(t: &Target, status: i32) -> Self {
        Self {
            kind: t.kind.clone(),
            target: t.name.clone(),
            access: t.access.clone(),
            success: status == 0,
            result: json!({"domain":"NTSTATUS", "code":hex(status as u32)}),
        }
    }
}

pub unsafe fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    unsafe {
        let mut bytes = 0;
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last(&format!("GetTokenInformation(class={class}, size)")));
        }
        // Zero-entry variable-length structures can be shorter than their C
        // one-element declarations. Keep enough backing storage for a Rust
        // reference to that declaration even when only the count is read.
        let allocation = (bytes as usize)
            .max(size_of::<TOKEN_GROUPS>())
            .max(size_of::<TOKEN_PRIVILEGES>());
        let mut data = vec![0usize; allocation.div_ceil(size_of::<usize>())];
        check(
            GetTokenInformation(token, class, data.as_mut_ptr().cast(), bytes, &mut bytes),
            &format!("GetTokenInformation(class={class})"),
        )?;
        Ok(data)
    }
}
pub unsafe fn sid_string(sid: PSID) -> String {
    unsafe {
        let mut text = null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return last("ConvertSidToStringSidW");
        }
        let result = utf16_ptr(text);
        LocalFree(text.cast());
        result
    }
}
pub unsafe fn utf16_ptr(text: *const u16) -> String {
    unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}
pub unsafe fn groups(data: &[usize]) -> &[SID_AND_ATTRIBUTES] {
    unsafe {
        let g = &*data.as_ptr().cast::<TOKEN_GROUPS>();
        std::slice::from_raw_parts(g.Groups.as_ptr(), g.GroupCount as usize)
    }
}
pub unsafe fn token_attestation(token: HANDLE) -> Result<Value> {
    unsafe {
        let group_data = token_buffer(token, TokenGroups)?;
        let user_data = token_buffer(token, TokenUser)?;
        let user = &*user_data.as_ptr().cast::<TOKEN_USER>();
        let token_type = token_buffer(token, TokenType)?;
        let restrict_data = token_buffer(token, TokenRestrictedSids)?;
        let is_app = token_buffer(token, TokenIsAppContainer)?;
        let is_app = *is_app.as_ptr().cast::<u32>() != 0;
        let capability_data = if is_app {
            Some(token_buffer(token, TokenCapabilities)?)
        } else {
            None
        };
        let app = if is_app {
            Some(token_buffer(token, TokenAppContainerSid)?)
        } else {
            None
        };
        let app_sid = app
            .as_ref()
            .map(|data| (*data.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer)
            .unwrap_or(null_mut());
        let integrity = token_buffer(token, TokenIntegrityLevel)?;
        let mandatory = &*integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let privs = token_buffer(token, TokenPrivileges)?;
        let p = &*privs.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let entries = std::slice::from_raw_parts(p.Privileges.as_ptr(), p.PrivilegeCount as usize);
        let privilege_name = |luid: &LUID| {
            let mut text = [0u16; 256];
            let mut length = text.len() as u32;
            if LookupPrivilegeNameW(std::ptr::null(), luid, text.as_mut_ptr(), &mut length) != 0 {
                String::from_utf16_lossy(&text[..length as usize])
            } else {
                last("LookupPrivilegeNameW")
            }
        };
        let (less, lpac_query) = if is_app {
            match token_buffer(token, TokenIsLessPrivilegedAppContainer) {
                Ok(data) => (
                    *data.as_ptr().cast::<u32>() != 0,
                    json!({"api":"GetTokenInformation","class":TokenIsLessPrivilegedAppContainer}),
                ),
                Err(error) => {
                    let mut value = 0u32;
                    let mut returned = 0;
                    let status = NtQueryInformationToken(
                        token,
                        TokenIsLessPrivilegedAppContainer,
                        (&mut value as *mut u32).cast(),
                        4,
                        &mut returned,
                    );
                    if status != 0 {
                        return Err(format!(
                            "{error}; NtQueryInformationToken(class={}): {}",
                            TokenIsLessPrivilegedAppContainer,
                            hex(status as u32)
                        ));
                    }
                    (
                        value != 0,
                        json!({"api":"NtQueryInformationToken","class":TokenIsLessPrivilegedAppContainer,"Win32_failure":error,"NTSTATUS":hex(status as u32)}),
                    )
                }
            }
        } else {
            (false, json!({"not_queried":"not an AppContainer"}))
        };
        let render = |list: &[SID_AND_ATTRIBUTES]| {
            list.iter().map(|s| json!({"sid":sid_string(s.Sid),"attributes":hex(s.Attributes),"deny_only":s.Attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0})).collect::<Vec<_>>()
        };
        Ok(json!({
            "token_type":*token_type.as_ptr().cast::<u32>(),"user_sid":sid_string(user.User.Sid),
            "lpac": less, "lpac_query":lpac_query, "appcontainer":is_app,
            "appcontainer_sid":if app_sid.is_null() {Value::Null} else {json!(sid_string(app_sid))},
            "capabilities":capability_data.as_ref().map(|data|render(groups(data))).unwrap_or_default(), "restricting_sids":render(groups(&restrict_data)),
            "groups":render(groups(&group_data)), "integrity":sid_string(mandatory.Label.Sid),
            "privileges":entries.iter().map(|p|json!({"name":privilege_name(&p.Luid),"luid_low":p.Luid.LowPart,"luid_high":p.Luid.HighPart,"attributes":hex(p.Attributes)})).collect::<Vec<_>>()
        }))
    }
}

// These NT APIs expose namespaces and handle metadata that Win32 cannot enumerate.
// The structures use the public PHNT layout for 64-bit Windows, the CI target.
#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}
#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root: HANDLE,
    pub name: *mut UnicodeString,
    pub attributes: u32,
    pub descriptor: *mut c_void,
    pub qos: *mut c_void,
}
#[repr(C)]
struct DirectoryEntry {
    name: UnicodeString,
    kind: UnicodeString,
}
#[repr(C)]
struct HandleEntry {
    handle: HANDLE,
    count: usize,
    pointers: usize,
    access: u32,
    type_index: u32,
    attributes: u32,
    reserved: u32,
}
#[repr(C)]
struct HandleSnapshot {
    count: usize,
    reserved: usize,
    handles: [HandleEntry; 1],
}
#[repr(C)]
struct AlpcAttributes {
    flags: u32,
    qos: SECURITY_QUALITY_OF_SERVICE,
    max_message: usize,
    bandwidth: usize,
    max_pool: usize,
    max_section: usize,
    max_view: usize,
    max_total: usize,
    dup_types: u32,
    reserved: u32,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationToken(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtOpenDirectoryObject(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtQueryDirectoryObject(
        handle: HANDLE,
        buffer: *mut c_void,
        size: u32,
        single: u8,
        restart: u8,
        context: *mut u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtOpenSection(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenEvent(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenMutant(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenSemaphore(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenTimer(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenSymbolicLinkObject(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes)
    -> i32;
    fn NtOpenKeyedEvent(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenKey(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenJobObject(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtSetValueKey(
        key: HANDLE,
        name: *mut UnicodeString,
        title_index: u32,
        value_type: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
    fn NtCreateSection(
        out: *mut HANDLE,
        access: u32,
        attrs: *mut ObjectAttributes,
        size: *mut i64,
        protect: u32,
        allocation: u32,
        file: HANDLE,
    ) -> i32;
    fn NtCreateEvent(
        out: *mut HANDLE,
        access: u32,
        attrs: *mut ObjectAttributes,
        kind: u32,
        state: u8,
    ) -> i32;
    fn NtAlpcConnectPort(
        out: *mut HANDLE,
        name: *mut UnicodeString,
        attrs: *mut ObjectAttributes,
        port_attrs: *mut AlpcAttributes,
        flags: u32,
        sid: PSID,
        message: *mut c_void,
        message_size: *mut usize,
        out_message: *mut c_void,
        in_message: *mut c_void,
        timeout: *mut i64,
    ) -> i32;
    pub fn NtQueryKey(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
}
fn us(text: &mut [u16]) -> UnicodeString {
    UnicodeString {
        length: ((text.len() - 1) * 2) as u16,
        maximum_length: (text.len() * 2) as u16,
        buffer: text.as_mut_ptr(),
    }
}
fn oa(name: &mut UnicodeString) -> ObjectAttributes {
    ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: null_mut(),
        name,
        attributes: 0x40,
        descriptor: null_mut(),
        qos: null_mut(),
    }
}
unsafe fn us_text(s: &UnicodeString) -> String {
    unsafe {
        if s.buffer.is_null() {
            String::new()
        } else {
            String::from_utf16_lossy(std::slice::from_raw_parts(s.buffer, s.length as usize / 2))
        }
    }
}

pub fn enumerate_directory(path: &str) -> Result<Vec<(String, String)>> {
    unsafe {
        let mut w = wide(path);
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        let mut handle = null_mut();
        let status = NtOpenDirectoryObject(&mut handle, 1, &mut attrs);
        if status < 0 {
            return Err(format!(
                "NtOpenDirectoryObject({path}): {}",
                hex(status as u32)
            ));
        }
        let handle = Handle(handle);
        let mut result = Vec::new();
        let mut context = 0;
        loop {
            let mut buffer = vec![0usize; 8192];
            let mut returned = 0;
            let status = NtQueryDirectoryObject(
                handle.0,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * size_of::<usize>()) as u32,
                1,
                u8::from(context == 0),
                &mut context,
                &mut returned,
            );
            if status as u32 == 0x8000001a {
                break;
            } // STATUS_NO_MORE_ENTRIES
            if status < 0 {
                return Err(format!(
                    "NtQueryDirectoryObject({path}): {}",
                    hex(status as u32)
                ));
            }
            let entry = &*buffer.as_ptr().cast::<DirectoryEntry>();
            result.push((
                format!("{path}\\{}", us_text(&entry.name)),
                us_text(&entry.kind),
            ));
        }
        result.sort();
        Ok(result)
    }
}
unsafe fn object_string(handle: HANDLE, class: u32) -> Value {
    unsafe {
        let mut buffer = vec![0usize; 8192];
        let mut returned = 0;
        let status = NtQueryObject(
            handle,
            class,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut returned,
        );
        if status < 0 {
            json!({"error":hex(status as u32)})
        } else {
            json!(us_text(&*buffer.as_ptr().cast::<UnicodeString>()))
        }
    }
}
pub fn handle_table() -> Result<Vec<Value>> {
    unsafe {
        let mut buffer = vec![0usize; 16384];
        let mut returned = 0;
        let status = NtQueryInformationProcess(
            GetCurrentProcess(),
            51,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut returned,
        );
        if status < 0 {
            return Err(format!("ProcessHandleInformation: {}", hex(status as u32)));
        }
        let snapshot = &*buffer.as_ptr().cast::<HandleSnapshot>();
        let entries = std::slice::from_raw_parts(snapshot.handles.as_ptr(), snapshot.count);
        Ok(entries.iter().map(|h|{
            let kind=object_string(h.handle,2);
            json!({"handle":h.handle as usize,"type":kind,"name":object_string(h.handle,1),"file_type":if kind=="File" {Some(GetFileType(h.handle))}else{None},"granted_access":hex(h.access),"attributes":hex(h.attributes)})
        }).collect())
    }
}
pub fn object_probe(t: &Target) -> Probe {
    unsafe {
        let mut w = wide(&t.name);
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        let mut handle = null_mut();
        let status = match t.kind.as_str() {
            "section" => NtOpenSection(&mut handle, 1, &mut attrs), // SECTION_QUERY
            "section_read" => NtOpenSection(&mut handle, 4, &mut attrs), // SECTION_MAP_READ
            "section_write" => NtOpenSection(&mut handle, 2, &mut attrs), // SECTION_MAP_WRITE
            "event" => NtOpenEvent(&mut handle, 1, &mut attrs),     // EVENT_QUERY_STATE
            "event_modify" => NtOpenEvent(&mut handle, 2, &mut attrs),
            "registry_native" => NtOpenKey(&mut handle, 1, &mut attrs), // KEY_QUERY_VALUE
            "job" => NtOpenJobObject(&mut handle, 4, &mut attrs),       // JOB_OBJECT_QUERY
            "mutant" => NtOpenMutant(&mut handle, 1, &mut attrs),
            "semaphore" => NtOpenSemaphore(&mut handle, 1, &mut attrs),
            "timer" => NtOpenTimer(&mut handle, 1, &mut attrs),
            "symbolic_link" => NtOpenSymbolicLinkObject(&mut handle, 1, &mut attrs),
            "KeyedEvent" => NtOpenKeyedEvent(&mut handle, 1, &mut attrs),
            "alpc" => {
                let mut port_attrs: AlpcAttributes = zeroed();
                port_attrs.max_message = 256;
                port_attrs.qos = SECURITY_QUALITY_OF_SERVICE {
                    Length: size_of::<SECURITY_QUALITY_OF_SERVICE>() as u32,
                    ImpersonationLevel: SecurityIdentification,
                    ContextTrackingMode: 0,
                    EffectiveOnly: true,
                };
                let mut timeout: i64 = -1_000_000;
                NtAlpcConnectPort(
                    &mut handle,
                    &mut name,
                    null_mut(),
                    &mut port_attrs,
                    0,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut timeout,
                )
            }
            "object_directory" => NtOpenDirectoryObject(&mut handle, 1, &mut attrs),
            _ => {
                return Probe {
                    kind: t.kind.clone(),
                    target: t.name.clone(),
                    access: t.access.clone(),
                    success: false,
                    result: json!({"not_attempted":"unsupported object type"}),
                };
            }
        };
        if status >= 0 {
            drop(Handle(handle));
        }
        Probe::nt(t, status)
    }
}
pub fn registry_name(h: HANDLE) -> Value {
    unsafe {
        let mut buffer = vec![0u32; 8192];
        let mut returned = 0;
        let status = NtQueryKey(
            h,
            3,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 4) as u32,
            &mut returned,
        );
        if status < 0 {
            json!({"error":hex(status as u32)})
        } else {
            json!(String::from_utf16_lossy(std::slice::from_raw_parts(
                buffer.as_ptr().add(1).cast::<u16>(),
                buffer[0] as usize / 2
            )))
        }
    }
}
pub fn registry_write_native(path: &str) -> Probe {
    unsafe {
        let target = Target::new(
            "registry_write_native",
            path,
            "KEY_SET_VALUE / NtSetValueKey(REG_BINARY)",
        );
        let mut w = wide(path);
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        let mut key = null_mut();
        let status = NtOpenKey(&mut key, 2, &mut attrs);
        if status != 0 {
            return Probe::nt(&target, status);
        }
        let key = Handle(key);
        let mut w = wide("win-confine-native-fixture");
        let mut name = us(&mut w);
        Probe::nt(
            &target,
            NtSetValueKey(key.0, &mut name, 0, 3, [1u8].as_ptr().cast(), 1),
        )
    }
}

pub fn package_fixtures(directory: &str) -> Result<(Handle, Handle, Handle)> {
    unsafe {
        let mut w = wide(directory);
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        let mut directory_handle = null_mut();
        let status = NtOpenDirectoryObject(&mut directory_handle, 1, &mut attrs);
        if status != 0 {
            return Err(format!(
                "NtOpenDirectoryObject(package fixture): {}",
                hex(status as u32)
            ));
        }
        let directory_handle = Handle(directory_handle);
        let mut sd: SECURITY_DESCRIPTOR = zeroed();
        check(
            InitializeSecurityDescriptor((&mut sd as *mut SECURITY_DESCRIPTOR).cast(), 1),
            "InitializeSecurityDescriptor(package fixture)",
        )?;
        check(
            SetSecurityDescriptorDacl(
                (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                std::ptr::null(),
                0,
            ),
            "SetSecurityDescriptorDacl(package fixture)",
        )?;
        let mut w = wide(&format!("{directory}\\basal.spike.package.section"));
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        attrs.descriptor = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
        let mut section = null_mut();
        let mut size = 4096;
        let status = NtCreateSection(
            &mut section,
            0xf001f,
            &mut attrs,
            &mut size,
            4,
            0x8000000,
            null_mut(),
        );
        if status != 0 {
            return Err(format!(
                "NtCreateSection(package fixture): {}",
                hex(status as u32)
            ));
        }
        let section = Handle(section);
        let mut w = wide(&format!("{directory}\\basal.spike.package.event"));
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        attrs.descriptor = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
        let mut event = null_mut();
        let status = NtCreateEvent(&mut event, 0x1f0003, &mut attrs, 0, 0);
        if status != 0 {
            return Err(format!(
                "NtCreateEvent(package fixture): {}",
                hex(status as u32)
            ));
        }
        Ok((directory_handle, section, Handle(event)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wide_string_has_one_trailing_terminator() {
        assert_eq!(wide("a😀"), vec![97, 0xd83d, 0xde00, 0]);
    }
    #[test]
    fn timeout_is_not_an_alpc_connection() {
        assert!(!Probe::nt(&Target::new("alpc", "port", "connect"), 0x102).success);
        assert!(Probe::nt(&Target::new("alpc", "port", "connect"), 0).success);
    }
    #[test]
    fn native_64_bit_layout_matches_nt_headers() {
        assert_eq!(size_of::<UnicodeString>(), 16);
        assert_eq!(size_of::<ObjectAttributes>(), 48);
        assert_eq!(size_of::<HandleEntry>(), 40);
        assert_eq!(std::mem::offset_of!(HandleSnapshot, handles), 16);
        assert_eq!(size_of::<AlpcAttributes>(), 72);
    }
    #[test]
    fn token_buffer_can_back_a_zero_entry_c_declaration() {
        unsafe {
            let mut token = null_mut();
            check(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                "OpenProcessToken(test)",
            )
            .unwrap();
            let token = Handle(token);
            let data = token_buffer(token.0, TokenType).unwrap();
            assert_eq!(*data.as_ptr().cast::<u32>(), 1);
            assert!(data.len() * size_of::<usize>() >= size_of::<TOKEN_GROUPS>());
        }
    }
    #[test]
    fn normal_token_attestation_records_absent_package_identity() {
        unsafe {
            let mut token = null_mut();
            check(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                "OpenProcessToken(test)",
            )
            .unwrap();
            let token = Handle(token);
            let report = token_attestation(token.0).unwrap();
            assert_eq!(report["appcontainer"], false);
            assert_eq!(report["lpac"], false);
            assert_eq!(report["appcontainer_sid"], Value::Null);
            assert_eq!(report["capabilities"], json!([]));
        }
    }
}
