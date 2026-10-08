//! The time basal's own limits are measured on: token windows, rate
//! windows and run deadlines.
//!
//! It is separate from the clock a script reads (`Date.now()`, which comes
//! from the host and is journaled): these decisions are the runtime's, and
//! tests drive them by setting the time rather than by waiting for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::store::system_now_ms;
use std::cell::Cell;

thread_local! {
    // A transaction samples its store's clock once. Keeping the scope on the
    // calling thread also lets low-level state transitions share that sample
    // without giving a second store or another thread the wrong clock.
    static WRITE_TIME: Cell<Option<i64>> = const { Cell::new(None) };
}

pub(crate) fn write_time() -> Option<i64> {
    WRITE_TIME.with(Cell::get)
}

pub(crate) struct WriteTime(Option<i64>);

impl WriteTime {
    pub(crate) fn enter(now: i64) -> Self {
        Self(WRITE_TIME.with(|time| time.replace(Some(now))))
    }
}

impl Drop for WriteTime {
    fn drop(&mut self) {
        WRITE_TIME.with(|time| time.set(self.0));
    }
}

/// Wall time in milliseconds since the Unix epoch, or a manual time a test
/// sets. Cloning shares the same manual time.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    manual: Option<Arc<AtomicI64>>,
}

impl Clock {
    /// The system's wall clock.
    pub fn system() -> Self {
        Self { manual: None }
    }

    /// A clock that reads `start_ms` until it is moved.
    pub fn manual(start_ms: i64) -> Self {
        Self {
            manual: Some(Arc::new(AtomicI64::new(start_ms))),
        }
    }

    pub fn now_ms(&self) -> i64 {
        match &self.manual {
            Some(t) => t.load(Ordering::SeqCst),
            None => system_now_ms(),
        }
    }

    /// Sets a manual clock. A system clock ignores it.
    pub fn set(&self, ms: i64) {
        if let Some(t) = &self.manual {
            t.store(ms, Ordering::SeqCst);
        }
    }

    /// Moves a manual clock forward. A system clock ignores it.
    pub fn advance(&self, ms: i64) {
        if let Some(t) = &self.manual {
            t.fetch_add(ms, Ordering::SeqCst);
        }
    }
}
