//! Rate-limiting helpers for repeated log messages during sync recovery.
//!
//! These types throttle log output without changing any recovery semantics.
//! All diagnostic information is preserved at `debug!` level.

use std::sync::atomic::{AtomicU64, Ordering};

/// Allows one `true` return per distinct ledger value (monotonically increasing).
///
/// Initialized with `u64::MAX` sentinel so the first call always returns `true`.
/// Since `current_ledger` only increases, a simple `swap` + equality check
/// suffices: the only duplicate case is same-value calls.
pub(crate) struct LogOncePerLedger(AtomicU64);

impl LogOncePerLedger {
    pub fn new() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }

    /// Returns `true` the first time called for a given `ledger` value.
    /// Subsequent calls with the same `ledger` return `false`.
    /// When `ledger` advances, the first call with the new value returns `true`.
    pub fn should_log(&self, ledger: u64) -> bool {
        let prev = self.0.swap(ledger, Ordering::Relaxed);
        prev != ledger
    }

    /// Reset to initial state so the next sync-loss episode gets a fresh
    /// info-level log.
    pub fn reset(&self) {
        self.0.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Allows one `true` return per `interval` seconds.
///
/// Uses elapsed seconds from a reference instant (e.g., `start_instant`).
/// Initialized with `u64::MAX` sentinel so the first call always returns `true`,
/// avoiding the "first N seconds suppressed" bug.
///
/// Single-caller context assumed (tokio single-threaded async).
pub(crate) struct LogThrottleSecs {
    last_logged: AtomicU64,
    interval: u64,
}

impl LogThrottleSecs {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            last_logged: AtomicU64::new(u64::MAX),
            interval: interval_secs,
        }
    }

    /// Returns `true` if this is the first call (sentinel) or at least
    /// `interval` seconds have elapsed since the last `true` return.
    pub fn should_log(&self, now_secs: u64) -> bool {
        let last = self.last_logged.load(Ordering::Relaxed);
        if last == u64::MAX || now_secs >= last + self.interval {
            self.last_logged.store(now_secs, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Reset to initial state so the next episode gets an immediate log.
    pub fn reset(&self) {
        self.last_logged.store(u64::MAX, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_once_per_ledger_first_call_returns_true() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
    }

    #[test]
    fn test_log_once_per_ledger_same_ledger_returns_false() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        assert!(!throttle.should_log(100));
    }

    #[test]
    fn test_log_once_per_ledger_new_ledger_returns_true() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        assert!(throttle.should_log(101));
        assert!(!throttle.should_log(101));
        assert!(throttle.should_log(200));
    }

    #[test]
    fn test_log_once_per_ledger_reset_allows_refire() {
        let throttle = LogOncePerLedger::new();
        assert!(throttle.should_log(100));
        assert!(!throttle.should_log(100));
        throttle.reset();
        assert!(throttle.should_log(100));
    }

    #[test]
    fn test_log_throttle_secs_first_call_returns_true() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
    }

    #[test]
    fn test_log_throttle_secs_within_window_returns_false() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(1));
        assert!(!throttle.should_log(5));
        assert!(!throttle.should_log(9));
    }

    #[test]
    fn test_log_throttle_secs_after_window_returns_true() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(5));
        assert!(throttle.should_log(10));
        assert!(!throttle.should_log(15));
        assert!(throttle.should_log(20));
    }

    #[test]
    fn test_log_throttle_secs_reset_allows_refire() {
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
        assert!(!throttle.should_log(5));
        throttle.reset();
        assert!(throttle.should_log(5));
    }

    #[test]
    fn test_log_throttle_secs_sentinel_always_fires_first() {
        // Even at time 0, first call should return true (sentinel is u64::MAX).
        let throttle = LogThrottleSecs::new(10);
        assert!(throttle.should_log(0));
    }
}
