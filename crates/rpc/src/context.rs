use std::sync::Arc;
use std::time::Duration;

use henyey_app::App;
use tokio::sync::Semaphore;

use crate::fee_window::FeeWindows;

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
    /// Timeout for read-only request execution.
    pub request_timeout: Duration,
}

impl RpcContext {
    /// Construct an `RpcContext` from the given app, sizing semaphores and
    /// timeout from `app.config().rpc`. The returned `Arc` is the shared
    /// state passed to axum handlers.
    pub fn new(app: Arc<App>, fee_windows: Arc<FeeWindows>) -> Arc<Self> {
        let rpc_config = &app.config().rpc;
        let max_sims = rpc_config.max_concurrent_simulations.max(1) as usize;
        let max_requests = rpc_config.max_concurrent_requests.max(1);
        let db_concurrency = rpc_config.rpc_db_concurrency.max(1);
        let request_timeout = Duration::from_secs(rpc_config.request_timeout_secs);

        Arc::new(Self {
            app,
            fee_windows,
            simulation_semaphore: Arc::new(Semaphore::new(max_sims)),
            request_semaphore: Arc::new(Semaphore::new(max_requests)),
            db_semaphore: Arc::new(Semaphore::new(db_concurrency)),
            request_timeout,
        })
    }
}
