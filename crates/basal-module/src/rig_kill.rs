//! A one-shot kill switch for basal's isolated test rig, ckdev-flows
//! (`docs/rig.md`). It is compiled only with the `rig-kill-hook` feature,
//! which only `script/flows-rig.sh build` turns on for the rig's own
//! `ckdev-basal`; the production `ck-basal` never contains it.
//!
//! The rig's contract suite uses it to SIGKILL `ck-basal` at an exact runtime
//! boundary, for example after core has answered a journaled `sink.digest`
//! and before basal commits that answer, and then checks that the restarted
//! module re-sends the write and core applies it once. The boundary is named
//! by the file at `$BASAL_RIG_KILL_FILE`: while the file holds the boundary's
//! `Debug` text (`HostAnswered { position: 1 }`), the first runtime thread to
//! reach that boundary deletes the file and kills the process. Without the
//! variable, or without the file, the hook does nothing.

use std::path::PathBuf;

use basal_core::hooks::{Boundary, Hooks, Step};

/// The environment variable naming the arming file.
pub const KILL_FILE_ENV: &str = "BASAL_RIG_KILL_FILE";

pub struct RigKillHook {
    path: PathBuf,
}

impl RigKillHook {
    /// The hook armed by the file `$BASAL_RIG_KILL_FILE` names, or `None`
    /// when the variable is unset or empty.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os(KILL_FILE_ENV).filter(|v| !v.is_empty())?;
        Some(Self {
            path: PathBuf::from(path),
        })
    }
}

impl Hooks for RigKillHook {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        let Ok(armed) = std::fs::read_to_string(&self.path) else {
            return Step::Continue;
        };
        let point = format!("{boundary:?}");
        if armed.trim() != point {
            return Step::Continue;
        }
        // Disarm before dying, so the restarted process passes the same
        // boundary when it re-sends the call. Only one thread can remove the
        // file; a thread that cannot (another got there first, or the file
        // cannot be removed) carries on rather than risk a kill loop.
        if std::fs::remove_file(&self.path).is_err() {
            return Step::Continue;
        }
        tracing::warn!("rig kill hook: killing ck-basal in run {run_id} at {point}");
        eprintln!("ck-basal: rig kill hook: killing itself in run {run_id} at {point}");
        // SAFETY: kill(2) on our own pid has no memory-safety preconditions.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        // kill(2) can return before the signal takes the process down; this
        // thread must not go on to commit the answer meanwhile.
        loop {
            std::thread::park();
        }
    }
}
