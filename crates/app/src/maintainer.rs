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
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            period: DEFAULT_MAINTENANCE_PERIOD,
            count: DEFAULT_MAINTENANCE_COUNT,
            enabled: true,
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

        // Delete old ledger headers
        match self
            .database
            .delete_old_ledger_headers(lmin, self.config.count)
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

        if let Err(e) = self.database.delete_old_ledger_headers(lmin, count) {
            warn!(error = %e, "Failed to delete old ledger headers");
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
}
