//! The `schedules` table and a schedule's lifecycle: approve (create or
//! replace), disable, enable and remove.
//!
//! One row per flow: the approved version whose manifest has a schedule
//! trigger, its spec and code, whether it is active, the instant intervals
//! count from, the next due time and the last due time that fired. Every
//! function here takes the caller's transaction, so an install path can
//! record an approval and its schedule in one commit.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::config::SchedulerConfig;
use super::spec::{CompiledSchedule, ScheduleSpec, validate};
use super::tick;
use crate::error::{CoreError, Result};
use crate::ids::code_hash;

/// An approved flow version whose trigger is a schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledFlow {
    pub flow_id: String,
    pub version: u64,
    pub spec: ScheduleSpec,
    /// The approved script, snapshotted into every run a fire admits.
    pub script: String,
    /// The approved manifest bytes, snapshotted likewise.
    pub manifest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleState {
    Active,
    /// The flow is disabled: the schedule keeps its row but never ticks.
    Disabled,
}

impl ScheduleState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            other => Err(CoreError::Corrupt(format!("schedule state {other:?}"))),
        }
    }
}

/// A schedule as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRow {
    pub flow_id: String,
    pub version: u64,
    pub spec: ScheduleSpec,
    pub script: String,
    pub manifest: String,
    pub state: ScheduleState,
    /// When this version's schedule was created; intervals count from it.
    pub anchor: Timestamp,
    /// The next due time; `None` when the pattern never matches again.
    pub next_due: Option<Timestamp>,
    /// The newest due time that produced a fire, across versions.
    pub last_fired_due: Option<Timestamp>,
}

impl ScheduleRow {
    /// The stored spec, compiled. It was validated when it was approved, so
    /// a failure here means the store holds something this build did not
    /// write.
    pub fn compiled(&self) -> Result<CompiledSchedule> {
        validate(&self.spec)
            .map_err(|e| CoreError::Corrupt(format!("schedule of {}: {e}", self.flow_id)))
    }
}

/// What an approval did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// The flow had no schedule; it has one now.
    Created,
    /// An older version's schedule was replaced.
    Replaced { previous_version: u64 },
    /// The same version with the same code was approved again; nothing
    /// changed.
    Unchanged,
}

pub(crate) fn to_ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}

pub(crate) fn from_ms(ms: i64, what: &str) -> Result<Timestamp> {
    Timestamp::from_millisecond(ms).map_err(|e| CoreError::Corrupt(format!("{what} {ms}: {e}")))
}

fn version_sql(version: u64) -> Result<i64> {
    i64::try_from(version).map_err(|_| CoreError::Invalid(format!("version {version} too large")))
}

/// The instant a new schedule counts from: the approval, to the whole
/// second, so interval due times read cleanly in trigger ids.
fn anchor_at(now: Timestamp) -> Result<Timestamp> {
    let secs = now.as_second();
    // A timestamp before 1970 with a fractional second floors downwards.
    let secs = if now.subsec_nanosecond() < 0 {
        secs - 1
    } else {
        secs
    };
    Timestamp::from_second(secs).map_err(|e| CoreError::Invalid(format!("anchor {now}: {e}")))
}

fn due_error(flow_id: &str, e: super::due::DueError) -> CoreError {
    CoreError::Corrupt(format!("schedule of {flow_id}: {e}"))
}

pub fn load(c: &Connection, flow_id: &str) -> Result<Option<ScheduleRow>> {
    let raw = c
        .query_row(
            "SELECT version, spec, script, manifest, state, anchor_ms, next_due_ms, last_fired_ms \
             FROM schedules WHERE flow_id = ?1",
            [flow_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((version, spec, script, manifest, state, anchor, next_due, last_fired)) = raw else {
        return Ok(None);
    };
    let version = u64::try_from(version)
        .map_err(|_| CoreError::Corrupt(format!("schedule version {version}")))?;
    let spec: ScheduleSpec = serde_json::from_str(&spec)
        .map_err(|e| CoreError::Corrupt(format!("schedule spec of {flow_id}: {e}")))?;
    Ok(Some(ScheduleRow {
        flow_id: flow_id.to_owned(),
        version,
        spec,
        script,
        manifest,
        state: ScheduleState::parse(&state)?,
        anchor: from_ms(anchor, "schedule anchor")?,
        next_due: next_due
            .map(|ms| from_ms(ms, "next due time"))
            .transpose()?,
        last_fired_due: last_fired
            .map(|ms| from_ms(ms, "last fired due time"))
            .transpose()?,
    }))
}

/// Every flow with a schedule, in flow id order.
pub fn list(c: &Connection) -> Result<Vec<ScheduleRow>> {
    let mut stmt = c.prepare("SELECT flow_id FROM schedules ORDER BY flow_id")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(row) = load(c, &id)? {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Records an approved flow version with a schedule trigger.
///
/// - No schedule yet: one is created, anchored at `now`, first due after
///   `now`.
/// - An older version's schedule: due times of the old version that have
///   already passed are planned first, under the old version's code (they
///   were due while it was the approved one). Then the new version takes
///   over, anchored at `now` and first due after `now`: the new pattern's
///   due times between the old schedule's last fire and the approval never
///   fire, because the new version was not approved at those times. The
///   schedule's state (active or disabled) is kept.
/// - The same version and code again: nothing changes, so a retried
///   install is harmless. The same version with different code, or an
///   older version, is refused.
pub fn approve(
    tx: &Transaction,
    flow: &ScheduledFlow,
    now: Timestamp,
    config: &SchedulerConfig,
) -> Result<Approval> {
    let compiled =
        validate(&flow.spec).map_err(|e| CoreError::Invalid(format!("{}: {e}", flow.flow_id)))?;
    let version = version_sql(flow.version)?;
    let spec = serde_json::to_string(&flow.spec)
        .map_err(|e| CoreError::Invalid(format!("schedule spec: {e}")))?;
    let hash = code_hash(&flow.script, &flow.manifest);
    let anchor = anchor_at(now)?;
    let next_due_after = |after: Timestamp| -> Result<Option<i64>> {
        Ok(compiled
            .next_due_after(anchor, after)
            .map_err(|e| due_error(&flow.flow_id, e))?
            .map(to_ms))
    };

    let Some(old) = load(tx, &flow.flow_id)? else {
        let next_due = next_due_after(now)?;
        tx.execute(
            "INSERT INTO schedules (flow_id, version, spec, script, manifest, code_hash, state, \
             anchor_ms, next_due_ms, last_fired_ms, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, NULL, ?9)",
            params![
                flow.flow_id,
                version,
                spec,
                flow.script,
                flow.manifest,
                hash.as_slice(),
                to_ms(anchor),
                next_due,
                to_ms(now)
            ],
        )?;
        return Ok(Approval::Created);
    };

    if flow.version < old.version {
        return Err(CoreError::Invalid(format!(
            "{}: version {} is older than the approved version {}",
            flow.flow_id, flow.version, old.version
        )));
    }
    if flow.version == old.version {
        let same = old.spec == flow.spec && code_hash(&old.script, &old.manifest) == hash;
        return if same {
            Ok(Approval::Unchanged)
        } else {
            Err(CoreError::Invalid(format!(
                "{}: version {} is already approved with different code",
                flow.flow_id, flow.version
            )))
        };
    }

    // Plan the old version's due times that have already passed, under the
    // old version's code, before its schedule is replaced: they fell due
    // while it was the approved version.
    if old.state == ScheduleState::Active {
        tick::plan_flow(tx, &flow.flow_id, now, config)?;
    }
    // The new version counts from its own approval. Its pattern's due times
    // between the old version's last fire and now were never its to fire,
    // so the swap itself creates no missed time.
    let counted_from = now;
    let next_due = next_due_after(counted_from)?;
    tx.execute(
        "UPDATE schedules SET version = ?2, spec = ?3, script = ?4, manifest = ?5, \
         code_hash = ?6, anchor_ms = ?7, next_due_ms = ?8, updated_at = ?9 WHERE flow_id = ?1",
        params![
            flow.flow_id,
            version,
            spec,
            flow.script,
            flow.manifest,
            hash.as_slice(),
            to_ms(anchor),
            next_due,
            to_ms(now)
        ],
    )?;
    Ok(Approval::Replaced {
        previous_version: old.version,
    })
}

/// Stops a flow's schedule. Due times passing while it is disabled never
/// fire, and fires planned but not yet admitted are dropped: disabling
/// fences new work. Returns whether the flow had a schedule.
pub fn disable(tx: &Transaction, flow_id: &str, now: Timestamp) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE schedules SET state = 'disabled', updated_at = ?2 WHERE flow_id = ?1",
        params![flow_id, to_ms(now)],
    )?;
    tx.execute("DELETE FROM schedule_fires WHERE flow_id = ?1", [flow_id])?;
    Ok(changed > 0)
}

/// Starts a disabled schedule again. Its next due time is the first after
/// `now`: the time it spent disabled is not missed time, so nothing catches
/// up. An interval keeps its original anchor. Returns whether the flow had
/// a schedule.
pub fn enable(tx: &Transaction, flow_id: &str, now: Timestamp) -> Result<bool> {
    let Some(row) = load(tx, flow_id)? else {
        return Ok(false);
    };
    if row.state == ScheduleState::Active {
        return Ok(true);
    }
    let next_due = row
        .compiled()?
        .next_due_after(row.anchor, now)
        .map_err(|e| due_error(flow_id, e))?
        .map(to_ms);
    tx.execute(
        "UPDATE schedules SET state = ?2, next_due_ms = ?3, updated_at = ?4 WHERE flow_id = ?1",
        params![
            flow_id,
            ScheduleState::Active.as_str(),
            next_due,
            to_ms(now)
        ],
    )?;
    Ok(true)
}

/// Deletes a flow's schedule and any fires planned but not yet admitted.
/// Used when a flow is removed, or when a newer approved version has no
/// schedule trigger. Returns whether the flow had a schedule.
pub fn remove(tx: &Transaction, flow_id: &str) -> Result<bool> {
    tx.execute("DELETE FROM schedule_fires WHERE flow_id = ?1", [flow_id])?;
    let removed = tx.execute("DELETE FROM schedules WHERE flow_id = ?1", [flow_id])?;
    Ok(removed > 0)
}
