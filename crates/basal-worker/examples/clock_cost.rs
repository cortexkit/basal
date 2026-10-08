//! Descriptive native syscall cost, not a performance assertion or a worker budget.
fn main() {
    #[cfg(target_os = "linux")]
    {
        let samples = 1_000_000u32;
        println!(
            "architecture={} clock=CLOCK_THREAD_CPUTIME_ID samples_per_batch={samples}",
            std::env::consts::ARCH
        );
        for batch in 1..=5 {
            let start = std::time::Instant::now();
            for _ in 0..samples {
                let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
                // CLOCK_THREAD_CPUTIME_ID has no vDSO implementation: this is a real syscall.
                let rc = unsafe {
                    libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, value.as_mut_ptr())
                };
                assert_eq!(rc, 0);
                std::hint::black_box(unsafe { value.assume_init() });
            }
            println!(
                "batch={batch} nanoseconds_per_call={:.2}",
                start.elapsed().as_nanos() as f64 / samples as f64
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    panic!("clock-cost measurement requires native Linux");
}
