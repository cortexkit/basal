use crate::native::*;
use serde_json::{Value, json};
use std::{
    fs,
    mem::{size_of, zeroed},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{
        Diagnostics::ToolHelp::*, JobObjects::*, SystemServices::PROCESS_MITIGATION_DEP_POLICY,
        Threading::*,
    },
};

pub unsafe fn mitigations(process: HANDLE) -> Vec<Value> {
    unsafe {
        [
            ("dep", ProcessDEPPolicy, size_of::<PROCESS_MITIGATION_DEP_POLICY>()),
            ("aslr", ProcessASLRPolicy, 4), ("dynamic_code", ProcessDynamicCodePolicy, 4),
            ("strict_handle", ProcessStrictHandleCheckPolicy, 4),
            ("win32k", ProcessSystemCallDisablePolicy, 4),
            ("extension_points", ProcessExtensionPointDisablePolicy, 4),
            ("cfg", ProcessControlFlowGuardPolicy, 4), ("signature", ProcessSignaturePolicy, 4),
            ("font", ProcessFontDisablePolicy, 4), ("image_load", ProcessImageLoadPolicy, 4),
            ("system_call_filter", ProcessSystemCallFilterPolicy, 4),
            ("payload", ProcessPayloadRestrictionPolicy, 4), ("child_process", ProcessChildProcessPolicy, 4),
            ("side_channel", ProcessSideChannelIsolationPolicy, 4),
            ("user_shadow_stack", ProcessUserShadowStackPolicy, 4),
            ("redirection_trust", ProcessRedirectionTrustPolicy, 4),
            ("pointer_auth", ProcessUserPointerAuthPolicy, 4), ("sehop", ProcessSEHOPPolicy, 4),
        ].iter().map(|(name, class, bytes)| {
            let mut flags = [0u32; 4];
            let ok = GetProcessMitigationPolicy(process, *class, flags.as_mut_ptr().cast(), *bytes) != 0;
            json!({"policy":name,"success":ok,"words":&flags[..bytes.div_ceil(4)],"error":if ok {0} else {GetLastError()}})
        }).collect()
    }
}

unsafe fn primary(process: HANDLE) -> Value {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return json!({"error":last("OpenProcessToken(external)")});
        }
        let token = Handle(token);
        token_attestation(token.0).unwrap_or_else(|e| json!({"error":e}))
    }
}

// Toolhelp has no main-thread flag. The earliest surviving creation time is
// recorded, rather than asserting that a thread ID is the original main thread.
unsafe fn main_thread(pid: u32) -> Value {
    unsafe {
        let snapshot = match Handle::new(
            CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0),
            "CreateToolhelp32Snapshot(threads)",
        ) {
            Ok(h) => h,
            Err(e) => return json!({"error":e}),
        };
        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snapshot.0, &mut entry);
        let mut chosen = None;
        let mut errors = Vec::new();
        while ok != 0 {
            if entry.th32OwnerProcessID == pid {
                match Handle::new(
                    OpenThread(THREAD_QUERY_INFORMATION, 0, entry.th32ThreadID),
                    "OpenThread(external)",
                ) {
                    Ok(h) => {
                        let mut creation: FILETIME = zeroed();
                        let mut exit: FILETIME = zeroed();
                        let mut kernel: FILETIME = zeroed();
                        let mut user: FILETIME = zeroed();
                        if GetThreadTimes(h.0, &mut creation, &mut exit, &mut kernel, &mut user)
                            != 0
                        {
                            let time = (u64::from(creation.dwHighDateTime) << 32)
                                | u64::from(creation.dwLowDateTime);
                            if chosen.as_ref().is_none_or(|(old, _, _)| time < *old) {
                                chosen = Some((time, entry.th32ThreadID, h));
                            }
                        } else {
                            errors.push(
                                json!({"tid":entry.th32ThreadID,"error":last("GetThreadTimes")}),
                            );
                        }
                    }
                    Err(e) => errors.push(json!({"tid":entry.th32ThreadID,"error":e})),
                }
            }
            ok = Thread32Next(snapshot.0, &mut entry);
        }
        match chosen {
            None => json!({"error":"no queryable thread","enumeration_errors":errors}),
            Some((time, tid, h)) => {
                let mut token = null_mut();
                let token = if OpenThreadToken(h.0, TOKEN_QUERY, 1, &mut token) == 0 {
                    let error = GetLastError();
                    json!({"present":false,"error":error,"no_token":error == ERROR_NO_TOKEN})
                } else {
                    let token = Handle(token);
                    json!({"present":true,"attestation":token_attestation(token.0).unwrap_or_else(|e|json!({"error":e}))})
                };
                json!({"selection":"earliest surviving thread creation time","tid":tid,"creation_time":time,"token":token,"enumeration_errors":errors})
            }
        }
    }
}

unsafe fn jobs(process: HANDLE, broker_pid: u32) -> Value {
    unsafe {
        let mut member = 0;
        let ok = IsProcessInJob(process, null_mut(), &mut member) != 0;
        let membership =
            json!({"success":ok,"in_any_job":member != 0,"error":if ok {0} else {GetLastError()}});
        let broker = match Handle::new(
            OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE,
                0,
                broker_pid,
            ),
            "OpenProcess(Chrome broker jobs)",
        ) {
            Ok(h) => h,
            Err(e) => return json!({"membership":membership,"error":e}),
        };
        let handles = match handle_table_for(broker.0) {
            Ok(h) => h,
            Err(e) => return json!({"membership":membership,"error":e}),
        };
        let mut matched = Vec::new();
        let mut errors = Vec::new();
        for handle in handles.iter().filter(|h| h["type"] == "Job") {
            let value = handle["handle"].as_u64().unwrap() as usize as HANDLE;
            let mut duplicate = null_mut();
            if DuplicateHandle(
                broker.0,
                value,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                errors.push(json!({"handle":handle,"error":last("DuplicateHandle(Chrome job)")}));
                continue;
            }
            let duplicate = Handle(duplicate);
            let mut in_job = 0;
            if IsProcessInJob(process, duplicate.0, &mut in_job) == 0 {
                errors.push(
                    json!({"handle":handle,"error":last("IsProcessInJob(Chrome exact job)")}),
                );
                continue;
            }
            if in_job == 0 {
                continue;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            let limits_ok = QueryInformationJobObject(
                duplicate.0,
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            ) != 0;
            let limits_error = if limits_ok { 0 } else { GetLastError() };
            let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = zeroed();
            let ui_ok = QueryInformationJobObject(
                duplicate.0,
                JobObjectBasicUIRestrictions,
                (&mut ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                null_mut(),
            ) != 0;
            matched.push(json!({"broker_handle":handle,"exact_membership":true,"limits_query_success":limits_ok,"limits_query_error":limits_error,"limit_flags":hex(limits.BasicLimitInformation.LimitFlags),"active_process_limit":limits.BasicLimitInformation.ActiveProcessLimit,"process_memory_limit":limits.ProcessMemoryLimit,"job_memory_limit":limits.JobMemoryLimit,"ui_query_success":ui_ok,"ui_query_error":if ui_ok {0} else {GetLastError()},"ui_flags":hex(ui.UIRestrictionsClass)}));
        }
        json!({"membership":membership,"matching_broker_jobs":matched,"errors":errors,"coverage":"only jobs whose handles are visible in the Chrome browser; outer runner jobs may be unavailable"})
    }
}

pub fn run() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let number = |key: &str| -> Result<u32> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .ok_or_else(|| format!("missing {key}"))?
            .parse()
            .map_err(|e| format!("{key}: {e}"))
    };
    let pid = number("--observe")?;
    let broker_pid = number("--broker")?;
    let output = args
        .iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .ok_or("missing --output")?;
    let report = unsafe {
        match Handle::new(
            OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid),
            "OpenProcess(external attestation)",
        ) {
            Ok(h) => {
                json!({"pid":pid,"primary_token":primary(h.0),"main_thread":main_thread(pid),"mitigations":mitigations(h.0),"job":jobs(h.0, broker_pid),"handles":handle_table_for(h.0).unwrap_or_else(|e|vec![json!({"error":e})])})
            }
            Err(e) => json!({"pid":pid,"error":e}),
        }
    };
    fs::write(
        output,
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
