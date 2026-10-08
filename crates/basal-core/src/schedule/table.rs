//! The `schedules` table and a schedule's lifecycle: approve (create or
//! replace), disable, enable and remove.
//!
//! One row per flow: the approved version whose manifest has a schedule
//! trigger, its spec, whether it is active, the instant intervals count
//! from, the next due time and the last due time that fired. The row keeps
//! no code: a fire runs the flow's approved version at the moment it is
//! admitted. Every function here takes the caller's transaction, and the
//! install path calls them in the transactions that approve, disable and
//! enable the flow.

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::config::SchedulerConfig;
use super::spec::{CompiledSchedule, ScheduleSpec, validate};
use super::tick;
use crate::error::{CoreError, Result};

/// An approved flow version whose trigger is a schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledFlow {
    pub flow_id: String,
    pub version: u64,
    pub spec: ScheduleSpec,
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
        type Cache = std::collections::VecDeque<(ScheduleSpec, CompiledSchedule)>;
        static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some((_, compiled)) = cache.iter().find(|(spec, _)| spec == &self.spec) {
            return Ok(compiled.clone());
        }
        let compiled = validate(&self.spec)
            .map_err(|e| CoreError::Corrupt(format!("schedule of {}: {e}", self.flow_id)))?;
        // A compiled spec depends only on the spec itself: the validation
        // rules and the bundled timezone database never change while the
        // process runs, so the cache is keyed by the exact spec and never
        // needs invalidating. Only valid specs are cached, so a bad one stays
        // an error every time. The cache is one static for the whole process,
        // shared by every flow and store, so its size is a fixed number of
        // specs (oldest dropped first) rather than one entry per flow: memory
        // stays bounded however many flows there are or however often their
        // schedules change.
        const MAX_COMPILED_SPECS: usize = 128;
        if cache.len() == MAX_COMPILED_SPECS {
            cache.pop_front();
        }
        cache.push_back((self.spec.clone(), compiled.clone()));
        Ok(compiled)
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

const COLUMNS: &str = "flow_id, version, spec, state, anchor_ms, next_due_ms, last_fired_ms";

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ScheduleRow>> {
    let flow_id: String = row.get(0)?;
    let version: i64 = row.get(1)?;
    let spec: String = row.get(2)?;
    let state: String = row.get(3)?;
    let anchor: i64 = row.get(4)?;
    let next_due: Option<i64> = row.get(5)?;
    let last_fired: Option<i64> = row.get(6)?;
    Ok((|| {
        Ok(ScheduleRow {
            version: u64::try_from(version)
                .map_err(|_| CoreError::Corrupt(format!("schedule version {version}")))?,
            spec: serde_json::from_str(&spec)
                .map_err(|e| CoreError::Corrupt(format!("schedule spec of {flow_id}: {e}")))?,
            flow_id,
            state: ScheduleState::parse(&state)?,
            anchor: from_ms(anchor, "schedule anchor")?,
            next_due: next_due
                .map(|ms| from_ms(ms, "next due time"))
                .transpose()?,
            last_fired_due: last_fired
                .map(|ms| from_ms(ms, "last fired due time"))
                .transpose()?,
        })
    })())
}

pub fn load(c: &Connection, flow_id: &str) -> Result<Option<ScheduleRow>> {
    c.query_row(
        &format!("SELECT {COLUMNS} FROM schedules WHERE flow_id=?1"),
        [flow_id],
        decode_row,
    )
    .optional()?
    .transpose()
}

/// Every flow with a schedule, in flow id order, in one query.
pub fn list(c: &Connection) -> Result<Vec<ScheduleRow>> {
    let mut stmt = c.prepare(&format!("SELECT {COLUMNS} FROM schedules ORDER BY flow_id"))?;
    let rows = stmt
        .query_map([], decode_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().collect()
}

/// Records an approved flow version with a schedule trigger.
///
/// - No schedule yet: one is created, anchored at `now`, first due after
///   `now`.
/// - An older version's schedule: due times of the old version that have
///   already passed are planned first (they fell due while it was the
///   approved one, so they are owed). Like every fire, they run the version
///   approved when they are admitted, which is the new one. Then the new
///   version takes over, anchored at `now` and first due after `now`: the
///   new pattern's due times between the old schedule's last fire and the
///   approval never fire, because the new version was not approved at
///   those times. The schedule's state (active or disabled) is kept.
/// - The same version again: nothing changes, so a retried approval is
///   harmless (installs refuse the same version with other code). An older
///   version is refused.
pub fn approve(
    tx: &Transaction,
    flow: &ScheduledFlow,
    now: Timestamp,
    config: &SchedulerConfig,
) -> Result<Approval> {
    approve_inner(tx, flow, now, config, true)
}

/// Like `approve`, but a package instance may move to a lower version: core
/// orders instance changes by its own per-instance generation number, and a
/// persona can legitimately pin an older package version.
pub(crate) fn approve_instance(
    tx: &Transaction,
    flow: &ScheduledFlow,
    now: Timestamp,
    config: &SchedulerConfig,
) -> Result<Approval> {
    approve_inner(tx, flow, now, config, false)
}

fn approve_inner(
    tx: &Transaction,
    flow: &ScheduledFlow,
    now: Timestamp,
    config: &SchedulerConfig,
    monotonic: bool,
) -> Result<Approval> {
    let compiled =
        validate(&flow.spec).map_err(|e| CoreError::Invalid(format!("{}: {e}", flow.flow_id)))?;
    let version = version_sql(flow.version)?;
    let spec = serde_json::to_string(&flow.spec)
        .map_err(|e| CoreError::Invalid(format!("schedule spec: {e}")))?;
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
            "INSERT INTO schedules (flow_id, version, spec, state, anchor_ms, next_due_ms, \
             last_fired_ms, updated_at) VALUES (?1, ?2, ?3, 'active', ?4, ?5, NULL, ?6)",
            params![
                flow.flow_id,
                version,
                spec,
                to_ms(anchor),
                next_due,
                to_ms(now)
            ],
        )?;
        return Ok(Approval::Created);
    };

    if monotonic && flow.version < old.version {
        return Err(CoreError::Invalid(format!(
            "{}: version {} is older than the approved version {}",
            flow.flow_id, flow.version, old.version
        )));
    }
    if flow.version == old.version {
        return if old.spec == flow.spec {
            Ok(Approval::Unchanged)
        } else {
            Err(CoreError::Invalid(format!(
                "{}: version {} already has a different schedule",
                flow.flow_id, flow.version
            )))
        };
    }

    // Plan the old version's due times that have already passed before its
    // schedule is replaced: they fell due while it was the approved version,
    // so they are owed. They run whatever version is approved when they are
    // admitted.
    if old.state == ScheduleState::Active {
        tick::plan_flow(tx, &flow.flow_id, now, config)?;
    }
    // The new version counts from its own approval. Its pattern's due times
    // between the old version's last fire and now were never its to fire,
    // so the swap itself creates no missed time.
    let counted_from = now;
    let next_due = next_due_after(counted_from)?;
    tx.execute(
        "UPDATE schedules SET version = ?2, spec = ?3, anchor_ms = ?4, next_due_ms = ?5, \
         updated_at = ?6 WHERE flow_id = ?1",
        params![
            flow.flow_id,
            version,
            spec,
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
/// fire, and fires planned but not yet admitted are deleted without individual
/// `schedule_dropped` records: the flow's disable explains the discarded work.
/// Disabling fences new work. Returns whether the flow had a schedule.
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
