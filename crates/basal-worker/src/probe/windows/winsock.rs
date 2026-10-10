//! Only socket probes preload System32 Winsock while the startup token still
//! permits DLL loading. All startup checks then run unchanged. WSAStartup and
//! socket calls occur only after attestation and readiness. The engine and every
//! other probe keep Winsock out of their loader inventory and static imports.
#![allow(unsafe_op_in_unsafe_fn)]
use super::{last, wide};
use std::mem::{transmute, zeroed};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::Networking::WinSock::{SOCKADDR, SOCKET, WSADATA};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

type Startup = unsafe extern "system" fn(u16, *mut WSADATA) -> i32;
type Socket = unsafe extern "system" fn(i32, i32, i32) -> SOCKET;
type Connect = unsafe extern "system" fn(SOCKET, *const SOCKADDR, i32) -> i32;
type SendTo = unsafe extern "system" fn(SOCKET, *const u8, i32, i32, *const SOCKADDR, i32) -> i32;
type Error = unsafe extern "system" fn() -> i32;
type Close = unsafe extern "system" fn(SOCKET) -> i32;
type Cleanup = unsafe extern "system" fn() -> i32;
pub(super) struct Backend {
    // The mapping remains resident until process exit, just like a static DLL
    // import, so its callback and function addresses cannot outlive the module.
    _module: HMODULE,
    startup: Startup,
    socket: Socket,
    error: Error,
    close: Close,
    cleanup: Cleanup,
    connect: Connect,
    send_to: SendTo,
}

pub(super) unsafe fn preload() -> Result<Backend, String> {
    let module = LoadLibraryExW(
        wide("ws2_32.dll").as_ptr(),
        null_mut(),
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    );
    if module.is_null() {
        return Err(format!("LoadLibraryExW(ws2_32): {}", last()));
    }
    macro_rules! export {
        ($name:literal, $ty:ty) => {
            transmute::<unsafe extern "system" fn() -> isize, $ty>(
                GetProcAddress(module, concat!($name, "\0").as_ptr())
                    .ok_or(concat!("missing ", $name))?,
            )
        };
    }
    let startup = export!("WSAStartup", Startup);
    let socket = export!("socket", Socket);
    let error = export!("WSAGetLastError", Error);
    let close = export!("closesocket", Close);
    let cleanup = export!("WSACleanup", Cleanup);
    let connect = export!("connect", Connect);
    let send_to = export!("sendto", SendTo);
    Ok(Backend {
        _module: module,
        startup,
        socket,
        error,
        close,
        cleanup,
        connect,
        send_to,
    })
}

impl Backend {
    pub(super) unsafe fn attempt(&self, probe: &str, port: u16) -> Vec<u32> {
        let Self {
            startup,
            socket,
            error,
            close,
            cleanup,
            connect,
            send_to,
            ..
        } = self;
        let mut data = zeroed();
        let started = startup(0x202, &mut data);
        let udp = probe == "udp";
        let handle = socket(2, if udp { 2 } else { 1 }, if udp { 17 } else { 6 });
        if handle == usize::MAX {
            let code = error() as u32;
            if started == 0 {
                cleanup();
            }
            return vec![started as u32, code];
        }
        let mut address = [0u8; 16];
        address[..2].copy_from_slice(&2u16.to_ne_bytes());
        address[2..4].copy_from_slice(&port.to_be_bytes());
        address[4..8].copy_from_slice(&[127, 0, 0, 1]);
        let result = if udp {
            send_to(handle, b"probe".as_ptr(), 5, 0, address.as_ptr().cast(), 16)
        } else {
            connect(handle, address.as_ptr().cast(), 16)
        };
        let code = if result < 0 { error() as u32 } else { 0 };
        close(handle);
        if started == 0 {
            cleanup();
        }
        vec![started as u32, 0, code]
    }
}
