//! The test harness supplies native registry and object-manager paths. In
//! particular, it resolves the AppContainer's real storage key under its token;
//! a guessed HKCU spelling can name a missing key rather than test a denial.
#![allow(unsafe_op_in_unsafe_fn)]
use super::{Handle, wide};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{PSID, SECURITY_QUALITY_OF_SERVICE, SecurityIdentification};

#[repr(C)]
pub(crate) struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}
#[repr(C)]
pub(crate) struct ObjectAttributes {
    pub length: u32,
    pub root: HANDLE,
    pub name: *mut UnicodeString,
    pub attributes: u32,
    pub descriptor: *mut c_void,
    pub qos: *mut c_void,
}
#[repr(C)]
struct AlpcAttributes {
    flags: u32,
    qos: SECURITY_QUALITY_OF_SERVICE,
    max_message: usize,
    bandwidth: usize,
    max_pool: usize,
    max_section: usize,
    max_view: usize,
    max_total: usize,
    dup_types: u32,
    reserved: u32,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtOpenSection(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtOpenKey(out: *mut HANDLE, access: u32, attrs: *mut ObjectAttributes) -> i32;
    fn NtSetValueKey(
        key: HANDLE,
        name: *mut UnicodeString,
        title: u32,
        ty: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
    fn NtAlpcConnectPort(
        out: *mut HANDLE,
        name: *mut UnicodeString,
        attrs: *mut ObjectAttributes,
        port_attrs: *mut AlpcAttributes,
        flags: u32,
        sid: PSID,
        message: *mut c_void,
        size: *mut usize,
        out_message: *mut c_void,
        in_message: *mut c_void,
        timeout: *mut i64,
    ) -> i32;
}
pub(crate) fn us(text: &mut [u16]) -> UnicodeString {
    assert!(text.len() * 2 <= u16::MAX as usize);
    UnicodeString {
        length: ((text.len() - 1) * 2) as u16,
        maximum_length: (text.len() * 2) as u16,
        buffer: text.as_mut_ptr(),
    }
}
pub(crate) fn oa(name: &mut UnicodeString) -> ObjectAttributes {
    ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: null_mut(),
        name,
        attributes: 0x40,
        descriptor: null_mut(),
        qos: null_mut(),
    }
}
pub(super) unsafe fn section(path: &str) -> u32 {
    let mut text = wide(path);
    let mut name = us(&mut text);
    let mut attrs = oa(&mut name);
    let mut handle = null_mut();
    let status = NtOpenSection(&mut handle, 1, &mut attrs);
    if status == 0 {
        drop(Handle(handle));
    }
    status as u32
}
pub(super) unsafe fn alpc(path: &str) -> u32 {
    let mut text = wide(path);
    let mut name = us(&mut text);
    let mut attrs: AlpcAttributes = zeroed();
    attrs.max_message = 256;
    attrs.qos = SECURITY_QUALITY_OF_SERVICE {
        Length: size_of::<SECURITY_QUALITY_OF_SERVICE>() as u32,
        ImpersonationLevel: SecurityIdentification,
        ContextTrackingMode: 0,
        EffectiveOnly: true,
    };
    let mut handle = null_mut();
    let mut timeout = -1_000_000i64;
    let status = NtAlpcConnectPort(
        &mut handle,
        &mut name,
        null_mut(),
        &mut attrs,
        0,
        null_mut(),
        null_mut(),
        null_mut(),
        null_mut(),
        null_mut(),
        &mut timeout,
    );
    if status == 0 {
        drop(Handle(handle));
    }
    status as u32
}
pub(super) unsafe fn registry_write(path: &str) -> u32 {
    let mut text = wide(path);
    let mut name = us(&mut text);
    let mut attrs = oa(&mut name);
    let mut key = null_mut();
    let status = NtOpenKey(&mut key, 2, &mut attrs);
    if status != 0 {
        return status as u32;
    }
    let key = Handle(key);
    let mut text = wide("ckdev-confinement-fixture");
    let mut value = us(&mut text);
    NtSetValueKey(key.0, &mut value, 0, 3, [1u8].as_ptr().cast(), 1) as u32
}
