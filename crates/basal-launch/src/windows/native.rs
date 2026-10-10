//! Native Win32 and NT bindings, structures, and memory-safe wrappers.

use crate::windows::error::LaunchError;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::*, Security::*, System::Pipes::CreatePipe, System::Threading::*,
};

pub type PCWSTR = *const u16;
pub type PWSTR = *mut u16;
pub type HRESULT = i32;

pub const JOB_OBJECT_UILIMIT_ALL: u32 = 0x000000FF;
pub const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 1;
pub const PROCESS_CREATION_CHILD_PROCESS_RESTRICTED: u32 = 1;

/// Encodes a UTF-8 string as a null-terminated UTF-16 wide string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Reads a null-terminated wide string pointer into a Rust String.
pub unsafe fn utf16_ptr(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
}

/// Constructs a `LaunchError::Win32` capturing the last OS error.
pub fn last_win32_error(api: &'static str) -> LaunchError {
    let code = unsafe { GetLastError() };
    LaunchError::Win32 {
        api,
        code,
        detail: std::io::Error::from_raw_os_error(code as i32).to_string(),
    }
}

/// RAII wrapper for a Win32 HANDLE that calls `CloseHandle` on drop.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
            self.0 = null_mut();
        }
    }
}

impl Handle {
    pub fn new(h: HANDLE, api: &'static str) -> Result<Self, LaunchError> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            Err(last_win32_error(api))
        } else {
            Ok(Self(h))
        }
    }

    pub fn into_raw(mut self) -> HANDLE {
        let h = self.0;
        self.0 = null_mut();
        h
    }

    pub fn is_valid(&self) -> bool {
        !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
    }
}

/// RAII wrapper for a `PROC_THREAD_ATTRIBUTE_LIST`.
pub struct AttributeList {
    buffer: Vec<usize>,
}

impl AttributeList {
    pub fn new(count: u32) -> Result<Self, LaunchError> {
        unsafe {
            let mut bytes = 0;
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes);
            let mut this = Self {
                buffer: vec![0; (bytes as usize).div_ceil(size_of::<usize>())],
            };
            if InitializeProcThreadAttributeList(this.ptr(), count, 0, &mut bytes) == 0 {
                return Err(last_win32_error("InitializeProcThreadAttributeList"));
            }
            Ok(this)
        }
    }

    pub fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    pub fn add<T>(&mut self, key: u32, value: &T) -> Result<(), LaunchError> {
        unsafe {
            if UpdateProcThreadAttribute(
                self.ptr(),
                0,
                key as usize,
                (value as *const T).cast(),
                size_of::<T>(),
                null_mut(),
                std::ptr::null(),
            ) == 0
            {
                Err(last_win32_error("UpdateProcThreadAttribute"))
            } else {
                Ok(())
            }
        }
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

/// Creates an anonymous pipe pair: (read_handle, write_handle).
pub fn create_pipe(inheritable: bool) -> Result<(Handle, Handle), LaunchError> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: if inheritable { 1 } else { 0 },
        };
        let mut read = null_mut();
        let mut write = null_mut();
        if CreatePipe(&mut read, &mut write, &sa, 0) == 0 {
            return Err(last_win32_error("CreatePipe"));
        }
        Ok((Handle(read), Handle(write)))
    }
}

/// Queries token information into a heap-allocated buffer.
pub unsafe fn token_buffer(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<usize>, LaunchError> {
    unsafe {
        let mut bytes = 0;
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last_win32_error("GetTokenInformation(size)"));
        }
        let allocation = (bytes as usize)
            .max(size_of::<TOKEN_GROUPS>())
            .max(size_of::<TOKEN_PRIVILEGES>());
        let mut data = vec![0usize; allocation.div_ceil(size_of::<usize>())];
        if GetTokenInformation(token, class, data.as_mut_ptr().cast(), bytes, &mut bytes) == 0 {
            return Err(last_win32_error("GetTokenInformation"));
        }
        Ok(data)
    }
}

/// Native Unicode string representation for NT APIs.
#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

impl UnicodeString {
    pub fn as_slice(&self) -> &[u16] {
        if self.buffer.is_null() || self.length == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.buffer, (self.length / 2) as usize) }
        }
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(self.as_slice())
    }
}

/// Native object attributes structure.
#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root: HANDLE,
    pub name: *mut UnicodeString,
    pub attributes: u32,
    pub descriptor: *mut c_void,
    pub qos: *mut c_void,
}

/// Native security attribute entry inside `TokenSecurityAttributes`.
#[repr(C)]
pub struct SecurityAttribute {
    pub name: UnicodeString,
    pub kind: u16,
    pub reserved: u16,
    pub flags: u32,
    pub count: u32,
    pub values: *mut c_void,
}

/// Native header for `TokenSecurityAttributes`.
#[repr(C)]
pub struct SecurityAttributes {
    pub version: u16,
    pub reserved: u16,
    pub count: u32,
    pub attributes: *mut SecurityAttribute,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    pub fn NtQueryInformationToken(
        token: HANDLE,
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;

    pub fn NtCreateLowBoxToken(
        out: *mut HANDLE,
        existing: HANDLE,
        access: u32,
        attrs: *mut ObjectAttributes,
        package: PSID,
        capability_count: u32,
        capabilities: *const SID_AND_ATTRIBUTES,
        handle_count: u32,
        handles: *const HANDLE,
    ) -> i32;
}
