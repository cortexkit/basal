//! A one-shot kill switch for basal's isolated test rig, ckdev-flows
//! (`script/flows-rig.sh`). It is compiled only with the `rig-kill-hook` feature,
//! which only `script/flows-rig.sh build` turns on for the rig's own
//! `ckdev-basal`; the production `ck-basal` never contains it.
//!
//! The rig's contract suite uses it to SIGKILL `ck-basal` at one exact point:
//! after core has applied a `sink.digest` write and answered it, but before
//! basal has saved that answer in its journal. It then checks that the
//! restarted module sends the write again and that core applies it only
//! once.
//!
//! The file at `$BASAL_RIG_KILL_FILE` arms it with one flow and one of the
//! runtime's [`Boundary`] values, as JSON:
//! `{"flow_id": "rig-crash-…", "boundary": "HostAnswered { position: 0 }"}`,
//! the boundary in its `Debug` text (position 0 is a run's first call). The
//! first runtime thread to reach that boundary in a run of that flow deletes
//! the file and kills the process; runs of every other flow pass untouched.
//! Without the variable, or without the file, the hook does nothing.
//!
//! `$BASAL_RIG_UNSCOPED_FILE` separately arms one unscoped Broca send with
//! `{"flow_id":"rig-model-unscoped-…"}`. Only that flow consumes the file;
//! the rig checks that Broca refuses it before writing any run records.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use basal_core::hooks::{Boundary, Hooks, Step};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

/// The environment variable naming the arming file.
pub const KILL_FILE_ENV: &str = "BASAL_RIG_KILL_FILE";

pub const UNSCOPED_FILE_ENV: &str = "BASAL_RIG_UNSCOPED_FILE";

/// A separate, closed arming file prevents a crash boundary from stripping scope.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnscopedArmed {
    pub flow_id: String,
}

pub struct RigUnscopedSend {
    path: PathBuf,
}

impl RigUnscopedSend {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            path: std::env::var_os(UNSCOPED_FILE_ENV)
                .filter(|s| !s.is_empty())?
                .into(),
        })
    }

    /// Only the named flow consumes the file. Removing it before the send makes
    /// the switch one-shot even with concurrent dispatches or a module restart.
    pub fn take(&self, flow_id: Option<&str>) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(armed) = serde_json::from_str::<UnscopedArmed>(&text) else {
            return false;
        };
        if armed.flow_id.is_empty() || flow_id != Some(armed.flow_id.as_str()) {
            return false;
        }
        std::fs::remove_file(&self.path).is_ok()
    }
}

/// The arming file's content.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Armed {
    pub flow_id: String,
    pub boundary: String,
}

pub struct RigKillHook {
    path: PathBuf,
    /// The module's store, set once the daemon has named it. The hook is
    /// told only a run id, so it reads the run's flow from the store.
    store: Arc<OnceLock<PathBuf>>,
}

impl RigKillHook {
    /// Capture the journal at the crash boundary, before the supervisor can
    /// restart basal. This distinguishes a selected-but-unsent call from an
    /// accepted call whose result has not reached the journal.
    fn evidence(&self, run_id: &str, boundary: &Boundary) -> Option<String> {
        let c = Connection::open_with_flags(self.store.get()?, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
        let row = c.query_row(
            "SELECT COALESCE(request, args), dispatch, attempts, settlement FROM journal WHERE run_id = ?1 AND position = 0",
            [run_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?)),
        ).ok()?;
        let snapshots: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM broca_calls WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .ok()?;
        Some(
            serde_json::json!({"run_id": run_id, "boundary": format!("{boundary:?}"),
            "args": serde_json::from_str::<serde_json::Value>(&row.0).ok()?,
            "dispatch": row.1, "attempts": row.2, "settlement": row.3,
            "broca_snapshots": snapshots})
            .to_string(),
        )
    }

    /// The hook armed by the file `$BASAL_RIG_KILL_FILE` names, or `None`
    /// when the variable is unset or empty. `store` is filled with the
    /// store's path when the module learns it.
    pub fn from_env(store: Arc<OnceLock<PathBuf>>) -> Option<Self> {
        let path = std::env::var_os(KILL_FILE_ENV).filter(|v| !v.is_empty())?;
        Some(Self {
            path: PathBuf::from(path),
            store,
        })
    }

    /// The flow `run_id` belongs to, read through a read-only connection.
    fn flow_of(&self, run_id: &str) -> Option<String> {
        let connection = Connection::open_with_flags(
            self.store.get()?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        connection
            .query_row(
                "SELECT flow_id FROM runs WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .optional()
            .ok()?
    }
}

/// Whether `armed` names this boundary in a run of `flow_id`.
pub fn matches(armed: &Armed, flow_id: Option<&str>, boundary: &Boundary) -> bool {
    armed.boundary == format!("{boundary:?}") && flow_id == Some(armed.flow_id.as_str())
}

impl Hooks for RigKillHook {
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Step::Continue;
        };
        let Ok(armed) = serde_json::from_str::<Armed>(&text) else {
            return Step::Continue;
        };
        // The cheap comparison first: the store is read only at the armed
        // boundary.
        if armed.boundary != format!("{boundary:?}")
            || !matches(&armed, self.flow_of(run_id).as_deref(), boundary)
        {
            return Step::Continue;
        }
        if let Some(evidence) = self.evidence(run_id, boundary) {
            let _ = std::fs::write(self.path.with_extension("evidence"), evidence);
        }
        // Disarm before dying, so the restarted process passes the same
        // boundary when it re-sends the call. Only one thread can remove the
        // file; a thread that cannot (another got there first, or the file
        // cannot be removed) carries on rather than risk a kill loop.
        if std::fs::remove_file(&self.path).is_err() {
            return Step::Continue;
        }
        let point = &armed.boundary;
        let flow = &armed.flow_id;
        tracing::warn!("rig kill hook: killing ck-basal in run {run_id} of {flow} at {point}");
        eprintln!("ck-basal: rig kill hook: killing itself in run {run_id} of {flow} at {point}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_send_fires_only_for_the_armed_flow_once() {
        let path = std::env::temp_dir().join(format!("basal-unscoped-{}.arm", std::process::id()));
        let hook = RigUnscopedSend { path: path.clone() };
        let _ = std::fs::remove_file(&path);
        assert!(!hook.take(Some("armed")), "absent file");
        std::fs::write(&path, r#"{"flow_id":"armed"}"#).unwrap();
        assert!(
            !hook.take(Some("unarmed")),
            "another flow must not consume the arm"
        );
        assert!(path.exists());
        assert!(!hook.take(None), "unknown flow");
        assert!(hook.take(Some("armed")));
        assert!(!path.exists());
        assert!(!hook.take(Some("armed")), "one shot");
    }

    #[test]
    fn unscoped_send_arming_file_is_closed_and_malformed_arms_do_not_fire() {
        let path =
            std::env::temp_dir().join(format!("basal-unscoped-closed-{}.arm", std::process::id()));
        let hook = RigUnscopedSend { path: path.clone() };
        for text in [
            r#"{"flow_id":"f","extra":1}"#,
            "f",
            "{}",
            r#"{"flow_id":""}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(!hook.take(Some("f")), "{text}");
            assert!(path.exists());
        }
        std::fs::remove_file(path).unwrap();
    }

    fn armed() -> Armed {
        serde_json::from_str(r#"{"flow_id":"rig-crash","boundary":"HostAnswered { position: 0 }"}"#)
            .unwrap()
    }

    #[test]
    fn only_the_armed_flow_at_the_armed_boundary_matches() {
        let at = Boundary::HostAnswered { position: 0 };
        assert!(matches(&armed(), Some("rig-crash"), &at));
        assert!(
            !matches(&armed(), Some("rig-sinks"), &at),
            "another flow's run"
        );
        assert!(!matches(&armed(), None, &at), "a run whose flow is unknown");
        assert!(!matches(
            &armed(),
            Some("rig-crash"),
            &Boundary::HostAnswered { position: 1 }
        ));
    }

    #[test]
    fn the_arming_file_is_closed() {
        assert!(
            serde_json::from_str::<Armed>(r#"{"flow_id":"f","boundary":"b","extra":1}"#).is_err()
        );
        assert!(serde_json::from_str::<Armed>("HostAnswered { position: 0 }").is_err());
    }

    #[test]
    fn crash_evidence_records_the_unsent_journal_not_a_later_outcome() {
        let path =
            std::env::temp_dir().join(format!("basal-rig-evidence-{}.db", std::process::id()));
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE journal (run_id TEXT, position INTEGER, args TEXT, request TEXT,
            dispatch TEXT, attempts INTEGER, settlement TEXT);
            CREATE TABLE broca_calls (run_id TEXT);
            INSERT INTO journal VALUES ('r', 0, '{\"prompt\":\"authored\"}', '{\"selection\":{\"decisionID\":\"d1\"}}', 'sent', 1, NULL);").unwrap();
        let store = Arc::new(OnceLock::new());
        store.set(path.clone()).unwrap();
        let hook = RigKillHook {
            path: path.with_extension("arm"),
            store,
        };
        let at = Boundary::CallCommitted { position: 0 };
        let evidence: serde_json::Value =
            serde_json::from_str(&hook.evidence("r", &at).unwrap()).unwrap();
        assert_eq!(evidence["run_id"], "r");
        assert_eq!(evidence["boundary"], "CallCommitted { position: 0 }");
        assert_eq!(evidence["args"]["selection"]["decisionID"], "d1");
        assert_eq!(evidence["attempts"], 1);
        assert_eq!(evidence["dispatch"], "sent");
        assert_eq!(evidence["broca_snapshots"], 0);
        assert!(evidence["settlement"].is_null());
        assert!(hook.evidence("other", &at).is_none());
        drop(c);
        std::fs::remove_file(path).unwrap();
    }
}
