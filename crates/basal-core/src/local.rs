//! The local, eager effect class: calls the runtime serves itself, whose
//! effect commits in the same transaction as the call's outcome.
//!
//! The `kv` primitives are served here against a plain table. That is the
//! shape a flow's durable `kv` takes (eager: each write commits with its
//! journal record, exactly once, and stays visible even if the run later
//! fails), without its size caps and per-flow authorization.

use basal_host::HostOutcome;
use basal_proto::{CallKind, JsonText, Primitive};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::error::Result;

/// Whether the runtime serves `kind` itself as a local effect.
pub fn is_local(kind: &CallKind) -> bool {
    matches!(
        kind,
        CallKind::Primitive(Primitive::KvGet | Primitive::KvSet | Primitive::KvDelete)
    )
}

fn reject(message: &str) -> HostOutcome {
    HostOutcome::rejected(
        JsonText::new(json!({"message": message, "code": "invalid_arguments"}).to_string())
            .unwrap_or_else(|_| JsonText::null()),
    )
}

fn fulfil(value: &Value) -> HostOutcome {
    match JsonText::new(value.to_string()) {
        Ok(text) => HostOutcome::fulfilled(text),
        Err(_) => reject("value too large"),
    }
}

/// Applies a local call inside `tx` and returns its outcome. The caller
/// records the outcome in the same transaction.
pub fn apply(
    tx: &Transaction,
    flow_id: &str,
    key: &str,
    kind: &CallKind,
    args: &JsonText,
) -> Result<HostOutcome> {
    let Ok(args) = serde_json::from_str::<Value>(args.as_str()) else {
        return Ok(reject("arguments are not JSON"));
    };
    let Some(name) = args.get("key").and_then(Value::as_str) else {
        return Ok(reject("kv calls need a string key"));
    };
    let op = match kind {
        CallKind::Primitive(Primitive::KvGet) => {
            let stored: Option<String> = tx
                .query_row(
                    "SELECT value FROM local_kv WHERE flow_id = ?1 AND key = ?2",
                    params![flow_id, name],
                    |r| r.get(0),
                )
                .optional()?;
            let value = match stored {
                Some(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
                None => Value::Null,
            };
            return Ok(fulfil(&value));
        }
        CallKind::Primitive(Primitive::KvSet) => {
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            tx.execute(
                "INSERT INTO local_kv (flow_id, key, value) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (flow_id, key) DO UPDATE SET value = excluded.value",
                params![flow_id, name, value.to_string()],
            )?;
            "kv.set"
        }
        CallKind::Primitive(Primitive::KvDelete) => {
            tx.execute(
                "DELETE FROM local_kv WHERE flow_id = ?1 AND key = ?2",
                params![flow_id, name],
            )?;
            "kv.delete"
        }
        _ => return Ok(reject("not a local call")),
    };
    tx.execute(
        "INSERT INTO local_effects (idempotency_key, flow_id, op) VALUES (?1, ?2, ?3)",
        params![key, flow_id, op],
    )?;
    Ok(fulfil(&json!(true)))
}
