use crate::native::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::c_void, mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{
        Diagnostics::Debug::ReadProcessMemory, LibraryLoader::*,
        RemoteDesktop::ProcessIdToSessionId, Threading::*,
    },
};

unsafe fn memory(process: HANDLE, address: usize, bytes: usize) -> Result<Vec<u8>> {
    unsafe {
        if address == 0 || bytes > 4 * 1024 * 1024 {
            return Err("invalid or oversized remote buffer".into());
        }
        let mut buffer = vec![0u8; bytes];
        let mut read = 0;
        check(
            ReadProcessMemory(
                process,
                address as *const _,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut read,
            ),
            "ReadProcessMemory(process parameters)",
        )?;
        if read != bytes {
            return Err("short process-parameter read".into());
        }
        Ok(buffer)
    }
}
fn pointer(bytes: &[u8], offset: usize) -> usize {
    usize::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
unsafe fn remote_string(process: HANDLE, parameters: &[u8], offset: usize) -> Result<String> {
    unsafe {
        let length =
            u16::from_le_bytes(parameters[offset..offset + 2].try_into().unwrap()) as usize;
        if length == 0 {
            return Ok(String::new());
        }
        let bytes = memory(process, pointer(parameters, offset + 8), length)?;
        Ok(String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        ))
    }
}
unsafe fn parameters(process: HANDLE) -> Result<Value> {
    unsafe {
        // PHNT's x64 PEB and RTL_USER_PROCESS_PARAMETERS layouts are used only
        // for outside diagnostic reads, never as a production launch contract.
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
            return Err(format!("ProcessBasicInformation: {}", hex(status as u32)));
        }
        let peb = memory(process, basic[1], 0x28)?;
        let bytes = memory(process, pointer(&peb, 0x20), 0x400)?;
        let string = |offset| {
            remote_string(process, &bytes, offset)
                .map(|s| json!(s))
                .unwrap_or_else(|e| json!({"error":e}))
        };
        let environment = (|| -> Result<Value> {
            let data = memory(process, pointer(&bytes, 0x80), pointer(&bytes, 0x3f0))?;
            let words = data.chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect::<Vec<_>>();
            let mut names = Vec::new();
            let mut selected = BTreeMap::new();
            for entry in words.split(|c|*c == 0).filter(|s|!s.is_empty()) {
                let text = String::from_utf16_lossy(entry);
                let start = if text.starts_with('=') {1} else {0};
                if let Some(index) = text[start..].find('=') {
                    let index = index+start;
                    let name = text[..index].to_ascii_uppercase();
                    names.push(name.clone());
                    // Runner environments can contain credentials. Preserve
                    // names, but values only for these non-secret path variables.
                    if ["SYSTEMROOT","WINDIR","TEMP","TMP","USERPROFILE","PATH"].contains(&name.as_str()) { selected.insert(name,text[index+1..].to_owned()); }
                }
            }
            names.sort();
            Ok(json!({"byte_length":data.len(),"names":names,"selected_values":selected,"other_values":"omitted to avoid publishing runner credentials"}))
        })().unwrap_or_else(|e|json!({"error":e}));
        Ok(
            json!({"layout":"PHNT x64","current_directory":string(0x38),"image":string(0x60),"desktop":string(0xc0),"console_handle":pointer(&bytes,0x10),"environment":environment}),
        )
    }
}

struct UserObjects(HMODULE);
impl Drop for UserObjects {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}
impl UserObjects {
    unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Result<T> {
        unsafe {
            let address = GetProcAddress(self.0, name.as_ptr())
                .ok_or_else(|| last("GetProcAddress(User32 observer)"))?;
            Ok(std::mem::transmute_copy(&address))
        }
    }
}
unsafe fn desktop_security(name: &str, token: HANDLE) -> Result<Value> {
    unsafe {
        // Resolve User32 only in the observing broker. Static imports would
        // change which DLLs the measured worker initializes before Rust entry.
        let api = UserObjects(LoadLibraryExW(
            wide("user32.dll").as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        ));
        if api.0.is_null() {
            return Err(last("LoadLibraryExW(User32 observer)"));
        }
        let open_station: unsafe extern "system" fn(*const u16, u32, u32) -> HANDLE =
            api.symbol(b"OpenWindowStationW\0")?;
        let close_station: unsafe extern "system" fn(HANDLE) -> i32 =
            api.symbol(b"CloseWindowStation\0")?;
        let open_desktop: unsafe extern "system" fn(*const u16, u32, i32, u32) -> HANDLE =
            api.symbol(b"OpenDesktopW\0")?;
        let close_desktop: unsafe extern "system" fn(HANDLE) -> i32 =
            api.symbol(b"CloseDesktop\0")?;
        let current_station: unsafe extern "system" fn() -> HANDLE =
            api.symbol(b"GetProcessWindowStation\0")?;
        let object_info: unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32 =
            api.symbol(b"GetUserObjectInformationW\0")?;
        let security: unsafe extern "system" fn(
            HANDLE,
            *const u32,
            *mut c_void,
            u32,
            *mut u32,
        ) -> i32 = api.symbol(b"GetUserObjectSecurity\0")?;
        let query = |handle, station| -> Value {
            let information = 7u32;
            let mut needed = 0;
            security(handle, &information, null_mut(), 0, &mut needed);
            if needed == 0 {
                return json!({"error":last("GetUserObjectSecurity(size)")});
            }
            let mut bytes = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            if security(
                handle,
                &information,
                bytes.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return json!({"error":last("GetUserObjectSecurity")});
            }
            let mut mapping = if station {
                GENERIC_MAPPING {
                    GenericRead: 0x20303,
                    GenericWrite: 0x2003c,
                    GenericExecute: 0x20140,
                    GenericAll: 0xf037f,
                }
            } else {
                GENERIC_MAPPING {
                    GenericRead: 0x20041,
                    GenericWrite: 0x200be,
                    GenericExecute: 0x20100,
                    GenericAll: 0xf01ff,
                }
            };
            descriptor_attestation(bytes.as_mut_ptr().cast(), token, &mut mapping)
        };
        let (station, desktop) = name
            .split_once('\\')
            .ok_or("process parameters do not contain an explicit station\\desktop")?;
        let station_handle = open_station(wide(station).as_ptr(), 0, 0x20000);
        let station_acl = if station_handle.is_null() {
            json!({"error":last("OpenWindowStationW(observer)")})
        } else {
            let result = query(station_handle, true);
            close_station(station_handle);
            result
        };
        let mut current = [0u16; 512];
        let mut needed = 0;
        let same_station = object_info(
            current_station(),
            2,
            current.as_mut_ptr().cast(),
            std::mem::size_of_val(&current) as u32,
            &mut needed,
        ) != 0
            && String::from_utf16_lossy(
                &current[..current
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(current.len())],
            )
            .eq_ignore_ascii_case(station);
        let desktop_acl = if same_station {
            let handle = open_desktop(wide(desktop).as_ptr(), 0, 0, 0x20000);
            if handle.is_null() {
                json!({"error":last("OpenDesktopW(observer)")})
            } else {
                let result = query(handle, false);
                close_desktop(handle);
                result
            }
        } else {
            json!({"not_measured":"target station differs from broker; broker station was not changed"})
        };
        Ok(json!({"parameter_name":name,"window_station":station_acl,"desktop":desktop_acl}))
    }
}

pub unsafe fn inspect(process: HANDLE) -> Value {
    unsafe {
        let parameters = parameters(process).unwrap_or_else(|e| json!({"error":e}));
        let mut primary = null_mut();
        let mut impersonation = null_mut();
        let mut error = Value::Null;
        if OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut primary) != 0 {
            let primary = Handle(primary);
            if DuplicateTokenEx(
                primary.0,
                TOKEN_QUERY,
                null_mut(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut impersonation,
            ) == 0
            {
                error = json!(last("DuplicateTokenEx(startup DACL checks)"));
            }
        } else {
            error = json!(last("OpenProcessToken(startup DACL checks)"));
        }
        let token = Handle(impersonation);
        let desktop = parameters["desktop"]
            .as_str()
            .map(|name| desktop_security(name, token.0).unwrap_or_else(|e| json!({"error":e})))
            .unwrap_or_else(|| json!({"not_measured":"desktop parameter unavailable"}));
        let mut session = 0;
        let mut basic = [0usize; 6];
        let mut returned = 0;
        let status = NtQueryInformationProcess(
            process,
            0,
            basic.as_mut_ptr().cast(),
            std::mem::size_of_val(&basic) as u32,
            &mut returned,
        );
        let session_ok = status == 0 && ProcessIdToSessionId(basic[4] as u32, &mut session) != 0;
        let mut directories = vec![directory_attestation("\\KnownDlls", token.0)];
        if session_ok {
            directories.push(directory_attestation(
                &format!("\\Sessions\\{session}\\BaseNamedObjects"),
                token.0,
            ));
        }
        json!({"parameters":parameters,"desktop_security":desktop,"access_check_token_error":error,"named_object_acl_samples":directories,"named_object_open_trace":"not measured: loader snaps do not expose KernelBase's internal opens or CSR/console endpoint identity"})
    }
}
