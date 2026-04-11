## Pseudocode: crates/bucket/src/lib.rs

"BucketList implementation for rs-stellar-core."

"The BucketList is Stellar's core data structure for storing ledger state."
"It consists of 11 levels (0-10), where each level contains two buckets:"
"  - curr: The current bucket being filled with new entries"
"  - snap: The snapshot bucket from the previous spill"

"Merge Semantics (CAP-0020):"
"  INIT + DEAD = annihilated (nothing output)"
"  DEAD + INIT = LIVE (recreation cancels tombstone)"
"  INIT + LIVE = INIT with new value"
"  LIVE + DEAD = DEAD (deletion shadows old value)"

### Module declarations

```
MODULES:
  applicator, bloom_filter, bucket, bucket_list,
  cache, disk_bucket, entry, error, eviction,
  future_bucket, hot_archive, index, index_persistence,
  iterator, live_iterator, manager, merge, merge_map,
  metrics, snapshot
```

### Protocol version constants

```
CONST FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY = 11
  // CAP-0020: INIT + META entries
CONST FIRST_PROTOCOL_SHADOWS_REMOVED = 12
  // CAP-0020 follow-up: bucket shadows removed
CONST FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION = 23
  // CAP-0046/Soroban: persistent entry eviction
```

### Public API (re-exports)

```
"Core bucket types"
  Bucket, BucketLevel, BucketList, BucketListStats,
  HasNextState, PendingMergeState,
  BUCKET_LIST_LEVELS, HAS_NEXT_STATE_CLEAR,
  HAS_NEXT_STATE_OUTPUT

"Disk-backed storage"
  DiskBucket, DiskBucketIter, DEFAULT_BLOOM_SEED

"Bloom filter"
  BucketBloomFilter, HashSeed, HASH_KEY_BYTES

"Entry types and comparison"
  compare_entries, compare_keys, get_ttl_key,
  is_persistent_entry, ledger_entry_data_type,
  ledger_entry_to_key, ledger_key_type, BucketEntry

"Error handling"
  BucketError

"Eviction (Soroban state archival)"
  bucket_update_period, level_half, level_should_spill,
  level_size, update_starting_eviction_iterator,
  EvictionCandidate, EvictionIterator, EvictionResult,
  ResolvedEviction, StateArchivalSettings,
  DEFAULT_EVICTION_SCAN_SIZE,
  DEFAULT_MAX_ENTRIES_TO_ARCHIVE,
  DEFAULT_STARTING_EVICTION_SCAN_LEVEL

"Bucket management"
  canonical_bucket_filename, BucketManager,
  BucketManagerStats

"Merge operations"
  merge_buckets, merge_buckets_to_file,
  merge_buckets_to_file,
  merge_buckets,
  merge_buckets,
  merge_in_memory, merge_multiple, MergeIterator

"Async bucket merging"
  FutureBucket, FutureBucketSnapshot,
  FutureBucketState, MergeKey

"Hot archive bucket list (Soroban state archival)"
  is_hot_archive_tombstone, merge_hot_archive_buckets,
  HotArchiveBucket, HotArchiveBucketLevel,
  HotArchiveBucketList, HotArchiveBucketListStats,
  FIRST_PROTOCOL_SUPPORTING_HOT_ARCHIVE,
  HOT_ARCHIVE_BUCKET_LIST_LEVELS

"Snapshots"
  BucketLevelSnapshot, BucketListSnapshot,
  BucketSnapshot, BucketSnapshotManager,
  HotArchiveBucketLevelSnapshot,
  HotArchiveBucketListSnapshot,
  HotArchiveBucketSnapshot, InflationWinner,
  SearchableBucketListSnapshot,
  SearchableHotArchiveBucketListSnapshot

"Advanced indexing"
  AssetPoolIdMap, BucketEntryCounters, DiskIndex,
  InMemoryIndex, LiveBucketIndex, RangeEntry,
  TypeRange, DEFAULT_INDEX_CUTOFF, DEFAULT_PAGE_SIZE

"Index persistence"
  cleanup_orphaned_indexes, delete_index,
  index_path_for_bucket, load_disk_index,
  save_disk_index, BUCKET_INDEX_VERSION

"Caching"
  CacheStats, RandomEvictionCache

"Merge deduplication"
  BucketMergeMap, LiveMergeFutures, MergeFuturesStats

"Bucket applicator (catchup)"
  ApplicatorCounters, BucketApplicator,
  EntryToApply, DEFAULT_CHUNK_SIZE

"Metrics and counters"
  BucketListMetrics, BucketListMetricsSnapshot,
  EntryCountType, EvictionCounters,
  EvictionCountersSnapshot, MergeCounters,
  MergeCountersSnapshot

"Streaming iterators"
  BucketInputIterator, BucketOutputIterator,
  FileMergeInput, MemoryMergeInput, MergeInput

"Live entries streaming iterator"
  LiveEntriesIterator, LiveEntriesStats
```

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 279    | 90         |
| Functions     | 0      | 0          |

NOTE: This is the crate root — module declarations, re-exports,
and protocol version constants. No function logic.
