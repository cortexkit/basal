//! The start-up context a worker gets: a private desktop in a shared window
//! station, a private TEMP directory, a minimal environment and a working directory.

use super::native::{Result, check, groups, last, sid_string, token_buffer, wide};
use super::profile::{Library, PackageSid};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, HANDLE, LocalFree};
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

/// `GENERIC_READ | WINSTA_CREATEDESKTOP`: read and create private desktops.
const STATION_ACCESS: u32 = 0x8000_0008;
/// `WINSTA_READATTRIBUTES | WINSTA_CREATEDESKTOP`: the smaller requested access
/// that an existing batch-logon station may permit.
const STATION_RETRY_ACCESS: u32 = 0x0000_000a;
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
type GetObjectInformation =
    unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32;

/// Creating a desktop uses the process's current window station, which is
/// process-wide state; launches that switch it must not interleave.
static STATION_SWITCH: Mutex<()> = Mutex::new(());

/// NULL-name creation works without administrator rights. Windows chooses
/// the station's name from the logon session, so it is not a per-worker
/// isolation boundary. Retain one noninheritable handle for the process's
/// lifetime; Windows closes it on process termination (Rust statics do not drop).
static STATION: OnceLock<Result<Station>> = OnceLock::new();

struct Station {
    _user32: Library,
    handle: HANDLE,
    close: CloseObject,
    name: String,
    mode: &'static str,
}

// The cached station is immutable, and its handle is valid on every thread.
unsafe impl Send for Station {}
unsafe impl Sync for Station {}

impl Drop for Station {
    fn drop(&mut self) {
        unsafe { (self.close)(self.handle) };
    }
}

fn shared_station(descriptor: &Descriptor) -> Result<&'static Station> {
    STATION
        .get_or_init(|| unsafe {
            let _switch = STATION_SWITCH
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let user32 = Library::system32("user32.dll")?;
            let create: CreateStation = user32.symbol(b"CreateWindowStationW\0")?;
            let current: GetStation = user32.symbol(b"GetProcessWindowStation\0")?;
            let current_name = object_name(&user32, current())?;
            let close = user32.symbol(b"CloseWindowStation\0")?;
            let mut handle = create(null(), 0, STATION_ACCESS, &descriptor.attributes());
            if handle.is_null() && GetLastError() == ERROR_ACCESS_DENIED {
                // The logon-session station can already exist. Reopening it
                // with a smaller requested access mask must not rewrite its security.
                handle = create(null(), 0, STATION_RETRY_ACCESS, &descriptor.attributes());
            }
            if handle.is_null() {
                return Err(last("CreateWindowStationW"));
            }
            let mut station = Station {
                _user32: user32,
                handle,
                close,
                name: String::new(),
                mode: "alternate",
            };
            station.name = object_name(&station._user32, handle)?;
            if station.name.eq_ignore_ascii_case(&current_name) {
                station.mode = "current";
            }
            Ok(station)
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn object_name(user32: &Library, object: HANDLE) -> Result<String> {
    unsafe {
        let get: GetObjectInformation = user32.symbol(b"GetUserObjectInformationW\0")?;
        let mut bytes = 0;
        // UOI_NAME is 2; both buffer sizes are in bytes, not UTF-16 units.
        get(object, 2, null_mut(), 0, &mut bytes);
        if bytes == 0 || bytes % 2 != 0 {
            return Err(last("GetUserObjectInformationW(UOI_NAME size)"));
        }
        let mut name = vec![0u16; bytes as usize / 2];
        check(
            get(object, 2, name.as_mut_ptr().cast(), bytes, &mut bytes),
            "GetUserObjectInformationW(UOI_NAME)",
        )?;
        let length = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        if length == 0 {
            return Err("GetUserObjectInformationW(UOI_NAME): empty name".into());
        }
        Ok(String::from_utf16_lossy(&name[..length]))
    }
}

/// Select the worker station for parent-side isolation probes, excluding
/// concurrent desktop creation. On panic the drop guard attempts restoration.
#[cfg(feature = "deviations")]
pub(crate) fn with_station<T>(run: impl FnOnce() -> T) -> Result<T> {
    let station = STATION
        .get()
        .ok_or("the worker station has not been initialized")?
        .as_ref()
        .map_err(Clone::clone)?;
    let _switch = STATION_SWITCH
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    unsafe {
        let get: GetStation = station._user32.symbol(b"GetProcessWindowStation\0")?;
        let set: SetStation = station._user32.symbol(b"SetProcessWindowStation\0")?;
        struct Restore(HANDLE, SetStation);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { (self.1)(self.0) };
            }
        }
        let original = get();
        if object_name(&station._user32, original)?.eq_ignore_ascii_case(&station.name) {
            return Ok(run());
        }
        // The station handle used for desktop creation lacks atom access. The
        // parent probe needs its own authorized atom-capable handle, otherwise
        // a denied parent lookup would masquerade as cross-job isolation.
        type OpenStation = unsafe extern "system" fn(*const u16, i32, u32) -> HANDLE;
        let open: OpenStation = station._user32.symbol(b"OpenWindowStationW\0")?;
        // WINSTA_ENUMDESKTOPS | WINSTA_READATTRIBUTES | WINSTA_ACCESSGLOBALATOMS.
        let handle = open(wide(&station.name).as_ptr(), 0, 0x23);
        if handle.is_null() {
            return Err(last("OpenWindowStationW(atom probe)"));
        }
        struct OwnedStation(HANDLE, CloseObject);
        impl Drop for OwnedStation {
            fn drop(&mut self) {
                unsafe { (self.1)(self.0) };
            }
        }
        let owned = OwnedStation(handle, station.close);
        check(set(handle), "SetProcessWindowStation(probe)")?;
        let restore = Restore(original, set);
        let result = run();
        check(set(original), "SetProcessWindowStation(probe restore)")?;
        drop(restore);
        drop(owned);
        Ok(result)
    }
}

/// A worker's start-up context. Dropping it closes the desktop and removes
/// the TEMP directory, so it must outlive the worker. The station is process-wide.
pub(crate) struct Context {
    /// Keeps User32 loaded until the close functions below have run.
    _user32: Library,
    desktop: HANDLE,
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

// The desktop and module handles may be closed from any thread.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            if !self.desktop.is_null() {
                (self.close_desktop)(self.desktop);
            }
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
/// - **Window station:** shared by this process's workers. A newly created
///   station grants SYSTEM, Administrators and the parent's user full access,
///   and the logon, NULL and package SIDs read/execute, with a Low no-write-up
///   label. An existing logon-session station is reused without changing its
///   security. Every worker gets a private desktop with those full-access
///   access-control entries, read-control/read-objects/write-objects (`0x20081`) for the
///   logon/NULL/package SIDs, and a Low no-write-up label.
/// - **TEMP:** a fresh directory with a protected DACL. The worker's package
///   and the NULL SID get only read and execute, so the variables resolve to
///   an existing directory in which the worker can create nothing.
/// - **Working directory:** the image's own directory, which the package
///   must already be allowed to read for the image to load at all.
pub(crate) fn create(image: &Path, parent_token: HANDLE, package: &PackageSid) -> Result<Context> {
    unsafe {
        let user32 = Library::system32("user32.dll")?;
        let create_desktop: CreateDesktop = user32.symbol(b"CreateDesktopW\0")?;
        let get_station: GetStation = user32.symbol(b"GetProcessWindowStation\0")?;
        let set_station: SetStation = user32.symbol(b"SetProcessWindowStation\0")?;
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
        let station = shared_station(&station_sd)?;
        eprintln!(
            "basal-launch: window station mode={} station={:?} desktop={name:?}",
            station.mode, station.name
        );
        let mut context = Context {
            _user32: user32,
            desktop: null_mut(),
            close_desktop,
            desktop_name: wide(format!("{}\\{name}", station.name)),
            cwd: Vec::new(),
            environment: Vec::new(),
            temp: PathBuf::new(),
        };
        {
            let _switch = STATION_SWITCH
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let original = get_station();
            check(
                set_station(station.handle),
                "SetProcessWindowStation(worker)",
            )?;
            context.desktop = create_desktop(
                wide(&name).as_ptr(),
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

/// A fresh name for one launch's desktop and TEMP directory: 128
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
    use std::os::windows::io::AsRawHandle;

    #[test]
    fn concurrent_contexts_share_one_station_and_have_distinct_desktops() {
        let barrier = std::sync::Barrier::new(2);
        let contexts: Vec<_> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let token = super::super::token::own_token().unwrap();
                        let package = super::super::profile::create_or_open_profile().unwrap();
                        create(
                            &std::env::current_exe().unwrap(),
                            token.as_raw_handle(),
                            &package,
                        )
                        .expect("create a worker context without administrator rights")
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        let names: Vec<_> = contexts
            .iter()
            .map(|context| {
                let full = String::from_utf16_lossy(
                    &context.desktop_name[..context.desktop_name.len() - 1],
                );
                let (station, desktop) = full.split_once('\\').expect("station\\desktop");
                assert_eq!(
                    object_name(&context._user32, context.desktop).unwrap(),
                    desktop
                );
                (station.to_owned(), desktop.to_owned())
            })
            .collect();
        println!("concurrent worker contexts: {names:?}");
        assert!(!names[0].0.is_empty());
        assert_eq!(names[0].0, names[1].0, "one station per process");
        assert_ne!(names[0].1, names[1].1, "a private desktop per worker");
        let station = STATION.get().unwrap().as_ref().unwrap();
        assert_eq!(
            object_name(&station._user32, station.handle).unwrap(),
            names[0].0
        );
        drop(contexts);
        assert_eq!(
            object_name(&station._user32, station.handle).unwrap(),
            names[0].0,
            "dropping all workers must not close the process-wide station"
        );
    }

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
