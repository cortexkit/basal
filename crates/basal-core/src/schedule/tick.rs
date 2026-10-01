//! Ticking: turning due times that have passed into admitted runs.
//!
//! A tick works in two kinds of commit, so that no crash can mint a fire
//! twice or lose one:
//!
//! 1. **Plan** (one commit per schedule): find the due times that have
//!    passed, decide the fires by the `missed` policy, store those fires in
//!    `schedule_fires`, and advance the schedule past `now`. Because the
//!    fires and the advance commit together, a restart after this commit
//!    finds the fires waiting and the schedule already advanced, so it
//!    admits exactly those fires and never recomputes a catch-up over the
//!    same due times (a later recomputation would see more missed time and
//!    name a different catch-up fire).
//! 2. **Admit** (one commit per fire): admit the oldest planned fire
//!    through trigger admission and delete it from `schedule_fires`. A
//!    fire's trigger id is its due time, so admitting the same fire again
//!    is a duplicate, never a second run.
//!
//! While admission is draining, planned fires stay in `schedule_fires` and
//! are admitted by the first tick after the drain ends.
//!
//! **Which fires.** The due times in `[next due, now]` are examined. The
//! newest is on time if it is at most the grace period old; every other
//! one is missed (a newer due time has passed, or it is older than the
//! grace period: the machine slept, basal was down, or the clock jumped).
//! An on-time due time always fires. Missed due times follow the policy:
//! `once` makes one catch-up fire identified by the last missed due time,
//! `skip` makes none, `each` makes one per missed due time up to the cap,
//! keeping the newest and admitting them oldest first. Catch-up fires come
//! before the on-time fire.

use basal_proto::JsonText;
use jiff::Timestamp;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use super::config::SchedulerConfig;
use super::due::DueScan;
use super::spec::MissedPolicy;
use super::table::{self, ScheduleState, from_ms, to_ms};
use crate::admission::{self, Admission, TriggerSpec};
use crate::error::{CoreError, Result};

/// The trigger id of the fire for one due time. Admission deduplicates on
/// (flow, trigger id), so a due time can start at most one run of a flow
/// however often it is planned or admitted.
pub fn fire_trigger_id(due: Timestamp) -> String {
    format!("schedule:{due}")
}

/// One fire decided by planning.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFire {
    pub trigger_id: String,
    pub due: Timestamp,
    /// What the script sees as `trigger`.
    pub payload: Value,
}

/// The fires for a scan of due times, oldest first.
pub fn plan_fires(
    scan: &DueScan,
    policy: MissedPolicy,
    now: Timestamp,
    config: &SchedulerConfig,
) -> Result<Vec<PlannedFire>> {
    let Some(&newest) = scan.newest.last() else {
        return Err(CoreError::Corrupt(
            "a due-time scan found no due time".into(),
        ));
    };
    let on_time = now.duration_since(newest) <= config.grace();
    // The missed due times among the newest ones the scan kept, and how
    // many were missed in all.
    let kept_missed: &[Timestamp] = if on_time {
        &scan.newest[..scan.newest.len() - 1]
    } else {
        &scan.newest
    };
    let missed_count = if on_time {
        scan.count.saturating_sub(1)
    } else {
        scan.count
    };
    let mut fires = Vec::new();
    if missed_count > 0 {
        let Some(&last_missed) = kept_missed.last() else {
            return Err(CoreError::Corrupt(
                "a due-time scan kept no missed due time".into(),
            ));
        };
        let mut catch_up = json!({
            "policy": policy.as_str(),
            "missed_count": missed_count,
            "missed_window": {
                "first": scan.first.to_string(),
                "last": last_missed.to_string(),
            },
        });
        if scan.count_is_lower_bound {
            catch_up["missed_count_is_lower_bound"] = json!(true);
        }
        let fire = |due: Timestamp| PlannedFire {
            trigger_id: fire_trigger_id(due),
            due,
            payload: json!({
                "kind": "schedule",
                "due": due.to_string(),
                "catch_up": catch_up.clone(),
            }),
        };
        match policy {
            MissedPolicy::Once => fires.push(fire(last_missed)),
            MissedPolicy::Skip => {}
            MissedPolicy::Each { cap } => {
                let cap = usize::try_from(cap).unwrap_or(usize::MAX);
                let start = kept_missed.len().saturating_sub(cap);
                fires.extend(kept_missed[start..].iter().map(|d| fire(*d)));
            }
        }
    }
    if on_time {
        fires.push(PlannedFire {
            trigger_id: fire_trigger_id(newest),
            due: newest,
            payload: json!({ "kind": "schedule", "due": newest.to_string() }),
        });
    }
    Ok(fires)
}

/// What planning did for one schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub flow_id: String,
    pub fires: Vec<PlannedFire>,
    pub next_due: Option<Timestamp>,
}

/// Plans one schedule up to `now`, in the caller's transaction: stores its
/// fires and advances it. Does nothing (returns `None`) when the schedule
/// does not exist, is disabled, or is not due.
pub fn plan_flow(
    tx: &Transaction,
    flow_id: &str,
    now: Timestamp,
    config: &SchedulerConfig,
) -> Result<Option<Planned>> {
    let Some(row) = table::load(tx, flow_id)? else {
        return Ok(None);
    };
    if row.state != ScheduleState::Active {
        return Ok(None);
    }
    let Some(first) = row.next_due else {
        return Ok(None);
    };
    if first > now {
        return Ok(None);
    }
    let compiled = row.compiled()?;
    // Keep one more than the policy can fire, so the last missed due time
    // is known even when the newest one is on time.
    let keep = match compiled.missed {
        MissedPolicy::Each { cap } => usize::try_from(cap).unwrap_or(usize::MAX - 1) + 1,
        MissedPolicy::Once | MissedPolicy::Skip => 2,
    };
    let scan = compiled
        .scan(first, now, keep, config.count_limit)
        .map_err(|e| CoreError::Corrupt(format!("schedule of {flow_id}: {e}")))?;
    let fires = plan_fires(&scan, compiled.missed, now, config)?;
    let at = to_ms(now);
    for fire in &fires {
        tx.execute(
            "INSERT INTO schedule_fires (flow_id, trigger_id, version, due_ms, payload, script, \
             manifest, planned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (flow_id, trigger_id) DO NOTHING",
            params![
                flow_id,
                fire.trigger_id,
                i64::try_from(row.version).unwrap_or(i64::MAX),
                to_ms(fire.due),
                fire.payload.to_string(),
                row.script,
                row.manifest,
                at
            ],
        )?;
    }
    let last_fired = fires
        .iter()
        .map(|f| f.due)
        .max()
        .or(row.last_fired_due)
        .map(to_ms);
    tx.execute(
        "UPDATE schedules SET next_due_ms = ?2, last_fired_ms = ?3, updated_at = ?4 \
         WHERE flow_id = ?1",
        params![flow_id, scan.next.map(to_ms), last_fired, at],
    )?;
    Ok(Some(Planned {
        flow_id: flow_id.to_owned(),
        fires,
        next_due: scan.next,
    }))
}

/// Active schedules whose next due time is not after `now`.
pub fn due_flows(c: &rusqlite::Connection, now: Timestamp) -> Result<Vec<String>> {
    let mut stmt = c.prepare(
        "SELECT flow_id FROM schedules WHERE state = 'active' AND next_due_ms IS NOT NULL \
         AND next_due_ms <= ?1 ORDER BY next_due_ms, flow_id",
    )?;
    let ids = stmt
        .query_map([to_ms(now)], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// A planned fire that went through admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedFire {
    pub flow_id: String,
    pub trigger_id: String,
    pub due: Timestamp,
    pub admission: Admission,
}

/// Admits the oldest planned fire, in the caller's transaction, and
/// removes it from the planned fires unless admission is draining.
/// `None` when nothing is planned.
pub fn admit_next(tx: &Transaction, store_id: &str) -> Result<Option<AdmittedFire>> {
    let row = tx
        .query_row(
            "SELECT seq, flow_id, trigger_id, due_ms, payload, script, manifest \
             FROM schedule_fires ORDER BY seq LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((seq, flow_id, trigger_id, due, payload, script, manifest)) = row else {
        return Ok(None);
    };
    let trigger = JsonText::new(payload)
        .map_err(|e| CoreError::Corrupt(format!("planned fire {trigger_id}: {e:?}")))?;
    let spec = TriggerSpec {
        flow_id: flow_id.clone(),
        trigger_id: trigger_id.clone(),
        trigger,
        script,
        manifest,
    };
    let admission = admission::admit(tx, store_id, &spec)?;
    if admission != Admission::Draining {
        tx.execute("DELETE FROM schedule_fires WHERE seq = ?1", [seq])?;
    }
    Ok(Some(AdmittedFire {
        flow_id,
        trigger_id,
        due: from_ms(due, "planned fire due time")?,
        admission,
    }))
}

/// How many fires are planned and not yet admitted.
pub fn planned_count(c: &rusqlite::Connection) -> Result<u64> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM schedule_fires", [], |r| r.get(0))?;
    u64::try_from(n).map_err(|_| CoreError::Corrupt(format!("planned fire count {n}")))
}
