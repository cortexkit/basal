use crate::native::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::{Console::*, Diagnostics::ToolHelp::*, Memory::*, Registry::*, Threading::*},
};

fn mitigations() -> Vec<Value> {
    [
        ("dynamic_code",ProcessDynamicCodePolicy), ("signature",ProcessSignaturePolicy),
        ("image_load",ProcessImageLoadPolicy), ("win32k",ProcessSystemCallDisablePolicy),
        ("strict_handle",ProcessStrictHandleCheckPolicy), ("extension_points",ProcessExtensionPointDisablePolicy),
        ("child_process",ProcessChildProcessPolicy),
    ].iter().map(|(name,class)| unsafe {
        let mut flags = 0u32;
        let ok = GetProcessMitigationPolicy(GetCurrentProcess(),*class,(&mut flags as *mut u32).cast(),size_of::<u32>()) != 0;
        json!({"policy":name,"success":ok,"flags":hex(flags),"error":if ok {0} else {GetLastError()}})
    }).collect()
}
fn loaded_modules() -> Result<Vec<Value>> {
    unsafe {
        let snapshot = Handle::new(
            CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId()),
            "CreateToolhelp32Snapshot",
        )?;
        let mut entry: MODULEENTRY32W = zeroed();
        entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
        check(Module32FirstW(snapshot.0, &mut entry), "Module32FirstW")?;
        let mut modules = Vec::new();
        loop {
            modules.push(json!({"name":utf16_ptr(entry.szModule.as_ptr()),"path":utf16_ptr(entry.szExePath.as_ptr())}));
            if Module32NextW(snapshot.0, &mut entry) == 0 {
                break;
            }
        }
        Ok(modules)
    }
}
fn file_probe(t: &Target, creation: bool) -> Probe {
    unsafe {
        let access = if creation {
            FILE_WRITE_DATA
        } else if t.kind.ends_with("_query") {
            FILE_READ_ATTRIBUTES
        } else if t.kind.ends_with("_zero") {
            0
        } else if t.kind == "directory" {
            FILE_LIST_DIRECTORY
        } else {
            FILE_READ_DATA
        };
        let h = CreateFileW(
            wide(&t.name).as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            if creation { CREATE_NEW } else { OPEN_EXISTING },
            if t.kind.starts_with("directory") {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            },
            null_mut(),
        );
        let ok = h != INVALID_HANDLE_VALUE;
        let error = GetLastError();
        let mut result = Probe::win(t, ok, error);
        if ok {
            result.result["granted_access"] = granted_access(h);
            if t.kind.ends_with("_query") || t.kind.ends_with("_zero") {
                let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
                let queried = GetFileInformationByHandle(h, &mut info) != 0;
                let error = GetLastError();
                result.result["metadata"] = json!({"success":queried,"error":if queried{0}else{error},"attributes":hex(info.dwFileAttributes),"size_high":info.nFileSizeHigh,"size_low":info.nFileSizeLow});
            }
            drop(Handle(h));
        }
        result
    }
}
fn registry_probe(t: &Target) -> Probe {
    unsafe {
        let (root, path) = if t.name == "HKLM" {
            (HKEY_LOCAL_MACHINE, String::new())
        } else if t.name == "HKCU" {
            (HKEY_CURRENT_USER, String::new())
        } else {
            (
                HKEY_CURRENT_USER,
                t.name.strip_prefix("HKCU\\").unwrap_or(&t.name).to_string(),
            )
        };
        let mut h = null_mut();
        let error = RegOpenKeyExW(root, wide(&path).as_ptr(), 0, KEY_QUERY_VALUE, &mut h);
        let mut probe = Probe::win(t, error == 0, error);
        if error == 0 {
            probe.result["granted_access"] = if h as isize > 0 && h as usize % 4 == 0 {
                granted_access(h)
            } else {
                json!({"not_queried":"predefined or tagged Win32 registry handle"})
            };
            RegCloseKey(h);
        }
        probe
    }
}
unsafe extern "system" fn thread_entry(_: *mut std::ffi::c_void) -> u32 {
    0
}
fn win_probe(kind: &str, name: &str, access: &str, ok: bool, error: u32) -> Probe {
    Probe::win(&Target::new(kind, name, access), ok, error)
}
pub fn run() -> Result<()> {
    unsafe {
        eprintln!("probe-stage: Rust entry");
        // No untrusted input is consumed under the loader's more permissive token.
        let lower_requested = std::env::args().any(|a| a == "--lower-integrity");
        let replacement_requested = std::env::args().any(|a| a == "--replace-primary");
        let close_ambient = std::env::args().any(|a| a == "--close-ambient");
        // Borrow only the primary's adjustment right under the loader token.
        // No token handle with that right may survive into the input phase.
        let adjustment = if lower_requested {
            let mut h = null_mut();
            check(
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY
                        | TOKEN_ADJUST_DEFAULT
                        | if replacement_requested {
                            TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY
                        } else {
                            0
                        },
                    &mut h,
                ),
                "OpenProcessToken(self-lower before revert)",
            )?;
            Some(Handle(h))
        } else {
            None
        };
        check(RevertToSelf(), "RevertToSelf")?;
        let mut thread_token = null_mut();
        let still_impersonating =
            OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut thread_token) != 0;
        let no_token_error = if still_impersonating {
            drop(Handle(thread_token));
            0
        } else {
            GetLastError()
        };
        if still_impersonating || no_token_error != ERROR_NO_TOKEN {
            return Err(format!(
                "lockdown incomplete: OpenThreadToken={no_token_error}"
            ));
        }
        let self_lowering = if let Some(h) = adjustment {
            let before = token_buffer(h.0, TokenIntegrityLevel)?;
            let before = sid_string((*before.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid);
            let replacement = if replacement_requested {
                replace_primary_locked(h.0)
            } else {
                Value::Null
            };
            set_integrity(h.0, WinUntrustedLabelSid)?;
            drop(h);
            json!({"requested":true,"integrity_before":before,"set_error":0,"adjustment_handle_closed_before_input":true,"primary_replacement":replacement})
        } else {
            json!({"requested":false})
        };
        let mut token = null_mut();
        let token_opened = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0;
        let token_open_error = if token_opened { 0 } else { GetLastError() };
        // The current-process token pseudo-handle needs no DACL open. Its
        // authority is only this process's token query, never token adjustment.
        let attestation = if token_opened {
            let h = Handle(token);
            token_attestation(h.0)
        } else {
            token_attestation(-4isize as HANDLE)
        }
        .unwrap_or_else(|e| json!({"error":e}));
        // Snapshot before input and all probes: probe-created sockets must not masquerade as inherited authority.
        let handles = handle_table()?;
        let no_token_handles = token_handles_absent(&handles);
        if !no_token_handles {
            return Err("token handle or unidentified handle survived lockdown".into());
        }
        // Name every handle present before input, then close ALPC, Directory and
        // non-stdio File handles. Event, completion, worker-factory, timer and
        // wait-packet handles stay open: closing them is a separate measurement
        // because the loader may still use them. The second snapshot shows
        // whether each close removed that handle before the first input byte.
        let ambient = if close_ambient {
            let stdio = [
                GetStdHandle(STD_INPUT_HANDLE),
                GetStdHandle(STD_OUTPUT_HANDLE),
                GetStdHandle(STD_ERROR_HANDLE),
            ];
            Some(identify_and_close(
                &stdio,
                &["ALPC Port", "Directory", "File"],
            )?)
        } else {
            None
        };
        let input_handles = ambient
            .as_ref()
            .and_then(|ambient| ambient["after"].as_array().cloned())
            .unwrap_or(handles);
        let modules = loaded_modules()
            .map(|v| json!(v))
            .unwrap_or_else(|e| json!({"error":e}));
        let policies = mitigations();
        eprintln!("probe-stage: attestation complete");
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| e.to_string())?;
        let input: Input = serde_json::from_str(&input).map_err(|e| e.to_string())?;
        let mut probes = Vec::new();
        let leak_only = std::env::args().any(|a| a == "--probe-leak");
        eprintln!("probe-stage: input complete; leak_only={leak_only}");
        if !leak_only {
            for t in &input.targets {
                probes.push(match t.kind.as_str() {
                    "file" | "directory" | "file_query" | "directory_query" | "file_zero"
                    | "directory_zero" | "pipe" => file_probe(t, false),
                    "registry" => registry_probe(t),
                    _ => object_probe(t),
                });
            }
            for dir in [&input.temp, &input.package, &input.binary_dir] {
                let path = format!(
                    "{dir}\\win-confine-{}-{}.txt",
                    GetCurrentProcessId(),
                    input.mode
                );
                probes.push(file_probe(
                    &Target::new("file_create", path, "FILE_WRITE_DATA / CREATE_NEW"),
                    true,
                ));
            }
            let mut hive = null_mut();
            for target in &input.targets {
                if target.kind == "registry_native" {
                    probes.push(registry_write_native(&target.name));
                }
            }
            // The broker resolves the package's actual storage key before spawning.
            let path = input
                .package_registry
                .strip_prefix("HKCU\\")
                .unwrap_or(&input.package_registry);
            let error = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(path).as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut hive,
            );
            let error = if error == 0 {
                let e = RegSetValueExW(
                    hive,
                    wide("win-confine-path-fixture").as_ptr(),
                    0,
                    REG_BINARY,
                    [1u8].as_ptr(),
                    1,
                );
                RegCloseKey(hive);
                e
            } else {
                error
            };
            probes.push(win_probe(
                "registry_write",
                &input.package_registry,
                "KEY_SET_VALUE / REG_BINARY",
                error == 0,
                error,
            ));
            probes.extend(crate::network::probes(
                input.tcp_port,
                input.udp_port,
                &input.mode,
            ));
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let cmd = format!("{system}\\System32\\cmd.exe");
            let mut line = wide(&format!("\"{cmd}\" /c exit 0"));
            let mut startup: STARTUPINFOW = zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = zeroed();
            let ok = CreateProcessW(
                wide(&cmd).as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_NO_WINDOW,
                null(),
                null(),
                &startup,
                &mut pi,
            ) != 0;
            let error = GetLastError();
            if ok {
                WaitForSingleObject(pi.hProcess, 2000);
                TerminateProcess(pi.hProcess, 0);
                drop(Handle(pi.hThread));
                drop(Handle(pi.hProcess));
            }
            probes.push(win_probe(
                "process_create",
                &cmd,
                "CreateProcessW",
                ok,
                error,
            ));
            for access in [PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ] {
                let h = OpenProcess(access, 0, input.parent_pid);
                let error = GetLastError();
                let ok = !h.is_null();
                if ok {
                    drop(Handle(h));
                }
                probes.push(win_probe(
                    "parent_process",
                    &input.parent_pid.to_string(),
                    &hex(access),
                    ok,
                    error,
                ));
            }
            let memory = VirtualAlloc(
                null(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            );
            let error = GetLastError();
            probes.push(win_probe(
                "executable_memory",
                "VirtualAlloc",
                "PAGE_EXECUTE_READWRITE",
                !memory.is_null(),
                error,
            ));
            if !memory.is_null() {
                VirtualFree(memory, 0, MEM_RELEASE);
            }
            let memory = VirtualAlloc(null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
            if memory.is_null() {
                return Err(last("VirtualAlloc(PAGE_READWRITE)"));
            }
            let mut old = 0;
            let ok = VirtualProtect(memory, 4096, PAGE_EXECUTE_READ, &mut old) != 0;
            let error = GetLastError();
            VirtualFree(memory, 0, MEM_RELEASE);
            probes.push(win_probe(
                "executable_memory",
                "VirtualProtect",
                "RW -> PAGE_EXECUTE_READ",
                ok,
                error,
            ));
            let thread = CreateThread(null(), 0, Some(thread_entry), null(), 0, null_mut());
            let error = GetLastError();
            let ok = !thread.is_null();
            if ok {
                WaitForSingleObject(thread, 2000);
                drop(Handle(thread));
            }
            probes.push(win_probe(
                "thread",
                "CreateThread",
                "in-process thread",
                ok,
                error,
            ));
        }
        if leak_only {
            eprintln!("probe-stage: raw excluded-handle WriteFile");
            // HANDLE_LIST, not the inheritable bit, must exclude this deliberately usable file handle.
            let mut written = 0;
            let ok = WriteFile(
                input.leaked_handle as HANDLE,
                b"leak\n".as_ptr(),
                5,
                &mut written,
                null_mut(),
            ) != 0;
            let error = GetLastError();
            probes.push(win_probe(
                "leaked_handle",
                &input.leaked_name,
                "FILE_WRITE_DATA",
                ok,
                error,
            ));
        }
        let report = json!({"mode":input.mode,"self_lowering":self_lowering,"token_handles_absent":no_token_handles,"after_revert":{"present":still_impersonating,"open_error":no_token_error},"primary_token_open":{"success":token_opened,"error":token_open_error,"pseudo_handle_fallback":!token_opened},"primary_token":attestation,"mitigations":policies,"ambient_close":ambient,"handle_table":input_handles,"loaded_modules":modules,"probes":probes});
        serde_json::to_writer(std::io::stdout().lock(), &report).map_err(|e| e.to_string())?;
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        Ok(())
    }
}
