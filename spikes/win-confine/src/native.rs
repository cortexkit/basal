use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, System::IO::IO_STATUS_BLOCK,
    System::Threading::*,
};

pub type Result<T> = std::result::Result<T, String>;
pub const SE_GROUP_INTEGRITY: u32 = 32;
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 16;
pub const SE_GROUP_LOGON_ID: u32 = 0xc0000000;
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
unsafe fn default_dacl(token: HANDLE) -> Result<Value> {
    unsafe {
        let data = token_buffer(token, TokenDefaultDacl)?;
        let dacl = (*data.as_ptr().cast::<TOKEN_DEFAULT_DACL>()).DefaultDacl;
        if dacl.is_null() {
            return Ok(json!({"null_dacl":true}));
        }
        // TokenDefaultDacl protects newly created objects. It is distinct from
        // the token object's own DACL, which controls opening that token.
        let mut descriptor: SECURITY_DESCRIPTOR = zeroed();
        check(
            InitializeSecurityDescriptor((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), 1),
            "InitializeSecurityDescriptor(default DACL)",
        )?;
        check(
            SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                dacl,
                0,
            ),
            "SetSecurityDescriptorDacl(default DACL)",
        )?;
        let mut text = null_mut();
        check(
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                null_mut(),
            ),
            "ConvertSecurityDescriptorToStringSecurityDescriptorW(default DACL)",
        )?;
        let rendered = utf16_ptr(text);
        LocalFree(text.cast());
        Ok(json!({"sddl":rendered}))
    }
}

pub unsafe fn set_user_default_dacl(token: HANDLE) -> Result<()> {
    unsafe {
        let user = token_buffer(token, TokenUser)?;
        let sid = sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid);
        let mut descriptor = null_mut();
        check(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(&format!("D:(A;;GA;;;{sid})(A;;GA;;;BA)(A;;GA;;;SY)")).as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ),
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(Chrome default DACL)",
        )?;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = null_mut();
        let result = (|| {
            check(
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted),
                "GetSecurityDescriptorDacl(Chrome default DACL)",
            )?;
            let info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
            check(
                SetTokenInformation(
                    token,
                    TokenDefaultDacl,
                    (&info as *const TOKEN_DEFAULT_DACL).cast(),
                    size_of::<TOKEN_DEFAULT_DACL>() as u32,
                ),
                "SetTokenInformation(Chrome default DACL)",
            )
        })();
        LocalFree(descriptor);
        result
    }
}

pub unsafe fn descriptor_attestation(
    descriptor: *mut c_void,
    token: HANDLE,
    mapping: &mut GENERIC_MAPPING,
) -> Value {
    unsafe {
        let mut text = null_mut();
        let rendered = if ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            null_mut(),
        ) == 0
        {
            json!({"error":last("ConvertSecurityDescriptorToStringSecurityDescriptorW(object)")})
        } else {
            let rendered = utf16_ptr(text);
            LocalFree(text.cast());
            json!(rendered)
        };
        let checks = [1u32, 2, 0x20000].iter().map(|desired| {
            if token.is_null() { return json!({"desired_access":hex(*desired),"error":"primary impersonation duplicate unavailable"}); }
            let mut privileges = [0usize;128];
            let mut bytes = std::mem::size_of_val(&privileges) as u32;
            let mut granted = 0;
            let mut allowed = 0;
            let ok = AccessCheck(descriptor, token, *desired, mapping, privileges.as_mut_ptr().cast(), &mut bytes, &mut granted, &mut allowed) != 0;
            json!({"desired_access":hex(*desired),"query_success":ok,"allowed":ok && allowed != 0,"granted_access":hex(granted),"error":if ok {0} else {GetLastError()}})
        }).collect::<Vec<_>>();
        json!({"dacl_sddl":rendered,"primary_dacl_only_access_checks":checks,"coverage":"DACL evaluation only, not an operation or mandatory-integrity check; access bits have object-specific meanings"})
    }
}

pub unsafe fn directory_attestation(path: &str, token: HANDLE) -> Value {
    unsafe {
        let mut w = wide(path);
        let mut name = us(&mut w);
        let mut attrs = oa(&mut name);
        let mut h = null_mut();
        let status = NtOpenDirectoryObject(&mut h, 0x20000, &mut attrs);
        if status < 0 {
            return json!({"path":path,"open_ntstatus":hex(status as u32)});
        }
        let h = Handle(h);
        let mut bytes = 0;
        NtQuerySecurityObject(h.0, 7, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return json!({"path":path,"error":"no security descriptor size"});
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let status = NtQuerySecurityObject(h.0, 7, buffer.as_mut_ptr().cast(), bytes, &mut bytes);
        if status < 0 {
            return json!({"path":path,"query_ntstatus":hex(status as u32)});
        }
        let mut mapping = GENERIC_MAPPING {
            GenericRead: 0x20001,
            GenericWrite: 0x2000c,
            GenericExecute: 0x20002,
            GenericAll: 0xf000f,
        };
        json!({"path":path,"security":descriptor_attestation(buffer.as_mut_ptr().cast(), token, &mut mapping)})
    }
}

pub unsafe fn token_attestation(token: HANDLE) -> Result<Value> {
    unsafe {
        let group_data = token_buffer(token, TokenGroups)?;
        let user_data = token_buffer(token, TokenUser)?;
        let user = &*user_data.as_ptr().cast::<TOKEN_USER>();
        let token_type = token_buffer(token, TokenType)?;
        let kind = *token_type.as_ptr().cast::<u32>();
        let impersonation_level = if kind == TokenImpersonation as u32 {
            let data = token_buffer(token, TokenImpersonationLevel)?;
            Some(*data.as_ptr().cast::<u32>())
        } else {
            None
        };
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
                    let (effective, attributes) = if status == 0 {
                        (value != 0, Value::Null)
                    } else {
                        lpac_claim(token)?
                    };
                    (
                        effective,
                        json!({"api":if status==0 {"NtQueryInformationToken"}else{"TokenSecurityAttributes / WIN://NOALLAPPPKG"},"class":TokenIsLessPrivilegedAppContainer,"Win32_failure":error,"NTSTATUS":hex(status as u32),"attributes":attributes}),
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
            "token_type":kind,"impersonation_level":impersonation_level,"user_sid":sid_string(user.User.Sid),
            "lpac": less, "lpac_query":lpac_query, "appcontainer":is_app,
            "appcontainer_sid":if app_sid.is_null() {Value::Null} else {json!(sid_string(app_sid))},
            "capabilities":capability_data.as_ref().map(|data|render(groups(data))).unwrap_or_default(), "restricting_sids":render(groups(&restrict_data)),
            "groups":render(groups(&group_data)), "integrity":sid_string(mandatory.Label.Sid),
            "default_dacl":default_dacl(token).unwrap_or_else(|e|json!({"error":e})),
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
#[derive(Clone, Copy)]
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
struct ObjectTypeInfo {
    name: UnicodeString,
    counters: [u32; 13],
    mapping: [u32; 4],
    access: u32,
    security: u8,
    maintain: u8,
    index: u8,
    reserved: u8,
    pool: [u32; 3],
}
fn object_types() -> Result<BTreeMap<u32, String>> {
    unsafe {
        let mut buffer = vec![0usize; 32768];
        let mut returned = 0;
        let status = NtQueryObject(
            null_mut(),
            3,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut returned,
        );
        if status != 0 {
            return Err(format!(
                "NtQueryObject(ObjectTypesInformation): {}",
                hex(status as u32)
            ));
        }
        let count = *buffer.as_ptr().cast::<u32>();
        let mut offset = 8;
        let mut types = BTreeMap::new();
        for _ in 0..count {
            if offset + size_of::<ObjectTypeInfo>() > returned as usize {
                return Err("truncated ObjectTypesInformation".into());
            }
            let info = &*buffer
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<ObjectTypeInfo>();
            types.insert(info.index as u32, us_text(&info.name));
            offset = (offset + size_of::<ObjectTypeInfo>() + info.name.maximum_length as usize)
                .next_multiple_of(8);
        }
        Ok(types)
    }
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
#[repr(C)]
struct SecurityAttribute {
    name: UnicodeString,
    kind: u16,
    reserved: u16,
    flags: u32,
    count: u32,
    values: *mut c_void,
}
#[repr(C)]
struct SecurityAttributes {
    version: u16,
    reserved: u16,
    count: u32,
    attributes: *mut SecurityAttribute,
}

fn lpac_claim(token: HANDLE) -> Result<(bool, Value)> {
    unsafe {
        let mut bytes = 0;
        let status =
            NtQueryInformationToken(token, TokenSecurityAttributes, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(format!(
                "NtQueryInformationToken(TokenSecurityAttributes,size): {}",
                hex(status as u32)
            ));
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let status = NtQueryInformationToken(
            token,
            TokenSecurityAttributes,
            data.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        );
        if status != 0 {
            return Err(format!(
                "NtQueryInformationToken(TokenSecurityAttributes): {}",
                hex(status as u32)
            ));
        }
        let header = &*data.as_ptr().cast::<SecurityAttributes>();
        if header.version != 1 {
            return Err(format!(
                "unsupported token security attributes version {}",
                header.version
            ));
        }
        let entries = if header.count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(header.attributes, header.count as usize)
        };
        let mut less = false;
        let mut attributes = Vec::new();
        for entry in entries {
            let name = us_text(&entry.name);
            let values = if entry.kind == 2 && entry.count != 0 {
                std::slice::from_raw_parts(entry.values.cast::<u64>(), entry.count as usize)
                    .to_vec()
            } else {
                Vec::new()
            };
            if name == "WIN://NOALLAPPPKG" {
                less = entry.kind == 2 && values.first().is_some_and(|v| *v != 0);
            }
            attributes.push(json!({"name":name,"value_type":entry.kind,"flags":hex(entry.flags),"uint64_values":values}));
        }
        Ok((less, json!(attributes)))
    }
}
#[link(name = "ntdll")]
unsafe extern "system" {
    pub fn NtSetInformationProcess(
        process: HANDLE,
        class: u32,
        information: *const c_void,
        length: u32,
    ) -> i32;
    fn NtCreateLowBoxToken(
        out: *mut HANDLE,
        existing: HANDLE,
        access: u32,
        attrs: *mut ObjectAttributes,
        package: PSID,
        capability_count: u32,
        capabilities: *const SID_AND_ATTRIBUTES,
        handle_count: u32,
        handles: *const HANDLE,
    ) -> i32;
    fn NtQueryInformationToken(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQuerySecurityObject(
        handle: HANDLE,
        information: u32,
        descriptor: *mut c_void,
        bytes: u32,
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
    pub fn NtQueryInformationProcess(
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
    fn NtQueryInformationFile(
        handle: HANDLE,
        io: *mut IO_STATUS_BLOCK,
        buffer: *mut c_void,
        size: u32,
        class: u32,
    ) -> i32;
    fn NtDuplicateObject(
        source_process: HANDLE,
        source: HANDLE,
        target_process: HANDLE,
        target: *mut HANDLE,
        access: u32,
        attributes: u32,
        options: u32,
    ) -> i32;
    pub fn NtAlpcQueryInformation(
        port: HANDLE,
        class: u32,
        buffer: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryInformationWorkerFactory(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
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

pub fn handle_table() -> Result<Vec<Value>> {
    handle_table_for(unsafe { GetCurrentProcess() })
}
pub fn handle_table_for(process: HANDLE) -> Result<Vec<Value>> {
    let entries = handle_entries(process)?;
    // A snapshot handle can close on a DLL-owned background thread before
    // its type is queried. The process mitigation that rejects an invalid
    // handle makes that race fatal, so type names come from the global
    // type list and its stable indices.
    let types = object_types()?;
    Ok(entries.iter().map(|h|json!({"handle":h.handle as usize,"type_index":h.type_index,"type":types.get(&h.type_index),"granted_access":hex(h.access),"attributes":hex(h.attributes)})).collect())
}
fn handle_entries(process: HANDLE) -> Result<Vec<HandleEntry>> {
    unsafe {
        let mut buffer = vec![0usize; 16384];
        let mut returned = 0;
        let status = NtQueryInformationProcess(
            process,
            51,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut returned,
        );
        if status < 0 {
            return Err(format!("ProcessHandleInformation: {}", hex(status as u32)));
        }
        let snapshot = &*buffer.as_ptr().cast::<HandleSnapshot>();
        Ok(std::slice::from_raw_parts(snapshot.handles.as_ptr(), snapshot.count).to_vec())
    }
}
#[repr(C)]
struct ObjectNameInformation {
    name: UnicodeString,
}
// ObjectNameInformation is class 1. The name bytes follow the fixed
// header, so a short buffer returns the required length and is retried once.
// A successful empty name is recorded as empty; a negative status is recorded
// unchanged. Neither result is replaced with a guessed object name.
fn object_name(handle: HANDLE) -> Value {
    unsafe {
        let mut bytes = 1024u32;
        let mut buffer = vec![0u8; bytes as usize];
        let mut returned = 0;
        let mut status = NtQueryObject(handle, 1, buffer.as_mut_ptr().cast(), bytes, &mut returned);
        if (status as u32 == 0xc0000004 || status as u32 == 0x80000005) && returned > bytes {
            bytes = returned;
            buffer.resize(bytes as usize, 0);
            status = NtQueryObject(handle, 1, buffer.as_mut_ptr().cast(), bytes, &mut returned);
        }
        if status < 0 {
            return json!({"NTSTATUS": hex(status as u32), "returned": returned});
        }
        if (returned as usize) < size_of::<ObjectNameInformation>() {
            return json!({"NTSTATUS": "0x00000000", "truncated": returned});
        }
        let info = &*buffer.as_ptr().cast::<ObjectNameInformation>();
        let name = if info.name.buffer.is_null() || info.name.length == 0 {
            String::new()
        } else {
            let start = info.name.buffer as usize;
            let end = start + info.name.length as usize;
            let base = buffer.as_ptr() as usize;
            if start < base || end > base + returned as usize {
                return json!({"NTSTATUS": "0x00000000", "name_outside_buffer": true});
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(
                info.name.buffer,
                info.name.length as usize / 2,
            ))
        };
        json!({"NTSTATUS": "0x00000000", "name": name})
    }
}
// FileNameInformation is class 9. Its returned name begins after the
// length DWORD and is the NT path, which can differ from the object-manager
// name. A negative status is retained instead of guessing the file.
fn file_name(handle: HANDLE) -> Value {
    unsafe {
        let mut buffer = vec![0u8; 1024];
        let mut io: IO_STATUS_BLOCK = zeroed();
        let status = NtQueryInformationFile(
            handle,
            &mut io,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            9,
        );
        if status < 0 {
            return json!({"NTSTATUS": hex(status as u32)});
        }
        let length = *buffer.as_ptr().cast::<u32>() as usize;
        if 4 + length > buffer.len() {
            return json!({"NTSTATUS": "0x00000000", "truncated": length});
        }
        json!({
            "NTSTATUS": "0x00000000",
            "name": String::from_utf16_lossy(std::slice::from_raw_parts(
                buffer.as_ptr().add(4).cast::<u16>(),
                length / 2,
            ))
        })
    }
}
#[repr(C)]
#[allow(dead_code)]
struct AlpcBasicInformation {
    flags: u32,
    sequence: u32,
    context: *mut c_void,
}
// Retain raw ALPC query results, including failures. Class 12 adds server
// session/process information on recent Windows. No query sends a message.
fn alpc_identity(handle: HANDLE) -> Value {
    unsafe {
        let mut queries = Vec::new();
        for class in [0, 1, 2, 3, 4, 12] {
            let mut buffer = vec![0u8; 512];
            let mut returned = 0;
            let status = NtAlpcQueryInformation(
                handle,
                class,
                buffer.as_mut_ptr().cast(),
                if class == 12 { 8 } else { buffer.len() as u32 },
                &mut returned,
            );
            let shown = returned.min(buffer.len() as u32) as usize;
            queries.push(json!({
                "class": class,
                "NTSTATUS": hex(status as u32),
                "returned": returned,
                "bytes": if status < 0 { Value::Null } else { json!(hex_bytes(&buffer[..shown])) },
            }));
        }
        json!(queries)
    }
}
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn remote_alpc_name(process: HANDLE, handle: HANDLE) -> Value {
    unsafe {
        let mut duplicate = null_mut();
        let status = NtDuplicateObject(
            process,
            handle,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            2,
        );
        if status < 0 {
            return json!({"duplicate_status":hex(status as u32)});
        }
        let duplicate = Handle(duplicate);
        json!({"object_name":object_name(duplicate.0),"queries":alpc_identity(duplicate.0)})
    }
}
pub fn remote_worker_factory(process: HANDLE, handle: HANDLE) -> Value {
    unsafe {
        let mut duplicate = null_mut();
        let status = NtDuplicateObject(
            process,
            handle,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            2,
        );
        if status < 0 {
            return json!({"duplicate_status":hex(status as u32)});
        }
        let duplicate = Handle(duplicate);
        let mut basic = [0usize; 15];
        let mut returned = 0;
        let status = NtQueryInformationWorkerFactory(
            duplicate.0,
            7,
            basic.as_mut_ptr().cast(),
            120,
            &mut returned,
        );
        if status < 0 {
            return json!({"NTSTATUS":hex(status as u32),"returned":returned});
        }
        json!({"NTSTATUS":hex(status as u32),"process_id":basic[11],"start_routine":format!("0x{:016x}",basic[9]),"start_parameter":format!("0x{:016x}",basic[10])})
    }
}
// Identify one stable handle without using the process snapshot as a query
// target. The duplicate is closed here; the caller's handle is not.
fn identify_handle(process: HANDLE, entry: &HandleEntry, kind: Option<&str>) -> Value {
    unsafe {
        let mut duplicate = null_mut();
        let status = NtDuplicateObject(
            process,
            entry.handle,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            2, // DUPLICATE_SAME_ACCESS
        );
        if status < 0 {
            return json!({
                "handle": entry.handle as usize,
                "type": kind,
                "granted_access": hex(entry.access),
                "duplicate": hex(status as u32),
            });
        }
        let duplicate = Handle(duplicate);
        let mut identity = json!({
            "handle": entry.handle as usize,
            "type": kind,
            "type_index": entry.type_index,
            "granted_access": hex(entry.access),
            "attributes": hex(entry.attributes),
            "object_name": object_name(duplicate.0),
        });
        if kind == Some("File") {
            identity["file_name"] = file_name(duplicate.0);
        }
        if kind == Some("ALPC Port") {
            identity["alpc"] = alpc_identity(duplicate.0);
        }
        identity
    }
}
// Name every handle, then close the requested ambient types and prove the
// close against a second snapshot. Stdio pipes are never candidates. A close
// that fails is recorded and the handle stays in the surviving inventory.
pub fn identify_and_close(keep_stdio: &[HANDLE], close_types: &[&str]) -> Result<Value> {
    unsafe {
        let process = GetCurrentProcess();
        let types = object_types()?;
        let before = handle_entries(process)?;
        let identified: Vec<Value> = before
            .iter()
            .map(|entry| {
                identify_handle(
                    process,
                    entry,
                    types.get(&entry.type_index).map(String::as_str),
                )
            })
            .collect();
        let mut closed = Vec::new();
        for entry in &before {
            if keep_stdio.contains(&entry.handle) {
                continue;
            }
            let kind = types.get(&entry.type_index).map(String::as_str);
            if !kind.is_some_and(|kind| close_types.contains(&kind)) {
                continue;
            }
            let ok = CloseHandle(entry.handle) != 0;
            let error = if ok { 0 } else { GetLastError() };
            eprintln!(
                "ambient-close: handle={} type={kind:?} success={ok}",
                entry.handle as usize
            );
            closed.push(json!({
                "handle": entry.handle as usize,
                "type": kind,
                "granted_access": hex(entry.access),
                "success": ok,
                "error": error,
            }));
        }
        let after = handle_table()?;
        let closed_values: Vec<u64> = closed
            .iter()
            .filter(|entry| entry["success"] == true)
            .map(|entry| entry["handle"].as_u64().unwrap())
            .collect();
        let surviving: Vec<Value> = after
            .iter()
            .filter(|entry| closed_values.contains(&entry["handle"].as_u64().unwrap_or(0)))
            .cloned()
            .collect();
        Ok(json!({
            "before": identified,
            "closed": closed,
            "after": after,
            "closed_handles_still_present": surviving,
        }))
    }
}
pub fn granted_access(handle: HANDLE) -> Value {
    unsafe {
        // OBJECT_BASIC_INFORMATION is 56 bytes on 64-bit Windows; its second
        // DWORD is GrantedAccess, independent of the operation's requested mask.
        let mut data = [0usize; 7];
        let mut returned = 0;
        let status = NtQueryObject(handle, 0, data.as_mut_ptr().cast(), 56, &mut returned);
        if status != 0 {
            json!({"NTSTATUS":hex(status as u32)})
        } else {
            json!(hex(*data.as_ptr().cast::<u32>().add(1)))
        }
    }
}
pub fn token_handles_absent(handles: &[Value]) -> bool {
    handles
        .iter()
        .all(|h| matches!(h["type"].as_str(),Some(kind) if kind!="Token"))
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
        let mut probe = Probe::nt(t, status);
        if status == 0 {
            probe.result["granted_access"] = granted_access(handle);
            drop(Handle(handle));
        }
        probe
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
pub fn create_lowbox_token(source: HANDLE, package: PSID) -> Result<Handle> {
    unsafe {
        let mut attrs = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: null_mut(),
            name: null_mut(),
            attributes: 0,
            descriptor: null_mut(),
            qos: null_mut(),
        };
        let mut token = null_mut();
        let status = NtCreateLowBoxToken(
            &mut token,
            source,
            TOKEN_ALL_ACCESS,
            &mut attrs,
            package,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        );
        if status != 0 {
            Err(format!(
                "NtCreateLowBoxToken(initial, zero capabilities): {}",
                hex(status as u32)
            ))
        } else {
            Ok(Handle(token))
        }
    }
}
pub fn set_integrity(token: HANDLE, level: WELL_KNOWN_SID_TYPE) -> Result<()> {
    unsafe {
        let mut sid = [0u32; 17];
        let mut bytes = (sid.len() * 4) as u32;
        check(
            CreateWellKnownSid(
                level,
                std::ptr::null_mut(),
                sid.as_mut_ptr().cast(),
                &mut bytes,
            ),
            "CreateWellKnownSid(integrity)",
        )?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        check(
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                (size_of::<TOKEN_MANDATORY_LABEL>() + GetLengthSid(label.Label.Sid) as usize)
                    as u32,
            ),
            "SetTokenInformation(TokenIntegrityLevel)",
        )
    }
}
pub fn replace_primary_locked(source: HANDLE) -> Value {
    unsafe {
        let result = (|| -> Result<Value> {
            let data = token_buffer(source, TokenGroups)?;
            let disabled = groups(&data)
                .iter()
                .filter(|g| g.Attributes & SE_GROUP_INTEGRITY == 0)
                .map(|g| SID_AND_ATTRIBUTES {
                    Sid: g.Sid,
                    Attributes: 0,
                })
                .collect::<Vec<_>>();
            let mut sid = [0u32; 17];
            let mut bytes = (sid.len() * 4) as u32;
            check(
                CreateWellKnownSid(
                    WinNullSid,
                    std::ptr::null_mut(),
                    sid.as_mut_ptr().cast(),
                    &mut bytes,
                ),
                "CreateWellKnownSid(post-load NULL)",
            )?;
            let restricting = [SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: 0,
            }];
            let mut token = null_mut();
            check(
                CreateRestrictedToken(
                    source,
                    DISABLE_MAX_PRIVILEGE,
                    disabled.len() as u32,
                    disabled.as_ptr(),
                    0,
                    std::ptr::null(),
                    1,
                    restricting.as_ptr(),
                    &mut token,
                ),
                "CreateRestrictedToken(post-load lockdown)",
            )?;
            let token = Handle(token);
            set_integrity(token.0, WinUntrustedLabelSid)?;
            #[repr(C)]
            struct ProcessAccessToken {
                token: HANDLE,
                thread: HANDLE,
            }
            let access = ProcessAccessToken {
                token: token.0,
                thread: null_mut(),
            };
            let status = NtSetInformationProcess(
                GetCurrentProcess(),
                9,
                (&access as *const ProcessAccessToken).cast(),
                size_of::<ProcessAccessToken>() as u32,
            );
            Ok(
                json!({"api":"NtSetInformationProcess(ProcessAccessToken)","NTSTATUS":hex(status as u32),"success":status==0,"candidate":token_attestation(token.0)?}),
            )
        })();
        result.unwrap_or_else(|e| json!({"success":false,"error":e}))
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
    fn token_handle_inventory_rejects_token_and_unknown_types() {
        assert!(token_handles_absent(&[json!({"type":"File"})]));
        assert!(!token_handles_absent(&[json!({"type":"Token"})]));
        assert!(!token_handles_absent(&[
            json!({"type":{"error":"0xc0000008"}})
        ]));
    }
    #[test]
    fn live_token_handle_is_visible_to_handle_inventory() {
        unsafe {
            let mut token = null_mut();
            check(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                "OpenProcessToken(live inventory test)",
            )
            .unwrap();
            let token = Handle(token);
            let snapshot = handle_table().unwrap();
            assert!(
                snapshot
                    .iter()
                    .any(|h| h["handle"] == token.0 as usize && h["type"] == "Token")
            );
            assert!(!token_handles_absent(&snapshot));
        }
    }
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
        assert_eq!(size_of::<ObjectTypeInfo>(), 104);
        assert_eq!(std::mem::offset_of!(ObjectTypeInfo, index), 90);
        assert_eq!(size_of::<SecurityAttribute>(), 40);
        assert_eq!(size_of::<SecurityAttributes>(), 16);
        assert_eq!(size_of::<ObjectNameInformation>(), 16);
        assert_eq!(size_of::<AlpcBasicInformation>(), 16);
        assert_eq!(size_of::<IO_STATUS_BLOCK>(), 16);
    }
    #[test]
    fn named_object_query_returns_its_path_and_closes_only_the_duplicate() {
        unsafe {
            let mut name = wide("\\BaseNamedObjects");
            let mut unicode = us(&mut name);
            let mut attrs = oa(&mut unicode);
            let mut directory = null_mut();
            let status = NtOpenDirectoryObject(&mut directory, 1, &mut attrs);
            assert_eq!(status, 0, "{}", hex(status as u32));
            let directory = Handle(directory);
            let before = handle_table().unwrap();
            let entry = HandleEntry {
                handle: directory.0,
                count: 0,
                pointers: 0,
                access: 1,
                type_index: 0,
                attributes: 0,
                reserved: 0,
            };
            let identity = identify_handle(GetCurrentProcess(), &entry, Some("Directory"));
            assert_eq!(identity["object_name"]["NTSTATUS"], "0x00000000");
            assert_eq!(identity["object_name"]["name"], "\\BaseNamedObjects");
            let after = handle_table().unwrap();
            assert!(after.iter().any(|h| h["handle"] == directory.0 as usize));
            assert_eq!(
                after.iter().filter(|h| h["type"] == "Directory").count(),
                before.iter().filter(|h| h["type"] == "Directory").count()
            );
        }
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
