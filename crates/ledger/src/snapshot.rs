//! Point-in-time snapshots of ledger state.
//!
//! This module provides [`LedgerSnapshot`] and related types for capturing
//! and querying ledger state at specific points in time. Snapshots are
//! essential for:
//!
//! - **Concurrent reads during ledger close**: Transaction processing reads
//!   from a frozen snapshot while writes accumulate in the delta
//! - **Historical state queries**: Access past ledger states for analysis
//! - **Transaction validation**: Validate transactions against consistent state
//!
//! # Snapshot Hierarchy
//!
//! - [`LedgerSnapshot`]: The actual point-in-time state (header + cached entries)
//! - [`SnapshotHandle`]: Thread-safe wrapper with optional lookup functions
//! - [`SnapshotBuilder`]: Fluent API for constructing snapshots
//!
//! # Lazy Loading
//!
//! Snapshots can be configured with lookup functions that lazily fetch entries
//! not in the cache. This allows efficient snapshots that don't need to copy
//! the entire ledger state upfront.

use crate::{LedgerError, Result};
use henyey_common::Hash256;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use stellar_xdr::curr::{
    AccountEntry, AccountId, LedgerEntry, LedgerEntryData, LedgerHeader, LedgerKey, PoolId,
};

/// Lookup statistics for SnapshotHandle cache layers.
///
/// Tracks hits at each cache layer and fallback lookups. Shared across
/// clones of the same SnapshotHandle via `Arc`.
#[derive(Debug, Default)]
pub struct SnapshotLookupStats {
    /// Lookups served by the built-in snapshot cache (`inner.get_entry()`).
    pub snapshot_cache_hits: AtomicU64,
    /// Lookups served by the prefetch/read-through cache.
    pub prefetch_cache_hits: AtomicU64,
    /// Lookups dispatched to `lookup_fn` / `batch_lookup_fn` (not in either local cache).
    pub fallback_lookups: AtomicU64,
}

impl SnapshotLookupStats {
    /// Read all counters (snapshot_hits, prefetch_hits, fallback_lookups).
    pub fn read(&self) -> (u64, u64, u64) {
        (
            self.snapshot_cache_hits.load(Ordering::Relaxed),
            self.prefetch_cache_hits.load(Ordering::Relaxed),
            self.fallback_lookups.load(Ordering::Relaxed),
        )
    }
}

/// Statistics from a prefetch operation.
#[derive(Debug, Default)]
pub struct PrefetchStats {
    /// Number of keys that needed loading (not already cached).
    pub requested: usize,
    /// Number of entries actually loaded from the bucket list.
    pub loaded: usize,
}

/// A point-in-time snapshot of ledger state.
///
/// `LedgerSnapshot` provides a consistent, read-only view of the ledger
/// at a specific sequence number. The snapshot is immutable after creation
/// and can be safely shared across threads for concurrent reads.
///
/// # Cached vs. Full State
///
/// A snapshot contains a cache of entries, which may be a subset of the
/// full ledger state. Use [`SnapshotHandle`] with a lookup function to
/// enable lazy loading of entries not in the cache.
///
/// # Thread Safety
///
/// The snapshot itself is immutable after creation. For shared ownership
/// across threads, wrap in an [`Arc`] or use [`SnapshotHandle`].
#[derive(Debug)]
pub struct LedgerSnapshot {
    /// The ledger sequence number this snapshot represents.
    ledger_seq: u32,

    /// The complete ledger header at this sequence.
    header: LedgerHeader,

    /// SHA-256 hash of the XDR-encoded header.
    header_hash: Hash256,

    /// Cached entries keyed by LedgerKey directly.
    ///
    /// This may be a subset of the full ledger state. Entries not in
    /// this cache can be loaded via the lookup function in SnapshotHandle.
    entries: HashMap<LedgerKey, LedgerEntry>,
}

impl LedgerSnapshot {
    /// Create a new snapshot from a header and entries.
    pub fn new(
        header: LedgerHeader,
        header_hash: Hash256,
        entries: HashMap<LedgerKey, LedgerEntry>,
    ) -> Self {
        Self {
            ledger_seq: header.ledger_seq,
            header,
            header_hash,
            entries,
        }
    }

    /// Create an empty snapshot (for genesis or testing).
    pub fn empty(ledger_seq: u32) -> Self {
        Self {
            ledger_seq,
            header: LedgerHeader {
                ledger_version: 0,
                previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
                scp_value: stellar_xdr::curr::StellarValue {
                    tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    close_time: stellar_xdr::curr::TimePoint(0),
                    upgrades: stellar_xdr::curr::VecM::default(),
                    ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
                tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
                bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
                ledger_seq,
                total_coins: 0,
                fee_pool: 0,
                inflation_seq: 0,
                id_pool: 0,
                base_fee: 100,
                base_reserve: 5_000_000,
                max_tx_set_size: 1000,
                skip_list: std::array::from_fn(|_| stellar_xdr::curr::Hash([0u8; 32])),
                ext: stellar_xdr::curr::LedgerHeaderExt::V0,
            },
            header_hash: Hash256::ZERO,
            entries: HashMap::new(),
        }
    }

    /// Get the ledger sequence of this snapshot.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Get the ledger header.
    pub fn header(&self) -> &LedgerHeader {
        &self.header
    }

    /// Get the ledger header hash.
    pub fn header_hash(&self) -> &Hash256 {
        &self.header_hash
    }

    /// Get the protocol version.
    pub fn protocol_version(&self) -> u32 {
        self.header.ledger_version
    }

    /// Get the base fee.
    pub fn base_fee(&self) -> u32 {
        self.header.base_fee
    }

    /// Get the base reserve.
    pub fn base_reserve(&self) -> u32 {
        self.header.base_reserve
    }

    /// Get the bucket list hash.
    pub fn bucket_list_hash(&self) -> Hash256 {
        Hash256::from(self.header.bucket_list_hash.0)
    }

    /// Look up an entry by key.
    pub fn get_entry(&self, key: &LedgerKey) -> Option<&LedgerEntry> {
        self.entries.get(key)
    }

    /// Look up an account by ID.
    pub fn get_account(&self, account_id: &AccountId) -> Option<&AccountEntry> {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        if let Some(entry) = self.get_entry(&key) {
            if let LedgerEntryData::Account(ref account) = entry.data {
                return Some(account);
            }
        }
        None
    }

    /// Check if an entry exists.
    pub fn contains(&self, key: &LedgerKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Set the ID pool value in the header.
    ///
    /// This is used during replay to set the correct starting ID pool
    /// from the previous ledger, so that new offers get the correct IDs.
    pub fn set_id_pool(&mut self, id_pool: u64) {
        self.header.id_pool = id_pool;
    }

    /// Get the number of cached entries.
    pub fn num_entries(&self) -> usize {
        self.entries.len()
    }

    /// Iterate over all cached entries.
    pub fn entries(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.values()
    }
}

impl Clone for LedgerSnapshot {
    fn clone(&self) -> Self {
        Self {
            ledger_seq: self.ledger_seq,
            header: self.header.clone(),
            header_hash: self.header_hash,
            entries: self.entries.clone(),
        }
    }
}

/// Callback type for lazy entry lookup (e.g., from bucket list).
pub type EntryLookupFn = Arc<dyn Fn(&LedgerKey) -> Result<Option<LedgerEntry>> + Send + Sync>;

/// Callback type for full entry enumeration (e.g., bucket list scan).
pub type EntriesLookupFn = Arc<dyn Fn() -> Result<Vec<LedgerEntry>> + Send + Sync>;

/// Batch entry lookup function for loading multiple entries in a single bucket list pass.
pub type BatchEntryLookupFn = Arc<dyn Fn(&[LedgerKey]) -> Result<Vec<LedgerEntry>> + Send + Sync>;

/// Lookup function for offers by (account, asset) pair.
///
/// Returns all offers owned by the given account that buy or sell the given asset.
pub type OffersByAccountAssetFn =
    Arc<dyn Fn(&AccountId, &stellar_xdr::curr::Asset) -> Result<Vec<LedgerEntry>> + Send + Sync>;

/// Lookup function for pool share trustlines by account.
///
/// Returns the pool IDs for all pool share trustlines owned by the given account.
/// Used to ensure pool share trustlines are loaded before authorization revocation.
pub type PoolShareTrustlinesByAccountFn =
    Arc<dyn Fn(&AccountId) -> Result<Vec<PoolId>> + Send + Sync>;

/// Thread-safe handle to a ledger snapshot with optional lazy loading.
///
/// `SnapshotHandle` wraps a [`LedgerSnapshot`] in an `Arc` for efficient
/// sharing across threads, and optionally provides lookup functions for
/// entries not in the snapshot's cache.
///
/// # Lookup Functions
///
/// Two optional lookup functions can be configured:
///
/// - **Entry lookup**: Fetches individual entries (e.g., from bucket list)
/// - **Entries scan**: Returns all live entries (e.g., for full state analysis)
///
/// # Example
///
/// ```ignore
/// let handle = SnapshotHandle::with_lookup(snapshot, bucket_list_lookup);
///
/// // Entry lookup falls through to bucket list if not cached
/// let entry = handle.get_entry(&key)?;
/// ```
#[derive(Clone)]
pub struct SnapshotHandle {
    /// The underlying snapshot (shared via Arc).
    inner: Arc<LedgerSnapshot>,
    /// Optional fallback for entry lookups not in cache.
    lookup_fn: Option<EntryLookupFn>,
    /// Optional enumeration of all live entries.
    entries_fn: Option<EntriesLookupFn>,
    /// Optional batch lookup for multiple entries in a single pass.
    batch_lookup_fn: Option<BatchEntryLookupFn>,
    /// Optional index-based lookup for offers by (account, asset).
    offers_by_account_asset_fn: Option<OffersByAccountAssetFn>,
    /// Optional index-based lookup for pool share trustline pool IDs by account.
    pool_share_tls_by_account_fn: Option<PoolShareTrustlinesByAccountFn>,
    /// Cache populated by prefetch, checked before falling through to lookup_fn.
    /// Uses Arc<RwLock> so clones of SnapshotHandle share the same cache.
    prefetch_cache: Arc<parking_lot::RwLock<HashMap<LedgerKey, LedgerEntry>>>,
    /// Lookup statistics shared across clones.
    stats: Arc<SnapshotLookupStats>,
    /// True after `release_lookups()` has been called. Used to distinguish
    /// "lookups were never configured" from "lookups were dropped on purpose"
    /// so post-release fallback paths return errors instead of silent None.
    lookups_released: bool,
}

impl SnapshotHandle {
    /// Create a new handle from a snapshot.
    pub fn new(snapshot: LedgerSnapshot) -> Self {
        Self {
            inner: Arc::new(snapshot),
            lookup_fn: None,
            entries_fn: None,
            batch_lookup_fn: None,
            offers_by_account_asset_fn: None,
            pool_share_tls_by_account_fn: None,
            prefetch_cache: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            stats: Arc::new(SnapshotLookupStats::default()),
            lookups_released: false,
        }
    }

    /// Create a new handle with a lookup function for entries not in cache.
    pub fn with_lookup(snapshot: LedgerSnapshot, lookup_fn: EntryLookupFn) -> Self {
        let mut handle = Self::new(snapshot);
        handle.lookup_fn = Some(lookup_fn);
        handle
    }

    /// Create a new handle with lookup functions for entries and full scans.
    pub fn with_lookups_and_entries(
        snapshot: LedgerSnapshot,
        lookup_fn: EntryLookupFn,
        entries_fn: EntriesLookupFn,
    ) -> Self {
        let mut handle = Self::new(snapshot);
        handle.lookup_fn = Some(lookup_fn);
        handle.entries_fn = Some(entries_fn);
        handle
    }

    /// Set the lookup function.
    pub fn set_lookup(&mut self, lookup_fn: EntryLookupFn) {
        self.lookup_fn = Some(lookup_fn);
    }

    /// Set the full-entry lookup function.
    pub fn set_entries_lookup(&mut self, entries_fn: EntriesLookupFn) {
        self.entries_fn = Some(entries_fn);
    }

    /// Set the batch entry lookup function.
    pub fn set_batch_lookup(&mut self, batch_fn: BatchEntryLookupFn) {
        self.batch_lookup_fn = Some(batch_fn);
    }

    /// Set the offers-by-(account, asset) lookup function.
    pub fn set_offers_by_account_asset(&mut self, f: OffersByAccountAssetFn) {
        self.offers_by_account_asset_fn = Some(f);
    }

    /// Set the pool-share-trustlines-by-account lookup function.
    pub fn set_pool_share_tls_by_account(&mut self, f: PoolShareTrustlinesByAccountFn) {
        self.pool_share_tls_by_account_fn = Some(f);
    }

    /// Drop lookup closures to release captured resources (Arc references to
    /// soroban state snapshots, bucket list snapshots, etc.).
    ///
    /// After calling this, `get_entry()` and `load_entries()` will only check
    /// the snapshot cache and prefetch cache — any fallback to lookup_fn /
    /// batch_lookup_fn for uncached keys returns an error.
    ///
    /// Stats and prefetch cache are preserved for end-of-close reporting.
    pub fn release_lookups(&mut self) {
        self.lookup_fn = None;
        self.batch_lookup_fn = None;
        self.lookups_released = true;
    }

    /// Look up all pool IDs for pool share trustlines owned by `account_id`.
    ///
    /// Returns an empty vec if no index is available.
    // SECURITY: pool-share index always present in production snapshot; populated during initialization
    pub fn pool_share_tls_by_account(&self, account_id: &AccountId) -> Result<Vec<PoolId>> {
        if let Some(ref f) = self.pool_share_tls_by_account_fn {
            return f(account_id);
        }
        Ok(Vec::new())
    }

    /// Look up all offers owned by `account_id` that buy or sell `asset`.
    ///
    /// Uses the index-based lookup if available, otherwise falls back to
    /// `all_entries()` with a linear scan.
    pub fn offers_by_account_and_asset(
        &self,
        account_id: &AccountId,
        asset: &stellar_xdr::curr::Asset,
    ) -> Result<Vec<LedgerEntry>> {
        if let Some(ref f) = self.offers_by_account_asset_fn {
            return f(account_id, asset);
        }
        // Fallback: linear scan over all entries
        let entries = self.all_entries()?;
        Ok(entries
            .into_iter()
            .filter(|entry| {
                if let LedgerEntryData::Offer(ref offer) = entry.data {
                    offer.seller_id == *account_id
                        && (offer.buying == *asset || offer.selling == *asset)
                } else {
                    false
                }
            })
            .collect())
    }

    /// Load multiple entries by their keys.
    ///
    /// Checks the snapshot cache first, then the prefetch cache, then uses
    /// the batch lookup function (if available) for remaining keys.
    /// Falls back to individual lookups.
    pub fn load_entries(&self, keys: &[LedgerKey]) -> Result<Vec<LedgerEntry>> {
        // Check snapshot cache first, then prefetch cache, collect remaining keys
        let mut result = Vec::new();
        let mut remaining = Vec::new();
        let prefetch = self.prefetch_cache.read();
        for key in keys {
            if let Some(entry) = self.inner.get_entry(key) {
                self.stats
                    .snapshot_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                result.push(entry.clone());
            } else if let Some(entry) = prefetch.get(key) {
                self.stats
                    .prefetch_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                result.push(entry.clone());
            } else {
                remaining.push(key.clone());
            }
        }
        drop(prefetch);

        if remaining.is_empty() {
            return Ok(result);
        }

        // Count all remaining keys as fallback lookups (regardless of result)
        self.stats
            .fallback_lookups
            .fetch_add(remaining.len() as u64, Ordering::Relaxed);

        // Use batch lookup if available; cache all loaded entries for future callers
        let loaded = if let Some(ref batch_fn) = self.batch_lookup_fn {
            batch_fn(&remaining)?
        } else if let Some(ref lookup_fn) = self.lookup_fn {
            let mut loaded = Vec::new();
            for key in &remaining {
                if let Some(entry) = lookup_fn(key)? {
                    loaded.push(entry);
                }
            }
            loaded
        } else if self.lookups_released {
            return Err(crate::LedgerError::Internal(
                "snapshot load_entries attempted after release_lookups()".into(),
            ));
        } else {
            Vec::new()
        };

        if !loaded.is_empty() {
            let mut cache = self.prefetch_cache.write();
            for entry in &loaded {
                let key = henyey_common::entry_to_key(entry);
                cache.insert(key, entry.clone());
            }
        }
        result.extend(loaded);

        Ok(result)
    }

    /// Get the underlying snapshot.
    pub fn snapshot(&self) -> &LedgerSnapshot {
        &self.inner
    }

    /// Return all live entries when available, falling back to cached entries.
    pub fn all_entries(&self) -> Result<Vec<LedgerEntry>> {
        if let Some(entries_fn) = &self.entries_fn {
            return entries_fn();
        }
        Ok(self.inner.entries.values().cloned().collect())
    }

    /// Get the ledger sequence.
    pub fn ledger_seq(&self) -> u32 {
        self.inner.ledger_seq
    }

    /// Get the header.
    pub fn header(&self) -> &LedgerHeader {
        &self.inner.header
    }

    /// Get the header hash.
    pub fn header_hash(&self) -> Hash256 {
        self.inner.header_hash
    }

    /// Look up an entry.
    ///
    /// First checks the snapshot cache, then the prefetch cache, then falls
    /// back to the lookup function if one is configured (e.g., for bucket
    /// list lookups).
    pub fn get_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        // 1. Check snapshot's built-in cache
        if let Some(entry) = self.inner.get_entry(key) {
            self.stats
                .snapshot_cache_hits
                .fetch_add(1, Ordering::Relaxed);
            return Ok(Some(entry.clone()));
        }

        // 2. Check prefetch cache
        {
            if let Some(entry) = self.prefetch_cache.read().get(key) {
                self.stats
                    .prefetch_cache_hits
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(Some(entry.clone()));
            }
        }

        // 3. Fall back to lookup function if available; cache the result for future callers
        if let Some(ref lookup_fn) = self.lookup_fn {
            self.stats.fallback_lookups.fetch_add(1, Ordering::Relaxed);
            let result = lookup_fn(key)?;
            if let Some(ref entry) = result {
                self.prefetch_cache
                    .write()
                    .insert(key.clone(), entry.clone());
            }
            return Ok(result);
        }

        // 4. No lookup function — uncached key with no way to resolve
        if self.lookups_released {
            return Err(crate::LedgerError::Internal(
                "snapshot lookup attempted after release_lookups()".into(),
            ));
        }
        self.stats.fallback_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    /// Look up an account.
    pub fn get_account(&self, account_id: &AccountId) -> Result<Option<AccountEntry>> {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        if let Some(entry) = self.get_entry(&key)? {
            if let LedgerEntryData::Account(account) = entry.data {
                return Ok(Some(account));
            }
        }
        Ok(None)
    }

    /// Bulk-load keys into the prefetch cache.
    ///
    /// Uses batch_lookup_fn for a single bucket list traversal.
    /// Keys already in the snapshot cache or prefetch cache are skipped.
    pub fn prefetch(&self, keys: &[LedgerKey]) -> Result<PrefetchStats> {
        let mut needed = Vec::new();
        let cache = self.prefetch_cache.read();

        for key in keys {
            if self.inner.get_entry(key).is_some() || cache.contains_key(key) {
                continue;
            }
            needed.push(key.clone());
        }
        drop(cache); // Release read lock before write

        if needed.is_empty() {
            return Ok(PrefetchStats::default());
        }

        // Batch load from bucket list
        let entries = if let Some(ref batch_fn) = self.batch_lookup_fn {
            batch_fn(&needed)?
        } else if let Some(ref lookup_fn) = self.lookup_fn {
            let mut loaded = Vec::new();
            for k in &needed {
                if let Some(entry) = lookup_fn(k)? {
                    loaded.push(entry);
                }
            }
            loaded
        } else if self.lookups_released {
            return Err(crate::LedgerError::Internal(
                "snapshot prefetch attempted after release_lookups()".into(),
            ));
        } else {
            return Ok(PrefetchStats {
                requested: needed.len(),
                loaded: 0,
            });
        };

        let loaded = entries.len();
        let mut cache = self.prefetch_cache.write();
        for entry in entries {
            let key = henyey_common::entry_to_key(&entry);
            cache.insert(key, entry);
        }

        Ok(PrefetchStats {
            requested: needed.len(),
            loaded,
        })
    }

    /// Return the shared lookup statistics.
    pub fn lookup_stats(&self) -> &SnapshotLookupStats {
        &self.stats
    }

    /// Return the number of entries in the prefetch/read-through cache.
    pub fn prefetch_cache_len(&self) -> usize {
        self.prefetch_cache.read().len()
    }
}

/// Fluent builder for constructing [`LedgerSnapshot`] instances.
///
/// Use this builder when you need to construct a snapshot programmatically
/// with specific entries preloaded.
///
/// # Example
///
/// ```ignore
/// let snapshot = SnapshotBuilder::new(ledger_seq)
///     .with_header(header, header_hash)
///     .add_entry(key, entry)?
///     .build()?;
/// ```
pub struct SnapshotBuilder {
    /// Target ledger sequence.
    ledger_seq: u32,
    /// Optional header (required for build, optional for build_with_default_header).
    header: Option<LedgerHeader>,
    /// Hash of the header.
    header_hash: Hash256,
    /// Preloaded entries.
    entries: HashMap<LedgerKey, LedgerEntry>,
}

impl SnapshotBuilder {
    /// Create a new builder for a given ledger sequence.
    pub fn new(ledger_seq: u32) -> Self {
        Self {
            ledger_seq,
            header: None,
            header_hash: Hash256::ZERO,
            entries: HashMap::new(),
        }
    }

    /// Set the ledger header.
    pub fn with_header(mut self, header: LedgerHeader, hash: Hash256) -> Self {
        self.header = Some(header);
        self.header_hash = hash;
        self
    }

    /// Add an entry to the snapshot.
    pub fn add_entry(mut self, key: LedgerKey, entry: LedgerEntry) -> Self {
        self.entries.insert(key, entry);
        self
    }

    /// Add multiple entries.
    pub fn add_entries(
        mut self,
        entries: impl IntoIterator<Item = (LedgerKey, LedgerEntry)>,
    ) -> Self {
        for (key, entry) in entries {
            self.entries.insert(key, entry);
        }
        self
    }

    /// Build the snapshot.
    pub fn build(self) -> Result<LedgerSnapshot> {
        let header = self
            .header
            .ok_or_else(|| LedgerError::Snapshot("header not set".to_string()))?;

        Ok(LedgerSnapshot {
            ledger_seq: self.ledger_seq,
            header,
            header_hash: self.header_hash,
            entries: self.entries,
        })
    }

    /// Build the snapshot with a default header (for testing).
    pub fn build_with_default_header(self) -> LedgerSnapshot {
        let header = self.header.unwrap_or_else(|| LedgerHeader {
            ledger_version: 20,
            previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
            scp_value: stellar_xdr::curr::StellarValue {
                tx_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                close_time: stellar_xdr::curr::TimePoint(0),
                upgrades: stellar_xdr::curr::VecM::default(),
                ext: stellar_xdr::curr::StellarValueExt::Basic,
            },
            tx_set_result_hash: stellar_xdr::curr::Hash([0u8; 32]),
            bucket_list_hash: stellar_xdr::curr::Hash([0u8; 32]),
            ledger_seq: self.ledger_seq,
            total_coins: 100_000_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 1000,
            skip_list: std::array::from_fn(|_| stellar_xdr::curr::Hash([0u8; 32])),
            ext: stellar_xdr::curr::LedgerHeaderExt::V0,
        });

        LedgerSnapshot {
            ledger_seq: self.ledger_seq,
            header,
            header_hash: self.header_hash,
            entries: self.entries,
        }
    }
}

impl crate::EntryReader for SnapshotHandle {
    fn get_entry(&self, key: &LedgerKey) -> crate::Result<Option<LedgerEntry>> {
        SnapshotHandle::get_entry(self, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, LedgerEntryExt, PublicKey, SequenceNumber, Thresholds,
        Uint256,
    };

    fn create_test_account(seed: u8) -> (LedgerKey, LedgerEntry) {
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = seed;

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key_bytes)));

        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: stellar_xdr::curr::VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        (key, entry)
    }

    #[test]
    fn test_snapshot_builder() {
        let (key, entry) = create_test_account(1);

        let snapshot = SnapshotBuilder::new(10)
            .add_entry(key.clone(), entry.clone())
            .build_with_default_header();

        assert_eq!(snapshot.ledger_seq(), 10);
        assert!(snapshot.get_entry(&key).is_some());
    }

    #[test]
    fn test_snapshot_get_account() {
        let (key, entry) = create_test_account(1);

        let account_id = if let LedgerKey::Account(ref ak) = key {
            ak.account_id.clone()
        } else {
            panic!("Expected account key");
        };

        let snapshot = SnapshotBuilder::new(1)
            .add_entry(key, entry)
            .build_with_default_header();

        let account = snapshot.get_account(&account_id);
        assert!(account.is_some());
        assert_eq!(account.unwrap().balance, 1000000000);
    }

    /// Parity: LedgerTxnTests.cpp:1616 "LedgerTxn loadWithoutRecord"
    /// Reading from a snapshot should not produce any side effects (no delta impact).
    /// Snapshots are immutable point-in-time views.
    #[test]
    fn test_snapshot_read_is_side_effect_free() {
        let (key1, entry1) = create_test_account(1);
        let (key2, entry2) = create_test_account(2);

        let snapshot = SnapshotBuilder::new(5)
            .add_entry(key1.clone(), entry1.clone())
            .add_entry(key2.clone(), entry2.clone())
            .build_with_default_header();

        // Read entries multiple times
        for _ in 0..3 {
            assert!(snapshot.get_entry(&key1).is_some());
            assert!(snapshot.get_entry(&key2).is_some());
        }

        // Reading a non-existent entry is fine
        let (missing_key, _) = create_test_account(99);
        assert!(snapshot.get_entry(&missing_key).is_none());

        // Snapshot state hasn't changed: sequence, header, entries all same
        assert_eq!(snapshot.ledger_seq(), 5);
        let e1_again = snapshot.get_entry(&key1).unwrap();
        assert_eq!(e1_again.data, entry1.data);
    }

    /// Parity: LedgerTxnTests.cpp:1509 "when key does not exist"
    /// Loading an entry that was never added to the snapshot returns None.
    #[test]
    fn test_snapshot_entry_not_found() {
        let (key1, entry1) = create_test_account(1);
        let (missing_key, _) = create_test_account(99);

        let snapshot = SnapshotBuilder::new(5)
            .add_entry(key1.clone(), entry1)
            .build_with_default_header();

        // Existing entry: found
        assert!(snapshot.get_entry(&key1).is_some());

        // Missing entry: returns None (not error)
        assert!(snapshot.get_entry(&missing_key).is_none());

        // Missing account: returns None
        if let LedgerKey::Account(ref ak) = missing_key {
            assert!(snapshot.get_account(&ak.account_id).is_none());
        }
    }

    /// Parity: LedgerTxnTests.cpp:1562 "when key exists in grandparent, erased in parent"
    /// If an entry is removed from the snapshot (e.g., during rebuild), it cannot be loaded.
    /// This tests that snapshot entries are independent - removing one doesn't affect others.
    #[test]
    fn test_snapshot_selective_entries() {
        let (key1, entry1) = create_test_account(1);
        let (key2, entry2) = create_test_account(2);
        let (key3, _entry3) = create_test_account(3);

        // Build snapshot with only key1 and key2 (key3 was "erased")
        let snapshot = SnapshotBuilder::new(5)
            .add_entry(key1.clone(), entry1)
            .add_entry(key2.clone(), entry2)
            .build_with_default_header();

        // key1 and key2 are found
        assert!(snapshot.get_entry(&key1).is_some());
        assert!(snapshot.get_entry(&key2).is_some());

        // key3 was never added (simulating deletion) - not found
        assert!(snapshot.get_entry(&key3).is_none());
    }

    /// Snapshot provides an immutable header view.
    #[test]
    fn test_snapshot_header_immutability() {
        let snapshot = SnapshotBuilder::new(42).build_with_default_header();

        let h1 = snapshot.header().clone();
        let h2 = snapshot.header().clone();

        // Header should be identical on every read
        assert_eq!(h1.ledger_seq, h2.ledger_seq);
        assert_eq!(h1.ledger_seq, 42);
    }

    /// Verify that `load_entries()` caches loaded entries so a subsequent `get_entry()`
    /// for the same key does NOT invoke the `lookup_fn` again.
    #[test]
    fn test_load_entries_caches_results() {
        let (key, entry) = create_test_account(7);
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        let entry_clone = entry.clone();
        let count = call_count.clone();
        handle.set_lookup(Arc::new(move |_k| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(entry_clone.clone()))
        }));

        // load_entries fetches from lookup_fn (count goes to 1)
        let loaded = handle.load_entries(std::slice::from_ref(&key)).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // get_entry for the same key must be served from prefetch_cache, not lookup_fn
        let result = handle.get_entry(&key).unwrap();
        assert!(result.is_some());
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "lookup_fn invoked again after load_entries cached the result"
        );
    }

    /// Verify that `get_entry()` caches bucket list results so subsequent lookups
    /// do NOT re-invoke the `lookup_fn`.
    #[test]
    fn test_get_entry_caches_lookup_result() {
        let (key, entry) = create_test_account(42);
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        let entry_clone = entry.clone();
        let count = call_count.clone();
        handle.set_lookup(Arc::new(move |_k| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(entry_clone.clone()))
        }));

        // First call: hits lookup_fn
        let result1 = handle.get_entry(&key).unwrap();
        assert!(result1.is_some());
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second call: served from prefetch_cache, lookup_fn not called again
        let result2 = handle.get_entry(&key).unwrap();
        assert!(result2.is_some());
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "lookup_fn invoked again after cache-through"
        );
    }

    /// Regression test for catchup replay id_pool fix.
    ///
    /// During catchup replay, the executor needs the previous ledger's id_pool
    /// to correctly assign offer IDs when transactions create new offers.
    /// Without this fix, LedgerSnapshot::empty() would always have id_pool=0,
    /// causing new offers to get IDs starting from 1 instead of the correct
    /// sequential value from the checkpoint.
    #[test]
    fn test_set_id_pool_for_replay() {
        // Create an empty snapshot (as used in replay)
        let mut snapshot = LedgerSnapshot::empty(100);

        // Verify default id_pool is 0
        assert_eq!(snapshot.header().id_pool, 0);

        // Set id_pool to a value from a checkpoint (e.g., 20680)
        let checkpoint_id_pool = 20680;
        snapshot.set_id_pool(checkpoint_id_pool);

        // Verify id_pool was updated correctly
        assert_eq!(snapshot.header().id_pool, checkpoint_id_pool);

        // This ensures that when the executor creates new offers during replay,
        // they will get IDs starting from 20681, 20682, etc. instead of 1, 2, etc.
    }

    // ------------------------------------------------------------------
    // SnapshotLookupStats tests
    // ------------------------------------------------------------------

    #[test]
    fn test_get_entry_stats_snapshot_cache_hit() {
        let (key, entry) = create_test_account(1);
        let snapshot = SnapshotBuilder::new(1)
            .add_entry(key.clone(), entry)
            .build_with_default_header();
        let handle = SnapshotHandle::new(snapshot);

        let result = handle.get_entry(&key).unwrap();
        assert!(result.is_some());

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 1);
        assert_eq!(prefetch_hits, 0);
        assert_eq!(fallback, 0);
    }

    #[test]
    fn test_get_entry_stats_prefetch_cache_hit() {
        let (key, entry) = create_test_account(1);
        let snapshot = LedgerSnapshot::empty(1);
        let handle = SnapshotHandle::new(snapshot);

        // Manually populate the prefetch cache
        handle
            .prefetch_cache
            .write()
            .insert(key.clone(), entry.clone());

        let result = handle.get_entry(&key).unwrap();
        assert!(result.is_some());

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 0);
        assert_eq!(prefetch_hits, 1);
        assert_eq!(fallback, 0);
    }

    #[test]
    fn test_get_entry_stats_fallback_lookup() {
        let (key, entry) = create_test_account(1);
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        let entry_clone = entry.clone();
        handle.set_lookup(Arc::new(move |_k| Ok(Some(entry_clone.clone()))));

        let result = handle.get_entry(&key).unwrap();
        assert!(result.is_some());

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 0);
        assert_eq!(prefetch_hits, 0);
        assert_eq!(fallback, 1);
    }

    #[test]
    fn test_get_entry_stats_no_lookup_fn() {
        let (key, _) = create_test_account(1);
        let snapshot = LedgerSnapshot::empty(1);
        let handle = SnapshotHandle::new(snapshot);

        // No lookup_fn configured — should still count as fallback
        let result = handle.get_entry(&key).unwrap();
        assert!(result.is_none());

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 0);
        assert_eq!(prefetch_hits, 0);
        assert_eq!(fallback, 1);
    }

    #[test]
    fn test_get_entry_stats_read_through_becomes_prefetch_hit() {
        let (key, entry) = create_test_account(1);
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        let entry_clone = entry.clone();
        handle.set_lookup(Arc::new(move |_k| Ok(Some(entry_clone.clone()))));

        // First call: fallback lookup (caches result via read-through)
        handle.get_entry(&key).unwrap();
        let (_, _, fallback1) = handle.lookup_stats().read();
        assert_eq!(fallback1, 1);

        // Second call: served from prefetch cache
        handle.get_entry(&key).unwrap();
        let (snap_hits, prefetch_hits, fallback2) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 0);
        assert_eq!(prefetch_hits, 1);
        assert_eq!(fallback2, 1); // no new fallback
    }

    #[test]
    fn test_cloned_handles_share_stats() {
        let (key1, entry1) = create_test_account(1);
        let (key2, entry2) = create_test_account(2);

        let snapshot = SnapshotBuilder::new(1)
            .add_entry(key1.clone(), entry1)
            .build_with_default_header();
        let handle = SnapshotHandle::new(snapshot);
        let clone = handle.clone();

        // Lookup via original
        handle.get_entry(&key1).unwrap();
        // Populate prefetch cache on clone
        clone
            .prefetch_cache
            .write()
            .insert(key2.clone(), entry2.clone());
        clone.get_entry(&key2).unwrap();

        // Both share the same stats
        let (snap_hits, prefetch_hits, _) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 1);
        assert_eq!(prefetch_hits, 1);

        // Same values from clone's perspective
        let (snap_hits2, prefetch_hits2, _) = clone.lookup_stats().read();
        assert_eq!(snap_hits2, 1);
        assert_eq!(prefetch_hits2, 1);
    }

    #[test]
    fn test_load_entries_stats_batch_fallback_count() {
        let (key1, entry1) = create_test_account(1);
        let (key2, entry2) = create_test_account(2);
        let (key3, _) = create_test_account(3); // will not be found

        let snapshot = SnapshotBuilder::new(1)
            .add_entry(key1.clone(), entry1)
            .build_with_default_header();
        let mut handle = SnapshotHandle::new(snapshot);

        // Batch lookup returns only key2's entry (key3 not found)
        let entry2_clone = entry2.clone();
        handle.set_batch_lookup(Arc::new(move |_keys| Ok(vec![entry2_clone.clone()])));

        let loaded = handle
            .load_entries(&[key1.clone(), key2.clone(), key3.clone()])
            .unwrap();
        // key1 from snapshot, key2 from batch, key3 not found
        assert_eq!(loaded.len(), 2);

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 1); // key1 from snapshot
        assert_eq!(prefetch_hits, 0);
        assert_eq!(fallback, 2); // key2 + key3 both went to batch (remaining.len() = 2)
    }

    #[test]
    fn test_load_entries_stats_fallback_none_returns() {
        let (key1, _) = create_test_account(1);
        let (key2, _) = create_test_account(2);

        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        // Lookup always returns None (simulates OFFER keys skipped by bucket list)
        handle.set_lookup(Arc::new(|_k| Ok(None)));

        let loaded = handle.load_entries(&[key1, key2]).unwrap();
        assert_eq!(loaded.len(), 0);

        let (snap_hits, prefetch_hits, fallback) = handle.lookup_stats().read();
        assert_eq!(snap_hits, 0);
        assert_eq!(prefetch_hits, 0);
        assert_eq!(fallback, 2); // both keys counted despite returning None
    }

    #[test]
    fn test_prefetch_cache_len() {
        let (key1, entry1) = create_test_account(1);
        let (key2, entry2) = create_test_account(2);

        let snapshot = LedgerSnapshot::empty(1);
        let handle = SnapshotHandle::new(snapshot);

        assert_eq!(handle.prefetch_cache_len(), 0);

        handle.prefetch_cache.write().insert(key1, entry1);
        assert_eq!(handle.prefetch_cache_len(), 1);

        handle.prefetch_cache.write().insert(key2, entry2);
        assert_eq!(handle.prefetch_cache_len(), 2);
    }

    #[test]
    fn test_release_lookups_drops_arc_references() {
        let shared = Arc::new(42u64);
        let shared_clone = shared.clone();
        assert_eq!(Arc::strong_count(&shared), 2);

        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);
        let s1 = shared.clone();
        handle.set_lookup(Arc::new(move |_k| {
            let _ = &s1;
            Ok(None)
        }));
        let s2 = shared.clone();
        handle.set_batch_lookup(Arc::new(move |_k| {
            let _ = &s2;
            Ok(vec![])
        }));
        assert_eq!(Arc::strong_count(&shared), 4);

        handle.release_lookups();
        // Only our original + shared_clone should remain
        assert_eq!(Arc::strong_count(&shared), 2);
        assert!(handle.lookups_released);
        drop(shared_clone);
    }

    #[test]
    fn test_post_release_get_entry_returns_error_for_uncached() {
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);
        handle.set_lookup(Arc::new(|_k| Ok(None)));
        handle.release_lookups();

        let (key, _) = create_test_account(1);
        let result = handle.get_entry(&key);
        assert!(
            result.is_err(),
            "uncached lookup after release should error"
        );
    }

    #[test]
    fn test_post_release_get_entry_succeeds_for_cached() {
        let (key, entry) = create_test_account(1);
        let snapshot = SnapshotBuilder::new(1)
            .add_entry(key.clone(), entry.clone())
            .build_with_default_header();
        let handle_with_cache = SnapshotHandle::new(snapshot);

        // Also test prefetch cache
        let snapshot2 = LedgerSnapshot::empty(1);
        let mut handle_prefetch = SnapshotHandle::new(snapshot2);
        handle_prefetch
            .prefetch_cache
            .write()
            .insert(key.clone(), entry);
        handle_prefetch.set_lookup(Arc::new(|_k| Ok(None)));
        handle_prefetch.release_lookups();

        // Snapshot cache hit
        let result = handle_with_cache.get_entry(&key).unwrap();
        assert!(result.is_some());

        // Prefetch cache hit
        let result2 = handle_prefetch.get_entry(&key).unwrap();
        assert!(result2.is_some());
    }

    #[test]
    fn test_post_release_prefetch_returns_error() {
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);
        handle.set_lookup(Arc::new(|_k| Ok(None)));
        handle.release_lookups();

        let (key, _) = create_test_account(1);
        let result = handle.prefetch(&[key]);
        assert!(result.is_err(), "prefetch after release should error");
    }

    #[test]
    fn test_post_release_load_entries_returns_error() {
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);
        handle.set_lookup(Arc::new(|_k| Ok(None)));
        handle.release_lookups();

        let (key, _) = create_test_account(1);
        let result = handle.load_entries(&[key]);
        assert!(result.is_err(), "load_entries after release should error");
    }

    #[test]
    fn test_lifecycle_snapshot_release_then_mutate() {
        // Simulates the real commit path: snapshot → executor loaders → release → mutation
        let shared_data = Arc::new(std::collections::HashMap::<u32, u32>::new());
        assert_eq!(Arc::strong_count(&shared_data), 1);

        // Simulate create_snapshot capturing the data in closures
        let snapshot = LedgerSnapshot::empty(1);
        let mut handle = SnapshotHandle::new(snapshot);

        let data_for_lookup = shared_data.clone();
        handle.set_lookup(Arc::new(move |_k| {
            let _ = &data_for_lookup;
            Ok(None)
        }));
        let data_for_batch = shared_data.clone();
        handle.set_batch_lookup(Arc::new(move |_k| {
            let _ = &data_for_batch;
            Ok(vec![])
        }));
        assert_eq!(Arc::strong_count(&shared_data), 3);

        // Simulate executor loaders (clone of handle)
        let executor_handle = handle.clone();
        // lookup/batch closures are Arc-shared, so strong count stays at 3
        assert_eq!(Arc::strong_count(&shared_data), 3);

        // Simulate clearing executor loaders (Part 0)
        drop(executor_handle);

        // Simulate release_lookups on ltx.snapshot (Part 2)
        handle.release_lookups();
        assert_eq!(
            Arc::strong_count(&shared_data),
            1,
            "after clearing executor and releasing lookups, refcount should be 1"
        );
    }
}
