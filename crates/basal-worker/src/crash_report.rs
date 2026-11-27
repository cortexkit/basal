//! TEMPORARY crash report for an access violation seen only on Windows
//! arm64, in a confined worker that had already served many activations.
//!
//! When an exception would end the process unhandled, the top-level filter
//! prints to stderr: the exception code, the faulting address as module plus
//! offset, for an access violation whether it read, wrote or executed and
//! which address, what that address's memory region is (free, reserved,
//! committed, its protection), and the return addresses on the stack as
//! module plus offset. The test parent symbolizes offsets into the worker
//! executable with the build's PDB, which the confined worker cannot read.
//!
//! It is a top-level filter, not a vectored handler, so it runs only for an
//! exception nothing else handles, and it returns
//! `EXCEPTION_CONTINUE_SEARCH`: a run that would pass is unaffected, and a
//! crashing one ends with the same exit code as before. Remove this module
//! once the crash is fixed.

use std::ffi::c_void;
use std::fmt::Write as _;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_RECORD, RtlCaptureStackBackTrace};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
};
use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

/// `EXCEPTION_POINTERS`, declared here because windows-sys gates it behind
/// a feature the worker does not otherwise need.
#[repr(C)]
struct ExceptionPointers {
    record: *const EXCEPTION_RECORD,
    context: *mut c_void,
}

type Filter = unsafe extern "system" fn(*const ExceptionPointers) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetUnhandledExceptionFilter(filter: Option<Filter>) -> Option<Filter>;
}

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// Marks each line of the report, so the parent can find the frames to
/// symbolize.
pub const MARKER: &str = "basal-crash-report";

/// Installs the filter. Call once, before serving.
pub fn install() {
    // SAFETY: the filter is a plain function that lives for the whole
    // process; the previous filter, if any, is not needed.
    unsafe {
        SetUnhandledExceptionFilter(Some(report));
    }
}

unsafe extern "system" fn report(info: *const ExceptionPointers) -> i32 {
    // SAFETY: Windows passes valid exception pointers to the filter.
    let record = unsafe { &*(*info).record };
    let mut text = String::new();
    let code = record.ExceptionCode as u32;
    let pc = record.ExceptionAddress as usize;
    let _ = writeln!(text, "{MARKER}: exception {code:#010x} at {}", location(pc));
    let count = (record.NumberParameters as usize).min(record.ExceptionInformation.len());
    let parameters = &record.ExceptionInformation[..count];
    if code == 0xC000_0005 && count >= 2 {
        let access = match parameters[0] {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "unknown access",
        };
        let target = parameters[1];
        let _ = writeln!(
            text,
            "{MARKER}: {access} of {target:#x}; {}",
            region(target)
        );
    } else {
        let _ = writeln!(text, "{MARKER}: parameters {parameters:x?}");
    }
    let mut frames = [std::ptr::null_mut::<c_void>(); 62];
    // SAFETY: the buffer holds `frames.len()` pointers and no hash is asked for.
    let captured = unsafe {
        RtlCaptureStackBackTrace(
            0,
            frames.len() as u32,
            frames.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    for (index, frame) in frames[..usize::from(captured)].iter().enumerate() {
        let _ = writeln!(
            text,
            "{MARKER}: frame {index}: {}",
            location(*frame as usize)
        );
    }
    eprint!("{text}");
    EXCEPTION_CONTINUE_SEARCH
}

/// `address` as `exe+0x…` for the worker's own image, `<dll name>+0x…` for
/// another module, or the bare address outside any module.
fn location(address: usize) -> String {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS reads `address` as an address, not a string, and
    // UNCHANGED_REFCOUNT leaves nothing to release.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    } != 0;
    if !found {
        return format!("{address:#x}");
    }
    let offset = address.wrapping_sub(module as usize);
    // SAFETY: a null name asks for this process's executable.
    if module == unsafe { GetModuleHandleW(std::ptr::null()) } {
        return format!("exe+{offset:#x} ({address:#x})");
    }
    let mut name = [0u16; 260];
    // SAFETY: the buffer holds `name.len()` UTF-16 units.
    let length = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) };
    let path = String::from_utf16_lossy(&name[..length as usize]);
    let file = path.rsplit(['\\', '/']).next().unwrap_or(&path);
    format!("{file}+{offset:#x} ({address:#x})")
}

/// What the memory at `address` is: free, reserved or committed, with its
/// protection and type, so a stale pointer into released memory can be told
/// apart from a wild one.
fn region(address: usize) -> String {
    // SAFETY: MEMORY_BASIC_INFORMATION is plain data; zero is a valid value.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: VirtualQuery only describes the address and writes `info`.
    let written = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if written == 0 {
        return format!("region unknown ({})", std::io::Error::last_os_error());
    }
    format!(
        "region base {:#x} size {:#x} allocation base {:#x} state {:#x} protect {:#x} type {:#x}",
        info.BaseAddress as usize,
        info.RegionSize,
        info.AllocationBase as usize,
        info.State,
        info.Protect,
        info.Type
    )
}
