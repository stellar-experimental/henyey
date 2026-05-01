//! Spawn helpers with structured error logging.
//!
//! Provides:
//! - [`spawn_blocking_logged`] and [`await_blocking_logged`] — thin wrappers
//!   around `tokio::task::spawn_blocking` that log any `JoinError`
//!   (differentiating panic from cancellation) at ERROR level.
//! - [`await_join_logged`] — task-agnostic `JoinHandle` observer (panics at
//!   ERROR, cancellation at WARN).
//! - [`spawn_observed`] — fire-and-forget spawn with structured panic logging.

use std::future::Future;

use tokio::task::JoinHandle;
use tracing::{error, warn};

/// Await a `spawn_blocking` handle, logging any `JoinError` with context.
///
/// Returns `Ok(value)` on success, `Err(JoinError)` on panic or cancellation.
/// On error, logs the error with the provided `context` string, differentiating
/// panic from non-panic failures.
///
/// Callers that need domain-specific context (e.g., `ledger_seq`) should log
/// an additional line when handling the `Err` case.
pub async fn await_blocking_logged<T>(
    context: &str,
    handle: JoinHandle<T>,
) -> Result<T, tokio::task::JoinError> {
    match handle.await {
        Ok(val) => Ok(val),
        Err(e) if e.is_panic() => {
            error!(error = %e, "{context} panicked in spawn_blocking");
            Err(e)
        }
        Err(e) => {
            error!(error = %e, "spawn_blocking join error for {context}");
            Err(e)
        }
    }
}

/// Spawn a blocking closure and log any `JoinError`/panic.
///
/// Convenience wrapper: spawns on the blocking pool and immediately awaits.
/// Use [`await_blocking_logged`] when you need to do work between spawn and
/// await (e.g., setting a phase marker).
pub async fn spawn_blocking_logged<T, F>(context: &str, f: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    await_blocking_logged(context, tokio::task::spawn_blocking(f)).await
}

/// Await any `JoinHandle`, logging panics at ERROR and cancellation at WARN
/// with structured fields.
///
/// Works for handles from both `tokio::spawn` and `tokio::task::spawn_blocking`.
/// The `context` parameter is emitted as a structured `task` field for log
/// aggregation.
///
/// Unlike [`await_blocking_logged`] (which logs all errors at ERROR), this
/// function distinguishes panic (ERROR) from cancellation (WARN) — appropriate
/// for fire-and-forget tasks where shutdown-driven cancellation is expected.
///
/// # Limitations
///
/// In release builds with `panic = "abort"`, task panics abort the process
/// before this function can observe them. This helper is most useful in
/// dev/test builds and as documentation of intent.
pub async fn await_join_logged<T>(
    context: &str,
    handle: JoinHandle<T>,
) -> Result<T, tokio::task::JoinError> {
    match handle.await {
        Ok(val) => Ok(val),
        Err(e) if e.is_panic() => {
            error!(task = context, error = %e, "spawned task panicked");
            Err(e)
        }
        Err(e) => {
            warn!(task = context, error = %e, "spawned task cancelled");
            Err(e)
        }
    }
}

/// Spawn a short-lived async task with structured panic observability.
///
/// Spawns a lightweight monitor task that awaits the primary task's
/// `JoinHandle` and logs any panic (ERROR) or cancellation (WARN) via
/// structured tracing with a `task` field.
///
/// # Intended use
///
/// Use for **short-lived, detached background tasks** where the caller
/// does not need the return value but should be alerted to panics.
/// Not suitable for long-lived service tasks (overlay manager, HTTP server)
/// where a panic should be treated as fatal.
///
/// # Limitations
///
/// - The monitor task may be cancelled during runtime shutdown alongside
///   the observed task, preventing the log emission.
/// - In release builds (`panic = "abort"`), the process aborts before
///   observation. See [`await_join_logged`] docs.
pub fn spawn_observed(context: &'static str, fut: impl Future<Output = ()> + Send + 'static) {
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        let _ = await_join_logged(context, handle).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::{
        field::{Field, Visit},
        Event, Metadata, Subscriber,
    };

    /// Captured tracing event for asserting log output in tests.
    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        level: String,
        task: Option<String>,
        message: Option<String>,
    }

    impl Visit for CapturedEvent {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "task" => self.task = Some(value.to_string()),
                _ => {}
            }
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = Some(format!("{value:?}").trim_matches('"').to_string());
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

    #[tokio::test]
    async fn test_spawn_blocking_logged_success() {
        let result = spawn_blocking_logged("test-success", || 42).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_spawn_blocking_logged_panic() {
        let result = spawn_blocking_logged("test-panic", || -> () { panic!("boom") }).await;
        let err = result.unwrap_err();
        assert!(err.is_panic());
    }

    #[tokio::test]
    async fn test_await_blocking_logged_success() {
        let handle = tokio::task::spawn_blocking(|| "hello");
        let result = await_blocking_logged("test-await", handle).await;
        assert_eq!(result.unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_await_blocking_logged_panic() {
        let handle = tokio::task::spawn_blocking(|| -> i32 { panic!("kaboom") });
        let result = await_blocking_logged("test-await-panic", handle).await;
        let err = result.unwrap_err();
        assert!(err.is_panic());
    }

    #[tokio::test]
    async fn test_await_join_logged_panic() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let _guard = tracing::subscriber::set_default(sub);

        let handle = tokio::spawn(async { panic!("boom") });
        let result = await_join_logged("test-task", handle).await;

        drop(_guard);

        let err = result.unwrap_err();
        assert!(err.is_panic());

        let evs = events.lock().unwrap();
        let panic_events: Vec<_> = evs
            .iter()
            .filter(|e| e.task.as_deref() == Some("test-task"))
            .collect();
        assert_eq!(panic_events.len(), 1);
        assert_eq!(panic_events[0].level, "ERROR");
    }

    #[tokio::test]
    async fn test_await_join_logged_cancel() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let _guard = tracing::subscriber::set_default(sub);

        let handle = tokio::spawn(async {
            // Task that would run forever
            std::future::pending::<()>().await
        });
        handle.abort();
        let result = await_join_logged("test-cancel", handle).await;

        drop(_guard);

        let err = result.unwrap_err();
        assert!(err.is_cancelled());

        let evs = events.lock().unwrap();
        let cancel_events: Vec<_> = evs
            .iter()
            .filter(|e| e.task.as_deref() == Some("test-cancel"))
            .collect();
        assert_eq!(cancel_events.len(), 1);
        assert_eq!(cancel_events[0].level, "WARN");
    }

    #[tokio::test]
    async fn test_spawn_observed_panic() {
        let sub = CapturingSubscriber::default();
        let events = sub.events.clone();
        let _guard = tracing::subscriber::set_default(sub);

        spawn_observed("test-observed", async { panic!("boom") });

        // Poll for the event with retry to avoid flakiness
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let evs = events.lock().unwrap();
            if evs
                .iter()
                .any(|e| e.task.as_deref() == Some("test-observed"))
            {
                break;
            }
        }

        drop(_guard);

        let evs = events.lock().unwrap();
        let observed_events: Vec<_> = evs
            .iter()
            .filter(|e| e.task.as_deref() == Some("test-observed"))
            .collect();
        assert_eq!(
            observed_events.len(),
            1,
            "expected one ERROR event for test-observed, got: {observed_events:?}"
        );
        assert_eq!(observed_events[0].level, "ERROR");
    }
}
