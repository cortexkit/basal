//! Stored-value compatibility, changed only in the transaction admitting work.

#[cfg(test)]
#[path = "state_epoch_tests.rs"]
mod tests;

use rusqlite::{Connection, Transaction, params};

use crate::error::{CoreError, Result};
use crate::manifest::Manifest;

pub fn recorded(conn: &Connection, flow_id: &str) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT state_epoch FROM flows WHERE flow_id=?1",
        [flow_id],
        |r| r.get(0),
    )?)
}

/// Compare an ordinary flow or all instances of a package/template on one snapshot.
pub fn would_reset(conn: &Connection, flow_id: &str, epoch: u32) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM flows WHERE (flow_id=?1 OR package=?1) AND state_epoch<>?2)",
        params![flow_id, epoch],
        |r| r.get(0),
    )?)
}

/// Durable reset history is the log: it cannot describe a rolled-back reset.
pub(crate) fn reset(tx: &Transaction, flow_id: &str, manifest: &str, now: i64) -> Result<()> {
    let new_epoch = Manifest::parse(manifest)
        .map_err(|e| CoreError::Corrupt(e.to_string()))?
        .state_epoch;
    let old_epoch = recorded(tx, flow_id)?;
    if old_epoch != new_epoch {
        let keys_cleared = tx.execute("DELETE FROM kv WHERE flow_id=?1", [flow_id])?;
        tx.execute(
            "UPDATE flows SET state_epoch=?2,last_state_reset_at=?3 WHERE flow_id=?1",
            params![flow_id, new_epoch, now],
        )?;
        tx.execute("INSERT INTO state_resets(flow_id,old_epoch,new_epoch,keys_cleared,at) VALUES (?1,?2,?3,?4,?5)", params![flow_id, old_epoch, new_epoch, keys_cleared, now])?;
    }
    Ok(())
}
