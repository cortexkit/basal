//! Small helpers over the Win32 and native calls the launcher makes.

use std::ffi::{OsStr, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    CopySid, CreateWellKnownSid, GetLengthSid, GetTokenInformation, PSID, SID_AND_ATTRIBUTES,
    TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_PRIVILEGES, WELL_KNOWN_SID_TYPE,
};

pub(crate) type Result<T> = std::result::Result<T, String>;

/// `SE_GROUP_INTEGRITY`: marks the token's integrity label, which is not an
/// access group.
pub(crate) const SE_GROUP_INTEGRITY: u32 = 0x20;
/// `SE_GROUP_USE_FOR_DENY_ONLY`: the group matches only deny entries.
pub(crate) const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;
/// `SE_GROUP_LOGON_ID`: the group is the session's logon SID.
pub(crate) const SE_GROUP_LOGON_ID: u32 = 0xc000_0000;

/// A NUL-terminated UTF-16 copy of `text`.
pub(crate) fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

/// The calling thread's last Win32 error, named after the call that failed.
pub(crate) fn last(api: &str) -> String {
    let code = unsafe { GetLastError() };
    format!(
        "{api}: Win32 {code} ({})",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

/// Turns a Win32 `BOOL` result into a `Result`, naming the call.
pub(crate) fn check(ok: i32, api: &str) -> Result<()> {
    if ok == 0 { Err(last(api)) } else { Ok(()) }
}

/// Takes ownership of a handle a call returned, refusing the two failure
/// values.
pub(crate) fn owned(handle: HANDLE, api: &str) -> Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last(api))
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// Reads one variable-length token information class into an aligned buffer.
///
/// # Safety
///
/// `token` must be a valid token handle opened with `TOKEN_QUERY`.
pub(crate) unsafe fn token_buffer(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<usize>> {
    unsafe {
        let mut bytes = 0;
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
        if bytes == 0 {
            return Err(last(&format!("GetTokenInformation(class {class}, size)")));
        }
        // A list with zero entries can be shorter than the C declaration,
        // which reserves one entry. Keep room for that declaration so a
        // reference to it never reaches past the allocation.
        let allocation = (bytes as usize)
            .max(size_of::<TOKEN_GROUPS>())
            .max(size_of::<TOKEN_PRIVILEGES>());
        let mut data = vec![0usize; allocation.div_ceil(size_of::<usize>())];
        check(
            GetTokenInformation(token, class, data.as_mut_ptr().cast(), bytes, &mut bytes),
            &format!("GetTokenInformation(class {class})"),
        )?;
        Ok(data)
    }
}

/// The entries of a `TOKEN_GROUPS` buffer.
///
/// # Safety
///
/// `data` must hold a `TOKEN_GROUPS` structure returned by the system.
pub(crate) unsafe fn groups(data: &[usize]) -> &[SID_AND_ATTRIBUTES] {
    unsafe {
        let list = &*data.as_ptr().cast::<TOKEN_GROUPS>();
        std::slice::from_raw_parts(list.Groups.as_ptr(), list.GroupCount as usize)
    }
}

/// The `S-1-...` form of a SID.
///
/// # Safety
///
/// `sid` must point to a valid SID.
pub(crate) unsafe fn sid_string(sid: PSID) -> Result<String> {
    unsafe {
        let mut text = null_mut();
        check(
            ConvertSidToStringSidW(sid, &mut text),
            "ConvertSidToStringSidW",
        )?;
        let result = utf16_until_nul(text);
        LocalFree(text.cast());
        Ok(result)
    }
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
///
/// `text` must point to a NUL-terminated UTF-16 string.
pub(crate) unsafe fn utf16_until_nul(text: *const u16) -> String {
    unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// A SID held in memory this process owns.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SidBuf(Vec<u32>);

impl SidBuf {
    /// The SID of a well-known kind, such as the NULL SID or an integrity label.
    pub(crate) fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self> {
        // SECURITY_MAX_SID_SIZE is 68 bytes.
        let mut sid = vec![0u32; 17];
        let mut bytes = (sid.len() * 4) as u32;
        check(
            unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &mut bytes) },
            "CreateWellKnownSid",
        )?;
        Ok(Self(sid))
    }

    /// Parses the `S-1-...` form.
    #[cfg_attr(not(feature = "deviations"), allow(dead_code))]
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let mut sid = null_mut();
        check(
            unsafe { ConvertStringSidToSidW(wide(text).as_ptr(), &mut sid) },
            "ConvertStringSidToSidW",
        )?;
        let copy = unsafe { Self::copy(sid) };
        unsafe { LocalFree(sid) };
        copy
    }

    /// Copies a SID the system returned.
    ///
    /// # Safety
    ///
    /// `sid` must point to a valid SID.
    pub(crate) unsafe fn copy(sid: PSID) -> Result<Self> {
        unsafe {
            let bytes = GetLengthSid(sid);
            let mut buffer = vec![0u32; (bytes as usize).div_ceil(4)];
            check(CopySid(bytes, buffer.as_mut_ptr().cast(), sid), "CopySid")?;
            Ok(Self(buffer))
        }
    }

    /// A pointer for calls that read the SID. Callers that write through it
    /// must not exist; the pointer is mutable only because the API types are.
    pub(crate) fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast::<c_void>()
    }

    pub(crate) fn to_text(&self) -> Result<String> {
        unsafe { sid_string(self.as_psid()) }
    }
}
