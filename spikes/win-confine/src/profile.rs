use crate::native::*;
use std::{
    mem::transmute_copy,
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{LibraryLoader::*, Registry::HKEY},
    },
    core::*,
};

type Create = unsafe extern "system" fn(
    PCWSTR,
    PCWSTR,
    PCWSTR,
    *const SID_AND_ATTRIBUTES,
    u32,
    *mut PSID,
) -> HRESULT;
type Delete = unsafe extern "system" fn(PCWSTR) -> HRESULT;
type Folder = unsafe extern "system" fn(PCWSTR, *mut PWSTR) -> HRESULT;
type Registry = unsafe extern "system" fn(u32, *mut HKEY) -> HRESULT;
type Free = unsafe extern "system" fn(*const std::ffi::c_void);

struct Module(HMODULE);
impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}
impl Module {
    fn load(name: &str) -> Result<Self> {
        unsafe {
            let h = LoadLibraryExW(
                wide(name).as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if h.is_null() {
                Err(last("LoadLibraryExW(parent-only API)"))
            } else {
                Ok(Self(h))
            }
        }
    }
    unsafe fn symbol<T>(&self, name: &[u8]) -> Result<T> {
        unsafe {
            let f = GetProcAddress(self.0, name.as_ptr())
                .ok_or_else(|| last("GetProcAddress(parent-only API)"))?;
            Ok(transmute_copy(&f))
        }
    }
}

// Userenv and Ole32 import User32/GDI transitively and initialize GUI/COM
// authority at DLL startup. Resolve these broker-only APIs in the parent so
// the same binary's child role does not load them before lockdown.
pub struct UserEnv {
    _user: Module,
    _com: Module,
    create: Create,
    delete: Delete,
    folder: Folder,
    registry: Registry,
    free: Free,
}
impl UserEnv {
    pub fn new() -> Result<Self> {
        unsafe {
            let user = Module::load("userenv.dll")?;
            let com = Module::load("ole32.dll")?;
            Ok(Self {
                create: user.symbol(b"CreateAppContainerProfile\0")?,
                delete: user.symbol(b"DeleteAppContainerProfile\0")?,
                folder: user.symbol(b"GetAppContainerFolderPath\0")?,
                registry: user.symbol(b"GetAppContainerRegistryLocation\0")?,
                free: com.symbol(b"CoTaskMemFree\0")?,
                _user: user,
                _com: com,
            })
        }
    }
    pub unsafe fn create(&self, name: &str, sid: *mut PSID) -> HRESULT {
        unsafe {
            (self.create)(
                wide(name).as_ptr(),
                wide(name).as_ptr(),
                wide("Native reachability experiment").as_ptr(),
                null(),
                0,
                sid,
            )
        }
    }
    pub unsafe fn delete(&self, name: &str) -> HRESULT {
        unsafe { (self.delete)(wide(name).as_ptr()) }
    }
    pub unsafe fn folder(&self, sid: &str) -> Result<String> {
        unsafe {
            let mut path = null_mut();
            let hr = (self.folder)(wide(sid).as_ptr(), &mut path);
            if hr < 0 {
                return Err(format!("GetAppContainerFolderPath: {}", hex(hr as u32)));
            }
            let result = utf16_ptr(path);
            (self.free)(path.cast());
            Ok(result)
        }
    }
    pub unsafe fn registry(&self, access: u32, key: *mut HKEY) -> HRESULT {
        unsafe { (self.registry)(access, key) }
    }
}
