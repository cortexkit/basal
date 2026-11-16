//! Bus delivery state is independent of the fallback call on the same row.
use crate::{CoreError, Result, Runtime};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};

pub const LATE_RESULT_READY: &str = "codemode.late_result_ready";

pub struct BusEvent {
    pub seq: i64,
    pub name: &'static str,
    pub reader: Option<String>,
    pub message_id: String,
    pub body: String,
}

/// Called only for a newly inserted feed row, inside that row's transaction.
pub fn enqueue_late_result_wake(
    tx: &Transaction<'_>,
    custodian: &str,
    seq: i64,
    now: i64,
) -> Result<()> {
    let Some(module) = custodian.strip_prefix("reserved:") else {
        // Direct callers have no module reader and continue pulling the feed.
        return Ok(());
    };
    cortexkit_bus_naming::validate_token(cortexkit_bus_naming::TokenKind::ModuleId, module)
        .map_err(|e| CoreError::Invalid(e.to_string()))?;
    tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body,bus_retry_at) VALUES (?1,?2,'',?3,?4,?1)",
        params![now, LATE_RESULT_READY, module, json!({"seq":seq}).to_string()])?;
    Ok(())
}

impl Runtime {
    pub fn pending_bus_events(&self, limit: usize) -> Result<Vec<BusEvent>> {
        self.store().read(|c| {
            let mut stmt = c.prepare("SELECT seq,kind,recipient,body FROM outbox WHERE bus_published_at IS NULL AND kind IN ('flow.owner_notice','codemode.late_result_ready','attribution.fact') AND bus_retry_at<=?1 ORDER BY bus_retry_at,seq LIMIT ?2")?;
            let rows = stmt.query_map(params![self.config().clock.now_ms(), limit as i64], |r| {
                Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?, r.get::<_,String>(3)?))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|(seq,kind,reader,body)| {
                let value: Value = serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                let (name, reader, message_id) = if kind == LATE_RESULT_READY {
                    let reader = reader.ok_or_else(|| CoreError::Corrupt("wake lacks recipient".into()))?;
                    let feed_seq = value["seq"].as_i64().filter(|s| *s > 0).ok_or_else(|| CoreError::Corrupt("wake lacks sequence".into()))?;
                    ("late_result_ready", Some(reader.clone()), format!("{reader}:{feed_seq}"))
                } else if kind == "attribution.fact" {
                    ("attribution_fact", None, value["fact_id"].as_str().ok_or_else(|| CoreError::Corrupt("attribution fact lacks id".into()))?.to_owned())
                } else {
                    ("flow_owner_notice", None, value["notice_id"].as_str().ok_or_else(|| CoreError::Corrupt("owner notice lacks id".into()))?.to_owned())
                };
                Ok(BusEvent { seq, name, reader, message_id, body })
            }).collect()
        })
    }

    pub fn bus_event_published(&self, event: &BusEvent) -> Result<()> {
        self.store().write(|tx| {
            tx.execute(
                "UPDATE outbox SET bus_published_at=?2 WHERE seq=?1 AND bus_published_at IS NULL",
                params![event.seq, self.config().clock.now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn bus_event_retry(&self, event: &BusEvent) -> Result<()> {
        self.store().write(|tx| {
            let attempts: u32 = tx.query_row("SELECT bus_attempts FROM outbox WHERE seq=?1", [event.seq], |r|r.get(0))?;
            let delay = (250i64 * (1i64 << attempts.min(7))).min(30_000);
            tx.execute("UPDATE outbox SET bus_attempts=bus_attempts+1,bus_retry_at=?2 WHERE seq=?1 AND bus_published_at IS NULL", params![event.seq,self.config().clock.now_ms().saturating_add(delay)])?;
            Ok(())
        })
    }
}
