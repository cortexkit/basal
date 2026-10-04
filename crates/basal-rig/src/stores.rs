//! Read-only views of the stores the suite checks against.
//!
//! Both stores are SQLite in WAL mode with a live writer (core and basal), so
//! each read opens the file with SQLite's read-only flag and never writes,
//! checkpoints or migrates anything. Core's store is read for records no core
//! management op returns: the `flow_sink_receipt` rows and its `flow_install`
//! records. Basal's store is read for its journal, runs and install cards.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

fn open(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {} read-only: {e}", path.display()))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    Ok(connection)
}

/// prefrontal-core's store, read-only.
pub struct CoreStore {
    pub path: PathBuf,
}

/// One sink receipt core recorded: its request key and the reply it stored.
#[derive(Debug, Clone)]
pub struct Receipt {
    pub key: Value,
    pub reply: Value,
}

impl CoreStore {
    /// Every sink receipt of one flow. Core keys a digest receipt
    /// `["sink.digest", flow id, run id, call position]` and a status receipt
    /// `["sink.status", flow id, agent name, status revision]`.
    pub fn receipts(&self, flow_id: &str) -> Result<Vec<Receipt>, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare("SELECT request_key, reply_json FROM flow_sink_receipt ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        let mut receipts = Vec::new();
        for row in rows {
            let (key, reply) = row.map_err(|e| e.to_string())?;
            let key: Value = serde_json::from_str(&key).map_err(|e| e.to_string())?;
            if key.get(1).and_then(Value::as_str) == Some(flow_id) {
                receipts.push(Receipt {
                    key,
                    reply: serde_json::from_str(&reply).map_err(|e| e.to_string())?,
                });
            }
        }
        Ok(receipts)
    }

    /// Core's install records for one flow: version, whether revoked, and
    /// the author it recorded.
    pub fn installs(&self, flow_id: &str) -> Result<Value, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare(
                "SELECT version, revoked_at_ms IS NOT NULL FROM flow_install
                  WHERE flow_id = ?1 ORDER BY version",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok(json!({ "version": r.get::<_, i64>(0)?, "revoked": r.get::<_, bool>(1)? }))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
            .map_err(|e| e.to_string())
    }
}

/// basal's store, read-only.
pub struct BasalStore {
    pub path: PathBuf,
}

/// The journal's kind codes for the primitives the suite checks
/// (`basal_proto::types::Primitive::code`).
pub const KIND_FACTS: i64 = 3;
pub const KIND_SINK_DIGEST: i64 = 6;
pub const KIND_SINK_STATUS: i64 = 7;

/// One journaled call of a run.
#[derive(Debug, Clone)]
pub struct Call {
    pub run_id: String,
    pub position: u64,
    pub kind_code: i64,
    pub dispatch: String,
    pub attempts: i64,
    pub settlement: Option<String>,
    pub value: Option<Value>,
}

impl BasalStore {
    /// The selected request is journaled before dispatch; the Broca snapshot
    /// holds the separate frozen send and terminal result/usage it read.
    pub fn model_call(&self, run_id: &str) -> Result<Option<Value>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT j.args, b.snapshot FROM journal j JOIN broca_calls b
                ON b.run_id = j.run_id AND b.position = j.position
              WHERE j.run_id = ?1 AND j.position = 0",
            [run_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .map(|(args, snapshot)| {
            Ok(
                json!({"journal": serde_json::from_str::<Value>(&args).map_err(|e| e.to_string())?,
                "broca": serde_json::from_str::<Value>(&snapshot).map_err(|e| e.to_string())?}),
            )
        })
        .transpose()
    }

    pub fn ledger(&self, run_id: &str) -> Result<Vec<Value>, String> {
        let c = open(&self.path)?;
        let mut s = c.prepare("SELECT state, reserved, input_tokens, cache_write_tokens, output_tokens,
            cached_input_tokens, unreported_tokens FROM token_ledger WHERE run_id = ?1 ORDER BY position")
            .map_err(|e| e.to_string())?;
        let rows = s.query_map([run_id], |r| Ok(json!({
            "state": r.get::<_, String>(0)?, "reserved": r.get::<_, i64>(1)?,
            "input_tokens": r.get::<_, Option<i64>>(2)?, "cache_write_tokens": r.get::<_, Option<i64>>(3)?,
            "output_tokens": r.get::<_, Option<i64>>(4)?, "cached_input_tokens": r.get::<_, Option<i64>>(5)?,
            "unreported_tokens": r.get::<_, Option<i64>>(6)?,
        }))).map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// The state of the install card for one flow version, if basal has one.
    pub fn card_state(&self, flow_id: &str, version: i64) -> Result<Option<String>, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT state FROM install_cards WHERE flow_id = ?1 AND version = ?2",
            params![flow_id, version],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    /// Every journaled call of every run of one flow, in order.
    pub fn calls(&self, flow_id: &str) -> Result<Vec<Call>, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare(
                "SELECT j.run_id, j.position, j.kind_code, j.dispatch, j.attempts,
                        j.settlement, j.value
                   FROM journal j JOIN runs r ON r.run_id = j.run_id
                  WHERE r.flow_id = ?1
                  ORDER BY r.rowid, j.position",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut calls = Vec::new();
        for row in rows {
            let ((run_id, position), kind_code, dispatch, attempts, settlement, value) =
                row.map_err(|e| e.to_string())?;
            calls.push(Call {
                run_id,
                position: u64::try_from(position).unwrap_or(0),
                kind_code,
                dispatch,
                attempts,
                settlement,
                value: value.map(|v| serde_json::from_str(&v).unwrap_or(Value::String(v))),
            });
        }
        Ok(calls)
    }

    /// How many activations of a run basal recorded. A claim that basal's
    /// install gate turned back before the run reached a worker is not one.
    pub fn activations(&self, run_id: &str) -> Result<i64, String> {
        let c = open(&self.path)?;
        c.query_row(
            "SELECT COUNT(*) FROM activations WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())
    }

    /// The runs of one flow, oldest first: id, state, the script's return
    /// value and the error, if any.
    pub fn runs(&self, flow_id: &str) -> Result<Vec<Run>, String> {
        let c = open(&self.path)?;
        let mut statement = c
            .prepare(
                "SELECT run_id, state, result, error_kind, error_detail FROM runs
                  WHERE flow_id = ?1 ORDER BY rowid",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![flow_id], |r| {
                Ok(Run {
                    run_id: r.get(0)?,
                    state: r.get(1)?,
                    result: r
                        .get::<_, Option<String>>(2)?
                        .map(|v| serde_json::from_str(&v).unwrap_or(Value::String(v))),
                    error: match (
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ) {
                        (None, None) => None,
                        (kind, detail) => Some(json!({ "kind": kind, "detail": detail })),
                    },
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

/// Broca's independent run index, not basal's assertion about sends. These
/// reads never open a Broca route or initiate a model call. The index stores
/// the full bind triple as JSON; message.session uses the U+001F-joined triple.
pub struct BrocaStore {
    pub path: PathBuf,
}

impl BrocaStore {
    pub fn runs(&self, route: &Value) -> Result<Vec<Value>, String> {
        let c = open(&self.path)?;
        let mut s = c
            .prepare(
                "SELECT run_id, state, usage_json FROM run_index
            WHERE json_extract(session, '$.project_root') = ?1
              AND json_extract(session, '$.harness') = ?2
              AND json_extract(session, '$.session') = ?3 ORDER BY rowid",
            )
            .map_err(|e| e.to_string())?;
        let field = |name| {
            route[name]
                .as_str()
                .ok_or_else(|| format!("missing route {name}"))
        };
        let rows = s
            .query_map(
                params![field("project_root")?, field("harness")?, field("session")?],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let (id, state, usage) = row.map_err(|e| e.to_string())?;
            Ok(json!({"run_id": id, "state": state, "usage": usage
                .map(|u| serde_json::from_str::<Value>(&u)).transpose().map_err(|e| e.to_string())?}))
        }).collect()
    }

    /// The same final assistant text that run.result reads from the durable
    /// transcript. Each flow call has its own session and exactly one run.
    pub fn final_text(&self, route: &Value) -> Result<Option<String>, String> {
        let c = open(&self.path)?;
        let key = ["project_root", "harness", "session"]
            .iter()
            .map(|k| {
                route[k]
                    .as_str()
                    .ok_or_else(|| format!("missing route {k}"))
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("\u{1f}");
        let mut s = c
            .prepare("SELECT json FROM message WHERE session = ?1 ORDER BY ord DESC")
            .map_err(|e| e.to_string())?;
        let rows = s
            .query_map([key], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let msg: Value = serde_json::from_str(&row.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            if msg["role"] == "assistant" {
                let content = msg["content"]
                    .as_array()
                    .ok_or("assistant message has no content")?;
                return Ok(Some(
                    content
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect(),
                ));
            }
        }
        Ok(None)
    }
}

/// One run of a flow, as basal recorded it.
#[derive(Debug, Clone)]
pub struct Run {
    pub run_id: String,
    pub state: String,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl Run {
    /// Whether the run has ended, one way or the other.
    pub fn ended(&self) -> bool {
        !matches!(self.state.as_str(), "pending" | "running" | "suspended")
    }
}
