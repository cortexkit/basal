//! Saved Broca calls stored beside the call journal. Text, completion reason,
//! token usage and the cursor used to resume reading commit together.

use crate::{CoreError, Store};
use basal_host::broca::{BrocaError, StateStore, StoredCall};
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
