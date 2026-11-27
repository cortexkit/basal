//! TEMPORARY crash report for an access violation seen only on Windows
//! arm64, in a confined worker that had already served many activations.
//!
//! A first vectored exception handler sees every exception on every thread
//! before anything handles it. For the fault codes below it writes straight
//! to the stderr handle with `WriteFile` (no Rust lock or buffer in the
//! way): the code, the thread and whether it is the main thread, the
//! faulting address as module plus offset, for an access violation whether
//! it read, wrote or executed and which address with that address's memory
//! region, the faulting stack and frame pointers, the thread's stack layout,
//! and the stack's return addresses as module plus offset. The test parent
//! symbolizes offsets into the worker executable with the build's PDB (debug
//! symbols), which the confined worker cannot read. The handler returns
//! `EXCEPTION_CONTINUE_SEARCH`, so it never changes how an exception is
//! handled or how the process ends.
//!
//! [`check_stack`] runs after every activation and reports only an unusual
//! main-thread stack: no guard page left, or nearly all of the reserve
//! committed. A thread whose stack is exhausted cannot run any exception
//! handler, so that state would otherwise go unseen.
//!
//! Remove this module once the crash is fixed.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_RECORD, RtlCaptureStackBackTrace};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
};
use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, GetCurrentThreadStackLimits};

/// `EXCEPTION_POINTERS`, declared here because windows-sys gates it behind
/// a feature the worker does not otherwise need.
#[repr(C)]
struct ExceptionPointers {
    record: *const EXCEPTION_RECORD,
    context: *const u8,
}

type Handler = unsafe extern "system" fn(*const ExceptionPointers) -> i32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(first: u32, handler: Handler) -> *mut c_void;
}

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const MEM_COMMIT: u32 = 0x1000;
const PAGE_GUARD: u32 = 0x100;

/// Marks each line of the report, so the parent can find the frames to
/// symbolize.
pub const MARKER: &str = "basal-crash-report";

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
/// Reports so far; a fault that repeats while unwinding is reported a few
/// times at most.
static REPORTS: AtomicU32 = AtomicU32::new(0);
/// Whether an unusual stack has been reported already.
static STACK_REPORTED: AtomicU32 = AtomicU32::new(0);

/// Installs the handler. Call once, on the main thread, before serving.
pub fn install() {
    // SAFETY: GetCurrentThreadId has no preconditions.
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    // SAFETY: the handler is a plain function that lives for the whole
    // process; it is never removed.
    unsafe {
        AddVectoredExceptionHandler(1, report);
    }
}

/// Writes `text` to the stderr handle directly.
fn write_stderr(text: &str) {
    // SAFETY: the handle is the process's stderr or invalid, in which case
    // WriteFile fails harmlessly; the buffer is live for the call.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        let mut written = 0u32;
        WriteFile(
            handle,
            text.as_ptr(),
            text.len().min(u32::MAX as usize) as u32,
            &mut written,
            std::ptr::null_mut(),
        );
    }
}

unsafe extern "system" fn report(info: *const ExceptionPointers) -> i32 {
    // SAFETY: Windows passes valid exception pointers to the handler.
    let (record, context) = unsafe { (&*(*info).record, (*info).context) };
    let code = record.ExceptionCode as u32;
    let fatal = matches!(
        code,
        0xC000_0005
            | 0xC000_001D
            | 0xC000_0094
            | 0xC000_0096
            | 0xC000_00FD
            | 0xC000_0374
            | 0x8000_0002
    );
    if !fatal || REPORTS.fetch_add(1, Ordering::SeqCst) >= 3 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let mut text = String::new();
    // SAFETY: GetCurrentThreadId has no preconditions.
    let thread = unsafe { GetCurrentThreadId() };
    let main = thread == MAIN_THREAD.load(Ordering::SeqCst);
    let pc = record.ExceptionAddress as usize;
    let _ = writeln!(
        text,
        "{MARKER}: exception {code:#010x} flags {:#x} on thread {thread} (main: {main}) at {}",
        record.ExceptionFlags,
        location(pc)
    );
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
    let _ = writeln!(text, "{MARKER}: {}", registers(context));
    let _ = writeln!(text, "{MARKER}: {}", stack_layout().0);
    let _ = writeln!(text, "{MARKER}: {}", threads());
    let _ = writeln!(
        text,
        "{MARKER}: {}",
        crate::confinement::windows::diagnostic_handles()
    );
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
    write_stderr(&text);
    EXCEPTION_CONTINUE_SEARCH
}

/// The faulting thread's stack pointer, program counter and, on arm64, every
/// general register (x18 holds the thread environment block (TEB), x29 the
/// frame pointer and x30 the link register), read from the `CONTEXT` record
/// at the fixed offsets winnt.h gives for each architecture.
fn registers(context: *const u8) -> String {
    if context.is_null() {
        return "no context".into();
    }
    // SAFETY: `context` is the exception's CONTEXT record, which is larger
    // than every offset read here; reads are unaligned-safe.
    let at = |offset: usize| unsafe { context.add(offset).cast::<u64>().read_unaligned() };
    if cfg!(target_arch = "aarch64") {
        // X0..X28, then Fp (X29) and Lr (X30), from offset 8.
        let general: Vec<String> = (0..31)
            .map(|index| format!("x{index} {:#x}", at(8 + index * 8)))
            .collect();
        format!("sp {:#x} pc {:#x} {}", at(256), at(264), general.join(" "))
    } else if cfg!(target_arch = "x86_64") {
        format!("rsp {:#x} rip {:#x}", at(0x98), at(0xF8))
    } else {
        "registers not decoded on this architecture".into()
    }
}

/// The calling thread's stack: its limits and each memory region in them,
/// whether a guard page remains, and how much is committed.
fn stack_layout() -> (String, bool, usize) {
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: both out-pointers are live.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut text = format!("stack {low:#x}..{high:#x}:");
    let (mut guard, mut committed) = (false, 0usize);
    let mut address = low;
    let mut regions = 0;
    while address < high && regions < 16 {
        // SAFETY: MEMORY_BASIC_INFORMATION is plain data; zero is valid.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: VirtualQuery only describes the address and writes `info`.
        let written = unsafe {
            VirtualQuery(
                address as *const c_void,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 || info.RegionSize == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let end = (base + info.RegionSize).min(high);
        let _ = write!(
            text,
            " [{base:#x}+{:#x} state {:#x} protect {:#x}]",
            end - base,
            info.State,
            info.Protect
        );
        if info.State == MEM_COMMIT {
            committed += end - base;
            guard |= info.Protect & PAGE_GUARD != 0;
        }
        address = end;
        regions += 1;
    }
    let _ = write!(text, " committed {committed:#x}, guard page {guard}");
    (text, guard, committed)
}

/// Reports, once, an unusual main-thread stack after an activation: no guard
/// page left, or more than 7 MiB committed. Prints nothing otherwise.
pub fn check_stack() {
    let (text, guard, committed) = stack_layout();
    if (!guard || committed > 7 * 1024 * 1024) && STACK_REPORTED.swap(1, Ordering::SeqCst) == 0 {
        write_stderr(&format!(
            "{MARKER}: unusual stack after an activation: {text}\n"
        ));
    }
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

/// A FILETIME as 100-nanosecond units.
fn units(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime)
}

/// Every thread of this process: its id, its age, and how long after the
/// process started it was created, so a thread-pool thread created or retired
/// just before the fault shows up.
fn threads() -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, GetThreadTimes, OpenThread,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // Now, as 100-nanosecond units since 1601, the FILETIME epoch.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64 / 100 + 116_444_736_000_000_000)
        .unwrap_or(0);
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle names this process; the out-pointers are live.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    let process_start = units(created);
    let mut text = format!(
        "process age {} ms; threads:",
        now.saturating_sub(process_start) / 10_000
    );
    // SAFETY: plain Toolhelp calls; the entry's size is set before use and
    // every handle opened is closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return format!(
                "{text} snapshot failed ({})",
                std::io::Error::last_os_error()
            );
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        let me = GetCurrentProcessId();
        while more {
            if entry.th32OwnerProcessID == me {
                let id = entry.th32ThreadID;
                let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id);
                if thread.is_null() {
                    let _ = write!(
                        text,
                        " {id} (not opened: {})",
                        std::io::Error::last_os_error()
                    );
                } else {
                    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
                    GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user);
                    CloseHandle(thread);
                    let born = units(created);
                    let _ = write!(
                        text,
                        " {id} (created at {} ms, cpu {} ms)",
                        born.saturating_sub(process_start) / 10_000,
                        (units(kernel) + units(user)) / 10_000
                    );
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    text
}
