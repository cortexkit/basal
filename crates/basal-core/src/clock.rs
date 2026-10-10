//! The time basal's own limits are measured on: token windows, rate
//! windows and run deadlines.
//!
//! It is separate from the clock a script reads (`Date.now()`, which comes
//! from the host and is journaled): these decisions are the runtime's, and
//! tests drive them by setting the time rather than by waiting for it.

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

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

/// Called after a manual clock moves.
pub(crate) type Watcher = Arc<dyn Fn() + Send + Sync>;

/// Wall time in milliseconds since the Unix epoch, or a manual time a test
/// sets. Cloning shares the same manual time.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    manual: Option<Arc<Manual>>,
}

struct Manual {
    now: AtomicI64,
    /// Weak so that a sleeper which has finished is dropped from the list
    /// instead of being kept alive by the clock.
    watchers: Mutex<Vec<Weak<dyn Fn() + Send + Sync>>>,
}

impl fmt::Debug for Manual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Manual")
            .field(&self.now.load(Ordering::SeqCst))
            .finish()
    }
}

impl Manual {
    fn moved(&self) {
        // Call the watchers outside the lock, so a watcher may itself read
        // the clock or register another watcher.
        let live: Vec<Watcher> = {
            let mut watchers = self.watchers.lock().unwrap_or_else(|p| p.into_inner());
            watchers.retain(|w| w.strong_count() > 0);
            watchers.iter().filter_map(Weak::upgrade).collect()
        };
        for watcher in live {
            watcher();
        }
    }
}

impl Clock {
    /// The system's wall clock.
    pub fn system() -> Self {
        Self { manual: None }
    }

    /// A clock that reads `start_ms` until it is moved.
    pub fn manual(start_ms: i64) -> Self {
        Self {
            manual: Some(Arc::new(Manual {
                now: AtomicI64::new(start_ms),
                watchers: Mutex::new(Vec::new()),
            })),
        }
    }

    pub fn now_ms(&self) -> i64 {
        match &self.manual {
            Some(t) => t.now.load(Ordering::SeqCst),
            None => system_now_ms(),
        }
    }

    /// Sets a manual clock. A system clock ignores it.
    pub fn set(&self, ms: i64) {
        if let Some(t) = &self.manual {
            t.now.store(ms, Ordering::SeqCst);
            t.moved();
        }
    }

    /// Moves a manual clock forward. A system clock ignores it.
    pub fn advance(&self, ms: i64) {
        if let Some(t) = &self.manual {
            t.now.fetch_add(ms, Ordering::SeqCst);
            t.moved();
        }
    }

    /// Calls `watcher` after every later move of a manual clock, until the
    /// caller drops its last reference to `watcher`. A thread that sleeps
    /// until a deadline measured on this clock uses it to wake when a test
    /// moves the time, since a manual clock does not move with real time.
    /// A system clock is never moved this way, so it records nothing.
    pub(crate) fn watch(&self, watcher: &Watcher) {
        if let Some(t) = &self.manual {
            let mut watchers = t.watchers.lock().unwrap_or_else(|p| p.into_inner());
            watchers.retain(|w| w.strong_count() > 0);
            watchers.push(Arc::downgrade(watcher));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn manual_moves_call_live_watchers_and_forget_dropped_ones() {
        let clock = Clock::manual(0);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let watcher: Watcher = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        clock.watch(&watcher);
        clock.set(5);
        clock.clone().advance(1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(watcher);
        clock.advance(1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let watchers = clock.manual.as_ref().unwrap().watchers.lock().unwrap();
        assert!(watchers.is_empty(), "a dropped watcher must not be kept");
    }

    #[test]
    fn a_system_clock_keeps_no_watchers() {
        let watcher: Watcher = Arc::new(|| panic!("a system clock never calls watchers"));
        let clock = Clock::system();
        clock.watch(&watcher);
        clock.set(5);
        assert!(clock.manual.is_none());
        assert_eq!(Arc::strong_count(&watcher), 1);
        assert_eq!(Arc::weak_count(&watcher), 0);
    }
}
