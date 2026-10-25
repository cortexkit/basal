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

    /// This thread's kernel plus user time as Windows charges it, read
    /// independently of `JsClock`.
    #[cfg(windows)]
    fn os_thread_time() -> Duration {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentThread, GetThreadTimes},
        };
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: all output pointers are live and the pseudo-handle names
        // this test thread.
        let ok = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
        let units = |v: FILETIME| (u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime);
        Duration::from_nanos((units(kernel) + units(user)) * 100)
    }

    #[cfg(windows)]
    fn native_sample() -> String {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentThread, GetCurrentThreadId, GetThreadTimes},
        };
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryPerformanceCounter(counter: *mut i64) -> i32;
            fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
            fn QueryThreadCycleTime(thread: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let (mut qpc, mut frequency, mut cycles) = (0, 0, 0);
        // SAFETY: all output pointers are live and the pseudo-handle names this
        // test thread. These independent counters are diagnostic, not a clock
        // conversion or part of the engine's measured interval.
        unsafe {
            assert_ne!(
                GetThreadTimes(
                    GetCurrentThread(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user
                ),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
            assert_ne!(QueryPerformanceCounter(&mut qpc), 0);
            assert_ne!(QueryPerformanceFrequency(&mut frequency), 0);
            assert_ne!(QueryThreadCycleTime(GetCurrentThread(), &mut cycles), 0);
        }
        let units = |v: FILETIME| (u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime);
        format!(
            "thread={} kernel_100ns={} user_100ns={} qpc={qpc} qpc_hz={frequency} cycles={cycles}",
            unsafe { GetCurrentThreadId() },
            units(kernel),
            units(user)
        )
    }

    #[cfg(not(windows))]
    fn native_sample() -> String {
        format!("thread_cpu_time={:?}", thread_cpu_time())
    }

    fn engine_intervals(sample: usize) -> Duration {
        const BUDGET: Duration = Duration::from_millis(150);
        let before = native_sample();
        #[cfg(windows)]
        let os_before = os_thread_time();
        let clock = JsClock::new(BUDGET);
        clock.enter();
        burn(Duration::from_millis(30));
        clock.leave();
        #[cfg(windows)]
        let os_after = os_thread_time();
        let after = native_sample();
        let spent = clock.spent();
        let details = format!("sample={sample} spent={spent:?} before=[{before}] after=[{after}]");
        #[cfg(windows)]
        println!("{details}");
        // The clock charges what the OS charged this thread while entered,
        // and nothing more. The amount itself isn't bounded: on a virtual
        // machine the host can stall the thread mid-interval, and Windows
        // charges that stall to the thread (one CI sample charged 171.875 ms
        // of kernel time for 30 ms of work, matching the 171.9 ms of wall
        // time the performance counter measured).
        #[cfg(windows)]
        assert!(
            spent <= os_after.saturating_sub(os_before),
            "the clock charged more than the OS: {details}"
        );
        // Sleeping and burning CPU while left must not count.
        std::thread::sleep(Duration::from_millis(50));
        burn(Duration::from_millis(200));
        assert_eq!(clock.spent(), spent, "paused CPU time changed: {details}");
        assert_eq!(clock.over_budget(), spent > BUDGET, "{details}");
        clock.enter();
        burn(Duration::from_millis(200));
        assert!(
            clock.over_budget(),
            "entered CPU time was not charged: {details}"
        );
        spent
    }

    /// Exercise the real CPU source as well as the pause/resume bookkeeping.
    /// On Windows, also check that an entered interval never charges more
    /// than the OS charged the thread, since an oversized charge would end a
    /// real activation early.
    #[test]
    fn time_outside_the_engine_is_not_counted() {
        engine_intervals(0);
    }

    /// Compare the tick-accounted clock with wall time and raw thread cycles.
    /// Keep every sample so a large accounting jump is visible in CI output.
    #[cfg(windows)]
    #[test]
    fn windows_engine_interval_samples() {
        let mut largest = Duration::ZERO;
        for index in 0..512 {
            largest = largest.max(engine_intervals(index));
        }
        println!("largest entered interval over 512 samples: {largest:?}");
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
