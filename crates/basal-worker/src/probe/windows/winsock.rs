//! Winsock is resolved only after entering probe mode. The engine has no socket
//! code path and must not acquire a static Winsock import just for these tests.
#![allow(unsafe_op_in_unsafe_fn)]
use super::{last, wide};
use std::mem::{transmute, zeroed};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
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
struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}

pub(super) unsafe fn attempt(probe: &str, port: u16) -> Result<Vec<u32>, String> {
    let module = LoadLibraryExW(
        wide("ws2_32.dll").as_ptr(),
        null_mut(),
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    );
    if module.is_null() {
        return Err(format!("LoadLibraryExW(ws2_32): {}", last()));
    }
    let _module = Library(module);
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
    let mut data = zeroed();
    let started = startup(0x202, &mut data);
    let udp = probe == "udp";
    let handle = socket(2, if udp { 2 } else { 1 }, if udp { 17 } else { 6 });
    if handle == usize::MAX {
        let code = error() as u32;
        if started == 0 {
            cleanup();
        }
        return Ok(vec![started as u32, code]);
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
    Ok(vec![started as u32, 0, code])
}
