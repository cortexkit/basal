//! The startup sequence: the six checks in order, before the first frame
//! read. See the parent module for what each step guards.

use super::allowlist::{self, closed_at_startup};
use super::native::{self, CURRENT_PROCESS_TOKEN, OBJ_INHERIT, PackageSid, close, last};
use super::{Arguments, Refusal, attest, mitigation, reason};
use crate::confinement::{ConfinementError, Entered};
use basal_proto::Confinement;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{ERROR_NO_TOKEN, GetLastError, HANDLE};
use windows_sys::Win32::Security::{RevertToSelf, TOKEN_ADJUST_DEFAULT, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

/// Runs the six startup checks. On success the worker is fully confined and
/// may read input; the report it sends in its welcome is all true, because a
/// worker that failed any check has already exited.
pub fn enter(package: &PackageSid, arguments: &Arguments) -> Result<Entered, ConfinementError> {
    run(package, arguments).map_err(ConfinementError::Windows)?;
    Ok(Entered {
        confinement: Confinement::Windows {
            lpac: true,
            untrusted: true,
            no_thread_token: true,
            mitigations: true,
            handle_table: true,
        },
        descriptors: Vec::new(),
    })
}

fn run(package: &PackageSid, arguments: &Arguments) -> Result<(), Refusal> {
    // The handle used to lower the primary token is opened first, while the
    // main thread still impersonates the start-up token. Once the thread has
    // reverted, access checks use the restricted primary, which is not
    // allowed to open its own token for adjustment.
    // A failure here is reported by step 2, after step 1 has run.
    let adjustment = open_primary(if arguments.opens_primary_read_only() {
        TOKEN_QUERY
    } else {
        TOKEN_QUERY | TOKEN_ADJUST_DEFAULT
    });
    drop_thread_token(arguments.keeps_thread_token())?;
    lower_integrity(adjustment, arguments.skips_lowering())?;
    close_startup_leftovers()?;
    attest::check(&native::primary_facts(CURRENT_PROCESS_TOKEN, package))?;
    let words = native::mitigation_words()
        .map_err(|error| Refusal::new(reason::MITIGATION_MISMATCH, error))?;
    mitigation::validate(&words)
        .map_err(|error| Refusal::new(reason::MITIGATION_MISMATCH, error))?;
    let table =
        native::handle_table().map_err(|error| Refusal::new(reason::HANDLE_NOT_ALLOWED, error))?;
    allowlist::check(&table).map_err(|error| Refusal::new(reason::HANDLE_NOT_ALLOWED, error))?;
    Ok(())
}

/// A token handle closed when dropped, unless closed explicitly first.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = close(self.0, "token");
        }
    }
}

fn open_primary(access: u32) -> Result<Token, String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return Err(last("OpenProcessToken(own primary)"));
    }
    Ok(Token(token))
}

/// Step 1: stop impersonating the start-up token, then require that the
/// thread has no token at all (`ERROR_NO_TOKEN`).
fn drop_thread_token(keep: bool) -> Result<(), Refusal> {
    let refuse = |detail: String| Refusal::new(reason::THREAD_TOKEN_PRESENT, detail);
    if !keep && unsafe { RevertToSelf() } == 0 {
        return Err(refuse(last("RevertToSelf")));
    }
    let mut token = null_mut();
    // Open-as-self checks access against the process's primary token rather
    // than against whatever token the thread may still carry.
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0 {
        let _ = close(token, "thread token");
        return Err(refuse("the main thread still has a token".into()));
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_NO_TOKEN {
        return Err(refuse(format!(
            "OpenThreadToken: Win32 {error}, not ERROR_NO_TOKEN"
        )));
    }
    Ok(())
}

/// Step 2: lower the actual primary from Low to Untrusted, then close the
/// handle that could adjust it, so none survives into the input phase.
fn lower_integrity(adjustment: Result<Token, String>, skip: bool) -> Result<(), Refusal> {
    let refuse = |detail: String| Refusal::new(reason::INTEGRITY_LOWER_FAILED, detail);
    let mut token = adjustment.map_err(refuse)?;
    if !skip {
        native::set_untrusted(token.0).map_err(refuse)?;
    }
    let handle = std::mem::replace(&mut token.0, null_mut());
    close(handle, "primary adjustment handle").map_err(refuse)
}

/// Step 3: close the ALPC port and private File handles the loader left (see
/// [`closed_at_startup`]). A handle that cannot be listed or closed refuses
/// startup as `handle-not-allowed`, the check that would otherwise see it.
fn close_startup_leftovers() -> Result<(), Refusal> {
    let refuse = |detail: String| Refusal::new(reason::HANDLE_NOT_ALLOWED, detail);
    let types = native::object_types().map_err(refuse)?;
    let standard = native::standard_handles();
    for entry in native::handle_snapshot().map_err(refuse)? {
        let Some(type_name) = types.get(&entry.type_index) else {
            continue;
        };
        let value = entry.handle as usize;
        if closed_at_startup(
            type_name,
            standard.contains(&value),
            entry.attributes & OBJ_INHERIT != 0,
        ) {
            close(entry.handle, &format!("{type_name} {value:#x}")).map_err(refuse)?;
        }
    }
    Ok(())
}
