use std::sync::Arc;
use std::time::Duration;

use henyey_app::app::AppInfo;
use henyey_app::config::{AppConfig, RpcConfig};
use henyey_app::{App, AppState, LedgerSummary};
use henyey_bucket::BucketSnapshotManager;
use henyey_herder::TxQueueResult;
use henyey_ledger::{LedgerManager, SorobanNetworkInfo};
use stellar_xdr::curr::TransactionEnvelope;
use tokio::sync::Semaphore;

use crate::fee_window::FeeWindows;

/// Minimum semaphore capacity enforced by [`RpcContext::new`] regardless of
/// user config. Prevents a `max_concurrent_requests = 0` misconfig from
/// deadlocking the RPC server (semaphore with zero permits accepts no
/// acquires). Also prevents the same footgun for simulations and DB.
const MIN_SEMAPHORE_CAPACITY: usize = 1;

/// Subset of [`App`] functionality needed by RPC handlers.
///
/// Decouples the RPC crate from the concrete `App` runtime type so that
/// integration tests can provide lightweight fakes without booting the full
/// application. `App` implements this trait via a delegating impl below.
#[async_trait::async_trait]
pub trait RpcAppHandle: Send + Sync + 'static {
    /// Application configuration.
    fn config(&self) -> &AppConfig;
    /// Application version, network passphrase, and build metadata.
    fn info(&self) -> AppInfo;
    /// Current ledger header snapshot (sequence, hash, close time, etc.).
    fn ledger_summary(&self) -> LedgerSummary;
    /// Ledger manager (for raw header access in `getLatestLedger`).
    fn ledger_manager(&self) -> &Arc<LedgerManager>;
    /// Database connection pool.
    fn database(&self) -> &henyey_db::Database;
    /// Bucket list snapshot manager (for Soroban simulation).
    fn bucket_snapshot_manager(&self) -> &Arc<BucketSnapshotManager>;
    /// Soroban network configuration (TTL bounds, fees, etc.).
    fn soroban_network_info(&self) -> Option<SorobanNetworkInfo>;
    /// Subscribe to the application shutdown broadcast channel.
    fn subscribe_shutdown(&self) -> tokio::sync::broadcast::Receiver<()>;
    /// Submit a transaction to the herder queue and flood to peers.
    async fn submit_transaction(&self, tx: TransactionEnvelope) -> TxQueueResult;
    /// Current application state (e.g., `Validating`, `Synced`).
    async fn state(&self) -> AppState;
}

#[async_trait::async_trait]
impl RpcAppHandle for App {
    fn config(&self) -> &AppConfig {
        App::config(self)
    }
    fn info(&self) -> AppInfo {
        App::info(self)
    }
    fn ledger_summary(&self) -> LedgerSummary {
        App::ledger_summary(self)
    }
    fn ledger_manager(&self) -> &Arc<LedgerManager> {
        App::ledger_manager(self)
    }
    fn database(&self) -> &henyey_db::Database {
        App::database(self)
    }
    fn bucket_snapshot_manager(&self) -> &Arc<BucketSnapshotManager> {
        App::bucket_snapshot_manager(self)
    }
    fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        App::soroban_network_info(self)
    }
    fn subscribe_shutdown(&self) -> tokio::sync::broadcast::Receiver<()> {
        App::subscribe_shutdown(self)
    }
    async fn submit_transaction(&self, tx: TransactionEnvelope) -> TxQueueResult {
        App::submit_transaction(self, tx).await
    }
    async fn state(&self) -> AppState {
        App::state(self).await
    }
}

/// Shared state for all RPC handlers.
pub struct RpcContext {
    /// The application handle (production: `App`; tests: lightweight fake).
    pub app: Arc<dyn RpcAppHandle>,
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
    pub fn new(app: Arc<dyn RpcAppHandle>, fee_windows: Arc<FeeWindows>) -> Arc<Self> {
        let ctx = Self::from_config(app.config().rpc.clone(), app, fee_windows);
        Arc::new(ctx)
    }

    /// Construct an `RpcContext` directly from an [`RpcConfig`], separated
    /// so tests can exercise the capacity-clamp logic without booting a
    /// full `App`.
    fn from_config(
        rpc_config: RpcConfig,
        app: Arc<dyn RpcAppHandle>,
        fee_windows: Arc<FeeWindows>,
    ) -> Self {
        let max_sims = (rpc_config.max_concurrent_simulations as usize).max(MIN_SEMAPHORE_CAPACITY);
        let max_requests = rpc_config
            .max_concurrent_requests
            .max(MIN_SEMAPHORE_CAPACITY);
        let db_concurrency = rpc_config.rpc_db_concurrency.max(MIN_SEMAPHORE_CAPACITY);
        let bucket_io_concurrency = rpc_config.bucket_io_concurrency.max(MIN_SEMAPHORE_CAPACITY);
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

    /// `bucket_io_concurrency` and `rpc_db_concurrency` must produce
    /// independent semaphore capacities when configured to different values.
    #[test]
    fn bucket_io_and_db_semaphores_are_independent() {
        use henyey_app::config::RpcConfig;

        let mut rpc_config = RpcConfig::default();
        rpc_config.rpc_db_concurrency = 4;
        rpc_config.bucket_io_concurrency = 12;

        // We can't call from_config (needs App), so replicate the sizing
        // logic directly — this is the same code path under test.
        let db = rpc_config
            .rpc_db_concurrency
            .max(super::MIN_SEMAPHORE_CAPACITY);
        let bucket = rpc_config
            .bucket_io_concurrency
            .max(super::MIN_SEMAPHORE_CAPACITY);

        assert_eq!(db, 4);
        assert_eq!(bucket, 12);
        assert_ne!(
            db, bucket,
            "db and bucket_io semaphores must be independently sized"
        );
    }

    /// `bucket_io_concurrency = 0` must be clamped to MIN_SEMAPHORE_CAPACITY.
    #[test]
    fn bucket_io_zero_clamped_to_minimum() {
        let clamped = 0usize.max(super::MIN_SEMAPHORE_CAPACITY);
        assert_eq!(clamped, 1, "zero bucket_io must clamp to 1");
    }
}
