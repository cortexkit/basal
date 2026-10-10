//! Parent-only startup handle diagnostics. These calls never send an ALPC message.
use crate::native::*;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;
}
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandle {
    object: usize,
    pid: usize,
    handle: usize,
    access: u32,
    backtrace: u16,
    type_index: u16,
    attributes: u32,
    reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TraceEntry {
    handle: usize,
    pid: usize,
    tid: usize,
    kind: u32,
    stacks: [usize; 16],
}

pub fn enable_tracing(process: HANDLE) -> Value {
    let settings = [0u32, 16384];
    let status = unsafe { NtSetInformationProcess(process, 32, settings.as_ptr().cast(), 8) };
    json!({"class":32,"slots":16384,"NTSTATUS":hex(status as u32)})
}

fn system_handles() -> Result<Vec<SystemHandle>> {
    unsafe {
        let mut bytes = 1 << 20;
        loop {
            let mut buffer = vec![0usize; bytes / size_of::<usize>()];
            let mut returned = 0;
            let status = NtQuerySystemInformation(
                64,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                &mut returned,
            );
            if status as u32 == 0xc0000004 && bytes < (1 << 27) {
                bytes = (bytes * 2).max(returned as usize + 4096);
                bytes = bytes.next_multiple_of(size_of::<usize>());
                continue;
            }
            if status < 0 {
                return Err(format!(
                    "SystemExtendedHandleInformation: {}",
                    hex(status as u32)
                ));
            }
            let count = buffer[0];
            if count > (bytes - 16) / size_of::<SystemHandle>() {
                return Err("system handle count exceeds buffer".into());
            }
            return Ok(std::slice::from_raw_parts(
                buffer.as_ptr().add(2).cast::<SystemHandle>(),
                count,
            )
            .to_vec());
        }
    }
}
fn image_name(process: HANDLE) -> Value {
    unsafe {
        let mut name = vec![0u16; 32768];
        let mut size = name.len() as u32;
        if QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut size) == 0 {
            json!({"error":GetLastError()})
        } else {
            json!(String::from_utf16_lossy(&name[..size as usize]))
        }
    }
}

// Kernel pointer disclosure and foreign-process queries may require
// SeDebugPrivilege even for an administrator. Restore its previous state.
struct DebugPrivilege {
    token: Handle,
    previous: TOKEN_PRIVILEGES,
}
impl DebugPrivilege {
    fn enable() -> Result<Self> {
        unsafe {
            let mut token = null_mut();
            check(
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut token,
                ),
                "OpenProcessToken(debug privilege)",
            )?;
            let token = Handle(token);
            let mut luid = zeroed();
            check(
                LookupPrivilegeValueW(null(), wide("SeDebugPrivilege").as_ptr(), &mut luid),
                "LookupPrivilegeValueW",
            )?;
            let wanted = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = zeroed();
            let mut returned = 0;
            SetLastError(0);
            check(
                AdjustTokenPrivileges(
                    token.0,
                    0,
                    &wanted,
                    size_of::<TOKEN_PRIVILEGES>() as u32,
                    &mut previous,
                    &mut returned,
                ),
                "AdjustTokenPrivileges",
            )?;
            if GetLastError() != 0 {
                return Err(last("AdjustTokenPrivileges(SeDebugPrivilege)"));
            }
            Ok(Self { token, previous })
        }
    }
}
impl Drop for DebugPrivilege {
    fn drop(&mut self) {
        unsafe {
            AdjustTokenPrivileges(self.token.0, 0, &self.previous, 0, null_mut(), null_mut());
        }
    }
}

pub struct LoaderSetting {
    path: Vec<u16>,
    pub report: Value,
}
impl Drop for LoaderSetting {
    fn drop(&mut self) {
        unsafe {
            RegDeleteKeyW(HKEY_LOCAL_MACHINE, self.path.as_ptr());
        }
    }
}
pub fn loader_setting(count: u32) -> Result<LoaderSetting> {
    unsafe {
        let path = wide(
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options\\win-confine-gui.exe",
        );
        let mut key = null_mut();
        let mut disposition = 0;
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            null(),
            &mut key,
            &mut disposition,
        );
        if status != 0 {
            return Err(format!("RegCreateKeyExW(MaxLoaderThreads): {status}"));
        }
        if disposition != REG_CREATED_NEW_KEY {
            RegCloseKey(key);
            return Err("refusing existing GUI IFEO key".into());
        }
        let setting = LoaderSetting {
            path,
            report: json!({"MaxLoaderThreads":count,"diagnostic_only":true}),
        };
        let status = RegSetValueExW(
            key,
            wide("MaxLoaderThreads").as_ptr(),
            0,
            REG_DWORD,
            (&count as *const u32).cast(),
            4,
        );
        let mut readback = 0u32;
        let mut size = 4;
        let read_status = RegQueryValueExW(
            key,
            wide("MaxLoaderThreads").as_ptr(),
            null(),
            null_mut(),
            (&mut readback as *mut u32).cast(),
            &mut size,
        );
        RegCloseKey(key);
        if status != 0 || read_status != 0 || readback != count {
            return Err(format!(
                "MaxLoaderThreads write/read: {status}/{read_status}/{readback}"
            ));
        }
        Ok(setting)
    }
}

#[repr(C)]
struct SymbolInfo {
    size: u32,
    type_index: u32,
    reserved: [u64; 2],
    index: u32,
    symbol_size: u32,
    module: u64,
    flags: u32,
    value: u64,
    address: u64,
    register: u32,
    scope: u32,
    tag: u32,
    name_len: u32,
    max_name_len: u32,
    name: [u8; 1024],
}
type Initialize = unsafe extern "system" fn(HANDLE, *const u16, i32) -> i32;
type FromAddress = unsafe extern "system" fn(HANDLE, u64, *mut u64, *mut SymbolInfo) -> i32;
type Cleanup = unsafe extern "system" fn(HANDLE) -> i32;
type ModuleInfo = unsafe extern "system" fn(HANDLE, u64, *mut c_void) -> i32;
struct Symbols {
    library: HMODULE,
    process: HANDLE,
    from_address: FromAddress,
    cleanup: Cleanup,
}
impl Symbols {
    fn new(process: HANDLE) -> Result<Self> {
        unsafe {
            let image = std::env::var("BASAL_DBGHELP").unwrap_or_else(|_| "dbghelp.dll".into());
            if let Ok(server) = std::env::var("BASAL_SYMSRV") {
                LoadLibraryExW(
                    wide(&server).as_ptr(),
                    null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
                );
            }
            let library = LoadLibraryExW(
                wide(&image).as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32
                    | if image.contains('\\') {
                        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
                    } else {
                        0
                    },
            );
            if library.is_null() {
                return Err(last("LoadLibraryExW(dbghelp)"));
            }
            let exports = (
                GetProcAddress(library, c"SymInitializeW".as_ptr().cast()),
                GetProcAddress(library, c"SymFromAddr".as_ptr().cast()),
                GetProcAddress(library, c"SymCleanup".as_ptr().cast()),
            );
            let (Some(init), Some(from), Some(cleanup)) = exports else {
                FreeLibrary(library);
                return Err("DbgHelp exports unavailable".into());
            };
            let init: Initialize = std::mem::transmute(init);
            let cache = std::env::current_dir()
                .map_err(|error| error.to_string())?
                .join("evidence")
                .join("symbols");
            let path = wide(&format!(
                "srv*{}*https://msdl.microsoft.com/download/symbols",
                cache.display()
            ));
            if init(process, path.as_ptr(), 1) == 0 {
                let error = last("SymInitializeW");
                FreeLibrary(library);
                return Err(error);
            }
            Ok(Self {
                library,
                process,
                from_address: std::mem::transmute(from),
                cleanup: std::mem::transmute(cleanup),
            })
        }
    }
    fn address(&self, address: usize) -> Value {
        unsafe {
            let mut symbol: SymbolInfo = zeroed();
            // SYMBOL_INFO has an 88-byte ABI header; the name buffer is variable.
            symbol.size = 88;
            symbol.max_name_len = 1024;
            let mut displacement = 0;
            if (self.from_address)(self.process, address as u64, &mut displacement, &mut symbol)
                == 0
            {
                return json!({"address":format!("0x{address:016x}"),"error":GetLastError()});
            }
            let module = GetProcAddress(self.library, c"SymGetModuleInfo64".as_ptr().cast()).map(|export| {
                let query: ModuleInfo = std::mem::transmute(export);
                let mut buffer = [0u64; 210];
                buffer[0] = 1680;
                if query(self.process, address as u64, buffer.as_mut_ptr().cast()) == 0 { return json!({"error":GetLastError()}); }
                let bytes = std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), 1680);
                let text = |start: usize, length: usize| {
                    let value = &bytes[start..start + length];
                    String::from_utf8_lossy(&value[..value.iter().position(|byte| *byte == 0).unwrap_or(length)]).into_owned()
                };
                json!({"base":format!("0x{:016x}",buffer[1]),"symbol_type":u32::from_le_bytes(bytes[32..36].try_into().unwrap()),"name":text(36,32),"pdb":text(580,256)})
            });
            json!({"address":format!("0x{address:016x}"),"symbol":String::from_utf8_lossy(&symbol.name[..(symbol.name_len as usize).min(1024)]),"displacement":displacement,"module_base":format!("0x{:016x}",symbol.module),"module":module})
        }
    }
}
impl Drop for Symbols {
    fn drop(&mut self) {
        unsafe {
            (self.cleanup)(self.process);
            FreeLibrary(self.library);
        }
    }
}
fn traces(process: HANDLE, symbols: Option<&Symbols>, handles: &[Value]) -> Value {
    unsafe {
        let mut buffer = vec![0usize; (16 + 16384 * size_of::<TraceEntry>()) / 8];
        let mut returned = 0;
        let status = NtQueryInformationProcess(
            process,
            32,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut returned,
        );
        if status < 0 {
            return json!({"NTSTATUS":hex(status as u32),"returned":returned});
        }
        let count = *buffer.as_ptr().add(1).cast::<u32>() as usize;
        if count > (buffer.len() * 8 - 16) / size_of::<TraceEntry>() {
            return json!({"error":"trace count exceeds buffer","count":count});
        }
        let entries =
            std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<TraceEntry>(), count);
        let mut cache = std::collections::BTreeMap::new();
        let relevant: Vec<_> = entries.iter().filter(|entry| handles.iter().any(|handle| handle["handle"].as_u64() == Some(entry.handle as u64))).map(|entry| {
            let stack: Vec<_> = entry.stacks.iter().copied().filter(|address| *address != 0).map(|address| cache.entry(address).or_insert_with(|| symbols.map(|symbols| symbols.address(address)).unwrap_or_else(|| json!({"address":format!("0x{address:016x}")}))).clone()).collect();
            json!({"handle":entry.handle,"pid":entry.pid,"tid":entry.tid,"operation":entry.kind,"stack":stack})
        }).collect();
        json!({"NTSTATUS":hex(status as u32),"total_traces":count,"entries_for_current_handles":relevant})
    }
}

pub fn inspect(process: HANDLE, main_thread: HANDLE, pid: u32, inventory: &Value) -> Value {
    let modules = unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
        if snapshot == INVALID_HANDLE_VALUE {
            json!({"error":GetLastError()})
        } else {
            let snapshot = Handle(snapshot);
            let mut entry: MODULEENTRY32W = zeroed();
            entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
            let mut modules = Vec::new();
            if Module32FirstW(snapshot.0, &mut entry) != 0 {
                loop {
                    modules.push(json!({"name":utf16_ptr(entry.szModule.as_ptr()),"path":utf16_ptr(entry.szExePath.as_ptr()),"base":format!("0x{:016x}",entry.modBaseAddr as usize),"bytes":entry.modBaseSize}));
                    if Module32NextW(snapshot.0, &mut entry) == 0 {
                        break;
                    }
                }
            }
            json!(modules)
        }
    };
    let privilege = DebugPrivilege::enable();
    let privilege_report = privilege
        .as_ref()
        .map(|_| json!({"enabled":true}))
        .unwrap_or_else(|error| json!({"error":error}));
    let handles = inventory["ambient_close"]["before"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let symbols = Symbols::new(process);
    let symbol_setup = symbols
        .as_ref()
        .map(|_| json!({"initialized":true,"server":"https://msdl.microsoft.com/download/symbols"}))
        .unwrap_or_else(|error| json!({"error":error}));
    let trace_report = traces(process, symbols.as_ref().ok(), &handles);
    let system = system_handles();
    let alpc = match system {
        Err(error) => json!({"error":error}),
        Ok(ref entries) => {
            let correlations: Vec<_> = entries.iter().filter(|entry| entry.pid == pid as usize && handles.iter().any(|handle| handle["handle"].as_u64() == Some(entry.handle as u64))).map(|entry| {
                let foreign: Vec<_> = entries.iter().filter(|other| entry.object != 0 && other.object == entry.object && other.pid != entry.pid).map(|other| json!({"pid":other.pid,"handle":other.handle,"access":hex(other.access)})).collect();
                json!({"handle":entry.handle,"object":format!("0x{:016x}",entry.object),"foreign_handles":foreign})
            }).collect();
            let worker: Vec<_> = entries
                .iter()
                .filter(|entry| {
                    entry.pid == pid as usize
                        && handles.iter().any(|handle| {
                            handle["type"] == "ALPC Port"
                                && handle["handle"].as_u64() == Some(entry.handle as u64)
                        })
                })
                .copied()
                .collect();
            let identified: Vec<_> = worker.iter().map(|client| {
                let mut session = [0u32; 2];
                let mut returned = 0;
                let mut duplicate = null_mut();
                let duplicated = unsafe { DuplicateHandle(process, client.handle as HANDLE, GetCurrentProcess(), &mut duplicate, 0, 0, DUPLICATE_SAME_ACCESS) } != 0;
                let mut server_info = vec![0usize; 128];
                let (session_status, server_status) = if duplicated {
                    let duplicate = Handle(duplicate);
                    server_info[0] = main_thread as usize;
                    unsafe { (NtAlpcQueryInformation(duplicate.0, 12, session.as_mut_ptr().cast(), 8, &mut returned), NtAlpcQueryInformation(null_mut(), 4, server_info.as_mut_ptr().cast(), (server_info.len() * 8) as u32, &mut returned)) }
                } else { (-1, -1) };
                let peer_pid = if session_status >= 0 { session[1] as usize } else { 0 };
                let matches: Vec<_> = entries.iter().filter(|entry| entry.object == client.object && entry.pid != pid as usize).map(|entry| json!({"pid":entry.pid,"handle":entry.handle,"object":format!("0x{:016x}",entry.object)})).collect();
                let peer = unsafe { OpenProcess(PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION, 0, peer_pid as u32) };
                let peer_info = if peer.is_null() {
                    let duplicate_error = unsafe { GetLastError() };
                    let query = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, peer_pid as u32) };
                    let image = if query.is_null() { json!({"error":unsafe {GetLastError()}}) } else { let query = Handle(query); image_name(query.0) };
                    json!({"pid":peer_pid,"image":image,"duplicate_open_error":duplicate_error})
                } else {
                    let peer = Handle(peer);
                    let ports: Vec<_> = entries.iter().filter(|entry| entry.pid == peer_pid && entry.type_index == client.type_index).map(|entry| json!({"handle":entry.handle,"object":format!("0x{:016x}",entry.object),"access":hex(entry.access),"identity":remote_alpc_name(peer.0, entry.handle as HANDLE)})).collect();
                    json!({"pid":peer_pid,"image":image_name(peer.0),"alpc_ports":ports})
                };
                json!({"handle":client.handle,"object":format!("0x{:016x}",client.object),"same_object_foreign_handles":matches,"server_session":{"NTSTATUS":hex(session_status as u32),"session_id":session[0],"process_id":session[1]},"server_information":{"NTSTATUS":hex(server_status as u32),"thread_blocked":if server_status >= 0 {Some(server_info[0] & 0xff != 0)} else {None},"connected_pid":if server_status >= 0 {Some(server_info[1])} else {None}},"peer":peer_info})
            }).collect();
            json!({"system_handle_count":entries.len(),"worker_ports":identified,"worker_object_correlations":correlations})
        }
    };
    let factories: Vec<_> = handles
        .iter()
        .filter(|handle| handle["type"] == "TpWorkerFactory")
        .map(|handle| {
            json!({
                "handle": handle["handle"],
                "basic": remote_worker_factory(
                    process,
                    handle["handle"].as_u64().unwrap_or(0) as HANDLE,
                )
            })
        })
        .collect();

    let symbolize_addr = |addr_val: &Value| -> Value {
        if let Some(s) = addr_val.as_str() {
            if let Ok(addr) = usize::from_str_radix(s.trim_start_matches("0x"), 16) {
                if let Some(syms) = symbols.as_ref().ok() {
                    return syms.address(addr);
                }
            }
        }
        Value::Null
    };

    let symbolize_thread_list = |threads_json: &Value| -> Vec<Value> {
        threads_json
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|t| {
                        let mut item = t.clone();
                        let win32_sym = symbolize_addr(&t["win32_start_address"]);
                        let nt_sym = symbolize_addr(&t["nt_start_address"]);
                        item["win32_symbol"] = win32_sym;
                        item["nt_symbol"] = nt_sym;
                        item
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let threads_report = if inventory.get("threads").is_some() {
        let pre = symbolize_thread_list(&inventory["threads"]["pre_pool_activity"]["threads"]);
        let post = symbolize_thread_list(&inventory["threads"]["post_pool_activity"]["threads"]);
        let parent_observed = unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap != INVALID_HANDLE_VALUE {
                let snap = Handle(snap);
                let mut te: THREADENTRY32 = zeroed();
                te.dwSize = size_of::<THREADENTRY32>() as u32;
                let mut observed = Vec::new();
                if Thread32First(snap.0, &mut te) != 0 {
                    loop {
                        if te.th32OwnerProcessID == pid {
                            let th = OpenThread(THREAD_QUERY_INFORMATION, 0, te.th32ThreadID);
                            if !th.is_null() {
                                let th = Handle(th);
                                let mut t_self = null_mut();
                                let ok_self =
                                    OpenThreadToken(th.0, TOKEN_QUERY, 1, &mut t_self) != 0;
                                let err_self = if ok_self { 0 } else { GetLastError() };
                                let self_val = if ok_self {
                                    let t = Handle(t_self);
                                    crate::threads::query_token_attributes(t.0)
                                } else {
                                    json!({"error": err_self})
                                };
                                let mut t_client = null_mut();
                                let ok_client =
                                    OpenThreadToken(th.0, TOKEN_QUERY, 0, &mut t_client) != 0;
                                let err_client = if ok_client { 0 } else { GetLastError() };
                                let client_val = if ok_client {
                                    let t = Handle(t_client);
                                    crate::threads::query_token_attributes(t.0)
                                } else {
                                    json!({"error": err_client})
                                };
                                observed.push(json!({
                                    "tid": te.th32ThreadID,
                                    "parent_open_as_self": {"has_token": ok_self, "token": self_val},
                                    "parent_open_as_client": {"has_token": ok_client, "token": client_val},
                                }));
                            } else {
                                observed.push(json!({
                                    "tid": te.th32ThreadID,
                                    "open_thread_error": GetLastError(),
                                }));
                            }
                        }
                        if Thread32Next(snap.0, &mut te) == 0 {
                            break;
                        }
                    }
                }
                json!(observed)
            } else {
                json!({"error": GetLastError()})
            }
        };

        json!({
            "pre_pool_threads_symbolized": pre,
            "post_pool_threads_symbolized": post,
            "parent_observed_threads": parent_observed,
            "pre_pool_all_no_token": inventory["threads"]["pre_activity_all_no_token"],
            "post_pool_all_no_token": inventory["threads"]["post_activity_all_no_token"],
            "conclusion": inventory["threads"]["conclusion"],
        })
    } else {
        Value::Null
    };

    let scheduler_report = if let Ok(ref entries) = system {
        let sched_handle = handles
            .iter()
            .find(|h| h["type"] == "SchedulerSharedData")
            .and_then(|h| h["handle"].as_u64());

        if let Some(sh) = sched_handle {
            let match_entry = entries
                .iter()
                .find(|e| e.pid == pid as usize && e.handle == sh as usize);
            let obj_addr = match_entry.map(|e| e.object).unwrap_or(0);
            let foreign: Vec<_> = entries
                .iter()
                .filter(|other| obj_addr != 0 && other.object == obj_addr && other.pid != pid as usize)
                .map(|other| json!({"pid": other.pid, "handle": other.handle, "access": hex(other.access)}))
                .collect();

            let ldr_analysis = if let Some(_syms) = symbols.as_ref().ok() {
                let frame_addr = trace_report["entries"].as_array().and_then(|entries| {
                    entries
                        .iter()
                        .find(|e| e["handle"].as_u64() == Some(sh))
                        .and_then(|e| {
                            e["trace"].as_array().and_then(|frames| {
                                frames.iter().find_map(|f| {
                                    if f["symbol"].as_str()
                                        == Some("LdrpAllocateSchedulerSharedData")
                                    {
                                        f["address"].as_str().and_then(|s| {
                                            usize::from_str_radix(s.trim_start_matches("0x"), 16)
                                                .ok()
                                        })
                                    } else {
                                        None
                                    }
                                })
                            })
                        })
                });

                if let Some(addr) = frame_addr {
                    let mut code = [0u8; 64];
                    let mut read_bytes = 0usize;
                    let read_ok = unsafe {
                        ReadProcessMemory(
                            process,
                            addr as *const c_void,
                            code.as_mut_ptr().cast(),
                            code.len(),
                            &mut read_bytes,
                        ) != 0
                    };
                    json!({
                        "function": "LdrpAllocateSchedulerSharedData",
                        "address": format!("0x{:016x}", addr),
                        "read_success": read_ok,
                        "raw_bytes": hex_bytes(&code[..read_bytes]),
                        "process_information_class": 112,
                        "class_name": "ProcessSchedulerSharedData",
                    })
                } else {
                    json!({"note": "creation frame not in trace or already resolved"})
                }
            } else {
                json!({"error": "symbols unavailable"})
            };

            json!({
                "present": true,
                "handle": sh,
                "object": format!("0x{:016x}", obj_addr),
                "foreign_handles": foreign,
                "ldr_analysis": ldr_analysis,
                "is_shared_across_processes": !foreign.is_empty(),
            })
        } else {
            json!({"present": false, "reason": "absent on Windows Server 2022"})
        }
    } else {
        json!({"error": "system handles unavailable"})
    };

    json!({
        "privilege": privilege_report,
        "modules": modules,
        "symbols": symbol_setup,
        "tracing": trace_report,
        "alpc": alpc,
        "worker_factories": factories,
        "loader_threads_after_load": loader_threads(process, None),
        "threads": threads_report,
        "scheduler_shared_data": scheduler_report,
        "messages_sent": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_diagnostic_layouts_match_x64_abi() {
        assert_eq!(size_of::<SystemHandle>(), 40);
        assert_eq!(size_of::<TraceEntry>(), 160);
        assert_eq!(std::mem::offset_of!(TraceEntry, stacks), 32);
        assert_eq!(std::mem::offset_of!(SymbolInfo, name), 84);
    }
}

// LoaderThreads is an undocumented x64 process-parameter field. Record a read
// for every recipe; only the explicitly labelled diagnostic writes it.
// LoaderThreads is an undocumented x64 process-parameter field. Record a read
// for every recipe; only the explicitly labelled diagnostic writes it.
pub fn loader_threads(process: HANDLE, requested: Option<u32>) -> Value {
    unsafe {
        let result = (|| {
            let mut basic = [0usize; 6];
            let status =
                NtQueryInformationProcess(process, 0, basic.as_mut_ptr().cast(), 48, null_mut());
            if status < 0 {
                return Err(format!("ProcessBasicInformation: {}", hex(status as u32)));
            }
            let mut parameters = 0usize;
            let mut read = 0;
            check(
                ReadProcessMemory(
                    process,
                    (basic[1] + 0x20) as *const c_void,
                    (&mut parameters as *mut usize).cast(),
                    8,
                    &mut read,
                ),
                "ReadProcessMemory(process parameters)",
            )?;
            let address = parameters + 0x40c;
            let mut before = 0u32;
            check(
                ReadProcessMemory(
                    process,
                    address as *const c_void,
                    (&mut before as *mut u32).cast(),
                    4,
                    &mut read,
                ),
                "ReadProcessMemory(LoaderThreads)",
            )?;
            if let Some(count) = requested {
                check(
                    WriteProcessMemory(
                        process,
                        address as *mut c_void,
                        (&count as *const u32).cast(),
                        4,
                        &mut read,
                    ),
                    "WriteProcessMemory(LoaderThreads)",
                )?;
            }
            let mut after = 0u32;
            check(
                ReadProcessMemory(
                    process,
                    address as *const c_void,
                    (&mut after as *mut u32).cast(),
                    4,
                    &mut read,
                ),
                "ReadProcessMemory(LoaderThreads readback)",
            )?;
            if requested.is_some_and(|count| count != after) {
                return Err("LoaderThreads readback mismatch".into());
            }
            Ok::<_, String>(
                json!({"offset":"0x40c","before":before,"requested":requested,"after":after,"diagnostic_only":requested.is_some()}),
            )
        })();
        result.unwrap_or_else(|error| json!({"error":error}))
    }
}
