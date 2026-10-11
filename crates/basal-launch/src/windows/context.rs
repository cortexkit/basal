//! The start-up context a worker gets: a private window station and desktop,
//! a private TEMP directory, a minimal environment and a working directory.

use super::native::{Result, check, groups, last, sid_string, token_buffer, wide};
use super::profile::{Library, PackageSid};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_USER, TokenGroups,
    TokenUser,
};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

use super::native::SE_GROUP_LOGON_ID;

/// `WINSTA_ALL_ACCESS | STANDARD_RIGHTS_REQUIRED`.
const STATION_ACCESS: u32 = 0x000f_037f;
/// Every desktop right plus the standard rights, requested by the creator.
const DESKTOP_ACCESS: u32 = 0x000f_01ff;

type CreateStation =
    unsafe extern "system" fn(*const u16, u32, u32, *const SECURITY_ATTRIBUTES) -> HANDLE;
type CreateDesktop = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const c_void,
    u32,
    u32,
    *const SECURITY_ATTRIBUTES,
) -> HANDLE;
type GetStation = unsafe extern "system" fn() -> HANDLE;
type SetStation = unsafe extern "system" fn(HANDLE) -> i32;
type CloseObject = unsafe extern "system" fn(HANDLE) -> i32;

/// Creating a desktop uses the process's current window station, which is
/// process-wide state; launches that switch it must not interleave.
static STATION_SWITCH: Mutex<()> = Mutex::new(());

/// A worker's start-up context. Dropping it closes the station and desktop
/// and removes the TEMP directory, so it must outlive the worker.
pub(crate) struct Context {
    /// Keeps User32 loaded until the close functions below have run.
    _user32: Library,
    station: HANDLE,
    desktop: HANDLE,
    close_station: CloseObject,
    close_desktop: CloseObject,
    /// `station\desktop`, NUL-terminated, for `STARTUPINFO.lpDesktop`.
    pub(crate) desktop_name: Vec<u16>,
    /// The working directory, NUL-terminated.
    pub(crate) cwd: Vec<u16>,
    /// The environment block: sorted `NAME=value` strings, each
    /// NUL-terminated, then one more NUL.
    pub(crate) environment: Vec<u16>,
    /// The private TEMP directory.
    pub(crate) temp: PathBuf,
}

// The station and desktop handles and the module handle are process-wide
// and may be closed from any thread.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            if !self.desktop.is_null() {
                (self.close_desktop)(self.desktop);
            }
            (self.close_station)(self.station);
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

/// A security descriptor parsed from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn parse(sddl: &str) -> Result<Self> {
        let mut descriptor = null_mut();
        check(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        )?;
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// The Windows directory, from the system rather than from this process's
/// own environment.
pub(crate) fn system_root() -> Result<String> {
    let mut buffer = [0u16; 260];
    let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(last("GetSystemWindowsDirectoryW"));
    }
    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
}

/// The environment block a worker gets, and nothing from the parent's own
/// environment: only the Windows directory variables the system DLLs read,
/// `PATH` limited to System32, and TEMP-style variables naming the private
/// directory. Names are sorted case-insensitively, as Windows requires of an
/// environment block.
pub(crate) fn environment_block(system_root: &str, temp: &str) -> Vec<u16> {
    let drive = system_root.get(..2).unwrap_or(system_root);
    let mut variables = [
        ("LOCALAPPDATA", temp.to_owned()),
        ("PATH", format!("{system_root}\\System32")),
        ("SYSTEMDRIVE", drive.to_owned()),
        ("SYSTEMROOT", system_root.to_owned()),
        ("TEMP", temp.to_owned()),
        ("TMP", temp.to_owned()),
        ("windir", system_root.to_owned()),
    ];
    variables.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    variables
        .iter()
        .flat_map(|(name, value)| {
            format!("{name}={value}")
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>()
        })
        .chain([0])
        .collect()
}

/// Creates the context for one worker.
///
/// - **Window station and desktop:** private to this worker. Only SYSTEM,
///   Administrators and the parent's user have full access; the logon SID,
///   the NULL SID and the package get read and execute on the station and
///   read-control, read-objects and write-objects (`0x20081`) on the
///   desktop. Both carry a Low no-write-up label.
/// - **TEMP:** a fresh directory with a protected DACL. The worker's package
///   and the NULL SID get only read and execute, so the variables resolve to
///   an existing directory in which the worker can create nothing.
/// - **Working directory:** the image's own directory, which the package
///   must already be allowed to read for the image to load at all.
pub(crate) fn create(image: &Path, parent_token: HANDLE, package: &PackageSid) -> Result<Context> {
    unsafe {
        let user32 = Library::system32("user32.dll")?;
        let create_station: CreateStation = user32.symbol(b"CreateWindowStationW\0")?;
        let create_desktop: CreateDesktop = user32.symbol(b"CreateDesktopW\0")?;
        let get_station: GetStation = user32.symbol(b"GetProcessWindowStation\0")?;
        let set_station: SetStation = user32.symbol(b"SetProcessWindowStation\0")?;
        let close_station: CloseObject = user32.symbol(b"CloseWindowStation\0")?;
        let close_desktop: CloseObject = user32.symbol(b"CloseDesktop\0")?;

        let user = token_buffer(parent_token, TokenUser)?;
        let user = sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid)?;
        let group_data = token_buffer(parent_token, TokenGroups)?;
        let logon = match groups(&group_data)
            .iter()
            .find(|group| group.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
        {
            Some(group) => Some(sid_string(group.Sid)?),
            None => None,
        };
        let package = package.as_str();
        let logon_station = logon
            .as_deref()
            .map(|sid| format!("(A;;GRGX;;;{sid})"))
            .unwrap_or_default();
        let logon_desktop = logon
            .as_deref()
            .map(|sid| format!("(A;;0x20081;;;{sid})"))
            .unwrap_or_default();
        let station_sd = Descriptor::parse(&format!(
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user}){logon_station}(A;;GRGX;;;S-1-0-0)(A;;GRGX;;;{package})S:(ML;;NW;;;LW)"
        ))?;
        let desktop_sd = Descriptor::parse(&format!(
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user}){logon_desktop}(A;;0x20081;;;S-1-0-0)(A;;0x20081;;;{package})S:(ML;;NW;;;LW)"
        ))?;

        let name = context_name()?;
        let station = create_station(
            wide(&name).as_ptr(),
            0,
            STATION_ACCESS,
            &station_sd.attributes(),
        );
        if station.is_null() {
            return Err(last("CreateWindowStationW"));
        }
        let mut context = Context {
            _user32: user32,
            station,
            desktop: null_mut(),
            close_station,
            close_desktop,
            desktop_name: wide(format!("{name}\\worker")),
            cwd: Vec::new(),
            environment: Vec::new(),
            temp: PathBuf::new(),
        };
        {
            let _switch = STATION_SWITCH
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let original = get_station();
            check(set_station(station), "SetProcessWindowStation(worker)")?;
            context.desktop = create_desktop(
                wide("worker").as_ptr(),
                null(),
                null(),
                0,
                DESKTOP_ACCESS,
                &desktop_sd.attributes(),
            );
            let desktop_error = context.desktop.is_null().then(|| last("CreateDesktopW"));
            // Restore the parent's station before anything else can run on it.
            check(set_station(original), "SetProcessWindowStation(restore)")?;
            if let Some(error) = desktop_error {
                return Err(error);
            }
        }

        let directory = image
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", image.display()))?;
        context.cwd = wide(directory);
        // `create_dir` fails on an existing name, so a directory that is
        // already there (left by a killed process, or planted) is never
        // reused; with an unpredictable name nobody can plant it in advance.
        let temp = std::env::temp_dir().join(&name);
        std::fs::create_dir(&temp)
            .map_err(|error| format!("create {}: {error}", temp.display()))?;
        // The system TEMP path often holds 8.3 short names (`RUNNER~1`);
        // give the worker the long form.
        context.temp = long_path(&temp);
        let temp_text = context.temp.to_string_lossy().into_owned();
        let temp_sd = Descriptor::parse(&format!(
            "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GA;;;{user})(A;OICI;GRGX;;;S-1-0-0)(A;OICI;GRGX;;;{package})S:(ML;OICI;NW;;;LW)"
        ))?;
        check(
            SetFileSecurityW(
                wide(&context.temp).as_ptr(),
                DACL_SECURITY_INFORMATION
                    | LABEL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                temp_sd.0,
            ),
            "SetFileSecurityW(worker TEMP)",
        )?;
        context.environment = environment_block(&system_root()?, &temp_text);
        Ok(context)
    }
}

/// A fresh name for one launch's window station and TEMP directory: 128
/// bits from the system's random number generator. A name made from the
/// process id and a counter repeats when a later process gets the same id,
/// and can be guessed and created by someone else first.
pub(crate) fn context_name() -> Result<String> {
    let mut random = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(format!("BCryptGenRandom: NTSTATUS {status:#x}"));
    }
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("cortexkit_basal_{hex}"))
}

/// The long form of an existing path, without the `\\?\` prefix that
/// canonicalization adds, or the path unchanged if it has no such form.
fn long_path(path: &Path) -> PathBuf {
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return path.to_owned();
    };
    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_names_are_random_and_never_repeat() {
        let names: std::collections::BTreeSet<String> =
            (0..64).map(|_| context_name().unwrap()).collect();
        assert_eq!(names.len(), 64);
        for name in &names {
            let hex = name.strip_prefix("cortexkit_basal_").unwrap();
            assert_eq!(hex.len(), 32, "{name}");
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
        }
    }
}
