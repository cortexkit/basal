//! The scheduler: schedule triggers for flows.
//!
//! - [`spec`]: the `trigger.schedule` shape a manifest declares,
//!   [`ScheduleSpec`], and [`validate`], which compiles it.
//! - [`due`]: due times, with the time-zone rules for daylight saving
//!   gaps and overlaps, and interval anchoring.
//! - [`table`]: the `schedules` table and the lifecycle the install path
//!   drives in its own transactions ([`crate::install`]): [`table::approve`]
//!   (create or replace) when a version with a schedule trigger is
//!   approved, [`table::remove`] when a newer version without one is, and
//!   [`table::disable`] and [`table::enable`] with the flow.
//! - [`tick`]: planning fires for due times that have passed, by the
//!   `missed` policy, and admitting them through trigger admission.
//!
//! [`Scheduler`] ticks over a [`Store`] with an injectable [`Clock`]
//! (`Runtime::scheduler` builds one sharing the runtime's clock and
//! admission limits). Every operation also exists as a function over a
//! transaction, so a caller can combine it with its own writes in one
//! commit.

pub mod config;
pub mod due;
pub mod spec;
pub mod table;
pub mod tick;

use std::sync::Arc;

use jiff::Timestamp;

pub use config::{Clock, ManualClock, SchedulerConfig, SystemClock, TickHooks, TickPoint};
pub use due::DueError;
pub use spec::{
    CompiledSchedule, DEFAULT_EACH_CAP, MAX_EACH_CAP, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS, Missed,
    MissedPolicy, ScheduleSpec, SpecError, When, validate,
};
pub use table::{Approval, ScheduleRow, ScheduleState, ScheduledFlow};
pub use tick::{AdmittedFire, DroppedFire, Planned, PlannedFire, fire_trigger_id};

use crate::admission::{Admission, AdmitContext};
use crate::error::{CoreError, Result};
use crate::hooks::{NoHooks, Step};
use crate::rate::RateLimits;
use crate::store::Store;

/// The runtime's clock reads as the scheduler's, so ticks, approvals and
/// admission limits all see one time.
impl Clock for crate::clock::Clock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millisecond(self.now_ms()).unwrap_or(Timestamp::UNIX_EPOCH)
    }
}

/// The admission limits a tick admits fires under: the runtime's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmitLimits {
    pub rate: RateLimits,
    /// The deadline of a run whose manifest sets none, in milliseconds.
    pub default_deadline_ms: i64,
}

impl Default for AdmitLimits {
    fn default() -> Self {
        Self {
            rate: RateLimits::default(),
            default_deadline_ms: 600_000,
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickReport {
    /// Schedules planned in this tick, with their fires.
    pub planned: Vec<Planned>,
    /// Fires that went through admission in this tick, in order. Includes
    /// fires planned by an earlier tick that a crash or a drain left behind.
    pub admitted: Vec<AdmittedFire>,
    /// Fires still planned and not admitted (admission is draining).
    pub waiting: u64,
}

impl TickReport {
    /// The run ids of fires that started new runs.
    pub fn new_runs(&self) -> Vec<&str> {
        self.admitted
            .iter()
            .filter_map(|f| match &f.admission {
                Admission::Admitted { run_id } => Some(run_id.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// The scheduler over one store.
pub struct Scheduler {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    config: SchedulerConfig,
    hooks: Arc<dyn TickHooks>,
    limits: AdmitLimits,
}

impl Scheduler {
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>, config: SchedulerConfig) -> Self {
        Self {
            store,
            clock,
            config,
            hooks: Arc::new(NoHooks),
            limits: AdmitLimits::default(),
        }
    }

    /// The same scheduler, admitting fires under these limits.
    pub fn with_limits(mut self, limits: AdmitLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The same scheduler, asking `hooks` at every commit boundary of a tick
    /// whether to carry on or to stop as if the process had died there.
    pub fn with_hooks(mut self, hooks: Arc<dyn TickHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn at(&self, point: TickPoint) -> Result<()> {
        match self.hooks.at(&point) {
            Step::Continue => Ok(()),
            Step::Crash => {
                self.store.cut();
                Err(CoreError::Cut)
            }
        }
    }

    pub fn schedule(&self, flow_id: &str) -> Result<Option<ScheduleRow>> {
        self.store.read(|c| table::load(c, flow_id))
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>> {
        self.store.read(table::list)
    }

    /// Planned fires admission refused for good, oldest first.
    pub fn dropped(&self) -> Result<Vec<DroppedFire>> {
        self.store.read(tick::dropped)
    }

    /// Ticks at the clock's current time.
    pub fn tick(&self) -> Result<TickReport> {
        self.tick_at(self.now())
    }

    /// Plans every active schedule due at `now` and admits every planned
    /// fire. Fires left planned by an interrupted earlier tick are admitted
    /// first, so they keep their place ahead of newer ones. Ticking again
    /// with the same `now` admits nothing new.
    pub fn tick_at(&self, now: Timestamp) -> Result<TickReport> {
        let mut report = TickReport::default();
        // While admission drains, planning still runs: fires are decided
        // when they fall due (so they count as on time) and wait in the
        // planned fires until admission resumes.
        self.admit_planned(now, &mut report.admitted)?;
        let due = self.store.read(|c| tick::due_flows(c, now))?;
        for flow_id in due {
            let planned = self
                .store
                .write(|tx| tick::plan_flow(tx, &flow_id, now, &self.config))?;
            if let Some(planned) = planned {
                report.planned.push(planned);
                self.at(TickPoint::Planned {
                    flow_id: flow_id.clone(),
                })?;
            }
        }
        self.admit_planned(now, &mut report.admitted)?;
        report.waiting = self.store.read(tick::planned_count)?;
        Ok(report)
    }

    /// Admits planned fires oldest first, one commit each, until none is
    /// left or admission is draining.
    fn admit_planned(&self, now: Timestamp, out: &mut Vec<AdmittedFire>) -> Result<()> {
        let store_id = self.store.store_id().to_owned();
        let ctx = AdmitContext {
            now_ms: now.as_millisecond(),
            rate: self.limits.rate,
            default_deadline_ms: self.limits.default_deadline_ms,
        };
        loop {
            let Some(fire) = self
                .store
                .write(|tx| tick::admit_next(tx, &store_id, &ctx))?
            else {
                return Ok(());
            };
            if fire.admission == Admission::Draining {
                return Ok(());
            }
            let point = TickPoint::Admitted {
                flow_id: fire.flow_id.clone(),
                trigger_id: fire.trigger_id.clone(),
            };
            out.push(fire);
            self.at(point)?;
        }
    }
}
