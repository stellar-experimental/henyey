//! BucketList implementation for rs-stellar-core.
//!
//! The BucketList is Stellar's core data structure for storing ledger state.
//! It provides:
//!
//! - Efficient incremental updates as ledgers close
//! - Merkle tree structure for integrity verification
//! - Hierarchical organization with multiple levels
//! - Support for live entries, dead entries, and init entries
//!
//! # Structure
//!
//! The BucketList consists of 11 levels (0-10), where each level contains two buckets:
//! - `curr`: The current bucket being filled with new entries
//! - `snap`: The snapshot bucket from the previous spill
//!
//! Lower levels update more frequently, while higher levels contain older data
//! and update less often (similar to a log-structured merge tree). This design
//! optimizes for append-heavy workloads while maintaining efficient lookups.
//!
//! # Spill Frequency
//!
//! Levels spill on a schedule derived from their size and half-size boundaries:
//!
//! | Level | Size    | Half    | Spill Period |
//! |-------|---------|---------|--------------|
//! | 0     | 4       | 2       | 2 ledgers    |
//! | 1     | 16      | 8       | 8 ledgers    |
//! | 2     | 64      | 32      | 32 ledgers   |
//! | ...   | ...     | ...     | ...          |
//!
//! This matches stellar-core's `BucketListBase::levelShouldSpill` logic.
//!
//! # Entry Types
//!
//! Bucket entries come in four types, each with specific merge semantics:
//!
//! - [`BucketEntry::Live`]: A live ledger entry (the current state)
//! - [`BucketEntry::Dead`]: A tombstone marking deletion (shadows older entries)
//! - [`BucketEntry::Init`]: Like Live but with CAP-0020 merge semantics
//! - [`BucketEntry::Metadata`]: Bucket metadata (protocol version, bucket list type)
//!
//! # Merge Semantics (CAP-0020)
//!
//! When buckets are merged, entries interact according to these rules:
//!
//! - `INIT + DEAD` = Both annihilated (nothing output)
//! - `DEAD + INIT` = `LIVE` (recreation cancels tombstone)
//! - `INIT + LIVE` = `INIT` with new value (preserves init status)
//! - `LIVE + DEAD` = `DEAD` (deletion shadows old value)
//!
//! # Eviction (Soroban State Archival)
//!
//! For Soroban entries (ContractData, ContractCode), this crate provides
//! incremental eviction scanning. Expired entries are either:
//!
//! - **Temporary entries**: Deleted immediately
//! - **Persistent entries**: Archived to the hot archive bucket list
//!
//! See [`EvictionIterator`] and [`StateArchivalSettings`] for details.
//!
//! # Example
//!
//! ```ignore
//! use henyey_bucket::{BucketList, BucketManager};
//! use stellar_xdr::curr::BucketListType;
//!
//! // Create a bucket manager for disk storage
//! let manager = BucketManager::new("/tmp/buckets".into())?;
//!
//! // Create a new bucket list
//! let mut bucket_list = BucketList::new();
//!
//! // Add entries from a closed ledger
//! bucket_list.add_batch(
//!     1,                          // ledger sequence
//!     protocol_version,           // protocol version
//!     BucketListType::Live,       // live vs hot archive
//!     init_entries,               // newly created entries
//!     live_entries,               // updated entries
//!     dead_entries,               // deleted entries
//! )?;
//!
//! // Look up an entry by key
//! if let Some(entry) = bucket_list.get(&key)? {
//!     // Use the entry
//! }
//!
//! // Get the bucket list hash for verification
//! let hash = bucket_list.hash();
//! ```

mod applicator;
mod bloom_filter;
mod bucket;
mod bucket_list;
mod cache;
mod disk_bucket;
mod entry;
mod error;
mod eviction;
mod future_bucket;
mod hot_archive;
mod index;
mod index_persistence;
mod iterator;
mod live_iterator;
mod manager;
mod merge;
mod merge_map;
mod metrics;
pub mod snapshot;

// ============================================================================
// Core bucket types
// ============================================================================

pub use bucket::Bucket;
pub use bucket_list::{
    BucketLevel, BucketList, BucketListStats, HasNextState, PendingMergeState,
    BUCKET_LIST_LEVELS, HAS_NEXT_STATE_CLEAR, HAS_NEXT_STATE_OUTPUT,
};

// ============================================================================
// Disk-backed storage
// ============================================================================

pub use disk_bucket::{DiskBucket, DiskBucketIter, DEFAULT_BLOOM_SEED};

// ============================================================================
// Bloom filter for fast negative lookups
// ============================================================================

pub use bloom_filter::{BucketBloomFilter, HashSeed, HASH_KEY_BYTES};

// ============================================================================
// Entry types and comparison
// ============================================================================

pub use entry::{
    compare_entries, compare_keys, get_ttl_key, get_ttl_live_until, is_persistent_entry,
    is_persistent_key, is_soroban_entry, is_soroban_key, is_temporary_entry, is_ttl_expired,
    ledger_entry_data_type, ledger_entry_to_key, ledger_key_type, BucketEntry,
};

// ============================================================================
// Error handling
// ============================================================================

pub use error::BucketError;

// ============================================================================
// Eviction (Soroban state archival)
// ============================================================================

pub use eviction::{
    bucket_update_period, level_half, level_should_spill, level_size,
    update_starting_eviction_iterator, EvictionCandidate, EvictionIterator, EvictionResult,
    ResolvedEviction, StateArchivalSettings,
    DEFAULT_EVICTION_SCAN_SIZE, DEFAULT_MAX_ENTRIES_TO_ARCHIVE, DEFAULT_STARTING_EVICTION_SCAN_LEVEL,
};

// ============================================================================
// Bucket management
// ============================================================================

pub use manager::{canonical_bucket_filename, BucketManager, BucketManagerStats};

// ============================================================================
// Merge operations
// ============================================================================

pub use merge::{
    merge_buckets, merge_buckets_to_file, merge_buckets_to_file_with_counters,
    merge_buckets_with_options, merge_buckets_with_options_and_shadows_and_counters,
    merge_in_memory, merge_multiple, MergeIterator,
};

// ============================================================================
// Async bucket merging
// ============================================================================

pub use future_bucket::{FutureBucket, FutureBucketSnapshot, FutureBucketState, MergeKey};

// ============================================================================
// Hot archive bucket list (Soroban state archival)
// ============================================================================

pub use hot_archive::{
    is_hot_archive_tombstone, merge_hot_archive_buckets, HotArchiveBucket, HotArchiveBucketLevel,
    HotArchiveBucketList, HotArchiveBucketListStats, FIRST_PROTOCOL_SUPPORTING_HOT_ARCHIVE,
    HOT_ARCHIVE_BUCKET_LIST_LEVELS,
};

// ============================================================================
// Snapshots (thread-safe concurrent access)
// ============================================================================

pub use snapshot::{
    BucketLevelSnapshot, BucketListSnapshot, BucketSnapshot, BucketSnapshotManager,
    HotArchiveBucketLevelSnapshot, HotArchiveBucketListSnapshot, HotArchiveBucketSnapshot,
    InflationWinner, SearchableBucketListSnapshot, SearchableHotArchiveBucketListSnapshot,
};

// ============================================================================
// Advanced indexing
// ============================================================================

pub use index::{
    AssetPoolIdMap, BucketEntryCounters, DiskIndex, InMemoryIndex, LiveBucketIndex, RangeEntry,
    TypeRange, DEFAULT_INDEX_CUTOFF, DEFAULT_PAGE_SIZE,
};

// ============================================================================
// Index persistence
// ============================================================================

pub use index_persistence::{
    cleanup_orphaned_indexes, delete_index, index_path_for_bucket, load_disk_index, save_disk_index,
    BUCKET_INDEX_VERSION,
};

// ============================================================================
// Caching
// ============================================================================

pub use cache::{CacheStats, RandomEvictionCache};

// ============================================================================
// Merge deduplication
// ============================================================================

pub use merge_map::{BucketMergeMap, LiveMergeFutures, MergeFuturesStats};

// ============================================================================
// Bucket applicator (catchup)
// ============================================================================

pub use applicator::{ApplicatorCounters, BucketApplicator, EntryToApply, DEFAULT_CHUNK_SIZE};

// ============================================================================
// Metrics and counters
// ============================================================================

pub use metrics::{
    BucketListMetrics, BucketListMetricsSnapshot, EntryCountType, EvictionCounters,
    EvictionCountersSnapshot, MergeCounters, MergeCountersSnapshot,
};

// ============================================================================
// Streaming iterators
// ============================================================================

pub use iterator::{
    BucketInputIterator, BucketOutputIterator, FileMergeInput, MemoryMergeInput, MergeInput,
};

// ============================================================================
// Live entries streaming iterator
// ============================================================================

pub use live_iterator::{LiveEntriesIterator, LiveEntriesStats};

// ============================================================================
// Protocol version constants
// ============================================================================

/// First protocol version supporting INITENTRY and METAENTRY (CAP-0020).
pub const FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY: u32 = 11;
/// First protocol version where bucket shadows are removed (CAP-0020 follow-up).
pub const FIRST_PROTOCOL_SHADOWS_REMOVED: u32 = 12;

/// First protocol version supporting persistent eviction (CAP-0046/Soroban).
pub const FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION: u32 = 23;

/// Result type for bucket operations.
///
/// This is a convenience alias for `std::result::Result<T, BucketError>`.
pub type Result<T> = std::result::Result<T, BucketError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BucketEntry;
    use stellar_xdr::curr::*; // Re-import to shadow XDR's BucketEntry

    const TEST_PROTOCOL: u32 = 25;

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_account_entry(bytes: [u8; 32], balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(bytes),
                balance,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_key(bytes: [u8; 32]) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(bytes),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_integration_bucket_list_with_manager() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = BucketManager::new(temp_dir.path().to_path_buf()).unwrap();

        // Create entries
        let entries = vec![
            BucketEntry::Live(make_account_entry([1u8; 32], 100)),
            BucketEntry::Live(make_account_entry([2u8; 32], 200)),
        ];

        // Create bucket through manager
        let bucket = manager.create_bucket(entries).unwrap();
        assert_eq!(bucket.len(), 2);

        // Verify bucket is on disk
        assert!(manager.bucket_exists(&bucket.hash()));

        // Load bucket
        manager.clear_cache();
        let loaded = manager.load_bucket(&bucket.hash()).unwrap();
        assert_eq!(loaded.hash(), bucket.hash());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_integration_full_workflow() {
        // Create a bucket list and add entries over multiple ledgers
        let mut bucket_list = BucketList::new();

        // Add entries for ledgers 1-10
        for i in 1..=10u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 100);
            bucket_list
                .add_batch(
                    i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }

        // Verify all entries are accessible
        for i in 1..=10u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            let entry = bucket_list.get(&key).unwrap().unwrap();
            if let LedgerEntryData::Account(account) = &entry.data {
                assert_eq!(account.balance, i as i64 * 100);
            }
        }

        // Update some entries
        for i in 1..=5u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 1000);
            bucket_list
                .add_batch(
                    10 + i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![],
                    vec![entry],
                    vec![],
                )
                .unwrap();
        }

        // Verify updates
        for i in 1..=5u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            let entry = bucket_list.get(&key).unwrap().unwrap();
            if let LedgerEntryData::Account(account) = &entry.data {
                assert_eq!(account.balance, i as i64 * 1000);
            }
        }

        // Delete some entries
        for i in 6..=8u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            bucket_list
                .add_batch(
                    10 + i,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![],
                    vec![],
                    vec![key],
                )
                .unwrap();
        }

        // Verify deletions
        for i in 6..=8u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            assert!(bucket_list.get(&key).unwrap().is_none());
        }

        // Remaining entries should still exist
        for i in [9u32, 10] {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            assert!(bucket_list.get(&key).unwrap().is_some());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_constants() {
        assert_eq!(BUCKET_LIST_LEVELS, 11);
        assert_eq!(BucketList::NUM_LEVELS, 11);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_entry_types() {
        let entry = make_account_entry([1u8; 32], 100);
        let key = make_account_key([1u8; 32]);

        let live = BucketEntry::Live(entry.clone());
        assert!(live.is_live());
        assert!(!live.is_dead());
        assert!(!live.is_init());
        assert!(!live.is_metadata());

        let dead = BucketEntry::Dead(key);
        assert!(!dead.is_live());
        assert!(dead.is_dead());
        assert!(!dead.is_init());

        let init = BucketEntry::Init(entry);
        assert!(!init.is_live());
        assert!(!init.is_dead());
        assert!(init.is_init());
    }
}
