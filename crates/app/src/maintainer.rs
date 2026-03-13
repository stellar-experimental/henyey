//! Database maintenance scheduler for rs-stellar-core.
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
//! - **txhistory / txsets / txresults**: Old transaction data (RPC retention window)
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
    /// up `ledger_close_meta`, `txhistory`, `txsets`, and `txresults` tables,
    /// keeping only ledgers within this window of the LCL.
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
    get_ledger_bounds: Box<dyn Fn() -> (u32, Option<u32>) + Send + Sync>,
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
            get_ledger_bounds: Box::new(get_ledger_bounds),
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
            get_ledger_bounds: Box::new(get_ledger_bounds),
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

        let mut interval = tokio::time::interval(self.config.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.perform_maintenance();
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

        // Get current ledger bounds
        let (lcl, min_queued) = (self.get_ledger_bounds)();

        // Calculate the minimum ledger we need to keep
        // We need to keep enough history to support checkpoint publishing
        let qmin = min_queued.unwrap_or(lcl).min(lcl);
        let lmin = qmin.saturating_sub(checkpoint_frequency());

        debug!(
            lcl = lcl,
            min_queued = ?min_queued,
            qmin = qmin,
            trim_below = lmin,
            "Trimming history"
        );

        // Delete old SCP history
        match self
            .database
            .delete_old_scp_entries(lmin, self.config.count)
        {
            Ok(deleted) => {
                if deleted > 0 {
                    debug!(deleted = deleted, "Deleted old SCP entries");
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to delete old SCP entries");
            }
        }

        // Delete old ledger headers.
        // When RPC retention is configured, use rpc_lmin directly so that
        // oldest_ledger (MIN of ledgerheaders) tracks the retention window.
        // Without RPC, fall back to the checkpoint-based lmin.
        //
        // Note: we intentionally ignore the publish queue for header pruning
        // when RPC is active — a stale publish queue (e.g. put_enabled=false)
        // must not prevent RPC data from being pruned.
        let header_lmin = if let Some(retention_window) = self.config.rpc_retention_window {
            lcl.saturating_sub(retention_window)
        } else {
            lmin
        };
        match self
            .database
            .delete_old_ledger_headers(header_lmin, self.config.count)
        {
            Ok(deleted) => {
                if deleted > 0 {
                    debug!(deleted = deleted, "Deleted old ledger headers");
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to delete old ledger headers");
            }
        }

        // Clean up RPC-specific tables if retention window is configured.
        // Events are RPC-only data and should be pruned at the RPC retention
        // window, not the core checkpoint threshold.
        if let Some(retention_window) = self.config.rpc_retention_window {
            let rpc_lmin = lcl.saturating_sub(retention_window);

            match self
                .database
                .delete_old_events(rpc_lmin, self.config.count)
            {
                Ok(deleted) => {
                    if deleted > 0 {
                        debug!(deleted = deleted, "Deleted old contract events");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to delete old events");
                }
            }

            match self
                .database
                .delete_old_ledger_close_meta(rpc_lmin, self.config.count)
            {
                Ok(deleted) => {
                    if deleted > 0 {
                        debug!(deleted = deleted, "Deleted old ledger close meta");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to delete old ledger close meta");
                }
            }

            match self
                .database
                .delete_old_tx_history(rpc_lmin, self.config.count)
            {
                Ok(deleted) => {
                    if deleted > 0 {
                        debug!(deleted = deleted, "Deleted old tx history");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to delete old tx history");
                }
            }
        }

        let elapsed = start.elapsed();
        if elapsed > Duration::from_secs(2) {
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
        let qmin = min_queued.unwrap_or(lcl).min(lcl);
        let lmin = qmin.saturating_sub(checkpoint_frequency());

        info!(
            trim_below = lmin,
            count = count,
            "Performing manual maintenance"
        );

        if let Err(e) = self.database.delete_old_scp_entries(lmin, count) {
            warn!(error = %e, "Failed to delete old SCP entries");
        }

        let header_lmin = if let Some(retention_window) = self.config.rpc_retention_window {
            lcl.saturating_sub(retention_window)
        } else {
            lmin
        };
        if let Err(e) = self.database.delete_old_ledger_headers(header_lmin, count) {
            warn!(error = %e, "Failed to delete old ledger headers");
        }

        // Clean up RPC-specific tables if retention window is configured.
        // Events are RPC-only data and belong in this block.
        if let Some(retention_window) = self.config.rpc_retention_window {
            let rpc_lmin = lcl.saturating_sub(retention_window);

            if let Err(e) = self.database.delete_old_events(rpc_lmin, count) {
                warn!(error = %e, "Failed to delete old events");
            }

            if let Err(e) = self.database.delete_old_ledger_close_meta(rpc_lmin, count) {
                warn!(error = %e, "Failed to delete old ledger close meta");
            }

            if let Err(e) = self.database.delete_old_tx_history(rpc_lmin, count) {
                warn!(error = %e, "Failed to delete old tx history");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

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
        let lmin = if qmin >= freq {
            qmin - freq
        } else {
            0
        };
        assert_eq!(lmin, 64);

        // If qmin is 32 (less than checkpoint frequency), lmin should be 0
        let qmin = 32u32;
        let lmin = if qmin >= freq {
            qmin - freq
        } else {
            0
        };
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
                    event_type: 0,
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
    fn test_headers_retained_to_rpc_window_when_rpc_enabled() {
        // When RPC retention is configured, ledger headers should be pruned at
        // rpc_lmin (ignoring the publish queue) so that oldest_ledger tracks
        // the retention window.
        //
        // Scenario: lcl=1000, min_queued=1000, checkpoint_freq=64, retention=200
        //   rpc_lmin = 800
        //   header_lmin = rpc_lmin = 800
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // We can't easily insert ledger headers without building XDR, so we
        // verify indirectly by checking that delete_old_ledger_headers was
        // called with the right threshold. Insert ledger_close_meta as a proxy
        // (both use the same pattern) and check the header threshold by
        // inserting raw rows into ledgerheaders.
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
    fn test_headers_pruned_despite_stale_publish_queue() {
        // Regression test: when the publish queue has stale entries (e.g.
        // put_enabled=false), lmin is dragged down to near the oldest queued
        // checkpoint. With the old min(lmin, rpc_lmin) logic, header_lmin
        // would be below all headers, preventing any pruning. The fix uses
        // rpc_lmin directly when RPC is active.
        //
        // Scenario: lcl=1000, min_queued=100 (stale), retention=200
        //   lmin = 100 - 64 = 36
        //   rpc_lmin = 1000 - 200 = 800
        //   OLD: header_lmin = min(36, 800) = 36 → nothing pruned (oldest header > 36)
        //   NEW: header_lmin = rpc_lmin = 800 → headers below 800 pruned
        let db = Arc::new(henyey_db::Database::open_in_memory().unwrap());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        insert_ledger_headers(&db, &(790..=810).collect::<Vec<_>>());
        assert_eq!(count_rows(&db, "ledgerheaders"), 21); // 790..=810

        let db_clone = db.clone();
        let maintainer = Maintainer::with_config(
            db.clone(),
            MaintenanceConfig {
                rpc_retention_window: Some(200),
                count: 100_000,
                ..MaintenanceConfig::default()
            },
            shutdown_rx,
            // min_queued=100 simulates a stale publish queue
            move || (1000, Some(100)),
        );

        maintainer.perform_maintenance();

        // Headers at ledgerseq <= 800 should be deleted (790..=800 = 11 rows)
        // Headers at ledgerseq > 800 should remain (801..=810 = 10 rows)
        assert_eq!(count_rows(&db_clone, "ledgerheaders"), 10);
        assert_eq!(
            min_ledger(&db_clone, "ledgerheaders", "ledgerseq"),
            Some(801)
        );
    }

    #[test]
    fn test_perform_maintenance_with_count_uses_correct_thresholds() {
        // Verify that perform_maintenance_with_count also uses the correct
        // thresholds (rpc_lmin for events, rpc_lmin for headers).
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

        // Same thresholds as the main perform_maintenance
        assert_eq!(count_rows(&db_clone, "events"), 10);
        assert_eq!(min_ledger(&db_clone, "events", "ledgerseq"), Some(801));
        assert_eq!(count_rows(&db_clone, "ledger_close_meta"), 10);
        assert_eq!(
            min_ledger(&db_clone, "ledger_close_meta", "sequence"),
            Some(801)
        );
    }
}
