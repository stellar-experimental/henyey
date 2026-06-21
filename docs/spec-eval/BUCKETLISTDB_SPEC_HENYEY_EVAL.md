# Henyey Bucket Crate — Specification Adherence Evaluation

**Evaluated against:** `stellar-specs/BUCKETLISTDB_SPEC.md` (stellar-core v26.0.1 BucketListDB reference)
**stellar-core pin:** v27.0.0 — evaluation body reflects the v26.0.1 baseline; Protocol 27 delta re-eval pending.
**Crate:** `crates/bucket/` (henyey-bucket)
**Date:** 2026-05-10

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Evaluation Methodology](#2-evaluation-methodology)
3. [Section-by-Section Evaluation](#3-section-by-section-evaluation)
   - [§1 Introduction](#31-introduction)
   - [§2 Architecture Overview](#32-architecture-overview)
   - [§3 Data Types & Encoding](#33-data-types--encoding)
   - [§4 BucketList Structure](#34-bucketlist-structure)
   - [§5 Bucket Lifecycle](#35-bucket-lifecycle)
   - [§6 Merge Algorithm](#36-merge-algorithm)
   - [§7 Async Merge Management](#37-async-merge-management)
   - [§8 BucketManager](#38-bucketmanager)
   - [§9 Indexing](#39-indexing)
   - [§10 Snapshot & Query Layer](#310-snapshot--query-layer)
   - [§11 Hot Archive BucketList](#311-hot-archive-bucketlist)
   - [§12 Eviction](#312-eviction)
   - [§13 Catchup & State Reconstruction](#313-catchup--state-reconstruction)
   - [§14 Serialization & Persistence](#314-serialization--persistence)
   - [§15 Invariants & Safety Properties](#315-invariants--safety-properties)
   - [§16 Constants](#316-constants)
   - [§17 References](#317-references)
   - [§18 Appendices](#318-appendices)
4. [Gap Summary](#4-gap-summary)
5. [Risk Assessment](#5-risk-assessment)
6. [v26.0.1 Implementation Delta](#6-v2601-implementation-delta)
7. [Recommendations](#7-recommendations)

---

## 1. Executive Summary

The henyey bucket crate implements the BucketListDB — the core state storage structure for Stellar's ledger. It manages two append-only bucket lists (live and hot archive), each with 11 levels, providing deterministic ledger state snapshots, background merges, indexed point lookups, and state eviction for Soroban entries.

The bucket crate is at **84% function-level parity** (see `crates/bucket/PARITY_STATUS.md` for current counts). The core data structures, merge algorithm, indexing system, and query layer are fully implemented. The primary gaps are in BucketManager's merge future deduplication cache and some operational metrics.

### Overall Adherence Rating

| Category | Rating | Notes |
|----------|--------|-------|
| **Architecture Overview** | **Full** | Clean module separation, proper layering of concerns |
| **Data Types & Encoding** | **Full** | XDR types, BucketEntry variants, sort order all correct |
| **BucketList Structure** | **Full** | 11 levels, level size formulas, curr/snap/next structure, hash computation |
| **Bucket Lifecycle** | **Full** | Create, populate, finalize flow; in-memory and disk-backed buckets |
| **Merge Algorithm** | **Full** | Two-pointer sorted merge, CAP-0020 INITENTRY rules, shadow cursor support |
| **Async Merge Management** | **High** | Async merges via tokio spawn_blocking; FutureBucket 5-state machine; in-flight merge dedup not yet implemented |
| **BucketManager** | **Medium** | Core bucket management works; completed-merge cache wired; in-flight `getMergeFuture()`/`putMergeFuture()` not implemented; some metrics missing |
| **Indexing** | **Full** | DiskIndex (page-based + bloom filter), InMemoryIndex, index persistence |
| **Snapshot & Query Layer** | **Full** | BucketListSnapshot with Arc-based cloning, BucketSnapshotManager with RwLock, full query API |
| **Hot Archive BucketList** | **Full** | 11 levels, proper ARCHIVED/LIVE merge semantics, V1 metadata |
| **Eviction** | **Full** | EvictionIterator, TTL-based scanning, state archival settings |
| **Catchup & State Reconstruction** | **Full** | BucketApplicator with chunked processing, dedup via seen keys |
| **Serialization & Persistence** | **High** | FutureBucket serialization works; BucketMergeMap caches completed merges; in-flight dedup not yet implemented |
| **Invariants & Safety Properties** | **Full** | Key safety properties enforced (sort order, merge correctness, hash determinism) |
| **Constants** | **Full** | All protocol constants match spec |
| **Metrics & Observability** | **Low** | Merge counters present; bloom miss meters, eviction cycle metrics missing |

**Estimated behavioral coverage: ~91%** of spec-defined behavior is implemented. The remaining gaps fall into three categories: (1) 1 moderate gap in merge future deduplication, (2) ~5 minor functional gaps, and (3) ~5 metrics/observability gaps that do not affect correctness.

---

## 2. Evaluation Methodology

This evaluation compares the henyey bucket implementation against the `BUCKETLISTDB_SPEC.md` specification derived from stellar-core v26.0.1. All key source files in `crates/bucket/src/` were read and compared against the spec requirements.

Each behavior is assessed on three dimensions:

1. **Structural completeness**: Are the required data structures, abstractions, and state machines present?
2. **Behavioral correctness**: Do the implementations follow the same algorithms, state transitions, and edge case handling?
3. **Constant fidelity**: Do hardcoded values, thresholds, and protocol version gates match?

Ratings per requirement:

| Symbol | Meaning |
|--------|---------|
| ✅ | Fully implemented and matches spec |
| ⚠️ | Partially implemented or minor deviation |
| ❌ | Not implemented |
| ➖ | Not applicable (protocol 24+ only or intentional departure) |

Source file references use the format `file.rs:line`.

**Note on scope:** Henyey targets protocol 24+ only. Shadow buckets (removed in protocol 12) and pre-protocol-11 INITENTRY behavior are not applicable.

**Note on v26.0.1 changes:** stellar-core v26.0.1 introduced significant concurrency/race fixes, a snapshot refactor to a single `BucketListSnapshot` class, and an XDR offset bug fix for old protocols. These changes are evaluated for parity impact in [§6 v26.0.1 Implementation Delta](#6-v2601-implementation-delta).

---

## 3. Section-by-Section Evaluation

### 3.1 Introduction

**Spec §1** provides background and motivation for the BucketListDB design. No implementation requirements.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Informational context only | ➖ | No implementation needed |

---

### 3.2 Architecture Overview

**Source files:** `lib.rs`, `bucket_list.rs`, `snapshot.rs`, `index.rs`
**Spec §2:** High-level architecture — BucketList as append-only LSM-tree, layered with indexing, snapshotting, and query access.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| BucketList as append-only sorted-merge structure | ✅ | `bucket_list.rs`: BucketList with 11 levels, sorted bucket entries, merge-on-spill |
| Separation of mutable bucket list from immutable snapshots | ✅ | `snapshot.rs`: BucketSnapshot (immutable), BucketSnapshotManager manages snapshot lifecycle |
| Index layer for point lookups without full scan | ✅ | `index.rs`: DiskIndex with page-based lookup + bloom filter; InMemoryIndex for small buckets |
| BucketManager as central coordinator | ✅ | `bucket_manager.rs` and module exports; manages bucket creation, adoption, temp directory |
| Clear module boundaries | ✅ | `lib.rs`: 21 submodules with well-defined responsibilities |

**Assessment: Full adherence.** The architecture cleanly maps to the spec's layered design. The Rust module structure provides strong separation of concerns matching the spec's component decomposition.

---

### 3.3 Data Types & Encoding

**Source files:** `entry.rs`, `lib.rs`, `merge.rs`
**Spec §3:** BucketEntry XDR types, entry variants (LIVEENTRY, DEADENTRY, INITENTRY), sort order, key comparison.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| BucketEntry type with LIVEENTRY, DEADENTRY, INITENTRY variants | ✅ | Uses `stellar-xdr` crate's `BucketEntry` type directly |
| BucketMetadata entry at position 0 of every bucket | ✅ | `merge.rs`: Metadata written as first entry; `iterator.rs`: metadata validated on read |
| INITENTRY semantics (CAP-0020) — marks entry creation boundary | ✅ | `merge.rs`: Full INITENTRY merge rules implemented |
| DEADENTRY semantics — tombstone for deleted entries | ✅ | `merge.rs`: DEAD entries properly handled in merge |
| Entry sort order: type → LedgerKey deterministic ordering | ✅ | `entry.rs`: `compare_keys()` and `ledger_entry_to_key()` implement proper ordering |
| Shadow entries (pre-protocol-12) | ➖ | Shadows removed in protocol 12; henyey targets protocol 24+. Shadow cursor code exists in `merge.rs` for completeness but is effectively dead code |
| BucketMetadata version field (V0 vs V1) | ✅ | `lib.rs`: Protocol constants; V1 metadata includes `BucketListType` discriminant (LIVE vs HOT_ARCHIVE) |
| XDR serialization of bucket entries | ✅ | Uses `stellar-xdr` crate for all serialization/deserialization |

**Assessment: Full adherence.** All data types and encoding requirements match the spec. The stellar-xdr crate ensures wire-compatible encoding.

---

### 3.4 BucketList Structure

**Source files:** `bucket_list.rs`
**Spec §4:** 11 levels (0–10), each with curr and snap buckets, level size doubling, spill mechanics, hash computation.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| 11 levels (0–10) | ✅ | `bucket_list.rs`: `NUM_LEVELS = 11`, `BucketLevel` struct array |
| Each level has `curr` and `snap` buckets | ✅ | `bucket_list.rs`: `BucketLevel { curr, snap, next }` |
| Level 0 size = 2 ledgers, each subsequent level 2× larger | ✅ | `bucket_list.rs`: Level size formulas match spec (`level_size(i) = 2^(2*(i+1))` for age boundaries) |
| Spill trigger: level `i` spills every `2^(2i)` ledgers | ✅ | `bucket_list.rs`: `level_should_spill()` implements correct spill frequency |
| Spill mechanics: old curr becomes new snap, new curr = merge(old snap, old next level curr) | ✅ | `bucket_list.rs`: `prepare_merge_for_level_above()` and spill logic in `add_batch()` |
| Fresh entries added to level 0 curr via merge with existing | ✅ | `bucket_list.rs`: `add_batch()` adds new entries at level 0 |
| Level 0 uses in-memory merge optimization | ✅ | `bucket_list.rs`: `PendingMerge::InMemory` used for level 0 merges |
| BucketList hash = SHA256(level_0_hash ∥ level_1_hash ∥ ... ∥ level_10_hash) | ✅ | `bucket_list.rs`: `get_hash()` computes SHA256 over concatenated level hashes |
| Level hash = SHA256(curr_hash ∥ snap_hash) | ✅ | `bucket_list.rs`: `BucketLevel::get_hash()` computes SHA256(curr ∥ snap) |
| Empty bucket has well-defined zero hash | ✅ | Empty buckets use the zero hash (all zeros) |

**Assessment: Full adherence.** The BucketList structure exactly matches the spec. Level sizing, spill triggers, hash computation, and the level 0 in-memory optimization are all correct.

---

### 3.5 Bucket Lifecycle

**Source files:** `bucket.rs`, `disk_bucket.rs`, `iterator.rs`
**Spec §5:** Bucket creation, population via output iterator, finalization (hash, close), immutability after finalization.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Buckets are created empty, populated, then finalized | ✅ | `BucketOutputIterator` writes entries in sorted order, finalizes with hash |
| Finalized buckets are immutable | ✅ | `DiskBucket` is read-only after creation; no mutation methods |
| Bucket identified by SHA256 hash of contents | ✅ | Hash computed during output iteration; used as bucket identifier |
| In-memory bucket representation | ✅ | Buckets can be `InMemory` (Vec of entries) or `DiskBacked` |
| Disk-backed bucket as gzip-compressed XDR stream | ✅ | `iterator.rs`: BucketInputIterator reads gzip-compressed XDR; BucketOutputIterator writes gzip |
| Bucket file naming convention | ✅ | Files named by hex hash in bucket directory |
| Temp bucket files during merge | ✅ | Temporary files used during merge, renamed on completion |

**Assessment: Full adherence.** The bucket lifecycle matches the spec from creation through finalization and immutability.

---

### 3.6 Merge Algorithm

**Source files:** `merge.rs`
**Spec §6:** Two-pointer sorted merge, INITENTRY merge rules (CAP-0020), DEAD entry handling, output deduplication.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Two-pointer merge of two sorted bucket streams | ✅ | `merge.rs`: `merge_entries()` implements classic two-pointer merge |
| Newer entries (from `curr`) shadow older entries (from `snap`) | ✅ | `merge.rs`: When keys match, newer entry takes precedence with merge rule application |
| INIT + DEAD → annihilation (both dropped) | ✅ | `merge.rs`: Explicit annihilation rule when INIT meets DEAD |
| DEAD + INIT → LIVE (tombstone absorbed by creation) | ✅ | `merge.rs`: DEAD from newer, INIT from older produces LIVE |
| INIT + LIVE → INIT (preserve creation boundary) | ✅ | `merge.rs`: INIT from newer, LIVE from older preserves INIT |
| LIVE + INIT → panic (invalid state) | ✅ | `merge.rs`: LIVE+INIT and INIT+INIT combinations trigger panic (should never occur in valid state) |
| Shadow cursor for entries shadowed at higher levels | ✅ | `merge.rs`: Shadow cursor support in merge functions; effectively unused post-protocol-12 |
| Output deduplication (no duplicate keys in output) | ✅ | `merge.rs`: BucketOutputIterator enforces sorted, deduplicated output |
| Merge produces sorted output | ✅ | `merge.rs`: Two-pointer merge inherently produces sorted output from sorted inputs |
| `merge_in_memory()` for small buckets | ✅ | `merge.rs`: In-memory merge path for level 0 and small buckets |
| `merge_buckets_to_file()` for large buckets | ✅ | `merge.rs`: Streaming disk-based merge for larger levels |
| MergeCounters for tracking merge statistics | ✅ | `metrics.rs`: MergeCounters tracks entries merged, new/old, annihilated |

**Assessment: Full adherence.** The merge algorithm is one of the most critical components and it fully matches the spec. All CAP-0020 INITENTRY merge rules are correctly implemented with the proper annihilation, absorption, and preservation semantics.

---

### 3.7 Async Merge Management

**Source files:** `future_bucket.rs`, `bucket_list.rs`
**Spec §7:** FutureBucket state machine, async merge scheduling, merge completion handling, merge future deduplication.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| FutureBucket 5-state machine (Clear/HashOutput/HashInputs/LiveOutput/LiveInputs) | ✅ | `future_bucket.rs`: All 5 states implemented as enum variants |
| `make_live()` — transition from deferred to resolved state | ✅ | `future_bucket.rs`: `make_live()` resolves the future bucket |
| Async merge via background thread/task | ✅ | `bucket_list.rs`: `AsyncMergeHandle` uses `tokio::task::spawn_blocking` + oneshot channel |
| `PendingMerge::InMemory` for level 0 | ✅ | `bucket_list.rs`: Level 0 merges stay in memory for performance |
| `PendingMerge::Async` for higher levels | ✅ | `bucket_list.rs`: Levels 1+ use async background merges |
| Resolve pending merges before accessing curr/snap | ✅ | `bucket_list.rs`: Merges resolved before bucket access |
| MergeKey for identifying unique merge operations | ✅ | `future_bucket.rs`: `MergeKey` struct for merge identity |
| FutureBucket serialization for HAS (HistoryArchiveState) | ✅ | `future_bucket.rs`: Full serialization/deserialization support |
| `getMergeFuture()` — reuse in-progress merge | ⚠️ | `merge_map.rs`: `BucketMergeMap` tracks completed merges; in-flight dedup not yet implemented |
| `putMergeFuture()` — register merge for reuse | ⚠️ | Same as above — completed-merge cache exists but in-flight reattachment is missing |
| Merge deduplication across concurrent operations | ❌ | In-flight merge dedup not yet implemented; duplicate merges may be scheduled during catchup/restart |

**Assessment: High adherence.** The FutureBucket state machine and async merge scheduling are fully implemented. The one notable gap is the in-flight merge deduplication (`getMergeFuture`/`putMergeFuture`) — `BucketMergeMap` caches completed merge outputs but in-flight reattachment is not yet implemented. This means duplicate merges may occur during catchup or restart scenarios, wasting compute but not affecting correctness.

---

### 3.8 BucketManager

**Source files:** `bucket_manager.rs`, `lib.rs`
**Spec §8:** Central bucket management — bucket adoption, temp directory, bucket sharing, metrics, garbage collection.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Bucket directory management | ✅ | Bucket files managed in configured directory |
| Temp directory for in-progress merges | ✅ | Temporary files for merge output |
| Bucket adoption (add finalized bucket to managed set) | ✅ | Buckets adopted by hash after merge completion |
| Bucket sharing (reuse existing bucket by hash) | ✅ | Hash-based lookup prevents duplicate storage |
| `getMergeFuture()`/`putMergeFuture()` merge cache | ❌ | In-flight merge dedup not yet implemented; completed-merge cache (BucketMergeMap) is wired but reattachment to running merges is missing |
| `scheduleVerifyReferencedBucketsWork()` | ⚠️ | Partial implementation per PARITY_STATUS.md |
| Publish queue integration | ✅ | Buckets referenced by publish queue during history archival |
| Garbage collection of unreferenced buckets | ✅ | Unreferenced bucket files cleaned up |
| Bucket size tracking and metrics | ⚠️ | Basic tracking present; some stellar-core metrics (bloom miss meters) not implemented |
| `forgetUnreferencedBuckets()` | ✅ | Implemented for cleanup after catchup |

**Assessment: Medium adherence.** Core BucketManager functionality (directory management, adoption, sharing, GC) works correctly. The primary gap is the merge future deduplication cache not being wired, which affects efficiency during catchup and restart but not correctness. Some operational metrics are also missing.

---

### 3.9 Indexing

**Source files:** `index.rs`, `bloom_filter.rs`, `index_persistence.rs`
**Spec §9:** Per-bucket indexes for point lookups, page-based disk index, bloom filter, in-memory index for small buckets, index persistence.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `LiveBucketIndex` facade over disk/in-memory variants | ✅ | `index.rs`: `LiveBucketIndex` enum dispatches to `DiskIndex` or `InMemoryIndex` |
| `DiskIndex` with page-based structure | ✅ | `index.rs`: Page-based index with `DEFAULT_PAGE_SIZE = 16384` |
| Bloom filter for negative lookups | ✅ | `bloom_filter.rs`: `BucketBloomFilter` integrated into DiskIndex |
| `InMemoryIndex` for small buckets | ✅ | `index.rs`: In-memory index used for buckets below cutoff |
| `DEFAULT_INDEX_CUTOFF = 20MB` — threshold for in-memory vs disk index | ✅ | `index.rs`: `DEFAULT_INDEX_CUTOFF` matches spec |
| `BucketEntryCounters` — track entry counts by type | ✅ | `index.rs`: Entry counters track LIVE, DEAD, INIT entry counts |
| `AssetPoolIdMap` for pool share lookups | ✅ | `index.rs`: Asset-to-pool-ID mapping for efficient pool share queries |
| `TypeRange` — byte range per entry type for scan optimization | ✅ | `index.rs`: Type ranges enable targeted scans within buckets |
| Index persistence (save/load from disk) | ✅ | `index_persistence.rs`: Save/load using bincode serialization (vs Cereal in C++) |
| Bloom filter false positive rate tuning | ✅ | Bloom filter configured with appropriate parameters |
| Bloom miss meter for monitoring false positive rate | ❌ | `PARITY_STATUS.md`: Bloom miss meters not integrated into metrics |
| Thread-safe index pointer access (v26 race fix) | ✅ | Henyey uses Rust's ownership model and `Arc`-based sharing; the C++ index pointer race (fixed in v26) is structurally impossible |

**Assessment: Full adherence.** The indexing system is comprehensively implemented with both disk and in-memory variants, bloom filter integration, page-based lookup, and persistence. The only gap is the bloom miss meter for monitoring, which is an observability concern rather than a correctness issue. Notably, the v26 index pointer race fix is not applicable to henyey — Rust's type system prevents the category of data races that affected stellar-core.

---

### 3.10 Snapshot & Query Layer

**Source files:** `snapshot.rs`, `cache.rs`
**Spec §10:** Immutable snapshots of BucketList state, concurrent query access, point lookups, range scans, pool share queries.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `BucketSnapshot` — immutable snapshot of a single bucket | ✅ | `snapshot.rs`: Immutable bucket snapshot with index reference |
| `BucketListSnapshot` — snapshot of entire BucketList | ✅ | `snapshot.rs`: Captures all 11 levels' curr/snap snapshots |
| `BucketSnapshotManager` — manages snapshot lifecycle | ✅ | `snapshot.rs`: Thread-safe manager with `parking_lot::RwLock` |
| Concurrent read access without blocking merges | ✅ | Snapshots are immutable; reads don't block writes |
| `SearchableBucketListSnapshot` — query API | ✅ | `snapshot.rs`: Full query interface |
| `load()` — single key point lookup | ✅ | Searches levels top-down, returns first match |
| `loadKeys()` — batch key lookup | ✅ | Batch lookup across all levels |
| `loadPoolShareTrustLines()` — pool share queries by asset | ✅ | Uses `AssetPoolIdMap` from index for efficient lookup |
| `loadInflationWinners()` — inflation winner query | ✅ | Implemented for protocol compatibility |
| `scanForEntriesOfType()` — type-based scanning | ✅ | Uses `TypeRange` from index for efficient scanning |
| Historical snapshots for past ledger queries | ✅ | `snapshot.rs`: Historical snapshot support |
| `RandomEvictionCache` for hot entry caching | ✅ | `cache.rs`: Random eviction cache for frequently accessed entries |
| Unified snapshot class (v26 refactor) | ✅ | Henyey already uses a single `BucketListSnapshot` struct; the v26 refactor to consolidate multiple C++ snapshot classes is not applicable |
| Snapshot copy safety for background threads (v26 race fix) | ✅ | `snapshot.rs`: Arc-based cloning (`copy_searchable_live_snapshot`) provides safe concurrent access without the deep-copy races that affected stellar-core |

**Assessment: Full adherence.** The snapshot and query layer is comprehensive. All query methods specified in the spec are implemented, concurrent access is properly handled via `parking_lot::RwLock` and `Arc` cloning, and the random eviction cache provides performance optimization. The v26 snapshot race fixes and refactor to a single class are structurally addressed by henyey's Rust-based design.

---

### 3.11 Hot Archive BucketList

**Source files:** `hot_archive.rs`
**Spec §11:** Separate BucketList for evicted Soroban entries, ARCHIVED/LIVE entry types, merge semantics, 11 levels.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Separate `HotArchiveBucketList` with 11 levels | ✅ | `hot_archive.rs`: Full 11-level structure mirroring live BucketList |
| `HotArchiveBucket` with InMemory and DiskBacked variants | ✅ | `hot_archive.rs`: Both storage variants implemented |
| ARCHIVED entry type for evicted entries | ✅ | Proper XDR type handling |
| LIVE entry type for restored entries (tombstone in archive context) | ✅ | LIVE in hot archive acts as "un-evict" marker |
| ARCHIVED + LIVE → annihilation (entry restored, remove from archive) | ✅ | `hot_archive.rs`: Correct merge rule — restored entries cancel archived ones |
| V1 BucketMetadata with HOT_ARCHIVE type discriminant | ✅ | `hot_archive.rs`: Proper metadata version and type tagging |
| `fresh()` — create bucket from new evicted entries | ✅ | Bucket creation from eviction candidates |
| Hot archive spill mechanics match live BucketList | ✅ | Same level structure and spill timing |
| `loadCompleteHotArchiveState()` — full archive scan | ⚠️ | `PARITY_STATUS.md`: Partial implementation |
| Scan for valid entry types (v26 addition) | ⚠️ | stellar-core v26 added validation that hot archive scans only return valid entry types; henyey's hot archive scan does not yet enforce this filter |

**Assessment: Full adherence.** The hot archive BucketList is comprehensively implemented with correct merge semantics. The `loadCompleteHotArchiveState()` partial implementation is used for diagnostics rather than consensus-critical operations. The v26 valid entry type scan filter is a minor gap.

---

### 3.12 Eviction

**Source files:** `eviction.rs`
**Spec §12:** TTL-based eviction of Soroban entries, EvictionIterator, state archival settings, eviction scanning.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `EvictionIterator` — stateful iterator across BucketList levels | ✅ | `eviction.rs`: Full iterator with position tracking across levels |
| `advance_to_next_bucket()` — move to next bucket when current exhausted | ✅ | `eviction.rs`: Proper level/bucket advancement |
| `update_starting_eviction_iterator()` — persist iterator position | ✅ | Position serialized/deserialized across ledger closes |
| `StateArchivalSettings` — protocol-level eviction parameters | ✅ | `eviction.rs`: Settings from network config |
| TTL-based eviction check | ✅ | Entries evicted when TTL expires relative to current ledger |
| `EvictionCandidate` — entry identified for eviction | ✅ | Proper candidate identification |
| `EvictionResult` — output of eviction scan | ✅ | Result type with evicted entries and updated iterator position |
| Eviction respects persistent vs temporary entry distinction | ✅ | Different TTL handling for persistent (archivable) vs temporary (deletable) entries |
| `EvictionStatistics` — cycle metrics | ⚠️ | `PARITY_STATUS.md`: `submitMetricsAndRestartCycle()` simplified; basic counters present but full cycle reporting missing |
| Protocol gate: persistent eviction from protocol 23+ | ✅ | `lib.rs`: `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION = 23` |
| Background eviction scan snapshot safety (v26 fix) | ✅ | Henyey passes snapshot data by value/Arc-clone to background eviction scans; the v26 fix for passing snapshot copies to background threads is structurally addressed |
| Eviction scan duration metrics (v26 addition) | ⚠️ | stellar-core v26 added `eviction_scan_duration` timer metric; henyey has `EvictionCounters` with cycle counts but not wall-clock duration per scan |

**Assessment: Full adherence.** The eviction system is fully functional with correct TTL-based scanning, proper persistent vs temporary entry handling, and iterator state persistence. The v26 background scan safety fix is already addressed by Rust's ownership model. The eviction scan duration metric is a minor observability gap.

---

### 3.13 Catchup & State Reconstruction

**Source files:** `applicator.rs`
**Spec §13:** Reconstructing ledger state from bucket files during catchup, chunked application, deduplication.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `BucketApplicator` — applies bucket entries to ledger state | ✅ | `applicator.rs`: Full applicator with chunked processing |
| Chunked processing to bound memory usage | ✅ | `applicator.rs`: Configurable chunk size, processes entries in batches |
| `ApplicatorCounters` — track application progress | ✅ | Counters for entries processed, upserted, deleted |
| `EntryToApply` — Upsert or Delete action | ✅ | Enum with proper semantics for state reconstruction |
| Deduplication via seen keys set | ✅ | `applicator.rs`: Tracks seen keys to skip entries superseded by newer levels |
| Top-down level traversal (newest entries win) | ✅ | Processes levels from 0 to 10, first occurrence of key wins |
| DEAD entries result in deletion during application | ✅ | DEAD entries translated to Delete actions |
| LIVE/INIT entries result in upsert during application | ✅ | LIVE/INIT entries translated to Upsert actions |

**Assessment: Full adherence.** Catchup and state reconstruction is complete with proper chunked processing, deduplication, and correct entry type handling during application.

---

### 3.14 Serialization & Persistence

**Source files:** `future_bucket.rs`, `merge_map.rs`, `index_persistence.rs`
**Spec §14:** FutureBucket serialization for HAS (HistoryArchiveState), merge map persistence, index serialization.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| FutureBucket serialization to HAS JSON | ✅ | `future_bucket.rs`: Full serialization/deserialization of 5-state FutureBucket |
| FutureBucket deserialization and state reconstruction | ✅ | `future_bucket.rs`: Can reconstruct merge state from persisted HAS |
| `MergeKey` serialization | ✅ | `future_bucket.rs`: MergeKey properly serialized |
| `BucketMergeMap` persistence | ⚠️ | `merge_map.rs`: Completed-merge cache is used at runtime but not persisted across restarts |
| Index file serialization (save to disk) | ✅ | `index_persistence.rs`: Bincode-based serialization (architectural departure from C++ Cereal, functionally equivalent) |
| Index file deserialization (load from disk) | ✅ | `index_persistence.rs`: Load and validate persisted indexes |
| Bucket file gzip compression | ✅ | `iterator.rs`: Gzip compression for disk bucket files |

**Assessment: High adherence.** Serialization and persistence works correctly for FutureBucket state, indexes, and bucket files. BucketMergeMap is used at runtime for completed-merge output caching but is not persisted across restarts, meaning merge reuse only works within a single process lifetime. This affects efficiency rather than correctness.

---

### 3.15 Invariants & Safety Properties

**Source files:** `merge.rs`, `entry.rs`, `bucket_list.rs`, `iterator.rs`
**Spec §15:** Key invariants that must hold for correctness — sort order, merge determinism, hash consistency.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Bucket entries are always sorted by key | ✅ | `iterator.rs`: BucketOutputIterator enforces sort order; merge algorithm preserves it |
| Merge output is deterministic (same inputs → same output) | ✅ | `merge.rs`: Deterministic two-pointer merge with deterministic tie-breaking |
| BucketList hash is deterministic | ✅ | `bucket_list.rs`: SHA256 hash chain is fully deterministic |
| No duplicate keys within a single bucket | ✅ | `iterator.rs`: BucketOutputIterator deduplicates |
| INITENTRY invariant: at most one INIT per key in merge scope | ✅ | `merge.rs`: Panics on LIVE+INIT or INIT+INIT (invalid states) |
| DEAD entries only in non-bottom levels (can be GC'd at bottom) | ✅ | Merge at bottom level drops DEAD entries |
| Metadata entry always at position 0 | ✅ | Enforced by output iterator; validated by input iterator |
| Protocol version gates for feature behavior | ✅ | `lib.rs`: All protocol version constants defined and used for gating |
| `scheduleVerifyReferencedBucketsWork()` — verify bucket integrity | ⚠️ | Partial per PARITY_STATUS.md; full integrity verification not complete |
| In-memory state deep copy safety (v26 fix) | ✅ | Rust's ownership semantics ensure deep copies where needed; the C++ deep-copy bug for invariant checking does not apply |

**Assessment: Full adherence.** All critical safety invariants are enforced. Sort order, merge determinism, hash consistency, and INITENTRY invariants are all properly maintained. The partial bucket verification work is a defense-in-depth measure rather than a correctness requirement.

---

### 3.16 Constants

**Source files:** `lib.rs`, `index.rs`
**Spec §16:** Protocol constants, level sizing parameters, index tuning parameters.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `NUM_LEVELS = 11` | ✅ | `bucket_list.rs` |
| `FIRST_PROTOCOL_SUPPORTING_INITENTRY = 11` | ✅ | `lib.rs` |
| `FIRST_PROTOCOL_SHADOWS_REMOVED = 12` | ✅ | `lib.rs` |
| `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION = 23` | ✅ | `lib.rs` |
| `DEFAULT_PAGE_SIZE = 16384` (index) | ✅ | `index.rs` |
| `DEFAULT_INDEX_CUTOFF = 20MB` (in-memory vs disk index threshold) | ✅ | `index.rs` |
| Level size formulas | ✅ | `bucket_list.rs`: Correct exponential sizing |

**Assessment: Full adherence.** All constants match the spec values.

---

### 3.17 References

**Spec §17** contains references to CAPs and design documents. No implementation requirements.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Informational only | ➖ | No implementation needed |

---

### 3.18 Appendices

**Spec §18** contains supplementary material. No implementation requirements.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Informational only | ➖ | No implementation needed |

---

## 4. Gap Summary

### Critical Gaps

None. All consensus-critical behavior (merge algorithm, BucketList structure, hash computation, entry ordering) is fully implemented.

### Moderate Gaps

| Gap | Spec Section | Impact | Difficulty |
|-----|-------------|--------|------------|
| Merge future deduplication not wired (`getMergeFuture`/`putMergeFuture`) | §7, §8 | Duplicate merges during catchup/restart waste compute; no correctness impact | Medium — requires designing in-flight dedup integration with BucketManager merge scheduling |

### Minor Gaps

| Gap | Spec Section | Impact | Difficulty |
|-----|-------------|--------|------------|
| `loadCompleteHotArchiveState()` partial | §11 | Diagnostic/admin functionality incomplete | Low |
| `scheduleVerifyReferencedBucketsWork()` partial | §15 | Defense-in-depth integrity verification incomplete | Low |
| `BucketInputIterator::seek()` missing | §6 | Cannot seek within bucket stream; must scan from start | Low |
| Bloom miss meters not integrated | §9 | Cannot monitor bloom filter false positive rates | Low |
| `EvictionStatistics::submitMetricsAndRestartCycle()` simplified | §12 | Eviction cycle metrics less detailed than stellar-core | Low |
| Hot archive valid entry type scan filter (v26) | §11 | Missing validation in hot archive scan results | Low |
| Eviction scan duration metric (v26) | §12 | No wall-clock timer for eviction scan duration | Low |

---

## 5. Risk Assessment

### Correctness Risk: Low

The core BucketListDB behavior is fully implemented:
- Merge algorithm with all CAP-0020 rules is correct
- BucketList structure with 11 levels, proper spill mechanics, and deterministic hashing
- Entry sort order and deduplication enforced
- Eviction and hot archive semantics correct
- Catchup state reconstruction complete

There are no known correctness gaps. The merge future deduplication gap affects performance/efficiency, not determinism or state correctness.

The v26 concurrency race fixes (index pointer, startup, snapshot access) are **not applicable** to henyey — Rust's ownership model, `Arc` cloning, and `parking_lot::RwLock` structurally prevent the categories of data races that affected stellar-core's C++ implementation.

The v26 XDR offset bug fix for old protocols is **not applicable** — henyey targets protocol 24+ only and does not process old-protocol bucket entries with the affected offset calculation.

### Performance Risk: Low–Medium

The unconnected merge future cache means that during catchup or restart, the same merge may be computed multiple times. In production steady-state operation this is unlikely to matter (merges are sequential per level), but during catchup from a distant ledger, duplicate work could slow recovery. The data structures are already implemented — wiring them in is the remaining work.

### Operational Risk: Low

Missing metrics (bloom miss meters, eviction cycle statistics, eviction scan duration) reduce observability but do not affect node operation. Operators will have less visibility into BucketListDB internals compared to stellar-core, but the system will function correctly.

---

## 6. v26.0.1 Implementation Delta

stellar-core v26.0.1 introduced significant bucket-related changes between v25.0.1 and v26.0.1. This section evaluates their impact on henyey parity.

### Concurrency & Race Fixes

| upstream commit | Change | Henyey Impact |
|-----------------|--------|---------------|
| `f1207c46c` | Fix race on bucket index pointer | **N/A** — Rust's type system prevents shared mutable access to index pointers without synchronization |
| `3f610e582` | Fix race on startup with HTTP server | **N/A** — henyey's `BucketSnapshotManager` initializes under `RwLock`; no startup race possible |
| `f31785617` | Fix snapshot access races | **N/A** — snapshot access uses `Arc` cloning via `copy_searchable_live_snapshot()`; no shared mutable state |
| `cde79db48`, `f75749645` | Bucket race fixes (index creation on merge) | **N/A** — index creation in henyey is scoped to the merge task and returned by value |
| `daf2fe92f`, `c603a9690` | Copy snapshot before background eviction scan | **N/A** — henyey clones snapshot Arc before passing to background tasks |
| `952546d3f` | Address gaps in concurrency thread safety | **N/A** — broadly addresses C++ shared-state issues not present in Rust's ownership model |

**Summary:** All v26 concurrency fixes address C++ shared-mutable-state bugs that are structurally impossible in henyey's Rust implementation. No action required.

### Architectural Refactors

| upstream commit | Change | Henyey Impact |
|-----------------|--------|---------------|
| `1a8409f81` | Refactor snapshots to single `BucketListSnapshot` class | **Already aligned** — henyey uses a single `BucketListSnapshot` struct (`snapshot.rs`) |
| `489516e53` | Move `inMemoryState` init to Bucket constructor for safety | **N/A** — henyey initializes bucket state at construction time via Rust struct initialization |
| `eb0030727` | Remove maintenance | **N/A** — maintenance scheduling not implemented (not needed for protocol 24+) |

### Bug Fixes

| upstream commit | Change | Henyey Impact |
|-----------------|--------|---------------|
| `20f87324a` | Fix BucketListDB XDR offset bug for old protocols | **N/A** — henyey targets protocol 24+ only; the affected offset calculation for pre-protocol entries does not exist |
| `2ef729710` | Fix deep copy of in-memory state for invariant | **N/A** — Rust's ownership ensures proper deep copies; no aliasing bugs possible |

### New Features & Improvements

| upstream commit | Change | Henyey Impact |
|-----------------|--------|---------------|
| `9a80e3ddc` | Scan hot archive for valid entry types | **Minor gap** — henyey's hot archive scan does not yet filter for valid entry types |
| `084f41c7d` | Add metrics for eviction scan duration | **Minor gap** — henyey has eviction counters but not wall-clock duration timers |
| `9b446e829` | Harden eviction scan interface | **Review needed** — may require interface alignment for robustness |
| `d6d4ce431` | Remove confusing `ltxEvictions` | **N/A** — henyey does not use this naming pattern |
| `8f2c51a71` | Ensure expired entries are never modified by LedgerTxn | **Handled in `henyey-tx`** — evicted entry immutability is enforced at the transaction layer |

### Overall v26 Parity Assessment

The v26 changes are overwhelmingly concurrency fixes for C++ shared-mutable-state bugs. Henyey's Rust implementation is **structurally immune** to these classes of bugs due to the ownership model, `Arc`/`RwLock` synchronization, and value-based snapshot cloning. The two minor functional gaps (hot archive valid entry type filtering and eviction scan duration metric) do not affect consensus correctness.

---

## 7. Recommendations

### Priority 1: Implement In-Flight Merge Dedup
- **What:** Implement in-flight merge deduplication in `BucketManager` so that `getMergeFuture()`/`putMergeFuture()` semantics are covered during merge scheduling. `BucketMergeMap` handles completed-merge caching; in-flight reattachment needs new design.
- **Why:** Prevents duplicate merges during catchup and restart, improving recovery time
- **Effort:** Medium — requires designing integration with the actual merge scheduling path

### Priority 2: Complete Partial Implementations
- **What:** Finish `loadCompleteHotArchiveState()`, `scheduleVerifyReferencedBucketsWork()`, and `BucketInputIterator::seek()`
- **Why:** Completes the remaining parity gap (see `crates/bucket/PARITY_STATUS.md`)
- **Effort:** Low per item

### Priority 3: Adopt v26 Hot Archive Scan Filter
- **What:** Add valid entry type filtering to hot archive scans (per `9a80e3ddc`)
- **Why:** Ensures hot archive queries only return well-formed entries
- **Effort:** Low

### Priority 4: Integrate Observability Metrics
- **What:** Wire bloom miss meters, eviction cycle statistics, eviction scan duration timer, and remaining BucketManager metrics
- **Why:** Enables production monitoring and debugging parity with stellar-core
- **Effort:** Low
