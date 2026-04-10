//! Unified transaction-set state tracker.
//!
//! Owns all tx-set bookkeeping that was previously spread across
//! three independent DashMaps in [`ScpDriver`]: the parsed tx-set cache,
//! pending fetch requests, and the validity cache.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use henyey_common::Hash256;
use tracing::{debug, info, warn};

use super::scp_driver::{CachedTxSet, PendingTxSet};
use crate::tx_queue::TransactionSet;

/// Diagnostic sizes for the tx-set tracker.
#[derive(Debug, Clone, Default)]
pub struct TxSetTrackerSizes {
    pub cache: usize,
    pub pending: usize,
    pub valid_cache: usize,
}

/// Unified transaction-set state tracker.
///
/// Replaces `ScpDriver.tx_set_cache`, `pending_tx_sets`, and `tx_set_valid_cache`.
pub struct TxSetTracker {
    /// Cached parsed transaction sets by hash.
    cache: DashMap<Hash256, CachedTxSet>,
    /// Pending tx-set requests: hash → slot + timing metadata.
    pending: DashMap<Hash256, PendingTxSet>,
    /// Validity cache: (lcl_hash, tx_set_hash, close_time_offset) → valid.
    /// Cleared on externalization, not during trim.
    valid_cache: DashMap<(Hash256, Hash256, u64), bool>,
    /// Maximum cache size.
    max_cache_size: usize,
}

impl TxSetTracker {
    pub fn new(max_cache_size: usize) -> Self {
        Self {
            cache: DashMap::new(),
            pending: DashMap::new(),
            valid_cache: DashMap::new(),
            max_cache_size,
        }
    }

    // --- Pending management ---

    /// Register a pending tx-set request. Returns true if new.
    pub fn request(&self, hash: Hash256, slot: u64) -> bool {
        if self.cache.contains_key(&hash) {
            return false;
        }

        if self.pending.contains_key(&hash) {
            if let Some(mut entry) = self.pending.get_mut(&hash) {
                entry.request_count += 1;
            }
            return false;
        }

        self.pending.insert(
            hash,
            PendingTxSet {
                hash,
                slot,
                requested_at: Instant::now(),
                request_count: 1,
            },
        );
        debug!(%hash, slot, "Registered pending tx set request");
        true
    }

    /// Check if we need a tx set (pending and not cached).
    pub fn needs(&self, hash: &Hash256) -> bool {
        self.pending.contains_key(hash) && !self.cache.contains_key(hash)
    }

    /// Get all pending hashes.
    pub fn pending_hashes(&self) -> Vec<Hash256> {
        self.pending.iter().map(|e| *e.key()).collect()
    }

    /// Get pending entries with slots.
    pub fn pending_entries(&self) -> Vec<(Hash256, u64)> {
        self.pending
            .iter()
            .map(|e| (*e.key(), e.value().slot))
            .collect()
    }

    /// Clear all pending requests.
    pub fn clear_pending(&self) {
        let count = self.pending.len();
        self.pending.clear();
        if count > 0 {
            debug!(count, "Cleared all pending tx set requests");
        }
    }

    /// Check if any pending request has been waiting longer than max_wait_secs.
    pub fn has_stale_pending(&self, max_wait_secs: u64) -> bool {
        let now = Instant::now();
        let max_wait = Duration::from_secs(max_wait_secs);
        self.pending
            .iter()
            .any(|entry| now.duration_since(entry.value().requested_at) >= max_wait)
    }

    /// Remove pending requests older than max_age_secs.
    pub fn cleanup_by_age(&self, max_age_secs: u64) {
        let cutoff = Instant::now() - Duration::from_secs(max_age_secs);
        self.pending.retain(|_, v| v.requested_at > cutoff);
    }

    /// Remove pending requests for slots older than current_slot.
    /// Returns count removed.
    pub fn cleanup_old_slots(&self, current_slot: u64) -> usize {
        let old_count = self.pending.len();
        self.pending.retain(|_, v| v.slot >= current_slot);
        old_count - self.pending.len()
    }

    // --- Cache management ---

    /// Cache a parsed tx set, evicting oldest if at capacity.
    pub fn store(&self, tx_set: TransactionSet) {
        let hash = tx_set.hash;

        if self.cache.len() >= self.max_cache_size {
            if let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|e| e.cached_at)
                .map(|e| *e.key())
            {
                self.cache.remove(&oldest);
            }
        }

        self.cache.insert(hash, CachedTxSet::new(tx_set));
    }

    /// Receive a parsed tx set from the network. Verifies hash integrity,
    /// removes from pending, caches it. Returns the slot it was needed for.
    pub fn receive(&self, tx_set: TransactionSet) -> Option<u64> {
        let hash = tx_set.hash;
        if let Some(recomputed) = tx_set.recompute_hash() {
            if recomputed != hash {
                warn!(
                    expected = %hash,
                    computed = %recomputed,
                    "Rejecting tx set with mismatched hash"
                );
                return None;
            }
        } else {
            warn!(%hash, "Rejecting tx set without recomputable hash");
            return None;
        }

        let pending = self.pending.remove(&hash);
        let slot = pending.map(|(_, p)| p.slot);

        self.store(tx_set);

        if let Some(s) = slot {
            debug!(%hash, slot = s, "Received pending tx set");
        } else {
            debug!(%hash, "Received tx set (not pending)");
        }

        slot
    }

    /// Get a cached parsed tx set, incrementing its request count.
    pub fn get(&self, hash: &Hash256) -> Option<TransactionSet> {
        self.cache.get_mut(hash).map(|mut entry| {
            entry.request_count += 1;
            entry.tx_set.clone()
        })
    }

    /// Check if a tx set is cached.
    pub fn is_cached(&self, hash: &Hash256) -> bool {
        self.cache.contains_key(hash)
    }

    // --- Validity cache ---

    /// Check if a validity result is cached.
    pub fn check_valid(&self, key: &(Hash256, Hash256, u64)) -> Option<bool> {
        self.valid_cache.get(key).map(|v| *v)
    }

    /// Store a validity result.
    pub fn store_valid(&self, key: (Hash256, Hash256, u64), valid: bool) {
        self.valid_cache.insert(key, valid);
    }

    /// Clear validity cache. Called on externalization, NOT during trim.
    pub fn clear_valid_cache(&self) {
        self.valid_cache.clear();
    }

    // --- Cleanup ---

    /// Trim pending entries: keep only slots > keep_after_slot.
    /// Does NOT touch cache (hash-keyed, bounded by max size).
    /// Does NOT touch valid_cache (cleared on externalization).
    pub fn trim_stale_pending(&self, keep_after_slot: u64) {
        self.pending
            .retain(|_, pending| pending.slot > keep_after_slot);
    }

    /// Clear cache + pending. Does NOT touch valid_cache.
    pub fn clear_all(&self) {
        let cache_count = self.cache.len();
        let pending_count = self.pending.len();
        self.cache.clear();
        self.pending.clear();
        if cache_count > 0 || pending_count > 0 {
            info!(cache_count, pending_count, "Cleared tx_set_tracker caches");
        }
    }

    /// Clear only the tx-set cache (not pending, not valid_cache).
    pub fn clear_cache(&self) {
        let count = self.cache.len();
        self.cache.clear();
        if count > 0 {
            info!(count, "Cleared tx_set_cache");
        }
    }

    // --- Diagnostics ---

    pub fn sizes(&self) -> TxSetTrackerSizes {
        TxSetTrackerSizes {
            cache: self.cache.len(),
            pending: self.pending.len(),
            valid_cache: self.valid_cache.len(),
        }
    }

    pub fn cache_count(&self) -> usize {
        self.cache.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tx_set(seed: u8) -> TransactionSet {
        let prev_hash = Hash256::from_bytes([seed; 32]);
        TransactionSet::new(prev_hash, Vec::new())
    }

    #[test]
    fn test_request_new_returns_true() {
        let tracker = TxSetTracker::new(256);
        let hash = Hash256::from_bytes([1; 32]);
        assert!(tracker.request(hash, 100));
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn test_request_duplicate_returns_false() {
        let tracker = TxSetTracker::new(256);
        let hash = Hash256::from_bytes([1; 32]);
        assert!(tracker.request(hash, 100));
        assert!(!tracker.request(hash, 100));
        assert_eq!(tracker.pending_count(), 1);
    }

    #[test]
    fn test_request_cached_returns_false() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        let hash = ts.hash;
        tracker.store(ts);
        assert!(!tracker.request(hash, 100));
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_needs_pending_not_cached() {
        let tracker = TxSetTracker::new(256);
        let hash = Hash256::from_bytes([1; 32]);
        tracker.request(hash, 100);
        assert!(tracker.needs(&hash));
    }

    #[test]
    fn test_needs_false_when_cached() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        let hash = ts.hash;
        tracker.request(hash, 100);
        tracker.store(ts);
        assert!(!tracker.needs(&hash));
    }

    #[test]
    fn test_receive_removes_pending() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        let hash = ts.hash;
        tracker.request(hash, 42);

        let slot = tracker.receive(ts);
        assert_eq!(slot, Some(42));
        assert_eq!(tracker.pending_count(), 0);
        assert!(tracker.is_cached(&hash));
    }

    #[test]
    fn test_cleanup_old_slots() {
        let tracker = TxSetTracker::new(256);
        tracker.request(Hash256::from_bytes([1; 32]), 10);
        tracker.request(Hash256::from_bytes([2; 32]), 20);
        tracker.request(Hash256::from_bytes([3; 32]), 30);

        let removed = tracker.cleanup_old_slots(20);
        assert_eq!(removed, 1); // slot 10 removed
        assert_eq!(tracker.pending_count(), 2);
    }

    #[test]
    fn test_trim_stale_pending_preserves_future() {
        let tracker = TxSetTracker::new(256);
        tracker.request(Hash256::from_bytes([1; 32]), 10);
        tracker.request(Hash256::from_bytes([2; 32]), 20);
        tracker.request(Hash256::from_bytes([3; 32]), 30);

        tracker.trim_stale_pending(15);
        assert_eq!(tracker.pending_count(), 2); // slots 20, 30 kept
    }

    #[test]
    fn test_trim_stale_pending_does_not_touch_cache() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        tracker.store(ts);
        tracker.request(Hash256::from_bytes([2; 32]), 10);

        tracker.trim_stale_pending(100);
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.cache_count(), 1); // cache untouched
    }

    #[test]
    fn test_trim_stale_pending_does_not_touch_valid_cache() {
        let tracker = TxSetTracker::new(256);
        let key = (
            Hash256::from_bytes([1; 32]),
            Hash256::from_bytes([2; 32]),
            0,
        );
        tracker.store_valid(key, true);
        tracker.request(Hash256::from_bytes([3; 32]), 10);

        tracker.trim_stale_pending(100);
        assert!(tracker.check_valid(&key).is_some()); // valid_cache untouched
    }

    #[test]
    fn test_clear_valid_cache_independent() {
        let tracker = TxSetTracker::new(256);
        let key = (
            Hash256::from_bytes([1; 32]),
            Hash256::from_bytes([2; 32]),
            0,
        );
        tracker.store_valid(key, true);
        let ts = make_tx_set(1);
        tracker.store(ts);

        tracker.clear_valid_cache();
        assert!(tracker.check_valid(&key).is_none());
        assert_eq!(tracker.cache_count(), 1); // cache untouched
    }

    #[test]
    fn test_clear_all_does_not_touch_valid_cache() {
        let tracker = TxSetTracker::new(256);
        let key = (
            Hash256::from_bytes([1; 32]),
            Hash256::from_bytes([2; 32]),
            0,
        );
        tracker.store_valid(key, true);
        let ts = make_tx_set(1);
        tracker.store(ts);
        tracker.request(Hash256::from_bytes([3; 32]), 10);

        tracker.clear_all();
        assert_eq!(tracker.cache_count(), 0);
        assert_eq!(tracker.pending_count(), 0);
        assert!(tracker.check_valid(&key).is_some()); // valid_cache preserved
    }

    #[test]
    fn test_clear_cache_only() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        tracker.store(ts);
        tracker.request(Hash256::from_bytes([2; 32]), 10);

        tracker.clear_cache();
        assert_eq!(tracker.cache_count(), 0);
        assert_eq!(tracker.pending_count(), 1); // pending untouched
    }

    #[test]
    fn test_store_evicts_oldest() {
        let tracker = TxSetTracker::new(1);

        let ts1 = make_tx_set(1);
        let hash1 = ts1.hash;
        tracker.store(ts1);
        assert!(tracker.is_cached(&hash1));

        let ts2 = make_tx_set(2);
        let hash2 = ts2.hash;
        tracker.store(ts2);
        assert!(tracker.is_cached(&hash2));
        assert!(!tracker.is_cached(&hash1)); // evicted
    }

    #[test]
    fn test_has_stale_pending() {
        let tracker = TxSetTracker::new(256);
        tracker.request(Hash256::from_bytes([1; 32]), 10);

        // With a very long timeout, nothing is stale
        assert!(!tracker.has_stale_pending(9999));
        // With 0 timeout, everything is stale
        assert!(tracker.has_stale_pending(0));
    }
}
