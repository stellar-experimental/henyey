//! Metrics and counters for bucket operations.
//!
//! This module provides various counters and statistics for tracking bucket
//! operations, useful for monitoring and debugging.
//!
//! # Counter Types
//!
//! - [`MergeCounters`]: Statistics about bucket merge operations
//! - [`EvictionCounters`]: Statistics about eviction scanning
//! - [`BucketListMetrics`]: Overall bucket list metrics
//!
//! # Usage
//!
//! Counters are typically updated during bucket operations and can be queried
//! for monitoring purposes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::bucket_list::BUCKET_LIST_LEVELS;

// ============================================================================
// Merge Counters
// ============================================================================

/// Counters for bucket merge operations.
///
/// These counters track various aspects of bucket merging, including protocol
/// version handling and entry type statistics.
#[derive(Debug, Default)]
pub struct MergeCounters {
    /// Merges where the old bucket has pre-INITENTRY protocol entries.
    pub pre_init_entry_protocol_merges: AtomicU64,
    /// Merges where both buckets support INITENTRY.
    pub post_init_entry_protocol_merges: AtomicU64,
    /// New metadata entries created during merges.
    pub new_meta_entries: AtomicU64,
    /// New init entries created during merges.
    pub new_init_entries: AtomicU64,
    /// New live entries created during merges.
    pub new_live_entries: AtomicU64,
    /// New dead entries created during merges.
    pub new_dead_entries: AtomicU64,
    /// Old entries shadowed during merges.
    pub old_entries_shadowed: AtomicU64,
    /// Init+Dead pairs annihilated during merges.
    pub entries_annihilated: AtomicU64,
    /// Total merges completed.
    pub merges_completed: AtomicU64,
    /// Total merge time in microseconds.
    pub merge_time_us: AtomicU64,
    /// Reattachments to running (in-flight) merges.
    /// Matches stellar-core `mRunningMergeReattachments`.
    pub running_merge_reattachments: AtomicU64,
    /// Reattachments to finished (completed-cache) merges.
    /// Matches stellar-core `mFinishedMergeReattachments`.
    pub finished_merge_reattachments: AtomicU64,
}

impl MergeCounters {
    /// Creates new merge counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a pre-INITENTRY protocol merge.
    pub fn record_pre_init_entry_merge(&self) {
        self.pre_init_entry_protocol_merges
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a post-INITENTRY protocol merge.
    pub fn record_post_init_entry_merge(&self) {
        self.post_init_entry_protocol_merges
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records new entries by type.
    pub fn record_new_entry(&self, entry_type: EntryCountType) {
        match entry_type {
            EntryCountType::Meta => self.new_meta_entries.fetch_add(1, Ordering::Relaxed),
            EntryCountType::Init => self.new_init_entries.fetch_add(1, Ordering::Relaxed),
            EntryCountType::Live => self.new_live_entries.fetch_add(1, Ordering::Relaxed),
            EntryCountType::Dead => self.new_dead_entries.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Records a shadowed entry.
    pub fn record_shadowed(&self) {
        self.old_entries_shadowed.fetch_add(1, Ordering::Relaxed);
    }

    /// Records an annihilated Init+Dead pair.
    pub fn record_annihilated(&self) {
        self.entries_annihilated.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a completed merge.
    pub fn record_merge_completed(&self, duration_us: u64) {
        self.merges_completed.fetch_add(1, Ordering::Relaxed);
        self.merge_time_us.fetch_add(duration_us, Ordering::Relaxed);
    }

    /// Records a reattachment to a running (in-flight) merge.
    pub fn record_running_reattachment(&self) {
        self.running_merge_reattachments
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a reattachment to a finished (completed-cache) merge.
    pub fn record_finished_reattachment(&self) {
        self.finished_merge_reattachments
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Returns a snapshot of the counters.
    pub fn snapshot(&self) -> MergeCountersSnapshot {
        MergeCountersSnapshot {
            pre_init_entry_protocol_merges: self
                .pre_init_entry_protocol_merges
                .load(Ordering::Relaxed),
            post_init_entry_protocol_merges: self
                .post_init_entry_protocol_merges
                .load(Ordering::Relaxed),
            new_meta_entries: self.new_meta_entries.load(Ordering::Relaxed),
            new_init_entries: self.new_init_entries.load(Ordering::Relaxed),
            new_live_entries: self.new_live_entries.load(Ordering::Relaxed),
            new_dead_entries: self.new_dead_entries.load(Ordering::Relaxed),
            old_entries_shadowed: self.old_entries_shadowed.load(Ordering::Relaxed),
            entries_annihilated: self.entries_annihilated.load(Ordering::Relaxed),
            merges_completed: self.merges_completed.load(Ordering::Relaxed),
            merge_time_us: self.merge_time_us.load(Ordering::Relaxed),
            running_merge_reattachments: self.running_merge_reattachments.load(Ordering::Relaxed),
            finished_merge_reattachments: self.finished_merge_reattachments.load(Ordering::Relaxed),
        }
    }

    /// Resets all counters to zero.
    pub fn reset(&self) {
        self.pre_init_entry_protocol_merges
            .store(0, Ordering::Relaxed);
        self.post_init_entry_protocol_merges
            .store(0, Ordering::Relaxed);
        self.new_meta_entries.store(0, Ordering::Relaxed);
        self.new_init_entries.store(0, Ordering::Relaxed);
        self.new_live_entries.store(0, Ordering::Relaxed);
        self.new_dead_entries.store(0, Ordering::Relaxed);
        self.old_entries_shadowed.store(0, Ordering::Relaxed);
        self.entries_annihilated.store(0, Ordering::Relaxed);
        self.merges_completed.store(0, Ordering::Relaxed);
        self.merge_time_us.store(0, Ordering::Relaxed);
        self.running_merge_reattachments.store(0, Ordering::Relaxed);
        self.finished_merge_reattachments
            .store(0, Ordering::Relaxed);
    }
}

/// Entry type for counting purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryCountType {
    /// Metadata entry.
    Meta,
    /// Init entry (CAP-0020).
    Init,
    /// Live entry.
    Live,
    /// Dead entry (tombstone).
    Dead,
}

/// Snapshot of merge counters (non-atomic copy).
#[derive(Debug, Clone, Default)]
pub struct MergeCountersSnapshot {
    pub pre_init_entry_protocol_merges: u64,
    pub post_init_entry_protocol_merges: u64,
    pub new_meta_entries: u64,
    pub new_init_entries: u64,
    pub new_live_entries: u64,
    pub new_dead_entries: u64,
    pub old_entries_shadowed: u64,
    pub entries_annihilated: u64,
    pub merges_completed: u64,
    pub merge_time_us: u64,
    pub running_merge_reattachments: u64,
    pub finished_merge_reattachments: u64,
}

impl MergeCountersSnapshot {
    /// Returns the total number of new entries.
    pub fn total_new_entries(&self) -> u64 {
        self.new_meta_entries
            + self.new_init_entries
            + self.new_live_entries
            + self.new_dead_entries
    }

    /// Returns the average merge time in microseconds.
    pub fn avg_merge_time_us(&self) -> f64 {
        if self.merges_completed == 0 {
            0.0
        } else {
            self.merge_time_us as f64 / self.merges_completed as f64
        }
    }
}

// ============================================================================
// Eviction Counters
// ============================================================================

/// Counters for eviction scanning operations.
///
/// These counters track the eviction scanning process for Soroban state archival.
#[derive(Debug, Default)]
pub struct EvictionCounters {
    /// Number of entries evicted.
    pub entries_evicted: AtomicU64,
    /// Number of temporary entries evicted (deleted immediately).
    pub temp_entries_evicted: AtomicU64,
    /// Number of persistent entries archived.
    pub persistent_entries_archived: AtomicU64,
    /// Total bytes scanned.
    pub bytes_scanned: AtomicU64,
    /// Number of incomplete bucket scans (bucket changed during scan).
    pub incomplete_bucket_scans: AtomicU64,
    /// Number of eviction scan cycles completed.
    pub scan_cycles_completed: AtomicU64,
    /// Total scan time in microseconds.
    pub scan_time_us: AtomicU64,

    // ------------------------------------------------------------------
    // Cycle gauges (last completed cycle), mirroring stellar-core's
    // `EvictionStatistics`/`EvictionMetrics` (BucketUtils.cpp:189-220).
    //
    // Unlike the monotonic `fetch_add` counters above, these two are
    // SET-semantics gauges: each completed cycle OVERWRITES them via
    // `store` (matching medida `Counter::set_count`), so they reflect only
    // the most recently completed cycle, not a running total.
    // ------------------------------------------------------------------
    /// Ledger span of the most recently completed eviction cycle
    /// (`currLedgerSeq - cycleStartLedger`). Last-cycle gauge (set, not
    /// monotonic). Mirrors stellar-core `evictionCyclePeriod`.
    pub eviction_cycle_period: AtomicU64,
    /// Integer mean age of entries evicted during the most recently
    /// completed cycle (`sum_age / count`, or 0 when the cycle evicted
    /// nothing). Last-cycle gauge (set, not monotonic). Mirrors
    /// stellar-core `averageEvictedEntryAge`.
    pub average_evicted_entry_age: AtomicU64,

    // ------------------------------------------------------------------
    // Private cycle accumulators (not exposed in the snapshot). These back
    // the two gauges above and mirror stellar-core's `EvictionStatistics`
    // private members.
    // ------------------------------------------------------------------
    /// Running sum of evicted entry ages within the in-progress cycle.
    /// Mirrors `mEvictedEntriesAgeSum`.
    evicted_age_sum: AtomicU64,
    /// Number of entries evicted within the in-progress cycle.
    /// Mirrors `mNumEntriesEvicted`.
    evicted_count_for_cycle: AtomicU64,
    /// Ledger sequence at which the in-progress cycle started.
    /// Mirrors `mEvictionCycleStartLedger`.
    cycle_start_ledger: AtomicU64,
    /// Whether at least one full cycle has elapsed; gates metric recording
    /// to avoid noise from the first (baseline) cycle. Mirrors
    /// `mCompleteCycle`.
    complete_cycle: AtomicBool,
}

impl EvictionCounters {
    /// Creates new eviction counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records evicted entries.
    pub fn record_evicted(&self, count: u64, temp_count: u64, persistent_count: u64) {
        self.entries_evicted.fetch_add(count, Ordering::Relaxed);
        self.temp_entries_evicted
            .fetch_add(temp_count, Ordering::Relaxed);
        self.persistent_entries_archived
            .fetch_add(persistent_count, Ordering::Relaxed);
    }

    /// Records bytes scanned.
    pub fn record_bytes_scanned(&self, bytes: u64) {
        self.bytes_scanned.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Records an incomplete bucket scan.
    pub fn record_incomplete_scan(&self) {
        self.incomplete_bucket_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a completed scan cycle.
    pub fn record_scan_cycle(&self, duration_us: u64) {
        self.scan_cycles_completed.fetch_add(1, Ordering::Relaxed);
        self.scan_time_us.fetch_add(duration_us, Ordering::Relaxed);
    }

    /// Records the age of a single evicted entry within the current cycle.
    ///
    /// `age` is the entry's lifetime in ledgers
    /// (`liveUntilLedger - evictionLedger`). Mirrors stellar-core
    /// `EvictionStatistics::recordEvictedEntry`: accumulates into the
    /// per-cycle age sum and count, which are folded into the
    /// `average_evicted_entry_age` gauge when the cycle closes via
    /// [`submit_metrics_and_restart_cycle`](Self::submit_metrics_and_restart_cycle).
    pub fn record_evicted_entry_age(&self, age: u64) {
        self.evicted_age_sum.fetch_add(age, Ordering::Relaxed);
        self.evicted_count_for_cycle.fetch_add(1, Ordering::Relaxed);
    }

    /// Closes the current eviction cycle and starts a new one at
    /// `curr_ledger_seq`.
    ///
    /// Mirrors stellar-core `EvictionStatistics::submitMetricsAndRestartCycle`:
    /// only if a full cycle has already elapsed (`complete_cycle`) does it
    /// publish the two last-cycle gauges:
    /// - `eviction_cycle_period = curr_ledger_seq - cycle_start_ledger`
    ///   (ledger span of the cycle just completed), and
    /// - `average_evicted_entry_age = count == 0 ? 0 : sum / count`
    ///   (integer mean; no divide-by-zero).
    ///
    /// It then resets the per-cycle accumulators, advances the cycle start
    /// to `curr_ledger_seq`, and marks the cycle complete. The first call
    /// establishes the baseline and publishes nothing.
    pub fn submit_metrics_and_restart_cycle(&self, curr_ledger_seq: u32) {
        // Only record metrics once we've observed a complete cycle, to avoid
        // noise from the initial (baseline) cycle.
        if self.complete_cycle.load(Ordering::Relaxed) {
            let period = u64::from(curr_ledger_seq)
                .saturating_sub(self.cycle_start_ledger.load(Ordering::Relaxed));
            self.eviction_cycle_period.store(period, Ordering::Relaxed);

            // count == 0 → 0 (no divide-by-zero), matching stellar-core's
            // `mNumEntriesEvicted == 0 ? 0 : sum / count`.
            let count = self.evicted_count_for_cycle.load(Ordering::Relaxed);
            let average_age = self
                .evicted_age_sum
                .load(Ordering::Relaxed)
                .checked_div(count)
                .unwrap_or(0);
            self.average_evicted_entry_age
                .store(average_age, Ordering::Relaxed);
        }

        // Reset to start a new cycle.
        self.complete_cycle.store(true, Ordering::Relaxed);
        self.evicted_age_sum.store(0, Ordering::Relaxed);
        self.evicted_count_for_cycle.store(0, Ordering::Relaxed);
        self.cycle_start_ledger
            .store(u64::from(curr_ledger_seq), Ordering::Relaxed);
    }

    /// Returns a snapshot of the counters.
    pub fn snapshot(&self) -> EvictionCountersSnapshot {
        EvictionCountersSnapshot {
            entries_evicted: self.entries_evicted.load(Ordering::Relaxed),
            temp_entries_evicted: self.temp_entries_evicted.load(Ordering::Relaxed),
            persistent_entries_archived: self.persistent_entries_archived.load(Ordering::Relaxed),
            bytes_scanned: self.bytes_scanned.load(Ordering::Relaxed),
            incomplete_bucket_scans: self.incomplete_bucket_scans.load(Ordering::Relaxed),
            scan_cycles_completed: self.scan_cycles_completed.load(Ordering::Relaxed),
            scan_time_us: self.scan_time_us.load(Ordering::Relaxed),
            eviction_cycle_period: self.eviction_cycle_period.load(Ordering::Relaxed),
            average_evicted_entry_age: self.average_evicted_entry_age.load(Ordering::Relaxed),
        }
    }

    /// Resets all counters to zero.
    pub fn reset(&self) {
        self.entries_evicted.store(0, Ordering::Relaxed);
        self.temp_entries_evicted.store(0, Ordering::Relaxed);
        self.persistent_entries_archived.store(0, Ordering::Relaxed);
        self.bytes_scanned.store(0, Ordering::Relaxed);
        self.incomplete_bucket_scans.store(0, Ordering::Relaxed);
        self.scan_cycles_completed.store(0, Ordering::Relaxed);
        self.scan_time_us.store(0, Ordering::Relaxed);
        self.eviction_cycle_period.store(0, Ordering::Relaxed);
        self.average_evicted_entry_age.store(0, Ordering::Relaxed);
        self.evicted_age_sum.store(0, Ordering::Relaxed);
        self.evicted_count_for_cycle.store(0, Ordering::Relaxed);
        self.cycle_start_ledger.store(0, Ordering::Relaxed);
        self.complete_cycle.store(false, Ordering::Relaxed);
    }
}

/// Snapshot of eviction counters (non-atomic copy).
#[derive(Debug, Clone, Default)]
pub struct EvictionCountersSnapshot {
    pub entries_evicted: u64,
    pub temp_entries_evicted: u64,
    pub persistent_entries_archived: u64,
    pub bytes_scanned: u64,
    pub incomplete_bucket_scans: u64,
    pub scan_cycles_completed: u64,
    pub scan_time_us: u64,
    /// Ledger span of the most recently completed eviction cycle.
    /// Last-cycle gauge. Mirrors stellar-core `evictionCyclePeriod`.
    pub eviction_cycle_period: u64,
    /// Integer mean age of entries evicted during the most recently
    /// completed cycle. Last-cycle gauge. Mirrors stellar-core
    /// `averageEvictedEntryAge`.
    pub average_evicted_entry_age: u64,
}

impl EvictionCountersSnapshot {
    /// Returns the average scan time in microseconds.
    pub fn avg_scan_time_us(&self) -> f64 {
        if self.scan_cycles_completed == 0 {
            0.0
        } else {
            self.scan_time_us as f64 / self.scan_cycles_completed as f64
        }
    }

    /// Returns the eviction rate (entries per cycle).
    pub fn eviction_rate(&self) -> f64 {
        if self.scan_cycles_completed == 0 {
            0.0
        } else {
            self.entries_evicted as f64 / self.scan_cycles_completed as f64
        }
    }
}

// ============================================================================
// Bucket List Metrics
// ============================================================================

/// Overall metrics for the bucket list.
#[derive(Debug, Default)]
pub struct BucketListMetrics {
    /// Total number of entries across all buckets.
    pub total_entries: AtomicU64,
    /// Total size of all buckets in bytes.
    pub total_size_bytes: AtomicU64,
    /// Number of non-empty buckets.
    pub bucket_count: AtomicU64,
    /// Number of entries by level.
    pub entries_by_level: [AtomicU64; BUCKET_LIST_LEVELS],
    /// Size by level in bytes.
    pub size_by_level: [AtomicU64; BUCKET_LIST_LEVELS],
}

impl BucketListMetrics {
    /// Creates new bucket list metrics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates metrics for a specific level.
    pub fn update_level(&self, level: usize, entry_count: u64, size_bytes: u64) {
        if level < BUCKET_LIST_LEVELS {
            self.entries_by_level[level].store(entry_count, Ordering::Relaxed);
            self.size_by_level[level].store(size_bytes, Ordering::Relaxed);
        }
    }

    /// Recalculates totals from level metrics.
    pub fn recalculate_totals(&self) {
        let mut total_entries = 0u64;
        let mut total_size = 0u64;
        let mut bucket_count = 0u64;

        for level in 0..BUCKET_LIST_LEVELS {
            let entries = self.entries_by_level[level].load(Ordering::Relaxed);
            let size = self.size_by_level[level].load(Ordering::Relaxed);
            total_entries += entries;
            total_size += size;
            if entries > 0 {
                bucket_count += 2; // curr and snap buckets
            }
        }

        self.total_entries.store(total_entries, Ordering::Relaxed);
        self.total_size_bytes.store(total_size, Ordering::Relaxed);
        self.bucket_count.store(bucket_count, Ordering::Relaxed);
    }

    /// Returns a snapshot of the metrics.
    pub fn snapshot(&self) -> BucketListMetricsSnapshot {
        let mut entries_by_level = [0u64; BUCKET_LIST_LEVELS];
        let mut size_by_level = [0u64; BUCKET_LIST_LEVELS];

        for level in 0..BUCKET_LIST_LEVELS {
            entries_by_level[level] = self.entries_by_level[level].load(Ordering::Relaxed);
            size_by_level[level] = self.size_by_level[level].load(Ordering::Relaxed);
        }

        BucketListMetricsSnapshot {
            total_entries: self.total_entries.load(Ordering::Relaxed),
            total_size_bytes: self.total_size_bytes.load(Ordering::Relaxed),
            bucket_count: self.bucket_count.load(Ordering::Relaxed),
            entries_by_level,
            size_by_level,
        }
    }

    /// Resets all metrics to zero.
    pub fn reset(&self) {
        self.total_entries.store(0, Ordering::Relaxed);
        self.total_size_bytes.store(0, Ordering::Relaxed);
        self.bucket_count.store(0, Ordering::Relaxed);
        for level in 0..BUCKET_LIST_LEVELS {
            self.entries_by_level[level].store(0, Ordering::Relaxed);
            self.size_by_level[level].store(0, Ordering::Relaxed);
        }
    }
}

/// Snapshot of bucket list metrics (non-atomic copy).
#[derive(Debug, Clone, Default)]
pub struct BucketListMetricsSnapshot {
    pub total_entries: u64,
    pub total_size_bytes: u64,
    pub bucket_count: u64,
    pub entries_by_level: [u64; BUCKET_LIST_LEVELS],
    pub size_by_level: [u64; BUCKET_LIST_LEVELS],
}

impl BucketListMetricsSnapshot {
    /// Returns the average entries per bucket.
    pub fn avg_entries_per_bucket(&self) -> f64 {
        if self.bucket_count == 0 {
            0.0
        } else {
            self.total_entries as f64 / self.bucket_count as f64
        }
    }

    /// Returns the total size in MB.
    pub fn total_size_mb(&self) -> f64 {
        self.total_size_bytes as f64 / (1024.0 * 1024.0)
    }

    /// Returns the size for a specific level in MB.
    pub fn size_mb_for_level(&self, level: usize) -> f64 {
        if level < BUCKET_LIST_LEVELS {
            self.size_by_level[level] as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_counters() {
        let counters = MergeCounters::new();

        counters.record_pre_init_entry_merge();
        counters.record_post_init_entry_merge();
        counters.record_post_init_entry_merge();
        counters.record_new_entry(EntryCountType::Live);
        counters.record_new_entry(EntryCountType::Live);
        counters.record_new_entry(EntryCountType::Dead);
        counters.record_shadowed();
        counters.record_annihilated();
        counters.record_merge_completed(1000);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.pre_init_entry_protocol_merges, 1);
        assert_eq!(snapshot.post_init_entry_protocol_merges, 2);
        assert_eq!(snapshot.new_live_entries, 2);
        assert_eq!(snapshot.new_dead_entries, 1);
        assert_eq!(snapshot.old_entries_shadowed, 1);
        assert_eq!(snapshot.entries_annihilated, 1);
        assert_eq!(snapshot.merges_completed, 1);
        assert_eq!(snapshot.merge_time_us, 1000);
        assert_eq!(snapshot.total_new_entries(), 3);
    }

    #[test]
    fn test_eviction_counters() {
        let counters = EvictionCounters::new();

        counters.record_evicted(10, 3, 7);
        counters.record_bytes_scanned(1024);
        counters.record_incomplete_scan();
        counters.record_scan_cycle(500);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.entries_evicted, 10);
        assert_eq!(snapshot.temp_entries_evicted, 3);
        assert_eq!(snapshot.persistent_entries_archived, 7);
        assert_eq!(snapshot.bytes_scanned, 1024);
        assert_eq!(snapshot.incomplete_bucket_scans, 1);
        assert_eq!(snapshot.scan_cycles_completed, 1);
        assert_eq!(snapshot.eviction_rate(), 10.0);
    }

    #[test]
    fn test_eviction_cycle_metrics_record_on_complete_cycle() {
        let counters = EvictionCounters::new();

        // First call establishes the baseline cycle: nothing recorded yet.
        counters.submit_metrics_and_restart_cycle(100);
        let snap = counters.snapshot();
        assert_eq!(snap.eviction_cycle_period, 0);
        assert_eq!(snap.average_evicted_entry_age, 0);

        // Record several entry ages during the cycle.
        counters.record_evicted_entry_age(10);
        counters.record_evicted_entry_age(20);
        counters.record_evicted_entry_age(30);

        // Closing the cycle at ledger 110 records the span and integer average.
        counters.submit_metrics_and_restart_cycle(110);
        let snap = counters.snapshot();
        assert_eq!(snap.eviction_cycle_period, 10); // 110 - 100
        assert_eq!(snap.average_evicted_entry_age, 20); // (10 + 20 + 30) / 3
    }

    #[test]
    fn test_eviction_average_age_zero_when_no_evictions() {
        let counters = EvictionCounters::new();

        // Establish baseline cycle.
        counters.submit_metrics_and_restart_cycle(100);
        // Close a second cycle with no recorded ages: count == 0 branch.
        counters.submit_metrics_and_restart_cycle(110);

        let snap = counters.snapshot();
        assert_eq!(snap.eviction_cycle_period, 10);
        assert_eq!(snap.average_evicted_entry_age, 0); // no divide-by-zero
    }

    #[test]
    fn test_eviction_cycle_period_resets_each_cycle() {
        let counters = EvictionCounters::new();

        // Baseline at 100.
        counters.submit_metrics_and_restart_cycle(100);
        // Second wrap at 110: period == 110 - 100.
        counters.submit_metrics_and_restart_cycle(110);
        assert_eq!(counters.snapshot().eviction_cycle_period, 10);
        // Third wrap at 145: cycle start was reset to 110, so period == 145 - 110.
        counters.submit_metrics_and_restart_cycle(145);
        assert_eq!(counters.snapshot().eviction_cycle_period, 35);
    }

    #[test]
    fn test_bucket_list_metrics() {
        let metrics = BucketListMetrics::new();

        metrics.update_level(0, 100, 10000);
        metrics.update_level(1, 200, 20000);
        metrics.recalculate_totals();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_entries, 300);
        assert_eq!(snapshot.total_size_bytes, 30000);
        assert_eq!(snapshot.bucket_count, 4); // 2 buckets per level
        assert_eq!(snapshot.entries_by_level[0], 100);
        assert_eq!(snapshot.entries_by_level[1], 200);
    }

    #[test]
    fn test_counter_reset() {
        let merge_counters = MergeCounters::new();
        merge_counters.record_merge_completed(1000);
        assert_eq!(merge_counters.snapshot().merges_completed, 1);
        merge_counters.reset();
        assert_eq!(merge_counters.snapshot().merges_completed, 0);

        let eviction_counters = EvictionCounters::new();
        eviction_counters.record_evicted(10, 5, 5);
        // Drive a full cycle so the cycle gauges and accumulators are non-zero.
        eviction_counters.submit_metrics_and_restart_cycle(100);
        eviction_counters.record_evicted_entry_age(40);
        eviction_counters.submit_metrics_and_restart_cycle(110);
        let snap = eviction_counters.snapshot();
        assert_eq!(snap.entries_evicted, 10);
        assert_eq!(snap.eviction_cycle_period, 10);
        assert_eq!(snap.average_evicted_entry_age, 40);
        eviction_counters.reset();
        let snap = eviction_counters.snapshot();
        assert_eq!(snap.entries_evicted, 0);
        assert_eq!(snap.eviction_cycle_period, 0);
        assert_eq!(snap.average_evicted_entry_age, 0);
        // After reset, the next cycle is again treated as a fresh baseline:
        // a single submit records nothing.
        eviction_counters.submit_metrics_and_restart_cycle(200);
        eviction_counters.record_evicted_entry_age(5);
        eviction_counters.submit_metrics_and_restart_cycle(205);
        let snap = eviction_counters.snapshot();
        assert_eq!(snap.eviction_cycle_period, 5);
        assert_eq!(snap.average_evicted_entry_age, 5);
    }
}
