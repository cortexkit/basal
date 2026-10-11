//! The worker's tokens: how they are built, and how the parent reads a
//! token back and compares it with what it built.

use super::deviation::Deviation;
use super::native::{
    Result, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID, SE_GROUP_USE_FOR_DENY_ONLY, SidBuf, check,
    groups, last, owned, sid_string, token_buffer,
};
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_NOT_ALL_ASSIGNED, GetHandleInformation, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    SetHandleInformation,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    DuplicateTokenEx, EqualSid, GetKernelObjectSecurity, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_REMOVED,
    SID_AND_ATTRIBUTES, SecurityImpersonation, SetKernelObjectSecurity, SetTokenInformation,
    TOKEN_ALL_ACCESS, TOKEN_APPCONTAINER_INFORMATION, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_USER, TokenAppContainerSid, TokenCapabilities,
    TokenGroups, TokenImpersonation, TokenImpersonationLevel, TokenIntegrityLevel,
    TokenIsAppContainer, TokenPrivileges, TokenRestrictedSids, TokenSecurityAttributes, TokenType,
    TokenUser, WinLowLabelSid, WinNullSid, WinWorldSid,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The Low integrity label, `S-1-16-4096`. Process creation turns the
/// primary of an AppContainer process Low whatever the supplied token says,
/// and an Untrusted-at-birth worker fails before entry, so the worker is born
/// Low and lowers itself to Untrusted before reading input.
pub const LOW_INTEGRITY: &str = "S-1-16-4096";

/// The NULL SID, `S-1-0-0`: the worker primary's only restricting SID. No
/// object grants it anything unless a grant names it explicitly, so every
/// access check the restricted half of the token must also pass fails.
pub const NULL_SID: &str = "S-1-0-0";

/// The claim that marks a Less Privileged AppContainer token.
const LPAC_CLAIM: &str = "WIN://NOALLAPPPKG";

/// `TOKEN_TYPE::TokenPrimary`.
pub const TOKEN_PRIMARY: i32 = 1;
/// `TOKEN_TYPE::TokenImpersonation`.
pub const TOKEN_IMPERSONATION: i32 = 2;
/// `SECURITY_IMPERSONATION_LEVEL::SecurityImpersonation`.
pub const IMPERSONATION_LEVEL: i32 = 2;

/// What the parent reads back from a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenFacts {
    /// `TOKEN_TYPE`: 1 primary, 2 impersonation.
    pub token_type: i32,
    /// The impersonation level of an impersonation token.
    pub impersonation_level: Option<i32>,
    /// The integrity label SID.
    pub integrity: String,
    /// The AppContainer (package) SID, if the token is an AppContainer token.
    pub appcontainer: Option<String>,
    /// The capability SIDs.
    pub capabilities: Vec<String>,
    /// Whether the token carries the Less Privileged AppContainer claim,
    /// `WIN://NOALLAPPPKG` as the single unsigned value 1.
    pub less_privileged: bool,
    /// The restricting SIDs.
    pub restricting_sids: Vec<String>,
    /// Access groups (every group but the integrity label) that are not
    /// deny-only.
    pub enabled_groups: Vec<String>,
    /// How many access groups are deny-only.
    pub deny_only_groups: usize,
    /// How many privileges the token holds, enabled or not.
    pub privileges: usize,
}

/// Reads the facts of a token opened with `TOKEN_QUERY`.
pub(crate) fn read_facts(token: HANDLE) -> Result<TokenFacts> {
    unsafe {
        let token_type = token_buffer(token, TokenType)?;
        let token_type = *token_type.as_ptr().cast::<i32>();
        let impersonation_level = if token_type == TOKEN_IMPERSONATION {
            let level = token_buffer(token, TokenImpersonationLevel)?;
            Some(*level.as_ptr().cast::<i32>())
        } else {
            None
        };
        let label = token_buffer(token, TokenIntegrityLevel)?;
        let integrity = sid_string((*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid)?;
        let is_app = token_buffer(token, TokenIsAppContainer)?;
        let is_app = *is_app.as_ptr().cast::<u32>() != 0;
        let (appcontainer, capabilities) = if is_app {
            let info = token_buffer(token, TokenAppContainerSid)?;
            let sid = (*info.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer;
            let capabilities = token_buffer(token, TokenCapabilities)?;
            (Some(sid_string(sid)?), sid_list(groups(&capabilities))?)
        } else {
            (None, Vec::new())
        };
        let restricting = token_buffer(token, TokenRestrictedSids)?;
        let group_data = token_buffer(token, TokenGroups)?;
        let mut enabled_groups = Vec::new();
        let mut deny_only_groups = 0;
        for group in groups(&group_data) {
            if group.Attributes & SE_GROUP_INTEGRITY != 0 {
                continue;
            }
            if group.Attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0 {
                deny_only_groups += 1;
            } else {
                enabled_groups.push(sid_string(group.Sid)?);
            }
        }
        let privileges = token_buffer(token, TokenPrivileges)?;
        let privileges = (*privileges.as_ptr().cast::<TOKEN_PRIVILEGES>()).PrivilegeCount as usize;
        Ok(TokenFacts {
            token_type,
            impersonation_level,
            integrity,
            appcontainer,
            capabilities,
            // Only an AppContainer token can be a Less Privileged one.
            less_privileged: is_app && less_privileged_claim(token)?,
            restricting_sids: sid_list(groups(&restricting))?,
            enabled_groups,
            deny_only_groups,
            privileges,
        })
    }
}

fn sid_list(list: &[SID_AND_ATTRIBUTES]) -> Result<Vec<String>> {
    list.iter()
        .map(|entry| unsafe { sid_string(entry.Sid) })
        .collect()
}

/// What the parent built into the worker's primary, and so expects to read
/// back from the suspended child.
#[derive(Debug, Clone)]
pub(crate) struct BirthExpectation {
    pub(crate) package: String,
    pub(crate) less_privileged: bool,
    pub(crate) capabilities: Vec<String>,
    pub(crate) restricting_sids: Vec<String>,
    pub(crate) enabled_groups: Vec<String>,
    pub(crate) privileges: bool,
}

/// Compares the suspended child's primary with what was built. Returns every
/// difference found.
pub(crate) fn check_birth(
    facts: &TokenFacts,
    expected: &BirthExpectation,
) -> std::result::Result<(), String> {
    let mut wrong = Vec::new();
    if facts.token_type != TOKEN_PRIMARY {
        wrong.push(format!("token type {} is not primary", facts.token_type));
    }
    if facts.integrity != LOW_INTEGRITY {
        wrong.push(format!("integrity {} is not Low", facts.integrity));
    }
    if facts.appcontainer.as_deref() != Some(expected.package.as_str()) {
        wrong.push(format!(
            "AppContainer SID {:?} is not the package {}",
            facts.appcontainer, expected.package
        ));
    }
    if facts.capabilities != expected.capabilities {
        wrong.push(format!(
            "capabilities {:?}, expected {:?}",
            facts.capabilities, expected.capabilities
        ));
    }
    if facts.less_privileged != expected.less_privileged {
        wrong.push(format!(
            "{LPAC_CLAIM} claim present {}, expected {}",
            facts.less_privileged, expected.less_privileged
        ));
    }
    if sorted(&facts.restricting_sids) != sorted(&expected.restricting_sids) {
        wrong.push(format!(
            "restricting SIDs {:?}, expected {:?}",
            facts.restricting_sids, expected.restricting_sids
        ));
    }
    if sorted(&facts.enabled_groups) != sorted(&expected.enabled_groups) {
        wrong.push(format!(
            "groups not deny-only {:?}, expected {:?}",
            facts.enabled_groups, expected.enabled_groups
        ));
    }
    if (facts.privileges != 0) != expected.privileges {
        wrong.push(format!(
            "{} privileges, expected {}",
            facts.privileges,
            if expected.privileges { "some" } else { "none" }
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

/// Compares the start-up thread token, read back from the suspended thread,
/// with the one the parent set: a Low impersonation token at
/// SecurityImpersonation for the same package. A token the system judges
/// stronger than the process's primary is silently downgraded to
/// SecurityIdentification, and the child then fails before entry, so the
/// level must be read back rather than assumed.
pub(crate) fn check_initial(facts: &TokenFacts, package: &str) -> std::result::Result<(), String> {
    let mut wrong = Vec::new();
    if facts.token_type != TOKEN_IMPERSONATION {
        wrong.push(format!(
            "token type {} is not impersonation",
            facts.token_type
        ));
    }
    if facts.impersonation_level != Some(IMPERSONATION_LEVEL) {
        wrong.push(format!(
            "impersonation level {:?} is not SecurityImpersonation",
            facts.impersonation_level
        ));
    }
    if facts.integrity != LOW_INTEGRITY {
        wrong.push(format!("integrity {} is not Low", facts.integrity));
    }
    if facts.appcontainer.as_deref() != Some(package) {
        wrong.push(format!(
            "AppContainer SID {:?} is not the package {package}",
            facts.appcontainer
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

fn sorted(list: &[String]) -> Vec<&str> {
    let mut list: Vec<&str> = list.iter().map(String::as_str).collect();
    list.sort_unstable();
    list
}

/// Opens this process's own token with full access, the source of the
/// worker's primary.
pub(crate) fn own_token() -> Result<OwnedHandle> {
    let mut token = null_mut();
    check(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) },
        "OpenProcessToken(parent)",
    )?;
    owned(token, "OpenProcessToken(parent)")
}

/// Builds the worker's primary token from the parent's own token: every
/// access group deny-only (logon included), no privileges, the NULL SID as
/// the only restricting SID, Low integrity.
///
/// The AppContainer part is not added here. Process creation lowboxes this
/// token to the package with the security capabilities passed as a creation
/// attribute. Because the token is a restricted copy of the caller's own,
/// creating a process with it needs no SeAssignPrimaryTokenPrivilege.
pub(crate) fn build_primary(
    parent: HANDLE,
    package: &str,
    deviation: Deviation,
) -> Result<(OwnedHandle, BirthExpectation)> {
    unsafe {
        let data = token_buffer(parent, TokenGroups)?;
        let mut kept_enabled = Vec::new();
        let mut disabled = Vec::new();
        for group in groups(&data) {
            // The integrity label is not an access group.
            if group.Attributes & SE_GROUP_INTEGRITY != 0 {
                continue;
            }
            if deviation.keeps_logon_group()
                && group.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID
            {
                kept_enabled.push(sid_string(group.Sid)?);
                continue;
            }
            disabled.push(SID_AND_ATTRIBUTES {
                Sid: group.Sid,
                Attributes: 0,
            });
        }
        if deviation.keeps_logon_group() && kept_enabled.is_empty() {
            return Err("the parent token has no logon SID to keep enabled".into());
        }
        let null_sid = SidBuf::well_known(WinNullSid)?;
        let world = SidBuf::well_known(WinWorldSid)?;
        let mut restrict = vec![SID_AND_ATTRIBUTES {
            Sid: null_sid.as_psid(),
            Attributes: 0,
        }];
        if deviation.widens_restricting_sids() {
            restrict.push(SID_AND_ATTRIBUTES {
                Sid: world.as_psid(),
                Attributes: 0,
            });
        }
        let mut token = null_mut();
        check(
            CreateRestrictedToken(
                parent,
                DISABLE_MAX_PRIVILEGE,
                disabled.len() as u32,
                disabled.as_ptr(),
                0,
                null(),
                restrict.len() as u32,
                restrict.as_ptr(),
                &mut token,
            ),
            "CreateRestrictedToken(primary)",
        )?;
        let token = owned(token, "CreateRestrictedToken(primary)")?;
        // DISABLE_MAX_PRIVILEGE keeps SeChangeNotifyPrivilege. Remove it and
        // anything else left, so the token holds no privilege at all.
        if !deviation.keeps_privileges() {
            remove_privileges(token.as_raw_handle())?;
        }
        ensure_low(token.as_raw_handle())?;
        let mut restricting_sids = vec![null_sid.to_text()?];
        if deviation.widens_restricting_sids() {
            restricting_sids.push(world.to_text()?);
        }
        Ok((
            token,
            BirthExpectation {
                package: package.to_owned(),
                less_privileged: deviation.less_privileged(),
                capabilities: Vec::new(),
                restricting_sids,
                enabled_groups: kept_enabled,
                privileges: deviation.expects_privileges(),
            },
        ))
    }
}

/// Builds the start-up thread token from the token of a never-resumed
/// process born into the same AppContainer: a Low, same-package impersonation
/// token whose restricting SIDs are its own user and groups.
///
/// The worker's primary allows almost nothing, which is too little for the
/// loader to map system DLLs and initialize the process. The loader runs on
/// the main thread, which impersonates this token until the worker reverts
/// to its primary before reading any input. Restricting the token to the
/// same SIDs it already has keeps it from counting as stronger than the
/// restricted primary, which would get it downgraded to identification
/// level and fail the child before entry.
pub(crate) fn build_initial(source: HANDLE) -> Result<OwnedHandle> {
    unsafe {
        let source_groups = token_buffer(source, TokenGroups)?;
        let source_user = token_buffer(source, TokenUser)?;
        let source_dacl = token_dacl(source)?;
        let mut same_access = groups(&source_groups)
            .iter()
            .filter(|group| group.Attributes & SE_GROUP_INTEGRITY == 0)
            .map(|group| SID_AND_ATTRIBUTES {
                Sid: group.Sid,
                Attributes: 0,
            })
            .collect::<Vec<_>>();
        same_access.push(SID_AND_ATTRIBUTES {
            Sid: (*source_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
            Attributes: 0,
        });
        let mut loader = null_mut();
        check(
            CreateRestrictedToken(
                source,
                DISABLE_MAX_PRIVILEGE,
                0,
                null(),
                0,
                null(),
                same_access.len() as u32,
                same_access.as_ptr(),
                &mut loader,
            ),
            "CreateRestrictedToken(initial)",
        )?;
        let loader = owned(loader, "CreateRestrictedToken(initial)")?;
        // Filtering and duplicating otherwise give the new token object the
        // parent's default DACL, which does not grant the package SID: the
        // child could not query its own start-up token.
        set_token_dacl(loader.as_raw_handle(), &source_dacl)?;
        ensure_low(loader.as_raw_handle())?;
        let loader_dacl = token_dacl(loader.as_raw_handle())?;
        let mut initial = null_mut();
        check(
            DuplicateTokenEx(
                loader.as_raw_handle(),
                TOKEN_ALL_ACCESS,
                null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut initial,
            ),
            "DuplicateTokenEx(initial)",
        )?;
        let initial = owned(initial, "DuplicateTokenEx(initial)")?;
        set_token_dacl(initial.as_raw_handle(), &loader_dacl)?;
        check(
            SetHandleInformation(initial.as_raw_handle(), HANDLE_FLAG_INHERIT, 0),
            "SetHandleInformation(initial token)",
        )?;
        let mut flags = 0;
        check(
            GetHandleInformation(initial.as_raw_handle(), &mut flags),
            "GetHandleInformation(initial token)",
        )?;
        if flags & HANDLE_FLAG_INHERIT != 0 {
            return Err("the initial token handle stayed inheritable".into());
        }
        Ok(initial)
    }
}

/// Removes every privilege the token still holds.
unsafe fn remove_privileges(token: HANDLE) -> Result<()> {
    unsafe {
        let data = token_buffer(token, TokenPrivileges)?;
        let list = &*data.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let entries =
            std::slice::from_raw_parts(list.Privileges.as_ptr(), list.PrivilegeCount as usize);
        for entry in entries {
            let remove = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: entry.Luid,
                    Attributes: SE_PRIVILEGE_REMOVED,
                }],
            };
            check(
                AdjustTokenPrivileges(token, 0, &remove, 0, null_mut(), null_mut()),
                "AdjustTokenPrivileges(remove)",
            )?;
            // The call succeeds even when it changed nothing; that case is
            // reported only through the last error.
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                return Err(last("AdjustTokenPrivileges(remove)"));
            }
        }
        Ok(())
    }
}

/// Sets the token's integrity to Low unless it is Low already. Lowering
/// needs only TOKEN_ADJUST_DEFAULT on the handle.
unsafe fn ensure_low(token: HANDLE) -> Result<()> {
    unsafe {
        let low = SidBuf::well_known(WinLowLabelSid)?;
        let current = token_buffer(token, TokenIntegrityLevel)?;
        let current = (*current.as_ptr().cast::<TOKEN_MANDATORY_LABEL>())
            .Label
            .Sid;
        if EqualSid(current, low.as_psid()) != 0 {
            return Ok(());
        }
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: low.as_psid(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        let length = size_of::<TOKEN_MANDATORY_LABEL>()
            + windows_sys::Win32::Security::GetLengthSid(low.as_psid()) as usize;
        check(
            SetTokenInformation(
                token,
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                length as u32,
            ),
            "SetTokenInformation(TokenIntegrityLevel)",
        )
    }
}

unsafe fn token_dacl(token: HANDLE) -> Result<Vec<usize>> {
    unsafe {
        let mut bytes = 0;
        GetKernelObjectSecurity(token, DACL_SECURITY_INFORMATION, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last("GetKernelObjectSecurity(token DACL size)"));
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        check(
            GetKernelObjectSecurity(
                token,
                DACL_SECURITY_INFORMATION,
                data.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            ),
            "GetKernelObjectSecurity(token DACL)",
        )?;
        Ok(data)
    }
}

unsafe fn set_token_dacl(token: HANDLE, descriptor: &[usize]) -> Result<()> {
    check(
        unsafe {
            SetKernelObjectSecurity(
                token,
                DACL_SECURITY_INFORMATION,
                descriptor.as_ptr().cast_mut().cast(),
            )
        },
        "SetKernelObjectSecurity(token DACL)",
    )
}

// Token security attributes have no Win32 reader, so the claim is read with
// the native call. The layouts are those of
// TOKEN_SECURITY_ATTRIBUTES_INFORMATION and TOKEN_SECURITY_ATTRIBUTE_V1 on
// 64-bit Windows.
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
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

/// `TOKEN_SECURITY_ATTRIBUTE_TYPE_UINT64`.
const ATTRIBUTE_TYPE_UINT64: u16 = 2;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtQueryInformationToken(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// Whether the token carries `WIN://NOALLAPPPKG` as the single unsigned
/// value 1, the mark of a Less Privileged AppContainer. The dedicated
/// information class, `TokenIsLessPrivilegedAppContainer`, fails with
/// "invalid information class" on Windows Server 2022 and Windows 11, so the
/// claim is read instead.
fn less_privileged_claim(token: HANDLE) -> Result<bool> {
    unsafe {
        let mut bytes = 0;
        let status =
            NtQueryInformationToken(token, TokenSecurityAttributes, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(format!(
                "NtQueryInformationToken(TokenSecurityAttributes, size): {:#010x}",
                status as u32
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
                "NtQueryInformationToken(TokenSecurityAttributes): {:#010x}",
                status as u32
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
            return Ok(false);
        }
        let entries = std::slice::from_raw_parts(header.attributes, header.count as usize);
        for entry in entries {
            let name = &entry.name;
            let name = if name.buffer.is_null() {
                String::new()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(
                    name.buffer,
                    usize::from(name.length) / 2,
                ))
            };
            if name != LPAC_CLAIM {
                continue;
            }
            if entry.value_type != ATTRIBUTE_TYPE_UINT64 || entry.value_count != 1 {
                return Ok(false);
            }
            return Ok(*entry.values.cast::<u64>() == 1);
        }
        Ok(false)
    }
}
