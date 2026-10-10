//! The AppContainer profile every worker shares, and the test-only grant on
//! the directory of a worker binary.

use super::error::{LaunchError, Refusal};
use super::native::{Result, SidBuf, check, last, owned, wide};
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{
    FreeLibrary, HMODULE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, FreeSid, PSID, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};

/// The name of the one AppContainer profile all basal workers run under.
pub const PROFILE_NAME: &str = "cortexkit.basal.worker";

/// Serializes profile creation across every process of this user's session,
/// so concurrent first launches see one creation and later ones an existing
/// profile.
const PROFILE_MUTEX_NAME: &str = "Local\\cortexkit.basal.worker.profile";

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
const HRESULT_ALREADY_EXISTS: i32 = 0x8007_00b7_u32 as i32;

type CreateProfile = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const u16,
    *const SID_AND_ATTRIBUTES,
    u32,
    *mut PSID,
) -> i32;
type DeriveSid = unsafe extern "system" fn(*const u16, *mut PSID) -> i32;

/// The package SID of an AppContainer profile.
#[derive(Clone, PartialEq, Eq)]
pub struct PackageSid {
    sid: SidBuf,
    text: String,
}

impl PackageSid {
    /// The `S-1-15-2-...` form, as passed to the worker in `--package-sid`.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn as_psid(&self) -> PSID {
        self.sid.as_psid()
    }

    /// Takes a SID the profile API returned and frees the original.
    ///
    /// # Safety
    ///
    /// `sid` must be a valid SID allocated with `AllocateAndInitializeSid`,
    /// as the profile API documents.
    unsafe fn adopt(sid: PSID) -> Result<Self> {
        let copy = unsafe { SidBuf::copy(sid) };
        unsafe { FreeSid(sid) };
        let sid = copy?;
        let text = sid.to_text()?;
        Ok(Self { sid, text })
    }
}

impl std::fmt::Debug for PackageSid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PackageSid({})", self.text)
    }
}

impl std::fmt::Display for PackageSid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Creates the shared worker profile, or opens it if it exists, with no
/// capabilities. Either way the result is the profile's package SID.
///
/// A failure is the named refusal `appcontainer-profile-unavailable`; no
/// worker is ever started without a profile.
pub fn create_or_open_profile() -> std::result::Result<PackageSid, LaunchError> {
    create_or_open_named(PROFILE_NAME).map_err(unavailable)
}

pub(crate) fn unavailable(detail: String) -> LaunchError {
    LaunchError::refused(Refusal::AppContainerProfileUnavailable, detail)
}

pub(crate) fn create_or_open_named(name: &str) -> Result<PackageSid> {
    // Userenv pulls User32 and other GUI libraries in when it loads. It is
    // loaded here, at run time, only in the parent, so that no image linking
    // this crate imports it.
    let userenv = Library::system32("userenv.dll")?;
    let create: CreateProfile = unsafe { userenv.symbol(b"CreateAppContainerProfile\0")? };
    let derive: DeriveSid =
        unsafe { userenv.symbol(b"DeriveAppContainerSidFromAppContainerName\0")? };
    let _guard = SessionMutex::acquire(PROFILE_MUTEX_NAME)?;
    let name_w = wide(name);
    let description = wide("CortexKit basal flow worker");
    let mut sid = null_mut();
    let hr = unsafe {
        create(
            name_w.as_ptr(),
            name_w.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if hr >= 0 {
        return unsafe { PackageSid::adopt(sid) };
    }
    if hr != HRESULT_ALREADY_EXISTS {
        return Err(format!(
            "CreateAppContainerProfile({name}): HRESULT {:#010x}",
            hr as u32
        ));
    }
    // The profile exists. Its package SID is a fixed function of its name.
    let mut sid = null_mut();
    let hr = unsafe { derive(name_w.as_ptr(), &mut sid) };
    if hr < 0 {
        return Err(format!(
            "DeriveAppContainerSidFromAppContainerName({name}): HRESULT {:#010x}",
            hr as u32
        ));
    }
    unsafe { PackageSid::adopt(sid) }
}

/// Grants the package SID read and execute on the directory holding
/// `binary`, inherited by its contents, and changes no other entry of any ACL.
///
/// An AppContainer process cannot load an image, or open its working
/// directory, from a directory that grants its package nothing. In production
/// that grant is part of installation. Test harnesses, which run binaries
/// from a build directory, call this instead; the production launch path
/// never does.
pub fn grant_test_binary_directory(
    binary: &Path,
    package: &PackageSid,
) -> std::result::Result<(), LaunchError> {
    static GRANTS: Mutex<()> = Mutex::new(());
    let directory = binary
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", binary.display()))?;
    let path = wide(directory);
    // Reading and rewriting a DACL is not atomic; serialize this process's
    // grants so two concurrent ones cannot drop each other's entry.
    let _guard = GRANTS.lock().unwrap_or_else(|poison| poison.into_inner());
    unsafe {
        let mut old = null_mut();
        let mut descriptor = null_mut();
        let error = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old,
            null_mut(),
            &mut descriptor,
        );
        if error != 0 {
            return Err(format!(
                "GetNamedSecurityInfoW({}): Win32 {error}",
                directory.display()
            )
            .into());
        }
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: package.as_psid().cast(),
            },
        };
        let mut new = null_mut();
        let error = SetEntriesInAclW(1, &entry, old, &mut new);
        if error != 0 {
            LocalFree(descriptor);
            return Err(format!("SetEntriesInAclW: Win32 {error}").into());
        }
        let error = SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new,
            null(),
        );
        LocalFree(new.cast());
        LocalFree(descriptor);
        if error != 0 {
            return Err(format!(
                "SetNamedSecurityInfoW({}): Win32 {error}",
                directory.display()
            )
            .into());
        }
    }
    Ok(())
}

/// A DLL loaded from System32 only, never from the search path.
pub(crate) struct Library(HMODULE);

impl Library {
    pub(crate) fn system32(name: &str) -> Result<Self> {
        let module = unsafe {
            LoadLibraryExW(
                wide(name).as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            Err(last(&format!("LoadLibraryExW({name})")))
        } else {
            Ok(Self(module))
        }
    }

    /// Looks up an export.
    ///
    /// # Safety
    ///
    /// `T` must be the function pointer type of the export, and `name` must
    /// end with a NUL byte.
    pub(crate) unsafe fn symbol<T: Copy>(&self, name: &[u8]) -> Result<T> {
        debug_assert_eq!(size_of::<T>(), size_of::<*const c_void>());
        let function = unsafe { GetProcAddress(self.0, name.as_ptr()) }.ok_or_else(|| {
            last(&format!(
                "GetProcAddress({})",
                String::from_utf8_lossy(&name[..name.len().saturating_sub(1)])
            ))
        })?;
        Ok(unsafe { std::mem::transmute_copy(&function) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}

// The module handle is process-wide and FreeLibrary may run on any thread.
unsafe impl Send for Library {}

/// A named mutex held for the session, released on drop.
struct SessionMutex(std::os::windows::io::OwnedHandle);

impl SessionMutex {
    fn acquire(name: &str) -> Result<Self> {
        let handle = owned(
            unsafe { CreateMutexW(null(), 0, wide(name).as_ptr()) },
            "CreateMutexW(profile)",
        )?;
        // An abandoned mutex is still acquired: its previous holder died, and
        // profile creation is safe to repeat.
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            _ => Err(last("WaitForSingleObject(profile mutex)")),
        }
    }
}

impl Drop for SessionMutex {
    fn drop(&mut self) {
        let _ = check(
            unsafe { ReleaseMutex(self.0.as_raw_handle()) },
            "ReleaseMutex(profile)",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// A profile used only by the race test below, so deleting it cannot
    /// disturb launches other tests make under the shared profile.
    const RACE_PROFILE: &str = "cortexkit.basal.launch-test.race";

    fn delete_profile(name: &str) {
        type DeleteProfile = unsafe extern "system" fn(*const u16) -> i32;
        let userenv = Library::system32("userenv.dll").unwrap();
        let delete: DeleteProfile =
            unsafe { userenv.symbol(b"DeleteAppContainerProfile\0").unwrap() };
        // Deleting a profile that does not exist also succeeds.
        let hr = unsafe { delete(wide(name).as_ptr()) };
        assert!(
            hr >= 0,
            "DeleteAppContainerProfile({name}): {:#010x}",
            hr as u32
        );
    }

    fn derived(name: &str) -> String {
        let userenv = Library::system32("userenv.dll").unwrap();
        let derive: DeriveSid = unsafe {
            userenv
                .symbol(b"DeriveAppContainerSidFromAppContainerName\0")
                .unwrap()
        };
        let mut sid = null_mut();
        let hr = unsafe { derive(wide(name).as_ptr(), &mut sid) };
        assert!(
            hr >= 0,
            "DeriveAppContainerSidFromAppContainerName: {:#010x}",
            hr as u32
        );
        unsafe { PackageSid::adopt(sid) }
            .unwrap()
            .as_str()
            .to_owned()
    }

    fn race(name: &'static str, threads: usize) -> Vec<String> {
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    create_or_open_named(name).map(|sid| sid.as_str().to_owned())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("thread panicked")
                    .expect("create or open")
            })
            .collect()
    }

    /// Eight threads create a profile that does not exist yet, all at once.
    /// Exactly the race a first launch from concurrent tests meets: every
    /// one of them gets the profile's one package SID.
    #[test]
    fn concurrent_first_creation_yields_one_package_sid() {
        delete_profile(RACE_PROFILE);
        let sids = race(RACE_PROFILE, 8);
        let expected = derived(RACE_PROFILE);
        println!(
            "race profile {RACE_PROFILE}: {} creations, package SID {expected}",
            sids.len()
        );
        assert!(expected.starts_with("S-1-15-2-"), "{expected}");
        assert!(
            sids.iter().all(|sid| *sid == expected),
            "{sids:?} != {expected}"
        );
        delete_profile(RACE_PROFILE);
    }

    /// The shared worker profile, opened concurrently while it exists,
    /// always yields the SID derived from its name.
    #[test]
    fn concurrent_opens_of_the_worker_profile_agree() {
        let sids = race(PROFILE_NAME, 8);
        let expected = derived(PROFILE_NAME);
        println!("worker profile {PROFILE_NAME}: package SID {expected}");
        assert!(
            sids.iter().all(|sid| *sid == expected),
            "{sids:?} != {expected}"
        );
    }
}
