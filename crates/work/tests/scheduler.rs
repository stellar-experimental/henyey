//! Integration tests for the work scheduler.
//!
//! These tests verify the core functionality of the work scheduler including:
//! - Dependency ordering
//! - Retry behavior
//! - Cancellation handling
//! - Metrics and snapshots

use std::sync::{Arc, Mutex};
use std::time::Duration;

use henyey_work::{Work, WorkContext, WorkOutcome, WorkScheduler, WorkSchedulerConfig, WorkState};

// ============================================================================
// Test Work Item Implementations
// ============================================================================

/// A simple work item that logs its name when executed.
struct LogWork {
    name: String,
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Work for LogWork {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        self.log.lock().unwrap().push(self.name.clone());
        WorkOutcome::Success
    }
}

/// A work item that fails on the first attempt and succeeds on retry.
struct RetryWork {
    name: String,
    attempts: Arc<Mutex<u32>>,
}

#[async_trait::async_trait]
impl Work for RetryWork {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        let mut attempts = self.attempts.lock().unwrap();
        *attempts += 1;
        if *attempts == 1 {
            WorkOutcome::Retry {
                delay: Duration::from_millis(10),
            }
        } else {
            WorkOutcome::Success
        }
    }
}

/// A work item that checks for cancellation periodically.
struct CancellableWork {
    name: String,
}

#[async_trait::async_trait]
impl Work for CancellableWork {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self, ctx: &WorkContext) -> WorkOutcome {
        for _ in 0..5u32 {
            if ctx.is_cancelled() {
                return WorkOutcome::Cancelled;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        WorkOutcome::Success
    }
}

// ============================================================================
// Dependency Ordering Tests
// ============================================================================

/// Verifies that work items execute in dependency order.
#[tokio::test]
async fn test_dependency_ordering() {
    let log = Arc::new(Mutex::new(Vec::new()));

    let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
        max_concurrency: 2,
        retry_delay: Duration::from_millis(1),
        event_tx: None,
    });

    // Create work item A with no dependencies
    let a = scheduler.add_work(
        Box::new(LogWork {
            name: "a".to_string(),
            log: Arc::clone(&log),
        }),
        vec![],
        0,
    );

    // Create work item B that depends on A
    let _b = scheduler.add_work(
        Box::new(LogWork {
            name: "b".to_string(),
            log: Arc::clone(&log),
        }),
        vec![a],
        0,
    );

    scheduler.run_until_done().await;

    // Verify A executed before B
    let log = log.lock().unwrap();
    assert_eq!(log.as_slice(), ["a", "b"]);
}

// ============================================================================
// Retry Tests
// ============================================================================

/// Verifies that work items are retried when they return Retry outcome.
#[tokio::test]
async fn test_retry_then_success() {
    let attempts = Arc::new(Mutex::new(0u32));

    let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
        max_concurrency: 1,
        retry_delay: Duration::from_millis(1),
        event_tx: None,
    });

    let _ = scheduler.add_work(
        Box::new(RetryWork {
            name: "retry".to_string(),
            attempts: Arc::clone(&attempts),
        }),
        vec![],
        1, // Allow 1 retry
    );

    scheduler.run_until_done().await;

    // Verify the work was attempted twice (initial + 1 retry)
    let attempts = *attempts.lock().unwrap();
    assert_eq!(attempts, 2);
}

// ============================================================================
// Cancellation Tests
// ============================================================================

/// Verifies that work items can be cancelled via external token.
#[tokio::test]
async fn test_cancel_work() {
    let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
        max_concurrency: 1,
        retry_delay: Duration::from_millis(1),
        event_tx: None,
    });

    let id = scheduler.add_work(
        Box::new(CancellableWork {
            name: "cancel".to_string(),
        }),
        vec![],
        0,
    );

    // Set up external cancellation after a short delay
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(8)).await;
        cancel_clone.cancel();
    });

    scheduler.run_until_done_with_cancel(cancel).await;

    // Verify the work was cancelled
    assert_eq!(scheduler.state(id), Some(WorkState::Cancelled));
}

// ============================================================================
// Metrics and Snapshot Tests
// ============================================================================

/// Verifies that metrics and snapshots accurately reflect scheduler state.
#[tokio::test]
async fn test_metrics_snapshot() {
    let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
        max_concurrency: 1,
        retry_delay: Duration::from_millis(1),
        event_tx: None,
    });

    let _ = scheduler.add_work(
        Box::new(LogWork {
            name: "metrics".to_string(),
            log: Arc::new(Mutex::new(Vec::new())),
        }),
        vec![],
        0,
    );

    scheduler.run_until_done().await;

    // Verify metrics
    let metrics = scheduler.metrics();
    assert_eq!(metrics.total, 1);
    assert_eq!(metrics.success, 1);
    assert_eq!(metrics.failed, 0);
}
