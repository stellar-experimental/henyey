//! Close time drift tracking for monitoring clock synchronization.
//!
//! This module implements tracking of the drift between a node's local clock
//! and the network-agreed close time. If a node's clock drifts significantly
//! from the network, it may affect its ability to participate in consensus.
//!
//! # Parity
//!
//! This module corresponds to the `mDriftCTSlidingWindow` mechanism in
//! `HerderImpl.cpp`, including:
//! - `recordExternalizeAndCheckCloseTimeDrift()`
//! - `CLOSE_TIME_DRIFT_LEDGER_WINDOW_SIZE`
//! - `CLOSE_TIME_DRIFT_SECONDS_THRESHOLD`
//!
//! # Algorithm
//!
//! 1. When triggering a new ledger, record the local close time
//! 2. When the ledger is externalized, record the network close time
//! 3. Compute drift as: network_close_time - local_close_time
//! 4. Over a sliding window, compute the 75th percentile drift
//! 5. If drift exceeds threshold, log a warning about clock issues
//!
//! # Usage
//!
//! ```ignore
//! let mut tracker = CloseTimeDriftTracker::new();
//!
//! // When triggering ledger N
//! tracker.record_local_close_time(ledger_seq, local_time);
//!
//! // When ledger N is externalized
//! if let Some(warning) = tracker.record_externalized_close_time(ledger_seq, network_time) {
//!     warn!("{}", warning);
//! }
//! ```

use std::collections::BTreeMap;

/// Size of the sliding window for drift tracking.
///
/// Roughly 10 minutes of consensus at 5 seconds per ledger (120 ledgers).
/// This provides enough data points to compute meaningful statistics while
/// being responsive to recent clock drift.
pub const CLOSE_TIME_DRIFT_LEDGER_WINDOW_SIZE: usize = 120;

/// Threshold in seconds for warning about clock drift.
///
/// If the 75th percentile drift exceeds this threshold (positive or negative),
/// a warning is logged suggesting the local clock may be misconfigured.
pub const CLOSE_TIME_DRIFT_SECONDS_THRESHOLD: i64 = 10;

/// Warning message for potentially bad local clock.
pub const POSSIBLY_BAD_LOCAL_CLOCK: &str =
    "Your local clock may be out of sync, which can cause issues with consensus. \
     Consider checking your NTP configuration.";

/// Entry in the drift tracking window.
#[derive(Debug, Clone, Copy)]
struct DriftEntry {
    /// Local close time when the ledger was triggered (unix timestamp).
    local_close_time: u64,
    /// Network-agreed close time when externalized (unix timestamp), if available.
    externalized_close_time: Option<u64>,
}

/// Tracks close time drift between local clock and network consensus.
///
/// Maintains a sliding window of ledger close times, comparing the local
/// time when consensus was triggered against the network-agreed close time.
/// When the window is full, computes statistics and checks for significant drift.
#[derive(Debug)]
pub struct CloseTimeDriftTracker {
    /// Map from ledger sequence to close time pair.
    /// Using BTreeMap for ordered iteration and easy eviction of oldest entries.
    window: BTreeMap<u32, DriftEntry>,
    /// Maximum window size.
    window_size: usize,
    /// Drift threshold for warnings (in seconds).
    threshold: i64,
}

impl Default for CloseTimeDriftTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CloseTimeDriftTracker {
    /// Create a new drift tracker with default configuration.
    pub fn new() -> Self {
        Self::with_config(
            CLOSE_TIME_DRIFT_LEDGER_WINDOW_SIZE,
            CLOSE_TIME_DRIFT_SECONDS_THRESHOLD,
        )
    }

    /// Create a new drift tracker with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `window_size` - Number of ledgers in the sliding window
    /// * `threshold` - Drift threshold in seconds for warnings
    pub fn with_config(window_size: usize, threshold: i64) -> Self {
        Self {
            window: BTreeMap::new(),
            window_size,
            threshold,
        }
    }

    /// Record the local close time when triggering a ledger.
    ///
    /// This should be called from `trigger_next_ledger()` before any
    /// adjustments are made to make the close time valid.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The ledger sequence number being triggered
    /// * `local_close_time` - The local system time (unix timestamp)
    ///
    /// # Returns
    ///
    /// `true` if this is a new entry, `false` if the ledger was already recorded
    /// (which indicates `trigger_next_ledger` was called twice for the same ledger).
    pub fn record_local_close_time(&mut self, ledger_seq: u32, local_close_time: u64) -> bool {
        if self.window.contains_key(&ledger_seq) {
            // Already have an entry for this ledger
            return false;
        }

        self.window.insert(
            ledger_seq,
            DriftEntry {
                local_close_time,
                externalized_close_time: None,
            },
        );

        // Evict oldest entries if window is too large
        while self.window.len() > self.window_size {
            if let Some((&oldest_key, _)) = self.window.iter().next() {
                self.window.remove(&oldest_key);
            }
        }

        true
    }

    /// Record the externalized close time and check for drift.
    ///
    /// This should be called from `value_externalized()` when a ledger
    /// is closed by the network.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The ledger sequence number that was externalized
    /// * `network_close_time` - The network-agreed close time (unix timestamp)
    ///
    /// # Returns
    ///
    /// `Some(warning_message)` if drift exceeds threshold and window is full,
    /// `None` otherwise.
    pub fn record_externalized_close_time(
        &mut self,
        ledger_seq: u32,
        network_close_time: u64,
    ) -> Option<String> {
        // Update the entry if it exists
        if let Some(entry) = self.window.get_mut(&ledger_seq) {
            entry.externalized_close_time = Some(network_close_time);
        }

        // Check for drift if window is full
        if self.window.len() >= self.window_size {
            let result = self.check_and_clear_drift();
            return result;
        }

        None
    }

    /// Check drift statistics and clear the window if full.
    ///
    /// Computes the 75th percentile of drift values and returns a warning
    /// if it exceeds the threshold. Clears the window afterward.
    fn check_and_clear_drift(&mut self) -> Option<String> {
        let result = self
            .sorted_completed_drifts()
            .and_then(|drifts| match drift_p75(&drifts) {
                Some(drift_p75) if drift_p75.abs() > self.threshold => Some(format!(
                    "{} Close time local drift is: {} seconds",
                    POSSIBLY_BAD_LOCAL_CLOCK, drift_p75
                )),
                _ => None,
            });

        // Clear the window after checking
        self.window.clear();

        result
    }

    /// Get the current number of entries in the window.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Get the number of entries that have externalized close times.
    pub fn completed_entries(&self) -> usize {
        self.window
            .values()
            .filter(|e| e.externalized_close_time.is_some())
            .count()
    }

    /// Get drift statistics without clearing the window.
    ///
    /// Returns (min_drift, max_drift, median_drift, p75_drift) in seconds,
    /// or None if there are no completed entries.
    pub fn get_drift_stats(&self) -> Option<DriftStats> {
        let drifts = self.sorted_completed_drifts()?;

        let min = drifts[0];
        let max = drifts[drifts.len() - 1];
        let median_index = drifts.len() / 2;
        let median = drifts[median_index];
        let p75 = drift_p75(&drifts).expect("non-empty drift list has a p75 value");

        Some(DriftStats {
            min,
            max,
            median,
            p75,
            sample_count: drifts.len(),
        })
    }

    fn sorted_completed_drifts(&self) -> Option<Vec<i64>> {
        let mut drifts: Vec<i64> = self.window.values().filter_map(DriftEntry::drift).collect();

        if drifts.is_empty() {
            return None;
        }

        drifts.sort();
        Some(drifts)
    }
}

impl DriftEntry {
    fn drift(&self) -> Option<i64> {
        self.externalized_close_time.map(|network| {
            // Drift = network time - local time
            // Positive drift means network is ahead of local clock
            // Negative drift means local clock is ahead of network
            network as i64 - self.local_close_time as i64
        })
    }
}

fn drift_p75(drifts: &[i64]) -> Option<i64> {
    percentile_value(drifts, 0.75)
}

fn percentile_value(values: &[i64], percentile: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }

    let index = ((values.len() as f64 * percentile).ceil() as usize).saturating_sub(1);
    Some(values[index.min(values.len() - 1)])
}

/// Statistics about clock drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftStats {
    /// Minimum drift observed (seconds).
    pub min: i64,
    /// Maximum drift observed (seconds).
    pub max: i64,
    /// Median drift (seconds).
    pub median: i64,
    /// 75th percentile drift (seconds).
    pub p75: i64,
    /// Number of samples used.
    pub sample_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_tracker() {
        let tracker = CloseTimeDriftTracker::new();
        assert_eq!(tracker.window_len(), 0);
        assert_eq!(tracker.completed_entries(), 0);
        assert!(tracker.get_drift_stats().is_none());
    }

    #[test]
    fn test_record_local_time() {
        let mut tracker = CloseTimeDriftTracker::with_config(10, 10);

        // First record should succeed
        assert!(tracker.record_local_close_time(100, 1000));
        assert_eq!(tracker.window_len(), 1);

        // Duplicate should return false
        assert!(!tracker.record_local_close_time(100, 1001));
        assert_eq!(tracker.window_len(), 1);

        // New ledger should succeed
        assert!(tracker.record_local_close_time(101, 1005));
        assert_eq!(tracker.window_len(), 2);
    }

    #[test]
    fn test_window_eviction() {
        let mut tracker = CloseTimeDriftTracker::with_config(5, 10);

        // Add 7 entries - should evict oldest 2
        for i in 0..7 {
            tracker.record_local_close_time(100 + i, 1000 + (i as u64) * 5);
        }

        assert_eq!(tracker.window_len(), 5);
        // Should have ledgers 102-106, not 100-101
        assert!(!tracker.window.contains_key(&100));
        assert!(!tracker.window.contains_key(&101));
        assert!(tracker.window.contains_key(&102));
        assert!(tracker.window.contains_key(&106));
    }

    #[test]
    fn test_drift_calculation() {
        // Use a larger window size so we can inspect stats before clearing
        let mut tracker = CloseTimeDriftTracker::with_config(10, 10);

        // Add 5 entries with known drift (window size is 10, so won't trigger clear)
        for i in 0u32..5 {
            let local_time = 1000 + (i as u64) * 5;
            // Network time is 2 seconds ahead of local
            let network_time = local_time + 2;

            tracker.record_local_close_time(100 + i, local_time);
            let _ = tracker.record_externalized_close_time(100 + i, network_time);
        }

        let stats = tracker.get_drift_stats().unwrap();
        assert_eq!(stats.min, 2);
        assert_eq!(stats.max, 2);
        assert_eq!(stats.median, 2);
        assert_eq!(stats.p75, 2);
        assert_eq!(stats.sample_count, 5);
    }

    #[test]
    fn test_drift_warning_triggered() {
        let mut tracker = CloseTimeDriftTracker::with_config(5, 10);

        // Add entries with drift exceeding threshold
        for i in 0u32..5 {
            let local_time = 1000 + (i as u64) * 5;
            // Network time is 15 seconds ahead (exceeds 10s threshold)
            let network_time = local_time + 15;

            tracker.record_local_close_time(100 + i, local_time);
            // Only the last one should trigger check (window full)
            let result = tracker.record_externalized_close_time(100 + i, network_time);

            if i == 4 {
                assert!(result.is_some(), "Should have warning on full window");
                let msg = result.unwrap();
                assert!(msg.contains("15"), "Should mention drift value");
                assert!(msg.contains("clock"), "Should mention clock");
            } else {
                assert!(result.is_none(), "Should not warn until window full");
            }
        }

        // Window should be cleared after check
        assert_eq!(tracker.window_len(), 0);
    }

    #[test]
    fn test_drift_no_warning_within_threshold() {
        let mut tracker = CloseTimeDriftTracker::with_config(5, 10);

        // Add entries with drift within threshold
        for i in 0u32..5 {
            let local_time = 1000 + (i as u64) * 5;
            // Network time is 5 seconds ahead (within 10s threshold)
            let network_time = local_time + 5;

            tracker.record_local_close_time(100 + i, local_time);
            let result = tracker.record_externalized_close_time(100 + i, network_time);

            // Should not warn even when window is full
            assert!(result.is_none());
        }

        // Window should still be cleared
        assert_eq!(tracker.window_len(), 0);
    }

    #[test]
    fn test_negative_drift() {
        let mut tracker = CloseTimeDriftTracker::with_config(5, 10);

        // Local clock is ahead of network (negative drift)
        for i in 0u32..5 {
            let local_time = 1000 + (i as u64) * 5;
            // Network time is 15 seconds behind local (negative drift)
            let network_time = local_time.saturating_sub(15);

            tracker.record_local_close_time(100 + i, local_time);
            let result = tracker.record_externalized_close_time(100 + i, network_time);

            if i == 4 {
                assert!(result.is_some());
                let msg = result.unwrap();
                assert!(msg.contains("-15"));
            }
        }
    }

    #[test]
    fn test_partial_externalization() {
        // Use larger window size to avoid clearing on fill
        let mut tracker = CloseTimeDriftTracker::with_config(10, 10);

        // Record local times for 5 ledgers (window size is 10, won't trigger clear)
        for i in 0u32..5 {
            tracker.record_local_close_time(100 + i, 1000 + (i as u64) * 5);
        }

        // Only externalize 3 of them
        let _ = tracker.record_externalized_close_time(100, 1002);
        let _ = tracker.record_externalized_close_time(101, 1007);
        let _ = tracker.record_externalized_close_time(102, 1012);

        assert_eq!(tracker.window_len(), 5);
        assert_eq!(tracker.completed_entries(), 3);

        let stats = tracker.get_drift_stats().unwrap();
        assert_eq!(stats.sample_count, 3);
    }

    #[test]
    fn test_varying_drift() {
        let mut tracker = CloseTimeDriftTracker::with_config(10, 20);

        // Add entries with varying drift: 1, 2, 3, ..., 10 seconds
        for i in 0u32..10 {
            let local_time = 1000 + (i as u64) * 5;
            let network_time = local_time + (i as u64) + 1; // drift = i + 1

            tracker.record_local_close_time(100 + i, local_time);
            let _ = tracker.record_externalized_close_time(100 + i, network_time);
        }

        // Before window check triggers
        let _stats = tracker.get_drift_stats();
        // Window should have been cleared since size == 10
        // Let's verify by checking the tracker state
        assert_eq!(tracker.window_len(), 0);
    }

    #[test]
    fn test_stats_calculation_detail() {
        let mut tracker = CloseTimeDriftTracker::with_config(20, 100); // Large threshold to avoid clearing

        // Add 8 entries with drifts: -5, -2, 0, 1, 3, 5, 8, 12
        let drifts = [-5i64, -2, 0, 1, 3, 5, 8, 12];
        for (i, &drift) in drifts.iter().enumerate() {
            let local_time = 1000u64;
            let network_time = (local_time as i64 + drift) as u64;
            tracker.record_local_close_time(100 + i as u32, local_time);
            let _ = tracker.record_externalized_close_time(100 + i as u32, network_time);
        }

        let stats = tracker.get_drift_stats().unwrap();
        assert_eq!(stats.min, -5);
        assert_eq!(stats.max, 12);
        assert_eq!(stats.sample_count, 8);
        // Median of 8 values is at index 4 (0-indexed) = 3
        assert_eq!(stats.median, 3);
        // 75th percentile: ceil(8 * 0.75) - 1 = ceil(6) - 1 = 5 (0-indexed) = 5
        // Wait, sorted: [-5, -2, 0, 1, 3, 5, 8, 12]
        // Index 5 is value 5
        assert_eq!(stats.p75, 5);
    }
}
