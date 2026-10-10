//! Token construction, privilege stripping, lowboxing, and pre-resume token attestation.

use crate::windows::deviation::Deviation;
use crate::windows::error::LaunchError;
use crate::windows::native::*;
use crate::windows::profile::AppContainerSid;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::{Foundation::*, Security::*, System::Threading::*};

pub const SE_GROUP_INTEGRITY: u32 = 0x00000020;
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x00000010;
pub const SE_PRIVILEGE_REMOVED: u32 = 0x00000004;
pub const SECURITY_MANDATORY_LOW_RID: u32 = 0x00001000;
pub const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x00002000;

/// Constructed primary and initial impersonation tokens.
pub struct SpawnTokens {
    pub primary: Handle,
    pub initial: Option<Handle>,
}

unsafe fn token_dacl(token: HANDLE) -> Result<Vec<usize>, LaunchError> {
    unsafe {
        let mut bytes = 0;
        GetKernelObjectSecurity(token, DACL_SECURITY_INFORMATION, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last_win32_error("GetKernelObjectSecurity(size)"));
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if GetKernelObjectSecurity(
            token,
            DACL_SECURITY_INFORMATION,
            data.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            return Err(last_win32_error("GetKernelObjectSecurity"));
        }
        Ok(data)
    }
}

pub unsafe fn set_integrity(token: HANDLE, level: WELL_KNOWN_SID_TYPE) -> Result<(), LaunchError> {
    unsafe {
        let mut sid = [0u32; 17];
        let mut bytes = (sid.len() * 4) as u32;
        if CreateWellKnownSid(level, null_mut(), sid.as_mut_ptr().cast(), &mut bytes) == 0 {
            return Err(last_win32_error("CreateWellKnownSid(integrity)"));
        }
        let mut label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        if SetTokenInformation(
            token,
            TokenIntegrityLevel,
            (&mut label as *mut TOKEN_MANDATORY_LABEL).cast(),
            size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        ) == 0
        {
            return Err(last_win32_error("SetTokenInformation(integrity)"));
        }
        Ok(())
    }
}

unsafe fn groups(data: &[usize]) -> &[SID_AND_ATTRIBUTES] {
    let g = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
    unsafe { std::slice::from_raw_parts(g.Groups.as_ptr(), g.GroupCount as usize) }
}

/// Constructs the primary restricted token and initial thread impersonation token.
pub fn construct_tokens(
    package_sid: &AppContainerSid,
    deviation: Deviation,
) -> Result<SpawnTokens, LaunchError> {
    unsafe {
        let mut parent_token = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut parent_token,
        ) == 0
        {
            return Err(last_win32_error("OpenProcessToken(current process)"));
        }
        let parent_token = Handle(parent_token);

        let data = token_buffer(parent_token.0, TokenGroups)?;
        let mut disabled = Vec::new();
        for (i, g) in groups(&data).iter().enumerate() {
            if g.Attributes & SE_GROUP_INTEGRITY == 0 {
                // If GroupNotDenyOnly deviation is set, leave one group enabled
                if deviation == Deviation::GroupNotDenyOnly && i == 0 {
                    continue;
                }
                disabled.push(SID_AND_ATTRIBUTES {
                    Sid: g.Sid,
                    Attributes: 0,
                });
            }
        }

        let mut null_sid = [0u32; 17];
        let mut bytes = (null_sid.len() * 4) as u32;
        if CreateWellKnownSid(
            WinNullSid,
            null_mut(),
            null_sid.as_mut_ptr().cast(),
            &mut bytes,
        ) == 0
        {
            return Err(last_win32_error("CreateWellKnownSid(NULL)"));
        }

        let null_restrict = [SID_AND_ATTRIBUTES {
            Sid: null_sid.as_mut_ptr().cast(),
            Attributes: 0,
        }];

        let restrict: &[SID_AND_ATTRIBUTES] = if deviation == Deviation::RestrictingSidMismatch {
            &[]
        } else {
            &null_restrict
        };

        let mut lockdown = null_mut();
        if CreateRestrictedToken(
            parent_token.0,
            DISABLE_MAX_PRIVILEGE,
            disabled.len() as u32,
            disabled.as_ptr(),
            0,
            null(),
            restrict.len() as u32,
            restrict.as_ptr(),
            &mut lockdown,
        ) == 0
        {
            return Err(last_win32_error("CreateRestrictedToken(lockdown)"));
        }
        let lockdown = Handle(lockdown);

        // Strip any surviving privileges (such as SeChangeNotifyPrivilege) unless PrivilegesPresent
        if deviation != Deviation::PrivilegesPresent {
            let p = token_buffer(lockdown.0, TokenPrivileges)?;
            let p = &*p.as_ptr().cast::<TOKEN_PRIVILEGES>();
            for entry in
                std::slice::from_raw_parts(p.Privileges.as_ptr(), p.PrivilegeCount as usize)
            {
                let remove = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: entry.Luid,
                        Attributes: SE_PRIVILEGE_REMOVED,
                    }],
                };
                AdjustTokenPrivileges(lockdown.0, 0, &remove, 0, null_mut(), null_mut());
            }
        }

        // Set integrity level
        if deviation == Deviation::IntegrityNotUntrusted {
            set_integrity(lockdown.0, WinMediumLabelSid)?;
        } else {
            set_integrity(lockdown.0, WinLowLabelSid)?;
        }

        // Now construct the initial thread token
        let source_dacl = token_dacl(parent_token.0)?;
        let loader_groups = token_buffer(parent_token.0, TokenGroups)?;
        let loader_user = token_buffer(parent_token.0, TokenUser)?;
        let mut same_access = groups(&loader_groups)
            .iter()
            .filter(|g| g.Attributes & SE_GROUP_INTEGRITY == 0)
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            })
            .collect::<Vec<_>>();
        same_access.push(SID_AND_ATTRIBUTES {
            Sid: (*loader_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
            Attributes: 0,
        });

        let mut loader = null_mut();
        if CreateRestrictedToken(
            parent_token.0,
            DISABLE_MAX_PRIVILEGE,
            0,
            null(),
            0,
            null(),
            same_access.len() as u32,
            same_access.as_ptr(),
            &mut loader,
        ) == 0
        {
            return Err(last_win32_error("CreateRestrictedToken(loader)"));
        }
        let loader = Handle(loader);

        if SetKernelObjectSecurity(
            loader.0,
            DACL_SECURITY_INFORMATION,
            source_dacl.as_ptr().cast_mut().cast(),
        ) == 0
        {
            return Err(last_win32_error(
                "SetKernelObjectSecurity(filtered loader DACL)",
            ));
        }

        set_integrity(loader.0, WinLowLabelSid)?;

        // Lowbox the loader token
        let mut attrs = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root: null_mut(),
            name: null_mut(),
            attributes: 0,
            descriptor: null_mut(),
            qos: null_mut(),
        };
        let mut lowbox = null_mut();
        let dummy_cap: SID_AND_ATTRIBUTES = zeroed();
        let caps_ptr = &dummy_cap as *const SID_AND_ATTRIBUTES;
        let cap_count = if deviation == Deviation::CapabilitiesPresent {
            1
        } else {
            0
        };

        let status = NtCreateLowBoxToken(
            &mut lowbox,
            loader.0,
            TOKEN_ALL_ACCESS,
            &mut attrs,
            package_sid.as_psid(),
            cap_count,
            caps_ptr,
            0,
            null(),
        );
        if status != 0 {
            return Err(LaunchError::Other(format!(
                "NtCreateLowBoxToken(loader): 0x{status:08x}"
            )));
        }
        let lowbox = Handle(lowbox);

        let loader_dacl = token_dacl(lowbox.0)?;
        let mut impersonation = null_mut();
        let imp_level = if deviation == Deviation::InitialTokenOpen {
            SecurityIdentification // Level 1 instead of Level 2
        } else {
            SecurityImpersonation // Level 2
        };

        if DuplicateTokenEx(
            lowbox.0,
            TOKEN_ALL_ACCESS,
            null(),
            imp_level,
            TokenImpersonation,
            &mut impersonation,
        ) == 0
        {
            return Err(last_win32_error("DuplicateTokenEx(loader)"));
        }
        let initial = Handle(impersonation);

        if SetKernelObjectSecurity(
            initial.0,
            DACL_SECURITY_INFORMATION,
            loader_dacl.as_ptr().cast_mut().cast(),
        ) == 0
        {
            return Err(last_win32_error("SetKernelObjectSecurity(initial DACL)"));
        }

        if SetHandleInformation(initial.0, HANDLE_FLAG_INHERIT, 0) == 0 {
            return Err(last_win32_error("SetHandleInformation(initial token)"));
        }

        Ok(SpawnTokens {
            primary: lockdown,
            initial: Some(initial),
        })
    }
}

/// Checks the suspended child process's primary token at birth.
///
/// On mismatch, returns `LaunchError::BirthTokenMismatch`.
pub fn check_birth_token(
    process: HANDLE,
    package_sid: &AppContainerSid,
) -> Result<(), LaunchError> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(LaunchError::BirthTokenMismatch(
                "failed to open suspended process token".to_string(),
            ));
        }
        let token = Handle(token);

        // 1. Token type must be TokenPrimary (1)
        let token_type_buf = token_buffer(token.0, TokenType)
            .map_err(|e| LaunchError::BirthTokenMismatch(format!("query TokenType failed: {e}")))?;
        let kind = *token_type_buf.as_ptr().cast::<u32>();
        if kind != TokenPrimary as u32 {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "expected TokenPrimary (1), got {kind}"
            )));
        }

        // 2. Low integrity S-1-16-4096
        let integrity_buf = token_buffer(token.0, TokenIntegrityLevel).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenIntegrityLevel failed: {e}"))
        })?;
        let label = &*integrity_buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sub_auth_count = *GetSidSubAuthorityCount(label.Label.Sid);
        if sub_auth_count == 0 {
            return Err(LaunchError::BirthTokenMismatch(
                "empty integrity SID".to_string(),
            ));
        }
        let last_sub_auth = *GetSidSubAuthority(label.Label.Sid, (sub_auth_count - 1) as u32);
        if last_sub_auth != SECURITY_MANDATORY_LOW_RID {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "expected Low integrity (0x{:04x}), got 0x{last_sub_auth:04x}",
                SECURITY_MANDATORY_LOW_RID
            )));
        }

        // 3. AppContainer SID equals package SID
        let app_buf = token_buffer(token.0, TokenAppContainerSid).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenAppContainerSid failed: {e}"))
        })?;
        let app_sid =
            (*app_buf.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer;
        if app_sid.is_null() || !package_sid.matches(app_sid) {
            return Err(LaunchError::BirthTokenMismatch(
                "AppContainer SID does not match expected package SID".to_string(),
            ));
        }

        // 4. Zero capabilities
        let cap_buf = token_buffer(token.0, TokenCapabilities).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenCapabilities failed: {e}"))
        })?;
        let cap_count = (*cap_buf.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount;
        if cap_count != 0 {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "expected 0 capabilities, found {cap_count}"
            )));
        }

        // 5. The WIN://NOALLAPPPKG claim
        let mut bytes = 0;
        let status = NtQueryInformationToken(token.0, 44, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "NtQueryInformationToken(TokenSecurityAttributes size) failed: 0x{status:08x}"
            )));
        }
        let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let status =
            NtQueryInformationToken(token.0, 44, data.as_mut_ptr().cast(), bytes, &mut bytes);
        if status != 0 {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "NtQueryInformationToken(TokenSecurityAttributes) failed: 0x{status:08x}"
            )));
        }
        let header = &*data.as_ptr().cast::<SecurityAttributes>();
        let entries = if header.count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(header.attributes, header.count as usize)
        };
        let mut has_noallapppkg = false;
        for entry in entries {
            if entry.name.to_string_lossy() == "WIN://NOALLAPPPKG" {
                if entry.kind == 2 && entry.count >= 1 {
                    let vals = std::slice::from_raw_parts(
                        entry.values.cast::<u64>(),
                        entry.count as usize,
                    );
                    if vals.first().copied() == Some(1) {
                        has_noallapppkg = true;
                    }
                }
            }
        }
        if !has_noallapppkg {
            return Err(LaunchError::BirthTokenMismatch(
                "missing WIN://NOALLAPPPKG claim with value [1]".to_string(),
            ));
        }

        // 6. Restricting SIDs exactly {S-1-0-0}
        let mut null_sid = [0u32; 17];
        let mut bytes = (null_sid.len() * 4) as u32;
        CreateWellKnownSid(
            WinNullSid,
            null_mut(),
            null_sid.as_mut_ptr().cast(),
            &mut bytes,
        );

        let restrict_buf = token_buffer(token.0, TokenRestrictedSids).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenRestrictedSids failed: {e}"))
        })?;
        let restrict_groups = groups(&restrict_buf);
        if restrict_groups.len() != 1
            || EqualSid(restrict_groups[0].Sid, null_sid.as_mut_ptr().cast()) == 0
        {
            return Err(LaunchError::BirthTokenMismatch(
                "restricting SIDs are not exactly {S-1-0-0}".to_string(),
            ));
        }

        // 7. Every group deny-only except SE_GROUP_INTEGRITY
        let group_buf = token_buffer(token.0, TokenGroups).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenGroups failed: {e}"))
        })?;
        for g in groups(&group_buf) {
            if g.Attributes & SE_GROUP_INTEGRITY == 0 {
                if g.Attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0 {
                    return Err(LaunchError::BirthTokenMismatch(
                        "found access group that is not deny-only".to_string(),
                    ));
                }
            }
        }

        // 8. Zero privileges
        let priv_buf = token_buffer(token.0, TokenPrivileges).map_err(|e| {
            LaunchError::BirthTokenMismatch(format!("query TokenPrivileges failed: {e}"))
        })?;
        let priv_count = (*priv_buf.as_ptr().cast::<TOKEN_PRIVILEGES>()).PrivilegeCount;
        if priv_count != 0 {
            return Err(LaunchError::BirthTokenMismatch(format!(
                "expected 0 privileges, found {priv_count}"
            )));
        }

        Ok(())
    }
}

/// Verifies the suspended main thread token and closes the parent's initial handle before resume.
///
/// On failure, returns `LaunchError::InitialTokenOpen`.
pub fn check_initial_token_and_close(
    main_thread: HANDLE,
    tokens: &mut SpawnTokens,
    package_sid: &AppContainerSid,
    deviation: Deviation,
) -> Result<(), LaunchError> {
    unsafe {
        // Read back thread token from suspended main thread
        let mut thread_token = null_mut();
        if OpenThreadToken(main_thread, TOKEN_QUERY, 1, &mut thread_token) == 0 {
            return Err(LaunchError::InitialTokenOpen(
                "failed to open thread token on suspended thread".to_string(),
            ));
        }
        let thread_token = Handle(thread_token);

        // Impersonation level must be SecurityImpersonation (2)
        let imp_buf = token_buffer(thread_token.0, TokenImpersonationLevel).map_err(|e| {
            LaunchError::InitialTokenOpen(format!("query TokenImpersonationLevel failed: {e}"))
        })?;
        let imp_level = *imp_buf.as_ptr().cast::<u32>();
        if imp_level != SecurityImpersonation as u32 {
            return Err(LaunchError::InitialTokenOpen(format!(
                "expected SecurityImpersonation (2), got {imp_level}"
            )));
        }

        // Integrity must be Low
        let integrity_buf = token_buffer(thread_token.0, TokenIntegrityLevel).map_err(|e| {
            LaunchError::InitialTokenOpen(format!("query TokenIntegrityLevel failed: {e}"))
        })?;
        let label = &*integrity_buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sub_auth_count = *GetSidSubAuthorityCount(label.Label.Sid);
        if sub_auth_count == 0 {
            return Err(LaunchError::InitialTokenOpen(
                "empty integrity SID".to_string(),
            ));
        }
        let last_sub_auth = *GetSidSubAuthority(label.Label.Sid, (sub_auth_count - 1) as u32);
        if last_sub_auth != SECURITY_MANDATORY_LOW_RID {
            return Err(LaunchError::InitialTokenOpen(format!(
                "expected Low integrity, got 0x{last_sub_auth:04x}"
            )));
        }

        // Package SID must match
        let app_buf = token_buffer(thread_token.0, TokenAppContainerSid).map_err(|e| {
            LaunchError::InitialTokenOpen(format!("query TokenAppContainerSid failed: {e}"))
        })?;
        let app_sid =
            (*app_buf.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer;
        if app_sid.is_null() || !package_sid.matches(app_sid) {
            return Err(LaunchError::InitialTokenOpen(
                "initial thread token AppContainer SID does not match package SID".to_string(),
            ));
        }

        // Check handle close before resume
        if deviation == Deviation::InitialTokenOpen {
            // Intentionally leak or leave the initial handle open to trigger refusal
            return Err(LaunchError::InitialTokenOpen(
                "initial token handle remained open before resume".to_string(),
            ));
        }

        if let Some(initial_handle) = tokens.initial.take() {
            let raw = initial_handle.into_raw();
            if CloseHandle(raw) == 0 {
                return Err(LaunchError::InitialTokenOpen(
                    "failed to close initial token handle before resume".to_string(),
                ));
            }
        } else {
            return Err(LaunchError::InitialTokenOpen(
                "initial token handle was not held".to_string(),
            ));
        }

        Ok(())
    }
}
