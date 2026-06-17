//! Lock-latency telemetry for #1759 freeze investigation —
//! parking_lot companion to `crates/app/src/app/tracked_lock.rs`.
//!
//! Background: `fbbdd465` wrapped four event-loop-facing herder
//! entry points and every `syncing_ledgers` (tokio::sync::RwLock)
//! acquisition with hold-time telemetry. The subsequent live
//! freeze capture (session `86a7163c`, 20+ events, wchan
//! histogram: 40 threads in `futex_wait_queue`) produced **zero**
//! `WARN lock=…` or `WARN call=…` lines. The offending lock is
//! therefore NOT in the instrumented tokio-RwLock call sites.
//!
//! The next candidate set, per issue #1768, is the handful of
//! `parking_lot::RwLock` fields inside the herder crate that are
//! hit on every SCP envelope ingress and every ledger close:
//!
//! 1. [`Herder::state`](crate::herder::Herder) — protects the
//!    tri-state `HerderState` machine.
//! 2. `ScpDriver::externalized` / `latest_externalized` —
//!    guard the map of externalized slots and the current-tip
//!    cursor.
//! 3. `Arc<RwLock<SharedTrackingState>>` — shared between
//!    Herder and ScpDriver; read on every nomination-value
//!    validation.
//!
//! This module provides [`tracked_write`] and [`tracked_read`]
//! helpers whose returned [`TrackedGuard`] emits a single
//! `WARN lock=<label>` on drop when a hold or wait exceeds
//! [`LOCK_SLOW_THRESHOLD`].
//!
//! ## Differences from the app-crate helper
//!
//! - `parking_lot::RwLock::{read, write}` are **synchronous**;
//!   no `.await`, no tokio scheduler jitter contaminating
//!   wait-time measurements. What we measure here is pure
//!   futex-blocking time — exactly the quantity implicated by
//!   the wchan=`futex_wait_queue` histogram.
//! - The guard types (`RwLockReadGuard`, `RwLockWriteGuard`)
//!   are not `Send`-across-await-boundary constrained.
//!
//! ## Emission contract
//!
//! Each acquisition helper emits at most two `WARN` lines:
//!
//! - `kind="wait"` — fires immediately after acquisition if
//!   the acquire-wait alone reached [`LOCK_SLOW_THRESHOLD`].
//!   Implies a different thread held the lock too long.
//! - `kind="hold"` — fires from [`TrackedGuard::drop`] if the
//!   held duration reached [`LOCK_SLOW_THRESHOLD`]. Drop runs
//!   on panic-unwind too: a slow held lock under panic is
//!   still a slow held lock.
//!
//! ## Fast-path cost
//!
//! Two `Instant::now()` calls per acquisition (~20 ns each on
//! Linux, `clock_gettime(CLOCK_MONOTONIC)` VDSO) plus a single
//! `Duration::cmp` comparison in the hot branch that almost
//! always bails out without emitting. Total overhead well
//! under 100 ns on healthy operation. For a lock read on
//! every SCP envelope (~100/sec peak), < 10 µs/sec budget.

use std::ops::{Deref, DerefMut};
use std::time::Instant;

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

// Re-export the shared timing primitives so in-crate call sites can use
// `tracked_lock::PhaseTimer` / `tracked_lock::LOCK_SLOW_THRESHOLD` without
// reaching into `henyey_common::tracking` directly. `LOCK_SLOW_THRESHOLD`
// is defined once in `henyey-common` and consumed by both this crate
// and the app-crate tokio companion, so both instrumentation families
// speak the same language in the validator log.
pub(crate) use henyey_common::tracking::{PhaseTimer, LOCK_SLOW_THRESHOLD};

/// RAII wrapper around a `parking_lot` RwLock guard that emits
/// a single `WARN lock=<label> kind="hold"` on drop iff the
/// held duration reached [`LOCK_SLOW_THRESHOLD`].
///
/// [`Deref`]/[`DerefMut`] are forwarded to the inner guard, so
/// `*tracked_write(LABEL, &self.lock) = value;` behaves
/// identically to `*self.lock.write() = value;` at the call
/// site — the wrapper is transparent to the consumer.
///
/// ### Drop semantics
///
/// - **Pattern A — bound guard** (`let g = tracked_write(...);`):
///   Drop runs at end of scope; measured hold = scope body
///   duration. Correct for measuring held-lock regressions.
/// - **Pattern B — one-shot expression**
///   (`*tracked_write(...) = x;` or
///   `let y = *tracked_read(...);`): Drop runs at end of
///   statement; measured hold ≈ 0 ns. Correct: a one-shot
///   read/write should not emit a `hold` WARN unless the
///   acquire-wait alone exceeded threshold.
///
/// ### Non-Deref API surface
///
/// `parking_lot::{RwLockReadGuard, RwLockWriteGuard}` have
/// inherent methods (`downgrade`, `map`, etc.) that are NOT
/// reachable through `Deref::Target`. Any call site that
/// needs those methods must bypass the wrapper (losing
/// telemetry for that one acquisition) or refactor. Current
/// call sites in herder.rs / scp_driver.rs do not use them.
pub(crate) struct TrackedGuard<G> {
    inner: G,
    label: &'static str,
    hold_start: Instant,
}

impl<G> Drop for TrackedGuard<G> {
    fn drop(&mut self) {
        let hold = self.hold_start.elapsed();
        if hold >= LOCK_SLOW_THRESHOLD {
            tracing::warn!(
                lock = self.label,
                kind = "hold",
                hold_ms = hold.as_millis() as u64,
                "Slow parking_lot::RwLock hold (>= {}ms) — possible #1759 contributor",
                LOCK_SLOW_THRESHOLD.as_millis()
            );
        }
    }
}

impl<G: Deref> Deref for TrackedGuard<G> {
    type Target = G::Target;
    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<G: DerefMut> DerefMut for TrackedGuard<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

/// Acquire a write lock on `lock`, timing both the acquire-wait
/// and (via the returned [`TrackedGuard`]) the subsequent hold.
///
/// Emits `WARN lock=<label> kind="wait"` immediately iff the
/// acquire-wait alone reached [`LOCK_SLOW_THRESHOLD`] (i.e.
/// a *different* thread held the lock longer than threshold).
/// The returned [`TrackedGuard`] independently emits
/// `WARN lock=<label> kind="hold"` on drop iff the hold itself
/// reached threshold.
pub(crate) fn tracked_write<'a, T>(
    label: &'static str,
    lock: &'a RwLock<T>,
) -> TrackedGuard<RwLockWriteGuard<'a, T>> {
    let acquire_start = Instant::now();
    let inner = lock.write();
    let wait = acquire_start.elapsed();
    if wait >= LOCK_SLOW_THRESHOLD {
        tracing::warn!(
            lock = label,
            kind = "wait",
            acquire_wait_ms = wait.as_millis() as u64,
            "Slow parking_lot::RwLock acquire-wait (>= {}ms) — possible #1759 contributor",
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }
    TrackedGuard {
        inner,
        label,
        hold_start: Instant::now(),
    }
}

/// Acquire a read lock on `lock`, timing both the acquire-wait
/// and (via the returned [`TrackedGuard`]) the subsequent hold.
///
/// Slow reader `wait` WARNs implicate a writer holding too
/// long. Slow reader `hold` WARNs are themselves suspect
/// because concurrent readers block writers under
/// `parking_lot`'s task-fair RwLock policy.
pub(crate) fn tracked_read<'a, T>(
    label: &'static str,
    lock: &'a RwLock<T>,
) -> TrackedGuard<RwLockReadGuard<'a, T>> {
    let acquire_start = Instant::now();
    let inner = lock.read();
    let wait = acquire_start.elapsed();
    if wait >= LOCK_SLOW_THRESHOLD {
        tracing::warn!(
            lock = label,
            kind = "wait",
            acquire_wait_ms = wait.as_millis() as u64,
            "Slow parking_lot::RwLock acquire-wait (>= {}ms) — possible #1759 contributor",
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }
    TrackedGuard {
        inner,
        label,
        hold_start: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use tracing::{
        field::{Field, Visit},
        subscriber::{with_default, Subscriber},
        Event, Metadata,
    };

    /// Hold duration comfortably above threshold (250 ms) but
    /// short enough to keep CI fast.
    const TEST_HOLD_DURATION: Duration = Duration::from_millis(350);

    /// Captured tracing event for the test harness.
    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        level: String,
        lock: Option<String>,
        kind: Option<String>,
        hold_ms: Option<u64>,
        acquire_wait_ms: Option<u64>,
    }

    #[derive(Clone, Default)]
    struct CapturingSubscriber {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl Visit for CapturedEvent {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "lock" => self.lock = Some(value.to_string()),
                "kind" => self.kind = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            match field.name() {
                "hold_ms" => self.hold_ms = Some(value),
                "acquire_wait_ms" => self.acquire_wait_ms = Some(value),
                _ => {}
            }
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.record_u64(field, value as u64);
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    impl Subscriber for CapturingSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut captured = CapturedEvent {
                level: event.metadata().level().to_string(),
                ..Default::default()
            };
            event.record(&mut captured);
            self.events.lock().unwrap().push(captured);
        }

        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// `tracked_write` emits a `kind="hold"` WARN when the
    /// guard is held at least `LOCK_SLOW_THRESHOLD`.
    #[test]
    fn tracked_write_warns_on_long_hold() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: RwLock<u32> = RwLock::new(0);

        with_default(sub, || {
            let mut guard = tracked_write("test.lock", &lock);
            *guard = 1;
            thread::sleep(TEST_HOLD_DURATION);
            drop(guard);
        });

        let evs = events.lock().unwrap();
        let hold_events: Vec<_> = evs
            .iter()
            .filter(|e| e.kind.as_deref() == Some("hold") && e.lock.as_deref() == Some("test.lock"))
            .collect();
        assert_eq!(hold_events.len(), 1, "expected exactly one hold WARN");
        assert_eq!(hold_events[0].level, "WARN");
        let ms = hold_events[0].hold_ms.expect("hold_ms missing");
        assert!(
            ms >= LOCK_SLOW_THRESHOLD.as_millis() as u64,
            "hold_ms={} must be >= threshold {}",
            ms,
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }

    /// `tracked_write` emits zero events when the guard is
    /// released promptly — zero log noise on the fast path.
    #[test]
    fn tracked_write_silent_on_fast_hold() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: RwLock<u32> = RwLock::new(0);

        with_default(sub, || {
            let mut guard = tracked_write("test.lock", &lock);
            *guard = 1;
            drop(guard);
        });

        let evs = events.lock().unwrap();
        let hold_events: Vec<_> = evs
            .iter()
            .filter(|e| e.kind.as_deref() == Some("hold"))
            .collect();
        assert_eq!(hold_events.len(), 0, "fast path should emit no WARN");
    }

    /// `tracked_write` emits a `kind="wait"` WARN when a
    /// writer waits at least `LOCK_SLOW_THRESHOLD` for a
    /// contended lock.
    ///
    /// Two `std::thread`s: holder owns the write guard for
    /// `TEST_HOLD_DURATION`, then releases. A `std::sync::mpsc`
    /// one-shot signal gates the waiter so its wait-time
    /// measurement only starts after contention exists.
    #[test]
    fn tracked_write_warns_on_long_wait() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: Arc<RwLock<u32>> = Arc::new(RwLock::new(0));
        let holder_lock = Arc::clone(&lock);
        let (held_tx, held_rx) = channel::<()>();

        with_default(sub, || {
            let holder = thread::spawn(move || {
                let _guard = holder_lock.write();
                held_tx.send(()).expect("signal receiver dropped");
                thread::sleep(TEST_HOLD_DURATION);
                // Guard released on function exit.
            });

            // Wait for holder to own the lock before measuring.
            held_rx.recv().expect("holder must acquire lock first");

            let guard = tracked_write("test.lock", &lock);
            drop(guard);
            holder.join().expect("holder panicked");
        });

        let evs = events.lock().unwrap();
        let wait_events: Vec<_> = evs
            .iter()
            .filter(|e| e.kind.as_deref() == Some("wait") && e.lock.as_deref() == Some("test.lock"))
            .collect();
        assert_eq!(
            wait_events.len(),
            1,
            "expected exactly one wait WARN; saw: {:?}",
            *evs
        );
        let ms = wait_events[0]
            .acquire_wait_ms
            .expect("acquire_wait_ms missing");
        assert!(
            ms >= LOCK_SLOW_THRESHOLD.as_millis() as u64,
            "acquire_wait_ms={} must be >= threshold {}",
            ms,
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }

    /// `tracked_read` emits a `kind="wait"` WARN when a reader
    /// is blocked behind a writer holding the lock for at least
    /// `LOCK_SLOW_THRESHOLD`.
    #[test]
    fn tracked_read_warns_on_long_wait() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: Arc<RwLock<u32>> = Arc::new(RwLock::new(0));
        let holder_lock = Arc::clone(&lock);
        let (held_tx, held_rx) = channel::<()>();

        with_default(sub, || {
            let holder = thread::spawn(move || {
                let _guard = holder_lock.write();
                held_tx.send(()).expect("signal receiver dropped");
                thread::sleep(TEST_HOLD_DURATION);
            });

            held_rx.recv().expect("holder must acquire lock first");

            let guard = tracked_read("test.lock", &lock);
            drop(guard);
            holder.join().expect("holder panicked");
        });

        let evs = events.lock().unwrap();
        let wait_events: Vec<_> = evs
            .iter()
            .filter(|e| e.kind.as_deref() == Some("wait") && e.lock.as_deref() == Some("test.lock"))
            .collect();
        assert_eq!(wait_events.len(), 1);
    }

    /// `tracked_read` emits zero events on the fast path.
    #[test]
    fn tracked_read_silent_on_fast_hold() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: RwLock<u32> = RwLock::new(42);

        with_default(sub, || {
            let guard = tracked_read("test.lock", &lock);
            let _v = *guard;
            drop(guard);
        });

        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 0);
    }

    /// Boundary: `tracked_write` fires when hold is at least
    /// threshold + 10 ms (guards the `>=` comparison).
    #[test]
    fn tracked_write_boundary_inclusive() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let lock: RwLock<u32> = RwLock::new(0);

        with_default(sub, || {
            let guard = tracked_write("test.lock", &lock);
            thread::sleep(LOCK_SLOW_THRESHOLD + Duration::from_millis(10));
            drop(guard);
        });

        let evs = events.lock().unwrap();
        let hold_events: Vec<_> = evs
            .iter()
            .filter(|e| e.kind.as_deref() == Some("hold"))
            .collect();
        assert_eq!(hold_events.len(), 1);
    }
}
