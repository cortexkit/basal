//! Scheduler settings and the clock it reads.

#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// How late a tick may see a due time and still fire it as on time.
    /// A due time seen later than this (the machine was asleep, basal was
    /// not running, the clock jumped) is missed, and the schedule's
    /// `missed` policy decides what happens to it. So is every due time
    /// that a newer due time has already passed.
    pub grace: Duration,
    /// How many missed due times one tick counts, at most, for a cron
    /// pattern. Counting walks every due time; past this many the count is
    /// reported as a lower bound instead of walking on (a minute-level
    /// pattern reaches the default after about 69 days of missed time).
    pub count_limit: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            count_limit: 100_000,
        }
    }
}

impl SchedulerConfig {
    pub(crate) fn grace(&self) -> SignedDuration {
        SignedDuration::try_from(self.grace).unwrap_or(SignedDuration::MAX)
    }
}

/// Where the scheduler reads the time. The runtime adapts its own clock;
/// tests drive a [`ManualClock`], so no test depends on wall time.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// A clock that moves only when told to.
#[cfg(any(test, feature = "test-support"))]
pub struct ManualClock {
    now: Mutex<Timestamp>,
}

#[cfg(any(test, feature = "test-support"))]
impl ManualClock {
    pub fn new(now: Timestamp) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn set(&self, now: Timestamp) {
        *self.now.lock().unwrap_or_else(|p| p.into_inner()) = now;
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().unwrap_or_else(|p| p.into_inner())
    }
}
