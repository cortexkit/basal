//! OS confinement for the worker process.
//!
//! The worker runs scripts that are approved but still untrusted input to a
//! C engine, so it assumes the engine can be subverted and makes sure a
//! subverted worker can do nothing but talk to its parent. Before it reads
//! any frame it closes every inherited descriptor except stdio. Linux applies
//! an empty Landlock ruleset and a mandatory fatal seccomp allowlist; macOS
//! applies a deny-by-default Seatbelt profile through `sandbox_init`, the
//! mechanism Chromium's helper processes use. The macOS API is deprecated but
//! enforced on current systems. Once applied, these boundaries cannot be lifted.

use std::fmt;

use basal_proto::Confinement;

#[cfg(target_os = "linux")]
pub mod linux;

/// The Seatbelt profile, checked in beside the crate and embedded at build
/// time so the binary cannot be pointed at a different one.
pub const SEATBELT_PROFILE: &str = include_str!("../sandbox/worker.sb");

/// Why the worker could not confine itself. The worker refuses to run any
/// script in that case.
#[derive(Debug)]
pub enum ConfinementError {
    /// `sandbox_init` refused the profile.
    Seatbelt(String),
    /// Inherited descriptors could not be listed or closed.
    Descriptors(String),
    /// This platform has no supported sandbox.
    Unsupported,
    /// A Linux startup precondition or confinement layer failed.
    #[cfg(target_os = "linux")]
    Linux(&'static str),
}

impl fmt::Display for ConfinementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seatbelt(e) => write!(f, "sandbox_init failed: {e}"),
            Self::Descriptors(e) => write!(f, "could not close inherited descriptors: {e}"),
            Self::Unsupported => write!(f, "no supported OS sandbox on this platform"),
            #[cfg(target_os = "linux")]
            Self::Linux(token) => f.write_str(token),
        }
    }
}

impl std::error::Error for ConfinementError {}

/// The state the worker is left in once confined.
#[derive(Debug)]
pub struct Entered {
    pub confinement: Confinement,
    /// Descriptors still open after inherited ones were closed, listed just
    /// before the sandbox applied (listing them afterwards is itself denied).
    pub descriptors: Vec<i32>,
}

/// Closes inherited descriptors and applies the sandbox.
#[cfg(not(target_os = "linux"))]
pub fn enter() -> Result<Entered, ConfinementError> {
    close_inherited_descriptors()?;
    let descriptors = open_descriptors();
    apply_seatbelt()?;
    Ok(Entered {
        confinement: Confinement::Seatbelt,
        descriptors,
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, CString, c_char, c_int};

    unsafe extern "C" {
        // From libsandbox, part of libSystem. Declared in <sandbox.h>.
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    pub fn apply(profile: &str) -> Result<(), String> {
        let profile =
            CString::new(profile).map_err(|_| "profile contains a NUL byte".to_owned())?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: `profile` is a valid NUL-terminated string and `error` is a
        // valid out-pointer. Flags 0 means the string is SBPL source.
        let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
        if rc == 0 {
            return Ok(());
        }
        let message = if error.is_null() {
            format!("sandbox_init returned {rc}")
        } else {
            // SAFETY: on failure sandbox_init sets `error` to a NUL-terminated
            // string that must be released with sandbox_free_error.
            let text = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { sandbox_free_error(error) };
            text
        };
        Err(message)
    }

    /// Lists this process's open descriptors with `proc_pidinfo`, which needs
    /// no file access and costs one call however high the descriptor limit is.
    pub fn open_descriptors() -> Result<Vec<i32>, String> {
        let pid = std::process::id() as c_int;
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        let mut capacity = 64usize;
        loop {
            let mut buf: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
            let bytes = (capacity * entry) as c_int;
            // SAFETY: the buffer has room for `capacity` entries and the size
            // passed is exactly that many bytes.
            let used = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    buf.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if used <= 0 {
                return Err(format!(
                    "proc_pidinfo failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let count = used as usize / entry;
            if count < capacity {
                // SAFETY: proc_pidinfo initialised `count` entries.
                unsafe { buf.set_len(count) };
                return Ok(buf.iter().map(|info| info.proc_fd).collect());
            }
            capacity *= 2;
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_seatbelt() -> Result<(), ConfinementError> {
    macos::apply(SEATBELT_PROFILE).map_err(ConfinementError::Seatbelt)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn apply_seatbelt() -> Result<(), ConfinementError> {
    Err(ConfinementError::Unsupported)
}

/// The worker should inherit nothing but stdio, and the parent spawns it
/// that way; this closes anything else regardless, so a parent bug cannot
/// hand the engine a descriptor to a store, socket, or daemon launch nonce.
/// A failed close refuses startup: the sandbox denies new opens, but does not
/// revoke access through an inherited descriptor.
#[cfg(target_os = "macos")]
pub fn close_inherited_descriptors() -> Result<(), ConfinementError> {
    for fd in macos::open_descriptors().map_err(ConfinementError::Descriptors)? {
        if fd > 2 {
            // SAFETY: closing a descriptor this process owns. Nothing in the
            // worker holds descriptors above 2 at this point in startup.
            if unsafe { libc::close(fd) } != 0 {
                return Err(ConfinementError::Descriptors(format!(
                    "descriptor {fd}: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn close_inherited_descriptors() -> Result<(), ConfinementError> {
    Err(ConfinementError::Unsupported)
}

#[cfg(target_os = "linux")]
pub fn enter() -> Result<Entered, ConfinementError> {
    linux::enter(&linux::Options::default())
}

#[cfg(target_os = "linux")]
pub fn close_inherited_descriptors() -> Result<(), ConfinementError> {
    linux::close_descriptors(false).map(|_| ())
}

#[cfg(target_os = "linux")]
pub fn open_descriptors() -> Vec<i32> {
    linux::descriptors().unwrap_or_default()
}

/// The descriptors currently open.
#[cfg(target_os = "macos")]
pub fn open_descriptors() -> Vec<i32> {
    macos::open_descriptors().unwrap_or_default()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn open_descriptors() -> Vec<i32> {
    Vec::new()
}
