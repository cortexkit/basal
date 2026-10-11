//! Transactional lifecycle outbox. Bodies remain here for authorized retrieval;
//! the bus carries only their digest and routing labels.
use crate::{Result, Runtime};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const EVENTS: [&str; 4] = [
    "flow_instance_created",
    "flow_instance_extended",
    "flow_instance_ended",
    "flow_instance_expired",
];
pub const BODY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct OutboxEvent {
    pub flow_id: String,
    pub seq: i64,
    pub event_name: String,
    pub agent_id: String,
    pub body: String,
}
impl OutboxEvent {
    pub fn event_key(&self) -> String {
        format!("{}:{}", self.flow_id, self.seq)
    }
}

fn snapshot(c: &rusqlite::Connection, flow_id: &str) -> Result<Value> {
    Ok(c.query_row("SELECT f.flow_id,f.package,f.approved_version,f.owner,t.expires_at,t.seq FROM template_instances t JOIN flows f USING(flow_id) WHERE flow_id=?1", [flow_id], |r| Ok(json!({"flow_id":r.get::<_,String>(0)?,"template":r.get::<_,String>(1)?,"version":r.get::<_,u32>(2)?,"agent_id":r.get::<_,String>(3)?,"expires_at":r.get::<_,i64>(4)?,"seq":r.get::<_,i64>(5)?})))?)
}

/// The caller's state change, sequence increment and immutable body commit in
/// one transaction. A failed insert therefore rolls back the state change too.
pub(crate) fn enqueue(tx: &Transaction<'_>, flow_id: &str, event: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE template_instances SET seq=seq+1 WHERE flow_id=?1",
        [flow_id],
    )?;
    let mut body = snapshot(tx, flow_id)?;
    if event == "flow_instance_expired" {
        let text: String = tx.query_row(
            "SELECT params FROM template_instances WHERE flow_id=?1",
            [flow_id],
            |r| r.get(0),
        )?;
        body["params"] =
            serde_json::from_str(&text).map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
    }
    tx.execute("INSERT INTO template_event_outbox(flow_id,seq,event_name,body,retry_at) VALUES (?1,?2,?3,?4,?5)", params![flow_id, body["seq"].as_i64(), event, body.to_string(), now])?;
    Ok(())
}

pub(crate) fn prune(tx: &Transaction<'_>, now: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM template_event_outbox WHERE published_at IS NOT NULL AND published_at<?1",
        [now.saturating_sub(BODY_RETENTION_MS)],
    )?;
    Ok(())
}

impl Runtime {
    pub fn live_instances(&self) -> Result<Value> {
        self.store().read(|c| {
            let mut stmt = c.prepare("SELECT t.flow_id FROM template_instances t JOIN flows f USING(flow_id) WHERE f.removed=0 ORDER BY t.flow_id")?;
            let ids = stmt.query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let instances = ids.iter().map(|id| snapshot(c, id)).collect::<Result<Vec<_>>>()?;
            Ok(json!({"instances":instances}))
        })
    }

    pub fn pending_template_events(&self, limit: usize) -> Result<Vec<OutboxEvent>> {
        let now = self.config().clock.now_ms();
        self.store().read(|c| {
            let mut stmt = c.prepare("SELECT flow_id,seq,event_name,body FROM template_event_outbox WHERE published_at IS NULL AND retry_at<=?1 ORDER BY retry_at,flow_id,seq LIMIT ?2")?;
            Ok(stmt.query_map(params![now, limit as i64], |r| {
                let body: String = r.get(3)?;
                let value: Value = serde_json::from_str(&body).map_err(|e| rusqlite::Error::FromSqlConversionFailure(3,rusqlite::types::Type::Text,Box::new(e)))?;
                Ok(OutboxEvent {flow_id:r.get(0)?,seq:r.get(1)?,event_name:r.get(2)?,agent_id:value["agent_id"].as_str().unwrap_or_default().into(),body})
            })?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn template_event_published(&self, event: &OutboxEvent) -> Result<()> {
        self.store().write(|tx| {
            tx.execute("UPDATE template_event_outbox SET published_at=?3 WHERE flow_id=?1 AND seq=?2 AND published_at IS NULL", params![event.flow_id,event.seq,self.config().clock.now_ms()])?;
            Ok(())
        })
    }

    pub fn template_event_retry(&self, event: &OutboxEvent) -> Result<()> {
        self.store().write(|tx| {
            let attempts: u32 = tx.query_row("SELECT attempts FROM template_event_outbox WHERE flow_id=?1 AND seq=?2", params![event.flow_id,event.seq], |r|r.get(0))?;
            let delay = (250i64 * (1i64 << attempts.min(7))).min(30_000);
            tx.execute("UPDATE template_event_outbox SET attempts=attempts+1,retry_at=?3 WHERE flow_id=?1 AND seq=?2 AND published_at IS NULL", params![event.flow_id,event.seq,self.config().clock.now_ms().saturating_add(delay)])?;
            Ok(())
        })
    }

    /// Authorization belongs to the module operation, before this lookup.
    /// Pending bodies are readable too: a notice can arrive before its ack is
    /// recorded locally, and a crash can leave that recording incomplete.
    pub fn template_event_body(&self, event_key: &str) -> Result<Option<Value>> {
        self.store().read(|c| {
            Ok(c.query_row("SELECT body,published_at,event_name FROM template_event_outbox WHERE flow_id||':'||seq=?1", [event_key], |r| {
                let body: String = r.get(0)?;
                let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
                Ok(json!({"body":body,"digest":digest,"event_key":event_key,"event_name":r.get::<_,String>(2)?,"published_at":r.get::<_,Option<i64>>(1)?}))
            }).optional()?)
        })
    }
}
