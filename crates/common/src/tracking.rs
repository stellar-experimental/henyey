//! Lightweight timing primitives for event-loop latency telemetry.
//!
//! This module is the shared home for per-call and per-phase timing
//! helpers used to diagnose event-loop freezes on mainnet validators.
//! It was extracted from `crates/app/src/app/tracked_lock.rs` so that
//! both `henyey-app` (async/tokio side) and `henyey-herder` (sync/
//! parking_lot side) can reuse the same primitives without
//! duplication.
//!
//! ## Primitives
//!
//! - [`LOCK_SLOW_THRESHOLD`] — shared 250 ms threshold above which a
//!   call or lock hold emits a `WARN` log line. Mirrors the threshold
//!   used by the tokio- and parking_lot-RwLock wrappers.
//!
//! - [`time_call`] — wrap a single synchronous call; emit one `WARN`
//!   with `call=<label> elapsed_ms=...` iff the total elapsed reaches
//!   [`LOCK_SLOW_THRESHOLD`]. Intended for naming a single slow entry
//!   point (e.g. `herder.receive_tx_set`).
//!
//! - [`PhaseTimer`] — record a sequence of named phase durations
//!   between `mark` points. On `finish`, emit a single structured
//!   `WARN` with every phase as a `<name>=<ms>` field plus a
//!   `total_ms=<sum>` field, iff total >= [`LOCK_SLOW_THRESHOLD`].
//!   Intended for breaking a slow `time_call`ed function into its
//!   sub-phases so the next freeze log names the dominant phase.
//!
//! ## Fast-path cost
//!
//! All primitives are `Instant::now()` + arithmetic on the happy path;
//! the `WARN` emission and field formatting only run when the threshold
//! is reached. `PhaseTimer::mark` is one `Instant::now()` + one push to
//! a small `Vec` (typically <= 8 phases — inline storage could be added
//! later, but the current cost is well under 200 ns on a healthy
//! validator).

use std::time::{Duration, Instant};

/// Threshold above which [`time_call`] or [`PhaseTimer::finish`]
/// emits a `WARN` log line.
///
/// 250 ms is conservative relative to the observed multi-second
/// event-loop freezes documented in issues #1759 / #1768 / #1772 while
/// remaining well above normal-case baselines (sub-millisecond on a
/// healthy node).
pub const LOCK_SLOW_THRESHOLD: Duration = Duration::from_millis(250);

/// Time a synchronous function call, emitting a single `WARN` if the
/// total wall time reaches [`LOCK_SLOW_THRESHOLD`].
///
/// Intended for wrapping event-loop-facing synchronous herder entry
/// points. A `WARN` here conflates lock-wait time with compute time —
/// [`PhaseTimer`] exists to narrow that further once a slow call is
/// identified.
///
/// Identity-preserving: the wrapped call's return value is forwarded
/// unchanged.
pub fn time_call<R, F: FnOnce() -> R>(label: &'static str, f: F) -> R {
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    if elapsed >= LOCK_SLOW_THRESHOLD {
        tracing::warn!(
            call = label,
            elapsed_ms = elapsed.as_millis() as u64,
            "Slow call (>= {}ms) — possible #1759 contributor",
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }
    result
}

/// Sequential phase-timing recorder for a single event-loop call.
///
/// Use this to break a slow `time_call`ed function into its sub-phases
/// without emitting per-phase log spam:
///
/// ```ignore
/// use henyey_common::tracking::PhaseTimer;
///
/// let mut timer = PhaseTimer::start();
/// do_phase_one();
/// timer.mark("phase_one_ms");
/// do_phase_two();
/// timer.mark("phase_two_ms");
/// do_phase_three();
/// timer.mark("phase_three_ms");
/// timer.finish("my.function");
/// ```
///
/// On [`finish`](Self::finish), exactly one `WARN` line is emitted iff
/// the total time across all phases reaches [`LOCK_SLOW_THRESHOLD`].
/// The WARN carries each phase's duration as a named structured field
/// plus a `total_ms` field, so downstream log tooling can group and
/// rank phases directly.
///
/// Zero log output on the fast path: if the function ran within
/// [`LOCK_SLOW_THRESHOLD`], no event is emitted at all.
///
/// Fast-path cost per `mark`: one `Instant::now()` + one `Duration`
/// subtraction + one `Vec::push`. For the typical 3-5 phase function,
/// well under 1 µs of overhead per call.
///
/// # Field-cardinality cap
///
/// `tracing`'s `warn!` macro accepts a fixed number of structured
/// fields at the macro call site, but this API wants dynamic phase
/// names set by the caller. We emit the phase breakdown via a single
/// pre-formatted string field (`phases`) carrying `k=v k=v …` pairs;
/// structured log backends can split on whitespace. The `total_ms`,
/// `call`, and `threshold_ms` fields are emitted as native structured
/// fields so alerting can key on them directly.
pub struct PhaseTimer {
    start: Instant,
    last_mark: Instant,
    phases: Vec<(&'static str, Duration)>,
}

impl PhaseTimer {
    /// Start a new timer. The first `mark` measures from `start` to
    /// the mark point.
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last_mark: now,
            phases: Vec::with_capacity(8),
        }
    }

    /// Record the elapsed time since the previous `mark` (or since
    /// `start` for the first call) under `name`.
    ///
    /// `name` is a `&'static str` by design: phase names live in the
    /// call-site source, not in per-call allocations, and the WARN
    /// line below reads them cheaply.
    pub fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        let phase = now.saturating_duration_since(self.last_mark);
        self.phases.push((name, phase));
        self.last_mark = now;
    }

    /// Return the number of recorded phases so far. Exposed primarily
    /// for test assertions.
    pub fn phase_count(&self) -> usize {
        self.phases.len()
    }

    /// Borrow the recorded phases. Each entry is `(name, duration)`
    /// in the order `mark()` was called. Zero-allocation.
    pub fn phases(&self) -> &[(&'static str, Duration)] {
        &self.phases
    }

    /// Finish the timer. If the total wall time since `start` reaches
    /// [`LOCK_SLOW_THRESHOLD`], emit a single structured `WARN` line
    /// with every phase as a field plus `total_ms` and `call`.
    ///
    /// The WARN is emitted exactly once per `finish` call; the timer
    /// is consumed (`self`) to prevent accidental double emission.
    pub fn finish(self, label: &'static str) {
        let total = self.start.elapsed();
        if total < LOCK_SLOW_THRESHOLD {
            return;
        }

        // Format phases as `name=ms name=ms …` into a single field so
        // we stay within tracing's fixed-arity macro surface while
        // preserving per-phase visibility.
        let mut phases_field = String::with_capacity(self.phases.len() * 24);
        for (i, (name, dur)) in self.phases.iter().enumerate() {
            if i > 0 {
                phases_field.push(' ');
            }
            phases_field.push_str(name);
            phases_field.push('=');
            // `as_millis()` truncates; for a 250ms+ call that's
            // acceptable and matches the rest of the telemetry.
            phases_field.push_str(&dur.as_millis().to_string());
        }

        tracing::warn!(
            call = label,
            total_ms = total.as_millis() as u64,
            threshold_ms = LOCK_SLOW_THRESHOLD.as_millis() as u64,
            phases = %phases_field,
            "Slow call (>= {}ms) phase breakdown — possible #1759/#1772 contributor",
            LOCK_SLOW_THRESHOLD.as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::{
        field::{Field, Visit},
        subscriber::{with_default, Subscriber},
        Event, Metadata,
    };

    /// Captured tracing event used by the unit tests here. Mirrors the
    /// shape of the per-crate capturing subscribers in
    /// `app/src/app/tracked_lock.rs` and `herder/src/tracked_lock.rs`;
    /// it is intentionally kept module-local to avoid exporting a
    /// test-only helper from a dependency-light utility crate.
    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        level: String,
        call: Option<String>,
        elapsed_ms: Option<u64>,
        total_ms: Option<u64>,
        phases: Option<String>,
    }

    impl Visit for CapturedEvent {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "call" {
                self.call = Some(value.to_string());
            }
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "elapsed_ms" {
                self.elapsed_ms = Some(value);
            } else if field.name() == "total_ms" {
                self.total_ms = Some(value);
            }
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.record_u64(field, value as u64);
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            // `phases` is recorded via the %variable (Display) syntax,
            // which tracing routes through `record_debug` for
            // non-primitive types. Capture it here so tests can
            // assert on the formatted phase-breakdown string.
            if field.name() == "phases" {
                self.phases = Some(format!("{:?}", value).trim_matches('"').to_string());
            }
        }
    }

    #[derive(Clone, Default)]
    struct CapturingSubscriber {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
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

    /// `THRESHOLD + 100ms = 350ms` — comfortably over threshold but
    /// keeps CI fast.
    const TEST_SLOW_DURATION: Duration = Duration::from_millis(350);

    #[test]
    fn time_call_warns_on_slow_call() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();

        with_default(sub, || {
            let _ = time_call("common.test_slow", || {
                std::thread::sleep(TEST_SLOW_DURATION);
                42u32
            });
        });

        let evs = events.lock().unwrap();
        let call_events: Vec<_> = evs
            .iter()
            .filter(|e| e.call.as_deref() == Some("common.test_slow"))
            .collect();
        assert_eq!(call_events.len(), 1);
        assert_eq!(call_events[0].level, "WARN");
        let ms = call_events[0].elapsed_ms.expect("elapsed_ms missing");
        assert!(ms >= LOCK_SLOW_THRESHOLD.as_millis() as u64);
    }

    #[test]
    fn time_call_silent_on_fast_call() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();

        with_default(sub, || {
            let _ = time_call("common.test_fast", || 42u32);
        });

        let evs = events.lock().unwrap();
        let call_events: Vec<_> = evs
            .iter()
            .filter(|e| e.call.as_deref() == Some("common.test_fast"))
            .collect();
        assert_eq!(call_events.len(), 0);
    }

    #[test]
    fn time_call_boundary_inclusive() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();

        with_default(sub, || {
            let _ = time_call("common.test_boundary", || {
                std::thread::sleep(LOCK_SLOW_THRESHOLD + Duration::from_millis(10));
            });
        });

        let evs = events.lock().unwrap();
        let call_events: Vec<_> = evs
            .iter()
            .filter(|e| e.call.as_deref() == Some("common.test_boundary"))
            .collect();
        assert_eq!(call_events.len(), 1);
    }

    #[test]
    fn phase_timer_emits_structured_fields_on_slow_total() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();

        with_default(sub, || {
            let mut timer = PhaseTimer::start();
            std::thread::sleep(TEST_SLOW_DURATION);
            timer.mark("phase_one_ms");
            timer.mark("phase_two_ms");
            timer.mark("phase_three_ms");
            timer.finish("common.test_phases");
        });

        let evs = events.lock().unwrap();
        let phase_events: Vec<_> = evs
            .iter()
            .filter(|e| e.call.as_deref() == Some("common.test_phases"))
            .collect();
        assert_eq!(
            phase_events.len(),
            1,
            "expected exactly one phase WARN; saw: {:?}",
            *evs
        );
        assert_eq!(phase_events[0].level, "WARN");
        let total = phase_events[0].total_ms.expect("total_ms missing");
        assert!(
            total >= LOCK_SLOW_THRESHOLD.as_millis() as u64,
            "total_ms={} must be >= threshold",
            total
        );
        let phases = phase_events[0]
            .phases
            .as_deref()
            .expect("phases field missing");
        assert!(phases.contains("phase_one_ms="), "phases={:?}", phases);
        assert!(phases.contains("phase_two_ms="), "phases={:?}", phases);
        assert!(phases.contains("phase_three_ms="), "phases={:?}", phases);
    }

    #[test]
    fn phase_timer_silent_on_fast_total() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();

        with_default(sub, || {
            let mut timer = PhaseTimer::start();
            timer.mark("phase_one_ms");
            timer.mark("phase_two_ms");
            timer.mark("phase_three_ms");
            timer.finish("common.test_phases_fast");
        });

        let evs = events.lock().unwrap();
        let phase_events: Vec<_> = evs
            .iter()
            .filter(|e| e.call.as_deref() == Some("common.test_phases_fast"))
            .collect();
        assert_eq!(phase_events.len(), 0, "fast path must emit no WARN");
    }

    #[test]
    fn phase_timer_records_all_phases() {
        let mut timer = PhaseTimer::start();
        assert_eq!(timer.phase_count(), 0);
        assert!(timer.phases().is_empty());
        timer.mark("a");
        assert_eq!(timer.phase_count(), 1);
        timer.mark("b");
        assert_eq!(timer.phase_count(), 2);

        let phases = timer.phases();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].0, "a");
        assert_eq!(phases[1].0, "b");
        // Durations are non-negative (no wall-clock assertions)
        assert!(phases[0].1 >= Duration::ZERO);
        assert!(phases[1].1 >= Duration::ZERO);
    }
}
