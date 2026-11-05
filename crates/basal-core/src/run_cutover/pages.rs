//! Nontransactional SQLite maintenance, before any runtime can be constructed.
use super::legacy::corrupt;
use crate::{Boundary, CoreError, Hooks, Result, Step, Store};
use std::path::Path;

pub(super) fn set_pages(store: &Store, hooks: &dyn Hooks) -> Result<()> {
    set_pages_checked(store, hooks, free_bytes)
}
fn set_pages_checked(
    store: &Store,
    hooks: &dyn Hooks,
    available: impl Fn(&Path) -> Result<u64>,
) -> Result<()> {
    // VACUUM cannot run inside with_conn_fenced's transaction. The OS store
    // lease remains held throughout. Empty write transactions check the
    // persisted lease epoch immediately before and after maintenance.
    // No runtime or host exists yet.
    store.write(|_| Ok(()))?;
    let started = std::time::Instant::now();
    let sizes=store.read(|c| {
        let path:String=c.query_row("SELECT file FROM pragma_database_list WHERE name='main'",[],|r|r.get(0))?;
        let before=std::fs::metadata(&path).map_err(|e|CoreError::Store(e.to_string()))?.len();
        let result=(|| {
            let page:i64=c.pragma_query_value(None,"page_size",|r|r.get(0))?;
            if page!=1024 {
                let free=available(Path::new(&path))?;
                // page_count includes pages still in WAL. File length alone
                // could underestimate the copy space until the WAL checkpoints.
                let count:u64=c.pragma_query_value(None,"page_count",|r|r.get(0))?;
                let database=before.max(count.checked_mul(page as u64).ok_or_else(||corrupt("database size overflow"))?);
                let required=database.checked_mul(5).and_then(|n|n.checked_add(1)).map(|n|n/2).ok_or_else(||corrupt("store size overflow"))?;
                if free<required {return Err(corrupt(format!("insufficient free space for page-size VACUUM: {free} available bytes, {required} required (2.5x {database})")));}
                c.pragma_update(None,"journal_mode","DELETE")?;
                c.pragma_update(None,"page_size",1024)?;
                at(store,hooks,Boundary::MigrationBeforeVacuum)?;
                c.execute_batch("VACUUM")?;
                at(store,hooks,Boundary::MigrationAfterVacuum)?;
            }
            Ok(())
        })();
        // Even an injected logical crash or a failed VACUUM must not leave a
        // surviving process with DELETE mode. A real process death is completed
        // by the same resumable phase on its next startup.
        let wal=c.pragma_update(None,"journal_mode","WAL");
        if let Err(error)=wal {return Err(CoreError::Store(format!("restoring WAL after migration maintenance: {error}; maintenance result: {result:?}")));}
        result?;
        verify(c)?;
        let after=std::fs::metadata(&path).map_err(|e|CoreError::Store(e.to_string()))?.len();
        Ok((before,after))
    });
    store.write(|_| Ok(()))?;
    let (before, after) = sizes?;
    tracing::info!(target:"migration",duration_ms=started.elapsed().as_millis() as u64,before_bytes=before,after_bytes=after,page_size=1024,"page-size migration complete");
    Ok(())
}

#[cfg(test)]
pub(super) fn with_free_space(store: &Store, bytes: u64) -> Result<()> {
    set_pages_checked(store, &crate::NoHooks, |_| Ok(bytes))
}

fn at(store: &Store, hooks: &dyn Hooks, boundary: Boundary) -> Result<()> {
    if hooks.at("migration", &boundary) == Step::Crash {
        store.cut();
        Err(CoreError::Cut)
    } else {
        Ok(())
    }
}
pub(super) fn verify(c: &rusqlite::Connection) -> Result<()> {
    let page: i64 = c.pragma_query_value(None, "page_size", |r| r.get(0))?;
    let mode: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if page != 1024 || !mode.eq_ignore_ascii_case("wal") || integrity != "ok" {
        return Err(corrupt(format!(
            "page-size post-check: page_size={page}, journal_mode={mode}, integrity_check={integrity}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| corrupt("NUL in store path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs initializes the output only on success; the NUL-terminated path
    // stays alive through the call. f_bavail excludes root-only reserved blocks.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(CoreError::Store(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let stat = unsafe { stat.assume_init() };
    u64::try_from(u128::from(stat.f_bavail) * u128::from(stat.f_frsize))
        .map_err(|_| corrupt("free-space size overflow"))
}
#[cfg(windows)]
fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let parent = path
        .parent()
        .ok_or_else(|| corrupt("store has no parent volume"))?;
    let wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    // The path is NUL-terminated and available is writable for the call.
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(CoreError::Store(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(available)
}
