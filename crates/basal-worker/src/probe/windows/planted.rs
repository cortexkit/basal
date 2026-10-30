//! Identify the planted file before using a process-local handle value.
//!
//! An excluded handle leaves its numeric slot available for the child's loader
//! and libraries. A write error alone cannot distinguish exclusion from reuse.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::WriteFile;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

fn text(handle: HANDLE, class: u32) -> Result<Option<String>, String> {
    // A UNICODE_STRING has at most 65535 bytes; leave room for its header
    // and the rest of OBJECT_TYPE_INFORMATION as well.
    let mut buffer = vec![0usize; 128 * 1024 / size_of::<usize>()];
    let bytes = (buffer.len() * size_of::<usize>()) as u32;
    let mut returned = 0;
    let status = unsafe {
        NtQueryObject(
            handle,
            class,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut returned,
        )
    };
    if status as u32 == 0xc000_0008 {
        return Ok(None);
    }
    if status < 0 {
        return Err(format!(
            "NtQueryObject({class}): NTSTATUS {:#010x}",
            status as u32
        ));
    }
    if returned < size_of::<UnicodeString>() as u32 || returned > bytes {
        return Err("invalid object information size".into());
    }
    let name = unsafe { &*buffer.as_ptr().cast::<UnicodeString>() };
    if name.length == 0 {
        return Ok(Some(String::new()));
    }
    let base = buffer.as_ptr() as usize;
    let start = name.buffer as usize;
    let end = start.checked_add(usize::from(name.length));
    if name.length % 2 != 0 || start < base || end.is_none_or(|end| end > base + returned as usize)
    {
        return Err("object name outside its buffer".into());
    }
    Ok(Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(name.buffer, usize::from(name.length) / 2)
    })))
}

pub fn identity(handle: HANDLE) -> Result<Option<String>, String> {
    // ObjectTypeInformation and ObjectNameInformation both begin with a
    // UNICODE_STRING. Querying them opens no handles that could reuse the slot.
    let Some(kind) = text(handle, 2)? else {
        return Ok(None);
    };
    let name = text(handle, 1)?.ok_or("handle closed during identity query")?;
    Ok(Some(format!("{kind}:{name}")))
}

pub fn attempt(handle: HANDLE, expected: &str) -> Result<Vec<u32>, String> {
    let actual = identity(handle)?;
    eprintln!("windows-probe-handle:{:#x}:{actual:?}", handle as usize);
    // Exclusion leaves the numeric slot free for the child's own objects.
    // Never write to an unrelated object, even if it happens to be writable.
    if actual.as_deref() != Some(expected) {
        return Ok(vec![0]);
    }
    let mut written = 0;
    let ok = unsafe { WriteFile(handle, b"escaped".as_ptr(), 7, &mut written, null_mut()) };
    let code = if ok != 0 {
        0
    } else {
        unsafe { GetLastError() }
    };
    if code == 0 && written != 7 {
        return Err("short planted-handle write".into());
    }
    // The first field records object identity, not a Win32 error code:
    // [0] means absent or a different object; [1, code] means the planted file.
    Ok(vec![1, code])
}
