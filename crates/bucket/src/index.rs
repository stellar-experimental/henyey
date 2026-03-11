//! Advanced bucket indexing for efficient lookups.
//!
//! This module provides a hybrid indexing system that matches the stellar-core
//! `LiveBucketIndex` pattern. It supports both in-memory indexes for small buckets
//! and disk-based page indexes for large buckets.
//!
//! # Index Types
//!
//! - [`InMemoryIndex`]: Stores all entries in memory for small buckets
//! - [`DiskIndex`]: Page-based range index for large buckets (memory efficient)
//! - [`LiveBucketIndex`]: Facade that selects the appropriate index type
//!
//! # Features
//!
//! - **Range queries**: Efficiently find entries within key ranges
//! - **Type ranges**: Track contiguous ranges of entry types for faster type scans
//! - **Asset-to-PoolID mapping**: Maps assets to liquidity pool IDs for pool queries
//! - **Entry counters**: Track counts by entry type and durability

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    Asset, ContractDataDurability, LedgerEntry, LedgerEntryData, LedgerEntryType, LedgerKey,
    Limits, PoolId, TrustLineAsset, WriteXdr,
};

use crate::entry::{ledger_entry_data_type, ledger_key_type};

use henyey_common::BucketListDbConfig;

use crate::bloom_filter::{BucketBloomFilter, HashSeed};
use crate::entry::{compare_keys, BucketEntry, BucketEntryExt};

/// Default page size for disk index in bytes.
///
/// This matches stellar-core's default `BUCKETLIST_DB_INDEX_PAGE_SIZE` = 16384 (1 << 14).
/// Pages are sized by byte offset in the bucket file, not by entry count.
pub const DEFAULT_PAGE_SIZE: u64 = 16384;

/// Default file size cutoff in bytes for InMemory vs Disk index selection.
///
/// Buckets with file size below this threshold use InMemoryIndex (per-key offsets).
/// Matches stellar-core's `BUCKETLIST_DB_INDEX_CUTOFF` = 20 MB.
pub const DEFAULT_INDEX_CUTOFF: u64 = 20 * 1024 * 1024;

// ============================================================================
// Range Entry
// ============================================================================

/// A range of keys covered by a page or segment in the index.
#[derive(Debug, Clone)]
pub struct RangeEntry {
    /// The lower bound key (inclusive).
    pub lower_bound: LedgerKey,
    /// The upper bound key (inclusive).
    pub upper_bound: LedgerKey,
}

impl RangeEntry {
    /// Creates a new range entry.
    pub fn new(lower_bound: LedgerKey, upper_bound: LedgerKey) -> Self {
        Self {
            lower_bound,
            upper_bound,
        }
    }

    /// Checks if a key falls within this range.
    pub fn contains(&self, key: &LedgerKey) -> bool {
        compare_keys(key, &self.lower_bound) != std::cmp::Ordering::Less
            && compare_keys(key, &self.upper_bound) != std::cmp::Ordering::Greater
    }
}

// ============================================================================
// Entry Counters
// ============================================================================

/// Counters for bucket entries by type and durability.
///
/// This tracks the number of entries of each type in a bucket, useful for
/// statistics and optimizations.
#[derive(Debug, Clone, Default)]
pub struct BucketEntryCounters {
    /// Count of live entries by entry type.
    pub live_entries: HashMap<LedgerEntryType, u64>,
    /// Count of dead entries (tombstones) by entry type.
    pub dead_entries: HashMap<LedgerEntryType, u64>,
    /// Count of init entries by entry type.
    pub init_entries: HashMap<LedgerEntryType, u64>,
    /// XDR byte sizes per entry type (matches stellar-core's `entryTypeSizes`).
    pub entry_type_sizes: HashMap<LedgerEntryType, u64>,
    /// Count of persistent Soroban entries.
    pub persistent_soroban_entries: u64,
    /// Count of temporary Soroban entries.
    pub temporary_soroban_entries: u64,
}

impl BucketEntryCounters {
    /// Creates a new empty counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a bucket entry.
    pub fn record_entry(&mut self, entry: &BucketEntry) {
        // Compute XDR size of the full BucketEntry for type-size tracking.
        let xdr_size = entry.to_xdr_bytes().map(|v| v.len() as u64).unwrap_or(0);

        match entry {
            BucketEntry::Liveentry(e) => {
                let entry_type = ledger_entry_data_type(&e.data);
                *self.live_entries.entry(entry_type).or_insert(0) += 1;
                *self.entry_type_sizes.entry(entry_type).or_insert(0) += xdr_size;
                self.record_soroban_durability(e);
            }
            BucketEntry::Initentry(e) => {
                let entry_type = ledger_entry_data_type(&e.data);
                *self.init_entries.entry(entry_type).or_insert(0) += 1;
                *self.entry_type_sizes.entry(entry_type).or_insert(0) += xdr_size;
                self.record_soroban_durability(e);
            }
            BucketEntry::Deadentry(k) => {
                let entry_type = ledger_key_type(k);
                *self.dead_entries.entry(entry_type).or_insert(0) += 1;
                *self.entry_type_sizes.entry(entry_type).or_insert(0) += xdr_size;
            }
            BucketEntry::Metaentry(_) => {}
        }
    }

    /// Records Soroban durability for an entry.
    fn record_soroban_durability(&mut self, entry: &LedgerEntry) {
        if let LedgerEntryData::ContractData(data) = &entry.data {
            match data.durability {
                ContractDataDurability::Persistent => self.persistent_soroban_entries += 1,
                ContractDataDurability::Temporary => self.temporary_soroban_entries += 1,
            }
        } else if matches!(entry.data, LedgerEntryData::ContractCode(_)) {
            // ContractCode is always persistent
            self.persistent_soroban_entries += 1;
        }
    }

    /// Returns the total number of live and init entries.
    pub fn total_live(&self) -> u64 {
        self.live_entries.values().sum::<u64>() + self.init_entries.values().sum::<u64>()
    }

    /// Returns the total number of dead entries.
    pub fn total_dead(&self) -> u64 {
        self.dead_entries.values().sum()
    }

    /// Returns the total number of entries.
    pub fn total(&self) -> u64 {
        self.total_live() + self.total_dead()
    }

    /// Returns the count for a specific entry type (live + init).
    pub fn count_for_type(&self, entry_type: LedgerEntryType) -> u64 {
        self.live_entries.get(&entry_type).copied().unwrap_or(0)
            + self.init_entries.get(&entry_type).copied().unwrap_or(0)
    }

    /// Returns the total XDR byte size for a specific entry type.
    pub fn size_for_type(&self, entry_type: LedgerEntryType) -> u64 {
        self.entry_type_sizes.get(&entry_type).copied().unwrap_or(0)
    }

    /// Returns the total XDR byte size across all entry types.
    pub fn total_size(&self) -> u64 {
        self.entry_type_sizes.values().sum()
    }

    /// Merges another set of counters into this one (summing counts and sizes).
    pub fn merge(&mut self, other: &BucketEntryCounters) {
        for (k, v) in &other.live_entries {
            *self.live_entries.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.dead_entries {
            *self.dead_entries.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.init_entries {
            *self.init_entries.entry(*k).or_insert(0) += v;
        }
        for (k, v) in &other.entry_type_sizes {
            *self.entry_type_sizes.entry(*k).or_insert(0) += v;
        }
        self.persistent_soroban_entries += other.persistent_soroban_entries;
        self.temporary_soroban_entries += other.temporary_soroban_entries;
    }
}

// ============================================================================
// Asset to Pool ID Mapping
// ============================================================================

/// Maps assets to their associated liquidity pool IDs.
///
/// This allows efficient queries like "find all pool share trustlines for
/// an account and asset" by quickly identifying which pools contain a given asset.
#[derive(Debug, Clone, Default)]
pub struct AssetPoolIdMap {
    /// Maps asset hash to set of pool IDs containing that asset.
    asset_to_pools: HashMap<[u8; 32], HashSet<PoolId>>,
}

impl AssetPoolIdMap {
    /// Creates a new empty mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a pool mapping for the given assets.
    pub fn add_pool(&mut self, pool_id: PoolId, asset_a: &Asset, asset_b: &Asset) {
        let hash_a = Self::hash_asset(asset_a);
        let hash_b = Self::hash_asset(asset_b);

        self.asset_to_pools
            .entry(hash_a)
            .or_default()
            .insert(pool_id.clone());
        self.asset_to_pools
            .entry(hash_b)
            .or_default()
            .insert(pool_id);
    }

    /// Gets all pool IDs containing the given asset.
    pub fn get_pools_for_asset(&self, asset: &Asset) -> Vec<PoolId> {
        let hash = Self::hash_asset(asset);
        self.asset_to_pools
            .get(&hash)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Checks if the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.asset_to_pools.is_empty()
    }

    /// Returns the number of assets tracked.
    pub fn num_assets(&self) -> usize {
        self.asset_to_pools.len()
    }

    /// Returns a reference to the raw mapping.
    ///
    /// Used for serialization/persistence.
    pub fn raw_map(&self) -> &HashMap<[u8; 32], HashSet<PoolId>> {
        &self.asset_to_pools
    }

    /// Constructs an `AssetPoolIdMap` from a raw mapping.
    ///
    /// Used when restoring from persisted data.
    pub fn from_raw(asset_to_pools: HashMap<[u8; 32], HashSet<PoolId>>) -> Self {
        Self { asset_to_pools }
    }

    /// Computes a hash for an asset for use as a map key.
    fn hash_asset(asset: &Asset) -> [u8; 32] {
        let asset_bytes = asset.to_xdr(Limits::none()).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(&asset_bytes);
        let result = hasher.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&result);
        hash
    }
}

// ============================================================================
// Type Range
// ============================================================================

/// Tracks the byte range for a specific entry type in the bucket.
///
/// This enables efficient type-specific scans by identifying the contiguous
/// range of entries of a given type.
#[derive(Debug, Clone, Copy)]
pub struct TypeRange {
    /// Start offset in the bucket file.
    pub start_offset: u64,
    /// End offset in the bucket file (exclusive).
    pub end_offset: u64,
}

impl TypeRange {
    /// Creates a new type range.
    pub fn new(start_offset: u64, end_offset: u64) -> Self {
        Self {
            start_offset,
            end_offset,
        }
    }

    /// Returns the size of this range in bytes.
    pub fn size(&self) -> u64 {
        self.end_offset - self.start_offset
    }
}

// ============================================================================
// In-Memory Index
// ============================================================================

/// An in-memory index for small buckets.
///
/// This stores all entry keys and their offsets in memory, providing O(log n)
/// lookup time. Suitable for buckets with file size below [`DEFAULT_INDEX_CUTOFF`]
/// entries.
#[derive(Debug, Clone)]
pub struct InMemoryIndex {
    /// Maps keys to their byte offsets in the bucket file.
    key_to_offset: HashMap<Vec<u8>, u64>,
    /// Optional bloom filter for fast negative lookups.
    bloom_filter: Option<Arc<BucketBloomFilter>>,
    /// Bloom filter seed.
    bloom_seed: HashSeed,
    /// Asset to pool ID mapping.
    asset_to_pool_id: AssetPoolIdMap,
    /// Entry counters.
    counters: BucketEntryCounters,
    /// Ranges for each entry type.
    type_ranges: HashMap<LedgerEntryType, TypeRange>,
}

impl InMemoryIndex {
    /// Creates a new in-memory index from bucket entries.
    ///
    /// # Arguments
    ///
    /// * `entries` - Iterator over (BucketEntry, offset) pairs
    /// * `bloom_seed` - Seed for bloom filter construction
    pub fn from_entries<I>(entries: I, bloom_seed: HashSeed) -> Self
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        let mut key_to_offset = HashMap::new();
        let mut bloom_key_hashes = Vec::new();
        let mut asset_to_pool_id = AssetPoolIdMap::new();
        let mut counters = BucketEntryCounters::new();
        let mut type_ranges: HashMap<LedgerEntryType, (u64, u64)> = HashMap::new();
        let mut current_type: Option<LedgerEntryType> = None;
        let mut type_start_offset = 0u64;

        for (entry, offset) in entries {
            // Record counters
            counters.record_entry(&entry);

            // Extract key
            if let Some(key) = entry.key() {
                let entry_type = ledger_key_type(&key);

                // Track type ranges
                if current_type != Some(entry_type) {
                    // Close previous type range
                    if let Some(prev_type) = current_type {
                        type_ranges.insert(prev_type, (type_start_offset, offset));
                    }
                    current_type = Some(entry_type);
                    type_start_offset = offset;
                }

                // Serialize key for index
                let key_bytes = key.to_xdr(Limits::none()).unwrap_or_default();
                key_to_offset.insert(key_bytes.clone(), offset);

                // Compute bloom hash
                bloom_key_hashes.push(BucketBloomFilter::hash_key(&key, &bloom_seed));

                // Extract pool mappings from liquidity pool entries
                if let BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) = &entry {
                    if let LedgerEntryData::LiquidityPool(pool) = &e.data {
                        let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                            cp,
                        ) = &pool.body;
                        asset_to_pool_id.add_pool(
                            pool.liquidity_pool_id.clone(),
                            &cp.params.asset_a,
                            &cp.params.asset_b,
                        );
                    }
                }
            }
        }

        // Close final type range (use u64::MAX as sentinel for end)
        if let Some(prev_type) = current_type {
            type_ranges.insert(prev_type, (type_start_offset, u64::MAX));
        }

        // Build bloom filter
        let bloom_filter = if bloom_key_hashes.len() >= 2 {
            BucketBloomFilter::from_hashes(&bloom_key_hashes, &bloom_seed)
                .ok()
                .map(Arc::new)
        } else {
            None
        };

        // Convert type ranges
        let type_ranges = type_ranges
            .into_iter()
            .map(|(k, (start, end))| (k, TypeRange::new(start, end)))
            .collect();

        Self {
            key_to_offset,
            bloom_filter,
            bloom_seed,
            asset_to_pool_id,
            counters,
            type_ranges,
        }
    }

    /// Looks up the offset for a key.
    pub fn get_offset(&self, key: &LedgerKey) -> Option<u64> {
        // Check bloom filter first
        if let Some(ref filter) = self.bloom_filter {
            if !filter.may_contain(key, &self.bloom_seed) {
                return None;
            }
        }

        let key_bytes = key.to_xdr(Limits::none()).ok()?;
        self.key_to_offset.get(&key_bytes).copied()
    }

    /// Checks if a key may exist in the index (bloom filter check).
    pub fn may_contain(&self, key: &LedgerKey) -> bool {
        if let Some(ref filter) = self.bloom_filter {
            filter.may_contain(key, &self.bloom_seed)
        } else {
            true // No bloom filter, assume it might exist
        }
    }

    /// Checks if pre-serialized key bytes may exist (bloom filter check).
    pub fn may_contain_bytes(&self, key_bytes: &[u8]) -> bool {
        if let Some(ref filter) = self.bloom_filter {
            let hash =
                crate::bloom_filter::BucketBloomFilter::hash_bytes(key_bytes, &self.bloom_seed);
            filter.may_contain_hash(hash)
        } else {
            true
        }
    }

    /// Looks up the offset for a key using pre-serialized key bytes.
    ///
    /// Skips bloom filter check (caller should check separately if needed).
    pub fn get_offset_by_key_bytes(&self, key_bytes: &[u8]) -> Option<u64> {
        self.key_to_offset.get(key_bytes).copied()
    }

    /// Returns the entry counters.
    pub fn counters(&self) -> &BucketEntryCounters {
        &self.counters
    }

    /// Returns the asset to pool ID mapping.
    pub fn asset_to_pool_id(&self) -> &AssetPoolIdMap {
        &self.asset_to_pool_id
    }

    /// Returns the type range for a specific entry type.
    pub fn type_range(&self, entry_type: LedgerEntryType) -> Option<&TypeRange> {
        self.type_ranges.get(&entry_type)
    }

    /// Returns the bloom filter seed.
    pub fn bloom_seed(&self) -> HashSeed {
        self.bloom_seed
    }

    /// Returns the size of the bloom filter in bytes, or 0 if no filter exists.
    pub fn bloom_filter_size_bytes(&self) -> usize {
        self.bloom_filter.as_ref().map_or(0, |f| f.size_bytes())
    }

    /// Returns the number of indexed keys.
    pub fn len(&self) -> usize {
        self.key_to_offset.len()
    }

    /// Checks if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.key_to_offset.is_empty()
    }

    /// Estimates heap memory usage in bytes.
    ///
    /// Accounts for:
    /// - `key_to_offset` HashMap: key Vec allocations + value u64 + HashMap overhead (~48 bytes/entry)
    /// - bloom filter allocation
    /// - type_ranges and asset_to_pool_id (relatively small)
    pub fn estimated_heap_bytes(&self) -> usize {
        let key_data_bytes: usize = self.key_to_offset.keys().map(|k| k.len()).sum();
        // Each HashMap entry: key Vec (24 stack + heap), value u64 (8), plus ~48 bytes bucket overhead
        let map_overhead = self.key_to_offset.len() * (24 + 8 + 48);
        let bloom_bytes = self.bloom_filter_size_bytes();
        key_data_bytes + map_overhead + bloom_bytes
    }
}

// ============================================================================
// Disk Index
// ============================================================================

/// A disk-based page index for large buckets.
///
/// Instead of storing every key, this stores range information for pages
/// of entries. A lookup first finds the page that might contain the key,
/// then reads that page from disk.
#[derive(Debug, Clone)]
pub struct DiskIndex {
    /// Page size in bytes. Pages are split at byte-offset boundaries in the
    /// bucket file, matching stellar-core's `DiskIndex` behavior.
    page_size: u64,
    /// Maps page ranges to file offsets.
    /// Each entry contains (RangeEntry, page_start_offset).
    pages: Vec<(RangeEntry, u64)>,
    /// Optional bloom filter for fast negative lookups.
    bloom_filter: Option<Arc<BucketBloomFilter>>,
    /// Bloom filter seed.
    bloom_seed: HashSeed,
    /// Asset to pool ID mapping.
    asset_to_pool_id: AssetPoolIdMap,
    /// Entry counters.
    counters: BucketEntryCounters,
    /// Ranges for each entry type.
    type_ranges: HashMap<LedgerEntryType, TypeRange>,
}

impl DiskIndex {
    /// Creates a new disk index from bucket entries.
    ///
    /// Pages are built by byte offset in the bucket file, matching stellar-core's
    /// `DiskIndex` constructor. A new page starts whenever the entry's file offset
    /// crosses the next `page_size`-aligned boundary.
    ///
    /// # Arguments
    ///
    /// * `entries` - Iterator over (BucketEntry, offset) pairs
    /// * `bloom_seed` - Seed for bloom filter construction
    /// * `page_size` - Page size in bytes (must be a power of two)
    pub fn from_entries<I>(entries: I, bloom_seed: HashSeed, page_size: u64) -> Self
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        let mut pages = Vec::new();
        let mut bloom_key_hashes = Vec::new();
        let mut asset_to_pool_id = AssetPoolIdMap::new();
        let mut counters = BucketEntryCounters::new();
        let mut type_ranges: HashMap<LedgerEntryType, (u64, u64)> = HashMap::new();
        let mut current_type: Option<LedgerEntryType> = None;
        let mut type_start_offset = 0u64;

        // Byte-based page building state (matches stellar-core DiskIndex.cpp)
        let mut page_upper_bound: u64 = 0;
        let mut is_first_entry = true;

        for (entry, offset) in entries {
            // Record counters
            counters.record_entry(&entry);

            // Extract key
            if let Some(key) = entry.key() {
                let entry_type = ledger_key_type(&key);

                // Track type ranges
                if current_type != Some(entry_type) {
                    if let Some(prev_type) = current_type {
                        type_ranges.insert(prev_type, (type_start_offset, offset));
                    }
                    current_type = Some(entry_type);
                    type_start_offset = offset;
                }

                // Compute bloom hash
                bloom_key_hashes.push(BucketBloomFilter::hash_key(&key, &bloom_seed));

                // Page handling: start a new page when offset crosses page_upper_bound
                if is_first_entry || offset >= page_upper_bound {
                    // Align to page boundary and advance by one page
                    page_upper_bound = (offset & !(page_size - 1)) + page_size;
                    pages.push((RangeEntry::new(key.clone(), key.clone()), offset));
                    is_first_entry = false;
                } else {
                    // Extend current page's upper bound
                    pages.last_mut().unwrap().0.upper_bound = key.clone();
                }

                // Extract pool mappings
                if let BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) = &entry {
                    if let LedgerEntryData::LiquidityPool(pool) = &e.data {
                        let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                            cp,
                        ) = &pool.body;
                        asset_to_pool_id.add_pool(
                            pool.liquidity_pool_id.clone(),
                            &cp.params.asset_a,
                            &cp.params.asset_b,
                        );
                    }
                }
            }
        }

        // Close final type range
        if let Some(prev_type) = current_type {
            type_ranges.insert(prev_type, (type_start_offset, u64::MAX));
        }

        // Build bloom filter
        let bloom_filter = if bloom_key_hashes.len() >= 2 {
            BucketBloomFilter::from_hashes(&bloom_key_hashes, &bloom_seed)
                .ok()
                .map(Arc::new)
        } else {
            None
        };

        // Convert type ranges
        let type_ranges = type_ranges
            .into_iter()
            .map(|(k, (start, end))| (k, TypeRange::new(start, end)))
            .collect();

        Self {
            page_size,
            pages,
            bloom_filter,
            bloom_seed,
            asset_to_pool_id,
            counters,
            type_ranges,
        }
    }

    /// Finds the page that might contain a key.
    ///
    /// Returns the page offset if found, or None if the key is definitely
    /// not in any page.
    pub fn find_page_for_key(&self, key: &LedgerKey) -> Option<u64> {
        // Check bloom filter first
        if let Some(ref filter) = self.bloom_filter {
            if !filter.may_contain(key, &self.bloom_seed) {
                return None;
            }
        }

        // Binary search for the page containing the key
        let idx = self.pages.partition_point(|(range, _)| {
            compare_keys(&range.upper_bound, key) == std::cmp::Ordering::Less
        });

        if idx < self.pages.len() && self.pages[idx].0.contains(key) {
            Some(self.pages[idx].1)
        } else {
            None
        }
    }

    /// Checks if a key may exist in the index (bloom filter check).
    pub fn may_contain(&self, key: &LedgerKey) -> bool {
        if let Some(ref filter) = self.bloom_filter {
            filter.may_contain(key, &self.bloom_seed)
        } else {
            true
        }
    }

    /// Checks if pre-serialized key bytes may exist (bloom filter check).
    pub fn may_contain_bytes(&self, key_bytes: &[u8]) -> bool {
        if let Some(ref filter) = self.bloom_filter {
            let hash =
                crate::bloom_filter::BucketBloomFilter::hash_bytes(key_bytes, &self.bloom_seed);
            filter.may_contain_hash(hash)
        } else {
            true
        }
    }

    /// Returns the entry counters.
    pub fn counters(&self) -> &BucketEntryCounters {
        &self.counters
    }

    /// Returns the asset to pool ID mapping.
    pub fn asset_to_pool_id(&self) -> &AssetPoolIdMap {
        &self.asset_to_pool_id
    }

    /// Returns the type range for a specific entry type.
    pub fn type_range(&self, entry_type: LedgerEntryType) -> Option<&TypeRange> {
        self.type_ranges.get(&entry_type)
    }

    /// Returns the number of pages.
    pub fn num_pages(&self) -> usize {
        self.pages.len()
    }

    /// Returns the page size.
    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Creates a DiskIndex from persisted data.
    ///
    /// This is used when loading an index from disk storage instead of
    /// rebuilding it from the bucket file.
    ///
    /// # Arguments
    ///
    /// * `page_size` - Page size in bytes
    /// * `pages` - Page ranges and their offsets
    /// * `bloom_seed` - Seed for bloom filter reconstruction
    /// * `counters` - Entry counters
    /// * `type_ranges` - Ranges for each entry type
    /// * `bloom_filter` - Optional persisted bloom filter
    /// * `asset_to_pool_id` - Optional persisted asset-to-pool-id mapping
    pub fn from_persisted(
        page_size: u64,
        pages: Vec<(RangeEntry, u64)>,
        bloom_seed: HashSeed,
        counters: BucketEntryCounters,
        type_ranges: HashMap<LedgerEntryType, TypeRange>,
        bloom_filter: Option<BucketBloomFilter>,
        asset_to_pool_id: Option<AssetPoolIdMap>,
    ) -> Self {
        Self {
            page_size,
            pages,
            bloom_filter: bloom_filter.map(Arc::new),
            bloom_seed,
            asset_to_pool_id: asset_to_pool_id.unwrap_or_default(),
            counters,
            type_ranges,
        }
    }

    /// Returns an iterator over pages (range, offset) pairs.
    ///
    /// Used for serialization/persistence.
    pub fn pages_iter(&self) -> impl Iterator<Item = (&RangeEntry, &u64)> {
        self.pages.iter().map(|(range, offset)| (range, offset))
    }

    /// Returns an iterator over type ranges.
    ///
    /// Used for serialization/persistence.
    pub fn type_ranges_iter(&self) -> impl Iterator<Item = (&LedgerEntryType, &TypeRange)> {
        self.type_ranges.iter()
    }

    /// Returns the bloom filter seed.
    ///
    /// Used for serialization/persistence.
    pub fn bloom_seed(&self) -> HashSeed {
        self.bloom_seed
    }

    /// Returns a reference to the bloom filter, if present.
    ///
    /// Used for serialization/persistence.
    pub fn bloom_filter(&self) -> Option<&BucketBloomFilter> {
        self.bloom_filter.as_deref()
    }

    /// Returns the size of the bloom filter in bytes, or 0 if no filter exists.
    pub fn bloom_filter_size_bytes(&self) -> usize {
        self.bloom_filter.as_ref().map_or(0, |f| f.size_bytes())
    }

    /// Estimates heap memory usage in bytes.
    ///
    /// Accounts for:
    /// - `pages` Vec: each entry has two LedgerKey clones + u64 offset
    /// - bloom filter allocation
    /// - type_ranges and asset_to_pool_id (relatively small)
    pub fn estimated_heap_bytes(&self) -> usize {
        // Each page entry: RangeEntry (two LedgerKeys, ~200 bytes each estimated) + u64 offset
        // Plus Vec overhead for the allocation
        let pages_bytes = self.pages.capacity() * std::mem::size_of::<(RangeEntry, u64)>();
        let bloom_bytes = self.bloom_filter_size_bytes();
        pages_bytes + bloom_bytes
    }
}

// ============================================================================
// Live Bucket Index (Facade)
// ============================================================================

/// A hybrid index that selects between in-memory and disk-based indexing.
///
/// This facade automatically chooses the appropriate index type based on
/// bucket size, matching the stellar-core `LiveBucketIndex` pattern.
#[derive(Debug, Clone)]
pub enum LiveBucketIndex {
    /// In-memory index for small buckets.
    InMemory(InMemoryIndex),
    /// Disk-based page index for large buckets.
    Disk(DiskIndex),
}

impl LiveBucketIndex {
    /// Creates a new index from bucket entries.
    ///
    /// Automatically selects in-memory or disk-based indexing based on
    /// the bucket's file size, matching stellar-core's `BUCKETLIST_DB_INDEX_CUTOFF`.
    ///
    /// # Arguments
    ///
    /// * `entries` - Iterator over (BucketEntry, offset) pairs
    /// * `bloom_seed` - Seed for bloom filter construction
    /// * `file_size` - Size of the bucket file in bytes
    /// * `config` - BucketListDB configuration
    pub fn from_entries<I>(
        entries: I,
        bloom_seed: HashSeed,
        file_size: u64,
        config: &BucketListDbConfig,
    ) -> Self
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        if file_size < config.index_cutoff_bytes() {
            LiveBucketIndex::InMemory(InMemoryIndex::from_entries(entries, bloom_seed))
        } else {
            LiveBucketIndex::Disk(DiskIndex::from_entries(
                entries,
                bloom_seed,
                config.page_size_bytes(),
            ))
        }
    }

    /// Creates a new index from bucket entries with default config.
    ///
    /// Convenience method that uses the default `BucketListDbConfig`.
    pub fn from_entries_default<I>(entries: I, bloom_seed: HashSeed, file_size: u64) -> Self
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        let config = BucketListDbConfig::default();
        Self::from_entries(entries, bloom_seed, file_size, &config)
    }

    /// Creates a new index from bucket entries with default config, returning the
    /// consumed iterator alongside the index. This allows the caller to extract
    /// state accumulated during iteration (e.g., hash, record count).
    pub fn from_entries_default_with_iter<I>(
        entries: I,
        bloom_seed: HashSeed,
        file_size: u64,
    ) -> (Self, I)
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        let config = BucketListDbConfig::default();
        Self::from_entries_with_iter(entries, bloom_seed, file_size, &config)
    }

    /// Creates a new index, returning both the index and the consumed iterator.
    pub fn from_entries_with_iter<I>(
        mut entries: I,
        bloom_seed: HashSeed,
        file_size: u64,
        config: &BucketListDbConfig,
    ) -> (Self, I)
    where
        I: Iterator<Item = (BucketEntry, u64)>,
    {
        if file_size < config.index_cutoff_bytes() {
            let index = InMemoryIndex::from_entries(&mut entries, bloom_seed);
            (LiveBucketIndex::InMemory(index), entries)
        } else {
            let index = DiskIndex::from_entries(&mut entries, bloom_seed, config.page_size_bytes());
            (LiveBucketIndex::Disk(index), entries)
        }
    }

    /// Returns true if the given entry type is not supported by BucketListDB lookups.
    /// Matches stellar-core's `LiveBucketIndex::typeNotSupported(OFFER)`.
    pub fn type_not_supported(entry_type: LedgerEntryType) -> bool {
        entry_type == LedgerEntryType::Offer
    }

    /// Checks if this is an in-memory index.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, LiveBucketIndex::InMemory(_))
    }

    /// Checks if a key may exist in the index (bloom filter check).
    pub fn may_contain(&self, key: &LedgerKey) -> bool {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.may_contain(key),
            LiveBucketIndex::Disk(idx) => idx.may_contain(key),
        }
    }

    /// Checks if pre-serialized key bytes may exist (bloom filter check).
    pub fn may_contain_bytes(&self, key_bytes: &[u8]) -> bool {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.may_contain_bytes(key_bytes),
            LiveBucketIndex::Disk(idx) => idx.may_contain_bytes(key_bytes),
        }
    }

    /// Returns the entry counters.
    pub fn counters(&self) -> &BucketEntryCounters {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.counters(),
            LiveBucketIndex::Disk(idx) => idx.counters(),
        }
    }

    /// Returns the asset to pool ID mapping.
    pub fn asset_to_pool_id(&self) -> &AssetPoolIdMap {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.asset_to_pool_id(),
            LiveBucketIndex::Disk(idx) => idx.asset_to_pool_id(),
        }
    }

    /// Returns the type range for a specific entry type.
    pub fn type_range(&self, entry_type: LedgerEntryType) -> Option<&TypeRange> {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.type_range(entry_type),
            LiveBucketIndex::Disk(idx) => idx.type_range(entry_type),
        }
    }

    /// Gets pools containing a specific asset.
    pub fn get_pools_for_asset(&self, asset: &Asset) -> Vec<PoolId> {
        self.asset_to_pool_id().get_pools_for_asset(asset)
    }

    /// Generates trustline keys for pool share lookups.
    ///
    /// Given an account and asset, returns the trustline keys for all pool
    /// share trustlines that the account might have for pools containing
    /// the asset.
    pub fn get_pool_share_trustline_keys(
        &self,
        account_id: &stellar_xdr::curr::AccountId,
        asset: &Asset,
    ) -> Vec<LedgerKey> {
        let pools = self.get_pools_for_asset(asset);

        pools
            .into_iter()
            .map(|pool_id| {
                LedgerKey::Trustline(stellar_xdr::curr::LedgerKeyTrustLine {
                    account_id: account_id.clone(),
                    asset: TrustLineAsset::PoolShare(pool_id),
                })
            })
            .collect()
    }

    /// Returns the bloom filter seed.
    pub fn bloom_seed(&self) -> HashSeed {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.bloom_seed(),
            LiveBucketIndex::Disk(idx) => idx.bloom_seed(),
        }
    }

    /// Returns the size of the bloom filter in bytes, or 0 if no filter exists.
    pub fn bloom_filter_size_bytes(&self) -> usize {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.bloom_filter_size_bytes(),
            LiveBucketIndex::Disk(idx) => idx.bloom_filter_size_bytes(),
        }
    }

    /// Estimates heap memory usage in bytes.
    pub fn estimated_heap_bytes(&self) -> usize {
        match self {
            LiveBucketIndex::InMemory(idx) => idx.estimated_heap_bytes(),
            LiveBucketIndex::Disk(idx) => idx.estimated_heap_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BucketEntry;
    use stellar_xdr::curr::*;

    fn make_account_id(byte: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32])))
    }

    fn make_account_entry(byte: u8) -> LedgerEntry {
        LedgerEntry {
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
        }
    }

    fn make_account_key(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(byte),
        })
    }

    #[test]
    fn test_in_memory_index() {
        let entries: Vec<(BucketEntry, u64)> = (0..10u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        assert_eq!(index.len(), 10);

        // Test lookup
        let key = make_account_key(5);
        let offset = index.get_offset(&key);
        assert_eq!(offset, Some(500));

        // Test missing key
        let missing_key = make_account_key(100);
        assert!(index.get_offset(&missing_key).is_none());
    }

    #[test]
    fn test_disk_index() {
        // With byte-based pages: entries are spaced 128 bytes apart in test data.
        // Using page_size=1024 (power of two) → 10 entries per page.
        let entries: Vec<(BucketEntry, u64)> = (0..100u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 128,
                )
            })
            .collect();

        // page_size=1024, offsets 0..12672 → pages at 0, 1024, 2048, ..., 11264, 12288
        // That's ceil(12672/1024) = 13 pages, but last entry is at 99*128=12672.
        // Pages: [0,1024), [1024,2048), ..., [12288, 13312)
        // So 13 pages.
        let index = DiskIndex::from_entries(entries.into_iter(), [0u8; 16], 1024);

        assert!(index.num_pages() > 0);

        // Test page lookup
        let key = make_account_key(55);
        let page_offset = index.find_page_for_key(&key);
        assert!(page_offset.is_some());
    }

    #[test]
    fn test_entry_counters() {
        let mut counters = BucketEntryCounters::new();

        counters.record_entry(&BucketEntry::Liveentry(make_account_entry(1)));
        counters.record_entry(&BucketEntry::Liveentry(make_account_entry(2)));
        counters.record_entry(&BucketEntry::Deadentry(make_account_key(3)));

        assert_eq!(counters.total_live(), 2);
        assert_eq!(counters.total_dead(), 1);
        assert_eq!(counters.total(), 3);
        assert_eq!(counters.count_for_type(LedgerEntryType::Account), 2);
    }

    #[test]
    fn test_range_entry() {
        let lower = make_account_key(10);
        let upper = make_account_key(20);
        let range = RangeEntry::new(lower, upper);

        assert!(range.contains(&make_account_key(10)));
        assert!(range.contains(&make_account_key(15)));
        assert!(range.contains(&make_account_key(20)));
        assert!(!range.contains(&make_account_key(5)));
        assert!(!range.contains(&make_account_key(25)));
    }

    #[test]
    fn test_live_bucket_index_selection() {
        // Small file (below default 20MB cutoff) should use in-memory
        let entries: Vec<(BucketEntry, u64)> = (0..100u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        // file_size=10000 is well below the 20MB default cutoff
        let index = LiveBucketIndex::from_entries_default(entries.into_iter(), [0u8; 16], 10000);
        assert!(index.is_in_memory());

        assert!(index.may_contain(&make_account_key(50)));
    }

    #[test]
    fn test_live_bucket_index_file_size_threshold() {
        // Small file → InMemory
        let entries1: Vec<(BucketEntry, u64)> = (0..10u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();
        let config = BucketListDbConfig::default();
        let index = LiveBucketIndex::from_entries(entries1.into_iter(), [0u8; 16], 1000, &config);
        assert!(index.is_in_memory());

        // Custom config with cutoff_mb=0 → always DiskIndex
        let entries2: Vec<(BucketEntry, u64)> = (0..10u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();
        let mut config_small = BucketListDbConfig::default();
        config_small.index_cutoff_mb = 0;
        let index =
            LiveBucketIndex::from_entries(entries2.into_iter(), [0u8; 16], 1000, &config_small);
        assert!(!index.is_in_memory());
    }

    #[test]
    fn test_asset_pool_id_map() {
        let mut map = AssetPoolIdMap::new();

        let pool_id = PoolId(Hash([1u8; 32]));
        let asset_a = Asset::Native;
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: make_account_id(1),
        });

        map.add_pool(pool_id.clone(), &asset_a, &asset_b);

        let pools_for_native = map.get_pools_for_asset(&asset_a);
        assert_eq!(pools_for_native.len(), 1);
        assert_eq!(pools_for_native[0], pool_id);

        let pools_for_usd = map.get_pools_for_asset(&asset_b);
        assert_eq!(pools_for_usd.len(), 1);
    }

    // ============ P2-4: In-Memory Index Offer Positioning ============
    //
    // stellar-core: BucketIndexTests.cpp "in-memory index construction"
    // Tests that offer entries at different positions in the bucket
    // are correctly indexed.

    fn make_offer_entry(seed: u8, offer_id: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Offer(OfferEntry {
                seller_id: make_account_id(seed),
                offer_id,
                selling: Asset::Native,
                buying: Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*b"USD\0"),
                    issuer: make_account_id(0),
                }),
                amount: 1000,
                price: Price { n: 1, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_index_with_no_offers() {
        // Index construction should work correctly with no offers
        let entries: Vec<(BucketEntry, u64)> = (0..20u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        assert_eq!(index.len(), 20);
        // No offer type range should exist
        assert!(index.type_range(LedgerEntryType::Offer).is_none());
        assert!(index.type_range(LedgerEntryType::Account).is_some());
    }

    #[test]
    fn test_index_with_offers_at_end() {
        // Offers positioned after account entries (sorted by type discriminant)
        let mut entries: Vec<(BucketEntry, u64)> = Vec::new();

        // Account entries first
        for i in 0..10u8 {
            entries.push((
                BucketEntry::Liveentry(make_account_entry(i)),
                i as u64 * 100,
            ));
        }
        // Offer entries after
        for i in 0..5u8 {
            entries.push((
                BucketEntry::Liveentry(make_offer_entry(i, i as i64 + 1)),
                (10 + i) as u64 * 100,
            ));
        }

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        assert_eq!(index.len(), 15);
        assert!(index.type_range(LedgerEntryType::Account).is_some());
        assert!(index.type_range(LedgerEntryType::Offer).is_some());

        // Verify offer lookup works
        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: make_account_id(2),
            offer_id: 3,
        });
        assert!(index.get_offset(&offer_key).is_some());
    }

    #[test]
    fn test_index_with_offers_between_types() {
        // Mix of account, offer, and contract data entries
        let mut entries: Vec<(BucketEntry, u64)> = Vec::new();

        // Accounts (type discriminant comes first)
        for i in 0..5u8 {
            entries.push((
                BucketEntry::Liveentry(make_account_entry(i)),
                i as u64 * 100,
            ));
        }
        // Offers (between accounts and contract data)
        for i in 0..5u8 {
            entries.push((
                BucketEntry::Liveentry(make_offer_entry(i, i as i64 + 1)),
                (5 + i) as u64 * 100,
            ));
        }

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        assert_eq!(index.len(), 10);

        // Both lookups should work
        let acct_key = make_account_key(3);
        assert!(index.get_offset(&acct_key).is_some());

        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: make_account_id(3),
            offer_id: 4,
        });
        assert!(index.get_offset(&offer_key).is_some());
    }

    // ============ P2-5: ContractData Key with Same ScVal ============
    //
    // stellar-core: BucketIndexTests.cpp "ContractData key with same ScVal"
    // Tests that contract data entries with identical ScVal keys but
    // different contracts or durabilities are correctly distinguished.

    fn make_contract_data_entry(
        contract_seed: u8,
        key_val: i32,
        durability: ContractDataDurability,
    ) -> LedgerEntry {
        use stellar_xdr::curr::*;
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([contract_seed; 32]))),
                key: ScVal::I32(key_val),
                durability,
                val: ScVal::I64(100),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_contract_data_key(
        contract_seed: u8,
        key_val: i32,
        durability: ContractDataDurability,
    ) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([contract_seed; 32]))),
            key: ScVal::I32(key_val),
            durability,
        })
    }

    #[test]
    fn test_contract_data_same_scval_different_contracts() {
        // Same ScVal key, different contract addresses
        let entries: Vec<(BucketEntry, u64)> = vec![
            (
                BucketEntry::Liveentry(make_contract_data_entry(
                    1,
                    42,
                    ContractDataDurability::Persistent,
                )),
                0,
            ),
            (
                BucketEntry::Liveentry(make_contract_data_entry(
                    2,
                    42,
                    ContractDataDurability::Persistent,
                )),
                100,
            ),
        ];

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);
        assert_eq!(index.len(), 2);

        // Both should be independently findable
        let key1 = make_contract_data_key(1, 42, ContractDataDurability::Persistent);
        let key2 = make_contract_data_key(2, 42, ContractDataDurability::Persistent);
        assert!(index.get_offset(&key1).is_some());
        assert!(index.get_offset(&key2).is_some());
        assert_ne!(index.get_offset(&key1), index.get_offset(&key2));
    }

    #[test]
    fn test_contract_data_same_scval_different_durability() {
        // Same contract and ScVal key, but different durability
        let entries: Vec<(BucketEntry, u64)> = vec![
            (
                BucketEntry::Liveentry(make_contract_data_entry(
                    1,
                    42,
                    ContractDataDurability::Persistent,
                )),
                0,
            ),
            (
                BucketEntry::Liveentry(make_contract_data_entry(
                    1,
                    42,
                    ContractDataDurability::Temporary,
                )),
                100,
            ),
        ];

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);
        assert_eq!(index.len(), 2);

        let key_persistent = make_contract_data_key(1, 42, ContractDataDurability::Persistent);
        let key_temporary = make_contract_data_key(1, 42, ContractDataDurability::Temporary);
        assert!(index.get_offset(&key_persistent).is_some());
        assert!(index.get_offset(&key_temporary).is_some());
        assert_ne!(
            index.get_offset(&key_persistent),
            index.get_offset(&key_temporary)
        );
    }

    // ============ P2-6: Account Lookup by ID ============
    //
    // stellar-core: BucketIndexTests.cpp "loadAccountsByAccountID"
    // Tests that account entries can be looked up by account ID.

    #[test]
    fn test_account_lookup_by_id() {
        let entries: Vec<(BucketEntry, u64)> = (0..50u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        // Look up specific accounts
        for i in [0u8, 10, 25, 49] {
            let key = make_account_key(i);
            let offset = index.get_offset(&key);
            assert!(offset.is_some(), "Account {} should be in index", i);
            assert_eq!(offset.unwrap(), i as u64 * 100);
        }

        // Non-existent accounts
        for i in [50u8, 100, 255] {
            let key = make_account_key(i);
            assert!(
                index.get_offset(&key).is_none(),
                "Account {} should not be in index",
                i
            );
        }
    }

    #[test]
    fn test_account_lookup_with_bloom_filter() {
        let entries: Vec<(BucketEntry, u64)> = (0..100u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        // may_contain should return true for existing keys
        for i in [0u8, 50, 99] {
            let key = make_account_key(i);
            assert!(
                index.may_contain(&key),
                "Bloom filter should confirm account {}",
                i
            );
        }

        // may_contain may return true for non-existing keys (false positives OK)
        // but the actual offset should be None
        let key = make_account_key(200);
        // We can't assert may_contain is false (bloom filters have false positives)
        // but we can assert get_offset is None
        assert!(index.get_offset(&key).is_none());
    }

    // ============ P2-7: Soroban Cache Population ============
    //
    // stellar-core: BucketIndexTests.cpp "soroban cache population"
    // Tests that contract code/data entries are correctly counted in
    // the index entry counters.

    fn make_contract_code_entry(seed: u8) -> LedgerEntry {
        use stellar_xdr::curr::*;
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: Hash([seed; 32]),
                code: vec![0u8; 100].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_soroban_entry_counters() {
        let mut entries: Vec<(BucketEntry, u64)> = Vec::new();

        // Add contract code entries
        for i in 0..5u8 {
            entries.push((
                BucketEntry::Liveentry(make_contract_code_entry(i)),
                i as u64 * 100,
            ));
        }

        // Add persistent contract data entries
        for i in 0..3u8 {
            entries.push((
                BucketEntry::Liveentry(make_contract_data_entry(
                    i,
                    i as i32,
                    ContractDataDurability::Persistent,
                )),
                (5 + i) as u64 * 100,
            ));
        }

        // Add temporary contract data entries
        for i in 0..2u8 {
            entries.push((
                BucketEntry::Liveentry(make_contract_data_entry(
                    10 + i,
                    i as i32,
                    ContractDataDurability::Temporary,
                )),
                (8 + i) as u64 * 100,
            ));
        }

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        let counters = index.counters();
        assert_eq!(
            counters.count_for_type(LedgerEntryType::ContractCode),
            5,
            "Should have 5 contract code entries"
        );
        assert_eq!(
            counters.count_for_type(LedgerEntryType::ContractData),
            5,
            "Should have 5 contract data entries (3 persistent + 2 temporary)"
        );
        assert_eq!(
            counters.persistent_soroban_entries, 8,
            "Should have 8 persistent Soroban entries (5 code + 3 persistent data)"
        );
        assert_eq!(
            counters.temporary_soroban_entries, 2,
            "Should have 2 temporary Soroban entries"
        );
    }

    #[test]
    fn test_soroban_dead_entry_counters() {
        let entries: Vec<(BucketEntry, u64)> = vec![
            (BucketEntry::Liveentry(make_contract_code_entry(1)), 0),
            (BucketEntry::Liveentry(make_contract_code_entry(2)), 100),
            (
                BucketEntry::Deadentry(LedgerKey::ContractCode(LedgerKeyContractCode {
                    hash: Hash([3u8; 32]),
                })),
                200,
            ),
        ];

        let index = InMemoryIndex::from_entries(entries.into_iter(), [0u8; 16]);

        let counters = index.counters();
        assert_eq!(counters.total_live(), 2);
        assert_eq!(counters.total_dead(), 1);
    }

    #[test]
    fn test_entry_type_sizes() {
        let mut counters = BucketEntryCounters::new();

        // Record some entries of different types
        counters.record_entry(&BucketEntry::Liveentry(make_account_entry(1)));
        counters.record_entry(&BucketEntry::Liveentry(make_account_entry(2)));
        counters.record_entry(&BucketEntry::Liveentry(make_offer_entry(1, 1)));
        counters.record_entry(&BucketEntry::Deadentry(make_account_key(3)));

        // Verify sizes are accumulated for account type (2 live + 1 dead)
        let account_size = counters.size_for_type(LedgerEntryType::Account);
        assert!(account_size > 0, "Account size should be non-zero");

        // Verify offer size is tracked
        let offer_size = counters.size_for_type(LedgerEntryType::Offer);
        assert!(offer_size > 0, "Offer size should be non-zero");

        // Total size should be sum of all types
        assert_eq!(counters.total_size(), account_size + offer_size);

        // Non-existent type should return 0
        assert_eq!(counters.size_for_type(LedgerEntryType::Trustline), 0);
    }

    #[test]
    fn test_entry_counters_merge() {
        let mut counters1 = BucketEntryCounters::new();
        counters1.record_entry(&BucketEntry::Liveentry(make_account_entry(1)));
        counters1.record_entry(&BucketEntry::Liveentry(make_account_entry(2)));

        let mut counters2 = BucketEntryCounters::new();
        counters2.record_entry(&BucketEntry::Liveentry(make_account_entry(3)));
        counters2.record_entry(&BucketEntry::Liveentry(make_offer_entry(1, 1)));
        counters2.record_entry(&BucketEntry::Deadentry(make_account_key(4)));

        let size1 = counters1.size_for_type(LedgerEntryType::Account);
        let size2 = counters2.size_for_type(LedgerEntryType::Account);

        counters1.merge(&counters2);

        assert_eq!(counters1.count_for_type(LedgerEntryType::Account), 3);
        assert_eq!(counters1.count_for_type(LedgerEntryType::Offer), 1);
        assert_eq!(counters1.total_dead(), 1);
        assert_eq!(
            counters1.size_for_type(LedgerEntryType::Account),
            size1 + size2
        );
        assert!(counters1.size_for_type(LedgerEntryType::Offer) > 0);
    }

    #[test]
    fn test_type_not_supported_offer() {
        assert!(
            LiveBucketIndex::type_not_supported(LedgerEntryType::Offer),
            "OFFER should be unsupported"
        );
        assert!(
            !LiveBucketIndex::type_not_supported(LedgerEntryType::Account),
            "Account should be supported"
        );
        assert!(
            !LiveBucketIndex::type_not_supported(LedgerEntryType::Trustline),
            "Trustline should be supported"
        );
        assert!(
            !LiveBucketIndex::type_not_supported(LedgerEntryType::ContractData),
            "ContractData should be supported"
        );
    }
}
