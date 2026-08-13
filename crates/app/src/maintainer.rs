//! Database maintenance scheduler for henyey.
//!
//! The Maintainer is responsible for periodically cleaning up old data from the
//! database to prevent unbounded growth. It runs in the background and performs
//! incremental deletions to avoid long blocking operations.
//!
//! # Maintained Tables
//!
//! - **ledgerheaders**: Old ledger headers are pruned, keeping enough history
//!   to support checkpoint publishing
//! - **scphistory**: Old SCP consensus envelopes are pruned
//! - **scpquorums**: Old quorum sets are pruned based on their last-seen ledger
//! - **ledger_close_meta**: Old full ledger close metadata (RPC retention window)
//! - **txhistory / txsets / txresults**: Old transaction data (publish + RPC retention)
//!
//! # Configuration
//!
//! Maintenance is controlled by two parameters:
//!
//! - `maintenance_period`: How often to run maintenance (default: 4 hours)
//! - `maintenance_count`: Maximum entries to delete per cycle (default: 50000)
//!
//! # Usage
//!
//! ```ignore
//! use henyey_app::maintainer::Maintainer;
//!
//! let maintainer = Maintainer::new(app.clone());
//! maintainer.start().await;
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Default maintenance period (4 hours).
pub const DEFAULT_MAINTENANCE_PERIOD: Duration = Duration::from_secs(4 * 60 * 60);

/// Default number of entries to delete per maintenance cycle.
pub const DEFAULT_MAINTENANCE_COUNT: u32 = 50000;

/// Maximum checkpoint-distance for publish queue entries before eviction.
///
/// Entries more than this many checkpoint intervals behind the LCL are
/// permanently abandoned — they will never be published. This prevents
/// unbounded retention from persistently failing archive publishing.
///
/// 30 checkpoints × 64 ledgers × ~5s ≈ ~2.7 hours of checkpoint distance.
/// This is intentionally larger than `PUBLISH_QUEUE_MAX_SIZE` (16) used
/// during catchup, since normal operation may have legitimate publishing
/// backlogs (e.g., slow archive uploads).
///
/// **Parity divergence**: stellar-core does not evict stale publish queue
/// entries. See `crates/app/PARITY_STATUS.md`.
pub const MAX_PUBLISH_QUEUE_CHECKPOINT_DISTANCE: u32 = 30;

/// Checkpoint frequency — delegates to the runtime-configurable value.
pub fn checkpoint_frequency() -> u32 {
    henyey_history::checkpoint_frequency()
}

/// Configuration for automatic database maintenance.
#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    /// How often to run maintenance.
    pub period: Duration,
    /// Maximum entries to delete per cycle.
    pub count: u32,
    /// Whether maintenance is enabled.
    pub enabled: bool,
    /// RPC retention window in ledgers. When set, the maintainer will also clean
    /// up RPC-only tables (`events`, `ledger_close_meta`). Transaction history
    /// tables (`txhistory`, `txsets`, `txresults`) and ledger headers are always
    /// pruned at the publish-safe threshold; when RPC is also configured, they
    /// use the more conservative of the publish-safe and RPC thresholds.
    pub rpc_retention_window: Option<u32>,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            period: DEFAULT_MAINTENANCE_PERIOD,
            count: DEFAULT_MAINTENANCE_COUNT,
            enabled: true,
            rpc_retention_window: None,
        }
    }
}

impl MaintenanceConfig {
    /// Create a disabled maintenance config.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Set the RPC retention window.
    pub fn with_rpc_retention_window(mut self, window: u32) -> Self {
        self.rpc_retention_window = Some(window);
        self
    }
}

/// Background database maintainer.
///
/// Periodically cleans up old ledger headers and SCP history to prevent
/// unbounded database growth.
pub struct Maintainer {
    database: Arc<henyey_db::Database>,
    config: MaintenanceConfig,
    shutdown_rx: watch::Receiver<bool>,
    /// Function to get the current LCL and minimum queued checkpoint
    get_ledger_bounds: Arc<dyn Fn() -> (u32, Option<u32>) + Send + Sync>,
}

impl Maintainer {
    /// Create a new maintainer with default configuration.
    ///
    /// # Arguments
    ///
    /// * `database` - The database to maintain
    /// * `shutdown_rx` - Receiver for shutdown signals
    /// * `get_ledger_bounds` - Function returning (lcl, min_queued_checkpoint)
    pub fn new<F>(
        database: Arc<henyey_db::Database>,
        shutdown_rx: watch::Receiver<bool>,
        get_ledger_bounds: F,
    ) -> Self
    where
        F: Fn() -> (u32, Option<u32>) + Send + Sync + 'static,
    {
        Self {
            database,
            config: MaintenanceConfig::default(),
            shutdown_rx,
            get_ledger_bounds: Arc::new(get_ledger_bounds),
        }
    }

    /// Create a maintainer with custom configuration.
    pub fn with_config<F>(
        database: Arc<henyey_db::Database>,
        config: MaintenanceConfig,
        shutdown_rx: watch::Receiver<bool>,
        get_ledger_bounds: F,
    ) -> Self
    where
        F: Fn() -> (u32, Option<u32>) + Send + Sync + 'static,
    {
        Self {
            database,
            config,
            shutdown_rx,
            get_ledger_bounds: Arc::new(get_ledger_bounds),
        }
    }

    /// Start the maintenance loop.
    ///
    /// This method runs until a shutdown signal is received. It periodically
    /// performs maintenance according to the configured period.
    pub async fn start(mut self) {
        if !self.config.enabled {
            info!("Database maintenance disabled");
            return;
        }

        if self.config.period.is_zero() || self.config.count == 0 {
            info!("Database maintenance disabled (period or count is zero)");
            return;
        }

        info!(
            period_secs = self.config.period.as_secs(),
            count = self.config.count,
            "Starting database maintainer"
        );

        // Check if we can keep up with ledger production
        // Assuming ~5 second ledger close time
        let ledgers_per_period = self.config.period.as_secs() / 5;
        if self.config.count <= ledgers_per_period as u32 {
            warn!(
                count = self.config.count,
                ledgers_per_period = ledgers_per_period,
                "Maintenance may not keep up: count <= ledgers produced per period"
            );
        }

        let mut interval = build_maintenance_interval(self.config.period);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let db = Arc::clone(&self.database);
                    let config_count = self.config.count;
                    let rpc_retention_window = self.config.rpc_retention_window;
                    let get_bounds = Arc::clone(&self.get_ledger_bounds);

                    match henyey_common::spawn_blocking_logged(
                        "maintainer-cycle",
                        move || {
                            let (lcl, min_queued) = get_bounds();
                            let start = std::time::Instant::now();
                            info!("Performing database maintenance");

                            run_maintenance(
                                &db,
                                lcl,
                                min_queued,
                                rpc_retention_window,
                                config_count,
                            );

                            let elapsed = start.elapsed();
                            if elapsed > Duration::from_secs(10) {
                                warn!(
                                    elapsed_ms = elapsed.as_millis(),
                                    "Maintenance took too long; consider increasing AUTOMATIC_MAINTENANCE_COUNT \
                                     or performing manual database maintenance"
                                );
                            } else {
                                debug!(elapsed_ms = elapsed.as_millis(), "Maintenance complete");
                            }
                        },
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(join_err) if join_err.is_panic() => {
                            std::panic::resume_unwind(join_err.into_panic());
                        }
                        Err(join_err) => {
                            warn!("Maintenance task cancelled: {join_err}");
                        }
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        info!("Maintainer shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Perform one maintenance cycle.
    ///
    /// This method can also be called directly for manual maintenance.
    pub fn perform_maintenance(&self) {
        let start = std::time::Instant::now();
        info!("Performing database maintenance");

        let (lcl, min_queued) = (self.get_ledger_bounds)();
        run_maintenance(
            &self.database,
            lcl,
            min_queued,
            self.config.rpc_retention_window,
            self.config.count,
        );

        let elapsed = start.elapsed();
        if elapsed > Duration::from_secs(10) {
            warn!(
                elapsed_ms = elapsed.as_millis(),
                "Maintenance took too long; consider increasing AUTOMATIC_MAINTENANCE_COUNT \
                 or performing manual database maintenance"
            );
        } else {
            debug!(elapsed_ms = elapsed.as_millis(), "Maintenance complete");
        }
    }

    /// Perform maintenance for a specific count of entries.
    ///
    /// This is useful for manual maintenance with a different batch size.
    pub fn perform_maintenance_with_count(&self, count: u32) {
        let (lcl, min_queued) = (self.get_ledger_bounds)();
        run_maintenance(
            &self.database,
            lcl,
            min_queued,
            self.config.rpc_retention_window,
            count,
        );
    }
}

/// Build the periodic maintenance interval.
///
/// Unlike `tokio::time::interval`, whose first `tick()` resolves
/// **immediately** (firing a full 50k-row DELETE maintenance cycle at
/// startup), this uses `interval_at(now + period, period)` so the first
/// maintenance runs one full `period` in. This keeps the maintainer's write
/// burst from coinciding with the startup near-tip back-fill on the
/// restore-from-disk path, where per-slot `scp-persist-{slot}` purge txns +
/// back-to-back per-ledger close persists already contend for SQLite's single
/// WAL writer (see #3702).
///
/// This shifts only **when** periodic pruning first runs — the period value,
/// missed-tick behavior, and all pruning semantics are unchanged. There is no
/// consensus/hash/ledger effect (maintenance cadence only).
fn build_maintenance_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

/// Core maintenance logic shared between `App::perform_maintenance` and `Maintainer`.
///
/// Deletes old data from the database to prevent unbounded growth.
///
/// When `min_queued` is `Some`, publish queue staleness eviction runs first:
/// entries whose checkpoint-distance from the LCL exceeds
/// [`MAX_PUBLISH_QUEUE_CHECKPOINT_DISTANCE`] are permanently removed, and
/// `min_queued` is re-read from the (now-trimmed) queue. This prevents
/// persistently failing archive publishing from pinning the pruning
/// threshold indefinitely.
///
/// Tables are pruned based on their retention class:
/// - **Publish-only** (SCP history): pruned at `publish_safe_lmin` (stellar-core parity).
/// - **Publish + RPC** (headers, tx history/sets/results): always pruned at the
///   more conservative of `publish_safe_lmin` and `rpc_lmin`. These tables serve
///   both checkpoint publishing (`txsets`, `txresults`, headers) and RPC queries
///   (`txhistory`, headers), so we keep data needed by both consumers.
/// - **RPC-only** (events, close meta): pruned at `rpc_lmin` only when RPC is configured.
/// Maximum distinct ledgers deleted per SQL statement during maintenance.
///
/// Each `delete_old_*` call bounded by this runs as its own short implicit
/// transaction, so SQLite's single WAL write lock is released between chunks
/// and the ledger-close persist path can interleave. Without this bound, the
/// first maintenance pass after a checkpoint publish completes (which unlocks
/// ~64 ledgers of txsets/txresults blobs for deletion on every node within
/// the same tick) held the write lock for 21-28 s under MaxTPS load, stalling
/// ledger closes globally (2026-07-03 frontier run, `maintenance-delete`
/// write-ctx forensics).
const MAINTENANCE_CHUNK_LEDGERS: u32 = 2;

/// Pause between maintenance delete chunks. Gives the ledger-close persist
/// transaction (and other writers) a window to acquire the WAL write lock
/// between chunks.
const MAINTENANCE_CHUNK_PAUSE: Duration = Duration::from_millis(25);

/// Bounded retry count for a transiently-busy maintenance delete chunk.
/// Deliberately small and maintenance-local — NOT tied to
/// `commit_with_busy_retry`'s consensus-continuation semantics (that path
/// escalates to a recoverable shutdown when its budget is exhausted, which is
/// disproportionate for a routine retention trim: the node should simply wait
/// for the next scheduled maintenance cycle). See #3772.
const MAINTENANCE_DELETE_MAX_ATTEMPTS: u32 = 3;

/// Run one table's maintenance deletion in bounded chunks.
///
/// Calls `delete_chunk` with at most [`MAINTENANCE_CHUNK_LEDGERS`] ledgers per
/// call until it reports no more rows deleted or `total_budget` ledgers have
/// been requested, pausing [`MAINTENANCE_CHUNK_PAUSE`] between chunks. Errors
/// are logged and stop the loop (matching the previous per-table
/// warn-and-continue behavior). Returns total rows deleted.
fn delete_in_chunks(
    total_budget: u32,
    label: &str,
    mut delete_chunk: impl FnMut(u32) -> Result<u32, henyey_db::DbError>,
) -> u64 {
    let mut remaining = total_budget;
    let mut total_rows = 0u64;
    while remaining > 0 {
        let chunk = remaining.min(MAINTENANCE_CHUNK_LEDGERS);
        // Retry a transiently-busy chunk a bounded number of times before
        // giving up on this table's trim (#3772). Losing the SQLite
        // write-lock race for the whole `busy_timeout` window must not
        // silently abort the entire retention pass — that starves exactly the
        // trimming that would relieve the contention. A genuine (non-transient)
        // error still fails closed immediately, and the budget is small so a
        // fully-stuck table costs at most MAINTENANCE_DELETE_MAX_ATTEMPTS
        // attempts. This is deliberately NOT `commit_with_busy_retry`, whose
        // exhausted-budget path triggers a recoverable shutdown — overkill for
        // a routine trim that can wait for the next maintenance cycle.
        let mut outcome: Result<u32, henyey_db::DbError> = Ok(0);
        let mut attempts = 0u32;
        while attempts < MAINTENANCE_DELETE_MAX_ATTEMPTS {
            attempts += 1;
            // Per-chunk WAL write-lock instrumentation (#3702): the guard must
            // wrap a single bounded statement, not the whole pass, so a
            // reported long hold is a real single-transaction hold. Each retry
            // attempt is its own single delete statement, hence its own guard.
            let _write_ctx = henyey_common::WriteCtxGuard::new("maintenance-delete");
            match delete_chunk(chunk) {
                Ok(rows) => {
                    outcome = Ok(rows);
                    break;
                }
                Err(e) if crate::app::is_transient_db_busy(&e) => {
                    outcome = Err(e);
                    drop(_write_ctx);
                    // Back off before retrying, unless the budget is exhausted.
                    if attempts < MAINTENANCE_DELETE_MAX_ATTEMPTS {
                        // #3802: one increment per attempt that will actually be
                        // followed by another. This is the series that answers
                        // "did #3772's bounded retry convert lock-loss into
                        // successful trims?" — previously a successful retry
                        // emitted no signal whatsoever.
                        crate::metrics::record_db_busy_retry_attempt(
                            crate::metrics::SITE_RETENTION_TRIM,
                        );
                        std::thread::sleep(MAINTENANCE_CHUNK_PAUSE);
                    }
                }
                Err(e) => {
                    // Non-transient error: fail closed immediately, no retry.
                    outcome = Err(e);
                    break;
                }
            }
        }

        match outcome {
            Ok(0) => break,
            Ok(rows) => {
                total_rows += rows as u64;
            }
            Err(e) => {
                // #3802: this table's trim is abandoned. Count it as a
                // busy-caused loss ONLY when the give-up error is itself a
                // transient busy — the fail-closed non-transient path (e.g.
                // `Integrity`) is a different failure class and must not
                // inflate the busy-loss series. Note this arm is shared by
                // both: a chunk can retry a busy and then hit a non-transient
                // error, in which case `attempts` incremented but `dropped`
                // correctly does not.
                crate::metrics::record_db_busy_drop_if_transient(
                    crate::metrics::SITE_RETENTION_TRIM,
                    &e,
                );
                warn!(error = %e, attempts, "Failed to delete old {} entries", label);
                break;
            }
        }
        remaining = remaining.saturating_sub(chunk);
        if remaining > 0 {
            std::thread::sleep(MAINTENANCE_CHUNK_PAUSE);
        }
    }
    total_rows
}

pub fn run_maintenance(
    db: &henyey_db::Database,
    lcl: u32,
    min_queued: Option<u32>,
    rpc_retention_window: Option<u32>,
    count: u32,
) {
    // Evict stale publish queue entries when publishing is enabled.
    // Fail closed: on any DB error, keep the original min_queued to avoid
    // over-pruning.
    let min_queued = if let Some(orig_min) = min_queued {
        let max_lag = MAX_PUBLISH_QUEUE_CHECKPOINT_DISTANCE * checkpoint_frequency();
        let staleness_threshold = lcl.saturating_sub(max_lag);
        if staleness_threshold > 0 && orig_min < staleness_threshold {
            match db.remove_publish_entries_below(staleness_threshold) {
                Ok(removed) if removed > 0 => {
                    warn!(
                        removed,
                        staleness_threshold,
                        lcl,
                        original_oldest = orig_min,
                        "Permanently abandoned stale publish queue entries. \
                         These checkpoints will NOT be published. \
                         Check archive connectivity and credentials."
                    );
                    // Re-read queue after eviction. On failure, fall back
                    // to staleness_threshold — the most conservative safe
                    // value, since we just evicted everything below it.
                    match db.load_publish_queue(Some(1)) {
                        Ok(q) => q.first().copied().or(Some(staleness_threshold)),
                        Err(e) => {
                            warn!(error = %e, "Failed to re-read publish queue after eviction");
                            Some(staleness_threshold)
                        }
                    }
                }
                Ok(_) => min_queued,
                Err(e) => {
                    // #3802: log-and-continue, no retry — the eviction is
                    // simply lost until the next maintenance cycle. Count the
                    // transient-busy subset. Behaviour unchanged.
                    crate::metrics::record_db_busy_drop_if_transient(
                        crate::metrics::SITE_PUBLISH_QUEUE_EVICT,
                        &e,
                    );
                    warn!(error = %e, "Failed to evict stale publish queue entries");
                    min_queued
                }
            }
        } else {
            min_queued
        }
    } else {
        None
    };

    let qmin = min_queued.unwrap_or(lcl).min(lcl);

    // Publish-safe threshold: keeps data needed for queued checkpoint publishing.
    // Matches stellar-core: lmin = qmin - checkpoint_frequency.
    let publish_safe_lmin = qmin.saturating_sub(checkpoint_frequency());

    // RPC retention threshold: for tables that RPC serves.
    let rpc_lmin = rpc_retention_window.map(|w| lcl.saturating_sub(w));

    // For tables needed by both publish and RPC, use the more conservative
    // (lower) threshold so neither consumer loses required data.
    let publish_and_rpc_lmin = match rpc_lmin {
        Some(rpc_lmin) => publish_safe_lmin.min(rpc_lmin),
        None => publish_safe_lmin,
    };

    debug!(
        lcl = lcl,
        min_queued = ?min_queued,
        publish_safe_lmin = publish_safe_lmin,
        rpc_lmin = ?rpc_lmin,
        publish_and_rpc_lmin = publish_and_rpc_lmin,
        count = count,
        "Running maintenance"
    );

    // All deletions run in bounded chunks (MAINTENANCE_CHUNK_LEDGERS per
    // statement) so no single transaction holds the WAL write lock for long;
    // see delete_in_chunks.

    // --- Publish-only tables ---
    delete_in_chunks(count, "SCP", |chunk| {
        db.delete_old_scp_entries(publish_safe_lmin, chunk)
    });

    // --- Publish + RPC tables ---
    // These tables serve both checkpoint publishing (txsets, txresults, headers)
    // and RPC queries (txhistory, headers). Always pruned at the more conservative
    // of publish_safe_lmin and rpc_lmin so neither consumer loses required data.
    delete_in_chunks(count, "ledger header", |chunk| {
        db.delete_old_ledger_headers(publish_and_rpc_lmin, chunk)
    });

    delete_in_chunks(count, "tx history", |chunk| {
        db.delete_old_tx_history(publish_and_rpc_lmin, chunk)
    });

    // --- RPC-only tables (only when RPC retention is configured) ---
    if let Some(rpc_lmin) = rpc_lmin {
        delete_in_chunks(count, "event", |chunk| {
            db.delete_old_events(rpc_lmin, chunk)
        });

        delete_in_chunks(count, "ledger close meta", |chunk| {
            db.delete_old_ledger_close_meta(rpc_lmin, chunk)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    /// Regression test for #3702: the maintainer's first maintenance cycle
    /// must NOT fire immediately at startup.
    ///
    /// `tokio::time::interval(period)` resolves its first `tick()` immediately,
    /// which would fire the 50k-row DELETE maintenance burst right as the node
    /// enters the live loop — colliding with the startup near-tip back-fill on
    /// the restore-from-disk path and pinning SQLite's single WAL writer.
    ///
    /// This asserts that the first `tick()` of the interval built by
    /// `build_maintenance_interval` resolves only after ~`period` elapses.
    /// Written against the factored helper so it is deterministic under
    /// tokio's virtual clock. It FAILS on `tokio::time::interval` (immediate
    /// first tick) and PASSES with the `interval_at(now + period, period)`
    /// construction. The `assert_first_tick_immediate` sibling documents the
    /// pre-fix behavior it guards against.
    #[tokio::test(start_paused = true)]
    async fn test_maintenance_first_tick_is_delayed() {
        let period = Duration::from_secs(4 * 60 * 60);
        let mut interval = build_maintenance_interval(period);

        // The first tick must NOT be ready immediately. With virtual time
        // paused, a `select!` biased to a zero-duration timer proves the
        // interval's first tick is not already resolved at t=0.
        tokio::select! {
            biased;
            _ = interval.tick() => {
                panic!("maintenance first tick fired immediately at startup (#3702)");
            }
            _ = tokio::time::sleep(Duration::ZERO) => {}
        }

        // Advancing to just before `period` still must not fire.
        tokio::time::advance(period - Duration::from_secs(1)).await;
        tokio::select! {
            biased;
            _ = interval.tick() => {
                panic!("maintenance first tick fired before one full period elapsed");
            }
            _ = tokio::time::sleep(Duration::ZERO) => {}
        }

        // After a full period has elapsed, the first tick fires.
        tokio::time::advance(Duration::from_secs(1)).await;
        interval.tick().await;
    }

    /// Sanity check documenting the pre-fix hazard: `tokio::time::interval`
    /// (used on origin/main before #3702) fires its first tick immediately.
    /// This is the exact behavior `build_maintenance_interval` avoids.
    #[tokio::test(start_paused = true)]
    async fn test_bare_interval_first_tick_is_immediate() {
        let period = Duration::from_secs(4 * 60 * 60);
        let mut interval = tokio::time::interval(period);
        // First tick of a bare interval resolves immediately with no time advance.
        tokio::select! {
            biased;
            _ = interval.tick() => {}
            _ = tokio::time::sleep(Duration::ZERO) => {
                panic!("expected bare interval first tick to be immediate");
            }
        }
    }

    #[test]
    fn test_maintenance_config_default() {
        let config = MaintenanceConfig::default();
        assert!(config.enabled);
        assert_eq!(config.period, DEFAULT_MAINTENANCE_PERIOD);
        assert_eq!(config.count, DEFAULT_MAINTENANCE_COUNT);
    }

    #[test]
    fn test_maintenance_config_disabled() {
        let config = MaintenanceConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_checkpoint_frequency() {
        // Verify default checkpoint frequency matches Stellar standard
        assert_eq!(checkpoint_frequency(), 64);
    }

    #[tokio::test]
    async fn test_maintainer_creation() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let _maintainer = Maintainer::new(Arc::new(db), shutdown_rx, || (100, Some(64)));
    }

    #[tokio::test]
    async fn test_maintainer_disabled() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let maintainer = Maintainer::with_config(
            Arc::new(db),
            MaintenanceConfig::disabled(),
            shutdown_rx,
            || (100, None),
        );

        // Should return immediately when disabled
        maintainer.start().await;
    }

    #[test]
    fn test_perform_maintenance_empty_db() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let maintainer = Maintainer::new(Arc::new(db), shutdown_rx, || (100, Some(64)));

        // Should not panic on empty database
        maintainer.perform_maintenance();
    }

    #[test]
    fn test_lmin_calculation() {
        // Test that lmin is calculated correctly
        // If qmin is 128 and checkpoint_frequency() is 64, lmin should be 64
        let freq = checkpoint_frequency();
        let qmin = 128u32;
        let lmin = if qmin >= freq { qmin - freq } else { 0 };
        assert_eq!(lmin, 64);

        // If qmin is 32 (less than checkpoint frequency), lmin should be 0
        let qmin = 32u32;
        let lmin = if qmin >= freq { qmin - freq } else { 0 };
        assert_eq!(lmin, 0);
    }

    // -----------------------------------------------------------------------
    // Threshold correctness tests — verify that the maintainer uses the right
    // thresholds for events, headers, and RPC tables.
    // -----------------------------------------------------------------------

    /// Insert synthetic events into the database at the given ledger sequences.
    fn insert_events(db: &henyey_db::Database, ledger_seqs: &[u32]) {
        use henyey_db::queries::EventQueries;
        db.with_connection(|conn| {
            for &seq in ledger_seqs {
                let event = henyey_db::queries::events::EventRecord {
                    id: format!("{seq}-0"),
                    ledger_seq: seq,
                    tx_index: 0,
                    op_index: 0,
                    tx_hash: "aabb".to_string(),
                    contract_id: Some("CABC".to_string()),
                    event_type: stellar_xdr::ContractEventType::Contract,
                    topics: vec!["t1".to_string()],
                    event_xdr: "deadbeef".to_string(),
                    in_successful_contract_call: true,
                };
                conn.store_events(&[event]).unwrap();
            }
            Ok(())
        })
        .unwrap();
    }

    /// Insert synthetic ledger close meta into the database at the given sequences.
    fn insert_ledger_close_meta(db: &henyey_db::Database, ledger_seqs: &[u32]) {
        use henyey_db::queries::LedgerCloseMetaQueries;
        db.with_connection(|conn| {
            for &seq in ledger_seqs {
                conn.store_ledger_close_meta(seq, b"meta")?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// Insert synthetic ledger headers (raw SQL, minimal fields).
    fn insert_ledger_headers(db: &henyey_db::Database, ledger_seqs: &[u32]) {
        db.with_connection(|conn| {
            for &seq in ledger_seqs {
                let sql = format!(
                    "INSERT INTO ledgerheaders (ledgerhash, prevhash, bucketlisthash, ledgerseq, closetime, data) \
                     VALUES ('hash{seq}', 'prev{seq}', 'bucket{seq}', {seq}, 0, X'00')"
                );
                conn.execute(&sql, [])?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// Insert synthetic tx history rows (raw SQL, minimal fields).
    fn insert_tx_history(db: &henyey_db::Database, ledger_seqs: &[u32]) {
        db.with_connection(|conn| {
            for &seq in ledger_seqs {
                conn.execute(
                    &format!(
                        "INSERT INTO txhistory (txid, ledgerseq, txindex, txbody, txresult, txmeta) \
                         VALUES ('tx{seq}', {seq}, 0, X'00', X'00', X'00')"
                    ),
                    [],
                )?;
                conn.execute(
                    &format!(
                        "INSERT OR IGNORE INTO txsets (ledgerseq, data) \
                         VALUES ({seq}, X'00')"
                    ),
                    [],
                )?;
                conn.execute(
                    &format!(
                        "INSERT OR IGNORE INTO txresults (ledgerseq, data) \
                         VALUES ({seq}, X'00')"
                    ),
                    [],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// Count rows in a table.
    fn count_rows(db: &henyey_db::Database, table: &str) -> u32 {
        db.with_connection(|conn| {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            conn.query_row(&sql, [], |r| r.get::<_, u32>(0))
                .map_err(Into::into)
        })
        .unwrap()
    }

    /// Get the minimum ledger sequence in a table.
    fn min_ledger(db: &henyey_db::Database, table: &str, col: &str) -> Option<u32> {
        db.with_connection(|conn| {
            let sql = format!("SELECT MIN({col}) FROM {table}");
            conn.query_row(&sql, [], |r| r.get::<_, Option<u32>>(0))
                .map_err(Into::into)
        })
        .unwrap()
    }

    #[test]
    fn test_events_pruned_at_rpc_lmin_not_core_lmin() {
        // Scenario: lcl=1000, min_queued=1000, checkpoint_freq=64, retention_window=200
        //   lmin = 1000 - 64 = 936
        //   rpc_lmin = 1000 - 200 = 800
        // Events should be pruned at rpc_lmin=800, NOT lmin=936.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // Insert events at ledgers 790..=810 (spans the rpc_lmin=800 boundary)
        insert_events(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "events"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // Events at ledger <= 800 should be deleted (790..=800 = 11 events)
        // Events at ledger > 800 should remain (801..=810 = 10 events)
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
    }

    #[test]
    fn test_events_not_pruned_without_rpc() {
        // When rpc_retention_window is None, events should NOT be pruned at all.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_events(&db, &(10..=20).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "events"), 11);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // All events should remain
        assert_eq!(count_rows(&db_clone, "events"), 11);
    }

    #[test]
    fn test_ledger_close_meta_pruned_at_rpc_lmin() {
        // ledger_close_meta is RPC-only and should be pruned at rpc_lmin.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_close_meta(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledger_close_meta"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // Entries at sequence <= 800 should be deleted
        assert_eq!(count_rows(&db_clone, "ledger_close_meta"), 10);
        assert_eq!(
            min_ledger(&db_clone, "ledger_close_meta", "sequence"),
            Some(801)
        );
    }

    #[test]
    fn test_headers_use_min_of_publish_and_rpc_thresholds() {
        // Headers are needed by both publish and RPC, so they use the more
        // conservative (lower) threshold: min(publish_safe_lmin, rpc_lmin).
        //
        // Scenario: lcl=1000, min_queued=1000, checkpoint_freq=64, retention=200
        //   publish_safe_lmin = 1000 - 64 = 936
        //   rpc_lmin = 800
        //   header_lmin = min(936, 800) = 800
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_headers(&db, &(790..=940).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledgerheaders"), 151); // 790..=940

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // Headers at ledgerseq <= 800 should be deleted (790..=800 = 11 rows)
        // Headers at ledgerseq > 800 should remain (801..=940 = 140 rows)
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 140);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(801)
        );
    }

    #[test]
    fn test_headers_pruned_at_lmin_without_rpc() {
        // Without RPC, headers should be pruned at lmin (the core checkpoint threshold).
        //
        // Scenario: lcl=1000, min_queued=1000, checkpoint_freq=64
        //   lmin = 936
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_headers(&db, &(930..=940).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledgerheaders"), 11); // 930..=940

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // Headers at ledgerseq <= 936 should be deleted (930..=936 = 7 rows)
        // Headers at ledgerseq > 936 should remain (937..=940 = 4 rows)
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 4);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(937)
        );
    }

    #[test]
    fn test_headers_preserved_for_publish_queue_with_rpc() {
        // When the publish queue has pending entries, headers are preserved
        // for checkpoint publishing even when RPC retention is enabled.
        // RPC-only tables are still pruned at the RPC retention boundary.
        //
        // Scenario: lcl=1000, min_queued=127 (pending checkpoint), retention=200
        //   publish_safe_lmin = 127 - 64 = 63
        //   rpc_lmin = 1000 - 200 = 800
        //   header_lmin = publish_safe_lmin = 63 → all headers preserved
        //   RPC tables pruned at 800
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_headers(&db, &(790..=810).collect::<Vec<_>>());
        insert_events(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledgerheaders"), 21); // 790..=810
        assert_eq!(count_rows(&db, "events"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            // min_queued=127 is a realistic checkpoint-aligned value
            move || (1000, Some(127)),
        );

        maintainer.perform_maintenance();

        // All headers preserved: publish_safe_lmin = 63, all headers > 63
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 21);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(790)
        );

        // RPC tables pruned at rpc_lmin = 800: events 801..=810 remain
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
    }

    #[test]
    fn test_tx_history_preserved_for_publish_queue_with_rpc() {
        // Regression test: tx history/results must survive when the publish
        // queue needs them, even when RPC retention would prune them.
        //
        // Scenario: lcl=1000, min_queued=127 (pending checkpoint), retention=200
        //   publish_safe_lmin = 127 - 64 = 63
        //   rpc_lmin = 800
        //   publish_and_rpc_lmin = min(63, 800) = 63
        //   → tx history at 790..810 all preserved (all > 63)
        //   → events pruned at rpc_lmin = 800
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_tx_history(&db, &(790..=810).collect::<Vec<_>>());
        insert_events(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "txhistory"), 21);
        assert_eq!(count_rows(&db, "events"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(127)),
        );

        maintainer.perform_maintenance();

        // All tx history preserved: publish_and_rpc_lmin = 63, all rows > 63
        assert_eq!(count_rows(&db_clone, "txhistory"), 21);
        assert_eq!(min_ledger(&db_clone, "txhistory", "ledgerseq"), Some(790));
        assert_eq!(count_rows(&db_clone, "txsets"), 21);
        assert_eq!(min_ledger(&db_clone, "txsets", "ledgerseq"), Some(790));
        assert_eq!(count_rows(&db_clone, "txresults"), 21);
        assert_eq!(min_ledger(&db_clone, "txresults", "ledgerseq"), Some(790));

        // RPC-only events pruned at rpc_lmin = 800
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
    }

    #[test]
    fn test_perform_maintenance_with_count_uses_correct_thresholds() {
        // Verify that perform_maintenance_with_count uses the correct thresholds:
        // rpc_lmin for RPC-only tables (events, close meta).
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_events(&db, &(790..=810).collect::<Vec<_>>());
        insert_ledger_close_meta(&db, &(790..=810).collect::<Vec<_>>());

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        // Use a custom count
        maintainer.perform_maintenance_with_count(100_000);

        // RPC tables pruned at rpc_lmin = 800
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
        assert_eq!(count_rows(&db_clone, "ledger_close_meta"), 10);
        assert_eq!(
            min_ledger(&db_clone, "ledger_close_meta", "sequence"),
            Some(801)
        );
    }

    #[test]
    fn test_headers_pruned_at_rpc_lmin_when_publish_disabled() {
        // Regression test for #1989: when publishing is disabled (no writable
        // archives), callers pass min_queued=None. Headers should be pruned at
        // rpc_lmin, not pinned by stale publish queue entries.
        //
        // Scenario: lcl=1000, min_queued=None (publish disabled), retention=200
        //   publish_safe_lmin = 1000 - 64 = 936  (qmin defaults to lcl)
        //   rpc_lmin = 800
        //   publish_and_rpc_lmin = min(936, 800) = 800
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_headers(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledgerheaders"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            // min_queued=None simulates disabled publishing
            move || (1000, None),
        );

        maintainer.perform_maintenance();

        // Headers at ledgerseq <= 800 pruned (790..=800 = 11 rows deleted)
        // Headers at ledgerseq > 800 remain (801..=810 = 10 rows)
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 10);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(801)
        );
    }

    #[test]
    fn test_tx_history_pruned_at_rpc_lmin_when_publish_disabled() {
        // Regression test for #1989: tx history should also be pruned when
        // publishing is disabled, not pinned by stale queue entries.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_tx_history(&db, &(790..=810).collect::<Vec<_>>());
        insert_events(&db, &(790..=810).collect::<Vec<_>>());

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, None),
        );

        maintainer.perform_maintenance();

        // tx history pruned at publish_and_rpc_lmin = min(936, 800) = 800
        assert_eq!(count_rows(&db_clone, "txhistory"), 10);
        assert_eq!(min_ledger(&db_clone, "txhistory", "ledgerseq"), Some(801));
        assert_eq!(count_rows(&db_clone, "txsets"), 10);
        assert_eq!(min_ledger(&db_clone, "txsets", "ledgerseq"), Some(801));
        assert_eq!(count_rows(&db_clone, "txresults"), 10);
        assert_eq!(min_ledger(&db_clone, "txresults", "ledgerseq"), Some(801));

        // Events also pruned at rpc_lmin = 800
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
    }

    #[test]
    fn test_tx_history_pruned_without_rpc() {
        // Regression test for #2003: without RPC, tx history tables must still
        // be pruned at publish_safe_lmin. Previously, delete_old_tx_history was
        // gated inside the `if let Some(rpc_lmin)` block, so non-RPC nodes
        // never pruned txhistory/txsets/txresults.
        //
        // Scenario: lcl=1000, min_queued=1000, checkpoint_freq=64, no RPC
        //   publish_safe_lmin = 1000 - 64 = 936
        //   publish_and_rpc_lmin = 936 (no RPC to lower it)
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_tx_history(&db, &(930..=940).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "txhistory"), 11);
        assert_eq!(count_rows(&db, "txsets"), 11);
        assert_eq!(count_rows(&db, "txresults"), 11);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(1000)),
        );

        maintainer.perform_maintenance();

        // Rows at ledgerseq <= 936 should be deleted (930..=936 = 7 rows)
        // Rows at ledgerseq > 936 should remain (937..=940 = 4 rows)
        assert_eq!(count_rows(&db_clone, "txhistory"), 4);
        assert_eq!(min_ledger(&db_clone, "txhistory", "ledgerseq"), Some(937));
        assert_eq!(count_rows(&db_clone, "txsets"), 4);
        assert_eq!(min_ledger(&db_clone, "txsets", "ledgerseq"), Some(937));
        assert_eq!(count_rows(&db_clone, "txresults"), 4);
        assert_eq!(min_ledger(&db_clone, "txresults", "ledgerseq"), Some(937));
    }

    #[test]
    fn test_tx_history_preserved_for_publish_queue_without_rpc() {
        // Regression test for #2003: without RPC, when the publish queue has
        // pending checkpoints, tx history must be preserved for publishing.
        //
        // Scenario: lcl=1000, min_queued=127 (pending checkpoint), no RPC
        //   publish_safe_lmin = 127 - 64 = 63
        //   → tx history at 790..810 all preserved (all > 63)
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_tx_history(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "txhistory"), 21);
        assert_eq!(count_rows(&db, "txsets"), 21);
        assert_eq!(count_rows(&db, "txresults"), 21);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (1000, Some(127)),
        );

        maintainer.perform_maintenance();

        // All rows preserved: publish_safe_lmin = 63, all rows > 63
        assert_eq!(count_rows(&db_clone, "txhistory"), 21);
        assert_eq!(min_ledger(&db_clone, "txhistory", "ledgerseq"), Some(790));
        assert_eq!(count_rows(&db_clone, "txsets"), 21);
        assert_eq!(min_ledger(&db_clone, "txsets", "ledgerseq"), Some(790));
        assert_eq!(count_rows(&db_clone, "txresults"), 21);
        assert_eq!(min_ledger(&db_clone, "txresults", "ledgerseq"), Some(790));
    }

    // -----------------------------------------------------------------------
    // Publish queue staleness eviction tests (#2004)
    // -----------------------------------------------------------------------

    /// Insert synthetic publish queue entries.
    fn insert_publish_queue_entries(db: &henyey_db::Database, ledger_seqs: &[u32]) {
        db.with_connection(|conn| {
            use henyey_db::queries::publish_queue::PublishQueueQueries;
            for &seq in ledger_seqs {
                let has_json = format!(r#"{{"version":2,"currentLedger":{seq}}}"#);
                conn.enqueue_publish(seq, &has_json)?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// Count publish queue entries.
    fn publish_queue_count(db: &henyey_db::Database) -> u32 {
        db.with_connection(|conn| {
            conn.query_row("SELECT COUNT(*) FROM publishqueue", [], |r| {
                r.get::<_, u32>(0)
            })
            .map_err(Into::into)
        })
        .unwrap()
    }

    /// Get minimum ledger in publish queue.
    fn publish_queue_min(db: &henyey_db::Database) -> Option<u32> {
        db.with_connection(|conn| {
            conn.query_row("SELECT MIN(ledgerseq) FROM publishqueue", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map(|v| v.map(|v| v as u32))
            .map_err(Into::into)
        })
        .unwrap()
    }

    #[test]
    fn test_stale_entries_evicted() {
        // Scenario: publish queue has a very old entry (ledger 63) and the
        // LCL is 10000. The staleness threshold is:
        //   10000 - (30 * 64) = 10000 - 1920 = 8080
        // Entry at 63 is < 8080, so it should be evicted.
        // After eviction, publish_safe_lmin should advance.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // Insert a very old entry and a recent one
        insert_publish_queue_entries(&db, &[63, 9919]);
        assert_eq!(publish_queue_count(&db), 2);

        // Insert headers to observe pruning behavior
        insert_ledger_headers(&db, &(8000..=8100).collect::<Vec<_>>());

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            // min_queued=63 initially (oldest entry)
            move || (10000, Some(63)),
        );

        maintainer.perform_maintenance();

        // Old entry (63) should be evicted
        assert_eq!(publish_queue_count(&db_clone), 1);
        assert_eq!(publish_queue_min(&db_clone), Some(9919));

        // publish_safe_lmin should be computed from the fresh min (9919):
        //   9919 - 64 = 9855
        // Headers at 8000..=8100 are all < 9855, so all should be pruned
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 0);
    }

    #[test]
    fn test_no_eviction_within_window() {
        // All entries are within the 30-checkpoint window. None should be evicted.
        // LCL=10000, staleness_threshold = 10000 - 1920 = 8080
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // Entries at 8127 and 9919 are both > 8080
        insert_publish_queue_entries(&db, &[8127, 9919]);
        assert_eq!(publish_queue_count(&db), 2);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (10000, Some(8127)),
        );

        maintainer.perform_maintenance();

        // No entries evicted
        assert_eq!(publish_queue_count(&db_clone), 2);
    }

    #[test]
    fn test_mixed_stale_fresh_entries() {
        // Mix of stale and fresh entries. Only stale ones evicted.
        // LCL=10000, threshold = 8080
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_publish_queue_entries(&db, &[63, 127, 191, 8127, 8191, 9919]);
        assert_eq!(publish_queue_count(&db), 6);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (10000, Some(63)),
        );

        maintainer.perform_maintenance();

        // 63, 127, 191 evicted (all < 8080). 8127, 8191, 9919 remain.
        assert_eq!(publish_queue_count(&db_clone), 3);
        assert_eq!(publish_queue_min(&db_clone), Some(8127));
    }

    #[test]
    fn test_eviction_threshold_underflow() {
        // LCL is small (< max_lag). Threshold saturates at 0, no eviction.
        // max_lag = 30 * 64 = 1920. LCL=500 → threshold = 0
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_publish_queue_entries(&db, &[63, 127]);
        assert_eq!(publish_queue_count(&db), 2);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (500, Some(63)),
        );

        maintainer.perform_maintenance();

        // No eviction: threshold = 0
        assert_eq!(publish_queue_count(&db_clone), 2);
    }

    #[test]
    fn test_publish_disabled_no_eviction() {
        // min_queued = None (publishing disabled). No eviction should occur.
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_publish_queue_entries(&db, &[63, 127]);
        assert_eq!(publish_queue_count(&db), 2);

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (10000, None),
        );

        maintainer.perform_maintenance();

        // No eviction: publishing disabled
        assert_eq!(publish_queue_count(&db_clone), 2);
    }

    #[test]
    fn test_all_entries_stale_queue_empty_after_eviction() {
        // All entries are stale. After eviction, the queue is empty.
        // qmin should fall back to staleness_threshold (not lcl).
        // LCL=10000, threshold = 10000 - 1920 = 8080
        // publish_safe_lmin = 8080 - 64 = 8016
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // All entries are stale (all < 8080)
        insert_publish_queue_entries(&db, &[63, 127, 191]);
        assert_eq!(publish_queue_count(&db), 3);

        // Insert headers that span the threshold
        insert_ledger_headers(&db, &(8010..=8025).collect::<Vec<_>>());

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: None,
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            move || (10000, Some(63)),
        );

        maintainer.perform_maintenance();

        // All entries evicted
        assert_eq!(publish_queue_count(&db_clone), 0);

        // publish_safe_lmin = staleness_threshold - 64 = 8080 - 64 = 8016
        // Headers at 8010..=8016 should be pruned (<=8016), 8017..=8025 kept (9 rows)
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 9);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(8017)
        );
    }

    #[test]
    fn test_delete_in_chunks_bounds_each_call_and_exhausts() {
        // Simulate a table holding 7 ledgers' worth of rows (3 rows/ledger).
        let mut ledgers_left = 7u32;
        let mut calls: Vec<u32> = Vec::new();
        let total = delete_in_chunks(100, "test", |chunk| {
            calls.push(chunk);
            let deleted = chunk.min(ledgers_left);
            ledgers_left -= deleted;
            Ok(deleted * 3)
        });

        assert!(
            calls.iter().all(|&c| c <= MAINTENANCE_CHUNK_LEDGERS),
            "every chunk must be bounded by MAINTENANCE_CHUNK_LEDGERS, got {calls:?}"
        );
        assert_eq!(total, 21, "all 7 ledgers x 3 rows must be deleted");
        assert_eq!(ledgers_left, 0, "table must be fully drained");
        // 4 productive chunks (2+2+2+1 ledgers) + 1 terminating zero-row call.
        assert_eq!(calls.len(), 5);
    }

    #[test]
    fn test_delete_in_chunks_respects_total_budget() {
        let mut ledgers_left = 100u32;
        let total = delete_in_chunks(6, "test", |chunk| {
            let deleted = chunk.min(ledgers_left);
            ledgers_left -= deleted;
            Ok(deleted)
        });
        assert_eq!(total, 6, "budget of 6 ledgers must cap the pass");
        assert_eq!(ledgers_left, 94);
    }

    #[test]
    fn test_delete_in_chunks_stops_on_error() {
        let mut calls = 0u32;
        let total = delete_in_chunks(100, "test", |chunk| {
            calls += 1;
            if calls == 2 {
                Err(henyey_db::DbError::Integrity("boom".into()))
            } else {
                Ok(chunk)
            }
        });
        assert_eq!(calls, 2, "loop must stop at the failing chunk");
        assert_eq!(total, 2, "only the first chunk's rows are counted");
    }

    /// Build a transient `DatabaseBusy` (`SQLITE_BUSY`) error, the exact shape
    /// `is_transient_db_busy` classifies as recoverable. Mirrors the persist.rs
    /// test helper; extended_code is documented for traceability only — the
    /// gate is the primary `DatabaseBusy` code.
    fn busy_error() -> henyey_db::DbError {
        henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            Some("database is locked".to_string()),
        ))
    }

    /// #3772: a chunk that loses the SQLite write-lock race once (transient
    /// busy) must be retried, not silently abort the whole table's trim. On the
    /// unmodified single-shot code the first busy error `break`s the loop, so
    /// the closure is called once and the chunk's rows are lost.
    #[test]
    fn test_delete_in_chunks_retries_transient_busy_then_succeeds() {
        let mut calls = 0u32;
        let total = delete_in_chunks(100, "test", |chunk| {
            calls += 1;
            match calls {
                // First chunk: fail busy once, then succeed on retry.
                1 => Err(busy_error()),
                2 => Ok(chunk * 3),
                // Second chunk reports no rows -> terminates the pass.
                _ => Ok(0),
            }
        });
        assert_eq!(
            calls, 3,
            "busy chunk must be retried (call 1 fails busy, call 2 succeeds, call 3 drains)"
        );
        assert_eq!(
            total,
            (MAINTENANCE_CHUNK_LEDGERS as u64) * 3,
            "the retried chunk's rows must count toward the total"
        );
    }

    /// #3772: a persistently-busy chunk must give up after exactly
    /// `MAINTENANCE_DELETE_MAX_ATTEMPTS` attempts (bounded — no unbounded
    /// spin), then break the pass. On the unmodified code the loop breaks after
    /// a single call.
    #[test]
    fn test_delete_in_chunks_gives_up_after_max_attempts() {
        let mut calls = 0u32;
        let total = delete_in_chunks(100, "test", |_chunk| -> Result<u32, henyey_db::DbError> {
            calls += 1;
            Err(busy_error())
        });
        assert_eq!(
            calls, MAINTENANCE_DELETE_MAX_ATTEMPTS,
            "a persistently-busy chunk must be attempted exactly MAINTENANCE_DELETE_MAX_ATTEMPTS times"
        );
        assert_eq!(
            total, 0,
            "no rows are deleted when every attempt fails busy"
        );
    }

    /// #3772: a genuine (non-transient) error must NOT be retried — the
    /// fail-closed behavior for real corruption/integrity failures is
    /// preserved. Exactly one call, then break.
    #[test]
    fn test_delete_in_chunks_does_not_retry_non_transient_error() {
        let mut calls = 0u32;
        let total = delete_in_chunks(100, "test", |_chunk| -> Result<u32, henyey_db::DbError> {
            calls += 1;
            Err(henyey_db::DbError::Integrity("boom".into()))
        });
        assert_eq!(
            calls, 1,
            "a non-transient error must not be retried (fail closed immediately)"
        );
        assert_eq!(
            total, 0,
            "no rows counted on immediate non-transient failure"
        );
    }

    // ── #3802 busy telemetry ───────────────────────────────────────────
    //
    // Counters are process-global: each of these runs on a pristine local
    // recorder inside a synchronous `#[test]` (`with_local_recorder` is
    // thread-local), and pre-registers the label series before asserting any
    // `== 0`, since an unincremented labelled series is absent from the render.

    /// #3802: **this is the "is #3781/#3772's bounded retry working?"
    /// assertion.** A chunk that loses the write-lock race once and then
    /// succeeds must increment `attempts{retention_trim}` and leave
    /// `dropped{retention_trim}` at zero. Before this change a successful retry
    /// produced no signal at all, so the deploy could not answer the question.
    #[test]
    fn test_delete_in_chunks_counts_retry_attempts_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let mut calls = 0u32;
            let total = delete_in_chunks(100, "test", |chunk| {
                calls += 1;
                match calls {
                    1 => Err(busy_error()),
                    2 => Ok(chunk * 3),
                    _ => Ok(0),
                }
            });
            assert_eq!(total, (MAINTENANCE_CHUNK_LEDGERS as u64) * 3);

            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_retry_attempts_total{site=\"retention_trim\"} 1"),
                "the retried chunk must be counted; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"retention_trim\"} 0"),
                "a retry that then succeeds must NOT count as a dropped write; got:\n{out}"
            );
        });
    }

    /// #3802: a persistently-busy chunk exhausts the attempt budget and the
    /// table's trim is abandoned → exactly one `dropped{retention_trim}`, and
    /// `MAINTENANCE_DELETE_MAX_ATTEMPTS - 1` retried attempts (the final,
    /// giving-up attempt is the drop, not a retry).
    #[test]
    fn test_delete_in_chunks_counts_drop_on_exhaustion_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let total =
                delete_in_chunks(100, "test", |_chunk| -> Result<u32, henyey_db::DbError> {
                    Err(busy_error())
                });
            assert_eq!(total, 0);

            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"retention_trim\"} 1"),
                "an abandoned trim must count exactly one dropped write; got:\n{out}"
            );
            assert!(
                out.contains(&format!(
                    "henyey_db_busy_retry_attempts_total{{site=\"retention_trim\"}} {}",
                    MAINTENANCE_DELETE_MAX_ATTEMPTS - 1
                )),
                "only the non-final attempts are retries; got:\n{out}"
            );
        });
    }

    /// #3802: the give-up arm is shared by the busy and non-transient paths, so
    /// the drop increment MUST be gated on classification. A non-transient
    /// `Integrity` failure is fail-closed behaviour, not a busy-caused loss —
    /// counting it would poison the alertable series.
    #[test]
    fn test_delete_in_chunks_non_transient_error_counts_no_drop_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let total =
                delete_in_chunks(100, "test", |_chunk| -> Result<u32, henyey_db::DbError> {
                    Err(henyey_db::DbError::Integrity("boom".into()))
                });
            assert_eq!(total, 0);

            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_retry_attempts_total{site=\"retention_trim\"} 0"),
                "a non-transient error is never retried; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"retention_trim\"} 0"),
                "a non-transient error must NOT count as a busy-caused drop; got:\n{out}"
            );
        });
    }

    /// #3802 (documented subtlety): a chunk can retry a transient busy and then
    /// hit a NON-transient error on a later attempt. `attempts` increments
    /// while `dropped` correctly does not — the two series have no simple
    /// per-episode ordering relationship, and a missing `dropped` must not be
    /// read as a missing increment.
    #[test]
    fn test_delete_in_chunks_busy_then_non_transient_counts_attempt_not_drop_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let mut calls = 0u32;
            let total =
                delete_in_chunks(100, "test", |_chunk| -> Result<u32, henyey_db::DbError> {
                    calls += 1;
                    if calls == 1 {
                        Err(busy_error())
                    } else {
                        Err(henyey_db::DbError::Integrity("boom".into()))
                    }
                });
            assert_eq!(calls, 2, "busy is retried once, then fails non-transiently");
            assert_eq!(total, 0);

            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_retry_attempts_total{site=\"retention_trim\"} 1"),
                "the busy attempt was genuinely retried and must be counted; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"retention_trim\"} 0"),
                "the loss was non-transient, so no busy-caused drop; got:\n{out}"
            );
        });
    }
}
