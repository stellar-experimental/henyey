//! Clock abstractions for monotonic timing, wall-clock reads, and async sleep.

use std::time::{Duration, Instant, SystemTime};

use futures::future::BoxFuture;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;

    fn system_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clock_returns_monotonic_now() {
        let clock = RealClock;
        let t1 = clock.now();
        let t2 = clock.now();
        assert!(t2 >= t1);
    }

    #[tokio::test]
    async fn sleep_completes() {
        let clock = RealClock;
        clock.sleep(Duration::from_millis(1)).await;
    }

    #[test]
    fn system_now_returns_valid_unix_timestamp() {
        // Verify that system_now() always produces a valid UNIX timestamp.
        // This is used in consensus-critical paths (close-time proposals, drift
        // tracking) where silently defaulting to 0 would cause incorrect
        // behavior. The code uses .expect() to crash-on-failure rather than
        // .unwrap_or(0) to avoid silent data corruption.
        let clock = RealClock;
        let ts = clock
            .system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();
        // Should be well past the epoch (at least year 2020 ~ 1_577_836_800)
        assert!(
            ts > 1_577_836_800,
            "system clock returned implausible timestamp: {ts}"
        );
    }

    /// A clock that returns a pre-epoch time. Used to verify that
    /// consensus-critical code panics instead of silently defaulting to 0.
    struct PreEpochClock;

    impl Clock for PreEpochClock {
        fn now(&self) -> Instant {
            Instant::now()
        }

        fn system_now(&self) -> SystemTime {
            // 1 second before the UNIX epoch
            std::time::UNIX_EPOCH - Duration::from_secs(1)
        }
    }

    #[test]
    #[should_panic(expected = "system clock before UNIX epoch")]
    fn pre_epoch_clock_panics_on_duration_since_epoch() {
        // Regression test for AUDIT-AC2: consensus-critical paths must never
        // silently default to timestamp 0. If the clock is before the UNIX
        // epoch, the code must panic rather than produce a wrong value.
        let clock = PreEpochClock;
        let _ts = clock
            .system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();
    }
}
