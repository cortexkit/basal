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

#[cfg(any(windows, test))]
pub mod windows;

/// The calling thread's CPU time.
///
/// On Windows it is the thread's kernel plus user time from `GetThreadTimes`
/// (see [`windows`]), which advances in scheduler ticks.
#[cfg(windows)]
pub fn thread_cpu_time() -> Duration {
    windows::thread_cpu_time()
}

/// The calling thread's CPU time.
///
/// `CLOCK_THREAD_CPUTIME_ID` is supported on every macOS release this worker
/// targets and on Linux; if the call ever fails, the budget falls back to
/// treating the clock as stopped, and the parent's wall-clock deadline (which
/// kills the worker) remains the backstop.
#[cfg(not(windows))]
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
    /// Where CPU time samples come from: [`thread_cpu_time`], or a scripted
    /// source in tests.
    sample: fn() -> Duration,
}

impl JsClock {
    pub fn new(budget: Duration) -> Self {
        Self::with_source(budget, thread_cpu_time)
    }

    /// A clock that reads its CPU time samples from `sample`.
    pub fn with_source(budget: Duration, sample: fn() -> Duration) -> Self {
        Self {
            budget,
            spent: Cell::new(Duration::ZERO),
            entered_at: Cell::new(None),
            exhausted: Cell::new(false),
            sample,
        }
    }

    /// Starts counting: the worker is about to run JavaScript.
    pub fn enter(&self) {
        if self.entered_at.get().is_none() {
            self.entered_at.set(Some((self.sample)()));
        }
    }

    /// Stops counting: control has returned to the worker, or the worker is
    /// about to wait on the parent.
    pub fn leave(&self) {
        if let Some(start) = self.entered_at.take() {
            let now = (self.sample)();
            self.spent.set(self.spent.get() + now.saturating_sub(start));
        }
    }

    pub fn spent(&self) -> Duration {
        let running = self
            .entered_at
            .get()
            .map(|start| (self.sample)().saturating_sub(start))
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

    /// Against the real clock. The budget and the burns span several
    /// scheduler ticks (about 15.6 ms on Windows, where the thread CPU clock
    /// advances a tick at a time), so tick rounding cannot flip the outcome.
    #[test]
    fn time_outside_the_engine_is_not_counted() {
        let clock = JsClock::new(Duration::from_millis(150));
        clock.enter();
        burn(Duration::from_millis(30));
        clock.leave();
        // Sleeping and burning CPU while left must not count.
        std::thread::sleep(Duration::from_millis(50));
        burn(Duration::from_millis(200));
        assert!(!clock.over_budget(), "spent {:?}", clock.spent());
        clock.enter();
        burn(Duration::from_millis(200));
        assert!(clock.over_budget());
    }

    thread_local! {
        static SAMPLE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    fn scripted() -> Duration {
        SAMPLE.with(Cell::get)
    }

    fn set(millis: u64) {
        SAMPLE.with(|sample| sample.set(Duration::from_millis(millis)));
    }

    /// With injected samples: only the intervals between `enter` and
    /// `leave` count, a running interval counts up to the latest sample, and
    /// exhaustion stays latched.
    #[test]
    fn only_samples_taken_inside_the_engine_are_counted() {
        set(1_000);
        let clock = JsClock::with_source(Duration::from_millis(100), scripted);
        clock.enter();
        set(1_060);
        clock.leave();
        assert_eq!(clock.spent(), Duration::from_millis(60));
        // A long gap outside the engine is not counted.
        set(9_000);
        assert_eq!(clock.spent(), Duration::from_millis(60));
        assert!(!clock.over_budget());
        clock.enter();
        // Entering twice keeps the first sample.
        set(9_020);
        clock.enter();
        set(9_040);
        assert_eq!(clock.spent(), Duration::from_millis(100));
        assert!(!clock.over_budget(), "exactly the budget is not over it");
        set(9_041);
        assert!(clock.over_budget());
        clock.leave();
        assert!(clock.exhausted());
        assert!(clock.over_budget(), "exhaustion is latched");
    }

    /// A clock that failed returns zero, which can be below the sample taken
    /// on entry. The interval then counts as nothing rather than wrapping.
    #[test]
    fn a_sample_below_the_entry_sample_counts_as_nothing() {
        set(500);
        let clock = JsClock::with_source(Duration::from_millis(10), scripted);
        clock.enter();
        set(0);
        assert_eq!(clock.spent(), Duration::ZERO);
        clock.leave();
        assert_eq!(clock.spent(), Duration::ZERO);
        assert!(!clock.over_budget());
    }
}
