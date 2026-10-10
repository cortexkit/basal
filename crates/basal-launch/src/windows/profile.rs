//! AppContainer profile management and test ACL granting.

use crate::windows::error::LaunchError;
use crate::windows::native::{HRESULT, PCWSTR, utf16_ptr, wide};
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, Storage::FileSystem::*,
    System::LibraryLoader::*,
};

/// Fixed AppContainer profile name used for all basal workers.
pub const PROFILE_NAME: &str = "cortexkit.basal.worker";

type CreateAppContainerProfileFn = unsafe extern "system" fn(
    psz_app_container_name: PCWSTR,
    psz_display_name: PCWSTR,
    psz_description: PCWSTR,
    p_capabilities: *const SID_AND_ATTRIBUTES,
    dw_capability_count: u32,
    pp_sid_mapping: *mut PSID,
) -> HRESULT;

type DeriveAppContainerSidFromAppContainerNameFn = unsafe extern "system" fn(
    psz_app_container_name: PCWSTR,
    ppsid_app_container_sid: *mut PSID,
) -> HRESULT;

struct UserEnvModule(HMODULE);

impl Drop for UserEnvModule {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

impl UserEnvModule {
    fn load() -> Result<Self, LaunchError> {
        let h = unsafe {
            LoadLibraryExW(
                wide("userenv.dll").as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if h.is_null() {
            Err(LaunchError::AppContainerProfileUnavailable(
                "failed to load userenv.dll from System32".to_string(),
            ))
        } else {
            Ok(Self(h))
        }
    }

    unsafe fn symbol<T>(&self, name: &[u8]) -> Result<T, LaunchError> {
        let f = unsafe { GetProcAddress(self.0, name.as_ptr()) };
        if let Some(f) = f {
            Ok(unsafe { std::mem::transmute_copy(&f) })
        } else {
            Err(LaunchError::AppContainerProfileUnavailable(format!(
                "missing symbol in userenv.dll: {}",
                String::from_utf8_lossy(name)
            )))
        }
    }
}

/// An owned AppContainer SID.
#[derive(Clone, PartialEq, Eq)]
pub struct AppContainerSid {
    raw: Vec<u8>,
    string: String,
}

impl AppContainerSid {
    /// Creates an `AppContainerSid` from an existing Win32 `PSID`.
    pub unsafe fn from_psid(psid: PSID) -> Result<Self, LaunchError> {
        if psid.is_null() {
            return Err(LaunchError::AppContainerProfileUnavailable(
                "NULL PSID pointer".to_string(),
            ));
        }
        let len = unsafe { GetLengthSid(psid) };
        if len == 0 {
            return Err(LaunchError::AppContainerProfileUnavailable(
                "invalid SID length".to_string(),
            ));
        }
        let mut raw = vec![0u8; len as usize];
        if unsafe { CopySid(len, raw.as_mut_ptr().cast(), psid) } == 0 {
            return Err(LaunchError::AppContainerProfileUnavailable(
                "failed to copy SID".to_string(),
            ));
        }
        let mut string_ptr = null_mut();
        if unsafe { ConvertSidToStringSidW(raw.as_mut_ptr().cast(), &mut string_ptr) } == 0 {
            return Err(LaunchError::AppContainerProfileUnavailable(
                "failed to convert SID to string".to_string(),
            ));
        }
        let string = unsafe { utf16_ptr(string_ptr) };
        unsafe { LocalFree(string_ptr.cast()) };
        Ok(Self { raw, string })
    }

    /// Parses a string representation (e.g. `"S-1-15-2-..."`) into an `AppContainerSid`.
    pub fn from_string_sid(s: &str) -> Result<Self, LaunchError> {
        let mut psid = null_mut();
        let wide_str = wide(s);
        if unsafe { ConvertStringSidToSidW(wide_str.as_ptr(), &mut psid) } == 0 {
            return Err(LaunchError::AppContainerProfileUnavailable(format!(
                "invalid string SID: {s}"
            )));
        }
        let res = unsafe { Self::from_psid(psid) };
        unsafe { LocalFree(psid.cast()) };
        res
    }

    /// Returns a raw `PSID` pointer valid for the lifetime of this struct.
    pub fn as_psid(&self) -> PSID {
        self.raw.as_ptr() as PSID
    }

    /// Returns the string representation of this SID.
    pub fn as_str(&self) -> &str {
        &self.string
    }

    /// Checks whether this SID equals another `PSID`.
    pub fn matches(&self, other: PSID) -> bool {
        if other.is_null() {
            return false;
        }
        unsafe { EqualSid(self.as_psid(), other) != 0 }
    }
}

impl std::fmt::Debug for AppContainerSid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AppContainerSid({})", self.string)
    }
}

impl std::fmt::Display for AppContainerSid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.string)
    }
}

/// An opened or created AppContainer profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppContainerProfile {
    name: String,
    sid: AppContainerSid,
}

impl AppContainerProfile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sid(&self) -> &AppContainerSid {
        &self.sid
    }
}

static PROFILE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Creates or opens the named AppContainer profile with zero capabilities.
///
/// If the profile already exists, derives the same package SID.
/// Thread-safe and idempotent under concurrent calls.
/// Failure produces `LaunchError::AppContainerProfileUnavailable`.
pub fn create_or_open_profile(name: &str) -> Result<AppContainerProfile, LaunchError> {
    let _guard = PROFILE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let module = UserEnvModule::load()?;
    let create: CreateAppContainerProfileFn =
        unsafe { module.symbol(b"CreateAppContainerProfile\0")? };
    let derive: DeriveAppContainerSidFromAppContainerNameFn =
        unsafe { module.symbol(b"DeriveAppContainerSidFromAppContainerName\0")? };

    let wide_name = wide(name);
    let mut psid = null_mut();
    let hr = unsafe {
        create(
            wide_name.as_ptr(),
            wide_name.as_ptr(),
            wide("CortexKit basal worker").as_ptr(),
            null(),
            0,
            &mut psid,
        )
    };

    if hr >= 0 {
        let sid = unsafe { AppContainerSid::from_psid(psid) };
        unsafe { FreeSid(psid) };
        let sid = sid?;
        return Ok(AppContainerProfile {
            name: name.to_string(),
            sid,
        });
    }

    // Check for ERROR_ALREADY_EXISTS (183 = 0xB7; HRESULT = 0x800700B7)
    // or E_UNEXPECTED (0x8000FFFF) from concurrent creation
    let hr_u32 = hr as u32;
    let is_already_exists_or_unexpected =
        hr_u32 == 0x800700B7 || hr_u32 == 0x8000FFFF || (hr_u32 & 0xFFFF) == 183;
    if is_already_exists_or_unexpected {
        let mut derived_sid = null_mut();
        let hr_derive = unsafe { derive(wide_name.as_ptr(), &mut derived_sid) };
        if hr_derive >= 0 {
            let sid = unsafe { AppContainerSid::from_psid(derived_sid) };
            unsafe { FreeSid(derived_sid) };
            let sid = sid?;
            return Ok(AppContainerProfile {
                name: name.to_string(),
                sid,
            });
        }
    }

    Err(LaunchError::AppContainerProfileUnavailable(format!(
        "profile create/open failed for {name}: HRESULT 0x{hr:08x}"
    )))
}

/// Grants the package SID `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE` on the
/// directory of the worker binary about to run.
///
/// Test harnesses call this function; production paths do not.
pub fn grant_test_binary_directory(
    directory: impl AsRef<Path>,
    package_sid: &AppContainerSid,
) -> Result<(), LaunchError> {
    let path_str = directory
        .as_ref()
        .to_str()
        .ok_or_else(|| LaunchError::Other("directory path is not valid UTF-8".to_string()))?;
    let wide_path = wide(path_str);

    unsafe {
        let mut old_dacl = null_mut();
        let mut descriptor = null_mut();
        let err = GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut descriptor,
        );
        if err != 0 {
            return Err(LaunchError::Win32 {
                api: "GetNamedSecurityInfoW",
                code: err,
                detail: format!("failed to read DACL for {path_str}"),
            });
        }

        let ace = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: package_sid.as_psid() as *mut u16,
            },
        };

        let mut new_dacl = null_mut();
        let err = SetEntriesInAclW(1, &ace, old_dacl, &mut new_dacl);
        if err != 0 {
            LocalFree(descriptor.cast());
            return Err(LaunchError::Win32 {
                api: "SetEntriesInAclW",
                code: err,
                detail: "failed to build updated DACL with package ACE".to_string(),
            });
        }

        let err = SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null(),
        );

        LocalFree(new_dacl.cast());
        LocalFree(descriptor.cast());

        if err != 0 {
            return Err(LaunchError::Win32 {
                api: "SetNamedSecurityInfoW",
                code: err,
                detail: format!("failed to apply updated DACL for {path_str}"),
            });
        }

        Ok(())
    }
}
