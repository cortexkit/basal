//! The scheduler: schedule triggers for flows.
//!
//! - [`spec`]: the `trigger.schedule` shape a manifest declares,
//!   [`ScheduleSpec`], and [`validate`], which compiles it.
//! - [`due`]: due times, with the time-zone rules for daylight saving
//!   gaps and overlaps, and interval anchoring.
//! - [`table`]: the `schedules` table and the lifecycle an install path
//!   drives: [`table::approve`] (create or replace), [`table::disable`],
//!   [`table::enable`] and [`table::remove`].
//! - [`tick`]: planning fires for due times that have passed, by the
//!   `missed` policy, and admitting them through trigger admission.
//!
//! [`Scheduler`] bundles these over a [`Store`] with an injectable
//! [`Clock`]. Every operation also exists as a function over a transaction,
//! so a caller can combine it with its own writes in one commit.

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
pub use tick::{AdmittedFire, Planned, PlannedFire, fire_trigger_id};

use crate::admission::Admission;
use crate::error::{CoreError, Result};
use crate::hooks::{NoHooks, Step};
use crate::store::Store;

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
}

impl Scheduler {
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>, config: SchedulerConfig) -> Self {
        Self {
            store,
            clock,
            config,
            hooks: Arc::new(NoHooks),
        }
    }

    /// The same scheduler, stopping where `hooks` says.
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

    /// Records an approved flow version with a schedule trigger (see
    /// [`table::approve`]), then admits any fires the old version owed.
    pub fn approve(&self, flow: &ScheduledFlow) -> Result<Approval> {
        let now = self.now();
        let approval = self
            .store
            .write(|tx| table::approve(tx, flow, now, &self.config))?;
        self.admit_planned(&mut Vec::new())?;
        Ok(approval)
    }

    pub fn disable(&self, flow_id: &str) -> Result<bool> {
        let now = self.now();
        self.store.write(|tx| table::disable(tx, flow_id, now))
    }

    pub fn enable(&self, flow_id: &str) -> Result<bool> {
        let now = self.now();
        self.store.write(|tx| table::enable(tx, flow_id, now))
    }

    pub fn remove(&self, flow_id: &str) -> Result<bool> {
        self.store.write(|tx| table::remove(tx, flow_id))
    }

    pub fn schedule(&self, flow_id: &str) -> Result<Option<ScheduleRow>> {
        self.store.read(|c| table::load(c, flow_id))
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>> {
        self.store.read(table::list)
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
        self.admit_planned(&mut report.admitted)?;
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
        self.admit_planned(&mut report.admitted)?;
        report.waiting = self.store.read(tick::planned_count)?;
        Ok(report)
    }

    /// Admits planned fires oldest first, one commit each, until none is
    /// left or admission is draining.
    fn admit_planned(&self, out: &mut Vec<AdmittedFire>) -> Result<()> {
        let store_id = self.store.store_id().to_owned();
        loop {
            let Some(fire) = self.store.write(|tx| tick::admit_next(tx, &store_id))? else {
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
