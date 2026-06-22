//! Deterministic tracing-event capture helper for tests.
//!
//! All tests that capture tracing events SHOULD use [`capture_events()`] from
//! this module. The helper holds a process-wide mutex to serialize capture
//! sections, preventing concurrent `Dispatch::new()` → `register_dispatch()` →
//! interest rebuild cycles from racing with each other.
//!
//! Without serialization, the global callsite Interest cache in `tracing-core`
//! can transiently return stale results under parallel test execution, causing
//! events to be silently dropped (observed as flaky 0-event captures in CI —
//! see #3552/#3554 and #3559, which consolidated the per-crate copies into this
//! shared implementation).
//!
//! **Do not** use `tracing::subscriber::with_default` directly in tests —
//! always go through [`capture_events()`].
//!
//! # Global-interest-cache caveat (count assertions)
//!
//! For **default-target** events (plain `warn!`/`info!`/`debug!` with no
//! per-target gating) this helper yields deterministic exact-count capture:
//! the mutex serializes the dispatcher swap and the `Registry` records every
//! event the closure emits.
//!
//! For **target-gated `debug!`** callsites (e.g. an event guarded by
//! `tracing::enabled!(target: "…", Level::DEBUG)`), the per-callsite `Interest`
//! is cached in a *global*, process-wide cache in `tracing-core`. The mutex
//! serializes the capture sections but does **not** flush that cache, so the
//! callsite can be poisoned to `never` by a prior non-interested dispatcher and
//! the gated event then short-circuits before reaching our subscriber. Tests on
//! such callsites MUST therefore treat the event **count** as best-effort
//! (tolerate a 0-event capture) and rely on a separate deterministic
//! invariant — e.g. byte-equality of the computed result with the hook on vs
//! off — as the hard guarantee. Assert exact counts only when the hook fires.
//!
//! This is dev-only test infrastructure, gated behind the `henyey-common`
//! `test-support` feature so it never enters a normal (no-feature) build.

use std::sync::{Arc, Mutex};
use tracing::subscriber::with_default;
use tracing_subscriber::layer::SubscriberExt;

/// Process-wide lock serializing all tracing-capture test sections.
///
/// Ensures only one test at a time mutates the global dispatcher registry and
/// callsite interest cache, so `tracing::enabled!` callsites resolve against a
/// stable interest state for the duration of the captured closure.
static CAPTURE_MUTEX: Mutex<()> = Mutex::new(());

/// A captured tracing event with its fields and message.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    /// Captured non-message fields, as `(name, debug-formatted value)` pairs.
    pub fields: Vec<(String, String)>,
    /// The `message` field, debug-formatted (empty if the event had none).
    pub message: String,
}

/// Run `f` under a capturing subscriber and return all events emitted.
///
/// Holds [`CAPTURE_MUTEX`] for the duration to prevent interference from other
/// tracing-capture tests running in parallel. The subscriber is a
/// `tracing_subscriber::Registry` with a capture layer, which participates
/// correctly in the global interest-rebuild protocol (unlike a hand-rolled bare
/// `Subscriber`).
///
/// See the module docs for the count-assertion caveat on target-gated `debug!`
/// callsites.
pub fn capture_events<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
    let _lock = CAPTURE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    let layer = CaptureLayer::new();
    let events = layer.events.clone();
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    with_default(subscriber, f);
    let result = events.lock().unwrap().clone();
    result
}

// ---------------------------------------------------------------------------
// Internal capture layer implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl CaptureLayer {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            fields: visitor.fields,
            message: visitor.message,
        });
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: Vec<(String, String)>,
    message: String,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields
                .push((field.name().to_string(), format!("{:?}", value)));
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-test for the shared helper: emit a plain **default-target** `info!`
    /// (no `EnvFilter`, no target gating) inside `capture_events` and assert
    /// exactly one event is captured with the expected message and field. A
    /// default-target event has no global-interest-cache hazard, so the
    /// exact-count assertion is deterministic — this is the count guarantee the
    /// consumer crates rely on.
    #[test]
    fn capture_events_records_default_target_event() {
        let events = capture_events(|| {
            tracing::info!(k = 1u64, "captured msg");
        });

        assert_eq!(
            events.len(),
            1,
            "exactly one default-target event must be captured"
        );
        assert!(
            events[0].message.contains("captured msg"),
            "captured message should contain the emitted text, got {:?}",
            events[0].message
        );
        assert!(
            events[0].fields.iter().any(|(k, v)| k == "k" && v == "1"),
            "the `k` field should be captured, got {:?}",
            events[0].fields
        );
    }
}
