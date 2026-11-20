//! Durable owner-notice episodes for the bus outbox. Only error codes leave basal.
use crate::{Result, ops};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

const KIND: &str = "flow.owner_notice";

#[derive(Default)]
struct Streak {
    count: u64,
    since: Option<i64>,
    latest: Option<Failure>,
}
struct Failure {
    run_id: String,
    version: u32,
    kind: Option<String>,
    started: bool,
}

fn streak(conn: &Connection, flow: &str) -> Result<Streak> {
    let mut result = Streak::default();
    let sql = {
        "SELECT run_id,state,error_kind,ended_at,flow_version,script_started FROM run_history WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([flow], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, u32>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    for row in rows {
        let (run_id, state, kind, at, version, started) = row?;
        if state == "succeeded" {
            break;
        }
        if counted(conn, &run_id, &state, kind.as_deref())? {
            result.count += 1;
            result.since = Some(at);
            if result.latest.is_none() {
                result.latest = Some(Failure {
                    run_id,
                    version,
                    kind,
                    started,
                });
            }
        }
    }
    Ok(result)
}

fn sanitize(code: &str) -> String {
    code.bytes()
        .take(128)
        .map(|b| {
            if b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b) {
                char::from(b)
            } else {
                '_'
            }
        })
        .collect()
}

fn payload(flow: &str, episode: i64, kind: &str, streak: &Streak, run: &Failure, at: i64) -> Value {
    json!({
        "notice_id": format!("{flow}:{episode}:{kind}"), "flow_id": flow,
        "version": run.version, "kind": kind, "consecutive_failures": streak.count,
        "error_kind": if kind == "recovered" { None } else { run.kind.as_deref().map(sanitize) },
        "phase": if kind == "failing" { Some(if run.started { "script" } else { "preamble" }) } else { None },
        "failing_since_ms": streak.since, "run_id": run.run_id, "at_ms": at
    })
}

fn enqueue(tx: &Transaction, flow: &str, body: Value, now: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO outbox(at,kind,flow_id,body) VALUES (?1,?2,?3,?4)",
        params![now, KIND, flow, body.to_string()],
    )?;
    Ok(())
}

fn next_episode(tx: &Transaction, flow: &str) -> Result<i64> {
    tx.execute("INSERT INTO flow_owner_notice_latches(flow_id,episode) VALUES (?1,1) ON CONFLICT(flow_id) DO UPDATE SET episode=episode+1", [flow])?;
    Ok(tx.query_row(
        "SELECT episode FROM flow_owner_notice_latches WHERE flow_id=?1",
        [flow],
        |r| r.get(0),
    )?)
}

pub(crate) fn finish_run_staged(tx: &Transaction, run_id: &str, now: i64) -> Result<()> {
    finish(tx, run_id, now)
}

fn counted(conn: &Connection, id: &str, state: &str, kind: Option<&str>) -> Result<bool> {
    ops::counted_failure_staged(conn, id, state, kind)
}

fn finish(tx: &Transaction, run_id: &str, now: i64) -> Result<()> {
    let (flow, state, version, kind): (String, String, u32, Option<String>) = tx.query_row(
        "SELECT flow_id,state,flow_version,error_kind FROM run_history WHERE run_id=?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO flow_owner_notice_latches(flow_id) VALUES (?1)",
        [&flow],
    )?;
    let failing: Option<i64> = tx.query_row(
        "SELECT failing_episode FROM flow_owner_notice_latches WHERE flow_id=?1",
        [&flow],
        |r| r.get(0),
    )?;
    if state == "succeeded" {
        if let Some(episode) = failing {
            let run = Failure {
                run_id: run_id.into(),
                version,
                kind: None,
                started: true,
            };
            let since = tx.query_row(
                "SELECT failing_since_ms FROM flow_owner_notice_latches WHERE flow_id=?1",
                [&flow],
                |r| r.get(0),
            )?;
            enqueue(
                tx,
                &flow,
                payload(
                    &flow,
                    episode,
                    "recovered",
                    &Streak {
                        since,
                        ..Streak::default()
                    },
                    &run,
                    now,
                ),
                now,
            )?;
            tx.execute(
                "UPDATE flow_owner_notice_latches SET failing_episode=NULL WHERE flow_id=?1",
                [&flow],
            )?;
        }
    } else if failing.is_none() && counted(tx, run_id, &state, kind.as_deref())? {
        // Once this episode's notice is queued nothing more is sent until a
        // success, so later failures skip the history scan. A flow that keeps
        // failing would otherwise rescan its whole streak on every run, inside
        // the write transaction.
        let streak = streak(tx, &flow)?;
        if streak.count >= 3 {
            let episode = next_episode(tx, &flow)?;
            enqueue(
                tx,
                &flow,
                payload(
                    &flow,
                    episode,
                    "failing",
                    &streak,
                    streak.latest.as_ref().expect("counted failure"),
                    now,
                ),
                now,
            )?;
            tx.execute("UPDATE flow_owner_notice_latches SET failing_episode=?2,failing_since_ms=?3 WHERE flow_id=?1", params![flow,episode,streak.since])?;
        }
    }
    Ok(())
}

pub(crate) fn auto_disabled_committed(tx: &Transaction, flow: &str, now: i64) -> Result<()> {
    // Rate limits count admissions, including runs outside the terminal failure streak.
    let latest = tx.query_row(
        "SELECT run_id,flow_version,error_kind,script_started FROM run_history WHERE flow_id=?1 ORDER BY admit_seq DESC,run_id DESC LIMIT 1",
        [flow],
        |r| Ok(Failure { run_id: r.get(0)?, version: r.get(1)?, kind: r.get(2)?, started: r.get(3)? }),
    ).optional()?;
    let Some(latest) = latest else {
        tracing::warn!(target: "owner_notice", flow_id = flow, code = "run_missing", "cannot enqueue an auto-disabled owner notice without an admitted run");
        return Ok(());
    };
    let episode = next_episode(tx, flow)?;
    let streak = streak(tx, flow)?;
    enqueue(
        tx,
        flow,
        payload(flow, episode, "auto_disabled", &streak, &latest, now),
        now,
    )
}

/// Query SQLite for the latest run without accessing a run actor.
pub(crate) fn auto_disabled(tx: &Transaction, flow: &str, now: i64) -> Result<()> {
    let latest = tx.query_row(
        "SELECT l.run_id,i.flow_version,NULL FROM live_runs l JOIN run_identities i USING(run_id)
         WHERE l.flow_id=?1 ORDER BY l.admit_seq DESC,l.run_id DESC LIMIT 1",
        [flow],
        |r| Ok(Failure { run_id: r.get(0)?, version: r.get(1)?, kind: r.get(2)?, started: false }),
    ).optional()?;
    let latest = match latest {
        Some(latest) => Some(latest),
        None => tx
            .query_row(
                "SELECT run_id,flow_version,error_kind FROM run_history WHERE flow_id=?1
             ORDER BY admit_seq DESC,run_id DESC LIMIT 1",
                [flow],
                |r| {
                    Ok(Failure {
                        run_id: r.get(0)?,
                        version: r.get(1)?,
                        kind: r.get(2)?,
                        started: false,
                    })
                },
            )
            .optional()?,
    };
    if let Some(latest) = latest {
        let episode = next_episode(tx, flow)?;
        enqueue(
            tx,
            flow,
            payload(
                flow,
                episode,
                "auto_disabled",
                &Streak::default(),
                &latest,
                now,
            ),
            now,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_bounded_ascii_without_detail_text() {
        let code = sanitize(&format!("bad CODE/é{}", "x".repeat(200)));
        assert_eq!(code.len(), 128);
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b))
        );
        assert_eq!(sanitize("event_not_visible"), "event_not_visible");
    }

    #[test]
    fn request_bytes_match_committed_vectors() {
        let cases = [("failing-preamble", "failing", false, Some("event_not_visible")),
            ("failing-script", "failing", true, Some("script")),
            ("auto-disabled", "auto_disabled", true, Some("script")),
            ("recovered", "recovered", true, None)]
            .into_iter().map(|(name,kind,started,code)| {
                let run = Failure {run_id: if kind == "recovered" { "run-4" } else { "run-3" }.into(),version:2,kind:code.map(str::to_owned),started};
                let streak = Streak { count:if kind == "recovered" {0} else {3}, since:Some(1000),latest:None};
                json!({"name":name,"request":payload("flow-example",1,kind,&streak,&run,3000).to_string()})
            }).collect::<Vec<_>>();
        let generated = json!({"schema_version":1,"cases":cases});
        let committed: Value = serde_json::from_str(include_str!(
            "../../../test-vectors/owner-notice/vectors.json"
        ))
        .unwrap();
        assert_eq!(generated, committed, "owner notice request bytes drifted");
    }
}
