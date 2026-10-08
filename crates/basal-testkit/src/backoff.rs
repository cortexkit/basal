use std::time::{Duration, Instant};

/// Conditions without a notification source still need polling. Increase the
/// interval while they remain unchanged, without sleeping past the deadline.
pub(crate) struct Backoff(Duration);

impl Backoff {
    pub(crate) fn new() -> Self {
        Self(Duration::from_millis(1))
    }

    fn delay(&mut self, remaining: Duration) -> Duration {
        let delay = self.0.min(remaining);
        self.0 = (self.0 * 2).min(Duration::from_millis(50));
        delay
    }

    pub(crate) fn sleep(&mut self, deadline: Instant) {
        std::thread::sleep(self.delay(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_backoff_grows_from_one_ms_caps_at_fifty_and_respects_remaining_time() {
        let mut backoff = Backoff::new();
        for millis in [1, 2, 4, 8, 16, 32, 50, 50] {
            assert_eq!(
                backoff.delay(Duration::from_secs(1)),
                Duration::from_millis(millis)
            );
        }
        assert_eq!(
            backoff.delay(Duration::from_micros(7)),
            Duration::from_micros(7)
        );
        assert_eq!(backoff.delay(Duration::ZERO), Duration::ZERO);
    }
}
