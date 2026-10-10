//! Native thread enumeration and impersonation attestation for the confined worker.
use crate::native::*;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;

    fn NtQueryInformationThread(
        thread_handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;

    fn NtOpenThreadTokenEx(
        thread_handle: HANDLE,
        access: u32,
        open_as_self: u8,
        handle_attributes: u32,
        token_handle: *mut HANDLE,
    ) -> i32;
}

unsafe extern "system" {
    fn QueueUserWorkItem(
        function: Option<unsafe extern "system" fn(*mut c_void) -> u32>,
        context: *mut c_void,
        flags: u32,
    ) -> i32;
}

/// Query detailed security attributes of an impersonation token.
pub unsafe fn query_token_attributes(token: HANDLE) -> Value {
    let mut details = json!({});

    // Token type (Primary = 1, Impersonation = 2)
    if let Ok(data) = token_buffer(token, TokenType) {
        let ttype = *data.as_ptr().cast::<u32>();
        details["token_type"] = json!(ttype);
        details["token_type_name"] = json!(if ttype == 1 {
            "Primary"
        } else if ttype == 2 {
            "Impersonation"
        } else {
            "Unknown"
        });
    }

    // Impersonation level
    if let Ok(data) = token_buffer(token, TokenImpersonationLevel) {
        let level = *data.as_ptr().cast::<u32>();
        details["impersonation_level"] = json!(level);
        details["impersonation_level_name"] = json!(match level {
            0 => "SecurityAnonymous",
            1 => "SecurityIdentification",
            2 => "SecurityImpersonation",
            3 => "SecurityDelegation",
            _ => "Unknown",
        });
    }

    // Integrity level
    if let Ok(data) = token_buffer(token, TokenIntegrityLevel) {
        let sid = (*data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let sid_str = sid_string(sid);
        details["integrity"] = json!(sid_str);
        details["integrity_level"] = json!(if sid_str == "S-1-16-0" {
            "Untrusted"
        } else if sid_str == "S-1-16-4096" {
            "Low"
        } else if sid_str == "S-1-16-8192" {
            "Medium"
        } else if sid_str == "S-1-16-12288" {
            "High"
        } else if sid_str == "S-1-16-16384" {
            "System"
        } else {
            "Other"
        });
    }

    // AppContainer SID
    if let Ok(data) = token_buffer(token, TokenAppContainerSid) {
        let ac_sid = (*data.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>()).TokenAppContainer;
        if !ac_sid.is_null() {
            details["appcontainer_sid"] = json!(sid_string(ac_sid));
        } else {
            details["appcontainer_sid"] = Value::Null;
        }
    }

    // User SID
    if let Ok(data) = token_buffer(token, TokenUser) {
        let user_sid = (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        details["user"] = json!(sid_string(user_sid));
    }

    // Enabled groups
    if let Ok(data) = token_buffer(token, TokenGroups) {
        let grps = groups(&data);
        let mut enabled_groups = Vec::new();
        let mut all_groups = Vec::new();
        for g in grps {
            let s = sid_string(g.Sid);
            let is_enabled = (g.Attributes & SE_GROUP_ENABLED) != 0;
            let is_deny_only = (g.Attributes & 0x00000010) != 0; // SE_GROUP_USE_FOR_DENY_ONLY
            let is_integrity = (g.Attributes & SE_GROUP_INTEGRITY) != 0;
            if !is_integrity {
                all_groups.push(json!({
                    "sid": s,
                    "attributes": hex(g.Attributes),
                    "enabled": is_enabled,
                    "deny_only": is_deny_only,
                }));
                if is_enabled && !is_deny_only {
                    enabled_groups.push(s);
                }
            }
        }
        details["enabled_groups"] = json!(enabled_groups);
        details["all_groups"] = json!(all_groups);
    }

    // Privileges
    if let Ok(data) = token_buffer(token, TokenPrivileges) {
        let p = &*data.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let mut privs = Vec::new();
        for entry in std::slice::from_raw_parts(p.Privileges.as_ptr(), p.PrivilegeCount as usize) {
            privs.push(json!({
                "luid": entry.Luid,
                "attributes": hex(entry.Attributes),
                "enabled": (entry.Attributes & SE_PRIVILEGE_ENABLED) != 0,
            }));
        }
        details["privileges"] = json!(privs);
    }

    details
}

/// Query impersonation token on a given thread handle via both Win32 and native NT interfaces.
unsafe fn query_thread_tokens(thread: HANDLE) -> (Value, Value, Value, Value) {
    // 1. OpenThreadToken with OpenAsSelf = TRUE
    let mut t_self = null_mut();
    let ok_self = OpenThreadToken(thread, TOKEN_QUERY, 1, &mut t_self) != 0;
    let err_self = if ok_self { 0 } else { GetLastError() };
    let val_self = if ok_self {
        let t = Handle(t_self);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "error": 0, "token": details})
    } else {
        json!({"has_token": false, "error": err_self})
    };

    // 2. OpenThreadToken with OpenAsSelf = FALSE
    let mut t_client = null_mut();
    let ok_client = OpenThreadToken(thread, TOKEN_QUERY, 0, &mut t_client) != 0;
    let err_client = if ok_client { 0 } else { GetLastError() };
    let val_client = if ok_client {
        let t = Handle(t_client);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "error": 0, "token": details})
    } else {
        json!({"has_token": false, "error": err_client})
    };

    // 3. NtOpenThreadTokenEx with OpenAsSelf = TRUE
    let mut nt_self_t = null_mut();
    let nt_self_st = NtOpenThreadTokenEx(thread, TOKEN_QUERY, 1, 0, &mut nt_self_t);
    let val_nt_self = if nt_self_st == 0 {
        let t = Handle(nt_self_t);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "status": "0x00000000", "token": details})
    } else {
        json!({"has_token": false, "status": hex(nt_self_st as u32)})
    };

    // 4. NtOpenThreadTokenEx with OpenAsSelf = FALSE
    let mut nt_client_t = null_mut();
    let nt_client_st = NtOpenThreadTokenEx(thread, TOKEN_QUERY, 0, 0, &mut nt_client_t);
    let val_nt_client = if nt_client_st == 0 {
        let t = Handle(nt_client_t);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "status": "0x00000000", "token": details})
    } else {
        json!({"has_token": false, "status": hex(nt_client_st as u32)})
    };

    (val_self, val_client, val_nt_self, val_nt_client)
}

/// Inspect a single thread by TID, including start addresses and token queries.
pub unsafe fn inspect_single_thread(tid: u32, is_current: bool) -> Value {
    let (thread_h, open_err) = if is_current {
        (GetCurrentThread(), 0)
    } else {
        let h = OpenThread(THREAD_QUERY_INFORMATION, 0, tid);
        if !h.is_null() {
            (h, 0)
        } else {
            let err = GetLastError();
            let limited = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
            if !limited.is_null() {
                (limited, err)
            } else {
                (null_mut(), GetLastError())
            }
        }
    };

    if thread_h.is_null() {
        return json!({
            "tid": tid,
            "is_current": is_current,
            "open_thread_error": open_err,
            "open_as_self": {"has_token": false, "error": open_err},
            "open_as_client": {"has_token": false, "error": open_err},
            "nt_open_as_self": {"has_token": false, "status": hex(open_err)},
            "nt_open_as_client": {"has_token": false, "status": hex(open_err)},
        });
    }

    // Query Win32 start address
    let mut win32_start = 0usize;
    let mut ret_len = 0u32;
    let nt_start_status = NtQueryInformationThread(
        thread_h,
        9, // ThreadQuerySetWin32StartAddress
        (&mut win32_start as *mut usize).cast(),
        size_of::<usize>() as u32,
        &mut ret_len,
    );

    let (self_res, client_res, nt_self, nt_client) = query_thread_tokens(thread_h);

    if !is_current {
        CloseHandle(thread_h);
    }

    json!({
        "tid": tid,
        "is_current": is_current,
        "win32_start_address": if nt_start_status == 0 {
            json!(format!("0x{:016x}", win32_start))
        } else {
            json!({"status": hex(nt_start_status as u32)})
        },
        "open_as_self": self_res,
        "open_as_client": client_res,
        "nt_open_as_self": nt_self,
        "nt_open_as_client": nt_client,
    })
}

/// Enumerate process thread IDs using Toolhelp snapshot.
pub unsafe fn enumerate_toolhelp_tids(pid: u32) -> Result<Vec<u32>> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD): {}",
            GetLastError()
        ));
    }
    let snapshot = Handle(snapshot);
    let mut te: THREADENTRY32 = zeroed();
    te.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut tids = Vec::new();
    if Thread32First(snapshot.0, &mut te) != 0 {
        loop {
            if te.th32OwnerProcessID == pid {
                tids.push(te.th32ThreadID);
            }
            if Thread32Next(snapshot.0, &mut te) == 0 {
                break;
            }
        }
    }
    Ok(tids)
}

/// Enumerate process threads and NT start addresses using NtQuerySystemInformation (SystemProcessInformation = 5).
pub unsafe fn system_process_threads(pid: u32) -> Result<Vec<(u32, usize)>> {
    let mut bytes = 2 * 1024 * 1024; // 2 MB
    loop {
        let mut buffer = vec![0u8; bytes];
        let mut returned = 0u32;
        let status =
            NtQuerySystemInformation(5, buffer.as_mut_ptr().cast(), bytes as u32, &mut returned);
        if status as u32 == 0xc0000004 && bytes < (32 * 1024 * 1024) {
            bytes = (bytes * 2).max(returned as usize + 8192);
            continue;
        }
        if status < 0 {
            return Err(format!(
                "NtQuerySystemInformation(5): {}",
                hex(status as u32)
            ));
        }
        let mut offset = 0usize;
        let mut threads = Vec::new();
        while offset < buffer.len() {
            let next_offset = *buffer.as_ptr().add(offset).cast::<u32>() as usize;
            let thread_count = *buffer.as_ptr().add(offset + 4).cast::<u32>() as usize;
            let proc_id = *buffer.as_ptr().add(offset + 80).cast::<usize>() as u32;
            if proc_id == pid {
                // In x64 SYSTEM_PROCESS_INFORMATION, the Threads array starts at offset 256 (0x100).
                let thread_base = offset + 256;
                for i in 0..thread_count {
                    let entry_offset = thread_base + i * 80;
                    if entry_offset + 80 <= buffer.len() {
                        let start_addr = *buffer.as_ptr().add(entry_offset + 32).cast::<usize>();
                        let tid = *buffer.as_ptr().add(entry_offset + 48).cast::<usize>() as u32;
                        threads.push((tid, start_addr));
                    }
                }
                return Ok(threads);
            }
            if next_offset == 0 {
                break;
            }
            offset += next_offset;
        }
        return Ok(threads);
    }
}

/// Enumerate and inspect all threads belonging to the given PID.
pub unsafe fn inspect_all_threads(pid: u32) -> Result<Value> {
    let current_tid = GetCurrentThreadId();
    let nt_threads = system_process_threads(pid).unwrap_or_default();
    let toolhelp_tids = enumerate_toolhelp_tids(pid).unwrap_or_default();

    // Union of all thread IDs discovered
    let mut all_tids = std::collections::BTreeSet::new();
    all_tids.insert(current_tid);
    for &(tid, _) in &nt_threads {
        all_tids.insert(tid);
    }
    for &tid in &toolhelp_tids {
        all_tids.insert(tid);
    }

    let mut threads = Vec::new();
    let mut any_token_found = false;
    let mut any_stronger_token = false;

    for &tid in &all_tids {
        let mut info = inspect_single_thread(tid, tid == current_tid);
        if let Some(&(_, start_addr)) = nt_threads.iter().find(|&&(t, _)| t == tid) {
            info["nt_start_address"] = json!(format!("0x{:016x}", start_addr));
        }

        let has_self = info["open_as_self"]["has_token"] == true;
        let has_client = info["open_as_client"]["has_token"] == true;
        let has_nt_self = info["nt_open_as_self"]["has_token"] == true;
        let has_nt_client = info["nt_open_as_client"]["has_token"] == true;

        if has_self || has_client || has_nt_self || has_nt_client {
            any_token_found = true;
            // Check if stronger than Untrusted primary:
            // Untrusted integrity is "S-1-16-0". Anything with higher integrity,
            // non-empty enabled groups, or privileges is considered stronger.
            let token_data = if has_self {
                &info["open_as_self"]["token"]
            } else if has_client {
                &info["open_as_client"]["token"]
            } else if has_nt_self {
                &info["nt_open_as_self"]["token"]
            } else {
                &info["nt_open_as_client"]["token"]
            };
            let integrity = token_data["integrity"].as_str().unwrap_or("");
            let enabled_groups = token_data["enabled_groups"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            let privileges = token_data["privileges"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            if integrity != "S-1-16-0" || enabled_groups > 0 || privileges > 0 {
                any_stronger_token = true;
            }
        }

        threads.push(info);
    }

    Ok(json!({
        "thread_count": threads.len(),
        "discovered_toolhelp_tids": toolhelp_tids,
        "discovered_nt_thread_count": nt_threads.len(),
        "all_threads_have_no_impersonation_token": !any_token_found,
        "any_token_found": any_token_found,
        "any_stronger_token": any_stronger_token,
        "threads": threads,
    }))
}

struct PoolContext {
    remaining: AtomicUsize,
    done_event: HANDLE,
    records: Mutex<Vec<Value>>,
}

unsafe extern "system" fn pool_callback(param: *mut c_void) -> u32 {
    let ctx = &*(param as *const PoolContext);
    let tid = GetCurrentThreadId();
    let (self_res, client_res, nt_self, nt_client) = query_thread_tokens(GetCurrentThread());

    let has_token = self_res["has_token"] == true
        || client_res["has_token"] == true
        || nt_self["has_token"] == true
        || nt_client["has_token"] == true;

    let mut remedy = json!({"remedy_needed": false});
    if has_token {
        // If a thread carries an impersonation token, test the remedy in the same run:
        // Drop it via SetThreadToken(NULL) from that thread.
        let ok = SetThreadToken(null(), null_mut()) != 0;
        let err = if ok { 0 } else { GetLastError() };
        let (after_self, after_client, after_nt_self, after_nt_client) =
            query_thread_tokens(GetCurrentThread());
        remedy = json!({
            "remedy_needed": true,
            "set_thread_token_null_success": ok,
            "set_thread_token_null_error": err,
            "after_remedy": {
                "open_as_self": after_self,
                "open_as_client": after_client,
                "nt_open_as_self": after_nt_self,
                "nt_open_as_client": after_nt_client,
            }
        });
    }

    if let Ok(mut records) = ctx.records.lock() {
        records.push(json!({
            "tid": tid,
            "open_as_self": self_res,
            "open_as_client": client_res,
            "nt_open_as_self": nt_self,
            "nt_open_as_client": nt_client,
            "remedy": remedy,
        }));
    }

    let left = ctx.remaining.fetch_sub(1, Ordering::SeqCst);
    if left == 1 {
        SetEvent(ctx.done_event);
    }
    0
}

/// Force pool activity by queuing multiple work items to the default thread pool,
/// inspecting tokens from inside pool callbacks, and waiting for completion.
pub unsafe fn force_pool_activity(count: usize) -> Result<Value> {
    let event = CreateEventW(null(), 1, 0, null());
    if event.is_null() {
        return Err(format!("CreateEventW: {}", GetLastError()));
    }
    let event_h = Handle(event);
    let ctx = Arc::new(PoolContext {
        remaining: AtomicUsize::new(count),
        done_event: event_h.0,
        records: Mutex::new(Vec::new()),
    });

    let mut queued = 0;
    for _ in 0..count {
        let ptr = Arc::as_ptr(&ctx) as *mut c_void;
        if QueueUserWorkItem(Some(pool_callback), ptr, 0) != 0 {
            queued += 1;
        }
    }

    // Wait up to 10 seconds for all work items to complete
    let wait = WaitForSingleObject(event_h.0, 10_000);
    // Brief sleep to allow worker threads to settle back into pool
    Sleep(50);

    let executed = ctx.records.lock().map(|r| r.clone()).unwrap_or_default();
    Ok(json!({
        "work_items_requested": count,
        "work_items_queued": queued,
        "wait_status": wait,
        "executed_count": executed.len(),
        "callback_inspections": executed,
    }))
}

/// Complete pre-input thread impersonation measurement:
/// 1. Enumerate and inspect every thread before input.
/// 2. Force pool activity (wake/spawn dormant pool threads and inspect from worker).
/// 3. Re-enumerate and inspect all threads.
/// 4. Verify that either every thread has no impersonation token or tokens are no more privileged than Untrusted primary.
pub unsafe fn measure_thread_impersonation(pid: u32) -> Result<Value> {
    eprintln!("thread-attestation: starting pre-pool enumeration");
    let pre_activity = inspect_all_threads(pid)?;

    eprintln!("thread-attestation: forcing pool activity (8 work items)");
    let pool_activity = force_pool_activity(8)?;

    eprintln!("thread-attestation: starting post-pool re-enumeration");
    let post_activity = inspect_all_threads(pid)?;

    let pre_clean = pre_activity["all_threads_have_no_impersonation_token"] == true;
    let post_clean = post_activity["all_threads_have_no_impersonation_token"] == true;
    let any_stronger =
        pre_activity["any_stronger_token"] == true || post_activity["any_stronger_token"] == true;

    let conclusion = if pre_clean && post_clean {
        "verified: every thread has no impersonation token (pre-input and post-pool activity)"
    } else if !any_stronger {
        "verified: all observed thread tokens are no more privileged than Untrusted primary"
    } else {
        "gap: thread carries stronger token"
    };

    eprintln!("thread-attestation: complete. conclusion: {conclusion}");

    Ok(json!({
        "pre_pool_activity": pre_activity,
        "forced_pool_activity": pool_activity,
        "post_pool_activity": post_activity,
        "pre_activity_all_no_token": pre_clean,
        "post_activity_all_no_token": post_clean,
        "any_stronger_token": any_stronger,
        "conclusion": conclusion,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_query_current_thread_returns_valid_structure() {
        unsafe {
            let res = inspect_single_thread(GetCurrentThreadId(), true);
            assert_eq!(res["is_current"], true);
            assert!(res["open_as_self"].is_object());
            assert!(res["open_as_client"].is_object());
            assert!(res["nt_open_as_self"].is_object());
            assert!(res["nt_open_as_client"].is_object());
        }
    }
}
