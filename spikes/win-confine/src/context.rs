use crate::native::*;
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    System::LibraryLoader::*,
};

struct Descriptor(PSECURITY_DESCRIPTOR);
impl Descriptor {
    unsafe fn new(sddl: &str) -> Result<Self> {
        unsafe {
            let mut descriptor = null_mut();
            check(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                ),
                "ConvertStringSecurityDescriptorToSecurityDescriptorW(context)",
            )?;
            Ok(Self(descriptor))
        }
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub struct Context {
    api: HMODULE,
    station: HANDLE,
    desktop: HANDLE,
    close_station: unsafe extern "system" fn(HANDLE) -> i32,
    close_desktop: unsafe extern "system" fn(HANDLE) -> i32,
    pub name: Vec<u16>,
    pub cwd: Vec<u16>,
    pub environment: Vec<u16>,
    pub report: Value,
    temp: std::path::PathBuf,
}
impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            (self.close_desktop)(self.desktop);
            (self.close_station)(self.station);
            FreeLibrary(self.api);
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

unsafe fn set_directory(path: &str, user: &str, package: &str, write: bool) -> Result<()> {
    unsafe {
        // Only disposable worker directories receive the NULL restricting SID
        // grant. A Low label permits TEMP writes without weakening ancestor ACLs.
        let access = if write { "GA" } else { "GRGX" };
        let descriptor = Descriptor::new(&format!(
            "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GA;;;{user})(A;OICI;{access};;;S-1-0-0)(A;OICI;{access};;;{package})S:(ML;OICI;NW;;;LW)"
        ))?;
        check(
            SetFileSecurityW(
                wide(path).as_ptr(),
                DACL_SECURITY_INFORMATION
                    | LABEL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.0,
            ),
            "SetFileSecurityW(context directory)",
        )
    }
}

pub unsafe fn create(image: &str, token: HANDLE, package: PSID) -> Result<Context> {
    unsafe {
        // User32 stays broker-only so the context experiment does not add GUI
        // imports to the worker whose early loader activity is being measured.
        let api = LoadLibraryExW(
            wide("user32.dll").as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        if api.is_null() {
            return Err(last("LoadLibraryExW(context)"));
        }
        unsafe fn symbol<T: Copy>(api: HMODULE, name: &[u8]) -> Result<T> {
            unsafe {
                Ok(std::mem::transmute_copy(
                    &GetProcAddress(api, name.as_ptr())
                        .ok_or_else(|| last("GetProcAddress(context)"))?,
                ))
            }
        }
        let create_station: unsafe extern "system" fn(
            *const u16,
            u32,
            u32,
            *const SECURITY_ATTRIBUTES,
        ) -> HANDLE = symbol(api, b"CreateWindowStationW\0")?;
        let create_desktop: unsafe extern "system" fn(
            *const u16,
            *const u16,
            *const c_void,
            u32,
            u32,
            *const SECURITY_ATTRIBUTES,
        ) -> HANDLE = symbol(api, b"CreateDesktopW\0")?;
        let get_station: unsafe extern "system" fn() -> HANDLE =
            symbol(api, b"GetProcessWindowStation\0")?;
        let set_station: unsafe extern "system" fn(HANDLE) -> i32 =
            symbol(api, b"SetProcessWindowStation\0")?;
        let close_station = symbol(api, b"CloseWindowStation\0")?;
        let close_desktop = symbol(api, b"CloseDesktop\0")?;
        let user = token_buffer(token, TokenUser)?;
        let user = sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid);
        let groups_data = token_buffer(token, TokenGroups)?;
        let logon = groups(&groups_data)
            .iter()
            .find(|g| g.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
            .ok_or("no logon SID")?;
        let logon = sid_string(logon.Sid);
        let package = sid_string(package);
        let station_sddl = format!(
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user})(A;;GRGX;;;{logon})(A;;GRGX;;;S-1-0-0)(A;;GRGX;;;{package})S:(ML;;NW;;;LW)"
        );
        let desktop_sddl = format!(
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user})(A;;0x20081;;;{logon})(A;;0x20081;;;S-1-0-0)(A;;0x20081;;;{package})S:(ML;;NW;;;LW)"
        );
        let station_sd = Descriptor::new(&station_sddl)?;
        let desktop_sd = Descriptor::new(&desktop_sddl)?;
        let station_name = format!(
            "basal_spike_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        );
        let station = create_station(
            wide(&station_name).as_ptr(),
            0,
            0xf037f,
            &station_sd.attributes(),
        );
        if station.is_null() {
            FreeLibrary(api);
            return Err(last("CreateWindowStationW(context)"));
        }
        let mut context = Context {
            api,
            station,
            desktop: null_mut(),
            close_station,
            close_desktop,
            name: wide(&format!("{station_name}\\worker")),
            cwd: Vec::new(),
            environment: Vec::new(),
            report: Value::Null,
            temp: std::path::PathBuf::new(),
        };
        let original = get_station();
        check(set_station(station), "SetProcessWindowStation(alternate)")?;
        context.desktop = create_desktop(
            wide("worker").as_ptr(),
            null(),
            null(),
            0,
            0xf01ff,
            &desktop_sd.attributes(),
        );
        let desktop_error = if context.desktop.is_null() {
            Some(last("CreateDesktopW(context)"))
        } else {
            None
        };
        // CreateDesktop uses the process station, so restore the broker before
        // launching any other measured recipe or running parent-side probes.
        check(set_station(original), "SetProcessWindowStation(restore)")?;
        if let Some(error) = desktop_error {
            return Err(error);
        }
        let cwd = std::path::Path::new(image)
            .parent()
            .ok_or("image has no directory")?
            .to_string_lossy()
            .into_owned();
        context.temp = std::env::temp_dir().join(&station_name);
        std::fs::create_dir(&context.temp).map_err(|e| e.to_string())?;
        let temp = context.temp.to_string_lossy().into_owned();
        set_directory(&cwd, &user, &package, false)?;
        set_directory(&temp, &user, &package, true)?;
        let system = std::env::var("SystemRoot").map_err(|e| e.to_string())?;
        let mut variables = vec![
            ("CHROME_CRASHPAD_PIPE_NAME", String::new()),
            ("LOCALAPPDATA", temp.clone()),
            ("PATH", format!("{system}\\System32")),
            ("SYSTEMDRIVE", system[..2].to_owned()),
            ("SYSTEMROOT", system.clone()),
            ("TEMP", temp.clone()),
            ("TMP", temp.clone()),
            ("windir", system),
        ];
        variables.sort_by_key(|(key, _)| key.to_ascii_uppercase());
        context.environment = variables
            .iter()
            .flat_map(|(key, value)| wide(&format!("{key}={value}")))
            .chain([0])
            .collect();
        context.cwd = wide(&cwd);
        context.report = json!({"desktop":format!("{station_name}\\worker"),"station_sddl":station_sddl,"desktop_sddl":desktop_sddl,"cwd":cwd,"temp":temp,"environment":variables,"chrome_acl_equivalence":false});
        Ok(context)
    }
}
