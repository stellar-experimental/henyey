# stellar-core Parity Status

**Crate**: `henyey-bucket`
**Upstream**: `stellar-core/src/bucket/`
**Overall Parity**: 84%
**Last Updated**: 2026-04-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| BucketListBase spill math and level state | Full | Level sizing and spill sequencing match upstream |
| LiveBucketList and HotArchiveBucketList batching | Full | Live, archive, restore, and eviction flows present |
| Bucket and merge semantics | Full | CAP-0020 merge behavior implemented |
| BucketManager lifecycle and GC | Partial | Merge-future reattachment and metrics still incomplete |
| FutureBucket restart state | Full | HAS-compatible input/output hash snapshots supported |
| Snapshot and query APIs | Partial | Hot-archive refresh and scan helpers still missing |
| Live bucket indexing | Full | In-memory and page indexes, bloom filter, persistence |
| Hot archive indexing | Partial | Lookup exists, but no dedicated `HotArchiveBucketIndex` API |
| Bucket input/output iterators | Partial | Legacy gzip wrappers differ from upstream uncompressed iterators; malformed record marks now match `readOne` EOF/error semantics |
| Bucket applicator and live iteration | Full | Catchup application and live-entry scans implemented |
| Merge deduplication | Partial | Completed-merge cache wired; in-flight dedup tracker unused |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `BucketBase.h` / `BucketBase.cpp` | `bucket.rs`, `merge.rs` | Unified live bucket type plus merge helpers |
| `LiveBucket.h` / `LiveBucket.cpp` | `bucket.rs`, `merge.rs` | Live bucket creation, lookup, CAP-0020 merge rules |
| `HotArchiveBucket.h` / `HotArchiveBucket.cpp` | `hot_archive.rs` | Archived entry buckets and restore tombstones |
| `BucketListBase.h` / `BucketListBase.cpp` | `bucket_list.rs`, `eviction.rs` | Level math, spill logic, eviction helpers |
| `LiveBucketList.h` / `LiveBucketList.cpp` | `bucket_list.rs` | Live bucket-list batching and cache init |
| `HotArchiveBucketList.h` / `HotArchiveBucketList.cpp` | `hot_archive.rs` | Archive batching and restore markers |
| `BucketManager.h` / `BucketManager.cpp` | `manager.rs` | Bucket directory ownership, loading, GC, verification |
| `FutureBucket.h` / `FutureBucket.cpp` | `future_bucket.rs`, `bucket_list.rs` | Async merge state and HAS serialization |
| `BucketInputIterator.h` / `BucketInputIterator.cpp` | `iterator.rs` | Legacy gzip-based iterator, not the main production path |
| `BucketOutputIterator.h` / `BucketOutputIterator.cpp` | `iterator.rs` | Legacy gzip-based writer with dedup buffer |
| `BucketApplicator.h` / `BucketApplicator.cpp` | `applicator.rs` | Chunked catchup application |
| `BucketListSnapshot.h` / `BucketListSnapshot.cpp` | `snapshot.rs` | Live and hot-archive snapshots plus searchable views |
| `BucketSnapshotManager.h` / `BucketSnapshotManager.cpp` | `snapshot.rs` | Current and historical snapshot lifecycle |
| `LiveBucketIndex.h` / `LiveBucketIndex.cpp` | `index.rs`, `disk_bucket.rs` | Hybrid live index facade and disk lookup path |
| `DiskIndex.h` / `DiskIndex.cpp` | `index.rs`, `index_persistence.rs` | Page-based range index and persistence |
| `InMemoryIndex.h` / `InMemoryIndex.cpp` | `index.rs` | Per-key in-memory index for small buckets |
| `HotArchiveBucketIndex.h` / `HotArchiveBucketIndex.cpp` | `hot_archive.rs` | Lazy BTreeMap lookup only; no dedicated index type |
| `BucketIndexUtils.h` / `BucketIndexUtils.cpp` | `index.rs`, `index_persistence.rs`, `bloom_filter.rs` | Shared index helpers, bloom filter, persistence format |
| `BucketMergeMap.h` / `BucketMergeMap.cpp` | `merge_map.rs` | Completed merge cache and reverse mappings |
| `MergeKey.h` / `MergeKey.cpp` | `future_bucket.rs`, `merge_map.rs` | Merge identity hashing |
| `LedgerCmp.h` | `entry.rs` | Key and bucket-entry ordering |
| `BucketUtils.h` | `entry.rs`, `metrics.rs`, `eviction.rs` | Entry counters, eviction structs, merge counters |
| `BucketUtils.cpp` | `metrics.rs`, `eviction.rs`, `snapshot.rs` | Counter math and eviction-resolution helpers |
| `BucketMergeAdapter.h` | `iterator.rs` | Merge input adapters for file and memory sources |

## Component Mapping

### bucket core (`bucket.rs`, `merge.rs`)

Corresponds to: `BucketBase.h`, `LiveBucket.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BucketBase()` / `LiveBucket()` | `Bucket::empty()` | Full |
| `BucketBase(filename, hash, index)` | `Bucket::from_xdr_file_disk_backed()` | Full |
| `getHash()` / `getFilename()` / `getSize()` | `hash()` / `backing_file_path()` / `len()` | Full |
| `isEmpty()` / `isIndexed()` | `is_empty()` / `is_disk_backed()` | Full |
| `setIndex()` | `from_xdr_file_disk_backed_prebuilt()` | Full |
| `merge()` / `mergeInternal()` | `merge_buckets()` | Full |
| `randomBucketName()` | `canonical_bucket_filename()` | Full |
| `containsBucketIdentity()` | `Bucket::get()` | Full |
| `checkProtocolLegality()` | Protocol checks in merge paths | Full |
| `convertToBucketEntry()` | `BucketList::add_batch()` conversion | Full |
| `mergeCasesWithEqualKeys()` | `merge_entries()` | Full |
| `fresh()` / `freshInMemoryOnly()` | `from_entries()` / `fresh_in_memory_only()` | Full |
| `isTombstoneEntry()` | `BucketEntry::is_dead()` | Full |
| `bucketEntryToLoadResult()` | `Bucket::get_entry()` | Full |
| `mergeInMemory()` | `merge_in_memory()` | Full |
| `getBucketVersion()` / in-memory entry accessors | `protocol_version()` / `has_in_memory_entries()` / `get_in_memory_entries()` | Full |

### bucket list (`bucket_list.rs`, `eviction.rs`)

Corresponds to: `BucketListBase.h`, `LiveBucketList.h`, `HotArchiveBucketList.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `kNumLevels` | `BUCKET_LIST_LEVELS` / `HOT_ARCHIVE_BUCKET_LIST_LEVELS` | Full |
| `shouldMergeWithEmptyCurr()` | Inline spill logic | Full |
| `levelSize()` / `levelHalf()` | `level_size()` / `level_half()` | Full |
| `sizeOfCurr()` / `sizeOfSnap()` | `size_of_curr()` / `size_of_snap()` | Full |
| `oldestLedgerInCurr()` / `oldestLedgerInSnap()` | `oldest_ledger_in_curr()` / `oldest_ledger_in_snap()` | Full |
| `levelShouldSpill()` / `keepTombstoneEntries()` | `level_should_spill()` / inline checks | Full |
| `bucketUpdatePeriod()` | `bucket_update_period()` | Full |
| `BucketLevel::getHash/getNext/getCurr/getSnap` | `hash()` / `next()` / `curr()` / `snap()` | Full |
| `BucketLevel::setNext/setCurr/setSnap/commit` | `set_next()` / `set_curr()` / `set_snap()` / `commit()` | Full |
| `BucketLevel::prepare()` / `prepareFirstLevel()` | `prepare_with_normalization()` / `prepare_first_level()` | Full |
| `getHash()` / `getSize()` | `BucketList::hash()` / `size()` | Full |
| `restartMerges()` / `resolveAnyReadyFutures()` | `restart_merges_from_has()` / `resolve_ready_futures()` | Full |
| `futuresAllResolved()` / `getMaxMergeLevel()` | `futures_all_resolved()` / `calculate_skip_values()` | Full |
| `LiveBucketList::addBatch()` | `BucketList::add_batch()` | Full |
| `HotArchiveBucketList::addBatch()` | `HotArchiveBucketList::add_batch()` | Full |
| Eviction iterator helpers | `update_starting_eviction_iterator()` and incremental scan logic | Full |

### hot archive (`hot_archive.rs`)

Corresponds to: `HotArchiveBucket.h`, `HotArchiveBucketList.h`, `HotArchiveBucketIndex.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HotArchiveBucket()` | `HotArchiveBucket::empty()` | Full |
| `HotArchiveBucket(filename, hash, index)` | `from_xdr_file_disk_backed()` | Full |
| `getBucketVersion()` | `get_protocol_version()` | Full |
| `fresh()` | `HotArchiveBucket::fresh()` | Full |
| `isTombstoneEntry()` / `maybePut()` | `is_hot_archive_tombstone()` / merge helpers | Full |
| `mergeCasesWithEqualKeys()` | `merge_hot_archive_buckets()` | Full |
| `bucketEntryToLoadResult()` / `convertToBucketEntry()` | `get()` / `HotArchiveBucket::fresh()` conversion | Full |
| `countOldEntryType()` / `countNewEntryType()` / `checkProtocolLegality()` | No-ops consistent with upstream | Full |
| `HotArchiveBucketList::addBatch()` | `HotArchiveBucketList::add_batch()` | Full |
| `HotArchiveBucketIndex(...)` | Eager disk-backed BTreeMap lookup index | Partial |
| `HotArchiveBucketIndex::lookup()` | `HotArchiveBucket::get()` | Full |
| `HotArchiveBucketIndex::scan()` | *(none)* | None |
| `HotArchiveBucketIndex::getBucketEntryCounters()` | *(none)* | None |
| `HotArchiveBucketIndex::getPageSize()` | *(none)* | None |
| `HotArchiveBucketIndex::begin()` / `end()` | *(none)* | None |
| `HotArchiveBucketIndex::markBloomMiss()` | *(none)* | None |
| `BucketInputIterator<HotArchiveBucket>` metadata ext validation | `validate_hot_archive_metadata()` in load paths | Full |

### bucket manager (`manager.rs`)

Corresponds to: `BucketManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `create()` / `initialize()` | `BucketManager::new()` constructor path | Full |
| `bucketIndexFilename()` / `getBucketDir()` | `index_path_for_bucket()` / `bucket_dir()` | Full |
| `getBucketIfExists()` / `getBucketByHash()` | `bucket_exists()` / `load_bucket()` | Full |
| `adoptFileAsBucket()` / `noteEmptyMergeOutput()` | Temp promotion and empty-bucket handling in `merge()` | Full |
| `forgetUnreferencedBuckets()` | `retain_buckets()` | Full |
| `addLiveBatch()` / `addHotArchiveBatch()` | `BucketList::add_batch()` / `HotArchiveBucketList::add_batch()` | Full |
| `snapshotLedger()` / `assumeState()` | `snapshot_ledger()` / `assume_state()` | Full |
| `loadCompleteLedgerState()` / `loadCompleteHotArchiveState()` | Same-named Rust methods | Full |
| `mergeBuckets()` | `merge_all_buckets()` | Full |
| `visitLedgerEntries()` | `visit_ledger_entries()` / `_of_type()` | Full |
| `scheduleVerifyReferencedBucketsWork()` | `verify_referenced_bucket_hashes()` | Full |
| `startBackgroundEvictionScan()` / `resolveBackgroundEvictionScan()` | Snapshot-based eviction scan in `snapshot.rs` and `eviction.rs` | Full |
| `checkForMissingBucketsFiles()` / `getBucketListReferencedBuckets()` | `verify_buckets_exist()` / `all_referenced_hashes()` | Full |
| `getMergeFuture()` / `putMergeFuture()` | Completed-merge cache only; no manager-level future reattach | None |
| `getBloomMissMeter()` / `getBloomLookupMeter()` | No dedicated meter API | None |
| `getCacheHitMeter()` / `getCacheMissMeter()` | Cache stats exist, no meter API | Partial |
| `getMergeTimer()` | Merge counters only | Partial |
| `reportBucketEntryCountMetrics()` | Counter snapshots exist, reporting path partial | Partial |

### future bucket (`future_bucket.rs`)

Corresponds to: `FutureBucket.h`, `MergeKey.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `FutureBucket()` / `clear()` | `FutureBucket::clear()` | Full |
| Live merge constructor | `FutureBucket::start_merge()` | Full |
| `isLive()` / `isMerging()` / `isClear()` | Same-named Rust methods | Full |
| `hasHashes()` / `hasOutputHash()` / `getOutputHash()` | `has_hashes()` / `output_hash()` | Full |
| `mergeComplete()` / `resolve()` | `merge_complete()` / `resolve()` / `resolve_blocking()` | Full |
| `makeLive()` | `make_live()` | Full |
| `getHashes()` | `get_hashes()` | Full |
| cereal `load()` / `save()` plus `MergeKey` hashing | `FutureBucketSnapshot` serde plus `MergeKey` | Full |

### iterators (`iterator.rs`)

Corresponds to: `BucketInputIterator.h`, `BucketOutputIterator.h`, `BucketMergeAdapter.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BucketInputIterator(bucket)` | `BucketInputIterator::open()` | Partial |
| `operator bool()` | `has_next()` | Full |
| `seenMetadata()` / `getMetadata()` | `seen_metadata()` / `metadata()` | Full |
| `operator*()` / `operator++()` | `peek()` / `next_entry()` | Full |
| `pos()` / `size()` / `seek()` | `bytes_read()` only | Partial |
| `BucketOutputIterator(...)` | `BucketOutputIterator::new()` | Partial |
| `put()` | `put()` | Full |
| `getBucket()` | `finish()` returns path/hash/entries, not adopted bucket | Partial |
| `MergeInput` / `FileMergeInput` / `MemoryMergeInput` | Same-named Rust trait and structs | Full |

Record-marked XDR readers now distinguish clean EOF from malformed input in
line with `XDRInputFileStream::readOne`: live buckets, disk-backed live bucket
iterators/lookups, hot archive bucket readers, and legacy gzip input iteration
reject partial trailing record marks, truncated bodies, and XDR decode failures.
Record lengths preserve stellar-core's high-bit behavior by masking the top bit
rather than requiring it. `DiskBucket::scan_page_for_key` also returns errors
for truncated or undecodable records reached through mmap-backed page scans;
this is a defensive corruption check for henyey's index path rather than a
claim of byte-for-byte `readPage` equivalence.

### snapshots (`snapshot.rs`)

Corresponds to: `BucketListSnapshot.h`, `BucketSnapshotManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BucketListSnapshotData(bl, header)` | `BucketListSnapshot::new()` | Full |
| `BucketListSnapshotData::getLedgerSeq()` | `ledger_seq()` | Full |
| `SearchableBucketListSnapshot::load()` | `SearchableBucketListSnapshot::load()` | Full |
| `loadKeysFromLedger()` | Same-named Rust method | Full |
| `getLedgerSeq()` / `getLedgerHeader()` | `ledger_seq()` / `ledger_header()` | Full |
| `getSnapshotData()` / `getHistoricalSnapshots()` | Accessible via snapshot struct fields | Full |
| `SearchableLiveBucketListSnapshot::loadKeys()` | `load_keys()` | Full |
| `loadPoolShareTrustLinesByAccountAndAsset()` | Same-named Rust method | Full |
| `loadInflationWinners()` | Same-named Rust method | Full |
| `scanForEviction()` / `scanForEvictionInBucket()` | Same-named Rust methods | Full |
| `scanForEntriesOfType()` | Same-named Rust method | Full |
| `SearchableHotArchiveBucketListSnapshot::loadKeys()` | `load_keys()` | Full |
| `SearchableHotArchiveBucketListSnapshot::scanAllEntries()` | *(none)* | None |
| `BucketSnapshotManager(ctor)` | `BucketSnapshotManager::new()` | Full |
| `updateCurrentSnapshot()` | Same-named Rust method | Full |
| `copySearchableLiveBucketListSnapshot()` | `copy_searchable_live_snapshot()` | Full |
| `copySearchableHotArchiveBucketListSnapshot()` | `copy_searchable_hot_archive_snapshot()` | Full |
| `copySearchableBucketListSnapshots()` | `copy_live_and_hot_archive_snapshots()` | Full |
| `maybeCopySearchableBucketListSnapshot()` | `maybe_update_live_snapshot()` | Full |
| `maybeCopySearchableHotArchiveBucketListSnapshot()` | *(none)* | None |
| `maybeCopyLiveAndHotArchiveSnapshots()` | `copy_live_and_hot_archive_snapshots()` re-query pattern | Full |

### indexing and application (`index.rs`, `index_persistence.rs`, `applicator.rs`, `entry.rs`, `metrics.rs`, `merge_map.rs`, `bloom_filter.rs`, `cache.rs`)

Corresponds to: `LiveBucketIndex.h`, `DiskIndex.h`, `InMemoryIndex.h`, `BucketIndexUtils.h`, `BucketApplicator.h`, `BucketMergeMap.h`, `LedgerCmp.h`, `BucketUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LiveBucketIndex::lookup()` / `scan()` | `DiskBucket::get()` plus index page lookup helpers | Full |
| `getPoolIDsByAsset()` / cache init / type ranges / counters | `get_pools_for_asset()` / `maybe_initialize_caches()` / `type_range()` / `counters()` | Full |
| `DiskIndex::getOffsetBounds()` / `saveToDisk()` / preload | `find_page_for_key()` / `save_disk_index()` / `load_disk_index()` | Full |
| `InMemoryBucketState::insert()` / `scan()` | `InMemoryIndex::from_entries()` / `get_offset()` | Full |
| `createIndex()` / `loadIndex()` / version checks | `LiveBucketIndex::from_entries*()` / `load_disk_index()` | Full |
| `BucketApplicator` and `Counters` | `BucketApplicator` and `ApplicatorCounters` | Full |
| `BucketMergeMap` public API | `record_merge()` / `forget_all_merges_producing()` / `get_output()` / `get_outputs_for_input()` | Full |
| `LedgerEntryIdCmp` / `BucketEntryIdCmp` | `compare_keys()` / `compare_entries()` | Full |
| `MergeCounters` / `BucketEntryCounters` | Same-named Rust structs | Full |
| `EvictionResultEntry` / `EvictionResultCandidates` / `EvictedStateVectors` | `EvictionCandidate` / `EvictionResult` / `ResolvedEviction` | Full |
| `EvictionMetrics` / `EvictionStatistics::recordEvictedEntry()` | `EvictionCounters` / eviction stats | Full |
| `EvictionStatistics::submitMetricsAndRestartCycle()` | Simplified eviction statistics only | Partial |
| Binary fuse filter and random-eviction cache | `BucketBloomFilter` / `RandomEvictionCache` | Full |
| `updateTypeBoundaries()` / `buildTypeRangesMap()` | Type range tracking in index construction | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `BucketMergeAdapter.h` template layering | Rust traits and generics remove the C++ adapter pattern |
| Shadow-bucket merge semantics before protocol 12 | Repository targets protocol 24+ only |
| `TmpDirManager` and lockfile plumbing | Rust uses direct tempdir handling instead of the C++ helper hierarchy |
| `NonMovableOrCopyable` base classes | Rust ownership already enforces the same constraint |
| `BUILD_TESTS` conditional interfaces | Rust uses `#[cfg(test)]` |
| `AppConnector` service threading | Rust passes concrete dependencies directly |
| Medida-specific type names | Rust uses local counters and tracing instead of the library API |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `BucketManager::getMergeFuture()` / `putMergeFuture()` | High | No manager-level reattachment to in-flight merges |
| `HotArchiveBucketIndex` scan, counters, page-size, iterator, bloom API | Medium | Only lazy point lookup exists today |
| `BucketSnapshotManager::maybeCopySearchableHotArchiveBucketListSnapshot()` | Medium | Live snapshot refresher exists; hot archive equivalent does not |
| `SearchableHotArchiveBucketListSnapshot::scanAllEntries()` | Medium | Hot archive full-scan iteration not implemented |
| `BucketInputIterator` / `BucketOutputIterator` production parity | Medium | Legacy gzip wrappers differ from upstream uncompressed iterators |
| `BucketManager` meter accessors and merge timer parity | Low | Counter snapshots exist but not the public meter API |
| `BucketManager::reportBucketEntryCountMetrics()` | Low | Entry counters are collected but not fully published |
| `EvictionStatistics::submitMetricsAndRestartCycle()` | Low | Current stats are simplified and less structured |

## Architectural Differences

1. **Unified live bucket representation**
   - **stellar-core**: `LiveBucket` inherits from `BucketBase<LiveBucket, LiveBucketIndex>` and keeps the index behind the bucket object.
   - **Rust**: `Bucket` wraps in-memory and disk-backed storage modes behind one enum-backed type.
   - **Rationale**: The Rust design keeps the hot path simpler while preserving the same merge and lookup semantics.

2. **Async merge execution**
   - **stellar-core**: `FutureBucket` wraps `std::shared_future` work scheduled through the application's async infrastructure.
   - **Rust**: Higher-level merges use `tokio::task::spawn_blocking` and serialize via `FutureBucketSnapshot`.
   - **Rationale**: Tokio provides equivalent background execution without reproducing the C++ scheduler stack.

3. **Hot archive indexing**
   - **stellar-core**: Hot archive buckets expose a dedicated `HotArchiveBucketIndex` built on the same disk-index machinery as live buckets.
   - **Rust**: `HotArchiveBucket` eagerly builds a BTreeMap for point lookups and does not expose a standalone index type.
   - **Rationale**: Current users only need point lookup, but this leaves parity gaps for scan, counters, and metrics.

4. **Iterator path split**
   - **stellar-core**: `BucketInputIterator` and `BucketOutputIterator` are the main streaming primitives used by merge and catchup paths.
   - **Rust**: Mainline merge and lookup paths use `DiskBucket` streaming on uncompressed `.bucket.xdr`, while `iterator.rs` retains older gzip-based wrappers.
   - **Rationale**: Production performance work moved to the disk-bucket path first, leaving the legacy iterator API behind.

5. **Merge deduplication scope**
   - **stellar-core**: `BucketManager` deduplicates both finished merges and merges still running.
   - **Rust**: `BucketMergeMap` is wired for completed merges, but `LiveMergeFutures` is not yet used in the manager path.
   - **Rationale**: Completed-merge reuse covers restart and replay reuse, but concurrent reattachment parity is still missing.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| `BucketTests.cpp` | 8 TEST_CASE / 13 SECTION | ~79 `#[test]` | Merge rules, bucket encoding, hot archive behavior |
| `BucketListTests.cpp` | 17 TEST_CASE / 23 SECTION | ~29 `#[test]` | Spill scheduling, eviction scans, snapshots |
| `BucketManagerTests.cpp` | 11 TEST_CASE / 0 SECTION | ~30 `#[test]` | Lifecycle, cleanup, persistence, HAS restart |
| `BucketIndexTests.cpp` | 13 TEST_CASE / 10 SECTION | ~56 `#[test]` | In-memory index, disk index, cache, persistence |
| `BucketMergeMapTests.cpp` | 1 TEST_CASE / 0 SECTION | 13 `#[test]` | Completed-merge bookkeeping |
| Other (applicator, entry, future, metrics, iterator) | — | ~41 `#[test]` | Applicator, entry comparison, iterator tests |
| **Total** | **50 TEST_CASE / 46 SECTION** | **~248 `#[test]`** | Rust coverage is broader but still misses a few upstream scenarios |

### Test Gaps

- In-progress merge reattachment after restart is still thinner than upstream's `bucketmanager reattach to running merge` coverage.
- `maxEntriesToArchive` and eviction-cycle metric behavior from `BucketListTests.cpp` are only partially mirrored.
- No dedicated tests exercise a full `HotArchiveBucketIndex`-style scan or counter API because the Rust API does not exist yet.
- No tests exercise `scanAllEntries` on hot archive snapshots because the method does not exist yet.
- Legacy `iterator.rs` tests cover gzip wrappers, not parity with the uncompressed production bucket format used elsewhere in the crate.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 95 |
| Gaps (None + Partial) | 18 |
| Intentional Omissions | 7 |
| **Parity** | **95 / (95 + 18) = 84%** |
