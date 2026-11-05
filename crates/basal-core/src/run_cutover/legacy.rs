use crate::{CoreError, Result, Store};
use rusqlite::{Connection, Params, types::ValueRef};
use serde_json::{Map, Value};

pub(super) type Row = Map<String, Value>;
pub(super) fn corrupt(message: impl Into<String>) -> CoreError {
    CoreError::Corrupt(format!("run-log migration: {}", message.into()))
}

pub(super) fn visit(
    c: &Connection,
    query: &str,
    params: impl Params,
    mut f: impl FnMut(Row) -> Result<()>,
) -> Result<()> {
    let mut statement = c.prepare(query)?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut cursor = statement.query(params)?;
    while let Some(row) = cursor.next()? {
        let mut value = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let v = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => n.into(),
                ValueRef::Real(n) => serde_json::Number::from_f64(n)
                    .ok_or_else(|| corrupt("non-finite SQL number"))?
                    .into(),
                ValueRef::Text(s) => Value::String(
                    std::str::from_utf8(s)
                        .map_err(|_| corrupt("invalid UTF-8 SQL text"))?
                        .to_owned(),
                ),
                ValueRef::Blob(bytes) => serde_json::json!({"blob":bytes}),
            };
            value.insert(name.clone(), v);
        }
        f(value)?;
    }
    Ok(())
}
pub(super) fn rows(c: &Connection, query: &str, params: impl Params) -> Result<Vec<Row>> {
    let mut result = Vec::new();
    visit(c, query, params, |row| {
        result.push(row);
        Ok(())
    })?;
    Ok(result)
}
pub(super) fn text<'a>(row: &'a Row, key: &str) -> Result<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt(format!("missing text {key}")))
}
pub(super) fn optional_text(row: &Row, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_owned)
}
pub(super) fn integer(row: &Row, key: &str) -> Result<i64> {
    row.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| corrupt(format!("missing integer {key}")))
}
pub(super) fn unsigned(row: &Row, key: &str) -> Result<u64> {
    u64::try_from(integer(row, key)?).map_err(|_| corrupt(format!("negative {key}")))
}
pub(super) fn blob32(row: &Row, key: &str) -> Result<[u8; 32]> {
    let bytes: Vec<u8> = serde_json::from_value(
        row.get(key)
            .and_then(|v| v.get("blob"))
            .cloned()
            .ok_or_else(|| corrupt(format!("missing blob {key}")))?,
    )
    .map_err(|e| corrupt(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| corrupt(format!("invalid digest {key}")))
}
pub(super) fn json(row: &Row, key: &str) -> Result<Value> {
    serde_json::from_str(text(row, key)?).map_err(|e| corrupt(format!("{key}: {e}")))
}
pub(super) fn run_ids(c: &Connection, terminal: bool) -> Result<Vec<String>> {
    let predicate = if terminal { "IN" } else { "NOT IN" };
    let query = format!(
        "SELECT run_id FROM runs WHERE state {predicate} ('succeeded','failed','engine_mismatch','cancelled') ORDER BY run_id"
    );
    rows(c, &query, [])?
        .iter()
        .map(|r| text(r, "run_id").map(str::to_owned))
        .collect()
}
pub(super) fn run(c: &Connection, id: &str) -> Result<Row> {
    rows(c,"SELECT r.*,r.rowid legacy_rowid,s.script,s.manifest,s.flow_version snapshot_version,s.code_hash snapshot_hash FROM runs r JOIN run_snapshots s USING(snapshot_id) WHERE r.run_id=?1",[id])?.pop().ok_or_else(||corrupt(format!("missing run/snapshot {id}")))
}
pub(super) fn calls(c: &Connection, id: &str) -> Result<Vec<Row>> {
    rows(
        c,
        "SELECT * FROM journal_with_payloads WHERE run_id=?1 ORDER BY position",
        [id],
    )
}

/// Old untracked fs.write temporaries must be swept before their journal is
/// removed. Each deletion validates the stored path against its original roots;
/// retrying after a crash can only revisit files with the legacy temp-name shape.
pub(super) fn sweep(store: &Store) -> Result<()> {
    let writes=store.read(|c|rows(c,"SELECT DISTINCT json_extract(request,'$.args.path') path,json_extract(request,'$.grant.roots') roots FROM journal_with_payloads WHERE kind_code=?1 AND request IS NOT NULL AND json_valid(request)",[i64::from(basal_proto::Primitive::FsWrite.code())]))?;
    for row in writes {
        if let (Some(path), Some(roots)) =
            (optional_text(&row, "path"), optional_text(&row, "roots"))
            && let Ok(roots) = serde_json::from_str::<Vec<String>>(&roots)
        {
            let _ = basal_host::builtins::fs::remove_legacy_temps(&path, &roots);
        }
    }
    store.write(|tx| {
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('legacy_fs_temps_swept','1')",
            [],
        )?;
        Ok(())
    })
}

pub(super) struct Established {
    pub hash: [u8; 32],
    pub handle: Option<String>,
    pub settlement: String,
    pub value: String,
    pub order: Option<u64>,
    pub arrival: u64,
}
pub(super) fn established(c: &Connection, call: &Row) -> Result<Option<Established>> {
    let id = text(call, "run_id")?;
    let position = unsigned(call, "position")?;
    if let Some(settlement) = optional_text(call, "settlement") {
        return Ok(Some(Established {
            hash: blob32(call, "payload_hash")?,
            handle: optional_text(call, "handle"),
            settlement,
            value: optional_text(call, "value").unwrap_or_else(|| "null".into()),
            order: call.get("delivery_order").and_then(Value::as_u64),
            arrival: call
                .get("delivery_order")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }));
    }
    if let Some(row) = rows(
        c,
        "SELECT * FROM mailbox WHERE run_id=?1 AND position=?2",
        rusqlite::params![id, position],
    )?
    .pop()
    {
        return Ok(Some(Established {
            hash: blob32(&row, "payload_hash")?,
            handle: optional_text(&row, "handle").or_else(|| optional_text(call, "handle")),
            settlement: text(&row, "settlement")?.into(),
            value: text(&row, "value")?.into(),
            order: None,
            arrival: unsigned(&row, "seq")?,
        }));
    }
    let receipts = rows(
        c,
        "SELECT * FROM quarantine WHERE run_id=?1 AND position=?2 AND reason='run_cancelled' ORDER BY seq",
        rusqlite::params![id, position],
    )?;
    if let Some(first) = receipts.first() {
        let hash = blob32(first, "payload_hash")?;
        for row in &receipts {
            if blob32(row, "payload_hash")? != hash {
                return Err(corrupt(format!(
                    "contradictory cancelled receipts {id}:{position}"
                )));
            }
        }
        return Ok(Some(Established {
            hash,
            handle: optional_text(first, "handle").or_else(|| optional_text(call, "handle")),
            settlement: text(first, "settlement")?.into(),
            value: text(first, "value")?.into(),
            order: None,
            arrival: unsigned(first, "seq")?,
        }));
    }
    Ok(None)
}
