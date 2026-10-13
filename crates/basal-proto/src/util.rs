//! Small, dependency-free encodings and synchronization shared by the parent
//! and worker crates.

use std::sync::{Mutex, MutexGuard};

use crate::Settlement;

/// Milliseconds since the Unix epoch, or zero if the clock precedes it.
pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

/// Encode bytes as lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Recover the protected value after another thread panics while holding it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The settlement spelling stored in the journal and persistent host fixtures.
pub fn settlement_str(settlement: Settlement) -> &'static str {
    match settlement {
        Settlement::Fulfilled => "fulfilled",
        Settlement::Rejected => "rejected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_hex_preserves_leading_zeroes() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0, 1, 15, 16, 255]), "00010f10ff");
    }

    #[test]
    fn poisoned_lock_recovers_the_value() {
        let mutex = Mutex::new(7);
        let _ = std::panic::catch_unwind(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison the mutex");
        });
        assert_eq!(*lock(&mutex), 7);
    }

    #[test]
    fn settlement_spellings_are_stable() {
        assert_eq!(settlement_str(Settlement::Fulfilled), "fulfilled");
        assert_eq!(settlement_str(Settlement::Rejected), "rejected");
    }
}
