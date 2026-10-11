//! The thread CPU clock on Windows.
//!
//! `GetThreadTimes` reports the calling thread's kernel and user time in
//! 100-nanosecond units. The kernel charges that time a scheduler tick at a
//! time (about 15.6 ms by default), so the clock advances in ticks: a budget
//! smaller than a tick is imprecise, and the activation's wall deadline,
//! enforced by the parent, stays the hard bound. A failed read counts as
//! zero, the same fallback the Unix clock uses.

use std::time::Duration;

/// The calling thread's kernel plus user time.
#[cfg(windows)]
pub fn thread_cpu_time() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle names the calling thread, and the four
    // out-pointers are valid FILETIMEs for the duration of the call.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    } != 0;
    let ticks =
        |time: FILETIME| u64::from(time.dwHighDateTime) << 32 | u64::from(time.dwLowDateTime);
    cpu_time(ok.then(|| (ticks(kernel), ticks(user))))
}

/// CPU time from one `GetThreadTimes` sample of (kernel, user) time in
/// 100-nanosecond units, or zero when the read failed.
pub fn cpu_time(sample: Option<(u64, u64)>) -> Duration {
    match sample {
        None => Duration::ZERO,
        Some((kernel, user)) => {
            let units = kernel.saturating_add(user);
            Duration::new(units / 10_000_000, (units % 10_000_000) as u32 * 100)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_and_user_time_add_up_in_100_nanosecond_units() {
        // One second of kernel time and one scheduler tick (156,250 units) of
        // user time.
        assert_eq!(
            cpu_time(Some((10_000_000, 156_250))),
            Duration::from_secs(1) + Duration::from_micros(15_625)
        );
        assert_eq!(cpu_time(Some((0, 1))), Duration::from_nanos(100));
        assert_eq!(cpu_time(Some((0, 0))), Duration::ZERO);
    }

    #[test]
    fn a_failed_read_is_zero() {
        assert_eq!(cpu_time(None), Duration::ZERO);
    }

    #[test]
    fn the_largest_sample_saturates_instead_of_wrapping() {
        let largest = cpu_time(Some((u64::MAX, u64::MAX)));
        assert_eq!(largest, cpu_time(Some((u64::MAX, 0))));
        assert!(largest > cpu_time(Some((u64::MAX - 1, 0))));
    }

    /// The real clock advances while the thread burns CPU. Several ticks are
    /// burned so the reading moves even at scheduler-tick resolution. The
    /// loop is bounded by work done, not by the clock under test, so a clock
    /// stuck at zero fails instead of hanging.
    #[cfg(windows)]
    #[test]
    fn the_real_clock_advances_with_work() {
        let start = thread_cpu_time();
        let mut x = 0u64;
        for round in 0..10_000u32 {
            if thread_cpu_time().saturating_sub(start) >= Duration::from_millis(100) {
                break;
            }
            for _ in 0..100_000u32 {
                x = x.wrapping_mul(31).wrapping_add(u64::from(round));
                std::hint::black_box(x);
            }
        }
        assert!(thread_cpu_time() >= start + Duration::from_millis(100));
    }
}
