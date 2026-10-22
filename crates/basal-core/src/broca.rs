//! Saved Broca calls stored beside the call journal: one snapshot per call,
//! holding its frozen send, its Broca run, its outcome once `run.result`
//! reported one, and whether the runtime has recorded that outcome.

use crate::{CoreError, Store};
use basal_host::broca::{BrocaError, RunWake, StateStore, StoredCall, WakeKind};
use rusqlite::OptionalExtension;
use std::sync::{Arc, Mutex};

/// Lists calls still awaiting Broca's answer. Its WHERE clause repeats the
/// `broca_calls_pending` partial index's predicate byte for byte, because
/// SQLite uses a partial index only when the query states its exact
/// condition; a test checks the plan with this same text.
pub const PENDING_IDS_SQL: &str = "SELECT send_id FROM broca_calls WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0 ORDER BY send_id";

/// Can be constructed before the module shell opens SQLite, then bound to
/// that same store. The shared connection keeps every write under the runtime's
/// single-writer lease instead of opening a competing writer.
#[derive(Default)]
pub struct BrocaStore {
    store: Mutex<Option<Arc<Store>>>,
}
impl BrocaStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store: Mutex::new(Some(store)),
        }
    }
    pub fn bind(&self, store: Arc<Store>) -> Result<(), BrocaError> {
        let mut current = self
            .store
            .lock()
            .map_err(|e| BrocaError::Store(e.to_string()))?;
        if current.is_some() {
            return Err(BrocaError::Store("Broca store is already bound".into()));
        }
        *current = Some(store);
        Ok(())
    }
    fn store(&self) -> Result<Arc<Store>, BrocaError> {
        self.store
            .lock()
            .map_err(|e| BrocaError::Store(e.to_string()))?
            .clone()
            .ok_or_else(|| BrocaError::Store("Broca store is not bound yet".into()))
    }
}
impl StateStore for BrocaStore {
    fn codemode_active(&self, run_id: &str) -> Result<bool, BrocaError> {
        self.store()?.read(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM codemode_runs WHERE run_id=?1 AND status='running')", [run_id], |r| r.get(0))?))
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn get(&self, send_id: &str) -> Result<Option<StoredCall>, BrocaError> {
        let snapshot: Option<String> = self
            .store()?
            .read(|c| {
                Ok(c.query_row(
                    "SELECT snapshot FROM broca_calls WHERE send_id = ?1 UNION ALL SELECT snapshot FROM codemode_broca_calls WHERE send_id = ?1",
                    [send_id],
                    |row| row.get(0),
                )
                .optional()?)
            })
            .map_err(|e| BrocaError::Store(e.to_string()))?;
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        let call: StoredCall = serde_json::from_str(&snapshot)
            .map_err(|e| BrocaError::Store(format!("Broca snapshot: {e}")))?;
        // Upgrade a legacy byte array lazily and atomically, without decoding
        // every other retained call during module startup or dispatch.
        if serde_json::from_str::<serde_json::Value>(&snapshot)
            .map_err(|e| BrocaError::Store(e.to_string()))?["params"]
            .is_array()
        {
            self.save(&call)?;
        }
        Ok(Some(call))
    }
    fn pending_ids(&self) -> Result<Vec<String>, BrocaError> {
        self.store()?
            .read(|c| {
                let mut statement = c.prepare(PENDING_IDS_SQL)?;
                let rows = statement.query_map([], |row| row.get(0))?;
                let mut ids = rows.collect::<Result<Vec<_>, _>>()?;
                let mut statement = c.prepare("SELECT send_id FROM codemode_broca_calls WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0 ORDER BY send_id")?;
                ids.extend(statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<String>, _>>()?);
                Ok(ids)
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn identity(
        &self,
        run_id: &str,
    ) -> Result<Option<basal_host::broca::FlowIdentity>, BrocaError> {
        self.store()?
            .read(|c| {
                let run = crate::runs::load(c, run_id)?;
                let version = run
                    .flow_version
                    .ok_or_else(|| CoreError::Corrupt("model run has no install version".into()))?;
                Ok(Some(basal_host::broca::FlowIdentity {
                    flow_id: run.flow_id.clone(),
                    version,
                    code_hash: crate::ids::hex(&run.code_hash),
                }))
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn deadline(&self, run_id: &str) -> Result<Option<i64>, BrocaError> {
        // A codemode run's deadline is its wall deadline (the scope deadline,
        // or admission plus its wall limit if sooner) extended by the time
        // it spent waiting on a person, as the supervisor computes it.
        self.store()?
            .read(|c| {
                Ok(c.query_row(
                    "SELECT deadline_at FROM runs WHERE run_id = ?1 \
                     UNION ALL SELECT MIN(deadline_ms, admitted_at + COALESCE(json_extract(limits, '$.wall_ms'), deadline_ms)) + person_wait_ms \
                     FROM codemode_runs WHERE run_id = ?1",
                    [run_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn run_ended(&self, run_id: &str) -> Result<bool, BrocaError> {
        self.store()?
            .read(|c| {
                Ok(c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE run_id = ?1 AND state IN ('succeeded', 'failed', 'cancelled', 'engine_mismatch'))",
                    [run_id],
                    |r| r.get(0),
                )?)
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn forget(&self, send_id: &str) -> Result<(), BrocaError> {
        self.store()?
            .write(|tx| {
                tx.execute("DELETE FROM broca_waits WHERE send_id = ?1", [send_id])?;
                tx.execute("DELETE FROM broca_wakes WHERE send_id = ?1", [send_id])?;
                Ok(())
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn learn_session_ref(&self, send_id: &str, session_ref: &str) -> Result<(), BrocaError> {
        self.store()?
            .write(|tx| {
                crate::events::learn_session_ref_for(tx, send_id, session_ref)?;
                Ok(())
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn wakes(&self) -> Result<Vec<RunWake>, BrocaError> {
        self.store()?
            .read(|c| {
                let mut statement = c.prepare("SELECT send_id, kind, event_key, digest, run_id, session_ref, seq FROM broca_wakes ORDER BY received_at, rowid")?;
                let rows = statement.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                })?;
                rows.map(|row| {
                    let (send_id, kind, event_key, digest, run_id, session_ref, seq) = row?;
                    let kind = WakeKind::parse(&kind)
                        .ok_or_else(|| CoreError::Corrupt(format!("Broca wake kind {kind}")))?;
                    Ok(RunWake {
                        send_id,
                        kind,
                        event_key,
                        digest,
                        run_id,
                        session_ref,
                        seq,
                    })
                })
                .collect()
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn clear_wake(&self, event_key: &str, send_id: &str) -> Result<(), BrocaError> {
        self.store()?
            .write(|tx| {
                tx.execute(
                    "DELETE FROM broca_wakes WHERE send_id = ?1 AND event_key = ?2",
                    [send_id, event_key],
                )?;
                Ok(())
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn due(&self, now: i64) -> Result<Vec<String>, BrocaError> {
        self.store()?
            .read(|c| {
                let mut statement = c.prepare(
                    "SELECT send_id FROM broca_waits WHERE COALESCE(check_at, 0) <= ?1 ORDER BY check_at, send_id",
                )?;
                let rows = statement.query_map([now], |r| r.get(0))?;
                Ok(rows.collect::<Result<Vec<String>, _>>()?)
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn next_check(&self) -> Result<Option<i64>, BrocaError> {
        self.store()?
            .read(|c| {
                Ok(c.query_row(
                    "SELECT CASE WHEN EXISTS(SELECT 1 FROM broca_wakes) THEN 0 ELSE (SELECT MIN(COALESCE(check_at, 0)) FROM broca_waits) END",
                    [],
                    |r| r.get::<_, Option<i64>>(0),
                )?)
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        self.store()?
            .read(|c| {
                let mut stmt = c.prepare("SELECT snapshot FROM (SELECT send_id,snapshot FROM broca_calls UNION ALL SELECT send_id,snapshot FROM codemode_broca_calls) ORDER BY send_id")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.map(|row| {
                    serde_json::from_str(&row?)
                        .map_err(|e| CoreError::Corrupt(format!("Broca snapshot: {e}")))
                })
                .collect()
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
    fn save(&self, call: &StoredCall) -> Result<(), BrocaError> {
        let snapshot = serde_json::to_string(call).map_err(|e| BrocaError::Store(e.to_string()))?;
        let position = i64::try_from(call.position)
            .map_err(|_| BrocaError::Store("Broca call position".into()))?;
        self.store()?
            .write(|tx| {
                let sql = if call.route.flow_id.as_deref() == Some(&format!("codemode:{}", call.basal_run_id)) {
                    "INSERT INTO codemode_broca_calls (send_id, run_id, position, snapshot) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(send_id) DO UPDATE SET snapshot = excluded.snapshot"
                } else {
                    "INSERT INTO broca_calls (send_id, run_id, position, snapshot) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(send_id) DO UPDATE SET snapshot = excluded.snapshot"
                };
                tx.execute(
                    sql,
                    rusqlite::params![call.send_id, call.basal_run_id, position, snapshot],
                )?;
                // The wait row mirrors the snapshot in the same transaction.
                // A session_ref the event reader learned stays: only the
                // reader sets it (see `events::learn_session_ref`).
                if call.waiting() {
                    tx.execute(
                        "INSERT INTO broca_waits (send_id, broca_run_id, check_at) VALUES (?1, ?2, ?3) ON CONFLICT(send_id) DO UPDATE SET broca_run_id = excluded.broca_run_id, check_at = excluded.check_at",
                        rusqlite::params![call.send_id, call.broca_run_id, call.check_at],
                    )?;
                } else {
                    tx.execute("DELETE FROM broca_waits WHERE send_id = ?1", [&call.send_id])?;
                    tx.execute("DELETE FROM broca_wakes WHERE send_id = ?1", [&call.send_id])?;
                }
                Ok(())
            })
            .map_err(|e| BrocaError::Store(e.to_string()))
    }
}
