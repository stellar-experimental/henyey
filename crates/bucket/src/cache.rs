//! Random eviction cache for frequently-accessed account entries.
//!
//! This module provides an LRU-style cache for bucket entries that are
//! frequently accessed during transaction validation. The cache only stores
//! ACCOUNT entries, matching stellar-core's behavior.
//!
//! # Design
//!
//! The cache uses a "least-recent-out-of-2-random-choices" eviction strategy,
//! matching stellar-core's approach. This degrades more gracefully across
//! pathological load patterns than strict LRU, with less bookkeeping. Only
//! ACCOUNT entries are cached, as they are the most frequently accessed.
//!
//! # Thread Safety
//!
//! The cache uses a `Mutex` for thread-safe access. All operations are
//! synchronized to prevent data races.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use stellar_xdr::curr::{LedgerEntry, LedgerEntryData, LedgerKey};

use crate::entry::BucketEntry;


// ============================================================================
// Cache Entry
// ============================================================================

/// A cached bucket entry with access metadata.
#[derive(Debug, Clone)]
struct CacheEntry {
    /// The cached entry.
    entry: Arc<BucketEntry>,
    /// Estimated size in bytes.
    size_bytes: usize,
    /// Access counter for LRU tracking.
    access_count: u64,
    /// Index into CacheInner::keys for O(1) swap-remove.
    vec_index: usize,
}

impl CacheEntry {
    /// Creates a new cache entry.
    fn new(entry: BucketEntry, access_count: u64, vec_index: usize) -> Self {
        let size_bytes = Self::estimate_size(&entry);
        Self {
            entry: Arc::new(entry),
            size_bytes,
            access_count,
            vec_index,
        }
    }

    /// Estimates the memory size of a bucket entry.
    fn estimate_size(entry: &BucketEntry) -> usize {
        // Base size of the enum variant
        let base_size = std::mem::size_of::<BucketEntry>();

        // Add estimated size of contained data
        let data_size = match entry {
            BucketEntry::Live(e) | BucketEntry::Init(e) => Self::estimate_ledger_entry_size(e),
            BucketEntry::Dead(_) => 64, // LedgerKey is relatively small
            BucketEntry::Metadata(_) => 32,
        };

        base_size + data_size
    }

    /// Estimates the size of a ledger entry.
    fn estimate_ledger_entry_size(entry: &LedgerEntry) -> usize {
        match &entry.data {
            LedgerEntryData::Account(acc) => {
                // Account entry: ~200 bytes base + signers
                200 + acc.signers.len() * 72
            }
            LedgerEntryData::Trustline(_) => 150,
            LedgerEntryData::Offer(_) => 200,
            LedgerEntryData::Data(data) => 100 + data.data_value.len(),
            LedgerEntryData::ClaimableBalance(cb) => 200 + cb.claimants.len() * 100,
            LedgerEntryData::LiquidityPool(_) => 300,
            LedgerEntryData::ContractData(_) => {
                // Contract data can be large - use a conservative estimate
                // since ScVal doesn't easily report its size
                500
            }
            LedgerEntryData::ContractCode(cc) => {
                // Contract code is often large
                100 + cc.code.len()
            }
            LedgerEntryData::ConfigSetting(_) => 500,
            LedgerEntryData::Ttl(_) => 50,
        }
    }
}

// ============================================================================
// Random Eviction Cache
// ============================================================================

/// A cache with "least-recent-out-of-2-random-choices" eviction for bucket entries.
///
/// This cache stores frequently-accessed bucket entries to reduce disk I/O
/// during transaction validation. Eviction uses the "power of two choices"
/// strategy (matching stellar-core): pick two random entries, evict whichever
/// was accessed less recently. This is O(1) per eviction and approximates
/// LRU quality with minimal bookkeeping.
///
/// # Entry Type Filtering
///
/// Only ACCOUNT entries are cached, matching stellar-core's behavior.
///
/// # Memory Management
///
/// The cache tracks both entry count and estimated memory usage. Eviction
/// is triggered when either limit is exceeded.
pub struct RandomEvictionCache {
    /// The cache storage, protected by a mutex.
    inner: Mutex<CacheInner>,
    /// Maximum cache size in bytes.
    max_bytes: usize,
    /// Maximum number of entries.
    max_entries: usize,
    /// Current cache size in bytes (atomic for lock-free reads).
    current_bytes: AtomicUsize,
    /// Whether the cache is initialized and active.
    active: AtomicUsize,
}

/// Inner cache state protected by mutex.
struct CacheInner {
    /// Maps keys to cached entries.
    entries: HashMap<LedgerKey, CacheEntry>,
    /// Keys stored redundantly for O(1) random access during eviction.
    /// Indices are kept in sync with `CacheEntry::vec_index`.
    keys: Vec<LedgerKey>,
    /// Global access counter for LRU tracking.
    access_counter: u64,
    /// Simple xorshift64 RNG state for eviction sampling.
    rng_state: u64,
    /// Cache hit count (only ACCOUNT entries are cached).
    hits: u64,
    /// Cache miss count (only ACCOUNT entries are cached).
    misses: u64,
}

impl CacheInner {
    /// Returns a pseudo-random index in [0, len).
    fn rand_index(&mut self, len: usize) -> usize {
        // xorshift64
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        (x as usize) % len
    }
}

impl RandomEvictionCache {
    /// Creates a new cache with the given limits.
    pub fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::with_capacity(max_entries.min(1024)),
                keys: Vec::with_capacity(max_entries.min(1024)),
                access_counter: 0,
                rng_state: 0x5EED_CAFE_BABE_D00D, // arbitrary non-zero seed
                hits: 0,
                misses: 0,
            }),
            max_bytes,
            max_entries,
            current_bytes: AtomicUsize::new(0),
            active: AtomicUsize::new(0), // Not active initially
        }
    }

    /// Checks if the cache is active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }

    /// Activates the cache.
    pub fn activate(&self) {
        self.active.store(1, Ordering::Release);
    }

    /// Deactivates the cache and clears all entries.
    pub fn deactivate(&self) {
        self.active.store(0, Ordering::Release);
        self.clear();
    }

    /// Checks if an entry type should be cached.
    ///
    /// Only ACCOUNT entries are cached, matching stellar-core's behavior.
    pub fn is_cached_type(key: &LedgerKey) -> bool {
        matches!(key, LedgerKey::Account(_))
    }

    /// Gets an entry from the cache.
    ///
    /// Returns `None` if the entry is not cached or if the cache is not active.
    pub fn get(&self, key: &LedgerKey) -> Option<Arc<BucketEntry>> {
        if !self.is_active() || !Self::is_cached_type(key) {
            return None;
        }

        let mut inner = self.inner.lock();
        inner.access_counter += 1;
        let current_counter = inner.access_counter;

        if let Some(entry) = inner.entries.get_mut(key) {
            entry.access_count = current_counter;
            let result = Arc::clone(&entry.entry);
            inner.hits += 1;
            Some(result)
        } else {
            inner.misses += 1;
            None
        }
    }

    /// Inserts an entry into the cache.
    ///
    /// If the cache is at capacity, one entry is evicted using the
    /// "least-recent-out-of-2-random-choices" strategy (matching stellar-core).
    /// Does nothing if the cache is not active or if the entry type should not
    /// be cached.
    pub fn insert(&self, key: LedgerKey, entry: BucketEntry) {
        if !self.is_active() || !Self::is_cached_type(&key) {
            return;
        }

        let mut inner = self.inner.lock();
        inner.access_counter += 1;
        let access_count = inner.access_counter;

        // Check if updating an existing entry (no eviction or vec change needed)
        if let Some(existing) = inner.entries.get_mut(&key) {
            let old_size = existing.size_bytes;
            let new_entry = CacheEntry::new(entry, access_count, existing.vec_index);
            let new_size = new_entry.size_bytes;
            *existing = new_entry;
            let size_diff = new_size as isize - old_size as isize;
            if size_diff > 0 {
                self.current_bytes
                    .fetch_add(size_diff as usize, Ordering::Relaxed);
            } else if size_diff < 0 {
                self.current_bytes
                    .fetch_sub((-size_diff) as usize, Ordering::Relaxed);
            }
            return;
        }

        // New entry — evict if at capacity
        let new_entry = CacheEntry::new(entry, access_count, 0); // vec_index set below
        let entry_size = new_entry.size_bytes;

        if inner.keys.len() >= self.max_entries
            || self.current_bytes.load(Ordering::Relaxed) + entry_size > self.max_bytes
        {
            self.evict_one(&mut inner);
        }

        // Insert into keys vec and hashmap
        let vec_index = inner.keys.len();
        inner.keys.push(key.clone());
        let cache_entry = CacheEntry {
            vec_index,
            ..new_entry
        };
        inner.entries.insert(key, cache_entry);
        self.current_bytes.fetch_add(entry_size, Ordering::Relaxed);
    }

    /// Removes an entry from the cache.
    pub fn remove(&self, key: &LedgerKey) {
        if !self.is_active() {
            return;
        }

        let mut inner = self.inner.lock();
        if let Some(entry) = inner.entries.remove(key) {
            self.current_bytes
                .fetch_sub(entry.size_bytes, Ordering::Relaxed);
            Self::swap_remove_key(&mut inner, entry.vec_index);
        }
    }

    /// Clears all entries from the cache.
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.entries.clear();
        inner.keys.clear();
        inner.hits = 0;
        inner.misses = 0;
        self.current_bytes.store(0, Ordering::Relaxed);
    }

    /// Evicts one entry using the "least-recent-out-of-2-random-choices"
    /// strategy, matching stellar-core's `RandomEvictionCache::evictOne()`.
    ///
    /// Picks two random entries and evicts whichever was accessed less recently.
    /// This is O(1) and approximates LRU quality with minimal bookkeeping.
    fn evict_one(&self, inner: &mut CacheInner) {
        let sz = inner.keys.len();
        if sz == 0 {
            return;
        }

        // Pick two random candidates and evict the less-recently-used one
        let idx1 = inner.rand_index(sz);
        let idx2 = inner.rand_index(sz);
        let access1 = inner.entries[&inner.keys[idx1]].access_count;
        let access2 = inner.entries[&inner.keys[idx2]].access_count;
        let victim_idx = if access1 <= access2 { idx1 } else { idx2 };

        // Remove from hashmap
        let victim_key = inner.keys[victim_idx].clone();
        let entry = inner.entries.remove(&victim_key).unwrap();
        self.current_bytes
            .fetch_sub(entry.size_bytes, Ordering::Relaxed);

        // Swap-remove from keys vec
        Self::swap_remove_key(inner, victim_idx);
    }

    /// Swap-removes a key from the keys vec at `idx`, updating the swapped
    /// entry's `vec_index` in the hashmap.
    fn swap_remove_key(inner: &mut CacheInner, idx: usize) {
        let last_idx = inner.keys.len() - 1;
        if idx != last_idx {
            inner.keys.swap(idx, last_idx);
            // Clone the key so we can mutably borrow entries
            let swapped_key = inner.keys[idx].clone();
            inner.entries.get_mut(&swapped_key).unwrap().vec_index = idx;
        }
        inner.keys.pop();
    }

    /// Returns cache statistics.
    pub fn stats(&self) -> CacheStats {
        let inner = self.inner.lock();
        CacheStats {
            entry_count: inner.entries.len(),
            size_bytes: self.current_bytes.load(Ordering::Relaxed),
            max_bytes: self.max_bytes,
            max_entries: self.max_entries,
            hits: inner.hits,
            misses: inner.misses,
            hit_rate: if inner.hits + inner.misses > 0 {
                inner.hits as f64 / (inner.hits + inner.misses) as f64
            } else {
                0.0
            },
            active: self.is_active(),
        }
    }

    /// Returns the current number of cached entries.
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// Checks if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().entries.is_empty()
    }

    /// Returns the current cache size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.current_bytes.load(Ordering::Relaxed)
    }

    /// Resets hit/miss counters without clearing cached entries.
    ///
    /// Useful for collecting per-ledger statistics: call `stats()` to snapshot
    /// the counters, then `reset_counters()` to start fresh for the next ledger.
    pub fn reset_counters(&self) {
        let mut inner = self.inner.lock();
        inner.hits = 0;
        inner.misses = 0;
    }
}


impl std::fmt::Debug for RandomEvictionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("RandomEvictionCache")
            .field("entry_count", &stats.entry_count)
            .field("size_bytes", &stats.size_bytes)
            .field("max_bytes", &stats.max_bytes)
            .field("hit_rate", &format!("{:.2}%", stats.hit_rate * 100.0))
            .field("active", &stats.active)
            .finish()
    }
}

// ============================================================================
// Cache Statistics
// ============================================================================

/// Statistics about cache performance.
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of entries currently in the cache.
    pub entry_count: usize,
    /// Current size in bytes.
    pub size_bytes: usize,
    /// Maximum size in bytes.
    pub max_bytes: usize,
    /// Maximum number of entries.
    pub max_entries: usize,
    /// Number of cache hits.
    pub hits: u64,
    /// Number of cache misses.
    pub misses: u64,
    /// Hit rate (hits / total requests).
    pub hit_rate: f64,
    /// Whether the cache is active.
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BucketEntry; // Use our BucketEntry, not the XDR one
    use stellar_xdr::curr::*;

    fn make_account_id(byte: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32])))
    }

    fn make_account_entry(byte: u8) -> BucketEntry {
        BucketEntry::Live(LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(byte),
                balance: 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        })
    }

    fn make_account_key(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(byte),
        })
    }

    fn make_trustline_key(account_byte: u8, asset_code: &[u8; 4]) -> LedgerKey {
        LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: make_account_id(account_byte),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*asset_code),
                issuer: make_account_id(0),
            }),
        })
    }

    fn make_trustline_entry(account_byte: u8, asset_code: &[u8; 4]) -> BucketEntry {
        BucketEntry::Live(LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: make_account_id(account_byte),
                asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*asset_code),
                    issuer: make_account_id(0),
                }),
                balance: 1000,
                limit: 10000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        })
    }

    #[test]
    fn test_cache_basic_operations() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        cache.activate();

        let key = make_account_key(1);
        let entry = make_account_entry(1);

        // Initially empty
        assert!(cache.get(&key).is_none());

        // Insert and retrieve
        cache.insert(key.clone(), entry);
        assert!(cache.get(&key).is_some());

        // Remove
        cache.remove(&key);
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn test_cache_not_active() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        // Don't activate the cache

        let key = make_account_key(1);
        let entry = make_account_entry(1);

        cache.insert(key.clone(), entry);
        assert!(cache.get(&key).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn test_is_cached_type_account_only() {
        // Account key should be cached
        assert!(RandomEvictionCache::is_cached_type(&make_account_key(1)));

        // Trustline key should NOT be cached (matching stellar-core)
        let trustline_key = make_trustline_key(1, b"USD\0");
        assert!(!RandomEvictionCache::is_cached_type(&trustline_key));

        // ClaimableBalance key should NOT be cached
        let cb_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([0; 32])),
        });
        assert!(!RandomEvictionCache::is_cached_type(&cb_key));

        // LiquidityPool key should NOT be cached
        let lp_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: PoolId(Hash([0; 32])),
        });
        assert!(!RandomEvictionCache::is_cached_type(&lp_key));

        // Offer key should NOT be cached
        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: make_account_id(1),
            offer_id: 1,
        });
        assert!(!RandomEvictionCache::is_cached_type(&offer_key));

        // Data key should NOT be cached
        let data_key = LedgerKey::Data(LedgerKeyData {
            account_id: make_account_id(1),
            data_name: String64::from(stellar_xdr::curr::StringM::default()),
        });
        assert!(!RandomEvictionCache::is_cached_type(&data_key));
    }

    #[test]
    fn test_cache_eviction() {
        // Create cache with small entry limit — power-of-2 evicts one per insert
        // when at capacity, so after 10 inserts with max=5 we should have exactly 5.
        let cache = RandomEvictionCache::with_limits(1_000_000, 5);
        cache.activate();

        for i in 0..10u8 {
            cache.insert(make_account_key(i), make_account_entry(i));
        }

        assert_eq!(cache.len(), 5);
    }

    #[test]
    fn test_eviction_favors_recently_accessed() {
        // Power-of-2-choices should preferentially evict less-recently-used entries.
        // Use a large enough cache that self-eviction (both random picks landing on
        // the same index) is negligible.
        let cache = RandomEvictionCache::with_limits(1_000_000, 100);
        cache.activate();

        let hot_key = make_account_key(0);
        cache.insert(hot_key.clone(), make_account_entry(0));

        for i in 1..150u8 {
            // Touch the hot entry to keep its access_count high
            cache.get(&hot_key);
            cache.insert(make_account_key(i), make_account_entry(i));
        }

        // The frequently-accessed entry should still be in the cache
        assert!(cache.get(&hot_key).is_some());
        assert_eq!(cache.len(), 100);
    }

    #[test]
    fn test_eviction_by_byte_limit() {
        // Each account entry is ~200+ bytes. A 500-byte cache should only hold 1-2.
        let cache = RandomEvictionCache::with_limits(500, 1_000_000);
        cache.activate();

        for i in 0..10u8 {
            cache.insert(make_account_key(i), make_account_entry(i));
        }

        // Should have evicted down to fit within byte budget
        assert!(cache.size_bytes() <= 500);
        assert!(cache.len() <= 2);
    }

    #[test]
    fn test_keys_vec_consistency_after_removes() {
        // Verify the keys vec stays consistent after interleaved inserts and removes.
        let cache = RandomEvictionCache::with_limits(1_000_000, 100);
        cache.activate();

        // Insert 20 entries
        for i in 0..20u8 {
            cache.insert(make_account_key(i), make_account_entry(i));
        }
        assert_eq!(cache.len(), 20);

        // Remove odd-numbered entries
        for i in (1..20u8).step_by(2) {
            cache.remove(&make_account_key(i));
        }
        assert_eq!(cache.len(), 10);

        // All even entries should still be retrievable
        for i in (0..20u8).step_by(2) {
            assert!(
                cache.get(&make_account_key(i)).is_some(),
                "entry {} should still be present",
                i
            );
        }

        // Insert more entries — should work without panic from inconsistent indices
        for i in 20..30u8 {
            cache.insert(make_account_key(i), make_account_entry(i));
        }
        assert_eq!(cache.len(), 20);

        // Verify all new entries are retrievable
        for i in 20..30u8 {
            assert!(cache.get(&make_account_key(i)).is_some());
        }
    }

    #[test]
    fn test_cache_stats() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        cache.activate();

        let key = make_account_key(1);
        let entry = make_account_entry(1);

        // Miss
        cache.get(&key);

        // Insert
        cache.insert(key.clone(), entry);

        // Hit
        cache.get(&key);

        let stats = cache.stats();
        assert_eq!(stats.entry_count, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_cache_clear() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        cache.activate();

        for i in 0..5u8 {
            cache.insert(make_account_key(i), make_account_entry(i));
        }

        assert_eq!(cache.len(), 5);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn test_cache_update_existing() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        cache.activate();

        let key = make_account_key(1);
        let entry1 = make_account_entry(1);
        let entry2 = make_account_entry(1);

        cache.insert(key.clone(), entry1);
        let size1 = cache.size_bytes();

        cache.insert(key.clone(), entry2);
        let size2 = cache.size_bytes();

        // Size should be similar (same entry type)
        assert!((size1 as isize - size2 as isize).abs() < 100);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_rejects_non_account_types() {
        let cache = RandomEvictionCache::with_limits(1_000_000, 1_000);
        cache.activate();

        // Trustline should not be cached
        let tl_key = make_trustline_key(1, b"USD\0");
        let tl_entry = make_trustline_entry(1, b"USD\0");
        cache.insert(tl_key.clone(), tl_entry);
        assert!(cache.is_empty(), "Trustline should not be cached");
        assert!(cache.get(&tl_key).is_none());

        // Account should be cached
        let acct_key = make_account_key(1);
        let acct_entry = make_account_entry(1);
        cache.insert(acct_key.clone(), acct_entry);
        assert_eq!(cache.len(), 1, "Only account should be cached");
        assert!(cache.get(&acct_key).is_some());
    }
}
