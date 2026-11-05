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

#[cfg(any(test, feature = "test-support"))]
pub use config::ManualClock;
pub use config::{Clock, SchedulerConfig, TickHooks, TickPoint};
pub use due::DueError;
pub use spec::{
    CompiledSchedule, DEFAULT_EACH_CAP, MAX_EACH_CAP, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS, Missed,
    MissedPolicy, ScheduleSpec, SpecError, When, validate,
};
pub use table::{Approval, ScheduleRow, ScheduleState, ScheduledFlow};
pub use tick::{AdmittedFire, DroppedFire, Planned, PlannedFire, fire_trigger_id};

use crate::admission::Admission;
use crate::error::Result;

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
    runtime: crate::Runtime,
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    config: SchedulerConfig,

    limits: AdmitLimits,
}

impl Scheduler {
    pub fn new(runtime: crate::Runtime, clock: Arc<dyn Clock>, config: SchedulerConfig) -> Self {
        let store = runtime.shared.store.clone();
        Self {
            runtime,
            store,
            clock,
            config,
            limits: AdmitLimits::default(),
        }
    }

    /// The same scheduler, admitting fires under these limits.
    pub fn with_limits(mut self, limits: AdmitLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
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

    /// Plans and admits the tick in the log-backed admission batch.
    pub fn tick_at(&self, now: Timestamp) -> Result<TickReport> {
        self.runtime.staged_tick_at(now, &self.config, &self.limits)
    }
}
