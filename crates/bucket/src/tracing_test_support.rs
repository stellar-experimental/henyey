//! Test helper for capturing tracing events deterministically.
//!
//! All tests that capture tracing events MUST use [`capture_events()`] from
//! this module. The helper holds a process-wide mutex to serialize capture
//! sections, preventing concurrent `Dispatch::new()` → `register_dispatch()` →
//! interest rebuild cycles from racing with each other.
//!
//! Without serialization, the global callsite Interest cache in `tracing-core`
//! can transiently return stale results under parallel test execution, causing
//! events to be silently dropped (observed as flaky 0-event captures in CI —
//! see #3552/#3554 and the sibling helper in `henyey-history`).
//!
//! **Do not** use `tracing::subscriber::with_default` directly in tests —
//! always go through [`capture_events()`].

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
    /// Captured non-message fields. Retained for parity with the sibling
    /// `henyey-history` helper and for future field-asserting capture tests;
    /// the current bucket test matches only on `message`.
    #[allow(dead_code)]
    pub fields: Vec<(String, String)>,
    pub message: String,
}

/// Run `f` under a capturing subscriber and return all events emitted.
///
/// Holds [`CAPTURE_MUTEX`] for the duration to prevent interference from other
/// tracing-capture tests running in parallel. The subscriber is a
/// `tracing_subscriber::Registry` with a capture layer, which participates
/// correctly in the global interest-rebuild protocol (unlike a hand-rolled bare
/// `Subscriber`).
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
