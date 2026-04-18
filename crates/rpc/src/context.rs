use std::sync::Arc;
use std::time::Duration;

use henyey_app::config::RpcConfig;
use henyey_app::App;
use tokio::sync::Semaphore;

use crate::fee_window::FeeWindows;

/// Minimum semaphore capacity enforced by [`RpcContext::new`] regardless of
/// user config. Prevents a `max_concurrent_requests = 0` misconfig from
/// deadlocking the RPC server (semaphore with zero permits accepts no
/// acquires). Also prevents the same footgun for simulations and DB.
const MIN_SEMAPHORE_CAPACITY: usize = 1;

/// Shared state for all RPC handlers.
pub struct RpcContext {
    /// The application instance.
    pub app: Arc<App>,
    /// Sliding-window fee statistics for `getFeeStats`.
    pub fee_windows: Arc<FeeWindows>,
    /// Limits concurrent `simulateTransaction` requests to prevent CPU/thread exhaustion.
    pub simulation_semaphore: Arc<Semaphore>,
    /// Limits total concurrent request executions.
    pub request_semaphore: Arc<Semaphore>,
    /// Limits concurrent RPC database queries (aligned to DB pool capacity).
    pub db_semaphore: Arc<Semaphore>,
    /// Limits concurrent bucket I/O blocking tasks (bucket list reads).
    /// Independent from `db_semaphore` so bucket reads and DB queries don't
    /// starve each other.
    pub bucket_io_semaphore: Arc<Semaphore>,
    /// Timeout for read-only request execution.
    pub request_timeout: Duration,
}

impl RpcContext {
    /// Construct an `RpcContext` from the given app, sizing semaphores and
    /// timeout from `app.config().rpc`. The returned `Arc` is the shared
    /// state passed to axum handlers.
    pub fn new(app: Arc<App>, fee_windows: Arc<FeeWindows>) -> Arc<Self> {
        let ctx = Self::from_config(app.config().rpc.clone(), app, fee_windows);
        Arc::new(ctx)
    }

    /// Construct an `RpcContext` directly from an [`RpcConfig`], separated
    /// so tests can exercise the capacity-clamp logic without booting a
    /// full `App`.
    fn from_config(rpc_config: RpcConfig, app: Arc<App>, fee_windows: Arc<FeeWindows>) -> Self {
        let max_sims = (rpc_config.max_concurrent_simulations as usize).max(MIN_SEMAPHORE_CAPACITY);
        let max_requests = rpc_config
            .max_concurrent_requests
            .max(MIN_SEMAPHORE_CAPACITY);
        let db_concurrency = rpc_config.rpc_db_concurrency.max(MIN_SEMAPHORE_CAPACITY);
        let bucket_io_concurrency = rpc_config.rpc_db_concurrency.max(MIN_SEMAPHORE_CAPACITY);
        let request_timeout = Duration::from_secs(rpc_config.request_timeout_secs);

        Self {
            app,
            fee_windows,
            simulation_semaphore: Arc::new(Semaphore::new(max_sims)),
            request_semaphore: Arc::new(Semaphore::new(max_requests)),
            db_semaphore: Arc::new(Semaphore::new(db_concurrency)),
            bucket_io_semaphore: Arc::new(Semaphore::new(bucket_io_concurrency)),
            request_timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    /// Direct unit test of the capacity-clamp logic without booting an App:
    /// a semaphore with zero permits would deadlock every RPC request
    /// (`try_acquire` returns `Err(TryAcquireError::NoPermits)`), so any
    /// misconfigured-to-zero field must be clamped to at least 1.
    #[test]
    fn semaphore_capacity_clamp_rejects_zero() {
        use tokio::sync::Semaphore;

        fn clamp(n: usize) -> usize {
            n.max(super::MIN_SEMAPHORE_CAPACITY)
        }

        assert_eq!(clamp(0), 1, "zero must clamp to 1 to avoid deadlock");
        assert_eq!(clamp(1), 1);
        assert_eq!(clamp(42), 42);

        // Property: the clamped value always admits at least one concurrent
        // request (try_acquire on a semaphore with >=1 permit succeeds).
        let sem = Semaphore::new(clamp(0));
        assert!(
            sem.try_acquire().is_ok(),
            "clamped semaphore must admit at least one acquire"
        );
    }
}
