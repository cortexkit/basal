//! Moving an agent's flows to another computer: basal's store side of
//! `agent-data/v1`, the contract Entorhinal drives when an agent moves.
//!
//! An agent's data here is every flow that belongs to it (its owner, or
//! before any approval the single author of its versions), with their
//! versions, schedules, instances, `kv` state and run history. One move
//! goes through these steps on each side:
//!
//! - **Source.** `prepare` freezes the agent ([`freeze`]): its flows admit
//!   nothing, schedule fires wait and events wait in the backlog. The flows
//!   are encoded ([`encode`]) into a `basal-flows/1` stream (see [`stream`]),
//!   which the module ships through Engram's door. `abort` lifts the freeze
//!   ([`lift`]); `release` turns it into a tombstone ([`release`]).
//! - **Receiver.** `import` decodes and checks the stream into private
//!   staging and writes nothing here. `activate` writes it in one
//!   transaction ([`activate`]), disabled with `disabled_by = 'move'` where
//!   the flow was approved on the source; each such flow waits in
//!   `agent_move_pending` until this computer's prefrontal reports the same
//!   code active, and is then put back in the source's own enable state
//!   ([`lift_move`]).
//!
//! The per-cut checkpoint, the first reply to each state-changing request
//! and the holds are store rows, so every step survives a restart and a
//! repeated request is answered the same way.

pub mod stream;

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{CoreError, Result};
pub use stream::{DecodeError, FORMAT, STREAM_CAP, Staged};

/// How `flows.disabled_by` records a flow that moved in and waits for this
/// computer's approval, and how `removed_by` begins on a tombstoned flow.
pub const MOVE_ACTOR: &str = "move";
const MOVED_PREFIX: &str = "moved:";
/// `removed_by` of a local flow of the agent that an incoming copy replaced
/// without carrying it: the flow is gone, but not because the agent moved
/// away.
pub const REPLACED: &str = "moved:replaced";

/// A SQL condition on `flow_id`: the flow's agent holds no freeze or
/// tombstone.
pub const NOT_HELD: &str = "flow_id NOT IN (SELECT flow_id FROM agent_move_held_flows)";

/// The flows that belong to an agent (`?1`), tombstoned ones included.
const AGENT_FLOWS: &str = "SELECT flow_id FROM agent_move_flow_agents WHERE agent_id=?1";

/// The agent's flows that are not a tombstone of an earlier move away.
const LIVE_FLOWS: &str = "SELECT a.flow_id FROM agent_move_flow_agents a JOIN flows f USING(flow_id) \
     WHERE a.agent_id=?1 AND COALESCE(f.removed_by,'') NOT LIKE 'moved:%'";

/// Whether an agent's flows may run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// A cut is being taken (`prepare` until `release` or `abort`).
    Frozen { cut_id: String },
    /// The flows live on another computer, `holder`.
    Moved { cut_id: String, holder: String },
}

pub fn hold(conn: &Connection, agent: &str) -> Result<Option<Hold>> {
    let row: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT kind,cut_id,holder FROM agent_move_holds WHERE agent_id=?1",
            [agent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(
        row.map(|(kind, cut_id, holder)| match (kind.as_str(), holder) {
            ("moved", Some(holder)) => Hold::Moved { cut_id, holder },
            _ => Hold::Frozen { cut_id },
        }),
    )
}

/// Whether `flow_id` belongs to an agent that holds a freeze or tombstone.
/// Admission asks this for every trigger, so the common case (no move
/// anywhere) is one read of an empty table.
pub fn flow_held(conn: &Connection, flow_id: &str) -> Result<bool> {
    let any: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM agent_move_holds)", [], |r| {
        r.get(0)
    })?;
    if !any {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_move_held_flows WHERE flow_id=?1)",
        [flow_id],
        |r| r.get(0),
    )?)
}

/// Why an agent's flows can't be cut now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Busy {
    /// `run_active`, `reconcile_open`, `approval_open` for an install card
    /// still waiting for an answer, or `move_in_progress` when another
    /// cut froze the agent and hasn't been released or aborted.
    pub code: &'static str,
    pub reason: String,
}

/// Whether the agent's flows are in use: a live run, an unsettled
/// obligation or open decision card the operator must settle here first, or
/// an install card still waiting for an answer.
pub fn busy(conn: &Connection, agent: &str) -> Result<Option<Busy>> {
    let live: Option<String> = conn
        .query_row(
            &format!("SELECT flow_id FROM live_runs WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id LIMIT 1"),
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(flow) = live {
        return Ok(Some(Busy {
            code: "run_active",
            reason: format!("flow {flow} has a run in progress"),
        }));
    }
    let open: Option<String> = conn
        .query_row(
            &format!(
                "SELECT flow_id FROM (
                    SELECT r.flow_id FROM obligations o JOIN run_history r USING(run_id) WHERE o.state<>'settled'
                    UNION SELECT flow_id FROM decision_cards WHERE state='open')
                 WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id LIMIT 1"
            ),
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    Ok(open.map(|flow| Busy {
        code: "reconcile_open",
        reason: format!(
            "flow {flow} has a call outcome or decision the operator must settle on this computer first"
        ),
    })
    .or(waiting_card(conn, agent)?))
}

/// An install card still waiting for an answer: its version's approval
/// belongs to this computer's prefrontal and can't follow the move, so the
/// version would be stranded on the receiver.
fn waiting_card(conn: &Connection, agent: &str) -> Result<Option<Busy>> {
    let card: Option<(String, u32)> = conn
        .query_row(
            &format!(
                "SELECT flow_id,version FROM install_cards WHERE state='pending' \
                 AND flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id,version LIMIT 1"
            ),
            [agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(card.map(|(flow, version)| Busy {
        code: "approval_open",
        reason: format!(
            "flow {flow} version {version} waits for an install card's answer on this computer"
        ),
    }))
}

/// One cut's checkpoint on this computer.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub cut_id: String,
    pub agent: Option<String>,
    /// `source`, `receiver`, or `unknown` for an abort of a cut never seen.
    pub side: String,
    pub step: String,
    /// The refs (`{ref_kind, ref}` pairs naming the agent being moved)
    /// that the cut's first op named, in that order; later ops on the cut
    /// must name the same set.
    pub refs: Value,
    pub session_ref: Option<String>,
    pub end_offset: Option<u64>,
    pub digest: Option<String>,
    pub fingerprint: Option<String>,
    pub dropped: Option<Value>,
}

pub fn cut(conn: &Connection, cut_id: &str) -> Result<Option<Cut>> {
    conn.query_row(
        "SELECT cut_id,agent_id,side,step,refs,session_ref,end_offset,digest,fingerprint,dropped \
         FROM agent_move_cuts WHERE cut_id=?1",
        [cut_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        },
    )
    .optional()?
    .map(
        |(cut_id, agent, side, step, refs, session_ref, end, digest, fingerprint, dropped)| {
            let json = |text: &str| {
                serde_json::from_str(text)
                    .map_err(|e| CoreError::Corrupt(format!("cut {cut_id}: {e}")))
            };
            Ok(Cut {
                refs: json(&refs)?,
                dropped: dropped.as_deref().map(json).transpose()?,
                end_offset: end
                    .map(|n| {
                        u64::try_from(n)
                            .map_err(|_| CoreError::Corrupt(format!("cut {cut_id} offset")))
                    })
                    .transpose()?,
                cut_id: cut_id.clone(),
                agent,
                side,
                step,
                session_ref,
                digest,
                fingerprint,
            })
        },
    )
    .transpose()
}

pub fn save_cut(tx: &Transaction, cut: &Cut, now_ms: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO agent_move_cuts(cut_id,agent_id,side,step,refs,session_ref,end_offset,digest,fingerprint,dropped,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(cut_id) DO UPDATE SET agent_id=excluded.agent_id,side=excluded.side,step=excluded.step,
            refs=excluded.refs,session_ref=excluded.session_ref,end_offset=excluded.end_offset,
            digest=excluded.digest,fingerprint=excluded.fingerprint,dropped=excluded.dropped,updated_at=excluded.updated_at",
        params![
            cut.cut_id,
            cut.agent,
            cut.side,
            cut.step,
            cut.refs.to_string(),
            cut.session_ref,
            cut.end_offset.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
            cut.digest,
            cut.fingerprint,
            cut.dropped.as_ref().map(Value::to_string),
            now_ms
        ],
    )?;
    Ok(())
}

/// The first request and reply recorded for `(cut_id, op)`.
pub fn recorded_reply(conn: &Connection, cut_id: &str, op: &str) -> Result<Option<(Value, Value)>> {
    conn.query_row(
        "SELECT request,reply FROM agent_move_replies WHERE cut_id=?1 AND op=?2",
        params![cut_id, op],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()?
    .map(|(request, reply)| {
        let json = |t: &str| serde_json::from_str(t).map_err(|e| CoreError::Corrupt(e.to_string()));
        Ok((json(&request)?, json(&reply)?))
    })
    .transpose()
}

pub fn record_reply(
    tx: &Transaction,
    cut_id: &str,
    op: &str,
    request: &Value,
    reply: &Value,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO agent_move_replies(cut_id,op,request,reply) VALUES (?1,?2,?3,?4)",
        params![cut_id, op, request.to_string(), reply.to_string()],
    )?;
    Ok(())
}

/// A content fingerprint of the agent's live flows: which flows, versions
/// (by code hash), instances, `kv` revisions and runs there are. It leaves
/// out enable state, which the receiver changes on its own as approvals
/// arrive, so it changes only when the flows gain data a cut doesn't hold.
pub fn fingerprint(conn: &Connection, agent: &str) -> Result<String> {
    let list = |sql: &str| -> Result<Vec<String>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([agent], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    let content = json!({
        "flows": list(&format!("SELECT flow_id FROM ({LIVE_FLOWS}) ORDER BY flow_id"))?,
        "versions": list(&format!("SELECT flow_id||':'||version||':'||lower(hex(code_hash)) FROM installs WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id,version"))?,
        "instances": list(&format!("SELECT flow_id||':'||incarnation||':'||params FROM template_instances WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id"))?,
        "kv": list(&format!("SELECT flow_id||':'||revision||':'||key FROM kv WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY flow_id,key"))?,
        "runs": list(&format!("SELECT run_id FROM run_history WHERE flow_id IN ({LIVE_FLOWS}) ORDER BY run_id"))?,
    });
    Ok(stream::hex(&Sha256::digest(stream::canonical(&content))))
}

/// Whether the agent has live flows here at all.
pub fn has_live_flows(conn: &Connection, agent: &str) -> Result<bool> {
    Ok(
        conn.query_row(&format!("SELECT EXISTS({LIVE_FLOWS})"), [agent], |r| {
            r.get(0)
        })?,
    )
}

/// What this computer holds for the agent, compared with what an incoming
/// cut would bring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Local {
    /// Nothing the cut lacks: no live flow, or live flows that match the
    /// last cut this computer exported or imported for the agent.
    AtCut,
    /// Live flows with data no recorded cut holds.
    Advanced,
}

pub fn local(conn: &Connection, agent: &str) -> Result<Local> {
    if !has_live_flows(conn, agent)? {
        return Ok(Local::AtCut);
    }
    let last: Option<String> = conn
        .query_row(
            "SELECT fingerprint FROM agent_move_cuts WHERE agent_id=?1 AND fingerprint IS NOT NULL \
             AND step IN ('exported','released','activated') ORDER BY updated_at DESC, cut_id DESC LIMIT 1",
            [agent],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(if last == Some(fingerprint(conn, agent)?) {
        Local::AtCut
    } else {
        Local::Advanced
    })
}

/// A flow the stream carries that belongs to someone else here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub flow_id: String,
    /// The other owner, or `None` when no owner or author is recorded.
    pub owner: Option<String>,
}

pub fn collision(conn: &Connection, agent: &str, staged: &Staged) -> Result<Option<Collision>> {
    for flow_id in staged.flow_ids() {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM flows WHERE flow_id=?1)",
            [&flow_id],
            |r| r.get(0),
        )?;
        if !exists {
            continue;
        }
        let owner: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM agent_move_flow_agents WHERE flow_id=?1",
                [&flow_id],
                |r| r.get(0),
            )
            .optional()?;
        if owner.as_deref() != Some(agent) {
            return Ok(Some(Collision { flow_id, owner }));
        }
    }
    Ok(None)
}

/// Freezes the agent for `cut_id`, rechecking in the same transaction that
/// none of its flows is busy. An agent already tombstoned here keeps its
/// tombstone (its flows are stopped already).
pub fn freeze(
    tx: &Transaction,
    agent: &str,
    cut_id: &str,
    now_ms: i64,
) -> Result<std::result::Result<(), Busy>> {
    if let Some(busy) = busy(tx, agent)? {
        return Ok(Err(busy));
    }
    match hold(tx, agent)? {
        Some(Hold::Moved { .. }) => {}
        Some(Hold::Frozen { cut_id: other }) if other != cut_id => {
            return Ok(Err(Busy {
                code: "move_in_progress",
                reason: format!("another move of this agent (cut {other}) has not finished"),
            }));
        }
        _ => {
            tx.execute(
                "INSERT INTO agent_move_holds(agent_id,kind,cut_id,holder,at) VALUES (?1,'frozen',?2,NULL,?3)
                 ON CONFLICT(agent_id) DO UPDATE SET kind='frozen',cut_id=excluded.cut_id,holder=NULL,at=excluded.at",
                params![agent, cut_id, now_ms],
            )?;
        }
    }
    Ok(Ok(()))
}

/// Lifts the freeze `cut_id` set. A freeze of another cut, or a tombstone,
/// stays.
pub fn lift(tx: &Transaction, agent: &str, cut_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM agent_move_holds WHERE agent_id=?1 AND kind='frozen' AND cut_id=?2",
        params![agent, cut_id],
    )?;
    Ok(())
}

/// Encodes the agent's live flows as a `basal-flows/1` stream for
/// `cut_id`. `Err(Ok(message))` means the agent's flows can't be carried
/// (too large); `Err(Err(error))` is a store failure.
pub fn encode(
    conn: &Connection,
    agent: &str,
    cut_id: &str,
) -> Result<std::result::Result<Vec<u8>, String>> {
    use stream::{
        CALL, FLOW, GENERATION, HISTORY, INSTANCE, KV, POLICY, SCHEDULE, SNAPSHOT, VERSION, rows,
    };
    let mut writer = stream::Writer::new();
    let mut counts = BTreeMap::from(
        [
            "flow",
            "instance",
            "generation",
            "kv",
            "snapshot",
            "history",
        ]
        .map(|k| (k.to_owned(), 0)),
    );
    let flows = rows(
        conn,
        FLOW,
        "f.",
        &format!("FROM flows f WHERE f.flow_id IN ({LIVE_FLOWS}) ORDER BY f.flow_id"),
        &[&agent],
    )?;
    let mut snapshots = BTreeSet::new();
    let mut history = Vec::new();
    for mut flow in flows {
        let id = flow["flow_id"].as_str().unwrap_or_default().to_owned();
        let versions = rows(
            conn,
            VERSION,
            "",
            "FROM installs WHERE flow_id=?1 ORDER BY version",
            &[&id],
        )?;
        let policies = rows(
            conn,
            POLICY,
            "",
            "FROM install_tool_policies WHERE flow_id=?1 ORDER BY version,module,op",
            &[&id],
        )?;
        let schedule = rows(
            conn,
            SCHEDULE,
            "",
            "FROM schedules WHERE flow_id=?1",
            &[&id],
        )?;
        let package_hash: Option<Vec<u8>> = conn
            .query_row(
                "SELECT p.code_hash FROM flows f JOIN package_versions p ON p.package=f.package AND p.version=f.approved_version WHERE f.flow_id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?;
        flow.insert("kind".into(), json!("flow"));
        let mut versions = versions;
        let mut schedule = schedule;
        // A flow that moved in and still waits for approval here leaves in
        // the state it arrived with, so the next computer waits for the
        // same approval instead of finding it stopped by this move.
        let waiting: Option<(i64, String, Vec<u8>)> = conn
            .query_row(
                "SELECT version,restore,code_hash FROM agent_move_pending WHERE flow_id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((version, restore, _)) = &waiting {
            let version = *version;
            let restore: Map<String, Value> = serde_json::from_str(restore)
                .map_err(|e| CoreError::Corrupt(format!("move restore of {id}: {e}")))?;
            for key in ["state", "disabled_by", "disabled_reason", "disabled_at"] {
                flow.insert(key.into(), restore.get(key).cloned().unwrap_or(Value::Null));
            }
            flow.insert("approved_version".into(), json!(version));
            for v in versions
                .iter_mut()
                .filter(|v| v["version"].as_i64() == Some(version))
            {
                if v["state"] == "validated" {
                    v.insert("state".into(), json!("approved"));
                    v.insert("approval_ref".into(), json!("basal:moved"));
                }
            }
            if restore.get("state").and_then(Value::as_str) == Some("enabled") {
                for s in schedule.iter_mut() {
                    s.insert("state".into(), json!("active"));
                }
            }
        }
        flow.insert(
            "versions".into(),
            Value::Array(versions.into_iter().map(Value::Object).collect()),
        );
        flow.insert(
            "policies".into(),
            Value::Array(policies.into_iter().map(Value::Object).collect()),
        );
        flow.insert(
            "schedule".into(),
            schedule
                .into_iter()
                .next()
                .map_or(Value::Null, Value::Object),
        );
        let package_hash = match (&waiting, flow["package"].is_string()) {
            // The code the instance waits to see approved, whether or not
            // this computer has its template yet.
            (Some((_, _, waited)), true) => Some(waited.clone()),
            _ => package_hash,
        };
        flow.insert(
            "package_code_hash".into(),
            package_hash.map_or(Value::Null, |h| json!(stream::hex(&h))),
        );
        if let Err(message) = writer.frame(&Value::Object(flow)) {
            return Ok(Err(message));
        }
        for mut instance in rows(
            conn,
            INSTANCE,
            "",
            "FROM template_instances WHERE flow_id=?1",
            &[&id],
        )? {
            instance.insert("kind".into(), json!("instance"));
            if let Err(message) = writer.frame(&Value::Object(instance)) {
                return Ok(Err(message));
            }
        }
        for mut entry in rows(
            conn,
            KV,
            "",
            "FROM kv WHERE flow_id=?1 ORDER BY key",
            &[&id],
        )? {
            entry.insert("kind".into(), json!("kv"));
            if let Err(message) = writer.frame(&Value::Object(entry)) {
                return Ok(Err(message));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT h.run_id, s.flow_version, lower(hex(s.code_hash)) FROM run_history h JOIN run_snapshots s USING(snapshot_id) \
             WHERE h.flow_id=?1 ORDER BY h.admit_seq, h.run_id",
        )?;
        let runs = stmt
            .query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (run_id, version, hash) in runs {
            snapshots.insert((id.clone(), version, hash.clone()));
            history.push((run_id, hash));
        }
    }
    for mut generation in rows(
        conn,
        GENERATION,
        "",
        "FROM instance_generations WHERE agent_id=?1 ORDER BY package",
        &[&agent],
    )? {
        generation.insert("kind".into(), json!("generation"));
        if let Err(message) = writer.frame(&Value::Object(generation)) {
            return Ok(Err(message));
        }
    }
    for (flow_id, version, hash) in snapshots {
        let found = rows(
            conn,
            SNAPSHOT,
            "",
            "FROM run_snapshots WHERE flow_id=?1 AND flow_version=?2 AND lower(hex(code_hash))=?3 LIMIT 1",
            &[&flow_id, &version, &hash],
        )?;
        for mut snapshot in found {
            snapshot.insert("kind".into(), json!("snapshot"));
            if let Err(message) = writer.frame(&Value::Object(snapshot)) {
                return Ok(Err(message));
            }
        }
    }
    for (run_id, hash) in history {
        for mut run in rows(
            conn,
            HISTORY,
            "",
            "FROM run_history WHERE run_id=?1",
            &[&run_id],
        )? {
            let calls = rows(
                conn,
                CALL,
                "",
                "FROM run_calls WHERE run_id=?1 ORDER BY position",
                &[&run_id],
            )?;
            run.insert("kind".into(), json!("history"));
            run.insert("code_hash".into(), json!(hash));
            run.insert(
                "calls".into(),
                Value::Array(calls.into_iter().map(Value::Object).collect()),
            );
            if let Err(message) = writer.frame(&Value::Object(run)) {
                return Ok(Err(message));
            }
        }
    }
    counts.insert("queued_event".into(), queued_events(conn, agent)?);
    Ok(writer.finish(agent, cut_id, counts))
}

/// Events waiting in the backlog of the agent's live flows.
pub fn queued_events(conn: &Connection, agent: &str) -> Result<u64> {
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM event_receipts WHERE state='backlog' AND flow_id IN ({LIVE_FLOWS})"
        ),
        [agent],
        |r| r.get(0),
    )?)
}

/// The agent a flow belongs to: its owner, or before any approval the single
/// author of its versions. The same rule decides which flows a move carries.
pub fn flow_agent(conn: &Connection, flow_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT agent_id FROM agent_move_flow_agents WHERE flow_id=?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether `flow_id` is a tombstone: its agent moved to another computer.
pub fn tombstoned(conn: &Connection, flow_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(removed_by,'') LIKE 'moved:%' AND removed_by<>?2 FROM flows WHERE flow_id=?1",
            params![flow_id, REPLACED],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// What `release` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    /// The flows tombstoned, whose routes the caller releases.
    pub flows: Vec<String>,
    /// Backlogged events discarded: they wait on this computer's event
    /// providers, so they can't follow the move. This counts every one,
    /// including those queued after the cut, which the receiver's
    /// `dropped` count could not include.
    pub queued_events: u64,
}

/// Turns the agent's freeze into a tombstone naming `holder`: its flows are
/// marked moved and stopped, their planned fires and queued events are
/// discarded, and pending install cards for them are settled as superseded.
pub fn release(
    tx: &Transaction,
    agent: &str,
    cut_id: &str,
    holder: &str,
    now_ms: i64,
) -> Result<Released> {
    let queued_events = queued_events(tx, agent)?;
    let flows: Vec<String> = {
        let mut stmt = tx.prepare(&format!("{LIVE_FLOWS} ORDER BY flow_id"))?;
        stmt.query_map([agent], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let at =
        jiff::Timestamp::from_millisecond(now_ms).map_err(|e| CoreError::Invalid(e.to_string()))?;
    for flow in &flows {
        let pending: Vec<u32> = {
            let mut stmt = tx.prepare(
                "SELECT version FROM install_cards WHERE flow_id=?1 AND state='pending'",
            )?;
            stmt.query_map([flow], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for version in pending {
            crate::install::supersede_pending_card(tx, flow, version, now_ms)?;
        }
        tx.execute(
            "UPDATE flows SET state='disabled', disabled_by=?2, disabled_reason='moved', disabled_at=?3, \
             removed=1, removed_by=?4, removed_at=?3 WHERE flow_id=?1",
            params![flow, MOVE_ACTOR, now_ms, format!("{MOVED_PREFIX}{holder}")],
        )?;
        crate::schedule::table::disable(tx, flow, at)?;
        tx.execute(
            "UPDATE event_receipts SET state='refused' WHERE flow_id=?1 AND state='backlog'",
            [flow],
        )?;
        tx.execute("DELETE FROM agent_move_pending WHERE flow_id=?1", [flow])?;
    }
    tx.execute(
        "INSERT INTO agent_move_holds(agent_id,kind,cut_id,holder,at) VALUES (?1,'moved',?2,?3,?4)
         ON CONFLICT(agent_id) DO UPDATE SET kind='moved',cut_id=excluded.cut_id,holder=excluded.holder,at=excluded.at",
        params![agent, cut_id, holder, now_ms],
    )?;
    Ok(Released {
        flows,
        queued_events,
    })
}

/// Why `activate` wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivateRefusal {
    /// The receiver's flows are in use and cannot be replaced yet.
    Busy(Busy),
    /// This computer's live copy of the agent's flows gained data the cut
    /// lacks since `diverged`.
    Advanced,
    /// A flow the cut carries belongs to someone else here.
    Collision(Collision),
    /// The stream holds something this store refuses (a constraint the
    /// rows break): a fault in the moved data, not a transient one.
    Rejected(String),
}

fn sql_error(error: rusqlite::Error) -> ActivateRefusal {
    ActivateRefusal::Rejected(error.to_string())
}

/// Deletes the rows of the agent's old copy of `flow_id` that the incoming
/// copy replaces. The `flows` row itself is updated in place by the caller,
/// so rows keyed to it elsewhere (event receipts, health counters) stay.
fn clear_old_copy(tx: &Transaction, flow_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM run_history WHERE flow_id=?1 AND NOT EXISTS (SELECT 1 FROM obligations o WHERE o.run_id=run_history.run_id AND o.state<>'settled')",
        [flow_id],
    )?;
    tx.execute(
        "DELETE FROM run_snapshots WHERE flow_id=?1 AND snapshot_id NOT IN (SELECT snapshot_id FROM run_history UNION SELECT snapshot_id FROM live_runs)",
        [flow_id],
    )?;
    tx.execute("DELETE FROM kv WHERE flow_id=?1", [flow_id])?;
    crate::schedule::table::remove(tx, flow_id)?;
    tx.execute(
        "DELETE FROM template_event_outbox WHERE flow_id=?1",
        [flow_id],
    )?;
    tx.execute("DELETE FROM template_instances WHERE flow_id=?1", [flow_id])?;
    tx.execute(
        "DELETE FROM instance_revocations WHERE flow_id=?1",
        [flow_id],
    )?;
    tx.execute("DELETE FROM install_cards WHERE flow_id=?1", [flow_id])?;
    tx.execute("DELETE FROM installs WHERE flow_id=?1", [flow_id])?;
    tx.execute("DELETE FROM agent_move_pending WHERE flow_id=?1", [flow_id])?;
    Ok(())
}

/// Writes a decoded stream as the agent's flows on this computer, in the
/// caller's transaction. The agent's tombstoned copy (a move back), or a
/// live copy that matches the last cut, is replaced. A flow that was
/// approved on the source arrives disabled by `move`, with its approved
/// version back to `validated`, and waits in `agent_move_pending`; any
/// other flow arrives in the source's own enable state.
///
/// A refusal leaves nothing written: the rows are written under a savepoint
/// that a refusal rolls back. Returns the fingerprint of what it wrote.
pub fn activate(
    tx: &Transaction,
    agent: &str,
    cut_id: &str,
    staged: &Staged,
    now_ms: i64,
) -> Result<std::result::Result<String, ActivateRefusal>> {
    tx.execute_batch("SAVEPOINT agent_move_activate")?;
    let outcome = write_staged(tx, agent, cut_id, staged, now_ms);
    match &outcome {
        Ok(Ok(_)) => tx.execute_batch("RELEASE agent_move_activate")?,
        _ => tx.execute_batch("ROLLBACK TO agent_move_activate; RELEASE agent_move_activate")?,
    }
    outcome
}

fn write_staged(
    tx: &Transaction,
    agent: &str,
    cut_id: &str,
    staged: &Staged,
    now_ms: i64,
) -> Result<std::result::Result<String, ActivateRefusal>> {
    use stream::{
        CALL, FLOW, GENERATION, HISTORY, INSTANCE, KV, POLICY, SCHEDULE, SNAPSHOT, VERSION, insert,
        values,
    };
    // A live run can still commit its kv overlay into the receiver's copy.
    // Only a live run blocks here: a stray open card on flows about to be
    // replaced would otherwise hold the move at `imported` indefinitely.
    if let Some(busy) = busy(tx, agent)?.filter(|b| b.code == "run_active") {
        return Ok(Err(ActivateRefusal::Busy(busy)));
    }
    if local(tx, agent)? == Local::Advanced {
        return Ok(Err(ActivateRefusal::Advanced));
    }
    if let Some(collision) = collision(tx, agent, staged)? {
        return Ok(Err(ActivateRefusal::Collision(collision)));
    }
    let rejected = |m: String| Ok(Err(ActivateRefusal::Rejected(m)));
    let old: Vec<String> = {
        let mut stmt = tx.prepare(AGENT_FLOWS)?;
        stmt.query_map([agent], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let incoming: BTreeSet<String> = staged.flow_ids().into_iter().collect();
    for flow in &old {
        clear_old_copy(tx, flow)?;
        if !incoming.contains(flow) {
            // A flow of the agent here that the incoming copy doesn't carry:
            // its versions are gone with the old copy, so it ends removed
            // rather than enabled with nothing to run.
            tx.execute(
                "UPDATE flows SET owner=?2, state='disabled', disabled_by=?3, disabled_reason='replaced_by_move', \
                 disabled_at=?4, approved_version=NULL, removed=1, removed_by=?5, removed_at=?4 WHERE flow_id=?1",
                params![flow, agent, MOVE_ACTOR, now_ms, REPLACED],
            )?;
        }
    }
    tx.execute(
        "DELETE FROM instance_generations WHERE agent_id=?1",
        [agent],
    )?;
    for flow in staged.of_kind("flow") {
        let id = flow["flow_id"].as_str().unwrap_or_default();
        let source_state = json!({
            "state": flow["state"],
            "disabled_by": flow["disabled_by"],
            "disabled_reason": flow["disabled_reason"],
            "disabled_at": flow["disabled_at"],
        });
        let approved = flow["approved_version"].as_i64();
        let package = flow["package"].as_str();
        // The version the source approved, and the code hash this computer
        // must report active before the flow runs here.
        let waits: Option<(i64, String)> = match (approved, package) {
            (Some(v), Some(_)) => flow["package_code_hash"]
                .as_str()
                .map(|h| (v, h.to_owned())),
            (Some(v), None) => flow["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|x| x["version"].as_i64() == Some(v))
                .and_then(|x| x["code_hash"].as_str().map(|h| (v, h.to_owned()))),
            (None, _) => None,
        };
        let mut row = match values(
            flow,
            FLOW,
            &[
                "kind",
                "versions",
                "policies",
                "schedule",
                "package_code_hash",
            ],
            "flow",
        ) {
            Ok(row) => row,
            Err(m) => return rejected(m),
        };
        let set = |row: &mut Vec<SqlValue>, column: &str, value: SqlValue| {
            let at = FLOW
                .iter()
                .position(|(n, _)| *n == column)
                .expect("a listed flow column");
            row[at] = value;
        };
        if waits.is_some() {
            set(&mut row, "state", SqlValue::Text("disabled".into()));
            set(&mut row, "disabled_by", SqlValue::Text(MOVE_ACTOR.into()));
            set(
                &mut row,
                "disabled_reason",
                SqlValue::Text("moved_pending_approval".into()),
            );
            set(&mut row, "disabled_at", SqlValue::Integer(now_ms));
            if package.is_none() {
                set(&mut row, "approved_version", SqlValue::Null);
            }
        }
        let names: Vec<&str> = FLOW.iter().map(|(n, _)| *n).collect();
        let updates = names
            .iter()
            .skip(1)
            .map(|n| format!("{n}=excluded.{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let marks = (1..=names.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        if let Err(error) = tx.execute(
            &format!("INSERT INTO flows ({}) VALUES ({marks}) ON CONFLICT(flow_id) DO UPDATE SET {updates},scope_health=NULL", names.join(",")),
            rusqlite::params_from_iter(row),
        ) {
            return Ok(Err(sql_error(error)));
        }
        for version in flow["versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let mut row = match values(version, VERSION, &[], "version") {
                Ok(row) => row,
                Err(m) => return rejected(m),
            };
            if version["state"] == "approved" {
                // Approval doesn't move with the bytes: this computer's
                // prefrontal must stand behind them first.
                for (column, value) in [
                    ("state", SqlValue::Text("validated".into())),
                    ("approval_ref", SqlValue::Null),
                    ("approved_at", SqlValue::Null),
                    ("approval_kind", SqlValue::Null),
                    ("approval_rule", SqlValue::Null),
                ] {
                    let at = VERSION
                        .iter()
                        .position(|(n, _)| *n == column)
                        .expect("a listed version column");
                    row[at] = value;
                }
            }
            if let Err(error) = insert(
                tx,
                "INSERT",
                "installs",
                VERSION,
                row,
                &[("flow_id", SqlValue::Text(id.into()))],
            ) {
                return Ok(Err(ActivateRefusal::Rejected(error.to_string())));
            }
        }
        for policy in flow["policies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let row = match values(policy, POLICY, &[], "policy") {
                Ok(row) => row,
                Err(m) => return rejected(m),
            };
            if let Err(error) = insert(
                tx,
                "INSERT",
                "install_tool_policies",
                POLICY,
                row,
                &[("flow_id", SqlValue::Text(id.into()))],
            ) {
                return Ok(Err(ActivateRefusal::Rejected(error.to_string())));
            }
        }
        if let Some(schedule) = flow["schedule"].as_object() {
            let mut row = match values(schedule, SCHEDULE, &[], "schedule") {
                Ok(row) => row,
                Err(m) => return rejected(m),
            };
            if waits.is_some() {
                // Stopped with its next due time kept: when the flow is
                // approved here, `missed` decides what the gap owes.
                let at = SCHEDULE
                    .iter()
                    .position(|(n, _)| *n == "state")
                    .expect("state column");
                row[at] = SqlValue::Text("disabled".into());
            }
            if let Err(error) = insert(
                tx,
                "INSERT",
                "schedules",
                SCHEDULE,
                row,
                &[
                    ("flow_id", SqlValue::Text(id.into())),
                    ("updated_at", SqlValue::Integer(now_ms)),
                ],
            ) {
                return Ok(Err(ActivateRefusal::Rejected(error.to_string())));
            }
        }
        if let Some((version, hash)) = waits {
            let Some(hash) = stream::unhex(&hash) else {
                return rejected(format!("flow {id} carries an unreadable code hash"));
            };
            tx.execute(
                "INSERT INTO agent_move_pending(flow_id,agent_id,cut_id,version,code_hash,restore,since,retry_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
                params![id, agent, cut_id, version, hash, source_state.to_string(), now_ms],
            )?;
        }
    }
    for (kind, table, columns) in [
        ("instance", "template_instances", INSTANCE),
        ("generation", "instance_generations", GENERATION),
        ("kv", "kv", KV),
    ] {
        for record in staged.of_kind(kind) {
            let row = match values(record, columns, &["kind"], kind) {
                Ok(row) => row,
                Err(m) => return rejected(m),
            };
            if let Err(error) = insert(tx, "INSERT", table, columns, row, &[]) {
                return Ok(Err(ActivateRefusal::Rejected(error.to_string())));
            }
        }
    }
    for record in staged.of_kind("snapshot") {
        let row = match values(record, SNAPSHOT, &["kind"], "snapshot") {
            Ok(row) => row,
            Err(m) => return rejected(m),
        };
        // The code hash names the exact bytes, so a snapshot already here
        // under the same flow, version and hash is the same snapshot.
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_snapshots WHERE flow_id=?1 AND flow_version=?2 AND lower(hex(code_hash))=?3)",
            params![record["flow_id"].as_str(), record["flow_version"].as_i64(), record["code_hash"].as_str()],
            |r| r.get(0),
        )?;
        if !present && let Err(error) = insert(tx, "INSERT", "run_snapshots", SNAPSHOT, row, &[]) {
            return rejected(format!("a run snapshot: {error}"));
        }
    }
    let mut highest_admit_seq = 0i64;
    for record in staged.of_kind("history") {
        let row = match values(record, HISTORY, &["kind", "code_hash", "calls"], "history") {
            Ok(row) => row,
            Err(m) => return rejected(m),
        };
        let hash =
            stream::unhex(record["code_hash"].as_str().unwrap_or_default()).unwrap_or_default();
        let snapshot: Option<i64> = tx
            .query_row(
                "SELECT snapshot_id FROM run_snapshots WHERE flow_id=?1 AND flow_version=?2 AND code_hash=?3",
                params![record["flow_id"].as_str(), record["flow_version"].as_i64(), hash],
                |r| r.get(0),
            )
            .optional()?;
        let Some(snapshot) = snapshot else {
            return rejected(format!(
                "run {} names a snapshot the cut doesn't carry",
                record["run_id"]
            ));
        };
        let run_id = record["run_id"].as_str().unwrap_or_default().to_owned();
        highest_admit_seq = highest_admit_seq.max(record["admit_seq"].as_i64().unwrap_or(0));
        // A run id embeds the store that admitted it, so one already here
        // is the same run: keep the copy this computer has. Any other row
        // the store refuses is a fault in the moved data, never skipped.
        let present: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_history WHERE run_id=?1)",
            [&run_id],
            |r| r.get(0),
        )?;
        if present {
            continue;
        }
        if let Err(error) = insert(
            tx,
            "INSERT",
            "run_history",
            HISTORY,
            row,
            &[("snapshot_id", SqlValue::Integer(snapshot))],
        ) {
            return rejected(format!("run {run_id}: {error}"));
        }
        for call in record["calls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let row = match values(call, CALL, &[], "call") {
                Ok(row) => row,
                Err(m) => return rejected(m),
            };
            if let Err(error) = insert(
                tx,
                "INSERT",
                "run_calls",
                CALL,
                row,
                &[("run_id", SqlValue::Text(run_id.clone()))],
            ) {
                return Ok(Err(ActivateRefusal::Rejected(error.to_string())));
            }
        }
    }
    // Moved runs keep the admission order they had on the source. Runs this
    // computer admits from now on must come after them, or "latest run" and
    // failure streaks would read moved runs as newer than local ones. The
    // counter is shared with planned schedule fires, which only need it to
    // grow.
    if highest_admit_seq > 0 {
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('next_run',?1) ON CONFLICT(key) DO UPDATE \
             SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT)",
            [(highest_admit_seq + 1).to_string()],
        )?;
    }
    tx.execute(
        "DELETE FROM agent_move_holds WHERE agent_id=?1 AND kind='moved'",
        [agent],
    )?;
    Ok(Ok(fingerprint(tx, agent)?))
}

/// A moved flow waiting for this computer's approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub flow_id: String,
    pub agent: String,
    pub version: u32,
    pub code_hash: [u8; 32],
    /// The flow's template or package, for an instance.
    pub package: Option<String>,
    pub since_ms: i64,
}

/// Pending moved flows, oldest first; only `flow_id` when given.
pub fn pending(conn: &Connection, flow_id: Option<&str>) -> Result<Vec<Pending>> {
    let mut stmt = conn.prepare(
        "SELECT p.flow_id,p.agent_id,p.version,p.code_hash,f.package,p.since FROM agent_move_pending p \
         JOIN flows f USING(flow_id) WHERE ?1 IS NULL OR p.flow_id=?1 ORDER BY p.since,p.flow_id",
    )?;
    let rows = stmt
        .query_map([flow_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(flow_id, agent, version, hash, package, since_ms)| {
            Ok(Pending {
                flow_id,
                agent,
                version,
                code_hash: crate::model::digest(hash)?,
                package,
                since_ms,
            })
        })
        .collect()
}

/// The longest wait between checks while prefrontal can't be reached.
pub const RETRY_MAX_MS: i64 = 10 * 60 * 1000;
const RETRY_FIRST_MS: i64 = 2_000;

/// Records the outcome of one approval check of a pending flow. A check
/// that reached prefrontal waits for the next approval event; one that
/// couldn't reach it is tried again after a doubling wait.
pub fn note_check(
    tx: &Transaction,
    flow_id: &str,
    outcome: &str,
    reached: bool,
    now_ms: i64,
) -> Result<()> {
    if reached {
        tx.execute(
            "UPDATE agent_move_pending SET checked_at=?2,last_check=?3,retry_at=NULL,retries=0 WHERE flow_id=?1",
            params![flow_id, now_ms, outcome],
        )?;
    } else {
        tx.execute(
            "UPDATE agent_move_pending SET checked_at=?2,last_check=?3,retries=retries+1,
                retry_at=?2+MIN(?4,?5*(1<<MIN(retries,20))) WHERE flow_id=?1",
            params![flow_id, now_ms, outcome, RETRY_MAX_MS, RETRY_FIRST_MS],
        )?;
    }
    Ok(())
}

/// Makes pending moved flows due for a check now: all of them (`None`), one
/// flow, or every instance of one template or package.
pub fn recheck(
    tx: &Transaction,
    flow_id: Option<&str>,
    package: Option<&str>,
    now_ms: i64,
) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE agent_move_pending SET retry_at=?3,retries=0 WHERE (?1 IS NULL OR flow_id=?1)
           AND (?2 IS NULL OR flow_id IN (SELECT flow_id FROM flows WHERE package=?2))",
        params![flow_id, package, now_ms],
    )?)
}

/// When the next pending check falls due, if any is scheduled.
pub fn next_check_at(conn: &Connection) -> Result<Option<i64>> {
    Ok(
        conn.query_row("SELECT MIN(retry_at) FROM agent_move_pending", [], |r| {
            r.get(0)
        })?,
    )
}

/// A moved flow is approved here: put it back in the source's own enable
/// state. A schedule keeps its moved next due time, so `missed` applies to
/// the time the move took. Nothing changes when the flow isn't waiting, or
/// when someone else has disabled or enabled it since it arrived.
pub fn lift_move(tx: &Transaction, flow_id: &str, now_ms: i64) -> Result<bool> {
    let restore: Option<String> = tx
        .query_row(
            "SELECT restore FROM agent_move_pending WHERE flow_id=?1",
            [flow_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(restore) = restore else {
        return Ok(false);
    };
    tx.execute("DELETE FROM agent_move_pending WHERE flow_id=?1", [flow_id])?;
    let still_moved: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM flows WHERE flow_id=?1 AND state='disabled' AND disabled_by=?2)",
        params![flow_id, MOVE_ACTOR],
        |r| r.get(0),
    )?;
    if !still_moved {
        return Ok(true);
    }
    let restore: Map<String, Value> = serde_json::from_str(&restore)
        .map_err(|e| CoreError::Corrupt(format!("move restore of {flow_id}: {e}")))?;
    if restore.get("state").and_then(Value::as_str) == Some("enabled") {
        tx.execute(
            "UPDATE flows SET state='enabled',disabled_by=NULL,disabled_reason=NULL,disabled_at=NULL WHERE flow_id=?1",
            [flow_id],
        )?;
        tx.execute(
            "UPDATE schedules SET state='active',updated_at=?2 WHERE flow_id=?1",
            params![flow_id, now_ms],
        )?;
    } else {
        tx.execute(
            "UPDATE flows SET disabled_by=?2,disabled_reason=?3,disabled_at=?4 WHERE flow_id=?1",
            params![
                flow_id,
                restore.get("disabled_by").and_then(Value::as_str),
                restore.get("disabled_reason").and_then(Value::as_str),
                restore
                    .get("disabled_at")
                    .and_then(Value::as_i64)
                    .unwrap_or(now_ms)
            ],
        )?;
    }
    Ok(true)
}

/// What one approval check of the moved flows found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Flows approved here and put back in their source's enable state.
    pub approved: Vec<String>,
    /// Flows this computer's prefrontal answered for, without approving
    /// the moved code yet; they wait for the next approval event.
    pub waiting: Vec<String>,
    /// Flows whose check failed because prefrontal couldn't be reached;
    /// only these are worth checking again before another event.
    pub unreachable: Vec<String>,
}

impl crate::Runtime {
    /// Checks moved flows waiting for approval here (all of them, or only
    /// `flow_id`) against this computer's prefrontal. Prefrontal is read
    /// with no store lock held; each approval then commits on its own.
    pub fn check_moved_flows(&self, flow_id: Option<&str>) -> Result<CheckReport> {
        let waiting = self.store().read(|c| pending(c, flow_id))?;
        self.check_pending(waiting)
    }

    /// Checks the pending moved flows whose check is due.
    pub fn check_due_moved_flows(&self) -> Result<CheckReport> {
        let now = self.config().clock.now_ms();
        let due = self.store().read(|c| {
            let ids: Vec<String> = c
                .prepare("SELECT flow_id FROM agent_move_pending WHERE retry_at<=?1 ORDER BY retry_at,flow_id")?
                .query_map([now], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut out = Vec::new();
            for id in ids {
                out.extend(pending(c, Some(&id))?);
            }
            Ok(out)
        })?;
        self.check_pending(due)
    }

    /// Makes pending moved flows due now (see [`recheck`]) and wakes the
    /// engine to check them.
    pub fn recheck_moved_flows(&self, flow_id: Option<&str>, package: Option<&str>) -> Result<()> {
        // Most computers never receive a move: they answer from a read and
        // open no write transaction for every approval or instance event.
        let any: bool = self.store().read(|c| {
            Ok(
                c.query_row("SELECT EXISTS(SELECT 1 FROM agent_move_pending)", [], |r| {
                    r.get(0)
                })?,
            )
        })?;
        if !any {
            return Ok(());
        }
        let now = self.config().clock.now_ms();
        let changed = self
            .store()
            .write(|tx| recheck(tx, flow_id, package, now))?;
        if changed > 0 {
            self.shared.signal.bump();
        }
        Ok(())
    }

    /// [`Self::recheck_moved_flows`] after an op that has already committed:
    /// a failure only delays the check (startup checks every waiting flow
    /// again), so it is logged rather than turned into the op's error.
    pub fn recheck_moved_flows_after(&self, flow_id: Option<&str>, package: Option<&str>) {
        if let Err(error) = self.recheck_moved_flows(flow_id, package) {
            tracing::warn!(target: "agent_move", ?flow_id, ?package, "scheduling a moved-flow approval check: {error}");
        }
    }

    fn check_pending(&self, waiting: Vec<Pending>) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        for p in waiting {
            let now = self.config().clock.now_ms();
            let expected = stream::hex(&p.code_hash);
            let outcome = match self.core_install_status_with_approval(&p.flow_id, p.version) {
                Err(error) => {
                    self.note_moved_check(
                        &p.flow_id,
                        &format!("prefrontal unreachable: {error:?}"),
                        false,
                        now,
                    )?;
                    report.unreachable.push(p.flow_id);
                    continue;
                }
                Ok((basal_host::InstallStatus::Active { code_hash, .. }, approval))
                    if code_hash == expected =>
                {
                    match &p.package {
                        None => {
                            if self.approve_confirmed_install(
                                &p.flow_id,
                                p.version,
                                &p.code_hash,
                                approval,
                            )? {
                                // Approval lifts the move's disable in the same commit.
                                let lifted = self
                                    .store()
                                    .read(|c| Ok(pending(c, Some(&p.flow_id))?.is_empty()))?;
                                if lifted {
                                    report.approved.push(p.flow_id);
                                    continue;
                                }
                            }
                            "active here, but the version could not be approved (see flow.health)"
                                .to_owned()
                        }
                        Some(package) => {
                            let lifted = self.store().write(|tx| {
                                let local: Option<Vec<u8>> = tx
                                    .query_row(
                                        "SELECT code_hash FROM package_versions WHERE package=?1 AND version=?2",
                                        params![package, p.version],
                                        |r| r.get(0),
                                    )
                                    .optional()?;
                                if local.as_deref() != Some(p.code_hash.as_slice()) {
                                    return Ok(false);
                                }
                                lift_move(tx, &p.flow_id, now)
                            })?;
                            if lifted {
                                self.shared.signal.bump();
                                report.approved.push(p.flow_id);
                                continue;
                            }
                            format!(
                                "template {package} version {} with this code is not registered here",
                                p.version
                            )
                        }
                    }
                }
                Ok((status, _)) => format!("not approved here: {status:?}"),
            };
            self.note_moved_check(&p.flow_id, &outcome, true, now)?;
            report.waiting.push(p.flow_id);
        }
        Ok(report)
    }

    /// Releases the agent's flows to `holder` (see [`release`]), then
    /// releases their route scopes and wakes the engine. The `on_commit`
    /// closure runs inside the same transaction, after the tombstone, so a
    /// caller's checkpoint commits with it.
    pub fn release_agent(
        &self,
        agent: &str,
        cut_id: &str,
        holder: &str,
        on_commit: impl FnOnce(&Transaction, &Released) -> Result<()>,
    ) -> Result<Released> {
        let now = self.config().clock.now_ms();
        let released = self.store().write(|tx| {
            let released = release(tx, agent, cut_id, holder, now)?;
            on_commit(tx, &released)?;
            Ok(released)
        })?;
        tracing::info!(
            target: "agent_move",
            agent,
            cut_id,
            holder,
            flows = released.flows.len(),
            queued_events = released.queued_events,
            "released the agent's flows; its backlogged events stay behind"
        );
        for flow in &released.flows {
            self.release_unused_flow_routes_for(flow)?;
        }
        self.shared.signal.bump();
        Ok(released)
    }

    /// Wakes the engine after a freeze lifts or moved flows arrive, so
    /// waiting fires and queued events are admitted without delay.
    pub fn agent_move_changed(&self) {
        self.shared.signal.bump();
    }

    fn note_moved_check(
        &self,
        flow_id: &str,
        outcome: &str,
        reached: bool,
        now_ms: i64,
    ) -> Result<()> {
        let outcome: String = outcome.chars().take(512).collect();
        self.store()
            .write(|tx| note_check(tx, flow_id, &outcome, reached, now_ms))
    }
}

#[cfg(test)]
#[path = "agent_move/tests.rs"]
mod tests;
