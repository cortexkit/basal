//! Permanent ownership facts and their replay cursor. Inserts also enqueue bus
//! delivery through the table's trigger, in the caller's transaction.
use crate::{CoreError, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

fn agent(owner: Option<&str>) -> Option<&str> {
    owner.filter(|id| id.starts_with("agent_"))
}

fn flow_identity(conn: &Connection, flow_id: &str) -> Result<(Option<String>, String)> {
    let (owner, package): (Option<String>, Option<String>) = conn.query_row(
        "SELECT owner,package FROM flows WHERE flow_id=?1",
        [flow_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((
        owner,
        format!("basal.{}", package.as_deref().unwrap_or(flow_id)),
    ))
}

fn insert(tx: &Transaction<'_>, fact: Value) -> Result<()> {
    for (name, max, dots) in [("workload", 64, false), ("task", 128, true)] {
        let value = fact[name].as_str().unwrap_or_default();
        if value.is_empty()
            || value.len() > max
            || !value.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-'
                    || (dots && b == b'.')
            })
        {
            return Err(CoreError::Invalid(format!(
                "invalid attribution {name}: {value:?}"
            )));
        }
    }
    let body = fact.to_string();
    if body.len() >= 4096 {
        return Err(CoreError::Invalid(
            "attribution fact must be under 4 KiB".into(),
        ));
    }
    tx.execute("INSERT INTO attribution_facts(fact_id,body,recorded_at) VALUES(?1,?2,?3) ON CONFLICT(fact_id) DO NOTHING",
        params![fact["fact_id"].as_str(), body, fact["recorded_at_ms"].as_i64()])?;
    Ok(())
}

/// Called after approval or instance creation, in the transaction that owns it.
/// The raw owner comparison belongs to approval: non-agent owners normalize to
/// null in the public fact, but changing between them is still an owner change.
pub fn flow_approved(
    tx: &Transaction<'_>,
    flow_id: &str,
    owner_changed: bool,
    now: i64,
) -> Result<()> {
    let previous: Option<String> = tx.query_row(
        "SELECT fact_id FROM attribution_facts WHERE json_extract(body,'$.subject.kind')='flow' AND json_extract(body,'$.subject.flow_id')=?1 ORDER BY seq DESC LIMIT 1",
        [flow_id], |r| r.get(0)).optional()?;
    if previous.is_some() && !owner_changed {
        return Ok(());
    }
    let (owner, task) = flow_identity(tx, flow_id)?;
    let (version, package, created, first): (Option<u32>, Option<String>, i64, Option<i64>) = tx.query_row(
        "SELECT approved_version,package,created_at,first_approved_at FROM flows WHERE flow_id=?1", [flow_id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let id = if package.is_some() {
        format!("basal:flow:{flow_id}:created")
    } else {
        format!(
            "basal:flow:{flow_id}:v{}",
            version.ok_or_else(|| CoreError::Invalid(
                "attribution requires an approved version".into()
            ))?
        )
    };
    let effective = if package.is_some() {
        created
    } else if previous.is_none() {
        first.ok_or_else(|| CoreError::Invalid("approved flow lacks first approval time".into()))?
    } else {
        now
    };
    insert(
        tx,
        json!({"fact_id":id,"supersedes":previous,"subject":{"kind":"flow","flow_id":flow_id},
        "alfonso":agent(owner.as_deref()),"parent":null,"workload":"flow","task":task,
        "project_id":null,"effective_from_ms":effective,"recorded_at_ms":now}),
    )
}

pub fn flow_session(tx: &Transaction<'_>, flow_id: &str, session: &str, now: i64) -> Result<()> {
    let (owner, task) = flow_identity(tx, flow_id)?;
    insert(
        tx,
        json!({"fact_id":format!("basal:session:{session}"),"supersedes":null,
        "subject":{"kind":"session","runner":"broca","harness":"broca","session":session},
        "alfonso":agent(owner.as_deref()),"parent":{"kind":"flow","flow_id":flow_id},
        "workload":"flow","task":task,"project_id":null,"effective_from_ms":now,"recorded_at_ms":now}),
    )
}

pub fn program_session(tx: &Transaction<'_>, run_id: &str, session: &str, now: i64) -> Result<()> {
    let (owner, bind): (String, Option<String>) = tx.query_row(
        "SELECT COALESCE(on_behalf_of_agent,agent_id),bind_identity FROM codemode_runs WHERE run_id=?1", [run_id],
        |r| Ok((r.get(0)?,r.get(1)?)))?;
    let bind: Value = bind
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| CoreError::Corrupt(e.to_string()))?
        .unwrap_or(Value::Null);
    let parent = match (
        bind["runner"].as_str(),
        bind["harness"].as_str(),
        bind["session"].as_str(),
    ) {
        (Some(runner), Some(harness), Some(session))
            if !runner.is_empty() && !harness.is_empty() && !session.is_empty() =>
        {
            json!({"kind":"session","runner":runner,"harness":harness,"session":session})
        }
        _ => Value::Null,
    };
    insert(
        tx,
        json!({"fact_id":format!("basal:session:{session}"),"supersedes":null,
        "subject":{"kind":"session","runner":"broca","harness":"broca","session":session},"alfonso":agent(Some(&owner)),
        "parent":parent,"workload":"codemode","task":"basal.codemode","project_id":null,
        "effective_from_ms":now,"recorded_at_ms":now}),
    )
}

/// A read transaction fixes the high-water mark and page to one snapshot.
pub fn read(conn: &Connection, after: i64, limit: usize) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let through: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM attribution_facts",
        [],
        |r| r.get(0),
    )?;
    let mut stmt =
        tx.prepare("SELECT seq,body FROM attribution_facts WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
    let mut rows = stmt
        .query_map(params![after, limit as i64 + 1], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = more.then(|| rows.last().unwrap().0);
    let facts = rows
        .into_iter()
        .map(|(_, body)| {
            serde_json::from_str::<Value>(&body).map_err(|e| CoreError::Corrupt(e.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"facts":facts,"next":next,"through":through}))
}

#[cfg(test)]
#[path = "attribution_tests.rs"]
mod tests;
