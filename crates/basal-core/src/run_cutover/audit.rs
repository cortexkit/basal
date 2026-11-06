//! Before/after evidence for the cutover, kept in `meta.migration_audit` so a
//! resumed migration can still compare against the pre-migration store.
//!
//! "Late work" here means a call whose outcome can still arrive or change after
//! its run ended: an unsettled journal position, an unacknowledged Broca call,
//! a reserved token or dollar row, a queued refusal notice, or a quarantined
//! cancellation receipt. Each such source is listed exactly, and the post-check
//! proves every one survives as an open obligation or a settled receipt.
use super::legacy::{self, Row, corrupt, integer, text, unsigned};
use crate::{
    Result,
    run_log::RecoveredRecord,
    run_records::{Body, Record},
    run_store_sql as sql,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const TABLES: &[&str] = &[
    "trigger_inbox",
    "tombstones",
    "rate_windows",
    "schedules",
    "schedule_fires",
    "token_windows",
    "token_ledger",
    "classify_costs",
    "broca_calls",
    "broca_waits",
    "broca_wakes",
];
const SOURCES: [(&str, &str); 6] = [
    (
        "unsettled_positions",
        "SELECT j.* FROM journal_with_payloads j JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND j.settlement IS NULL AND NOT EXISTS(SELECT 1 FROM mailbox m WHERE m.run_id=j.run_id AND m.position=j.position) AND NOT EXISTS(SELECT 1 FROM quarantine q WHERE q.run_id=j.run_id AND q.position=j.position AND q.reason='run_cancelled') ORDER BY j.run_id,j.position",
    ),
    (
        "unacknowledged_broca_calls",
        "SELECT b.* FROM broca_calls b JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND json_extract(b.snapshot,'$.acknowledged')=0 ORDER BY b.run_id,b.position",
    ),
    (
        "reserved_token_rows",
        "SELECT t.* FROM token_ledger t JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND t.state='reserved' ORDER BY t.run_id,t.position,t.send_id",
    ),
    (
        "reserved_dollar_rows",
        "SELECT t.* FROM classify_costs t JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND t.kind='flow' AND t.charged IS NULL ORDER BY t.run_id,t.position",
    ),
    (
        "refusal_outbox_rows",
        "SELECT o.*,json_extract(o.body,'$.run_id') run_id,json_extract(o.body,'$.position') position FROM outbox o JOIN runs r ON r.run_id=CAST(json_extract(o.body,'$.run_id') AS TEXT) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND o.kind='refusal_committed' ORDER BY run_id,position,o.seq",
    ),
    (
        "run_cancelled_quarantine_receipts",
        "SELECT q.* FROM quarantine q JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') AND q.reason='run_cancelled' ORDER BY q.run_id,q.position,q.seq",
    ),
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Digest {
    rows: u64,
    blake3: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Late {
    run_id: String,
    position: u64,
    established: bool,
    receipt: Option<Row>,
    sources: BTreeMap<String, Vec<Row>>,
}
#[derive(Serialize, Deserialize)]
struct Audit {
    version: u32,
    tables: BTreeMap<String, Digest>,
    finished: Digest,
    live: Digest,
    counters: Digest,
    calls: Digest,
    receipts: Digest,
    late: Vec<Late>,
    tombstones_after: Option<Digest>,
    journal_meta_keys_before: u64,
}

struct HashRows {
    hash: blake3::Hasher,
    rows: u64,
}
impl HashRows {
    fn new() -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"basal-cutover-row-v1\0");
        Self { hash, rows: 0 }
    }
    fn add(&mut self, row: &Row) -> Result<()> {
        let bytes = serde_json::to_vec(row).map_err(|e| corrupt(e.to_string()))?;
        self.hash.update(&(bytes.len() as u64).to_le_bytes());
        self.hash.update(&bytes);
        self.rows += 1;
        Ok(())
    }
    fn finish(self) -> Digest {
        Digest {
            rows: self.rows,
            blake3: self.hash.finalize().to_hex().to_string(),
        }
    }
}
fn digest(c: &Connection, query: &str) -> Result<Digest> {
    let mut hash = HashRows::new();
    legacy::visit(c, query, [], |r| hash.add(&r))?;
    Ok(hash.finish())
}
fn table(c: &Connection, name: &str) -> Result<Digest> {
    let mut keys: Vec<(i64, String)> = Vec::new();
    legacy::visit(c, &format!("PRAGMA table_info(\"{name}\")"), [], |r| {
        let pk = integer(&r, "pk")?;
        if pk > 0 {
            keys.push((pk, text(&r, "name")?.to_owned()));
        }
        Ok(())
    })?;
    keys.sort();
    if keys.is_empty() {
        return Err(corrupt(format!("no primary key for {name}")));
    }
    digest(
        c,
        &format!(
            "SELECT * FROM \"{name}\" ORDER BY {}",
            keys.into_iter()
                .map(|(_, s)| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}
fn load(c: &Connection) -> Result<Audit> {
    let raw: String = c.query_row(
        "SELECT value FROM meta WHERE key='migration_audit'",
        [],
        |r| r.get(0),
    )?;
    let audit: Audit = serde_json::from_str(&raw).map_err(|e| corrupt(e.to_string()))?;
    if audit.version != 2 {
        return Err(corrupt("unsupported audit version"));
    }
    Ok(audit)
}
fn save(tx: &Transaction<'_>, audit: &Audit) -> Result<()> {
    let value = serde_json::to_string(audit).map_err(|e| corrupt(e.to_string()))?;
    tx.execute("INSERT INTO meta(key,value) VALUES('migration_audit',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[value])?;
    Ok(())
}
fn require(name: &str, expected: &Digest, actual: &Digest) -> Result<()> {
    if expected != actual {
        return Err(corrupt(format!(
            "post-check {name}: expected {expected:?}, got {actual:?}"
        )));
    }
    Ok(())
}

pub(super) type SourceDetails = BTreeMap<String, Vec<Row>>;
pub(super) type SourceIndex = BTreeMap<(String, u64), SourceDetails>;
pub(super) fn source_details(c: &Connection) -> Result<SourceIndex> {
    let audit = load(c)?;
    Ok(audit
        .late
        .into_iter()
        .map(|l| ((l.run_id, l.position), l.sources))
        .collect())
}

fn receipt_row(
    c: &Connection,
    id: &str,
    position: u64,
    hash: [u8; 32],
    handle: Option<String>,
    ended: i64,
) -> Result<Row> {
    let at: Option<i64> = c
        .query_row(
            "SELECT at FROM call_audit WHERE run_id=?1 AND position=?2",
            params![id, position],
            |r| r.get(0),
        )
        .optional()?;
    let admitted: i64 = c.query_row("SELECT admitted_at FROM runs WHERE run_id=?1", [id], |r| {
        r.get(0)
    })?;
    serde_json::from_value(json!({"run_id":id,"position":position,"value_hash":hash,"handle":handle,"audit_at":at.unwrap_or(admitted),"settled_at":ended})).map_err(|e|corrupt(e.to_string()))
}
fn expected_receipts(c: &Connection, late: &[Late]) -> Result<Digest> {
    let late: BTreeSet<_> = late
        .iter()
        .map(|l| (l.run_id.as_str(), l.position))
        .collect();
    let mut hash = HashRows::new();
    legacy::visit(
        c,
        "SELECT j.* FROM journal_with_payloads j JOIN runs r USING(run_id) WHERE r.state IN ('succeeded','failed','engine_mismatch','cancelled') ORDER BY j.run_id,j.position",
        [],
        |row| {
            let id = text(&row, "run_id")?.to_owned();
            let position = unsigned(&row, "position")?;
            if !(unsigned(&row, "attempts")? > 0
                && !matches!(text(&row, "class")?, "sync" | "local"))
                && !late.contains(&(id.as_str(), position))
            {
                return Ok(());
            }
            if let Some(outcome) = legacy::established(c, &row)? {
                let ended: i64 =
                    c.query_row("SELECT ended_at FROM runs WHERE run_id=?1", [&id], |r| {
                        r.get(0)
                    })?;
                hash.add(&receipt_row(
                    c,
                    &id,
                    position,
                    outcome.hash,
                    outcome.handle,
                    ended,
                )?)?;
            }
            Ok(())
        },
    )?;
    Ok(hash.finish())
}
fn actual_receipts(c: &Connection) -> Result<Digest> {
    let mut hash = HashRows::new();
    legacy::visit(
        c,
        "SELECT run_id,receipts FROM run_history ORDER BY run_id",
        [],
        |row| {
            let id = text(&row, "run_id")?;
            let receipts: BTreeMap<u64, Value> = serde_json::from_str(text(&row, "receipts")?)
                .map_err(|e| corrupt(e.to_string()))?;
            for (position, r) in receipts {
                if r.get("detail").is_some() {
                    return Err(corrupt("settled receipt retains call detail"));
                }
                let actual:Row=serde_json::from_value(json!({"run_id":id,"position":position,"value_hash":r["value_hash"],"handle":r["handle"],"audit_at":r["audit_at"],"settled_at":r["settled_at"]})).map_err(|e|corrupt(e.to_string()))?;
                hash.add(&actual)?;
            }
            Ok(())
        },
    )?;
    Ok(hash.finish())
}

pub(super) fn journal_meta_keys(c: &Connection) -> Result<u64> {
    Ok(c.query_row(
        "SELECT COUNT(*) FROM meta WHERE key GLOB 'journal_calls:*' OR key GLOB 'journal_bytes:*'",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn capture(tx: &Transaction<'_>) -> Result<()> {
    let invalid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs r LEFT JOIN run_snapshots s USING(snapshot_id) WHERE s.snapshot_id IS NULL OR r.flow_id<>s.flow_id OR COALESCE(r.flow_version,0)<>s.flow_version OR r.code_hash<>s.code_hash)",[],|r|r.get(0))?;
    if invalid {
        return Err(corrupt("immutable snapshot identity mismatch"));
    }
    let mut tables = BTreeMap::new();
    for name in TABLES {
        tables.insert((*name).into(), table(tx, name)?);
    }
    let mut positions: BTreeMap<(String, u64), Late> = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for (source, query) in SOURCES {
        let mut count = 0usize;
        legacy::visit(tx, query, [], |row| {
            let id = text(&row, "run_id")?.to_owned();
            let position = unsigned(&row, "position")?;
            let entry = positions
                .entry((id.clone(), position))
                .or_insert_with(|| Late {
                    run_id: id,
                    position,
                    established: false,
                    receipt: None,
                    sources: BTreeMap::new(),
                });
            entry.sources.entry(source.into()).or_default().push(row);
            count += 1;
            Ok(())
        })?;
        counts.insert(source, count);
    }
    for late in positions.values_mut() {
        let row = legacy::calls(tx, &late.run_id)?
            .into_iter()
            .find(|r| r.get("position").and_then(Value::as_u64) == Some(late.position))
            .ok_or_else(|| {
                corrupt(format!(
                    "late work without journal identity {}:{}",
                    late.run_id, late.position
                ))
            })?;
        if let Some(outcome) = legacy::established(tx, &row)? {
            late.established = true;
            let ended = tx.query_row(
                "SELECT ended_at FROM runs WHERE run_id=?1",
                [&late.run_id],
                |r| r.get(0),
            )?;
            late.receipt = Some(receipt_row(
                tx,
                &late.run_id,
                late.position,
                outcome.hash,
                outcome.handle,
                ended,
            )?);
        }
    }
    let late: Vec<_> = positions.into_values().collect();
    let bytes = serde_json::to_vec(&late)
        .map_err(|e| corrupt(e.to_string()))?
        .len();
    if bytes > 1024 * 1024 {
        return Err(corrupt(format!(
            "late-work audit is {bytes} bytes; exceeds 1048576-byte migration bound"
        )));
    }
    tracing::info!(target:"migration",?counts,open_union=late.iter().filter(|l|!l.established).count(),late_audit_bytes=bytes,"captured late-work sources");
    let audit = Audit {
        version: 2,
        tables,
        finished: digest(
            tx,
            "SELECT run_id,flow_id,COALESCE(flow_version,0) flow_version,snapshot_id,code_hash FROM runs WHERE state IN ('succeeded','failed','engine_mismatch','cancelled') ORDER BY run_id",
        )?,
        live: digest(
            tx,
            "SELECT run_id,flow_id,COALESCE(flow_version,0) flow_version,snapshot_id,code_hash FROM runs WHERE state NOT IN ('succeeded','failed','engine_mismatch','cancelled') ORDER BY run_id",
        )?,
        counters: digest(
            tx,
            "SELECT r.run_id,r.broken,(SELECT COUNT(*) FROM activations a WHERE a.run_id=r.run_id) activation_count FROM runs r ORDER BY r.run_id",
        )?,
        calls: digest(
            tx,
            "SELECT r.run_id,(SELECT COUNT(*) FROM journal j WHERE j.run_id=r.run_id) calls FROM runs r WHERE r.state IN ('failed','engine_mismatch','cancelled') ORDER BY r.run_id",
        )?,
        receipts: expected_receipts(tx, &late)?,
        late,
        tombstones_after: None,
        journal_meta_keys_before: journal_meta_keys(tx)?,
    };
    save(tx, &audit)
}

pub(super) fn check_shared(c: &Connection) -> Result<()> {
    let audit = load(c)?;
    for name in TABLES {
        require(name, &audit.tables[*name], &table(c, name)?)?;
    }
    Ok(())
}
pub(super) fn after_finished(tx: &Transaction<'_>) -> Result<()> {
    let mut audit = load(tx)?;
    audit.tombstones_after = Some(table(tx, "tombstones")?);
    save(tx, &audit)
}
pub(super) fn check_broca(c: &Connection) -> Result<()> {
    let audit = load(c)?;
    for name in ["broca_calls", "broca_waits", "broca_wakes"] {
        require(name, &audit.tables[name], &table(c, name)?)?;
    }
    Ok(())
}

pub(super) fn check(c: &Connection, frames: &[RecoveredRecord<Record>]) -> Result<()> {
    let audit = load(c)?;
    let journal_meta_keys_after = journal_meta_keys(c)?;
    if journal_meta_keys_after != 0 {
        return Err(corrupt(format!(
            "legacy journal meta keys: expected zero after dropping {} keys, got {journal_meta_keys_after}",
            audit.journal_meta_keys_before
        )));
    }
    for name in TABLES {
        let expected = if *name == "tombstones" {
            audit
                .tombstones_after
                .as_ref()
                .ok_or_else(|| corrupt("missing finished-run tombstone audit"))?
        } else {
            &audit.tables[*name]
        };
        require(name, expected, &table(c, name)?)?;
    }
    require(
        "finished identities",
        &audit.finished,
        &digest(
            c,
            "SELECT i.run_id,i.flow_id,i.flow_version,i.snapshot_id,i.code_hash FROM run_identities i JOIN run_history h USING(run_id) ORDER BY i.run_id",
        )?,
    )?;
    require(
        "live identities",
        &audit.live,
        &digest(
            c,
            "SELECT i.run_id,i.flow_id,i.flow_version,i.snapshot_id,i.code_hash FROM run_identities i JOIN live_runs l USING(run_id) ORDER BY i.run_id",
        )?,
    )?;
    let mut counters = legacy::rows(
        c,
        "SELECT run_id,broken,activation_count FROM run_history ORDER BY run_id",
        [],
    )?;
    let mut snapshots = Vec::new();
    for frame in frames {
        if let Body::Snapshot { state, .. } = &frame.record.body {
            if frame.lsn.sequence != 1 {
                return Err(corrupt("converted snapshot is not in first segment"));
            }
            snapshots.push(frame.record.run_id.clone());
            counters.push(serde_json::from_value(json!({"run_id":frame.record.run_id,"broken":state.broken,"activation_count":state.activation_count})).map_err(|e|corrupt(e.to_string()))?);
        }
    }
    let live_ids: Vec<String> =
        legacy::rows(c, "SELECT run_id FROM live_runs ORDER BY run_id", [])?
            .iter()
            .map(|r| text(r, "run_id").map(str::to_owned))
            .collect::<Result<_>>()?;
    snapshots.sort();
    if snapshots != live_ids {
        return Err(corrupt(
            "first-segment snapshot/live run inventory mismatch",
        ));
    }
    counters.sort_by(|a, b| a["run_id"].as_str().cmp(&b["run_id"].as_str()));
    let mut hash = HashRows::new();
    for row in counters {
        hash.add(&row)?;
    }
    require("broken/activation_count", &audit.counters, &hash.finish())?;
    require(
        "run_calls",
        &audit.calls,
        &digest(
            c,
            "SELECT h.run_id,(SELECT COUNT(*) FROM run_calls j WHERE j.run_id=h.run_id) calls FROM run_history h WHERE h.state IN ('failed','engine_mismatch','cancelled') ORDER BY h.run_id",
        )?,
    )?;
    require("settled receipts", &audit.receipts, &actual_receipts(c)?)?;
    let actual: BTreeSet<(String, u64)> = legacy::rows(
        c,
        "SELECT run_id,position FROM obligations ORDER BY run_id,position",
        [],
    )?
    .iter()
    .map(|r| Ok((text(r, "run_id")?.into(), unsigned(r, "position")?)))
    .collect::<Result<_>>()?;
    let expected: BTreeSet<_> = audit
        .late
        .iter()
        .filter(|l| !l.established)
        .map(|l| (l.run_id.clone(), l.position))
        .collect();
    if actual != expected {
        return Err(corrupt(format!(
            "open obligations: expected {} source-union positions, got {}",
            expected.len(),
            actual.len()
        )));
    }
    // Settled calls deliberately lose their payload detail. Compare every
    // surviving late-work source against the transient audit rather than
    // retaining a second copy of those payloads in each permanent receipt.
    for (source, query) in SOURCES
        .iter()
        .filter(|(name, _)| *name != "unsettled_positions")
    {
        let expected: Vec<_> = audit
            .late
            .iter()
            .flat_map(|l| l.sources.get(*source).into_iter().flatten().cloned())
            .collect();
        let actual = legacy::rows(c, &query.replace("JOIN runs r", "JOIN run_history r"), [])?;
        if actual != expected {
            return Err(corrupt(format!("late-work source detail lost: {source}")));
        }
    }
    for late in &audit.late {
        let obligation = sql::obligation(c, &late.run_id, late.position)?
            .ok_or_else(|| corrupt("late-work source not covered by obligation/receipt"))?;
        if let Some(expected) = &late.receipt {
            let actual:Row=serde_json::from_value(json!({"run_id":late.run_id,"position":late.position,"value_hash":match obligation.receipt {Some(sql::Receipt::Value(hash))=>Some(hash),_=>None},"handle":obligation.handle,"audit_at":obligation.audit_at,"settled_at":obligation.settled_at})).map_err(|e|corrupt(e.to_string()))?;
            if obligation.state != sql::ObligationState::Settled
                || &actual != expected
                || !obligation.detail.is_null()
            {
                return Err(corrupt("settled late-work source receipt mismatch"));
            }
            continue;
        }
        let sources: BTreeMap<String, Vec<Row>> =
            serde_json::from_value(obligation.detail["migration_sources"].clone())
                .map_err(|e| corrupt(e.to_string()))?;
        if sources != late.sources {
            return Err(corrupt(format!(
                "late-work source detail lost {}:{}",
                late.run_id, late.position
            )));
        }
    }
    super::check_broca_ownership(c)?;
    legacy::visit(
        c,
        "SELECT run_id,position,snapshot FROM broca_calls WHERE json_extract(snapshot,'$.acknowledged')=0 ORDER BY run_id,position",
        [],
        |row| {
            let id = text(&row, "run_id")?;
            let live: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_runs WHERE run_id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if !live && sql::obligation(c, id, unsigned(&row, "position")?)?.is_none() {
                return Err(corrupt(format!(
                    "Broca row without obligation or receipt {id}"
                )));
            }
            Ok(())
        },
    )?;
    drop_dependencies(c)
}

pub(super) fn drop_dependencies(c: &Connection) -> Result<()> {
    let dropped = [
        "runs",
        "journal",
        "journal_payloads",
        "mailbox",
        "call_audit",
        "activations",
        "event_body_journal",
        "runs_with_snapshots",
        "journal_with_payloads",
    ];
    for row in legacy::rows(
        c,
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        [],
    )? {
        let name = text(&row, "name")?;
        if dropped.contains(&name) {
            continue;
        }
        for fk in legacy::rows(
            c,
            &format!("PRAGMA foreign_key_list(\"{}\")", name.replace('"', "\"\"")),
            [],
        )? {
            let target = text(&fk, "table")?;
            if dropped.contains(&target) {
                return Err(corrupt(format!(
                    "retained table {name} references dropped table {target}"
                )));
            }
        }
    }
    for row in legacy::rows(
        c,
        "SELECT name,sql FROM sqlite_schema WHERE type IN ('view','trigger') ORDER BY name",
        [],
    )? {
        let name = text(&row, "name")?;
        if dropped.contains(&name) {
            continue;
        }
        let words = sql_identifiers(text(&row, "sql")?);
        if let Some(table) = words.iter().find(|w| dropped.contains(&w.as_str())) {
            return Err(corrupt(format!(
                "retained sqlite_schema {name} references {table}"
            )));
        }
    }
    Ok(())
}

fn sql_identifiers(sql: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prior = ' ';
            for c in chars.by_ref() {
                if prior == '*' && c == '/' {
                    break;
                }
                prior = c;
            }
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            word.push(ch.to_ascii_lowercase());
        } else if !word.is_empty() {
            result.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        result.push(word);
    }
    result
}
