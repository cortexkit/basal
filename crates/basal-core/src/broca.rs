//! Saved Broca calls stored beside the call journal: one snapshot per call,
//! holding its frozen send, its Broca run, its outcome once `run.result`
//! reported one, and whether the runtime has recorded that outcome.

use crate::{CoreError, Store};
use basal_host::broca::{BrocaError, StateStore, StoredCall};
use rusqlite::OptionalExtension;
use std::sync::{Arc, Mutex};

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
    fn get(&self, send_id: &str) -> Result<Option<StoredCall>, BrocaError> {
        let snapshot: Option<String> = self
            .store()?
            .read(|c| {
                Ok(c.query_row(
                    "SELECT snapshot FROM broca_calls WHERE send_id = ?1",
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
        self.store()?.read(|c| {
            let mut statement = c.prepare("SELECT send_id FROM broca_calls WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0 ORDER BY send_id")?;
            Ok(statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?)
        }).map_err(|e| BrocaError::Store(e.to_string()))
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
    fn load(&self) -> Result<Vec<StoredCall>, BrocaError> {
        self.store()?
            .read(|c| {
                let mut stmt = c.prepare("SELECT snapshot FROM broca_calls ORDER BY send_id")?;
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
        self.store()?.write(|tx| {
            tx.execute("INSERT INTO broca_calls (send_id, run_id, position, snapshot) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(send_id) DO UPDATE SET snapshot = excluded.snapshot",
                rusqlite::params![call.send_id, call.basal_run_id, i64::try_from(call.position).map_err(|_| CoreError::Invalid("Broca call position".into()))?, snapshot])?;
            Ok(())
        }).map_err(|e| BrocaError::Store(e.to_string()))
    }
}
