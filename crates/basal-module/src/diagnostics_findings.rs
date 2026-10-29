//! Read-only diagnoses. Evidence is allowlisted here rather than forwarding
//! health strings or stored payloads, which can contain provider secrets.

use basal_core::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::events;
use crate::ops::{OpError, OpResult};

const DEFAULT_WINDOW_MS: i64 = 21_600_000;
const MAX_WINDOW_MS: i64 = 604_800_000;
const BODY_CAP: usize = 65_536;
const SAMPLE_CAP: usize = 5;

// Three consecutive failures distinguish a broken flow from an isolated retry.
const FAILURE_THRESHOLD: i64 = 3;
// Allow a minute for the engine's deadline sweep or deferred-call retry pass.
const OVERDUE_GRACE_MS: i64 = 60_000;
// Human decisions and delivery retries may normally take minutes, not hours.
const STUCK_MS: i64 = 3_600_000;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    window_ms: Option<i64>,
}

fn window(params: Value) -> Result<i64, OpError> {
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        serde_json::from_value(params).map_err(|_| {
            OpError::new("invalid_params", "expected only integer window_ms or null")
        })?
    };
    let window = params.window_ms.unwrap_or(DEFAULT_WINDOW_MS);
    if !(1..=MAX_WINDOW_MS).contains(&window) {
        return Err(OpError::new(
            "invalid_params",
            "window_ms must be in 1..=604800000",
        ));
    }
    Ok(window)
}

#[derive(Debug, Serialize)]
struct Finding {
    check: &'static str,
    key: String,
    severity: &'static str,
    count: u64,
    first_seen_ms: i64,
    last_seen_ms: i64,
    summary: &'static str,
    samples: Vec<Value>,
    hint: &'static str,
}

impl crate::module::Module {
    pub(crate) fn diagnostics_findings(&self, params: Value) -> OpResult {
        let window = window(params)?;
        let now = self.rt.config().clock.now_ms();
        let events = self
            .event_reader
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(events::Reader::health);
        let pool = self.pool.stats();
        let codemode_pool = self.codemode_pool.stats();
        reply(
            self.rt.store(),
            now,
            window,
            events.as_ref(),
            &[("flows", &pool), ("codemode", &codemode_pool)],
        )
    }
}

fn reply(
    store: &Store,
    now: i64,
    window: i64,
    events: Option<&events::Health>,
    pools: &[(&str, &crate::pool::PoolStats)],
) -> OpResult {
    let mut findings = store_findings(store, now, window)
        .map_err(|_| OpError::new("storage_unavailable", "cannot read basal diagnostics"))?;
    if let Some(health) = events.filter(|h| !h.connected || h.error.is_some()) {
        findings.push(Finding {
            check: "event_reader", key: "bus".into(), severity: "error", count: 1,
            first_seen_ms: now, last_seen_ms: now,
            summary: "The bus event reader is not connected or is restarting; outage duration is not recorded.",
            samples: vec![json!({"code": if health.error.is_some() { "reader_failed" } else { "reader_connecting" },
                "retry_in_ms": health.retry_in_ms, "connected": health.connected})],
            hint: "Inspect basal event-reader health and bus connectivity; provider error text is deliberately omitted.",
        });
    }
    for (name, pool) in pools {
        if pool.spawn_error.is_some() || pool.spawn_retry_in_ms > 0 {
            findings.push(Finding {
                check: "worker_pool",
                key: (*name).into(),
                severity: "warn",
                count: 1,
                first_seen_ms: now,
                last_seen_ms: now,
                summary: "Worker spawning has failed or is in backoff.",
                samples: vec![
                    json!({"code":"spawn_backoff", "retry_in_ms":pool.spawn_retry_in_ms}),
                ],
                hint: "Inspect basal worker-pool health and worker launch configuration.",
            });
        }
    }
    bounded(now, window, findings)
}

fn finding(
    check: &'static str,
    key: &str,
    severity: &'static str,
    summary: &'static str,
    hint: &'static str,
) -> Finding {
    Finding {
        check,
        key: key.into(),
        severity,
        count: 0,
        first_seen_ms: i64::MAX,
        last_seen_ms: i64::MIN,
        summary,
        samples: Vec::new(),
        hint,
    }
}

fn aggregate<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
    mut finding: Finding,
    mut evidence: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<(i64, Value)>,
    standing_at: Option<i64>,
) -> basal_core::Result<Option<Finding>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        let (at, sample) = evidence(row)?;
        finding.count += 1;
        finding.first_seen_ms = finding.first_seen_ms.min(at);
        finding.last_seen_ms = finding.last_seen_ms.max(at);
        if finding.samples.len() < SAMPLE_CAP {
            finding.samples.push(sample);
        }
    }
    if finding.count == 0 {
        return Ok(None);
    }
    if let Some(now) = standing_at {
        finding.last_seen_ms = now;
    }
    Ok(Some(finding))
}

fn store_findings(store: &Store, now: i64, window: i64) -> basal_core::Result<Vec<Finding>> {
    store.read(|conn| {
        let mut findings = Vec::new();
        for flow in basal_core::ops::flow_health(conn, now)? {
            if flow.auto_disabled || (flow.enabled && flow.consecutive_failures >= FAILURE_THRESHOLD as u64) {
                let disabled_at: Option<i64> = conn.query_row("SELECT disabled_at FROM flows WHERE flow_id=?1",
                    [&flow.flow_id], |r| r.get(0))?;
                findings.push(Finding {
                    check: "flows_failing", key: flow.flow_id.clone(),
                    severity: if flow.auto_disabled { "error" } else { "warn" },
                    count: flow.consecutive_failures.max(1),
                    first_seen_ms: flow.failure_first_seen_ms.or(disabled_at).unwrap_or(now), last_seen_ms: now,
                    summary: if flow.auto_disabled { "A flow was auto-disabled by the runtime." }
                        else { "An enabled flow has repeated consecutive failures." },
                    samples: vec![json!({"flow_id":flow.flow_id, "consecutive_failures":flow.consecutive_failures,
                        "auto_disabled":flow.auto_disabled})],
                    hint: "Inspect flow.health and recent run outcomes before deciding whether to re-enable the flow.",
                });
            }
        }
        let run_sample = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, Value)> {
            let at: i64 = r.get(0)?;
            Ok((at, json!({"at_ms":at, "flow_id":r.get::<_,String>(1)?, "run_id":r.get::<_,String>(2)?})))
        };
        let call_sample = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, Value)> {
            let (at, mut sample) = run_sample(r)?;
            sample["position"] = json!(r.get::<_,i64>(3)?);
            Ok((at, sample))
        };
        findings.extend(aggregate(conn,
            "SELECT deadline_at,flow_id,run_id FROM runs WHERE ended_at IS NULL AND deadline_at<=?1 ORDER BY deadline_at,run_id",
            [now.saturating_sub(OVERDUE_GRACE_MS)],
            finding("runs_overdue", "deadline", "warn", "Unfinished runs are past their recorded deadlines.",
                "Inspect run state and the deadline sweep; diagnostics never cancels runs."), run_sample, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT j.retry_not_before,r.flow_id,r.run_id,j.position FROM journal j JOIN runs r USING(run_id) WHERE j.dispatch='deferred' AND j.retry_not_before<=?1 AND r.ended_at IS NULL AND r.state IN ('pending','running','suspended') ORDER BY j.retry_not_before,r.run_id,j.position",
            [now.saturating_sub(OVERDUE_GRACE_MS)],
            finding("runs_overdue", "deferred", "warn", "Deferred calls are past their scheduled retry times.",
                "Inspect flow.health and deferred-call wake processing."), call_sample, Some(now))?);
        // Reconcile state-entry times are not stored. Admission age is the
        // available conservative signal, not a claim about time in that state.
        findings.extend(aggregate(conn,
            "SELECT admitted_at,flow_id,run_id FROM runs WHERE state='needs_reconcile' AND admitted_at<=?1 ORDER BY admitted_at,run_id",
            [now.saturating_sub(STUCK_MS)],
            finding("reconcile_pending", "runs", "warn", "Runs admitted over an hour ago now need operator reconciliation; state-entry time is unavailable.",
                "Inspect unknown calls and use flow.reconcile only after establishing their outcomes."), run_sample, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT created_at,flow_id,run_id,position,seq FROM decision_cards WHERE state='open' AND created_at<=?1 ORDER BY created_at,seq",
            [now.saturating_sub(STUCK_MS)],
            finding("reconcile_pending", "decisions", "warn", "Decision cards have been pending for over an hour.",
                "Inspect pending basal decision cards with the operator."),
            |r| { let at: i64 = r.get(0)?; Ok((at,json!({"at_ms":at,"flow_id":r.get::<_,String>(1)?,
                "run_id":r.get::<_,Option<String>>(2)?,"position":r.get::<_,Option<i64>>(3)?,"seq":r.get::<_,i64>(4)?}))) }, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT COALESCE(disabled_at,0),flow_id FROM flows WHERE state='disabled' AND disabled_reason='grant_lost' ORDER BY disabled_at,flow_id",
            [], finding("grant_lost", "disabled", "warn", "Flows are disabled because a grant was lost.",
                "Inspect grant status through flow.health; grant references and labels are omitted."),
            |r| { let at: i64 = r.get(0)?; Ok((at, json!({"flow_id":r.get::<_,String>(1)?,"at_ms":at}))) }, Some(now))?);
        let outbox_sample = |r: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, Value)> {
            let at: i64 = r.get(0)?;
            Ok((at, json!({"at_ms":at, "flow_id":r.get::<_,String>(1)?, "seq":r.get::<_,i64>(2)?})))
        };
        findings.extend(aggregate(conn,
            "SELECT at,flow_id,seq,failure_code FROM outbox WHERE delivered_at IS NULL AND kind='flow.owner_notice' AND at<=?1 ORDER BY at,seq",
            [now.saturating_sub(STUCK_MS)],
            finding("owner_notices_stuck", "undelivered", "warn", "Owner notices remain undelivered for over an hour.",
                "Inspect owner-notice delivery and its retry state."),
            |r| { let (at, mut sample) = outbox_sample(r)?;
                sample["failure_code"] = json!(r.get::<_,Option<String>>(3)?);
                Ok((at, sample)) }, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT at,flow_id,seq FROM outbox WHERE delivered_at IS NULL AND kind<>'flow.owner_notice' AND NOT (kind='secret.revoke_flow_grants' AND failed_at IS NOT NULL) AND at<=?1 ORDER BY at,seq",
            [now.saturating_sub(STUCK_MS)],
            finding("outbox_stuck", "undelivered", "warn", "Outbox rows remain undelivered for over an hour.",
                "Inspect basal outbox delivery and bus connectivity."), outbox_sample, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT failed_at,flow_id,seq FROM outbox WHERE delivered_at IS NULL AND kind='secret.revoke_flow_grants' AND failed_at IS NOT NULL ORDER BY failed_at,seq",
            [], finding("outbox_stuck", "secret_revocations", "error", "Secret grant revocations failed permanently; grants may still be live.",
                "Have the operator inspect vault grants and failed basal revocation obligations."), outbox_sample, Some(now))?);
        findings.extend(aggregate(conn,
            "SELECT at,seq FROM audit WHERE action='schedule.corrupt' AND at>=?1 AND at<=?2 ORDER BY at,seq",
            rusqlite::params![now.saturating_sub(window),now],
            finding("quarantined", "schedules_and_fires", "warn", "Corrupt schedules or fires were quarantined in the requested window.",
                "Inspect scheduler quarantine audit entries; corrupt payloads are omitted."),
            |r| { let at: i64 = r.get(0)?; Ok((at,json!({"at_ms":at,"seq":r.get::<_,i64>(1)?,"code":"schedule_corrupt"}))) }, None)?);
        findings.extend(aggregate(conn,
            "SELECT at,run_id,position FROM quarantine WHERE at>=?1 AND at<=?2 ORDER BY at,seq",
            rusqlite::params![now.saturating_sub(window),now],
            finding("quarantined", "outcomes", "warn", "Call outcomes were quarantined in the requested window.",
                "Inspect quarantine records without treating their payloads as trusted outcomes."),
            |r| { let at: i64 = r.get(0)?; Ok((at,json!({"at_ms":at,"run_id":r.get::<_,String>(1)?,"position":r.get::<_,i64>(2)?}))) }, None)?);
        Ok(findings)
    })
}

fn bounded(now: i64, window: i64, mut findings: Vec<Finding>) -> OpResult {
    findings.sort_by(|a, b| {
        (a.severity != "error")
            .cmp(&(b.severity != "error"))
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.check.cmp(b.check))
            .then_with(|| a.key.cmp(&b.key))
    });
    for finding in &mut findings {
        finding.samples.truncate(SAMPLE_CAP);
    }
    let encoded_len = |value: &Value| {
        serde_json::to_vec(value)
            .map(|bytes| bytes.len())
            .map_err(|_| OpError::new("encode_failed", "cannot encode diagnostics"))
    };
    let mut len = encoded_len(&json!({"result":{
        "generated_at_ms":now,"window_ms":window,"findings":findings}}))?;
    while len >= BODY_CAP {
        if let Some(finding) = findings
            .iter_mut()
            .find(|finding| !finding.samples.is_empty())
        {
            let sample = finding.samples.pop().expect("nonempty samples");
            len -= encoded_len(&sample)? + usize::from(!finding.samples.is_empty());
        } else {
            let finding = findings.pop().expect("empty result fits below the cap");
            len -= encoded_len(&json!(finding))? + usize::from(!findings.is_empty());
        }
    }
    Ok(json!({"generated_at_ms":now,"window_ms":window,"findings":findings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const NOW: i64 = 1_777_593_600_000;
    const SECRET: &str = "PRIVATE-provider-error-script-arguments-event-body";

    fn with_store(tag: &str, test: impl FnOnce(&Store)) {
        let dir = basal_testkit::harness::scratch(&format!("diagnostics-{tag}"));
        let store = Store::open(dir.join("basal.db"), basal_core::Durability::throwaway()).unwrap();
        test(&store);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn seed(store: &Store, sql: &str) {
        store
            .write(|tx| {
                tx.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    }

    fn read(store: &Store) -> Value {
        let changes = store.read(|c| Ok(c.total_changes())).unwrap();
        let result = reply(store, NOW, DEFAULT_WINDOW_MS, None, &[]).unwrap();
        assert_eq!(store.read(|c| Ok(c.total_changes())).unwrap(), changes);
        assert_contract(&json!({"result":result}));
        assert!(!result.to_string().contains(SECRET));
        result
    }

    fn quiet(store: &Store) {
        assert_eq!(read(store)["findings"], json!([]));
    }

    fn only(store: &Store, check: &str, key: &str) -> Value {
        let result = read(store);
        let findings = result["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{result}");
        assert_eq!(findings[0]["check"], check);
        assert_eq!(findings[0]["key"], key);
        assert!(matches!(
            findings[0]["severity"].as_str(),
            Some("warn" | "error")
        ));
        findings[0].clone()
    }

    fn run(store: &Store, id: &str, state: &str, at: i64, seq: i64) {
        store.write(|tx| {
            tx.execute("INSERT INTO runs (run_id,flow_id,trigger_id,attempt,trigger,snapshot_id,code_hash,state,admitted_at,admit_seq,ended_at,error_kind) VALUES (?1,'f',?1,1,?2,?8,zeroblob(32),?3,?4,?5,?6,?7)",
                rusqlite::params![id, SECRET, state,at,seq, if state=="failed" {Some(at+1)} else {None}, if state=="failed" {Some("script")} else {None},basal_core::runs::snapshot(tx,"f",None,&[0;32],SECRET,"{}")?])?;
            Ok(())
        }).unwrap();
    }

    #[test]
    fn healthy_store_is_empty_and_read_failures_are_errors() {
        with_store("healthy", |store| {
            quiet(store);
            store.cut();
            let error = reply(store, NOW, 1, None, &[]).unwrap_err();
            assert_eq!(error.code, "storage_unavailable");
        });
    }

    #[test]
    fn event_reader_down_is_an_error_without_private_error_text() {
        with_store("reader", |store| {
            let mut health = events::Health {
                connected: true,
                ..Default::default()
            };
            assert_eq!(
                reply(store, NOW, 1, Some(&health), &[]).unwrap()["findings"],
                json!([])
            );
            health.connected = false;
            health.error = Some(SECRET.into());
            health.retry_in_ms = 30_000;
            let result = reply(store, NOW, 1, Some(&health), &[]).unwrap();
            assert_contract(&json!({"result":result}));
            let findings = result["findings"].as_array().unwrap();
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0]["check"], "event_reader");
            assert_eq!(findings[0]["severity"], "error");
            assert_eq!(findings[0]["count"], 1);
            assert_eq!(findings[0]["samples"][0]["retry_in_ms"], 30_000);
            assert!(!result.to_string().contains(SECRET));
            health.error = None;
            assert_eq!(
                reply(store, NOW, 1, Some(&health), &[]).unwrap()["findings"][0]["samples"][0]["code"],
                "reader_connecting"
            );
        });
    }

    #[test]
    fn failure_streak_and_auto_disable_are_standing_conditions() {
        with_store("failures", |store| {
            seed(
                store,
                "INSERT INTO flows(flow_id,created_at) VALUES ('f',0)",
            );
            run(store, "r1", "failed", NOW - 30_000_000, 1);
            run(store, "r2", "failed", NOW - 29_000_000, 2);
            quiet(store);
            run(store, "r3", "failed", NOW - 28_000_000, 3);
            let finding = only(store, "flows_failing", "f");
            assert_eq!(finding["severity"], "warn");
            assert_eq!(finding["count"], 3);
            assert_eq!(finding["first_seen_ms"], NOW - 30_000_000 + 1);
            seed(
                store,
                "UPDATE flows SET state='disabled',disabled_by='operator',disabled_at=1",
            );
            quiet(store);
            seed(store, "UPDATE flows SET disabled_by='runtime'");
            assert_eq!(only(store, "flows_failing", "f")["severity"], "error");
        });
    }

    #[test]
    fn overdue_deadlines_and_deferred_retries_allow_a_minute_grace() {
        with_store("deadlines", |store| {
            run(store, "r", "pending", NOW - 120_000, 1);
            seed(
                store,
                &format!("UPDATE runs SET deadline_at={}", NOW - 60_000 + 1),
            );
            quiet(store);
            seed(store, "UPDATE runs SET deadline_at=deadline_at-1");
            assert_eq!(only(store, "runs_overdue", "deadline")["count"], 1);
            seed(store, "UPDATE runs SET deadline_at=NULL");
            seed(
                store,
                &format!(
                    "INSERT INTO journal(run_id,position,kind_code,args_bytes,args_digest,idempotency_key,class,dispatch,issued_generation,refusal,retry_not_before,refusal_detail) VALUES ('r',0,1,length(CAST('{SECRET}' AS BLOB)),zeroblob(32),'j','query','deferred',1,'scope_ended',{},'{{\"reason\":\"scope_ended\",\"provider\":\"{SECRET}\",\"action\":\"{SECRET}\"}}'); INSERT INTO journal_payloads(run_id,position,args) VALUES ('r',0,'{SECRET}')",
                    NOW - 60_000 + 1
                ),
            );
            quiet(store);
            seed(
                store,
                "UPDATE journal SET retry_not_before=retry_not_before-1",
            );
            only(store, "runs_overdue", "deferred");
            seed(
                store,
                &format!("UPDATE runs SET state='succeeded',ended_at={NOW}"),
            );
            quiet(store);
        });
    }

    #[test]
    fn reconciliation_uses_admission_age_and_decisions_use_creation_age() {
        with_store("reconcile", |store| {
            run(store, "r", "needs_reconcile", NOW - 3_600_000 + 1, 1);
            quiet(store);
            seed(store, "UPDATE runs SET admitted_at=admitted_at-1");
            only(store, "reconcile_pending", "runs");
            seed(store, "UPDATE runs SET state='pending'");
            seed(
                store,
                &format!(
                    "INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('d','reenable','f',1,1,'{SECRET}','open',{})",
                    NOW - 3_600_000 + 1
                ),
            );
            quiet(store);
            seed(store, "UPDATE decision_cards SET created_at=created_at-1");
            only(store, "reconcile_pending", "decisions");
            seed(
                store,
                &format!("UPDATE decision_cards SET state='applied',answered_at={NOW}"),
            );
            quiet(store);
        });
    }

    #[test]
    fn lost_grant_disable_is_reported_immediately() {
        with_store("grant", |store| {
            seed(
                store,
                "INSERT INTO flows(flow_id,created_at,state,disabled_by,disabled_reason,disabled_at) VALUES ('f',0,'disabled','core','operator',1)",
            );
            quiet(store);
            seed(store, "UPDATE flows SET disabled_reason='grant_lost'");
            let finding = only(store, "grant_lost", "disabled");
            assert_eq!(finding["count"], 1);
            assert_eq!(finding["first_seen_ms"], 1);
            assert_eq!(finding["severity"], "warn");
        });
    }

    #[test]
    fn owner_notices_and_outbox_wait_an_hour_but_failed_revokes_are_immediate() {
        with_store("outbox", |store| {
            for kind in [
                "flow.owner_notice",
                "flow.lifecycle",
                "secret.revoke_flow_grants",
            ] {
                seed(
                    store,
                    &format!(
                        "INSERT INTO outbox(at,kind,flow_id,body) VALUES ({},'{kind}','f','{SECRET}')",
                        NOW - 3_600_000 + 1
                    ),
                );
            }
            quiet(store);
            seed(
                store,
                "UPDATE outbox SET at=at-1 WHERE kind='flow.owner_notice'",
            );
            only(store, "owner_notices_stuck", "undelivered");
            seed(
                store,
                &format!(
                    "UPDATE outbox SET delivered_at={NOW} WHERE kind='flow.owner_notice'; UPDATE outbox SET at=at-1 WHERE kind='flow.lifecycle'"
                ),
            );
            only(store, "outbox_stuck", "undelivered");
            seed(
                store,
                &format!(
                    "UPDATE outbox SET delivered_at={NOW} WHERE kind='flow.lifecycle'; UPDATE outbox SET failed_at={NOW},failure_code='{SECRET}' WHERE kind='secret.revoke_flow_grants'"
                ),
            );
            assert_eq!(
                only(store, "outbox_stuck", "secret_revocations")["severity"],
                "error"
            );
        });
    }

    #[test]
    fn owner_notice_samples_include_only_the_stored_failure_code() {
        with_store("owner-notice-code", |store| {
            seed(
                store,
                &format!(
                    "INSERT INTO outbox(at,kind,flow_id,body,failure_code) VALUES ({},'flow.owner_notice','f','{SECRET}','flow_owner_notice_invalid'),({},'flow.owner_notice','g','{SECRET}',NULL)",
                    NOW - STUCK_MS,
                    NOW - STUCK_MS
                ),
            );
            let finding = only(store, "owner_notices_stuck", "undelivered");
            assert_eq!(
                finding["samples"][0]["failure_code"],
                "flow_owner_notice_invalid"
            );
            assert!(finding["samples"][1]["failure_code"].is_null());
            assert!(!finding.to_string().contains(SECRET));
        });
    }

    #[test]
    fn quarantine_history_respects_window_and_omits_payloads() {
        with_store("quarantine", |store| {
            seed(
                store,
                &format!(
                    "INSERT INTO audit(at,actor,action,detail) VALUES ({},'runtime','schedule.corrupt','{SECRET}')",
                    NOW - DEFAULT_WINDOW_MS - 1
                ),
            );
            quiet(store);
            seed(store, "UPDATE audit SET at=at+1");
            only(store, "quarantined", "schedules_and_fires");
            seed(store, "DELETE FROM audit");
            seed(
                store,
                &format!(
                    "INSERT INTO quarantine(run_id,position,settlement,value,payload_hash,reason,at) VALUES ('r',0,'rejected','{SECRET}',zeroblob(32),'unknown_call',{})",
                    NOW - DEFAULT_WINDOW_MS - 1
                ),
            );
            quiet(store);
            seed(store, "UPDATE quarantine SET at=at+1");
            only(store, "quarantined", "outcomes");
            assert_eq!(
                reply(store, NOW, 1, None, &[]).unwrap()["findings"],
                json!([])
            );
        });
    }

    #[test]
    fn worker_health_backoff_is_reported_without_paths_or_error_text() {
        with_store("worker", |store| {
            let mut pool = crate::pool::PoolStats {
                live: 0,
                spares: 0,
                spawning: 0,
                busy: 0,
                bound: vec![],
                spawn_error: None,
                spawn_retry_in_ms: 0,
            };
            assert_eq!(
                reply(store, NOW, 1, None, &[("flows", &pool)]).unwrap()["findings"],
                json!([])
            );
            for error in [None, Some(SECRET.into())] {
                pool.spawn_error = error;
                pool.spawn_retry_in_ms = 250;
                let result = reply(store, NOW, 1, None, &[("flows", &pool)]).unwrap();
                assert_eq!(result["findings"][0]["check"], "worker_pool");
                assert_eq!(result["findings"][0]["severity"], "warn");
                assert!(!result.to_string().contains(SECRET));
            }
        });
    }

    #[test]
    fn aggregate_counts_are_not_sample_lengths() {
        with_store("counts", |store| {
            for _ in 0..8 {
                seed(
                    store,
                    "INSERT INTO outbox(at,kind,flow_id,body) VALUES (1,'flow.owner_notice','f','{}')",
                );
            }
            let finding = only(store, "owner_notices_stuck", "undelivered");
            assert_eq!(finding["count"], 8);
            assert_eq!(finding["samples"].as_array().unwrap().len(), 5);
        });
    }

    #[test]
    fn windows_and_unknown_params_are_strict() {
        for params in [Value::Null, json!({}), json!({"window_ms":null})] {
            assert_eq!(window(params).unwrap(), 21_600_000);
        }
        for valid in [1, 60_000, 604_800_000] {
            assert_eq!(window(json!({"window_ms":valid})).unwrap(), valid);
        }
        for params in [
            json!({"window_ms":0}),
            json!({"window_ms":-1}),
            json!({"window_ms":604_800_001i64}),
            json!({"window_ms":1.5}),
            json!({"window_ms":"60000"}),
            json!({"agent_id":"head"}),
            json!([]),
        ] {
            assert_eq!(window(params).unwrap_err().code, "invalid_params");
        }
    }

    fn assert_contract(body: &Value) {
        let result = body["result"].as_object().unwrap();
        assert!(result["generated_at_ms"].as_i64().is_some());
        assert!(result["window_ms"].as_i64().is_some());
        for finding in result["findings"].as_array().unwrap() {
            for field in ["check", "key", "severity", "summary", "hint"] {
                assert!(finding[field].as_str().is_some(), "{field}");
            }
            assert!(matches!(
                finding["severity"].as_str(),
                Some("warn" | "warning" | "error")
            ));
            assert!(finding["count"].as_u64().unwrap() > 0);
            for field in ["first_seen_ms", "last_seen_ms"] {
                assert!(finding[field].as_i64().is_some());
            }
            let samples = finding["samples"].as_array().unwrap();
            assert!(samples.len() <= 5);
            assert!(samples.iter().all(Value::is_object));
            assert!(finding.get("next").is_none());
        }
    }

    #[test]
    fn published_samples_are_pinned_and_follow_the_contract() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-vectors/diagnostics-findings-v1");
        for line in include_str!("../../../test-vectors/diagnostics-findings-v1/SHA256SUMS").lines()
        {
            let (digest, name) = line.split_once("  ").unwrap();
            let bytes = std::fs::read(dir.join(name)).unwrap();
            assert_eq!(format!("{:x}", Sha256::digest(&bytes)), digest, "{name}");
            if name.starts_with("reply-") {
                assert_contract(&serde_json::from_slice(&bytes).unwrap());
            } else if name.starts_with("request-") {
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request["method"], "diagnostics.findings");
                window(request["params"].clone()).unwrap();
            }
        }
    }

    #[test]
    fn cap_trims_samples_then_tail_without_changing_counts() {
        let make = |key: String, severity| Finding {
            check: "flows_failing",
            key,
            severity,
            count: 12,
            first_seen_ms: 1,
            last_seen_ms: 2,
            summary: "Repeated failures.",
            samples: vec![json!({"flow_id":"x".repeat(10_000)}); 5],
            hint: "Inspect flow health.",
        };
        let result = bounded(
            3,
            1,
            (0..500)
                .map(|i| {
                    make(
                        format!("flow-{i:04}"),
                        if i == 499 { "error" } else { "warn" },
                    )
                })
                .collect(),
        )
        .unwrap();
        let body = json!({"result":result});
        assert_contract(&body);
        assert!(serde_json::to_vec(&body).unwrap().len() < 65_536);
        let findings = body["result"]["findings"].as_array().unwrap();
        assert!(findings.len() < 500);
        assert_eq!(findings[0]["severity"], "error");
        assert_eq!(findings[0]["key"], "flow-0499");
        for finding in findings {
            assert_eq!(finding["count"], 12);
            assert!(finding["samples"].as_array().unwrap().is_empty());
        }
    }
}
