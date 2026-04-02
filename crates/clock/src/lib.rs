//! Clock abstractions for monotonic timing, async sleep, and periodic intervals.

use std::time::{Duration, Instant, SystemTime};

use futures::future::BoxFuture;
use futures::stream::unfold;
use futures::stream::BoxStream;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;

    fn system_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn interval(&self, period: Duration) -> BoxStream<'static, ()> {
        Box::pin(unfold(tokio::time::interval(period), |mut interval| async move {
            interval.tick().await;
            Some(((), interval))
        }))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone)]
pub struct VirtualClock {
    base_instant: Instant,
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self {
            base_instant: Instant::now(),
        }
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Instant {
        self.base_instant + self.base_instant.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn real_clock_returns_monotonic_now() {
        let clock = RealClock;
        let t1 = clock.now();
        let t2 = clock.now();
        assert!(t2 >= t1);
    }

    #[test]
    fn virtual_clock_base_instant_controls_now() {
        let base = Instant::now() - Duration::from_secs(2);
        let clock = VirtualClock { base_instant: base };
        let now = clock.now();
        assert!(now >= base + Duration::from_secs(2));
    }

    #[tokio::test]
    async fn sleep_completes() {
        let clock = RealClock;
        clock.sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn interval_yields_ticks() {
        let clock = RealClock;
        let mut ticks = clock.interval(Duration::from_millis(1));
        assert!(ticks.next().await.is_some());
        assert!(ticks.next().await.is_some());
    }
}
