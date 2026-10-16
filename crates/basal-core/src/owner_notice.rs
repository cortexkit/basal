//! Durable owner-notice episodes and handoff to core. Only error codes leave basal.
use crate::{Result, Runtime, ops};
use basal_host::{core_host::FLOW_OWNER_NOTICE_V1, transport::WireError};
use rusqlite::{Connection, Transaction, params};
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
    let mut stmt = conn.prepare_cached("SELECT run_id,state,error_kind,COALESCE(ended_at,admitted_at),COALESCE(flow_version,0),script_started FROM runs WHERE flow_id=?1 AND state IN ('succeeded','failed','engine_mismatch','cancelled') ORDER BY admit_seq DESC,run_id DESC")?;
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
        if ops::counted_failure(conn, &run_id, &state, kind.as_deref())? {
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
        "INSERT INTO outbox(at,kind,flow_id,body,retry_at) VALUES (?1,?2,?3,?4,?1)",
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

pub(crate) fn finish_run(tx: &Transaction, run_id: &str, now: i64) -> Result<()> {
    let (flow, state, version, kind): (String, String, u32, Option<String>) = tx.query_row(
        "SELECT flow_id,state,COALESCE(flow_version,0),error_kind FROM runs WHERE run_id=?1",
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
    } else if failing.is_none() && ops::counted_failure(tx, run_id, &state, kind.as_deref())? {
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

pub(crate) fn auto_disabled(tx: &Transaction, flow: &str, now: i64) -> Result<()> {
    let episode = next_episode(tx, flow)?;
    let streak = streak(tx, flow)?;
    let fallback = Failure {
        run_id: String::new(),
        version: tx.query_row(
            "SELECT COALESCE(approved_version,0) FROM flows WHERE flow_id=?1",
            [flow],
            |r| r.get(0),
        )?,
        kind: None,
        started: false,
    };
    enqueue(
        tx,
        flow,
        payload(
            flow,
            episode,
            "auto_disabled",
            &streak,
            streak.latest.as_ref().unwrap_or(&fallback),
            now,
        ),
        now,
    )
}

impl Runtime {
    /// One ordered head per flow. Network I/O happens after the read releases SQLite.
    pub fn drain_owner_notices(&self) -> Result<()> {
        let _drain = self
            .shared
            .owner_notice_drain
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !self
            .shared
            .host
            .supports_core_capability(FLOW_OWNER_NOTICE_V1)
        {
            return Ok(());
        }
        let now = self.config.clock.now_ms();
        let rows = self.store().read(|c| {
            let mut stmt = c.prepare_cached("SELECT seq,body,attempts FROM outbox o WHERE kind='flow.owner_notice' AND delivered_at IS NULL AND retry_at<=?1 AND NOT EXISTS (SELECT 1 FROM outbox older WHERE older.kind=o.kind AND older.flow_id=o.flow_id AND older.delivered_at IS NULL AND older.seq<o.seq) ORDER BY seq")?;
            Ok(stmt.query_map([now], |r| Ok((r.get::<_, i64>(0)?,r.get::<_, String>(1)?,r.get::<_, u32>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (seq, body, attempts) in rows {
            let request: Value = serde_json::from_str(&body)
                .map_err(|e| crate::CoreError::Corrupt(e.to_string()))?;
            let result = self.shared.host.owner_notice(request);
            let final_code = match &result {
                Err(WireError::Refused { code, .. } | WireError::RefusedDetails { code, .. }) => {
                    matches!(code.as_str(), "flow_not_installed" | "owner_unknown")
                        .then_some(code.as_str())
                }
                _ => None,
            };
            let final_refusal = final_code.is_some();
            let accepted = result
                .as_ref()
                .is_ok_and(|v| v.get("accepted") == Some(&Value::Bool(true)));
            if let Some(code) = final_code {
                tracing::warn!(target:"owner_notice", seq, code, "core refused an owner notice permanently");
            }
            self.store().write(|tx| {
                if accepted || final_refusal {
                    tx.execute(
                        "UPDATE outbox SET delivered_at=?2 WHERE seq=?1",
                        params![seq, self.config.clock.now_ms()],
                    )?;
                } else {
                    let delay = 1000_i64 * (1_i64 << attempts.min(8));
                    tx.execute(
                        "UPDATE outbox SET attempts=attempts+1,retry_at=?2 WHERE seq=?1",
                        params![seq, self.config.clock.now_ms().saturating_add(delay)],
                    )?;
                }
                Ok(())
            })?;
            if accepted || final_refusal {
                self.shared.signal.bump();
            }
        }
        Ok(())
    }

    pub(crate) fn owner_notice_next_wake(&self) -> Result<Option<i64>> {
        if !self
            .shared
            .host
            .supports_core_capability(FLOW_OWNER_NOTICE_V1)
        {
            return Ok(None);
        }
        self.store().read(|c| Ok(c.query_row("SELECT MIN(retry_at) FROM outbox o WHERE kind='flow.owner_notice' AND delivered_at IS NULL AND NOT EXISTS (SELECT 1 FROM outbox older WHERE older.kind=o.kind AND older.flow_id=o.flow_id AND older.delivered_at IS NULL AND older.seq<o.seq)", [], |r| r.get(0))?))
    }
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
