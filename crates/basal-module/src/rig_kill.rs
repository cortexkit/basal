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

/// Arms one unscoped Broca send for one named flow. It has its own arming file,
/// separate from the crash switch's, so arming a crash can never also make a
/// send go out without its scope.
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
    fn actor_evidence(
        &self,
        run_id: &str,
        boundary: &Boundary,
        position: Option<u64>,
        state: &basal_core::run_records::State,
    ) -> Option<String> {
        let call = state.calls.get(&position?)?;
        let args = call
            .intent
            .as_ref()
            .map_or(&call.identity.args, |i| &i.frozen_request);
        let dispatch = if call.unknown.is_some() && call.arrival.is_none() {
            "unknown"
        } else if call.deferred.is_some() && call.arrival.is_none() {
            "deferred"
        } else if call.accepted.is_some() {
            "accepted"
        } else if call.intent.is_some() {
            "sent"
        } else {
            "none"
        };
        let settlement = call.sync.as_ref().map(|_| "fulfilled").or_else(|| {
            call.delivery_order.and_then(|_| {
                call.arrival
                    .as_ref()
                    .map(|a| basal_core::model::settlement_str(a.settlement))
            })
        });
        let snapshots = self
            .store
            .get()
            .and_then(|path| {
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
            })
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM broca_calls WHERE run_id=?1",
                    [run_id],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
            })
            .unwrap_or(0);
        Some(serde_json::json!({"run_id":run_id,"boundary":format!("{boundary:?}"),"args":serde_json::from_str::<serde_json::Value>(args.as_str()).ok()?,"dispatch":dispatch,"attempts":call.intent.as_ref().map_or(0,|i|i.attempts),"settlement":settlement,"broca_snapshots":snapshots}).to_string())
    }

    fn fire(&self, armed: &Armed, evidence: Option<String>) -> Step {
        if let Some(evidence) = evidence {
            let _ = std::fs::write(self.path.with_extension("evidence"), evidence);
        }
        // Removing the arm first prevents a restart from entering a kill loop.
        if std::fs::remove_file(&self.path).is_err() {
            return Step::Continue;
        }
        tracing::warn!(flow_id=%armed.flow_id,boundary=%armed.boundary,"rig kill hook: killing ck-basal");
        eprintln!(
            "ck-basal: rig kill hook: killing itself at {} in flow {}",
            armed.boundary, armed.flow_id
        );
        crate::fatal::kill_self();
    }

    /// Capture the journal at the crash boundary, before the supervisor can
    /// restart basal. This distinguishes a selected-but-unsent call from an
    /// accepted call whose result has not reached the journal.
    fn evidence(&self, run_id: &str, boundary: &Boundary) -> Option<String> {
        let position = match boundary {
            Boundary::ResendAuthorized { position }
            | Boundary::HostCallReceived { position }
            | Boundary::ModelSelected { position }
            | Boundary::CallCommitted { position }
            | Boundary::SyncCommitted { position }
            | Boundary::SyncReplied { position }
            | Boundary::LocalCommitted { position }
            | Boundary::RefusalCommitted { position }
            | Boundary::HostAnswered { position }
            | Boundary::OutcomeCommitted { position }
            | Boundary::AcceptedCommitted { position }
            | Boundary::OrderCommitted { position, .. }
            | Boundary::Delivered { position } => *position,
            _ => return None,
        };
        let c = Connection::open_with_flags(self.store.get()?, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
        let row = c.query_row(
            "SELECT COALESCE(request, args), dispatch, attempts, settlement FROM journal_with_payloads WHERE run_id = ?1 AND position = ?2",
            rusqlite::params![run_id, position], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?)),
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
        let staged: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='run_identities')",
                [],
                |r| r.get(0),
            )
            .ok()?;
        connection
            .query_row(
                if staged {
                    "SELECT flow_id FROM run_identities WHERE run_id=?1"
                } else {
                    "SELECT flow_id FROM runs WHERE run_id = ?1"
                },
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
    fn at_run(
        &self,
        run_id: &str,
        flow_id: &str,
        boundary: &Boundary,
        position: Option<u64>,
        state: &basal_core::run_records::State,
    ) -> Step {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Step::Continue;
        };
        let Ok(armed) = serde_json::from_str::<Armed>(&text) else {
            return Step::Continue;
        };
        if !matches(&armed, Some(flow_id), boundary) {
            return Step::Continue;
        }
        // The driver supplies this run's records while holding its mutex.
        // Re-entering the run here would deadlock. Run-log executions do not
        // write the old SQLite journal tables.
        self.fire(
            &armed,
            self.actor_evidence(run_id, boundary, position, state),
        )
    }
    fn at(&self, run_id: &str, boundary: &Boundary) -> Step {
        if matches!(
            boundary,
            Boundary::DriverAppended
                | Boundary::DriverSynced
                | Boundary::DriverDispatched
                | Boundary::DriverReleased
        ) {
            return Step::Continue;
        }
        if matches!(boundary, Boundary::HostArrived)
            && let Some(path) = self.store.get()
            && let Ok(c) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            && c.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_runs WHERE run_id=?1)",
                [run_id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        {
            return Step::Continue;
        }
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Step::Continue;
        };
        let Ok(armed) = serde_json::from_str::<Armed>(&text) else {
            return Step::Continue;
        };
        // The cheap comparison first: the store is read only at the armed
        // boundary.
        if armed.boundary != format!("{boundary:?}") {
            return Step::Continue;
        }
        if self.flow_of(run_id).as_deref() != Some(armed.flow_id.as_str()) {
            return Step::Continue;
        }
        self.fire(&armed, self.evidence(run_id, boundary))
    }
}

#[cfg(test)]
mod actor_reader_tests {
    use super::*;
    use basal_core::{Admission, Clock, Durability, InstallGate, InstallRequest, Runtime, Store};
    use basal_host::{
        CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, TransportError,
    };
    use basal_proto::{ActivationResult, CallKind, HostCall, JsonText, WorkerMessage};
    use std::sync::Mutex;

    struct Observe {
        hook: RigKillHook,
        evidence: Mutex<Vec<serde_json::Value>>,
    }
    impl Hooks for Observe {
        fn at(&self, _: &str, _: &Boundary) -> Step {
            Step::Continue
        }
        fn at_run(
            &self,
            id: &str,
            _: &str,
            boundary: &Boundary,
            position: Option<u64>,
            state: &basal_core::run_records::State,
        ) -> Step {
            if matches!(boundary, Boundary::DriverSynced | Boundary::DriverReleased) {
                self.evidence.lock().unwrap().push(
                    serde_json::from_str(
                        &self
                            .hook
                            .actor_evidence(id, boundary, position, state)
                            .unwrap(),
                    )
                    .unwrap(),
                );
            }
            Step::Continue
        }
    }
    struct QueryHost;
    impl Host for QueryHost {
        fn classify(&self, _: &CallKind) -> CallClass {
            CallClass::Query
        }
        fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
            Ok(Dispatched::Completed(HostOutcome::fulfilled(
                JsonText::new("7").unwrap(),
            )))
        }
        fn attach(&self, _: Arc<dyn CompletionSink>) {}
        fn now_ms(&self) -> f64 {
            100.0
        }
        fn random(&self) -> f64 {
            0.5
        }
    }

    #[test]
    fn rig_evidence_reads_the_fenced_actor_of_a_real_staged_run() {
        let root = basal_core::test_support::timestamp_scratch("rig-kill", "actor-reader");
        let path = root.join("store.db");
        let store = Arc::new(Store::open(&path, Durability::throwaway()).unwrap());
        let named = Arc::new(OnceLock::new());
        named.set(path).unwrap();
        let hook = Arc::new(Observe {
            hook: RigKillHook {
                path: root.join("not-armed"),
                store: named,
            },
            evidence: Mutex::new(Vec::new()),
        });
        let catalog = Arc::new(basal_host::catalog::MockCatalog::standard());
        catalog.set_op(
            "test",
            "query",
            basal_host::catalog::OpDecl {
                kind: Some(basal_host::catalog::OpKind::Query),
                cause_echo: true,
                shell_capable: false,
            },
        );
        let rt = Runtime::new_staged_scratch(
            store,
            Arc::new(QueryHost),
            catalog,
            hook.clone(),
            None,
            basal_core::runtime::Config {
                clock: Clock::manual(100),
                install_gate: InstallGate::Off,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let manifest = serde_json::json!({"id":"actor-reader","version":1,"purpose":"Capture locked registry evidence","trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"test","op":"query"}]});
        let installed = rt
            .install(&InstallRequest {
                script: "return 7;".into(),
                manifest: manifest.to_string(),
                author: "operator".into(),
                loop_override: false,
            })
            .unwrap();
        rt.approve("actor-reader", 1, &installed.code_hash, "rig test")
            .unwrap();
        let Admission::Admitted { run_id } = rt
            .admit_trigger("actor-reader", "first", JsonText::null())
            .unwrap()
        else {
            panic!("not admitted")
        };
        let mut worker = basal_core::test_support::ScriptedWorker::new(vec![
            WorkerMessage::HostCall(HostCall {
                position: 0,
                kind: CallKind::Op {
                    module: "test".into(),
                    op: "query".into(),
                },
                args: JsonText::new("{\"question\":1}").unwrap(),
            }),
            WorkerMessage::Blocked { awaiting: vec![0] },
            WorkerMessage::Finished {
                activation_id: 0,
                result: ActivationResult::Completed {
                    value: JsonText::new("7").unwrap(),
                },
            },
        ]);
        assert!(matches!(
            rt.activate(&run_id, &mut worker).unwrap(),
            basal_core::ActivationEnd::Succeeded { .. }
        ));
        let evidence = hook.evidence.lock().unwrap();
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0]["args"], serde_json::json!({"question":1}));
        assert_eq!(evidence[0]["dispatch"], "sent");
        assert_eq!(evidence[0]["attempts"], 1);
        assert!(evidence[0]["settlement"].is_null());
        assert_eq!(evidence[1]["settlement"], "fulfilled");
        rt.store()
            .read(|c| {
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get::<_, u64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
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
        c.pragma_update(None, "synchronous", "OFF").unwrap();
        c.execute_batch("CREATE TABLE journal (run_id TEXT, position INTEGER, args TEXT, request TEXT,
            dispatch TEXT, attempts INTEGER, settlement TEXT);
            CREATE TABLE broca_calls (run_id TEXT);
            CREATE VIEW journal_with_payloads AS SELECT * FROM journal;
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

#[cfg(test)]
mod position_tests {
    use super::*;

    #[test]
    fn evidence_reads_the_position_named_by_the_boundary() {
        let dir =
            std::env::temp_dir().join(format!("basal-position-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "synchronous", "OFF").unwrap();
        c.execute_batch("CREATE TABLE journal (run_id TEXT, position INTEGER, args TEXT, request TEXT, dispatch TEXT, attempts INTEGER, settlement TEXT);
            CREATE TABLE broca_calls (run_id TEXT);
            CREATE VIEW journal_with_payloads AS SELECT * FROM journal;
            INSERT INTO journal VALUES ('r',0,'0',NULL,'sent',1,NULL), ('r',3,'3',NULL,'answered',2,'fulfilled');").unwrap();
        let store = Arc::new(OnceLock::new());
        store.set(path).unwrap();
        let hook = RigKillHook {
            path: dir.join("arm"),
            store,
        };
        let evidence: serde_json::Value = serde_json::from_str(
            &hook
                .evidence("r", &Boundary::HostAnswered { position: 3 })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["args"], 3);
        assert_eq!(evidence["attempts"], 2);
        drop(c);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
