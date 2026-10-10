//! The one lock every Windows child spawn in basal takes.
//!
//! A child created with `bInheritHandles = TRUE` and no explicit handle list
//! inherits every inheritable handle in the parent at that instant. basal's
//! own spawns pass an explicit list, so they inherit only their own three
//! standard handles; but a handle is only listable once it is inheritable,
//! so between "made inheritable" and "parent's copy closed" any concurrent
//! spawn without a list (a test variant, a library, the standard library's
//! `Command`) would copy it. A child holding another child's pipe end keeps
//! that pipe open after its real owner exits, and can read or write it.
//!
//! The rule that closes this window: a spawn holds [`SpawnLock`] from the
//! moment it makes its child's handles inheritable, through process
//! creation, until it has closed its copies of them. So at any instant at
//! most one spawn's handles are inheritable, and any spawn under the lock,
//! with or without a handle list, can only inherit its own.

use std::cell::Cell;
use std::os::windows::io::RawHandle;
use std::sync::{Mutex, MutexGuard};
use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

static SPAWN: Mutex<()> = Mutex::new(());

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// Proof that the calling thread holds the process-wide spawn lock.
/// Dropping it releases the lock.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct SpawnLock {
    _guard: MutexGuard<'static, ()>,
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        HELD.with(|held| held.set(false));
    }
}

impl std::fmt::Debug for SpawnLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnLock")
    }
}

/// Takes the process-wide spawn lock, blocking until it is free. The lock is
/// not reentrant: a thread that already holds it must not call this again.
///
/// A panic while the lock was held leaves no state behind it to repair, so
/// a poisoned lock is simply taken.
pub fn spawn_lock() -> SpawnLock {
    let guard = SPAWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    HELD.with(|held| held.set(true));
    SpawnLock { _guard: guard }
}

/// Whether the calling thread holds the spawn lock.
pub fn spawn_lock_held() -> bool {
    HELD.with(Cell::get)
}

/// Marks handles inheritable for a child about to be created. It takes the
/// lock guard so that no caller can make a handle inheritable outside it.
/// The caller must close its copies before dropping the guard.
pub fn make_inheritable(_held: &SpawnLock, handles: &[RawHandle]) -> std::io::Result<()> {
    for &handle in handles {
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_marks_the_thread_and_excludes_other_threads() {
        assert!(!spawn_lock_held());
        let guard = spawn_lock();
        assert!(spawn_lock_held());
        let elsewhere = std::thread::spawn(|| (spawn_lock_held(), SPAWN.try_lock().is_err()))
            .join()
            .unwrap();
        assert_eq!(elsewhere, (false, true));
        drop(guard);
        assert!(!spawn_lock_held());
    }
}
