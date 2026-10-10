//! Native thread enumeration and impersonation attestation for the confined worker.
#![allow(unsafe_op_in_unsafe_fn)]
use crate::native::*;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::io::{BufRead, Write};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;
fn tokens_absent(thread: &Value) -> bool {
    thread["nt_open_as_self"]["status"] == "0xc000007c"
        && thread["nt_open_as_client"]["status"] == "0xc000007c"
}

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

/// Preserve the complete token attestation, including restricting SIDs and capabilities.
pub unsafe fn query_token_attributes(token: HANDLE) -> Value {
    token_attestation(token).unwrap_or_else(|error| json!({"error": error}))
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
        json!({"has_token": if err_self == ERROR_NO_TOKEN {json!(false)} else {Value::Null}, "error": err_self})
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
        json!({"has_token": if err_client == ERROR_NO_TOKEN {json!(false)} else {Value::Null}, "error": err_client})
    };

    // 3. NtOpenThreadTokenEx with OpenAsSelf = TRUE
    let mut nt_self_t = null_mut();
    let nt_self_st = NtOpenThreadTokenEx(thread, TOKEN_QUERY, 1, 0, &mut nt_self_t);
    let val_nt_self = if nt_self_st == 0 {
        let t = Handle(nt_self_t);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "status": "0x00000000", "token": details})
    } else {
        json!({"has_token": if nt_self_st as u32 == 0xc000007c {json!(false)} else {Value::Null}, "status": hex(nt_self_st as u32)})
    };

    // 4. NtOpenThreadTokenEx with OpenAsSelf = FALSE
    let mut nt_client_t = null_mut();
    let nt_client_st = NtOpenThreadTokenEx(thread, TOKEN_QUERY, 0, 0, &mut nt_client_t);
    let val_nt_client = if nt_client_st == 0 {
        let t = Handle(nt_client_t);
        let details = query_token_attributes(t.0);
        json!({"has_token": true, "status": "0x00000000", "token": details})
    } else {
        json!({"has_token": if nt_client_st as u32 == 0xc000007c {json!(false)} else {Value::Null}, "status": hex(nt_client_st as u32)})
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
            "open_as_self": {"has_token": null, "error": open_err},
            "open_as_client": {"has_token": null, "error": open_err},
            "nt_open_as_self": {"has_token": null, "not_attempted": true},
            "nt_open_as_client": {"has_token": null, "not_attempted": true},
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

/// Enumerate the entire snapshot; an API failure is never an empty process.
pub unsafe fn enumerate_toolhelp_tids(pid: u32) -> Result<Vec<u32>> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CreateToolhelp32Snapshot: Win32 {}",
            GetLastError()
        ));
    }
    let snapshot = Handle(snapshot);
    let mut te: THREADENTRY32 = zeroed();
    te.dwSize = size_of::<THREADENTRY32>() as u32;
    if Thread32First(snapshot.0, &mut te) == 0 {
        return Err(format!("Thread32First: Win32 {}", GetLastError()));
    }
    let mut tids = Vec::new();
    loop {
        if te.th32OwnerProcessID == pid {
            tids.push(te.th32ThreadID);
        }
        te.dwSize = size_of::<THREADENTRY32>() as u32;
        if Thread32Next(snapshot.0, &mut te) == 0 {
            let error = GetLastError();
            if error != ERROR_NO_MORE_FILES {
                return Err(format!("Thread32Next: Win32 {error}"));
            }
            break;
        }
    }
    tids.sort_unstable();
    Ok(tids)
}

/// Read NumberOfThreads and every SYSTEM_THREAD_INFORMATION entry for the PID.
/// Offsets are the x64 ABI; bounds are checked against the returned record length.
pub unsafe fn system_process_threads(pid: u32) -> Result<Vec<(u32, usize)>> {
    let mut bytes = 2 * 1024 * 1024;
    loop {
        let mut buffer = vec![0usize; bytes / size_of::<usize>()];
        let mut returned = 0u32;
        let status =
            NtQuerySystemInformation(5, buffer.as_mut_ptr().cast(), bytes as u32, &mut returned);
        if status as u32 == 0xc0000004 && bytes < 32 * 1024 * 1024 {
            bytes = (bytes * 2)
                .max(returned as usize + 8192)
                .next_multiple_of(8);
            continue;
        }
        if status < 0 {
            return Err(format!(
                "NtQuerySystemInformation(5): {}",
                hex(status as u32)
            ));
        }
        let base = buffer.as_ptr().cast::<u8>();
        let end = returned as usize;
        if end > bytes {
            return Err("SystemProcessInformation returned oversized buffer".into());
        }
        let mut offset = 0usize;
        while offset + 256 <= end {
            let next = base.add(offset).cast::<u32>().read_unaligned() as usize;
            let count = base.add(offset + 4).cast::<u32>().read_unaligned() as usize;
            let record_end = if next == 0 { end } else { offset + next };
            if record_end > end || record_end < offset + 256 {
                return Err("Invalid process record size".into());
            }
            let process_id = base.add(offset + 80).cast::<usize>().read_unaligned();
            if process_id == pid as usize {
                if offset + 256 + count * 80 > record_end {
                    return Err("Truncated thread array".into());
                }
                let mut threads = Vec::with_capacity(count);
                for i in 0..count {
                    let thread = base.add(offset + 256 + i * 80);
                    let start = thread.add(32).cast::<usize>().read_unaligned();
                    let owner = thread.add(40).cast::<usize>().read_unaligned();
                    let tid = thread.add(48).cast::<usize>().read_unaligned();
                    if owner != pid as usize || tid == 0 {
                        return Err("Invalid thread CLIENT_ID".into());
                    }
                    threads.push((tid as u32, start));
                }
                return Ok(threads);
            }
            if next == 0 {
                break;
            }
            offset = record_end;
        }
        return Err(format!("PID {pid} missing from SystemProcessInformation"));
    }
}

/// Require two independent enumerators to agree; never add the observer's own TID.
pub unsafe fn inspect_all_threads(pid: u32) -> Result<Value> {
    let nt = system_process_threads(pid);
    let toolhelp = enumerate_toolhelp_tids(pid);
    let mut tids = std::collections::BTreeSet::new();
    if let Ok(entries) = &nt {
        tids.extend(entries.iter().map(|e| e.0));
    }
    if let Ok(entries) = &toolhelp {
        tids.extend(entries.iter().copied());
    }
    let nt_tids: std::collections::BTreeSet<_> = nt
        .as_ref()
        .map(|entries| entries.iter().map(|e| e.0).collect())
        .unwrap_or_default();
    let complete = nt.is_ok()
        && toolhelp.is_ok()
        && !nt_tids.is_empty()
        && nt_tids == toolhelp.as_ref().unwrap().iter().copied().collect();
    let mut threads = Vec::new();
    for tid in tids {
        let mut info = inspect_single_thread(
            tid,
            pid == GetCurrentProcessId() && tid == GetCurrentThreadId(),
        );
        if let Ok(entries) = &nt {
            if let Some((_, start)) = entries.iter().find(|e| e.0 == tid) {
                info["nt_start_address"] = json!(format!("0x{start:016x}"));
            }
        }
        threads.push(info);
    }
    let all_no_token = complete && threads.iter().all(tokens_absent);
    let any_token = threads.iter().any(|t| {
        t["nt_open_as_self"]["has_token"] == true || t["nt_open_as_client"]["has_token"] == true
    });
    Ok(json!({
        "enumeration_complete": complete,
        "thread_count": threads.len(),
        "process_reported_thread_count": nt.as_ref().ok().map(Vec::len),
        "nt_enumeration_error": nt.as_ref().err(),
        "toolhelp_error": toolhelp.as_ref().err(),
        "discovered_toolhelp_tids": toolhelp.as_ref().ok(),
        "all_threads_have_no_impersonation_token": all_no_token,
        "any_token_found": any_token,
        "threads": threads,
    }))
}

struct PoolContext {
    entered: AtomicUsize,
    departed: AtomicUsize,
    ready: HANDLE,
    release: HANDLE,
    records: Mutex<Vec<Value>>,
}

unsafe fn record_callback(param: *mut c_void, kind: &str) {
    let ctx = &*(param as *const PoolContext);
    let (s, c, ns, nc) = query_thread_tokens(GetCurrentThread());
    ctx.records
        .lock()
        .unwrap()
        .push(json!({"kind":kind,"tid":GetCurrentThreadId(),
        "open_as_self":s,"open_as_client":c,"nt_open_as_self":ns,"nt_open_as_client":nc}));
    if ctx.entered.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
        SetEvent(ctx.ready);
    }
    // Keep the callbacks alive until both enumerators and token queries have run.
    WaitForSingleObject(ctx.release, INFINITE);
    ctx.departed.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "system" fn work_callback(_: PTP_CALLBACK_INSTANCE, ctx: *mut c_void, _: PTP_WORK) {
    record_callback(ctx, "work");
}
unsafe extern "system" fn wait_callback(
    _: PTP_CALLBACK_INSTANCE,
    ctx: *mut c_void,
    _: PTP_WAIT,
    _: u32,
) {
    record_callback(ctx, "wait");
}
unsafe extern "system" fn timer_callback(_: PTP_CALLBACK_INSTANCE, ctx: *mut c_void, _: PTP_TIMER) {
    record_callback(ctx, "timer");
}

/// The confined token cannot enumerate the system on all supported Windows builds.
/// Request broker TIDs, start addresses and token queries over newline-delimited
/// JSON on stdio. Only this spike understands those messages; the broker withholds
/// the untrusted probe payload until the worker finishes every checkpoint.
fn observer_checkpoint(stage: &str) -> Result<Value> {
    println!("{}", json!({"mode":"thread_checkpoint","stage":stage}));
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut reply = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&reply).map_err(|e| e.to_string())
}

/// Exercise work, a signaled wait and a relative timer on the default pool.
/// The context remains alive until all callbacks have drained, even on timeout.
pub unsafe fn force_pool_activity(use_observer: bool) -> Result<Value> {
    let ready = Handle(CreateEventW(null(), 1, 0, null()));
    let release = Handle(CreateEventW(null(), 1, 0, null()));
    let trigger = Handle(CreateEventW(null(), 1, 1, null()));
    if ready.0.is_null() || release.0.is_null() || trigger.0.is_null() {
        return Err(last("CreateEventW(pool)"));
    }
    let ctx = PoolContext {
        entered: AtomicUsize::new(0),
        departed: AtomicUsize::new(0),
        ready: ready.0,
        release: release.0,
        records: Mutex::new(Vec::new()),
    };
    let param = (&ctx as *const PoolContext).cast_mut().cast();
    let work = CreateThreadpoolWork(Some(work_callback), param, null());
    let wait = CreateThreadpoolWait(Some(wait_callback), param, null());
    let timer = CreateThreadpoolTimer(Some(timer_callback), param, null());
    if work == 0 || wait == 0 || timer == 0 {
        let error = last("CreateThreadpool work/wait/timer");
        if work != 0 {
            CloseThreadpoolWork(work);
        }
        if wait != 0 {
            CloseThreadpoolWait(wait);
        }
        if timer != 0 {
            CloseThreadpoolTimer(timer);
        }
        return Err(error);
    }
    SubmitThreadpoolWork(work);
    SetThreadpoolWait(wait, trigger.0, null());
    let due = (-10_000i64).to_ne_bytes();
    let due = FILETIME {
        dwLowDateTime: u32::from_ne_bytes(due[..4].try_into().unwrap()),
        dwHighDateTime: u32::from_ne_bytes(due[4..].try_into().unwrap()),
    };
    SetThreadpoolTimer(timer, &due, 0, 0);
    let ready_status = WaitForSingleObject(ready.0, 10_000);
    let observer = if use_observer {
        observer_checkpoint("callbacks_held")
    } else {
        Ok(Value::Null)
    };
    let local = inspect_all_threads(GetCurrentProcessId());
    let records = ctx.records.lock().unwrap().clone();
    let held = ctx.entered.load(Ordering::SeqCst) == 3 && ctx.departed.load(Ordering::SeqCst) == 0;
    SetEvent(release.0);
    SetThreadpoolWait(wait, null_mut(), null());
    SetThreadpoolTimer(timer, null(), 0, 0);
    WaitForThreadpoolWorkCallbacks(work, 1);
    WaitForThreadpoolWaitCallbacks(wait, 1);
    WaitForThreadpoolTimerCallbacks(timer, 1);
    CloseThreadpoolWork(work);
    CloseThreadpoolWait(wait);
    CloseThreadpoolTimer(timer);
    Ok(
        json!({"ready_status":ready_status,"callbacks_held_during_snapshot":held,"callback_inspections":records,
        "observer":observer?,"local":local?}),
    )
}

/// Attest the live thread set before and after forcing pool activity.
pub unsafe fn measure_thread_impersonation(pid: u32) -> Result<Value> {
    let local_pre = inspect_all_threads(pid)?;
    let pre = observer_checkpoint("before_activity")?;
    let activity = force_pool_activity(true)?;
    let post = activity["observer"].clone();
    let callbacks = activity["callback_inspections"].as_array().unwrap();
    let tids: std::collections::BTreeSet<_> = post["threads"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|t| t["tid"].as_u64())
        .collect();
    let included = callbacks
        .iter()
        .all(|c| c["tid"].as_u64().is_some_and(|tid| tids.contains(&tid)));
    let complete = activity["ready_status"] == 0
        && activity["callbacks_held_during_snapshot"] == true
        && callbacks.len() == 3
        && included
        && pre["all_threads_have_no_impersonation_token"] == true
        && post["all_threads_have_no_impersonation_token"] == true
        && callbacks.iter().all(tokens_absent);
    let post_release = observer_checkpoint("after_release")?;
    let complete = complete && post_release["all_threads_have_no_impersonation_token"] == true;
    Ok(
        json!({"local_pre_pool_activity":local_pre,"pre_pool_activity":pre,
        "forced_pool_activity":activity,"post_pool_activity":post,"post_release":post_release,
        "callback_tids_in_enumerated_set":included,"pass":complete,
        "conclusion":if complete {"complete enumerations and every thread has STATUS_NO_TOKEN"} else {"incomplete or token-bearing: no confinement claim"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_enumerators_include_the_current_thread() {
        unsafe {
            let pid = GetCurrentProcessId();
            let tid = GetCurrentThreadId();
            assert!(
                system_process_threads(pid)
                    .unwrap()
                    .iter()
                    .any(|entry| entry.0 == tid)
            );
            assert!(enumerate_toolhelp_tids(pid).unwrap().contains(&tid));
        }
    }

    #[test]
    fn system_enumerator_rejects_a_missing_pid() {
        unsafe {
            assert!(system_process_threads(u32::MAX).is_err());
        }
    }

    #[test]
    fn denied_token_query_is_unknown_not_absent() {
        let denied = json!({"nt_open_as_self":{"status":"0xc0000022"},"nt_open_as_client":{"status":"0xc000007c"}});
        assert!(!tokens_absent(&denied));
    }

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

/// Test removing every observed token conservatively; do not classify unreadable
/// or partly attested tokens as weaker than the worker's primary.
pub unsafe fn remove_observed_tokens(pid: u32, before: &Value) -> Value {
    let mut attempts = Vec::new();
    for thread in before["threads"].as_array().unwrap_or(&Vec::new()) {
        if thread["nt_open_as_self"]["has_token"] != true
            && thread["nt_open_as_client"]["has_token"] != true
        {
            continue;
        }
        let tid = thread["tid"].as_u64().unwrap() as u32;
        let handle = OpenThread(THREAD_SET_THREAD_TOKEN, 0, tid);
        if handle.is_null() {
            attempts.push(json!({"tid":tid,"open_error":GetLastError()}));
            continue;
        }
        let handle = Handle(handle);
        let success = SetThreadToken(&handle.0, null_mut()) != 0;
        attempts.push(json!({"tid":tid,"set_thread_token_null_success":success,"error":if success {0} else {GetLastError()}}));
    }
    json!({"attempts":attempts,"remeasured":inspect_all_threads(pid).unwrap_or_else(|error|json!({"error":error}))})
}
