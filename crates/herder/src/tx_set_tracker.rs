//! Unified transaction-set state tracker.
//!
//! Owns all tx-set bookkeeping that was previously spread across
//! three independent DashMaps in [`ScpDriver`]: the parsed tx-set cache,
//! pending fetch requests, and the validity cache.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_crypto::RandomEvictionCache;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::scp_driver::{CachedTxSet, PendingTxSet};
use crate::tx_queue::TransactionSet;

/// Validity cache capacity — matches stellar-core's `TXSETVALID_CACHE_SIZE`
/// (`stellar-core/src/herder/HerderSCPDriver.cpp:39`).
const TXSET_VALID_CACHE_SIZE: usize = 1000;

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
    /// Uses `RandomEvictionCache` matching stellar-core's `mTxSetValidCache`.
    valid_cache: Mutex<RandomEvictionCache<(Hash256, Hash256, u64), bool>>,
    /// Maximum cache size.
    max_cache_size: usize,
    /// Monotonic counter for deterministic LRU eviction ordering.
    /// Relaxed ordering suffices — uniqueness is all we need.
    next_seq: AtomicU64,
}

impl TxSetTracker {
    pub fn new(max_cache_size: usize) -> Self {
        Self {
            cache: DashMap::new(),
            pending: DashMap::new(),
            valid_cache: Mutex::new(RandomEvictionCache::new(TXSET_VALID_CACHE_SIZE)),
            max_cache_size,
            next_seq: AtomicU64::new(0),
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

    /// Cache a parsed tx set, evicting the least-recently-touched if at capacity.
    ///
    /// Under concurrent access, eviction is best-effort LRU — capacity may
    /// briefly be exceeded and a stale victim may be chosen. Ordering is
    /// deterministic for single-threaded access.
    ///
    /// `pub(crate)` to ensure network-received tx sets go through `receive()`,
    /// which enforces the pending check (AUDIT-080).
    pub(crate) fn store(&self, tx_set: TransactionSet) {
        let hash = *tx_set.hash();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        // If already cached, replace in-place without evicting another entry.
        if self.cache.contains_key(&hash) {
            self.cache.insert(hash, CachedTxSet::new(tx_set, seq));
            return;
        }

        if self.max_cache_size == 0 {
            return;
        }

        if self.cache.len() >= self.max_cache_size {
            // Collect the key to evict before calling remove, to avoid holding
            // a DashMap shard read-lock while remove acquires a write-lock.
            let to_evict: Option<Hash256> = {
                let oldest = self.cache.iter().min_by_key(|e| e.touch_seq);
                oldest.map(|e| *e.key())
            };
            if let Some(k) = to_evict {
                self.cache.remove(&k);
            }
        }

        self.cache.insert(hash, CachedTxSet::new(tx_set, seq));
    }

    /// Receive a parsed tx set from the network. Verifies hash integrity,
    /// removes from pending, caches it. Returns the slot it was needed for.
    /// Only caches tx sets that were actually pending — unsolicited tx sets
    /// are rejected to prevent cache poisoning (AUDIT-080).
    pub fn receive(&self, tx_set: TransactionSet) -> Option<u64> {
        let hash = *tx_set.hash();
        let recomputed = tx_set.recompute_hash();
        if recomputed != hash {
            warn!(
                expected = %hash,
                computed = %recomputed,
                "Rejecting tx set with mismatched hash"
            );
            return None;
        }

        let pending = self.pending.remove(&hash);
        let slot = pending.map(|(_, p)| p.slot);

        if slot.is_some() {
            // Only cache tx sets we actually requested.
            self.store(tx_set);
        } else {
            debug!(%hash, "Ignoring unsolicited tx set (not pending)");
        }

        slot
    }

    /// Get a cached parsed tx set, refreshing its recency and incrementing
    /// request count. Refreshing `touch_seq` and `cached_at` prevents
    /// actively-used entries from being evicted by cache churn (AUDIT-080).
    pub fn get(&self, hash: &Hash256) -> Option<TransactionSet> {
        self.cache.get_mut(hash).map(|mut entry| {
            entry.request_count += 1;
            entry.cached_at = Instant::now();
            entry.touch_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            entry.tx_set.clone()
        })
    }

    /// Check if a tx set is cached.
    pub fn is_cached(&self, hash: &Hash256) -> bool {
        self.cache.contains_key(hash)
    }

    /// Check if a tx set is cached, and refresh its LRU touch_seq if so.
    /// This prevents eviction of tx-sets still referenced by buffered envelopes.
    pub fn is_cached_and_touch(&self, hash: &Hash256) -> bool {
        if let Some(mut entry) = self.cache.get_mut(hash) {
            entry.touch_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    // --- Validity cache ---

    /// Check if a validity result is cached.
    pub fn check_valid(&self, key: &(Hash256, Hash256, u64)) -> Option<bool> {
        self.valid_cache.lock().get(key).copied()
    }

    /// Store a validity result. Uses random-two-choice eviction at capacity.
    pub fn store_valid(&self, key: (Hash256, Hash256, u64), valid: bool) {
        self.valid_cache.lock().put(key, valid);
    }

    /// Clear validity cache. Called on externalization, NOT during trim.
    pub fn clear_valid_cache(&self) {
        self.valid_cache.lock().clear();
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
            valid_cache: self.valid_cache.lock().len(),
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
        let hash = *ts.hash();
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
        let hash = *ts.hash();
        tracker.request(hash, 100);
        tracker.store(ts);
        assert!(!tracker.needs(&hash));
    }

    #[test]
    fn test_receive_removes_pending() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(1);
        let hash = *ts.hash();
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
        let tracker = TxSetTracker::new(2);

        let ts1 = make_tx_set(1);
        let hash1 = *ts1.hash();
        tracker.store(ts1);

        let ts2 = make_tx_set(2);
        let hash2 = *ts2.hash();
        tracker.store(ts2);
        assert!(tracker.is_cached(&hash1));
        assert!(tracker.is_cached(&hash2));

        // Third insert at capacity — should evict ts1 (lowest touch_seq)
        let ts3 = make_tx_set(3);
        let hash3 = *ts3.hash();
        tracker.store(ts3);
        assert!(!tracker.is_cached(&hash1), "first entry should be evicted");
        assert!(tracker.is_cached(&hash2));
        assert!(tracker.is_cached(&hash3));
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

    #[test]
    fn test_valid_cache_bounded_at_capacity() {
        let tracker = TxSetTracker::new(256);
        let lcl = Hash256::from_bytes([0; 32]);
        // Fill to capacity
        for i in 0..TXSET_VALID_CACHE_SIZE {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xff) as u8;
            bytes[1] = ((i >> 8) & 0xff) as u8;
            let h = Hash256::from_bytes(bytes);
            tracker.store_valid((lcl, h, 0), true);
        }
        assert_eq!(tracker.sizes().valid_cache, TXSET_VALID_CACHE_SIZE);

        // Insert one more — should evict, staying at capacity
        let extra = Hash256::from_bytes([0xff; 32]);
        tracker.store_valid((lcl, extra, 0), true);
        assert_eq!(tracker.sizes().valid_cache, TXSET_VALID_CACHE_SIZE);
    }

    /// Regression test: overwriting an existing key at capacity must NOT
    /// spuriously evict another entry.
    #[test]
    fn test_store_valid_overwrite_at_capacity() {
        let tracker = TxSetTracker::new(256);
        let lcl = Hash256::from_bytes([0; 32]);
        let first_key_hash = Hash256::from_bytes([0; 32]);

        // Fill to capacity
        for i in 0..TXSET_VALID_CACHE_SIZE {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xff) as u8;
            bytes[1] = ((i >> 8) & 0xff) as u8;
            let h = Hash256::from_bytes(bytes);
            tracker.store_valid((lcl, h, 0), true);
        }
        assert_eq!(tracker.sizes().valid_cache, TXSET_VALID_CACHE_SIZE);

        // Overwrite the first key — size must remain at capacity, not shrink
        tracker.store_valid((lcl, first_key_hash, 0), false);
        assert_eq!(tracker.sizes().valid_cache, TXSET_VALID_CACHE_SIZE);
        // Verify the overwritten value is accessible
        assert_eq!(tracker.check_valid(&(lcl, first_key_hash, 0)), Some(false));
    }

    /// Regression test for AUDIT-080: unsolicited tx sets must not be cached.
    /// A peer sending unsolicited tx sets should not be able to pollute or evict
    /// entries from the cache.
    #[test]
    fn test_audit_080_unsolicited_tx_set_not_cached() {
        let tracker = TxSetTracker::new(4);

        // Register one pending tx set and receive it — should be cached
        let wanted = make_tx_set(1);
        let wanted_hash = *wanted.hash();
        tracker.request(wanted_hash, 100);
        let slot = tracker.receive(wanted.clone());
        assert_eq!(slot, Some(100));
        assert!(tracker.is_cached(&wanted_hash));

        // Send 10 unsolicited tx sets — none should be cached
        for i in 10..20u8 {
            let unsolicited = make_tx_set(i);
            let unsolicited_hash = *unsolicited.hash();
            let slot = tracker.receive(unsolicited);
            assert_eq!(slot, None, "unsolicited tx set should return None");
            assert!(
                !tracker.is_cached(&unsolicited_hash),
                "unsolicited tx set should not be cached"
            );
        }

        // The wanted tx set should still be in cache
        assert!(
            tracker.is_cached(&wanted_hash),
            "wanted tx set should survive unsolicited churn"
        );
        assert_eq!(tracker.sizes().cache, 1);
    }

    /// Regression test for AUDIT-080: get() refreshes touch_seq so actively-used
    /// entries are not evicted by cache pressure.
    #[test]
    fn test_audit_080_get_refreshes_cached_at() {
        let tracker = TxSetTracker::new(4);

        // Fill cache to capacity with 4 entries (seq 0, 1, 2, 3)
        for i in 0..4u8 {
            let ts = make_tx_set(i);
            let hash = *ts.hash();
            tracker.request(hash, 100 + i as u64);
            tracker.receive(ts);
        }
        assert_eq!(tracker.sizes().cache, 4);

        // Access entry 0 to refresh its touch_seq (now highest)
        let first_hash = *make_tx_set(0).hash();
        assert!(tracker.get(&first_hash).is_some());

        // Add a 5th entry — should evict entry 1 (lowest touch_seq after refresh)
        let new_ts = make_tx_set(99);
        let new_hash = *new_ts.hash();
        tracker.request(new_hash, 200);
        tracker.receive(new_ts);
        assert_eq!(tracker.sizes().cache, 4);

        // Entry 0 should still be cached (was refreshed via get)
        assert!(
            tracker.is_cached(&first_hash),
            "refreshed entry should survive eviction"
        );
        // Entry 1 should be evicted (lowest touch_seq)
        let second_hash = *make_tx_set(1).hash();
        assert!(
            !tracker.is_cached(&second_hash),
            "entry 1 should be the eviction victim"
        );
        // The new entry should be cached
        assert!(tracker.is_cached(&new_hash));
    }

    /// Re-storing an already-cached hash at capacity should replace in-place
    /// without evicting another entry.
    #[test]
    fn test_store_duplicate_hash_no_spurious_eviction() {
        let tracker = TxSetTracker::new(2);

        let ts1 = make_tx_set(1);
        let hash1 = *ts1.hash();
        tracker.store(ts1);

        let ts2 = make_tx_set(2);
        let hash2 = *ts2.hash();
        tracker.store(ts2);
        assert_eq!(tracker.sizes().cache, 2);

        // Re-store ts1 — should replace in-place, not evict ts2
        let ts1_again = make_tx_set(1);
        tracker.store(ts1_again);
        assert_eq!(tracker.sizes().cache, 2);
        assert!(tracker.is_cached(&hash1));
        assert!(
            tracker.is_cached(&hash2),
            "duplicate-hash store should not evict other entries"
        );
    }

    /// A tracker with max_cache_size == 0 should never cache anything.
    #[test]
    fn test_store_zero_capacity() {
        let tracker = TxSetTracker::new(0);

        let ts = make_tx_set(1);
        let hash = *ts.hash();
        tracker.store(ts);

        assert!(!tracker.is_cached(&hash));
        assert_eq!(tracker.sizes().cache, 0);
    }

    /// Regression test for #2070 review round 1: is_cached_and_touch refreshes
    /// LRU recency, preventing eviction of tx-sets still referenced by
    /// buffered envelopes.
    #[test]
    fn test_is_cached_and_touch_prevents_eviction() {
        let tracker = TxSetTracker::new(2);

        // Store two tx sets with explicit hashes.
        let hash_a = Hash256::from_bytes([0xAA; 32]);
        let hash_b = Hash256::from_bytes([0xBB; 32]);
        let ts_a = TransactionSet::with_unchecked_hash(Hash256::default(), hash_a, Vec::new());
        let ts_b = TransactionSet::with_unchecked_hash(Hash256::default(), hash_b, Vec::new());
        tracker.store(ts_a);
        tracker.store(ts_b);

        assert!(tracker.is_cached(&hash_a));
        assert!(tracker.is_cached(&hash_b));

        // Touch hash_a to refresh its recency.
        assert!(tracker.is_cached_and_touch(&hash_a));

        // Store a third tx set — should evict hash_b (oldest), not hash_a.
        let hash_c = Hash256::from_bytes([0xCC; 32]);
        let ts_c = TransactionSet::with_unchecked_hash(Hash256::default(), hash_c, Vec::new());
        tracker.store(ts_c);

        assert!(
            tracker.is_cached(&hash_a),
            "touched entry must survive eviction"
        );
        assert!(
            !tracker.is_cached(&hash_b),
            "untouched entry should be evicted"
        );
        assert!(tracker.is_cached(&hash_c));
    }

    #[test]
    fn test_is_cached_and_touch_returns_false_for_missing() {
        let tracker = TxSetTracker::new(10);
        let hash = Hash256::from_bytes([0xDD; 32]);
        assert!(!tracker.is_cached_and_touch(&hash));
    }
}
