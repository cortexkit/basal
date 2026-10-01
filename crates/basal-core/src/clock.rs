//! The time basal's own limits are measured on: token windows, rate
//! windows and run deadlines.
//!
//! It is separate from the clock a script reads (`Date.now()`, which comes
//! from the host and is journaled): these decisions are the runtime's, and
//! tests drive them by setting the time rather than by waiting for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::store::now_ms;

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
            None => now_ms(),
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
