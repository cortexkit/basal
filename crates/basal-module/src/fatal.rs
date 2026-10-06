//! Store failures end the process.
//!
//! The core fails closed on a storage error and does not retry in place: an
//! activation whose commit failed leaves its run `running`, and only
//! `Runtime::recover` at the next start puts it back in line. Serving on
//! would leave that run stuck for as long as the process lives. So the first
//! storage error anywhere (a loop, an activation, an op, a consent decision)
//! is raised here, logged, and the process exits non-zero; the supervisor
//! restarts it and recovery runs before anything else.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use basal_core::{CoreError, InstallError};

/// The exit status after a storage error: `EX_TEMPFAIL`, a failure a
/// restart is expected to clear.
pub const EXIT_STORE_FAILURE: i32 = 75;

/// Startup cannot safely launch workers with their own privacy identity.
/// The supervisor must report this refusal and restart, never launch plainly.
pub const EXIT_PRIVACY_IDENTITY_FAILURE: u8 = 76;

/// Whether a core error is a failure of the store itself, as opposed to a
/// refusal or a request the core found invalid.
pub fn is_storage(e: &CoreError) -> bool {
    matches!(e, CoreError::Store(_) | CoreError::Cut)
}

pub fn install_is_storage(e: &InstallError) -> bool {
    matches!(e, InstallError::Store(inner) if is_storage(inner))
}

/// A latch holding the first fatal error. Cloning shares it.
#[derive(Clone, Default)]
pub struct Fatal {
    inner: Arc<(Mutex<Option<String>>, Condvar)>,
}

impl Fatal {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.inner.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records a fatal error. The first one wins; later ones are logged only.
    pub fn raise(&self, why: impl Into<String>) {
        let why = why.into();
        tracing::error!(target: "store", "fatal: {why}");
        let mut slot = self.lock();
        if slot.is_none() {
            *slot = Some(why);
            self.inner.1.notify_all();
        }
    }

    /// Raises `e` if it is a storage error, and returns it unchanged.
    pub fn check(&self, e: CoreError) -> CoreError {
        if is_storage(&e) {
            self.raise(e.to_string());
        }
        e
    }

    pub fn get(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Waits up to `timeout` for a fatal error.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<String> {
        let guard = self.lock();
        let (guard, _) = self
            .inner
            .1
            .wait_timeout_while(guard, timeout, |slot| slot.is_none())
            .unwrap_or_else(|p| p.into_inner());
        guard.clone()
    }

    /// Waits for a fatal error, however long it takes.
    pub fn wait(&self) -> String {
        let mut guard = self.lock();
        loop {
            if let Some(why) = guard.clone() {
                return why;
            }
            guard = self.inner.1.wait(guard).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Starts a thread that ends the process with [`EXIT_STORE_FAILURE`] as
    /// soon as a fatal error is raised.
    pub fn exit_when_raised(&self) {
        let fatal = self.clone();
        std::thread::spawn(move || {
            let why = fatal.wait();
            eprintln!("ck-basal: exiting after a store failure: {why}");
            std::process::exit(EXIT_STORE_FAILURE);
        });
    }
}
