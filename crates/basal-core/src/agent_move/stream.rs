//! The `basal-flows/1` stream: one agent's flows as they leave one computer
//! and arrive on another.
//!
//! The stream is a sequence of frames. Each frame is a 4-byte big-endian
//! length followed by that many bytes of canonical JSON (object keys sorted,
//! no whitespace). The first frame is the header
//! `{format, agent, cut_id, counts}`; every later frame is one record with a
//! `kind`: `flow` (with its stored versions, their tool policies and its
//! schedule), `instance`, `generation`, `kv`, `snapshot` or `history` (with
//! the run's call rows). Script and manifest text is carried exactly, so the
//! receiver recomputes every code hash and refuses a stream whose bytes no
//! longer match it.
//!
//! Rows are carried column by column from fixed lists. A record with a
//! column the list doesn't name, or without one it names, is refused: a
//! column this build doesn't know would otherwise be dropped silently.

use std::collections::BTreeMap;

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, params_from_iter};
use serde_json::{Map, Value, json};

use crate::{CoreError, Result};

pub const FORMAT: &str = "basal-flows/1";

/// The largest stream basal writes or reads. An agent whose flows don't fit
/// is refused `not_exportable`.
pub const STREAM_CAP: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Col {
    Int,
    Text,
    /// Carried as lowercase hex.
    Blob,
}

pub(crate) type Columns = &'static [(&'static str, Col)];

use Col::{Blob, Int, Text};

pub(crate) const FLOW: Columns = &[
    ("flow_id", Text),
    ("owner", Text),
    ("state", Text),
    ("approved_version", Int),
    ("disabled_by", Text),
    ("disabled_reason", Text),
    ("disabled_at", Int),
    ("created_at", Int),
    ("state_epoch", Int),
    ("last_state_reset_at", Int),
    ("removed_by", Text),
    ("removed_at", Int),
    ("first_approved_at", Int),
    ("agent_retirement", Text),
    ("package", Text),
    ("removed", Int),
];

pub(crate) const VERSION: Columns = &[
    ("version", Int),
    ("code_hash", Blob),
    ("manifest", Text),
    ("script", Text),
    ("author", Text),
    ("loop_override", Int),
    ("approval_ref", Text),
    ("state", Text),
    ("installed_at", Int),
    ("approved_at", Int),
    ("revoked_at", Int),
    ("revoked_reason", Text),
    ("approval_kind", Text),
    ("approval_rule", Text),
];

pub(crate) const POLICY: Columns = &[
    ("version", Int),
    ("module", Text),
    ("op", Text),
    ("policy", Text),
];

pub(crate) const SCHEDULE: Columns = &[
    ("version", Int),
    ("spec", Text),
    ("state", Text),
    ("anchor_ms", Int),
    ("next_due_ms", Int),
    ("last_fired_ms", Int),
];

pub(crate) const INSTANCE: Columns = &[
    ("flow_id", Text),
    ("params", Text),
    ("grant_guards", Text),
    ("expires_at", Int),
    ("incarnation", Int),
    ("seq", Int),
    ("expiry_notified", Int),
];

pub(crate) const GENERATION: Columns = &[
    ("package", Text),
    ("agent_id", Text),
    ("generation", Int),
    ("operation", Text),
    ("version", Int),
    ("reply", Text),
];

pub(crate) const KV: Columns = &[
    ("flow_id", Text),
    ("key", Text),
    ("value", Text),
    ("bytes", Int),
    ("revision", Int),
];

pub(crate) const SNAPSHOT: Columns = &[
    ("flow_id", Text),
    ("flow_version", Int),
    ("code_hash", Blob),
    ("script", Text),
    ("manifest", Text),
];

/// Every `run_history` column except `snapshot_id`, which is local: the
/// receiver finds the run's snapshot by flow, version and code hash.
pub(crate) const HISTORY: Columns = &[
    ("run_id", Text),
    ("flow_id", Text),
    ("flow_version", Int),
    ("trigger_id", Text),
    ("trigger_kind", Text),
    ("trigger", Text),
    ("admitted_at", Int),
    ("admit_seq", Int),
    ("first_claim_at", Int),
    ("ended_at", Int),
    ("state", Text),
    ("error_kind", Text),
    ("error_detail", Text),
    ("result", Text),
    ("result_size", Int),
    ("call_counts", Text),
    ("writes_sent", Int),
    ("tokens_charged", Int),
    ("cost_microusd", Int),
    ("broken", Int),
    ("activation_count", Int),
    ("script_started", Int),
    ("phase", Text),
    ("deadline_exempt", Int),
    ("history_lost", Int),
    ("commit_seq", Int),
    ("receipts", Text),
];

pub(crate) const CALL: Columns = &[
    ("position", Int),
    ("kind", Int),
    ("class", Text),
    ("module", Text),
    ("op", Text),
    ("args_digest", Blob),
    ("outcome", Text),
    ("error_code", Text),
    ("issued_at", Int),
    ("arrived_at", Int),
    ("released_at", Int),
    ("detail", Text),
];

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex back to bytes; `None` for anything else.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2)
        || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn column_list(columns: Columns, alias: &str) -> String {
    columns
        .iter()
        .map(|(name, _)| format!("{alias}{name}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reads rows as records. `select` names the table and its filter after
/// `SELECT <columns> `, for example `FROM kv WHERE flow_id=?1 ORDER BY key`;
/// `alias` prefixes each column (`"f."` or `""`).
pub(crate) fn rows(
    conn: &Connection,
    columns: Columns,
    alias: &str,
    select: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Map<String, Value>>> {
    let sql = format!("SELECT {} {select}", column_list(columns, alias));
    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::new();
    let mut query = stmt.query(params)?;
    while let Some(row) = query.next()? {
        let mut record = Map::new();
        for (i, (name, col)) in columns.iter().enumerate() {
            let value = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Text(t) => Value::String(
                    std::str::from_utf8(t)
                        .map_err(|e| CoreError::Corrupt(format!("{name}: {e}")))?
                        .to_owned(),
                ),
                ValueRef::Blob(b) => Value::String(hex(b)),
                ValueRef::Real(_) => {
                    return Err(CoreError::Corrupt(format!("{name} holds a real number")));
                }
            };
            let fits = matches!(
                (col, &value),
                (_, Value::Null)
                    | (Int, Value::Number(_))
                    | (Text, Value::String(_))
                    | (Blob, Value::String(_))
            );
            if !fits {
                return Err(CoreError::Corrupt(format!(
                    "{name} holds a value of another type"
                )));
            }
            record.insert((*name).to_owned(), value);
        }
        out.push(record);
    }
    Ok(out)
}

/// The SQL values of a record's columns, in list order, after checking the
/// record names exactly those columns (plus `extra`, which the caller reads
/// itself) with values of the listed types.
pub(crate) fn values(
    record: &Map<String, Value>,
    columns: Columns,
    extra: &[&str],
    what: &str,
) -> std::result::Result<Vec<SqlValue>, String> {
    for key in record.keys() {
        if !columns.iter().any(|(name, _)| name == key) && !extra.contains(&key.as_str()) {
            return Err(format!("{what} record has an unknown member {key:?}"));
        }
    }
    columns
        .iter()
        .map(|(name, col)| {
            let value = record
                .get(*name)
                .ok_or_else(|| format!("{what} record has no {name:?}"))?;
            Ok(match (col, value) {
                (_, Value::Null) => SqlValue::Null,
                (Int, Value::Number(n)) => SqlValue::Integer(
                    n.as_i64()
                        .ok_or_else(|| format!("{what}.{name} is not an integer"))?,
                ),
                (Text, Value::String(s)) => SqlValue::Text(s.clone()),
                (Blob, Value::String(s)) => SqlValue::Blob(
                    unhex(s).ok_or_else(|| format!("{what}.{name} is not lowercase hex"))?,
                ),
                _ => return Err(format!("{what}.{name} has the wrong type")),
            })
        })
        .collect()
}

/// Inserts one record into `table` (`verb` is `INSERT` or `INSERT OR
/// IGNORE`), with `fixed` columns set from the caller instead of the record.
pub(crate) fn insert(
    tx: &rusqlite::Transaction,
    verb: &str,
    table: &str,
    columns: Columns,
    values: Vec<SqlValue>,
    fixed: &[(&str, SqlValue)],
) -> Result<usize> {
    let mut names: Vec<&str> = columns.iter().map(|(n, _)| *n).collect();
    let mut all = values;
    for (name, value) in fixed {
        names.push(name);
        all.push(value.clone());
    }
    let marks = (1..=names.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(tx.execute(
        &format!("{verb} INTO {table} ({}) VALUES ({marks})", names.join(",")),
        params_from_iter(all),
    )?)
}

/// Canonical JSON: keys sorted (serde_json's map is ordered), no spaces.
pub(crate) fn canonical(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("a JSON value always serializes")
}

/// Appends frames to a stream, refusing to grow it past [`STREAM_CAP`].
pub(crate) struct Writer {
    pub(crate) bytes: Vec<u8>,
    pub(crate) counts: BTreeMap<String, u64>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub(crate) fn frame(&mut self, value: &Value) -> std::result::Result<(), String> {
        let body = canonical(value);
        let total = self.bytes.len() as u64 + 4 + body.len() as u64;
        if total > STREAM_CAP {
            return Err(format!(
                "the agent's flows need more than the {} MiB a move carries",
                STREAM_CAP >> 20
            ));
        }
        let length = u32::try_from(body.len()).map_err(|_| "a record is too large".to_owned())?;
        self.bytes.extend(length.to_be_bytes());
        self.bytes.extend(body);
        if let Some(kind) = value["kind"].as_str() {
            *self.counts.entry(kind.to_owned()).or_default() += 1;
        }
        Ok(())
    }

    /// The finished stream: the header, then the records.
    pub(crate) fn finish(
        self,
        agent: &str,
        cut_id: &str,
        mut counts: BTreeMap<String, u64>,
    ) -> std::result::Result<Vec<u8>, String> {
        for (kind, n) in self.counts {
            counts.insert(kind, n);
        }
        let mut out = Writer::new();
        out.frame(&json!({"format": FORMAT, "agent": agent, "cut_id": cut_id, "counts": counts}))?;
        if out.bytes.len() as u64 + self.bytes.len() as u64 > STREAM_CAP {
            return Err(format!(
                "the agent's flows need more than the {} MiB a move carries",
                STREAM_CAP >> 20
            ));
        }
        out.bytes.extend(self.bytes);
        Ok(out.bytes)
    }
}

/// Why a stream could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The header names a format this build doesn't read.
    Incompatible { found: String },
    /// The bytes don't form the stream they claim to be: a cut frame, a
    /// record that doesn't parse, or a code hash that doesn't match its text.
    Damaged(String),
}

/// A decoded stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    pub agent: String,
    pub cut_id: String,
    pub counts: BTreeMap<String, u64>,
    pub records: Vec<Map<String, Value>>,
}

impl Staged {
    pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Map<String, Value>> {
        self.records
            .iter()
            .filter(move |r| r["kind"].as_str() == Some(kind))
    }

    /// The ids of the flows the stream carries.
    pub fn flow_ids(&self) -> Vec<String> {
        self.of_kind("flow")
            .filter_map(|r| r["flow_id"].as_str().map(str::to_owned))
            .collect()
    }

    /// Queued events left behind on the source, as the import reply's
    /// `dropped` map: absent when nothing was left behind.
    pub fn dropped(&self) -> Option<BTreeMap<String, u64>> {
        let queued = self.counts.get("queued_event").copied().unwrap_or(0);
        (queued > 0).then(|| BTreeMap::from([("queued_event".to_owned(), queued)]))
    }
}

fn frames(bytes: &[u8]) -> std::result::Result<Vec<&[u8]>, DecodeError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let head = bytes
            .get(at..at + 4)
            .ok_or_else(|| DecodeError::Damaged("the stream ends inside a frame length".into()))?;
        let length = u32::from_be_bytes(head.try_into().expect("four bytes")) as usize;
        let body = bytes
            .get(at + 4..at + 4 + length)
            .ok_or_else(|| DecodeError::Damaged("the stream ends inside a frame".into()))?;
        out.push(body);
        at += 4 + length;
    }
    Ok(out)
}

/// Reads only as far as the header, to decide whether this build can read
/// the stream at all.
pub fn header(bytes: &[u8]) -> std::result::Result<Value, DecodeError> {
    let head = bytes
        .get(..4)
        .ok_or_else(|| DecodeError::Damaged("the stream has no header".into()))?;
    let length = u32::from_be_bytes(head.try_into().expect("four bytes")) as usize;
    let body = bytes
        .get(4..4 + length)
        .ok_or_else(|| DecodeError::Damaged("the stream ends inside its header".into()))?;
    let header: Value = serde_json::from_slice(body)
        .map_err(|e| DecodeError::Damaged(format!("the header is not JSON: {e}")))?;
    let format = header["format"]
        .as_str()
        .ok_or_else(|| DecodeError::Damaged("the header names no format".into()))?;
    if format != FORMAT {
        return Err(DecodeError::Incompatible {
            found: format.to_owned(),
        });
    }
    Ok(header)
}

/// Decodes a whole stream and checks it: the header first, then every
/// record's shape and every carried code hash.
pub fn decode(bytes: &[u8]) -> std::result::Result<Staged, DecodeError> {
    if bytes.len() as u64 > STREAM_CAP {
        return Err(DecodeError::Damaged(
            "the stream is larger than its cap".into(),
        ));
    }
    let header = header(bytes)?;
    let damaged = |m: String| DecodeError::Damaged(m);
    let strict = header.as_object().is_some_and(|h| {
        h.len() == 4
            && ["format", "agent", "cut_id", "counts"]
                .iter()
                .all(|k| h.contains_key(*k))
    });
    if !strict {
        return Err(damaged(
            "the header has other members than format, agent, cut_id and counts".into(),
        ));
    }
    let agent = header["agent"]
        .as_str()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| damaged("the header names no agent".into()))?
        .to_owned();
    let cut_id = header["cut_id"]
        .as_str()
        .ok_or_else(|| damaged("the header names no cut".into()))?
        .to_owned();
    let counts: BTreeMap<String, u64> = serde_json::from_value(header["counts"].clone())
        .map_err(|e| damaged(format!("the header's counts: {e}")))?;
    let mut records = Vec::new();
    for body in frames(bytes)?.into_iter().skip(1) {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| damaged(format!("a record is not JSON: {e}")))?;
        let Value::Object(record) = value else {
            return Err(damaged("a record is not an object".into()));
        };
        check(&record).map_err(damaged)?;
        records.push(record);
    }
    let staged = Staged {
        agent,
        cut_id,
        counts,
        records,
    };
    for (kind, expected) in &staged.counts {
        if kind == "queued_event" {
            continue;
        }
        let found = staged.of_kind(kind).count() as u64;
        if found != *expected {
            return Err(damaged(format!(
                "the header counts {expected} {kind} records; the stream holds {found}"
            )));
        }
    }
    if staged.records.iter().any(|r| {
        !staged
            .counts
            .contains_key(r["kind"].as_str().unwrap_or_default())
    }) {
        return Err(damaged(
            "the stream holds records its header doesn't count".into(),
        ));
    }
    Ok(staged)
}

fn code_hash_matches(record: &Map<String, Value>, what: &str) -> std::result::Result<(), String> {
    let script = record["script"].as_str().unwrap_or_default();
    let manifest = record["manifest"].as_str().unwrap_or_default();
    let carried = record["code_hash"].as_str().unwrap_or_default();
    if hex(&crate::ids::code_hash(script, manifest)) != carried {
        return Err(format!(
            "{what}'s code hash does not match its script and manifest"
        ));
    }
    Ok(())
}

/// Checks one record: its kind, its members and types, and its code hashes.
fn check(record: &Map<String, Value>) -> std::result::Result<(), String> {
    let kind = record
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("a record has no kind")?;
    match kind {
        "flow" => {
            values(
                record,
                FLOW,
                &[
                    "kind",
                    "versions",
                    "policies",
                    "schedule",
                    "package_code_hash",
                ],
                "flow",
            )?;
            for key in ["versions", "policies", "schedule", "package_code_hash"] {
                if !record.contains_key(key) {
                    return Err(format!("flow record has no {key:?}"));
                }
            }
            for version in record["versions"]
                .as_array()
                .ok_or("flow.versions is not a list")?
            {
                let version = version.as_object().ok_or("a version is not an object")?;
                values(version, VERSION, &[], "version")?;
                code_hash_matches(version, "a version")?;
            }
            for policy in record["policies"]
                .as_array()
                .ok_or("flow.policies is not a list")?
            {
                values(
                    policy.as_object().ok_or("a policy is not an object")?,
                    POLICY,
                    &[],
                    "policy",
                )?;
            }
            match &record["schedule"] {
                Value::Null => {}
                Value::Object(schedule) => {
                    values(schedule, SCHEDULE, &[], "schedule")?;
                }
                _ => return Err("flow.schedule is neither null nor an object".into()),
            }
            match &record["package_code_hash"] {
                Value::Null => {}
                Value::String(h) if h.len() == 64 && unhex(h).is_some() => {}
                _ => return Err("flow.package_code_hash is not a code hash".into()),
            }
        }
        "instance" => {
            values(record, INSTANCE, &["kind"], kind)?;
        }
        "generation" => {
            values(record, GENERATION, &["kind"], kind)?;
        }
        "kv" => {
            values(record, KV, &["kind"], kind)?;
        }
        "snapshot" => {
            values(record, SNAPSHOT, &["kind"], kind)?;
            code_hash_matches(record, "a run snapshot")?;
        }
        "history" => {
            values(record, HISTORY, &["kind", "code_hash", "calls"], kind)?;
            match record.get("code_hash") {
                Some(Value::String(h)) if h.len() == 64 && unhex(h).is_some() => {}
                _ => return Err("history.code_hash is not a code hash".into()),
            }
            for call in record
                .get("calls")
                .and_then(Value::as_array)
                .ok_or("history.calls is not a list")?
            {
                values(
                    call.as_object().ok_or("a call is not an object")?,
                    CALL,
                    &[],
                    "call",
                )?;
            }
        }
        other => return Err(format!("a record has an unknown kind {other:?}")),
    }
    Ok(())
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
