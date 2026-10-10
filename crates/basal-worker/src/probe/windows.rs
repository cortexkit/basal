//! Native Windows probes. Each child attempts one operation after its own
//! readiness marker, so a startup refusal cannot be mistaken for a denial.

#![allow(unsafe_op_in_unsafe_fn)]

mod objects;
mod pool;
mod winsock;

use crate::confinement::windows::{self, ArgumentError, PackageSid};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Threading::*;

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn last() -> u32 {
    unsafe { GetLastError() }
}
fn win_result(ok: bool) -> u32 {
    if ok { 0 } else { last() }
}
pub(super) fn marker(name: &str) -> Result<(), String> {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "windows-probe-ready:{name}").map_err(|e| e.to_string())?;
    stderr.flush().map_err(|e| e.to_string())
}
pub(super) fn byte() -> Result<(), String> {
    std::io::stdin()
        .read_exact(&mut [0])
        .map_err(|e| e.to_string())
}

/// Windows-only options are deliberately separate from the Unix probe grammar.
pub fn run(args: &[String]) -> u8 {
    let mut startup = Vec::new();
    let mut sandbox = true;
    let mut probe = None;
    let mut target = String::new();
    let mut number = 0usize;
    for arg in args {
        if arg == "--no-sandbox" {
            sandbox = false;
        } else if arg == "--thread-barrier" {
            if probe.replace("thread-barrier".to_owned()).is_some() {
                return 64;
            }
        } else if let Some(value) = arg.strip_prefix("--probe=") {
            if probe.replace(value.to_owned()).is_some() {
                return 64;
            }
        } else if let Some(value) = arg.strip_prefix("--target=") {
            target = value.to_owned();
        } else if let Some(value) = arg.strip_prefix("--number=") {
            let Ok(value) = value.parse() else { return 64 };
            number = value;
        } else {
            startup.push(arg.clone());
        }
    }
    let Some(probe) = probe else { return 64 };
    if !matches!(
        probe.as_str(),
        "read"
            | "stat"
            | "create"
            | "registry"
            | "tcp"
            | "udp"
            | "loopback"
            | "process"
            | "parent-query"
            | "parent-read"
            | "pipe"
            | "alpc"
            | "section"
            | "allocate-executable"
            | "protect-executable"
            | "thread"
            | "excluded-handle"
            | "crash-before-ready"
            | "thread-barrier"
            | "pool"
            | "checkpoint"
    ) {
        return 64;
    }
    let arguments = match windows::engine_arguments(&startup) {
        Ok(arguments) => arguments,
        Err(ArgumentError::Missing) => {
            eprintln!("ck-basal-worker: refusing to run unconfined: package-sid-argument-missing");
            return 70;
        }
        Err(error) => {
            eprintln!("confinement probe: {error:?}");
            return 64;
        }
    };
    let package = match PackageSid::parse(&arguments.package_sid) {
        Ok(package) => package,
        Err(error) => {
            eprintln!("confinement probe: unparsable --package-sid: {error}");
            return 64;
        }
    };
    if sandbox {
        if let Err(error) = windows::enter(&package, &arguments) {
            eprintln!("ck-basal-worker: refusing to run unconfined: {error}");
            return 70;
        }
    } else if unsafe { windows_sys::Win32::Security::RevertToSelf() } == 0 {
        eprintln!("confinement probe: RevertToSelf: {}", last());
        return 70;
    }
    if probe == "crash-before-ready" {
        unsafe { TerminateProcess(GetCurrentProcess(), 0xc0000008) };
        return 71;
    }
    let result = (|| {
        marker(&probe)?;
        let codes = unsafe { attempt(&probe, &target, number)? };
        println!(
            "windows-probe-result:{probe}:{}",
            codes
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        Ok::<_, String>(())
    })();
    if let Err(error) = result {
        eprintln!("confinement probe: {error}");
        71
    } else {
        0
    }
}

unsafe extern "system" fn thread_entry(_: *mut c_void) -> u32 {
    0
}

unsafe fn attempt(probe: &str, target: &str, number: usize) -> Result<Vec<u32>, String> {
    let code = match probe {
        "read" | "stat" | "create" | "pipe" => {
            let handle = CreateFileW(
                wide(target).as_ptr(),
                match probe {
                    "stat" => FILE_READ_ATTRIBUTES,
                    "create" => FILE_WRITE_DATA,
                    _ => FILE_READ_DATA,
                },
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                if probe == "create" {
                    CREATE_NEW
                } else {
                    OPEN_EXISTING
                },
                0,
                null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                last()
            } else {
                let handle = Handle(handle);
                match probe {
                    "read" => {
                        let mut read = 0;
                        win_result(
                            ReadFile(handle.0, [0u8; 1].as_mut_ptr(), 1, &mut read, null_mut())
                                != 0,
                        )
                    }
                    "stat" => {
                        let mut info = zeroed();
                        win_result(GetFileInformationByHandle(handle.0, &mut info) != 0)
                    }
                    _ => 0,
                }
            }
        }
        "registry" => objects::registry_write(target),
        "alpc" => objects::alpc(target),
        "section" => objects::section(target),
        "tcp" | "udp" | "loopback" => return winsock::attempt(probe, number as u16),
        "process" => {
            let mut startup: STARTUPINFOW = zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = zeroed();
            let mut command = wide(&format!("\"{target}\" /c exit 0"));
            let ok = CreateProcessW(
                wide(target).as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_NO_WINDOW,
                null(),
                null(),
                &startup,
                &mut info,
            ) != 0;
            let code = win_result(ok);
            if ok {
                let process = Handle(info.hProcess);
                let _thread = Handle(info.hThread);
                if WaitForSingleObject(process.0, 10_000) != WAIT_OBJECT_0 {
                    TerminateProcess(process.0, 71);
                    return Err("control descendant did not exit".into());
                }
            }
            code
        }
        "parent-query" | "parent-read" => {
            let handle = OpenProcess(
                if probe == "parent-query" {
                    PROCESS_QUERY_LIMITED_INFORMATION
                } else {
                    PROCESS_VM_READ
                },
                0,
                number as u32,
            );
            if handle.is_null() {
                last()
            } else {
                drop(Handle(handle));
                0
            }
        }
        "allocate-executable" => {
            let memory = VirtualAlloc(
                null(),
                4096,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            );
            if memory.is_null() {
                last()
            } else {
                VirtualFree(memory, 0, MEM_RELEASE);
                0
            }
        }
        "protect-executable" => {
            let memory = VirtualAlloc(null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
            if memory.is_null() {
                return Err(format!("RW allocation: {}", last()));
            }
            let mut old = 0;
            let code = win_result(VirtualProtect(memory, 4096, PAGE_EXECUTE_READ, &mut old) != 0);
            VirtualFree(memory, 0, MEM_RELEASE);
            code
        }
        "thread" => {
            let thread = CreateThread(null(), 0, Some(thread_entry), null(), 0, null_mut());
            if thread.is_null() {
                last()
            } else {
                let thread = Handle(thread);
                if WaitForSingleObject(thread.0, 10_000) != WAIT_OBJECT_0 {
                    return Err("thread did not complete".into());
                }
                0
            }
        }
        "excluded-handle" => {
            let mut written = 0;
            let code = win_result(
                WriteFile(
                    number as HANDLE,
                    b"escaped".as_ptr(),
                    7,
                    &mut written,
                    null_mut(),
                ) != 0,
            );
            if code == 0 && written != 7 {
                return Err("short planted-handle write".into());
            }
            code
        }
        "thread-barrier" => {
            byte()?;
            pool::run(true)?;
            0
        }
        "pool" => {
            pool::run(false)?;
            0
        }
        "checkpoint" => {
            byte()?;
            0
        }
        _ => unreachable!(),
    };
    Ok(vec![code])
}
