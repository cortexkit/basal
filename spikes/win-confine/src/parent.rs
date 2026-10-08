use crate::native::*;
use crate::profile::UserEnv;
use serde_json::{Value, json};
use std::{
    fs,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    thread,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Security::{Authorization::*, Isolation::*},
    Storage::FileSystem::*,
    System::{
        Diagnostics::Debug::{DebugActiveProcess, DebugSetProcessKillOnExit},
        JobObjects::*,
        Memory::*,
        Pipes::*,
        RemoteDesktop::*,
        Threading::*,
        WindowsProgramming::*,
    },
};

const PROFILE: &str = "basal.spike.worker";
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
// PROCESS_CREATION_MITIGATION_POLICY_*_ALWAYS_ON values from winnt.h.
const MITIGATIONS: u64 =
    (1 << 24) | (1 << 28) | (1 << 32) | (1 << 36) | (1 << 44) | (1 << 52) | (1 << 56) | (1 << 60);
struct Tokens {
    primary: Handle,
    initial: Option<Handle>,
    report: Value,
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
struct Profile {
    sid: PSID,
    deleted: bool,
    api: UserEnv,
}
impl Drop for Profile {
    fn drop(&mut self) {
        unsafe {
            if !self.deleted {
                self.api.delete(PROFILE);
            }
            FreeSid(self.sid);
        }
    }
}
struct SuspendedContainer {
    process: Handle,
    _thread: Handle,
}
impl Drop for SuspendedContainer {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process.0, 0);
            WaitForSingleObject(self.process.0, 5000);
        }
    }
}
struct AttributeList {
    buffer: Vec<usize>,
}
impl AttributeList {
    fn new(count: u32) -> Result<Self> {
        unsafe {
            let mut bytes = 0;
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes);
            let mut this = Self {
                buffer: vec![0; (bytes as usize).div_ceil(size_of::<usize>())],
            };
            check(
                InitializeProcThreadAttributeList(this.ptr(), count, 0, &mut bytes),
                "InitializeProcThreadAttributeList",
            )?;
            Ok(this)
        }
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }
    fn add<T>(&mut self, key: u32, value: &T) -> Result<()> {
        unsafe {
            check(
                UpdateProcThreadAttribute(
                    self.ptr(),
                    0,
                    key as usize,
                    (value as *const T).cast(),
                    size_of::<T>(),
                    null_mut(),
                    null(),
                ),
                &format!("UpdateProcThreadAttribute({key:#x})"),
            )
        }
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}
fn pipe() -> Result<(Handle, Handle)> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut read = null_mut();
        let mut write = null_mut();
        check(CreatePipe(&mut read, &mut write, &sa, 0), "CreatePipe")?;
        Ok((Handle(read), Handle(write)))
    }
}
fn restricted_tokens(
    parent: HANDLE,
    loader_source: HANDLE,
    start_low: bool,
    lowbox_sid: Option<PSID>,
    same_primary: bool,
    enabled_groups: bool,
) -> Result<Tokens> {
    unsafe {
        let data = token_buffer(parent, TokenGroups)?;
        // Integrity labels are not access groups. Every actual group is disabled,
        // including the logon SID; no restricting SID can match a normal user ACL.
        let disabled = groups(&data)
            .iter()
            .filter(|g| g.Attributes & SE_GROUP_INTEGRITY == 0)
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            })
            .collect::<Vec<_>>();
        let mut null_sid = [0u32; 17];
        let mut bytes = (null_sid.len() * 4) as u32;
        check(
            CreateWellKnownSid(
                WinNullSid,
                null_mut(),
                null_sid.as_mut_ptr().cast(),
                &mut bytes,
            ),
            "CreateWellKnownSid(NULL)",
        )?;
        let null_restrict = [SID_AND_ATTRIBUTES {
            Sid: null_sid.as_mut_ptr().cast(),
            Attributes: 0,
        }];
        let primary_user = token_buffer(parent, TokenUser)?;
        let mut primary_same = groups(&data)
            .iter()
            .filter(|g| g.Attributes & SE_GROUP_INTEGRITY == 0)
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            })
            .collect::<Vec<_>>();
        primary_same.push(SID_AND_ATTRIBUTES {
            Sid: (*primary_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
            Attributes: 0,
        });
        let restrict = if same_primary {
            primary_same.as_slice()
        } else {
            &null_restrict
        };
        let mut lockdown = null_mut();
        check(
            CreateRestrictedToken(
                parent,
                DISABLE_MAX_PRIVILEGE,
                if enabled_groups {
                    0
                } else {
                    disabled.len() as u32
                },
                if enabled_groups {
                    null()
                } else {
                    disabled.as_ptr()
                },
                0,
                null(),
                restrict.len() as u32,
                restrict.as_ptr(),
                &mut lockdown,
            ),
            "CreateRestrictedToken(lockdown)",
        )?;
        let lockdown = Handle(lockdown);
        // DISABLE_MAX_PRIVILEGE keeps SeChangeNotifyPrivilege; remove that last
        // privilege as well rather than misreporting 'disabled' as 'removed'.
        let p = token_buffer(lockdown.0, TokenPrivileges)?;
        let p = &*p.as_ptr().cast::<TOKEN_PRIVILEGES>();
        for entry in std::slice::from_raw_parts(p.Privileges.as_ptr(), p.PrivilegeCount as usize) {
            let remove = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: entry.Luid,
                    Attributes: SE_PRIVILEGE_REMOVED,
                }],
            };
            check(
                AdjustTokenPrivileges(lockdown.0, 0, &remove, 0, null_mut(), null_mut()),
                "AdjustTokenPrivileges(remove)",
            )?;
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                return Err(last("AdjustTokenPrivileges(remove)"));
            }
        }
        let current = token_buffer(lockdown.0, TokenIntegrityLevel)?;
        let current = sid_string(
            (*current.as_ptr().cast::<TOKEN_MANDATORY_LABEL>())
                .Label
                .Sid,
        );
        if current != if start_low { "S-1-16-4096" } else { "S-1-16-0" } {
            set_integrity(
                lockdown.0,
                if start_low {
                    WinLowLabelSid
                } else {
                    WinUntrustedLabelSid
                },
            )?;
        }
        // Same-access restricting SIDs retain the source user and groups;
        // unlike the primary's NULL SID, they permit trusted loader operations.
        let loader_groups = token_buffer(loader_source, TokenGroups)?;
        let source_dacl = token_dacl(loader_source)?;
        let loader_user = token_buffer(loader_source, TokenUser)?;
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
        check(
            CreateRestrictedToken(
                loader_source,
                DISABLE_MAX_PRIVILEGE,
                0,
                null(),
                0,
                null(),
                same_access.len() as u32,
                same_access.as_ptr(),
                &mut loader,
            ),
            "CreateRestrictedToken(loader)",
        )?;
        let loader = Handle(loader);
        // Filtering and duplication otherwise use the broker's default DACL,
        // which lacks the package SID needed for the lowbox to query itself.
        check(
            SetKernelObjectSecurity(
                loader.0,
                DACL_SECURITY_INFORMATION,
                source_dacl.as_ptr().cast_mut().cast(),
            ),
            "SetKernelObjectSecurity(filtered loader DACL)",
        )?;
        if start_low {
            let level = token_buffer(loader.0, TokenIntegrityLevel)?;
            let level = sid_string((*level.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid);
            if level != "S-1-16-4096" {
                set_integrity(loader.0, WinLowLabelSid)?;
            }
        }
        let loader = if let Some(sid) = lowbox_sid {
            create_lowbox_token(loader.0, sid)?
        } else {
            loader
        };
        let loader_dacl = token_dacl(loader.0)?;
        let mut impersonation = null_mut();
        check(
            DuplicateTokenEx(
                loader.0,
                TOKEN_ALL_ACCESS,
                null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut impersonation,
            ),
            "DuplicateTokenEx(loader)",
        )?;
        let initial = Handle(impersonation);
        check(
            SetKernelObjectSecurity(
                initial.0,
                DACL_SECURITY_INFORMATION,
                loader_dacl.as_ptr().cast_mut().cast(),
            ),
            "SetKernelObjectSecurity(initial DACL)",
        )?;
        check(
            SetHandleInformation(initial.0, HANDLE_FLAG_INHERIT, 0),
            "SetHandleInformation(initial token)",
        )?;
        let mut flags = 0;
        check(
            GetHandleInformation(initial.0, &mut flags),
            "GetHandleInformation(initial token)",
        )?;
        if flags & HANDLE_FLAG_INHERIT != 0 {
            return Err("initial token handle remained inheritable".into());
        }
        let attestation = json!({"lockdown":token_attestation(lockdown.0)?,"initial":token_attestation(initial.0)?,"initial_handle_flags":hex(flags)});
        Ok(Tokens {
            primary: lockdown,
            initial: Some(initial),
            report: attestation,
        })
    }
}
fn job(ui_flags: u32) -> Result<(Handle, Value)> {
    unsafe {
        let h = Handle::new(CreateJobObjectW(null(), null()), "CreateJobObjectW")?;
        let flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = flags;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = MEMORY_LIMIT;
        check(
            SetInformationJobObject(
                h.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ),
            "SetInformationJobObject(limits)",
        )?;
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: ui_flags,
        };
        check(
            SetInformationJobObject(
                h.0,
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            ),
            "SetInformationJobObject(UI)",
        )?;
        let mut actual: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        let mut actual_ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = zeroed();
        check(
            QueryInformationJobObject(
                h.0,
                JobObjectExtendedLimitInformation,
                (&mut actual as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            ),
            "QueryInformationJobObject(owned handle)",
        )?;
        check(
            QueryInformationJobObject(
                h.0,
                JobObjectBasicUIRestrictions,
                (&mut actual_ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                null_mut(),
            ),
            "QueryInformationJobObject(UI)",
        )?;
        if actual.BasicLimitInformation.LimitFlags != flags
            || actual.BasicLimitInformation.ActiveProcessLimit != 1
            || actual.ProcessMemoryLimit != MEMORY_LIMIT
            || actual_ui.UIRestrictionsClass != ui_flags
        {
            return Err("job limit readback mismatch".into());
        }
        Ok((
            h,
            json!({"query_handle":h_value(&actual),"limit_flags":hex(actual.BasicLimitInformation.LimitFlags),"active_process_limit":actual.BasicLimitInformation.ActiveProcessLimit,"process_memory_limit":actual.ProcessMemoryLimit,"ui_restrictions":hex(actual_ui.UIRestrictionsClass),"breakaway":false}),
        ))
    }
}
fn h_value(_: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> &'static str {
    "parent-owned non-inherited handle"
}
fn grant_image(path: &str, sid: PSID) -> Result<()> {
    unsafe {
        let mut old = null_mut();
        let mut descriptor = null_mut();
        let e = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old,
            null_mut(),
            &mut descriptor,
        );
        if e != 0 {
            return Err(format!("GetNamedSecurityInfoW: {e}"));
        }
        let ace = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.cast(),
            },
        };
        let mut new = null_mut();
        let e = SetEntriesInAclW(1, &ace, old, &mut new);
        if e != 0 {
            LocalFree(descriptor);
            return Err(format!("SetEntriesInAclW: {e}"));
        }
        let e = SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new,
            null(),
        );
        LocalFree(new.cast());
        LocalFree(descriptor);
        if e != 0 {
            Err(format!("SetNamedSecurityInfoW: {e}"))
        } else {
            Ok(())
        }
    }
}
fn fixture() -> Result<(Handle, Handle)> {
    unsafe {
        let mut sd: SECURITY_DESCRIPTOR = zeroed();
        check(
            InitializeSecurityDescriptor((&mut sd as *mut SECURITY_DESCRIPTOR).cast(), 1),
            "InitializeSecurityDescriptor",
        )?;
        // A NULL DACL is intentionally more permissive than a typical system
        // object. It makes a successful native-object open a meaningful control.
        check(
            SetSecurityDescriptorDacl((&mut sd as *mut SECURITY_DESCRIPTOR).cast(), 1, null(), 0),
            "SetSecurityDescriptorDacl(NULL)",
        )?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        };
        let section = Handle::new(
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &sa,
                PAGE_READWRITE,
                0,
                4096,
                wide("Local\\basal.spike.section").as_ptr(),
            ),
            "CreateFileMappingW(fixture)",
        )?;
        let event = Handle::new(
            CreateEventW(&sa, 1, 0, wide("Local\\basal.spike.event").as_ptr()),
            "CreateEventW(fixture)",
        )?;
        Ok((section, event))
    }
}
fn add_directory(path: &str, targets: &mut Vec<Target>, enumeration: &mut Vec<Value>) {
    match enumerate_directory(path) {
        Ok(entries) => {
            enumeration.push(json!({"namespace":path,"count":entries.len()}));
            targets.push(Target::new("object_directory", path, "DIRECTORY_QUERY"));
            for (name, kind) in entries {
                let (kind, access) = match kind.as_str() {
                    "Section" => ("section", "SECTION_QUERY"),
                    "Event" => ("event", "EVENT_QUERY_STATE"),
                    "ALPC Port" => ("alpc", "NtAlpcConnectPort / SecurityIdentification"),
                    "Directory" => ("object_directory", "DIRECTORY_QUERY"),
                    "Mutant" => ("mutant", "MUTANT_QUERY_STATE"),
                    "Semaphore" => ("semaphore", "SEMAPHORE_QUERY_STATE"),
                    "Timer" => ("timer", "TIMER_QUERY_STATE"),
                    "SymbolicLink" => ("symbolic_link", "SYMBOLIC_LINK_QUERY"),
                    "Job" => ("job", "JOB_OBJECT_QUERY"),
                    other => (other, "unsupported object type"),
                };
                targets.push(Target::new(kind, &name, access));
                if kind == "section" {
                    targets.push(Target::new("section_read", &name, "SECTION_MAP_READ"));
                    targets.push(Target::new("section_write", &name, "SECTION_MAP_WRITE"));
                } else if kind == "event" {
                    targets.push(Target::new("event_modify", &name, "EVENT_MODIFY_STATE"));
                }
            }
        }
        Err(e) => enumeration.push(json!({"namespace":path,"error":e})),
    }
}
fn prepare_namespace(image: &str, sid: PSID, api: &UserEnv) -> Result<(SuspendedContainer, Value)> {
    unsafe {
        // The kernel materializes a lowbox object directory at process creation,
        // not profile creation. This never-resumed process holds that namespace
        // alive while the parent inventories it before any measured child starts.
        let mut attrs = AttributeList::new(2)?;
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        attrs.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
        )?;
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs.ptr();
        let mut pi: PROCESS_INFORMATION = zeroed();
        let mut cmd = wide(&format!("\"{image}\" --child"));
        check(
            CreateProcessW(
                wide(image).as_ptr(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
                null(),
                null(),
                &si.StartupInfo,
                &mut pi,
            ),
            "CreateProcessW(namespace initializer)",
        )?;
        let holder = SuspendedContainer {
            process: Handle(pi.hProcess),
            _thread: Handle(pi.hThread),
        };
        let mut token = null_mut();
        check(
            OpenProcessToken(holder.process.0, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token),
            "OpenProcessToken(namespace initializer)",
        )?;
        let token = Handle(token);
        let mut imp = null_mut();
        check(
            DuplicateTokenEx(
                token.0,
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut imp,
            ),
            "DuplicateTokenEx(namespace initializer)",
        )?;
        let imp = Handle(imp);
        check(
            SetThreadToken(null(), imp.0),
            "SetThreadToken(namespace initializer)",
        )?;
        let mut hive = null_mut();
        let hr = api.registry(1, &mut hive);
        let registry = if hr >= 0 {
            let value = registry_name(hive);
            windows_sys::Win32::System::Registry::RegCloseKey(hive);
            value
        } else {
            json!({"HRESULT":hex(hr as u32)})
        };
        // Revert even if the package registry API failed; enumeration and all
        // later process launches must run as the parent, not the initializer.
        check(RevertToSelf(), "RevertToSelf(namespace initializer)")?;
        Ok((
            holder,
            json!({"sequence":"CreateProcessW / LPAC / suspended / never resumed","registry":registry}),
        ))
    }
}
fn launch(
    input: &Input,
    image: &str,
    sid: PSID,
    parent_token: HANDLE,
    loader_source: HANDLE,
    sequence: &str,
    start_low: bool,
    native_lowbox_loader: bool,
    leak_only: bool,
) -> Result<Value> {
    launch_variant(
        input,
        image,
        sid,
        parent_token,
        loader_source,
        sequence,
        start_low,
        native_lowbox_loader,
        leak_only,
        MITIGATIONS,
        0xff,
    )
}
fn launch_variant(
    input: &Input,
    image: &str,
    sid: PSID,
    parent_token: HANDLE,
    loader_source: HANDLE,
    sequence: &str,
    start_low: bool,
    native_lowbox_loader: bool,
    leak_only: bool,
    mitigation: u64,
    ui_flags: u32,
) -> Result<Value> {
    unsafe {
        let full = input.mode == "full";
        let lpac = input.mode != "plain";
        let bare = sequence == "bare-token-control";
        let debug = sequence == "loader-trace";
        let mut tokens = if full {
            Some(restricted_tokens(
                parent_token,
                loader_source,
                start_low,
                if native_lowbox_loader {
                    Some(sid)
                } else {
                    None
                },
                sequence == "same-access-primary-control"
                    || sequence == "post-load-primary-control",
                sequence == "post-load-primary-control",
            )?)
        } else {
            None
        };
        let mut job = if full && !bare {
            Some(job(ui_flags)?)
        } else {
            None
        };
        let (child_in, parent_in) = pipe()?;
        let (parent_out, child_out) = pipe()?;
        let (parent_err, child_err) = pipe()?;
        for h in [&parent_in, &parent_out, &parent_err] {
            check(
                SetHandleInformation(h.0, HANDLE_FLAG_INHERIT, 0),
                "SetHandleInformation",
            )?;
        }
        let mut attrs = AttributeList::new(if full {
            6
        } else if lpac {
            3
        } else {
            1
        })?;
        let handles = [child_in.0, child_out.0, child_err.0];
        if let Some(tokens) = &mut tokens {
            let excluded = !handles.contains(&tokens.initial.as_ref().unwrap().0);
            tokens.report["initial_handle_not_in_list"] = json!(excluded);
            if !excluded {
                return Err("initial token is in the handle list".into());
            }
        }
        attrs.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let optout = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        let child_policy = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
        if lpac && !bare {
            attrs.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
            attrs.add(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                &optout,
            )?;
        }
        let jobs = [job.as_ref().map(|j| j.0.0).unwrap_or(null_mut())];
        if full && !bare {
            attrs.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
            attrs.add(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigation)?;
            attrs.add(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &child_policy)?;
        }
        let mut startup: STARTUPINFOEXW = zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = child_in.0;
        startup.StartupInfo.hStdOutput = child_out.0;
        startup.StartupInfo.hStdError = child_err.0;
        startup.lpAttributeList = if bare { null_mut() } else { attrs.ptr() };
        if bare {
            startup.StartupInfo.cb = size_of::<STARTUPINFOW>() as u32;
        }
        let mut pi: PROCESS_INFORMATION = zeroed();
        let mut command = wide(&format!(
            "\"{image}\" --child{}{}{}",
            if start_low { " --lower-integrity" } else { "" },
            if leak_only { " --probe-leak" } else { "" },
            if sequence == "post-load-primary-control" {
                " --replace-primary"
            } else {
                ""
            }
        ));
        let mut flags = if bare {
            CREATE_SUSPENDED | CREATE_NO_WINDOW
        } else {
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW
        };
        if debug {
            flags |= DEBUG_ONLY_THIS_PROCESS;
        }
        let mut debug_creation_error = Value::Null;
        let mut create = |flags, pi: &mut PROCESS_INFORMATION| {
            if full {
                CreateProcessAsUserW(
                    tokens.as_ref().unwrap().primary.0,
                    wide(image).as_ptr(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    flags,
                    null(),
                    null(),
                    &startup.StartupInfo,
                    pi,
                )
            } else {
                CreateProcessW(
                    wide(image).as_ptr(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    flags,
                    null(),
                    null(),
                    &startup.StartupInfo,
                    pi,
                )
            }
        };
        let mut ok = create(flags, &mut pi);
        let mut attached = false;
        if debug && ok == 0 {
            debug_creation_error = json!(last("CreateProcessAsUserW(DEBUG_ONLY_THIS_PROCESS)"));
            flags &= !DEBUG_ONLY_THIS_PROCESS;
            ok = create(flags, &mut pi);
            if ok != 0 {
                if DebugActiveProcess(pi.dwProcessId) == 0 {
                    let error = last("DebugActiveProcess(before resume)");
                    TerminateProcess(pi.hProcess, 1);
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    return Err(format!("{debug_creation_error}; {error}"));
                }
                attached = true;
            }
        }
        check(ok, sequence)?;
        let debug_setup = if debug {
            let ok = DebugSetProcessKillOnExit(1) != 0;
            json!({"success":ok,"error":if ok {0} else {GetLastError()}})
        } else {
            Value::Null
        };
        let process = Handle(pi.hProcess);
        let main_thread = Handle(pi.hThread);
        if let Some((h, info)) = &mut job {
            let mut member = 0;
            let ok = IsProcessInJob(process.0, h.0, &mut member) != 0;
            let error = GetLastError();
            info["member_at_birth"] = json!(ok && member != 0);
            info["membership_query_error"] = json!(if ok { 0 } else { error });
        }
        let mut birth_primary = null_mut();
        let birth_primary = if OpenProcessToken(process.0, TOKEN_QUERY, &mut birth_primary) == 0 {
            json!({"error":last("OpenProcessToken(suspended child)")})
        } else {
            let h = Handle(birth_primary);
            token_attestation(h.0).unwrap_or_else(|e| json!({"error":e}))
        };
        if full {
            if let Err(e) = check(
                SetThreadToken(
                    &main_thread.0,
                    tokens.as_ref().unwrap().initial.as_ref().unwrap().0,
                ),
                "SetThreadToken(suspended main thread)",
            ) {
                TerminateProcess(process.0, 1);
                return Err(e);
            }
        }
        let mut assigned = null_mut();
        let assigned_loader = if full {
            if OpenThreadToken(main_thread.0, TOKEN_QUERY, 1, &mut assigned) == 0 {
                json!({"error":last("OpenThreadToken(suspended child)")})
            } else {
                let h = Handle(assigned);
                token_attestation(h.0).unwrap_or_else(|e| json!({"error":e}))
            }
        } else {
            Value::Null
        };
        let initial_closed_before_resume = if let Some(tokens) = &mut tokens {
            let mut initial = tokens.initial.take().unwrap();
            if CloseHandle(initial.0) == 0 {
                TerminateProcess(process.0, 1);
                return Err(last("CloseHandle(initial before resume)"));
            }
            initial.0 = null_mut();
            drop(initial);
            true
        } else {
            false
        };
        if ResumeThread(main_thread.0) == u32::MAX {
            TerminateProcess(process.0, 1);
            return Err(last("ResumeThread"));
        }
        drop(child_in);
        drop(child_out);
        drop(child_err);
        // Drain both pipes concurrently so neither can deadlock a large namespace report.
        let out_value = parent_out.0 as usize;
        let err_value = parent_err.0 as usize;
        let reader = thread::spawn(move || read_pipe(out_value as HANDLE));
        let err_reader = thread::spawn(move || read_pipe(err_value as HANDLE));
        let payload = serde_json::to_vec(input).map_err(|e| e.to_string())?;
        let input_handle = parent_in.0 as usize;
        std::mem::forget(parent_in);
        // A debug stop suspends the reader before entry. Write on another thread
        // so a payload larger than the pipe buffer cannot stall the event pump.
        let writer = thread::spawn(move || {
            let parent_in = Handle(input_handle as HANDLE);
            let mut written = 0;
            let ok = WriteFile(
                parent_in.0,
                payload.as_ptr(),
                payload.len() as u32,
                &mut written,
                null_mut(),
            ) != 0;
            let error = if ok { 0 } else { GetLastError() };
            (ok, error, written)
        });
        let debug_events = if debug {
            crate::trace::collect(process.0, pi.dwProcessId)
        } else {
            Value::Null
        };
        let wait = WaitForSingleObject(process.0, 180_000);
        if wait != WAIT_OBJECT_0 {
            TerminateProcess(process.0, 124);
            WaitForSingleObject(process.0, 5000);
        }
        let mut exit = 0;
        GetExitCodeProcess(process.0, &mut exit);
        let (write_ok, write_error, written) =
            writer.join().map_err(|_| "stdin writer panicked")?;
        let stdout = reader.join().map_err(|_| "stdout reader panicked")??;
        let stderr = err_reader.join().map_err(|_| "stderr reader panicked")??;
        let child: Value = serde_json::from_slice(&stdout).unwrap_or_else(
            |e| json!({"parse_error":e.to_string(),"stdout":String::from_utf8_lossy(&stdout)}),
        );
        Ok(
            json!({"sequence":sequence,"debug":{"requested":debug,"setup":debug_setup,"attached_before_resume":attached,"creation_error":debug_creation_error,"trace":debug_events},"exit_code":hex(exit),"timeout":wait!=WAIT_OBJECT_0,"input_write":{"success":write_ok,"error":if write_ok{0}else{write_error},"bytes":written},"stderr":String::from_utf8_lossy(&stderr),"job":job.as_ref().map(|j|&j.1),"constructed_tokens":tokens.as_ref().map(|t|&t.report),"parent_before_resume":{"primary":birth_primary,"assigned_loader":assigned_loader,"initial_handle_closed":initial_closed_before_resume},"stdio_handles":{"stdin":handles[0] as usize,"stdout":handles[1] as usize,"stderr":handles[2] as usize},"child":child}),
        )
    }
}
fn read_pipe(h: HANDLE) -> Result<Vec<u8>> {
    unsafe {
        let mut output = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let mut bytes = 0;
            if ReadFile(
                h,
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                &mut bytes,
                null_mut(),
            ) == 0
            {
                let e = GetLastError();
                if e == ERROR_BROKEN_PIPE {
                    break;
                }
                return Err(format!("ReadFile(report): {e}"));
            }
            if bytes == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..bytes as usize]);
        }
        Ok(output)
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let started = Instant::now();
        let mut sid = null_mut();
        let api = UserEnv::new()?;
        let hr = api.create(PROFILE, &mut sid);
        if hr < 0 {
            return Err(format!(
                "CreateAppContainerProfile: {} (remove stale {PROFILE} profile before retry)",
                hex(hr as u32)
            ));
        }
        let mut profile = Profile {
            sid,
            deleted: false,
            api,
        };
        let sid_text = sid_string(profile.sid);
        let package = profile.api.folder(&sid_text)?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let binary_dir = exe.parent().unwrap().join("placed");
        fs::create_dir_all(&binary_dir).map_err(|e| e.to_string())?;
        grant_image(&binary_dir.to_string_lossy(), sid)?;
        let image = binary_dir.join("win-confine.exe");
        fs::copy(&exe, &image).map_err(|e| e.to_string())?;
        let namespace_initializer = prepare_namespace(&image.to_string_lossy(), sid, &profile.api);
        let temp = std::env::var("TEMP").map_err(|e| e.to_string())?;
        let user = std::env::var("USERPROFILE").map_err(|e| e.to_string())?;
        let system = std::env::var("SystemRoot").map_err(|e| e.to_string())?;
        let package_registry = format!(
            "HKCU\\Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppContainer\\Storage\\{PROFILE}"
        );
        let mut targets = vec![
            Target::new(
                "file",
                format!("{system}\\System32\\kernel32.dll"),
                "FILE_READ_DATA",
            ),
            Target::new(
                "file",
                format!("{system}\\System32\\cmd.exe"),
                "FILE_READ_DATA",
            ),
        ];
        for dir in [
            &user,
            &temp,
            &package,
            &binary_dir.to_string_lossy().to_string(),
        ] {
            targets.push(Target::new("directory", dir, "FILE_LIST_DIRECTORY"));
        }
        // Include the conventional backing-file location as a named probe, not an
        // assertion that all unpackaged AppContainers use a file-backed hive there.
        targets.push(Target::new(
            "file",
            format!("{package}\\SystemAppData\\Helium\\User.dat"),
            "FILE_READ_DATA",
        ));
        targets.push(Target::new(
            "file",
            format!(
                "{}\\Microsoft\\Windows\\UsrClass.dat",
                std::env::var("LOCALAPPDATA").map_err(|e| e.to_string())?
            ),
            "FILE_READ_DATA",
        ));
        for key in ["HKLM", "HKCU", &package_registry] {
            targets.push(Target::new("registry", key, "KEY_QUERY_VALUE"));
        }
        // Data-read denial alone says nothing about metadata or zero-access opens.
        // Include these weaker requests before interpreting an open denial as a
        // filesystem boundary.
        for target in targets.clone() {
            if target.kind == "file" || target.kind == "directory" {
                targets.push(Target::new(
                    &format!("{}_query", target.kind),
                    &target.name,
                    "FILE_READ_ATTRIBUTES / GetFileInformationByHandle",
                ));
                targets.push(Target::new(
                    &format!("{}_zero", target.kind),
                    &target.name,
                    "desired access 0 / GetFileInformationByHandle",
                ));
            }
        }
        let mut enumeration = Vec::new();
        let namespace_report = match &namespace_initializer {
            Ok((_, report)) => {
                if let Some(path) = report["registry"].as_str() {
                    targets.push(Target::new("registry_native", path, "KEY_QUERY_VALUE"));
                }
                report.clone()
            }
            Err(e) => json!({"error":e}),
        };
        match fs::read_dir(r"\\.\pipe\") {
            Ok(entries) => {
                let mut count = 0;
                for e in entries {
                    match e {
                        Ok(e) => {
                            count += 1;
                            targets.push(Target::new(
                                "pipe",
                                e.path().to_string_lossy(),
                                "FILE_READ_DATA",
                            ));
                        }
                        Err(e) => {
                            enumeration.push(json!({"namespace":"pipes","error":e.to_string()}))
                        }
                    }
                }
                enumeration.push(json!({"namespace":"pipes","count":count}));
            }
            Err(e) => enumeration.push(json!({"namespace":"pipes","error":e.to_string()})),
        }
        let _fixtures = fixture()?;
        add_directory(r"\RPC Control", &mut targets, &mut enumeration);
        add_directory(r"\BaseNamedObjects", &mut targets, &mut enumeration);
        let mut session = 0;
        check(
            ProcessIdToSessionId(GetCurrentProcessId(), &mut session),
            "ProcessIdToSessionId",
        )?;
        add_directory(
            &format!("\\Sessions\\{session}\\BaseNamedObjects"),
            &mut targets,
            &mut enumeration,
        );
        let mut object_path = [0u16; 2048];
        let mut len = 0;
        let mut package_object_fixtures = None;
        if GetAppContainerNamedObjectPath(
            null_mut(),
            sid,
            object_path.len() as u32,
            object_path.as_mut_ptr(),
            &mut len,
        ) != 0
        {
            let relative = utf16_ptr(object_path.as_ptr());
            let path = if relative.starts_with('\\') {
                relative
            } else {
                format!("\\Sessions\\{session}\\{relative}")
            };
            match package_fixtures(&path) {
                Ok(fixtures) => package_object_fixtures = Some(fixtures),
                Err(e) => enumeration.push(json!({"namespace":"package fixtures","error":e})),
            }
            add_directory(&path, &mut targets, &mut enumeration);
        } else {
            enumeration.push(json!({"namespace":"AppContainerNamedObjects","error":last("GetAppContainerNamedObjectPath")}));
        }
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        udp.set_nonblocking(true).map_err(|e| e.to_string())?;
        let leak_name = format!("{temp}\\win-confine-leaked-{}.txt", GetCurrentProcessId());
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let leaked = Handle::new(
            CreateFileW(
                wide(&leak_name).as_ptr(),
                FILE_WRITE_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            ),
            "CreateFileW(leaked fixture)",
        )?;
        let mut token = null_mut();
        check(
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token),
            "OpenProcessToken(parent)",
        )?;
        let token = Handle(token);
        let mut runs = Vec::new();
        for mode in ["plain", "lpac", "full"] {
            let input = Input {
                mode: mode.into(),
                parent_pid: GetCurrentProcessId(),
                targets: targets.clone(),
                temp: temp.clone(),
                package: package.clone(),
                binary_dir: binary_dir.to_string_lossy().into(),
                package_registry: package_registry.clone(),
                tcp_port: tcp.local_addr().unwrap().port(),
                udp_port: udp.local_addr().unwrap().port(),
                leaked_handle: leaked.0 as usize,
                leaked_name: leak_name.clone(),
            };
            let sequence = if mode == "full" {
                "CreateProcessAsUserW"
            } else {
                "CreateProcessW"
            };
            let result = match launch(
                &input,
                &image.to_string_lossy(),
                sid,
                token.0,
                token.0,
                sequence,
                false,
                false,
                false,
            ) {
                Ok(v) => v,
                Err(e) => json!({"sequence":sequence,"error":e}),
            };
            let mut attempts = vec![result];
            if mode == "full" {
                // Keep all birth attributes, but avoid an impersonation token
                // outranking the primary at startup. Only trusted loader code
                // runs at Low; the child must self-lower before reading input.
                let sequence =
                    "CreateProcessAsUserW / Low primary / NtCreateLowBoxToken loader / self-lower";
                let result = match launch(
                    &input,
                    &image.to_string_lossy(),
                    sid,
                    token.0,
                    token.0,
                    sequence,
                    true,
                    true,
                    false,
                ) {
                    Ok(v) => v,
                    Err(e) => json!({"sequence":sequence,"error":e}),
                };
                attempts.push(result);
            }
            if mode == "full"
                && (attempts.last().unwrap()["exit_code"] != "0x00000000"
                    || attempts.last().unwrap()["child"]["primary_token"]["integrity"]
                        != "S-1-16-0"
                    || attempts.last().unwrap()["parent_before_resume"]["assigned_loader"]["impersonation_level"]
                        != 2)
            {
                // An LPAC-born source also measures whether the initial
                // lowbox's all-application-packages policy must match.
                if let Ok((holder, _)) = &namespace_initializer {
                    let mut source = null_mut();
                    let sequence = "CreateProcessAsUserW / Low primary / same-package LPAC loader / self-lower";
                    let result = if OpenProcessToken(
                        holder.process.0,
                        TOKEN_ALL_ACCESS,
                        &mut source,
                    ) == 0
                    {
                        json!({"sequence":sequence,"error":last("OpenProcessToken(LPAC loader source)")})
                    } else {
                        let source = Handle(source);
                        match launch(
                            &input,
                            &image.to_string_lossy(),
                            sid,
                            token.0,
                            source.0,
                            sequence,
                            true,
                            false,
                            false,
                        ) {
                            Ok(v) => v,
                            Err(e) => json!({"sequence":sequence,"error":e}),
                        }
                    };
                    attempts.push(result);
                }
            }
            // Invalid-handle references may be fatal under strict-handle policy.
            // Isolate the required raw write so it cannot erase all other probes.
            let selected = attempts.last().unwrap();
            let start_low = mode == "full";
            let native_loader = mode == "full"
                && !selected["sequence"]
                    .as_str()
                    .unwrap_or("")
                    .contains("same-package LPAC");
            let mut source = null_mut();
            let source_holder = if mode == "full" && !native_loader {
                if let Ok((holder, _)) = &namespace_initializer {
                    if OpenProcessToken(holder.process.0, TOKEN_ALL_ACCESS, &mut source) != 0 {
                        Some(Handle(source))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let source = source_holder.as_ref().map(|h| h.0).unwrap_or(token.0);
            let mut diagnostics = Vec::new();
            if mode == "full" && selected["exit_code"] == "0xc0000142" {
                if source_holder.is_some() {
                    let result = launch_variant(
                        &input,
                        &image.to_string_lossy(),
                        sid,
                        token.0,
                        source,
                        "loader-trace",
                        true,
                        false,
                        false,
                        MITIGATIONS,
                        0xff,
                    )
                    .unwrap_or_else(|e| json!({"error":e}));
                    diagnostics.push(json!({"label":"full-policy loader snaps / debug event loop","result":result}));
                    let result = match launch_variant(
                        &input,
                        &image.to_string_lossy(),
                        sid,
                        token.0,
                        token.0,
                        "bare-token-control",
                        true,
                        false,
                        false,
                        0,
                        0,
                    ) {
                        Ok(v) => v,
                        Err(e) => json!({"error":e}),
                    };
                    diagnostics.push(json!({"label":"non-lowbox NULL primary / same-access Low loader / no startup attributes","result":result}));
                    let result = match launch_variant(
                        &input,
                        &image.to_string_lossy(),
                        sid,
                        token.0,
                        source,
                        "same-access-primary-control",
                        true,
                        false,
                        false,
                        MITIGATIONS,
                        0xff,
                    ) {
                        Ok(v) => v,
                        Err(e) => json!({"error":e}),
                    };
                    diagnostics.push(json!({"label":"full LPAC policy / primary user+group restricting SIDs instead of NULL","result":result}));
                    let result = match launch_variant(
                        &input,
                        &image.to_string_lossy(),
                        sid,
                        token.0,
                        source,
                        "post-load-primary-control",
                        true,
                        false,
                        false,
                        MITIGATIONS,
                        0xff,
                    ) {
                        Ok(v) => v,
                        Err(e) => json!({"error":e}),
                    };
                    diagnostics.push(json!({"label":"full policy / same-access enabled groups / post-load primary replacement","result":result}));
                }
                for (label, mitigation, ui_flags) in [
                    ("all mitigations off", 0, 0xff),
                    ("dynamic code off", MITIGATIONS & !(1 << 36), 0xff),
                    ("signature off", MITIGATIONS & !(1 << 44), 0xff),
                    ("win32k off", MITIGATIONS & !(1 << 28), 0xff),
                    ("strict handles off", MITIGATIONS & !(1 << 24), 0xff),
                    ("extension points off", MITIGATIONS & !(1 << 32), 0xff),
                    (
                        "image load mitigations off",
                        MITIGATIONS & !((1 << 52) | (1 << 56) | (1 << 60)),
                        0xff,
                    ),
                    ("UI restrictions off", MITIGATIONS, 0),
                    ("mitigations and UI off", 0, 0),
                ] {
                    let result = match launch_variant(
                        &input,
                        &image.to_string_lossy(),
                        sid,
                        token.0,
                        source,
                        label,
                        true,
                        native_loader,
                        false,
                        mitigation,
                        ui_flags,
                    ) {
                        Ok(v) => v,
                        Err(e) => json!({"error":e}),
                    };
                    diagnostics.push(json!({"label":label,"mitigation_mask":format!("0x{mitigation:016x}"),"ui_flags":hex(ui_flags),"result":result}));
                }
            }
            let isolated = match launch(
                &input,
                &image.to_string_lossy(),
                sid,
                token.0,
                source,
                "isolated leaked-handle write",
                start_low,
                native_loader,
                true,
            ) {
                Ok(v) => v,
                Err(e) => json!({"error":e}),
            };
            let mut tcp_count = 0;
            while tcp.accept().is_ok() {
                tcp_count += 1;
            }
            let mut datagrams = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok((n, from)) = udp.recv_from(&mut buffer) {
                datagrams.push(json!({"from":from.to_string(),"payload":String::from_utf8_lossy(&buffer[..n])}));
            }
            runs.push(json!({"mode":mode,"attempts":attempts,"diagnostics":diagnostics,"isolated_leaked_handle":isolated,"loopback_observed":{"tcp_connections":tcp_count,"udp_datagrams":datagrams}}));
        }
        drop(leaked);
        let leaked_content = fs::read(&leak_name).map_err(|e| e.to_string())?;
        fs::remove_file(&leak_name).map_err(|e| e.to_string())?;
        // Successful create probes are state evidence; remove the fixtures only
        // after all modes so later runs could detect their existence if requested.
        for run in &runs {
            for attempt in run["attempts"].as_array().unwrap() {
                if let Some(probes) = attempt["child"]["probes"].as_array() {
                    for p in probes {
                        if p["kind"] == "file_create" && p["success"] == true {
                            if let Some(path) = p["target"].as_str() {
                                fs::remove_file(path).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                }
            }
        }
        drop(package_object_fixtures);
        drop(namespace_initializer);
        let cleanup = profile.api.delete(PROFILE);
        profile.deleted = cleanup >= 0;
        let report = json!({"schema":1,"profile":PROFILE,"profile_cleanup":{"HRESULT":hex(cleanup as u32),"success":cleanup>=0},"system_root":system,"child_image":image.to_string_lossy(),"appcontainer_sid":sid_text,"package_folder":package,"package_registry_target":package_registry,"namespace_initializer":namespace_report,"enumeration":enumeration,"targets":targets,"parent_token":token_attestation(token.0)?,"runs":runs,"leaked_fixture_content":String::from_utf8_lossy(&leaked_content),"elapsed_seconds":started.elapsed().as_secs_f64()});
        let output = std::env::args()
            .skip_while(|a| a != "--output")
            .nth(1)
            .unwrap_or_else(|| "report.json".into());
        fs::write(
            &output,
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        println!("report: {output}");
        if cleanup < 0 {
            return Err(format!(
                "DeleteAppContainerProfile: {}",
                hex(cleanup as u32)
            ));
        }
        let full = report["runs"][2]["attempts"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        if full["exit_code"] != "0x00000000"
            || full["child"]["primary_token"]["lpac"] != true
            || full["child"]["primary_token"]["integrity"] != "S-1-16-0"
        {
            return Err("full-policy child did not complete; inspect report for exact launch or attestation failure".into());
        }
        Ok(())
    }
}
