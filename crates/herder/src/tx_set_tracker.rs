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

/// Best-effort cap on pending tx-set requests.
/// Under concurrent access the map may briefly exceed this by a few entries.
/// Defense-in-depth against unbounded growth from forged tx-set hash references.
/// Normal operation uses ≤12 entries (one per slot in MAX_SLOTS_TO_REMEMBER);
/// 512 provides >40× headroom. Matches `QuorumSetTracker::MAX_PENDING_QSET_REQUESTS`.
const MAX_PENDING_TXSET_REQUESTS: usize = 512;

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
    /// Returns false if already pending, already cached, or the pending map
    /// has reached its best-effort capacity cap (`MAX_PENDING_TXSET_REQUESTS`).
    ///
    /// # Reentrancy
    ///
    /// Callers (including tests) MUST NOT hold any guard into `self.pending`
    /// (e.g. a `dashmap::mapref::one::Ref`/`RefMut` from `pending.get` or
    /// `pending.get_mut`) when calling this. `request` acquires a write lock
    /// on the destination shard via `pending.entry(...)` and read locks on
    /// every shard via `pending.len()`; same-thread reentry will deadlock
    /// whenever the held guard's shard collides with one of those.
    pub fn request(&self, hash: Hash256, slot: u64) -> bool {
        if self.cache.contains_key(&hash) {
            return false;
        }

        // Pre-check cap BEFORE entry() — DashMap::len() acquires read locks on
        // all shards; calling it while holding an entry guard (shard write lock)
        // would deadlock.
        let at_cap = self.pending.len() >= MAX_PENDING_TXSET_REQUESTS;

        match self.pending.entry(hash) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                // Always increment — cap does not apply to existing entries.
                let pending = entry.get_mut();
                pending.request_count += 1;
                // Propagate max slot: a later slot referencing the same hash
                // should keep the entry alive longer during slot-based eviction
                // (matches stellar-core's last-seen-slot semantics).
                if slot > pending.slot {
                    pending.slot = slot;
                }
                false
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                if at_cap {
                    debug!("Dropping tx set request: pending cap reached");
                    return false;
                }
                entry.insert(PendingTxSet {
                    hash,
                    slot,
                    requested_at: Instant::now(),
                    request_count: 1,
                });
                debug!(%hash, slot, "Registered pending tx set request");
                true
            }
        }
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
    ///
    /// **Atomicity guarantees:**
    /// - **Occupied (re-store):** Fully atomic — updates in place via entry guard.
    /// - **Vacant, under capacity:** Fully atomic — inserts via entry guard.
    /// - **Vacant, at capacity:** Best-effort — drops entry guard to evict, then
    ///   re-inserts without a guard. A concurrent store of the same hash in this
    ///   window results in a benign overwrite; concurrent stores of different hashes
    ///   may briefly overshoot `max_cache_size` by one entry (acceptable, same as
    ///   the pre-fix behavior).
    pub(crate) fn store(&self, tx_set: TransactionSet) {
        self.store_inner(tx_set, None);
    }

    /// Cache a parsed tx set with slot provenance (from pending→cached promotion).
    /// If the hash already exists, keeps the **max** slot seen.
    pub(crate) fn store_with_slot(&self, tx_set: TransactionSet, slot: u64) {
        self.store_inner(tx_set, Some(slot));
    }

    fn store_inner(&self, tx_set: TransactionSet, slot: Option<u64>) {
        let hash = *tx_set.hash();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);

        if self.max_cache_size == 0 {
            return;
        }

        // Pre-check capacity BEFORE entry() — DashMap::len() acquires read locks
        // on all shards; calling it while holding an entry guard would deadlock.
        let at_capacity = self.cache.len() >= self.max_cache_size;

        match self.cache.entry(hash) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                // Already cached — atomic in-place update, no eviction needed.
                // Keep max slot: if existing has a higher slot, preserve it.
                let existing_slot = entry.get().slot;
                let merged_slot = match (existing_slot, slot) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                let mut new_entry = CachedTxSet::new(tx_set, seq);
                new_entry.slot = merged_slot;
                *entry.get_mut() = new_entry;
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                if at_capacity {
                    // Must drop entry guard before evicting — iter() acquires
                    // read locks on all shards, which deadlocks with entry's
                    // shard write lock.
                    drop(entry);
                    let to_evict: Option<Hash256> = {
                        let oldest = self.cache.iter().min_by_key(|e| e.touch_seq);
                        oldest.map(|e| *e.key())
                    };
                    if let Some(k) = to_evict {
                        self.cache.remove(&k);
                    }
                    // Best-effort insert after eviction. Not atomic with the
                    // vacancy check, but the common Occupied path is now race-free.
                    let new_entry = if let Some(s) = slot {
                        CachedTxSet::new_with_slot(tx_set, seq, s)
                    } else {
                        CachedTxSet::new(tx_set, seq)
                    };
                    // slot already set via constructor
                    self.cache.insert(hash, new_entry);
                } else {
                    // Under capacity — fully atomic insert via entry guard.
                    let new_entry = if let Some(s) = slot {
                        CachedTxSet::new_with_slot(tx_set, seq, s)
                    } else {
                        CachedTxSet::new(tx_set, seq)
                    };
                    entry.insert(new_entry);
                }
            }
        }
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

        if let Some(s) = slot {
            // Only cache tx sets we actually requested — with slot provenance.
            self.store_with_slot(tx_set, s);
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
    #[cfg(test)]
    pub fn is_cached_and_touch(&self, hash: &Hash256) -> bool {
        self.is_cached_and_touch_slot(hash, None)
    }

    /// Check if a tx set is cached, refresh its LRU touch_seq, and optionally
    /// propagate `slot` to `max(current, new)` — matching stellar-core's
    /// last-seen-slot semantics (`touchFetchCache` / `getKnownTxSet`).
    pub fn is_cached_and_touch_slot(&self, hash: &Hash256, slot: Option<u64>) -> bool {
        if let Some(mut entry) = self.cache.get_mut(hash) {
            entry.touch_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            if let Some(new_slot) = slot {
                entry.slot = Some(entry.slot.map_or(new_slot, |s| s.max(new_slot)));
            }
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
    ///
    /// Returns the previous cached value for this key, if any.
    ///
    /// # Panics
    ///
    /// Panics when a previously-invalid entry would be overwritten with `true`
    /// (false→true flip). This is fatal per HERDER_SPEC §9.7 / INV-H8 and
    /// mirrors stellar-core's `cacheValidTxSet` which throws
    /// `std::runtime_error("Inconsistent txSet validity for tx set ...")`.
    /// The panic fires BEFORE the overwrite so the cached `false` remains intact.
    pub fn store_valid(&self, key: (Hash256, Hash256, u64), valid: bool) -> Option<bool> {
        let mut cache = self.valid_cache.lock();
        let previous = cache.get(&key).copied();
        if previous == Some(false) && valid {
            // Fatal: the tx-set hash is the second tuple element.
            let tx_set_hash = key.1;
            panic!(
                "Inconsistent txSet validity for tx set {} (INV-H8): \
                 cached as invalid, now revalidated as valid",
                tx_set_hash.to_hex()
            );
        }
        cache.put(key, valid);
        previous
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

    /// Evict cached tx sets with a known slot <= `keep_after_slot`.
    /// Entries with `slot: None` (unknown-slot sentinel from direct `cache_tx_set()`)
    /// are preserved — they were not learned from pending fetches and must not be
    /// over-pruned by slot-range cleanup.
    /// Returns the number of entries evicted.
    pub fn trim_stale_cached(&self, keep_after_slot: u64) -> usize {
        let before = self.cache.len();
        self.cache
            .retain(|_, entry| entry.slot.map_or(true, |s| s > keep_after_slot));
        before - self.cache.len()
    }

    /// Evict cached tx sets with a known slot < `cutoff`.
    /// Entries with `slot: None` are preserved (unknown-slot sentinel).
    /// Returns the number of entries evicted.
    pub fn purge_cached_below(&self, cutoff: u64) -> usize {
        let before = self.cache.len();
        self.cache
            .retain(|_, entry| entry.slot.map_or(true, |s| s >= cutoff));
        before - self.cache.len()
    }

    /// Evict cached tx sets whose slot is in the given set of removed slots.
    /// Entries with `slot: None` are preserved (unknown-slot sentinel).
    /// Returns the number of entries evicted.
    pub fn evict_cached_for_slots(&self, slots: &[u64]) -> usize {
        if slots.is_empty() {
            return 0;
        }
        let slot_set: std::collections::HashSet<u64> = slots.iter().copied().collect();
        let before = self.cache.len();
        self.cache
            .retain(|_, entry| entry.slot.map_or(true, |s| !slot_set.contains(&s)));
        before - self.cache.len()
    }

    /// Evict cached tx sets with a known slot > `max_slot`.
    /// Entries with `slot: None` are preserved (unknown-slot sentinel).
    /// Parity: stellar-core `PendingEnvelopes::eraseOutsideRange` evicts tx-set
    /// cache entries with `slot > maxSlot` during `newSlotExternalized`.
    /// Returns the number of entries evicted.
    pub fn evict_cached_above(&self, max_slot: u64) -> usize {
        let before = self.cache.len();
        self.cache
            .retain(|_, entry| entry.slot.map_or(true, |s| s <= max_slot));
        before - self.cache.len()
    }

    /// Count cached tx sets whose slot falls within [from_slot, to_slot] (inclusive).
    /// Entries with `slot: None` are NOT counted (they are from direct inserts,
    /// not from pending fetches in the active catchup window).
    pub fn cached_count_in_window(&self, from_slot: u64, to_slot: u64) -> usize {
        self.cache
            .iter()
            .filter(|entry| entry.slot.map_or(false, |s| s >= from_slot && s <= to_slot))
            .count()
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

    /// Return all hashes currently in the cache. Test-only.
    #[cfg(test)]
    pub fn cached_hashes(&self) -> Vec<Hash256> {
        self.cache.iter().map(|entry| *entry.key()).collect()
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

    #[test]
    fn test_store_valid_returns_previous_value() {
        let tracker = TxSetTracker::new(256);
        let lcl = Hash256::from_bytes([0; 32]);
        let key_hash = Hash256::from_bytes([1; 32]);
        let key = (lcl, key_hash, 0u64);

        // First store: no previous value
        assert_eq!(tracker.store_valid(key, true), None);

        // Overwrite true→true: previous is true
        assert_eq!(tracker.store_valid(key, true), Some(true));

        // Overwrite true→false: previous is true
        assert_eq!(tracker.store_valid(key, false), Some(true));

        // NOTE: false→true flip is now fatal (INV-H8, #2818) — tested
        // separately in test_store_valid_false_to_true_flip_panics_before_overwrite.
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

    /// Regression test: pending entries are capped to prevent unbounded memory
    /// growth from forged tx-set hash references.
    #[test]
    fn test_pending_cap_rejects_overflow() {
        let tracker = TxSetTracker::new(256);

        // Fill to the cap
        for i in 0..MAX_PENDING_TXSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            assert!(tracker.request(hash, 100));
        }
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS);

        // Next request should be rejected
        let overflow_hash = Hash256::from_bytes([0xFF; 32]);
        assert!(!tracker.request(overflow_hash, 100));
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS);
    }

    /// Regression test: the cap is self-healing — receiving a pending tx set
    /// frees a slot, allowing the next request to succeed.
    #[test]
    fn test_pending_cap_self_heals_after_receive() {
        let tracker = TxSetTracker::new(256);

        // Fill to cap, using the first entry as a "real" tx set we can receive
        let real_ts = make_tx_set(0);
        let real_hash = *real_ts.hash();
        assert!(tracker.request(real_hash, 100));

        for i in 1..MAX_PENDING_TXSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            assert!(tracker.request(hash, 100));
        }
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS);

        // Verify overflow is rejected
        let overflow = Hash256::from_bytes([0xFE; 32]);
        assert!(!tracker.request(overflow, 100));

        // Receive the real tx set — frees one slot
        let slot = tracker.receive(real_ts);
        assert_eq!(slot, Some(100));
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS - 1);

        // Now a new request should succeed
        let new_hash = Hash256::from_bytes([0xFD; 32]);
        assert!(tracker.request(new_hash, 101));
    }

    /// Boundary test: duplicate request at cap still returns false and does not
    /// grow the map beyond the cap.
    #[test]
    fn test_pending_cap_preserves_dedup_semantics() {
        let tracker = TxSetTracker::new(256);

        // Fill to cap
        let first_hash = Hash256::from_bytes([0; 32]);
        assert!(tracker.request(first_hash, 100));
        for i in 1..MAX_PENDING_TXSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            assert!(tracker.request(hash, 100));
        }

        // Duplicate request for an existing entry — returns false (dedup, not cap)
        assert!(!tracker.request(first_hash, 100));
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS);
    }

    /// Boundary test: request for an already-cached hash returns false before
    /// the cap check is reached (cached check has priority).
    #[test]
    fn test_pending_cap_preserves_cached_check() {
        let tracker = TxSetTracker::new(256);

        // Cache a tx set
        let cached_ts = make_tx_set(42);
        let cached_hash = *cached_ts.hash();
        tracker.store(cached_ts);

        // Fill pending to cap
        for i in 0..MAX_PENDING_TXSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            tracker.request(hash, 100);
        }

        // Request for cached hash still returns false (cache hit, not cap)
        assert!(!tracker.request(cached_hash, 100));
        // Pending count unchanged (not inserted)
        assert_eq!(tracker.pending_count(), MAX_PENDING_TXSET_REQUESTS);
    }

    // --- Concurrent regression tests for entry() TOCTOU fix (#2469) ---

    #[test]
    fn test_concurrent_request_same_hash() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tracker = Arc::new(TxSetTracker::new(256));
        let hash = Hash256::from_bytes([42; 32]);
        let n = 16;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    tracker.request(hash, 100)
                })
            })
            .collect();

        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let true_count = results.iter().filter(|&&r| r).count();

        // Exactly one thread should win the insert.
        assert_eq!(true_count, 1, "exactly one request() should return true");
        assert_eq!(tracker.pending_count(), 1);

        // request_count should equal total requests.
        let entry = tracker.pending.get(&hash).unwrap();
        assert_eq!(entry.request_count, n as u32);
    }

    #[test]
    fn test_concurrent_store_same_hash_no_spurious_eviction() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        // Cache size 2: store two distinct hashes, then race-store a third
        // that is the same as the first. The Occupied path should fire for
        // at least some threads, preventing unnecessary eviction of hash B.
        let tracker = Arc::new(TxSetTracker::new(2));
        let ts_a = make_tx_set(1);
        let ts_b = make_tx_set(2);
        let hash_a = *ts_a.hash();
        let hash_b = *ts_b.hash();

        tracker.store(ts_a.clone());
        tracker.store(ts_b.clone());
        assert_eq!(tracker.cache_count(), 2);

        let n = 8;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                let ts = ts_a.clone();
                thread::spawn(move || {
                    barrier.wait();
                    tracker.store(ts);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // hash_a should still be cached (updated in place).
        assert!(
            tracker.get(&hash_a).is_some(),
            "hash_a should still be cached"
        );
        // hash_b should still be cached (no spurious eviction from same-hash stores).
        assert!(
            tracker.get(&hash_b).is_some(),
            "hash_b should not be evicted"
        );
    }

    #[test]
    fn test_request_at_cap_still_increments_existing() {
        let tracker = TxSetTracker::new(256);

        // Fill to capacity.
        for i in 0..MAX_PENDING_TXSET_REQUESTS {
            let mut bytes = [0u8; 32];
            bytes[0] = (i & 0xFF) as u8;
            bytes[1] = ((i >> 8) & 0xFF) as u8;
            let hash = Hash256::from_bytes(bytes);
            tracker.request(hash, 100);
        }

        // The first hash we inserted should still be incrementable.
        let first_hash = Hash256::from_bytes([0u8; 32]);
        assert!(!tracker.request(first_hash, 100));
        // Snapshot the field by value and let the `Ref` drop at the end of
        // this statement BEFORE re-entering `tracker.request(...)` below.
        // Holding the shard read-guard across `request()` would deadlock
        // whenever `new_hash` collides with `first_hash` on a DashMap shard,
        // because `request()` needs a write lock on the destination shard.
        let request_count = tracker.pending.get(&first_hash).unwrap().request_count;
        assert_eq!(
            request_count, 2,
            "existing entry should increment even at cap"
        );

        // REGRESSION GUARD for PR #2747 / issues #2744, #2750.
        //
        // The pre-fix code held a `dashmap::mapref::one::Ref` into `pending`
        // across the call to `tracker.request(new_hash, ...)`. `request()`
        // takes a write lock on the destination shard via `pending.entry(...)`.
        // When `new_hash` and `first_hash` land on the same DashMap shard,
        // the still-live read guard from `pending.get(&first_hash)` causes
        // `parking_lot`'s shard `RwLock::write()` call to deadlock the same
        // thread.
        //
        // DashMap's shard count is `4 * num_cpus().next_power_of_two()`, so
        // whether two arbitrary hashes share a shard depends on the runner.
        // To make this regression test fail _deterministically_ on every
        // runner if the snapshot pattern above is ever reverted, we pick
        // `new_hash` to be provably on the same shard as `first_hash` using
        // `DashMap::determine_map` (gated behind dashmap's `raw-api`
        // dev-feature in `crates/herder/Cargo.toml`).
        //
        // Inlining `tracker.pending.get(&first_hash).unwrap().request_count`
        // into the `assert_eq!` above (or re-binding it to a `let entry =
        // tracker.pending.get(...)` that outlives the next `tracker.request`
        // call) resurrects the deadlock.
        let target_shard = tracker.pending.determine_map(&first_hash);
        let new_hash = {
            let mut found = None;
            for i in 0u32..1_000_000 {
                let mut bytes = [0xFFu8; 32];
                bytes[0..4].copy_from_slice(&i.to_le_bytes());
                let candidate = Hash256::from_bytes(bytes);
                if candidate == first_hash {
                    continue;
                }
                if tracker.pending.contains_key(&candidate) {
                    continue;
                }
                if tracker.pending.determine_map(&candidate) == target_shard {
                    found = Some(candidate);
                    break;
                }
            }
            found.expect(
                "must find a hash on the same DashMap shard as first_hash within 1M tries; \
                 shard count is 4 * num_cpus().next_power_of_two() so this should converge in O(shards)",
            )
        };
        // Load-bearing invariant: if this ever fires, the regression guard
        // below has lost its teeth and the test silently stops catching the
        // bug it was written for.
        assert_eq!(
            tracker.pending.determine_map(&first_hash),
            tracker.pending.determine_map(&new_hash),
            "test invariant: first_hash and new_hash must collide on a DashMap shard \
             so a Ref-held-across-request() regression deadlocks deterministically"
        );

        // A brand new hash should be rejected.
        assert!(!tracker.request(new_hash, 100));
        assert!(
            tracker.pending.get(&new_hash).is_none(),
            "new hash rejected at cap"
        );
    }

    #[test]
    fn test_concurrent_store_new_hash_at_full_cache() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        // Cache size 2, pre-filled with 2 entries. Race-store N new distinct
        // hashes concurrently. Validates the best-effort eviction path doesn't
        // panic and the cache stabilizes at max_cache_size.
        let tracker = Arc::new(TxSetTracker::new(2));
        tracker.store(make_tx_set(1));
        tracker.store(make_tx_set(2));
        assert_eq!(tracker.cache_count(), 2);

        let n = 8;
        let barrier = Arc::new(Barrier::new(n));

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                // Each thread stores a unique new hash.
                let ts = make_tx_set(10 + i as u8);
                thread::spawn(move || {
                    barrier.wait();
                    tracker.store(ts);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Cache overshoot is expected under concurrent full-cache stores because
        // the eviction path is best-effort (drops entry guard, evicts, then inserts
        // without rechecking capacity). The key invariant is: no panic, and the
        // cache remains finite and bounded by max_cache_size + concurrent_writers.
        assert!(
            tracker.cache_count() <= 2 + n,
            "cache overshoot exceeds concurrent writers (got {})",
            tracker.cache_count()
        );
        // At least 1 entry should survive (not everything evicted).
        assert!(
            tracker.cache_count() >= 1,
            "cache should not be empty after stores"
        );
    }

    /// Regression test for #2818: A false→true validity-cache flip must panic
    /// (fatal per HERDER_SPEC §9.7 / INV-H8) and must NOT overwrite the cached
    /// `false` entry.
    #[test]
    fn test_store_valid_false_to_true_flip_panics_before_overwrite() {
        let tracker = TxSetTracker::new(256);
        let lcl = Hash256::from_bytes([0; 32]);
        let key_hash = Hash256::from_bytes([1; 32]);
        let key = (lcl, key_hash, 0u64);

        // Seed the cache with false.
        tracker.store_valid(key, false);
        assert_eq!(tracker.check_valid(&key), Some(false));

        // Attempt false→true: must panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracker.store_valid(key, true);
        }));
        assert!(result.is_err(), "false→true flip must panic (INV-H8)");

        // The cached value must still be `false` — the panic fires BEFORE the
        // overwrite so the invariant-violating `true` is never persisted.
        assert_eq!(
            tracker.check_valid(&key),
            Some(false),
            "cached value must remain false after panic"
        );
    }

    /// Regression test for #2818: The panic message for a false→true flip must
    /// contain the upstream-style prefix and the tx-set hash so operators can
    /// correlate it with the offending tx set.
    #[test]
    fn test_store_valid_false_to_true_flip_panic_mentions_tx_set_hash() {
        let tracker = TxSetTracker::new(256);
        let lcl = Hash256::from_bytes([0; 32]);
        let key_hash = Hash256::from_bytes([0xab; 32]);
        let key = (lcl, key_hash, 0u64);

        tracker.store_valid(key, false);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracker.store_valid(key, true);
        }));
        let err = result.expect_err("false→true flip must panic");
        let msg = err
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");

        assert!(
            msg.contains("Inconsistent txSet validity for tx set"),
            "panic message must contain upstream prefix, got: {msg}"
        );
        // The tx-set hash component of the key should appear in the message.
        let hash_hex = key_hash.to_hex();
        assert!(
            msg.contains(&hash_hex),
            "panic message must contain tx-set hash {hash_hex}, got: {msg}"
        );
    }

    #[test]
    fn test_range_cleanup_preserves_unknown_slot_cached_entries() {
        let tracker = TxSetTracker::new(256);

        // Store a tx set through the direct `store()` path (no slot provenance)
        let ts_direct = make_tx_set(50);
        let hash_direct = *ts_direct.hash();
        tracker.store(ts_direct);

        // Store a tx set with slot provenance (via request+receive)
        let ts_slotted_old = make_tx_set(51);
        let hash_old = *ts_slotted_old.hash();
        tracker.request(hash_old, 90);
        tracker.receive(ts_slotted_old);

        let ts_slotted_new = make_tx_set(52);
        let hash_new = *ts_slotted_new.hash();
        tracker.request(hash_new, 110);
        tracker.receive(ts_slotted_new);

        // Verify all three are cached
        assert!(tracker.is_cached(&hash_direct));
        assert!(tracker.is_cached(&hash_old));
        assert!(tracker.is_cached(&hash_new));

        // Run slot-range cleanup: trim stale cached with keep_after_slot = 100
        let evicted = tracker.trim_stale_cached(100);
        assert_eq!(evicted, 1); // Only the old slotted entry (slot 90)

        // Unknown-slot entry is preserved
        assert!(tracker.is_cached(&hash_direct));
        // Old slotted entry (slot 90 <= 100) is evicted
        assert!(!tracker.is_cached(&hash_old));
        // New slotted entry (slot 110 > 100) is preserved
        assert!(tracker.is_cached(&hash_new));
    }

    #[test]
    fn test_cached_count_in_window() {
        let tracker = TxSetTracker::new(256);

        // Direct store (no slot) — should NOT count in any window
        let ts_direct = make_tx_set(60);
        tracker.store(ts_direct);

        // Slotted entries
        let ts1 = make_tx_set(61);
        let h1 = *ts1.hash();
        tracker.request(h1, 50);
        tracker.receive(ts1);

        let ts2 = make_tx_set(62);
        let h2 = *ts2.hash();
        tracker.request(h2, 100);
        tracker.receive(ts2);

        let ts3 = make_tx_set(63);
        let h3 = *ts3.hash();
        tracker.request(h3, 150);
        tracker.receive(ts3);

        // Window [51, 120] should only include slot 100
        assert_eq!(tracker.cached_count_in_window(51, 120), 1);
        // Window [50, 150] should include all slotted entries
        assert_eq!(tracker.cached_count_in_window(50, 150), 3);
        // Window [200, 300] should include none
        assert_eq!(tracker.cached_count_in_window(200, 300), 0);
    }

    #[test]
    fn test_request_propagates_max_slot() {
        let tracker = TxSetTracker::new(256);
        let hash = Hash256::from_bytes([0xAA; 32]);

        // First request at slot 10
        assert!(tracker.request(hash, 10));
        assert_eq!(tracker.pending.get(&hash).unwrap().slot, 10);

        // Duplicate request at slot 20 — should propagate max
        assert!(!tracker.request(hash, 20));
        assert_eq!(tracker.pending.get(&hash).unwrap().slot, 20);

        // Duplicate request at slot 5 — should NOT regress
        assert!(!tracker.request(hash, 5));
        assert_eq!(tracker.pending.get(&hash).unwrap().slot, 20);
    }

    #[test]
    fn test_is_cached_and_touch_slot_propagates_max() {
        let tracker = TxSetTracker::new(256);
        let ts = make_tx_set(42);
        let hash = *ts.hash();

        // Request at slot 10, receive it → cached with slot=10
        tracker.request(hash, 10);
        tracker.receive(ts);
        assert_eq!(tracker.cache.get(&hash).unwrap().slot, Some(10));

        // Touch with slot 20 → should update to 20
        assert!(tracker.is_cached_and_touch_slot(&hash, Some(20)));
        assert_eq!(tracker.cache.get(&hash).unwrap().slot, Some(20));

        // Touch with slot 5 → should NOT regress
        assert!(tracker.is_cached_and_touch_slot(&hash, Some(5)));
        assert_eq!(tracker.cache.get(&hash).unwrap().slot, Some(20));

        // Touch with None → should keep 20
        assert!(tracker.is_cached_and_touch_slot(&hash, None));
        assert_eq!(tracker.cache.get(&hash).unwrap().slot, Some(20));
    }

    #[test]
    fn test_evict_cached_above_drops_entries_above_max_slot() {
        let tracker = TxSetTracker::new(256);

        // Direct store (no slot) — should be preserved
        let ts_direct = make_tx_set(70);
        let hash_direct = *ts_direct.hash();
        tracker.store(ts_direct);

        // Slotted entries
        let ts_within = make_tx_set(71);
        let hash_within = *ts_within.hash();
        tracker.request(hash_within, 100);
        tracker.receive(ts_within);

        let ts_at_boundary = make_tx_set(72);
        let hash_at = *ts_at_boundary.hash();
        tracker.request(hash_at, 200);
        tracker.receive(ts_at_boundary);

        let ts_above = make_tx_set(73);
        let hash_above = *ts_above.hash();
        tracker.request(hash_above, 201);
        tracker.receive(ts_above);

        let ts_far_above = make_tx_set(74);
        let hash_far = *ts_far_above.hash();
        tracker.request(hash_far, 9999);
        tracker.receive(ts_far_above);

        // Evict entries with slot > 200
        let evicted = tracker.evict_cached_above(200);
        assert_eq!(evicted, 2); // ts_above (201) and ts_far_above (9999)

        // Direct-store (no slot) preserved
        assert!(tracker.is_cached(&hash_direct));
        // slot 100 preserved
        assert!(tracker.is_cached(&hash_within));
        // slot 200 (boundary, <=) preserved
        assert!(tracker.is_cached(&hash_at));
        // slot 201 evicted
        assert!(!tracker.is_cached(&hash_above));
        // slot 9999 evicted
        assert!(!tracker.is_cached(&hash_far));
    }
}
