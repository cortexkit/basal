//! A flow's own durable state: `kv.get`, `kv.set` and `kv.delete`.
//!
//! The runtime serves these itself. Each call's journal row, its effect and
//! its outcome commit in one transaction, so a crash leaves all three or
//! none, and recovery never applies a committed write twice. Writes are
//! eager: they stay even if the run later fails or stops in
//! `needs_reconcile`, so a flow that posted something and recorded it does
//! not lose the record. Keys belong to the flow id, so every version of a
//! flow sees the same state.
//!
//! Every limit is checked before the write and refused as a journaled
//! rejection with code `kv_limit`, which the script can catch.

use basal_host::HostOutcome;
use basal_proto::{CallKind, JsonText, Primitive};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::error::{CoreError, Result};

/// Size limits on a flow's `kv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvLimits {
    /// The longest key, in UTF-8 bytes.
    pub max_key_bytes: usize,
    /// The largest value, as JSON text.
    pub max_value_bytes: usize,
    /// The most bytes (keys plus values) one flow may hold.
    pub max_flow_bytes: usize,
}

impl Default for KvLimits {
    fn default() -> Self {
        Self {
            max_key_bytes: 256,
            max_value_bytes: 64 * 1024,
            max_flow_bytes: 1024 * 1024,
        }
    }
}

/// Whether the runtime serves `kind` itself as a local effect.
pub fn is_kv(kind: &CallKind) -> bool {
    matches!(
        kind,
        CallKind::Primitive(Primitive::KvGet | Primitive::KvSet | Primitive::KvDelete)
    )
}

/// A rejection outcome with a code the script can branch on.
pub fn rejection(code: &str, message: &str) -> HostOutcome {
    HostOutcome::rejected(
        JsonText::new(json!({"message": message, "code": code}).to_string())
            .unwrap_or_else(|_| JsonText::null()),
    )
}

fn fulfil(value: &Value) -> HostOutcome {
    match JsonText::new(value.to_string()) {
        Ok(text) => HostOutcome::fulfilled(text),
        Err(_) => rejection("too_large", "the stored value is too large to return"),
    }
}

fn bytes_i64(n: usize) -> Result<i64> {
    i64::try_from(n).map_err(|_| CoreError::Invalid(format!("{n} bytes")))
}

/// Applies a `kv` call inside `tx` and returns its outcome. The caller
/// records the outcome in the same transaction.
pub fn apply(
    tx: &Transaction,
    flow_id: &str,
    kind: &CallKind,
    args: &JsonText,
    limits: &KvLimits,
) -> Result<HostOutcome> {
    let Ok(args) = serde_json::from_str::<Value>(args.as_str()) else {
        return Ok(rejection("invalid_arguments", "arguments are not JSON"));
    };
    let Some(key) = args.get("key").and_then(Value::as_str) else {
        return Ok(rejection("invalid_arguments", "kv calls need a string key"));
    };
    if key.is_empty() {
        return Ok(rejection("invalid_arguments", "kv keys must not be empty"));
    }
    if key.len() > limits.max_key_bytes {
        return Ok(rejection(
            crate::authorize::codes::KV_LIMIT,
            &format!("keys are limited to {} bytes", limits.max_key_bytes),
        ));
    }
    match kind {
        CallKind::Primitive(Primitive::KvGet) => {
            let stored: Option<String> = tx
                .query_row(
                    "SELECT value FROM kv WHERE flow_id = ?1 AND key = ?2",
                    params![flow_id, key],
                    |r| r.get(0),
                )
                .optional()?;
            let value = match stored {
                Some(text) => serde_json::from_str(&text)
                    .map_err(|e| CoreError::Corrupt(format!("kv value for {key:?}: {e}")))?,
                None => Value::Null,
            };
            Ok(fulfil(&value))
        }
        CallKind::Primitive(Primitive::KvSet) => {
            // `kv.set(k, undefined)` arrives without a value and stores null.
            let value = args
                .get("value")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string();
            if value.len() > limits.max_value_bytes {
                return Ok(rejection(
                    crate::authorize::codes::KV_LIMIT,
                    &format!("values are limited to {} bytes", limits.max_value_bytes),
                ));
            }
            let entry = bytes_i64(key.len() + value.len())?;
            let (total, previous): (i64, i64) = tx.query_row(
                "SELECT COALESCE(SUM(bytes), 0), \
                 COALESCE(SUM(CASE WHEN key = ?2 THEN bytes ELSE 0 END), 0) \
                 FROM kv WHERE flow_id = ?1",
                params![flow_id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if total - previous + entry > bytes_i64(limits.max_flow_bytes)? {
                return Ok(rejection(
                    crate::authorize::codes::KV_LIMIT,
                    &format!(
                        "a flow's kv is limited to {} bytes in total",
                        limits.max_flow_bytes
                    ),
                ));
            }
            tx.execute(
                "INSERT INTO kv (flow_id, key, value, bytes, revision) VALUES (?1, ?2, ?3, ?4, 1) \
                 ON CONFLICT (flow_id, key) DO UPDATE SET value = excluded.value, \
                 bytes = excluded.bytes, revision = kv.revision + 1",
                params![flow_id, key, value, entry],
            )?;
            Ok(fulfil(&json!(true)))
        }
        CallKind::Primitive(Primitive::KvDelete) => {
            let removed = tx.execute(
                "DELETE FROM kv WHERE flow_id = ?1 AND key = ?2",
                params![flow_id, key],
            )?;
            Ok(fulfil(&json!(removed > 0)))
        }
        _ => Ok(rejection("invalid_arguments", "not a kv call")),
    }
}
