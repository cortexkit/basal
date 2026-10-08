use crate::native::*;
use serde_json::{Value, json};
use std::{mem::zeroed, time::Instant};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Diagnostics::Debug::*, Threading::*},
};

unsafe fn image_path(file: HANDLE) -> Value {
    unsafe {
        if file.is_null() {
            return Value::Null;
        }
        let mut path = vec![0u16; 32768];
        let n = GetFinalPathNameByHandleW(file, path.as_mut_ptr(), path.len() as u32, 0);
        let result = if n == 0 || n as usize >= path.len() {
            json!({"error":last("GetFinalPathNameByHandleW(debug DLL)")})
        } else {
            json!(String::from_utf16_lossy(&path[..n as usize]))
        };
        CloseHandle(file);
        result
    }
}

pub unsafe fn loader_flag(process: HANDLE) -> Value {
    unsafe {
        // The spike is an x64 image. Reading the suspended PEB verifies that the
        // image-specific registry setting reached this process, not just HKLM.
        let mut basic = [0usize; 6];
        let mut returned = 0;
        let status = NtQueryInformationProcess(
            process,
            0,
            basic.as_mut_ptr().cast(),
            std::mem::size_of_val(&basic) as u32,
            &mut returned,
        );
        if status < 0 {
            return json!({"error":hex(status as u32)});
        }
        let mut flags = 0u32;
        let mut read = 0;
        let ok = ReadProcessMemory(
            process,
            (basic[1] + 0xbc) as *const _,
            (&mut flags as *mut u32).cast(),
            4,
            &mut read,
        ) != 0;
        json!({"architecture":"x64","peb_ntglobalflag_offset":"0xbc","read_success":ok,"error":if ok {0} else {GetLastError()},"flags":hex(flags),"loader_snaps_enabled":ok && read == 4 && flags & 2 != 0})
    }
}

// The creating thread must pump debug events. Pipe readers run independently
// because a child that reaches entry can write more than one pipe buffer.
pub unsafe fn collect(process: HANDLE, pid: u32) -> Value {
    unsafe {
        let started = Instant::now();
        let mut events = Vec::new();
        let mut completed = false;
        let initial_loader_flag = loader_flag(process);
        let mut startup_breakpoint_seen = false;
        let mut errors = Vec::new();
        while started.elapsed().as_secs() < 180 {
            let mut event: DEBUG_EVENT = zeroed();
            if WaitForDebugEventEx(&mut event, 1000) == 0 {
                let error = GetLastError();
                if error == ERROR_SEM_TIMEOUT {
                    continue;
                }
                errors.push(json!({"api":"WaitForDebugEventEx","error":error}));
                break;
            }
            let mut disposition = DBG_CONTINUE;
            let detail = match event.dwDebugEventCode {
                CREATE_PROCESS_DEBUG_EVENT => {
                    let info = event.u.CreateProcessInfo;
                    // Windows closes debug-event process/thread handles when
                    // their exit events are continued; only hFile is ours to close.
                    json!({"kind":"create_process","base":info.lpBaseOfImage as usize,"path":image_path(info.hFile)})
                }
                LOAD_DLL_DEBUG_EVENT => {
                    let info = event.u.LoadDll;
                    json!({"kind":"load_dll","base":info.lpBaseOfDll as usize,"path":image_path(info.hFile),"loader_flag":loader_flag(process)})
                }
                UNLOAD_DLL_DEBUG_EVENT => {
                    json!({"kind":"unload_dll","base":event.u.UnloadDll.lpBaseOfDll as usize})
                }
                OUTPUT_DEBUG_STRING_EVENT => {
                    let info = event.u.DebugString;
                    let unicode = info.fUnicode != 0;
                    let mut bytes =
                        vec![0u8; info.nDebugStringLength as usize * if unicode { 2 } else { 1 }];
                    let mut read = 0;
                    let ok = ReadProcessMemory(
                        process,
                        info.lpDebugStringData.cast(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                        &mut read,
                    ) != 0;
                    let error = if ok { 0 } else { GetLastError() };
                    bytes.truncate(read);
                    let text = if unicode {
                        let words = bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]]))
                            .collect::<Vec<_>>();
                        String::from_utf16_lossy(&words)
                    } else {
                        String::from_utf8_lossy(&bytes).into_owned()
                    };
                    json!({"kind":"debug_string","unicode":unicode,"text":text.trim_end_matches('\0'),"read_success":ok,"read_error":error,"bytes_read":read,"bytes_requested":info.nDebugStringLength as usize * if unicode {2} else {1}})
                }
                EXCEPTION_DEBUG_EVENT => {
                    let info = event.u.Exception;
                    let code = info.ExceptionRecord.ExceptionCode;
                    // Only the debugger's startup breakpoint is consumed; real
                    // faults must retain the child's normal exception behavior.
                    if code == EXCEPTION_BREAKPOINT && !startup_breakpoint_seen {
                        startup_breakpoint_seen = true;
                    } else {
                        disposition = DBG_EXCEPTION_NOT_HANDLED;
                    }
                    json!({"kind":"exception","code":hex(code as u32),"first_chance":info.dwFirstChance,"address":info.ExceptionRecord.ExceptionAddress as usize})
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    completed = true;
                    json!({"kind":"exit_process","exit_code":hex(event.u.ExitProcess.dwExitCode)})
                }
                CREATE_THREAD_DEBUG_EVENT => json!({"kind":"create_thread"}),
                EXIT_THREAD_DEBUG_EVENT => {
                    json!({"kind":"exit_thread","exit_code":hex(event.u.ExitThread.dwExitCode)})
                }
                code => json!({"kind":"other","event_code":code}),
            };
            events.push(json!({"pid":event.dwProcessId,"tid":event.dwThreadId,"event":detail}));
            if ContinueDebugEvent(event.dwProcessId, event.dwThreadId, disposition) == 0 {
                errors.push(json!({"api":"ContinueDebugEvent","error":GetLastError()}));
                break;
            }
            if completed {
                break;
            }
        }
        if !completed {
            TerminateProcess(process, 124);
            DebugActiveProcessStop(pid);
            WaitForSingleObject(process, 5000);
        }
        json!({"completed":completed,"initial_loader_flag":initial_loader_flag,"errors":errors,"events":events})
    }
}
