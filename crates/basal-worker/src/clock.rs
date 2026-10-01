//! The per-activation JavaScript time budget.
//!
//! The budget counts time spent inside the engine only. It is measured with
//! the worker thread's CPU clock, sampled when the worker enters the engine
//! and when it leaves, and paused around every wait on the parent. CPU time
//! rather than wall time keeps the budget from being spent by a busy machine
//! descheduling the worker, and pausing around waits keeps a slow host call
//! from spending it at all.

use std::cell::Cell;
use std::time::Duration;

/// The calling thread's CPU time.
///
/// `CLOCK_THREAD_CPUTIME_ID` is supported on every macOS release this worker
/// targets and on Linux; if the call ever fails, the budget falls back to
/// treating the clock as stopped, and the parent's wall-clock deadline (which
/// kills the worker) remains the backstop.
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return Duration::ZERO;
    }
    Duration::new(
        ts.tv_sec.max(0) as u64,
        ts.tv_nsec.clamp(0, 999_999_999) as u32,
    )
}

/// Engine time spent so far and the limit it is held to.
#[derive(Debug)]
pub struct JsClock {
    budget: Duration,
    spent: Cell<Duration>,
    entered_at: Cell<Option<Duration>>,
    exhausted: Cell<bool>,
}

impl JsClock {
    pub fn new(budget: Duration) -> Self {
        Self {
            budget,
            spent: Cell::new(Duration::ZERO),
            entered_at: Cell::new(None),
            exhausted: Cell::new(false),
        }
    }

    /// Starts counting: the worker is about to run JavaScript.
    pub fn enter(&self) {
        if self.entered_at.get().is_none() {
            self.entered_at.set(Some(thread_cpu_time()));
        }
    }

    /// Stops counting: control has returned to the worker, or the worker is
    /// about to wait on the parent.
    pub fn leave(&self) {
        if let Some(start) = self.entered_at.take() {
            let now = thread_cpu_time();
            self.spent.set(self.spent.get() + now.saturating_sub(start));
        }
    }

    pub fn spent(&self) -> Duration {
        let running = self
            .entered_at
            .get()
            .map(|start| thread_cpu_time().saturating_sub(start))
            .unwrap_or_default();
        self.spent.get() + running
    }

    /// Called from the engine's interrupt handler. Once the budget is gone it
    /// stays gone, so every later poll interrupts too.
    pub fn over_budget(&self) -> bool {
        if !self.exhausted.get() && self.spent() > self.budget {
            self.exhausted.set(true);
        }
        self.exhausted.get()
    }

    pub fn exhausted(&self) -> bool {
        self.exhausted.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burn(duration: Duration) {
        let start = thread_cpu_time();
        let mut x = 0u64;
        while thread_cpu_time().saturating_sub(start) < duration {
            x = x.wrapping_mul(31).wrapping_add(7);
            std::hint::black_box(x);
        }
    }

    #[test]
    fn time_outside_the_engine_is_not_counted() {
        let clock = JsClock::new(Duration::from_millis(30));
        clock.enter();
        burn(Duration::from_millis(5));
        clock.leave();
        // Sleeping and burning CPU while left must not count.
        std::thread::sleep(Duration::from_millis(50));
        burn(Duration::from_millis(40));
        assert!(!clock.over_budget(), "spent {:?}", clock.spent());
        clock.enter();
        burn(Duration::from_millis(40));
        assert!(clock.over_budget());
    }
}
