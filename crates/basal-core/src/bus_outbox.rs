//! Bus delivery state is independent of the fallback call on the same row.
use crate::{CoreError, Result, Runtime};
use cortexkit_role_tool_provider::{
    LateResultReady, late_result_ready_msg_id, late_result_ready_reader,
};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LATE_RESULT_READY: &str = "codemode.late_result_ready";

#[derive(Serialize, Deserialize)]
struct LateResultWake {
    #[serde(flatten)]
    ready: LateResultReady,
    event_id: String,
}

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
    call_key: &str,
    outcome: &str,
    event_id: &str,
    now: i64,
) -> Result<()> {
    let Some(module) = late_result_ready_reader(custodian) else {
        // Direct callers have no module reader and continue pulling the feed.
        return Ok(());
    };
    let wake = LateResultWake {
        ready: LateResultReady::new(call_key, outcome),
        event_id: event_id.to_owned(),
    };
    wake.ready
        .validate()
        .map_err(|e| CoreError::Invalid(e.to_string()))?;
    tx.execute("INSERT INTO outbox(at,kind,flow_id,recipient,body,bus_retry_at) VALUES (?1,?2,'',?3,?4,?1)",
        params![now, LATE_RESULT_READY, module, serde_json::to_string(&wake).map_err(|e| CoreError::Invalid(e.to_string()))?])?;
    Ok(())
}

impl Runtime {
    /// Earliest durable bus retry, including lifecycle events. No pending rows
    /// means the publisher can remain asleep until an outbox commit signals it.
    pub fn next_bus_event_at(&self) -> Result<Option<i64>> {
        self.store().read(|c| {
            Ok(c.query_row("SELECT MIN(at) FROM (SELECT MIN(bus_retry_at) AS at FROM outbox WHERE bus_published_at IS NULL AND kind IN ('flow.owner_notice','codemode.late_result_ready','attribution.fact') UNION ALL SELECT MIN(retry_at) AS at FROM template_event_outbox WHERE published_at IS NULL)", [], |r| r.get(0))?)
        })
    }

    pub fn pending_bus_events(&self, limit: usize) -> Result<Vec<BusEvent>> {
        let rows = self.store().read(|c| {
            let mut stmt = c.prepare("SELECT seq,kind,recipient,body FROM outbox WHERE bus_published_at IS NULL AND kind IN ('flow.owner_notice','codemode.late_result_ready','attribution.fact') AND bus_retry_at<=?1 ORDER BY bus_retry_at,seq LIMIT ?2")?;
            let rows = stmt.query_map(params![self.config().clock.now_ms(), limit as i64], |r| {
                Ok((r.get::<_,i64>(0)?, r.get::<_,String>(1)?, r.get::<_,Option<String>>(2)?, r.get::<_,String>(3)?))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.into_iter().filter_map(|(seq,kind,reader,body)| {
            // Old builds stored only a feed sequence. A wake is only a hint:
            // readers reconcile on startup, so retire it instead of retrying an
            // obsolete payload or guessing the feed entry's event id.
            if kind == LATE_RESULT_READY
                && serde_json::from_str::<Value>(&body).ok().is_some_and(|value| {
                    value.as_object().is_some_and(|fields| fields.len() == 1)
                        && value["seq"].as_i64().is_some_and(|seq| seq > 0)
                })
            {
                return match self.finish_bus_row(seq) {
                    Ok(changed) => {
                        if changed {
                            tracing::info!(target: "events", seq, "retired legacy late-result wake without publishing; readers reconcile their feeds");
                        }
                        None
                    }
                    Err(error) => Some(Err(error)),
                };
            }
            let event = (|| {
                let value: Value = serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                let (name, reader, message_id, body) = if kind == LATE_RESULT_READY {
                    let reader = reader.ok_or_else(|| CoreError::Corrupt("wake lacks recipient".into()))?;
                    let wake: LateResultWake = serde_json::from_str(&body).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    wake.ready.validate().map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    if wake.event_id.is_empty() {
                        return Err(CoreError::Corrupt("wake lacks event id".into()));
                    }
                    let target = if basal_host::core_identity::is_core_module(&reader) {
                        self.catalog().core_module().to_owned()
                    } else { reader };
                    let message_id = late_result_ready_msg_id(&wake.event_id, &target);
                    let payload = serde_json::to_string(&wake.ready).map_err(|e| CoreError::Corrupt(e.to_string()))?;
                    ("late_result_ready", Some(target), message_id, payload)
                } else if kind == "attribution.fact" {
                    ("attribution_fact", None, value["fact_id"].as_str().ok_or_else(|| CoreError::Corrupt("attribution fact lacks id".into()))?.to_owned(), body)
                } else {
                    ("flow_owner_notice", None, value["notice_id"].as_str().ok_or_else(|| CoreError::Corrupt("owner notice lacks id".into()))?.to_owned(), body)
                };
                Ok(BusEvent { seq, name, reader, message_id, body })
            })();
            if event.is_err() {
                match self.retry_bus_row(seq) {
                    Ok(true) => tracing::warn!(target: "events", seq, "malformed bus outbox row; retaining it with durable backoff"),
                    Ok(false) => {}
                    Err(error) => return Some(Err(error)),
                }
            }
            Some(event)
        }).collect()
    }

    pub fn bus_event_published(&self, event: &BusEvent) -> Result<()> {
        self.finish_bus_row(event.seq).map(|_| ())
    }

    fn finish_bus_row(&self, seq: i64) -> Result<bool> {
        self.store().write(|tx| {
            let changed = tx.execute(
                "UPDATE outbox SET bus_published_at=?2 WHERE seq=?1 AND bus_published_at IS NULL",
                params![seq, self.config().clock.now_ms()],
            )?;
            Ok(changed != 0)
        })
    }

    pub fn bus_event_retry(&self, event: &BusEvent) -> Result<()> {
        self.retry_bus_row(event.seq).map(|_| ())
    }

    fn retry_bus_row(&self, seq: i64) -> Result<bool> {
        self.store().write(|tx| {
            let attempts: u32 = tx.query_row("SELECT bus_attempts FROM outbox WHERE seq=?1", [seq], |r|r.get(0))?;
            let delay = (250i64 * (1i64 << attempts.min(7))).min(30_000);
            tx.execute("UPDATE outbox SET bus_attempts=bus_attempts+1,bus_retry_at=?2 WHERE seq=?1 AND bus_published_at IS NULL", params![seq,self.config().clock.now_ms().saturating_add(delay)])?;
            Ok(attempts == 0)
        })
    }
}
