#![allow(unsafe_op_in_unsafe_fn)]
use basal_launch::{LaunchOptions, create_or_open_profile, grant_test_binary_directory, launch};
use basal_proto::FLOW_JOB_COMMIT_BYTES;
use basal_worker::confinement::windows::allowlist::{self, HandleEntry, Stdio};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Threading::*;

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    handle: usize,
    handles: usize,
    pointers: usize,
    access: u32,
    ty: u32,
    attrs: u32,
    reserved: u32,
}
#[repr(C)]
struct ObjectType {
    name: UnicodeString,
    counters: [u32; 13],
    mapping: [u32; 4],
    access: u32,
    security: u8,
    maintain: u8,
    index: u8,
    reserved: u8,
    pool: [u32; 3],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Trace {
    handle: usize,
    pid: usize,
    tid: usize,
    kind: u32,
    stack: [usize; 16],
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSetInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *const c_void,
        bytes: u32,
    ) -> i32;
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;
}
fn enable(process: HANDLE, _: u32) -> Result<(), String> {
    let settings = [0u32, 16384];
    let status = unsafe { NtSetInformationProcess(process, 32, settings.as_ptr().cast(), 8) };
    if status != 0 {
        return Err(format!("ProcessHandleTracing enable: {:#x}", status as u32));
    }
    Ok(())
}
unsafe fn query(process: HANDLE, class: u32, object: bool) -> Result<(Vec<usize>, usize), String> {
    let mut bytes = 64 * 1024;
    for _ in 0..10 {
        let mut buffer = vec![0usize; bytes / 8];
        let mut returned = 0;
        let status = if object {
            NtQueryObject(
                process,
                class,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                &mut returned,
            )
        } else {
            NtQueryInformationProcess(
                process,
                class,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                &mut returned,
            )
        };
        if matches!(status as u32, 0xc0000004 | 0x80000005 | 0xc0000023) {
            bytes = (bytes * 2)
                .max(returned as usize + 4096)
                .next_multiple_of(8);
            continue;
        }
        if status < 0 {
            return Err(format!("query class {class}: {:#x}", status as u32));
        }
        if returned as usize > bytes {
            return Err("query returned oversized buffer".into());
        }
        return Ok((buffer, returned as usize));
    }
    Err("query kept growing".into())
}
unsafe fn text(name: &UnicodeString) -> String {
    String::from_utf16_lossy(std::slice::from_raw_parts(
        name.buffer,
        name.length as usize / 2,
    ))
}
unsafe fn inventory(process: HANDLE, stdio: [usize; 3]) -> Result<Vec<HandleEntry>, String> {
    let (types_buffer, end) = query(null_mut(), 3, true)?;
    let base = types_buffer.as_ptr().cast::<u8>();
    let mut offset = 8;
    let mut types = BTreeMap::new();
    for _ in 0..*base.cast::<u32>() {
        if offset + size_of::<ObjectType>() > end {
            return Err("truncated object types".into());
        }
        let ty = &*base.add(offset).cast::<ObjectType>();
        types.insert(ty.index as u32, text(&ty.name));
        offset = (offset + size_of::<ObjectType>() + ty.name.maximum_length as usize)
            .next_multiple_of(8);
    }
    let (buffer, returned) = query(process, 51, false)?;
    let count = buffer[0];
    if count > (returned.saturating_sub(16)) / size_of::<Entry>() {
        return Err("truncated handle table".into());
    }
    let entries = std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<Entry>(), count);
    let mut result = Vec::new();
    for entry in entries {
        let ty = types.get(&entry.ty).ok_or("unknown object type")?.clone();
        let name = if ty == "Directory" {
            let mut duplicate = null_mut();
            if DuplicateHandle(
                process,
                entry.handle as HANDLE,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return Err(format!("duplicate Directory: {}", GetLastError()));
            }
            let duplicate = Handle(duplicate);
            let (buffer, _) = query(duplicate.0, 1, true)?;
            Some(text(&*buffer.as_ptr().cast::<UnicodeString>()))
        } else {
            None
        };
        result.push(HandleEntry {
            value: entry.handle,
            type_name: ty,
            access: entry.access,
            inheritable: entry.attrs & 2 != 0,
            stdio: stdio
                .iter()
                .position(|value| *value == entry.handle)
                .map(|index| [Stdio::Input, Stdio::Output, Stdio::Error][index]),
            name,
        });
    }
    Ok(result)
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
type ModuleInfo = unsafe extern "system" fn(HANDLE, u64, *mut c_void) -> i32;
type Cleanup = unsafe extern "system" fn(HANDLE) -> i32;
struct Symbols {
    library: HMODULE,
    symbols_server: HMODULE,
    process: HANDLE,
    address: FromAddress,
    module: ModuleInfo,
    cleanup: Cleanup,
}
impl Symbols {
    unsafe fn new(process: HANDLE) -> Result<Self, String> {
        let symbols_server = if let Ok(server) = std::env::var("BASAL_SYMSRV") {
            let module = LoadLibraryExW(
                wide(&server).as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            );
            if module.is_null() {
                return Err(format!("SymSrv: {}", GetLastError()));
            }
            module
        } else {
            null_mut()
        };
        let image = std::env::var("BASAL_DBGHELP").unwrap_or_else(|_| "dbghelp.dll".into());
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
            return Err(format!("DbgHelp: {}", GetLastError()));
        }
        let exports = (
            GetProcAddress(library, c"SymInitializeW".as_ptr().cast()),
            GetProcAddress(library, c"SymFromAddr".as_ptr().cast()),
            GetProcAddress(library, c"SymGetModuleInfo64".as_ptr().cast()),
            GetProcAddress(library, c"SymCleanup".as_ptr().cast()),
        );
        let (Some(init), Some(address), Some(module), Some(cleanup)) = exports else {
            FreeLibrary(library);
            return Err("DbgHelp exports unavailable".into());
        };
        let init: Initialize = std::mem::transmute(init);
        let cache = std::env::temp_dir().join("ckdev-provenance-symbols");
        std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        if init(
            process,
            wide(&format!(
                "srv*{}*https://msdl.microsoft.com/download/symbols",
                cache.display()
            ))
            .as_ptr(),
            1,
        ) == 0
        {
            let error = GetLastError();
            FreeLibrary(library);
            return Err(format!("SymInitialize: {error}"));
        }
        let address: FromAddress = std::mem::transmute(address);
        let module: ModuleInfo = std::mem::transmute(module);
        let cleanup: Cleanup = std::mem::transmute(cleanup);
        Ok(Self {
            library,
            symbols_server,
            process,
            address,
            module,
            cleanup,
        })
    }
    unsafe fn resolve(&self, address: usize) -> Result<Value, String> {
        let mut symbol: SymbolInfo = zeroed();
        symbol.size = 88;
        symbol.max_name_len = 1024;
        let mut displacement = 0;
        if (self.address)(self.process, address as u64, &mut displacement, &mut symbol) == 0 {
            return Err(format!("unresolved {address:#x}: {}", GetLastError()));
        }
        let mut buffer = [0u64; 210];
        buffer[0] = 1680;
        if (self.module)(self.process, address as u64, buffer.as_mut_ptr().cast()) == 0 {
            return Err(format!("module at {address:#x}: {}", GetLastError()));
        }
        let bytes = std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), 1680);
        let field = &bytes[36..68];
        let module = String::from_utf8_lossy(
            &field[..field
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(field.len())],
        )
        .to_ascii_lowercase();
        let symbol_type = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        let name = String::from_utf8_lossy(&symbol.name[..(symbol.name_len as usize).min(1024)])
            .into_owned();
        Ok(
            json!({"address":format!("{address:#x}"),"module":module,"symbol":name,"displacement":displacement,"symbol_type":symbol_type}),
        )
    }
}
impl Drop for Symbols {
    fn drop(&mut self) {
        unsafe {
            (self.cleanup)(self.process);
            FreeLibrary(self.library);
            if !self.symbols_server.is_null() {
                FreeLibrary(self.symbols_server);
            }
        }
    }
}

pub(super) fn run() -> Result<(), String> {
    if cfg!(feature = "deviations") {
        return Err("measurement requires the production worker build".into());
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: windows_provenance <production-image> <output.json>".into());
    }
    let image = std::fs::canonicalize(&args[0]).map_err(|e| e.to_string())?;
    let output = PathBuf::from(&args[1]);
    let directory = std::env::temp_dir().join(format!("ckdev-provenance-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let copy = directory.join("ckdev-basal-worker.exe");
    std::fs::copy(&image, &copy).map_err(|e| e.to_string())?;
    let package = create_or_open_profile().map_err(|e| e.to_string())?;
    grant_test_binary_directory(&copy, &package).map_err(|e| e.to_string())?;
    let mut options = LaunchOptions::new(&copy, FLOW_JOB_COMMIT_BYTES)
        .arg("--confinement-probe")
        .arg("--probe=checkpoint");
    options.before_resume = Some(enable);
    let mut child = launch(&options).map_err(|e| e.to_string())?;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut diagnostics = String::new();
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(60))
            .map_err(|e| format!("startup checkpoint missing: {e}; {diagnostics}"))?;
        diagnostics.push_str(&line);
        diagnostics.push('\n');
        if line == "windows-probe-ready:checkpoint" {
            break;
        }
    }
    let process = child.as_raw_handle();
    let measured = unsafe {
        assert_eq!(size_of::<Trace>(), 160);
        assert_eq!(std::mem::offset_of!(Trace, stack), 32);
        let handles = inventory(process, child.inherited_stdio())?;
        let ceiling = allowlist::check(&handles)
            .map_err(|e| format!("STOP: production inventory outside approved ceilings: {e}"))?;
        let symbols = Symbols::new(process)?;
        let (buffer, returned) = query(process, 32, false)?;
        let count = *buffer.as_ptr().add(1).cast::<u32>() as usize;
        if count > (returned.saturating_sub(16)) / size_of::<Trace>() {
            return Err("truncated handle traces".into());
        }
        let traces = std::slice::from_raw_parts(buffer.as_ptr().add(2).cast::<Trace>(), count);
        let mut rows = Vec::new();
        let mut unresolved = Vec::new();
        for handle in &handles {
            if handle.stdio.is_some() {
                rows.push(json!({"handle":handle.value,"type":handle.type_name,"access":format!("{:#x}",handle.access),"creator":"parent CreatePipe / explicit HANDLE_LIST", "stack":[]}));
                continue;
            }
            let trace = traces
                .iter()
                .rev()
                .find(|trace| trace.handle == handle.value && trace.kind == 1);
            let mut stack = Vec::new();
            if let Some(trace) = trace {
                for address in trace.stack.iter().copied().filter(|address| *address != 0) {
                    if address >= 0xffff_8000_0000_0000 {
                        stack.push(json!({"address":format!("{address:#x}"), "module":"kernel (unsymbolized)"}));
                        continue;
                    }
                    match symbols.resolve(address) {
                        Ok(frame) => stack.push(frame),
                        Err(error) => {
                            unresolved.push(error.clone());
                            stack.push(json!({"address":format!("{address:#x}"),"error":error}));
                        }
                    }
                }
            } else {
                unresolved.push(format!(
                    "no open trace for {} {:#x}",
                    handle.type_name, handle.value
                ));
            }
            // Export-only labels can be arbitrarily far from the actual creator.
            // Require PDB symbols and an initialization chain, not a nearest export.
            let anchored = stack.iter().any(|frame| {
                let module = frame["module"].as_str().unwrap_or("");
                let symbol = frame["symbol"].as_str().unwrap_or("");
                frame["symbol_type"] == 3
                    && ((module == "ntdll"
                        && (symbol.starts_with("Ldr")
                            || symbol.starts_with("Tpp")
                            || symbol.starts_with("Tp")))
                        || (module == "kernelbase"
                            && (symbol.contains("Initialize") || symbol.contains("Init"))))
            });
            let first_user = stack
                .iter()
                .find(|frame| frame["module"] != "kernel (unsymbolized)");
            let creator_resolved = first_user.is_some_and(|frame| {
                frame["symbol_type"] == 3
                    && matches!(frame["module"].as_str(), Some("ntdll" | "kernelbase"))
            });
            if !anchored || !creator_resolved {
                unresolved.push(format!(
                    "{} {:#x}: creator not resolved to ntdll/KernelBase initialization",
                    handle.type_name, handle.value
                ));
            }
            rows.push(json!({"handle":handle.value,"type":handle.type_name,"access":format!("{:#x}",handle.access),"name":handle.name,"stack":stack}));
        }
        json!({"image":image,"runner_image":std::env::var("ImageOS").ok(),"runner_version":std::env::var("ImageVersion").ok(),"profile":ceiling.name,"total":handles.len(),"handles":rows,"unresolved":unresolved,"tracing_class":32,"tracing_slots":16384})
    };
    // Save raw evidence even when creator resolution needs a human decision.
    std::fs::write(
        &output,
        serde_json::to_vec_pretty(&measured).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // The snapshot is complete. Remove only the diagnostic trace database
    // before releasing the worker; permanent strict-handle checks are unchanged.
    let disabled = unsafe { NtSetInformationProcess(process, 32, std::ptr::null(), 0) };
    if disabled != 0 {
        return Err(format!(
            "disable ProcessHandleTracing: {:#x}",
            disabled as u32
        ));
    }
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"x")
        .map_err(|e| e.to_string())?;
    let code = child.wait().map_err(|e| e.to_string())?;
    if code != 0 {
        return Err(format!("checkpoint worker exit {code:#x}"));
    }
    drop(child);
    std::fs::remove_file(copy).map_err(|e| e.to_string())?;
    println!(
        "production inventory {}: {} handles; saved {}",
        measured["profile"],
        measured["total"],
        output.display()
    );
    if !measured["unresolved"].as_array().unwrap().is_empty() {
        return Err(format!(
            "STOP: creators need resolution: {}",
            measured["unresolved"]
        ));
    }
    Ok(())
}
