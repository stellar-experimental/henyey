# Plan: Reduce Mainnet Validator Memory to Under 16 GB

## Context

A henyey mainnet validator currently uses ~24.5 GB RSS. The goal is to reduce
steady-state memory to under 16 GB — a ~8.5 GB reduction.

### Real Mainnet Memory Breakdown (L61740160, 2026-03-20)

**RSS: 24,532 MB** (anon: 24,212 MB, file: 320 MB)

| Component | MB | % tracked | Notes |
|---|---|---|---|
| module_cache | 1,890 | 38% | 1,165 compiled WASM modules |
| offers | 1,275 | 26% | 933K offers, 4 indexes |
| bucket_list_heap | 612 | 12% | InMemory bucket entries + indexes |
| soroban_data | 539 | 11% | 2.9M contract data entries (IMS) |
| soroban_code | 473 | 10% | 1,165 contract code entries (IMS) |
| hot_archive_heap | 90 | 2% | InMemory hot archive entries |
| bucket_list_cache | 71 | 1% | RandomEvictionCache |
| executor_state | 3 | 0% | |
| **Tracked heap total** | **4,953** | | |
| bucket_list_mmap | 12,436 | (virtual) | Only ~320 MB resident as file RSS |
| hot_archive_mmap | 86 | (virtual) | |
| **Unaccounted anon RSS** | **~19,260** | | jemalloc not enabled; no allocator stats |

**Critical finding**: 19 GB of anonymous RSS is unaccounted for. Without
jemalloc, we have no visibility into allocator overhead, fragmentation, or
missed components. This is the biggest unknown and must be investigated first.

---

## Phase 0: Enable jemalloc + Memory Profiling (prerequisite)

**Goal**: Understand the 19 GB gap between tracked heap (5 GB) and anon RSS (24 GB).

### Changes

1. **Enable jemalloc feature** in the henyey binary crate
   - File: `crates/henyey/Cargo.toml`
   - Add `tikv-jemallocator` and `tikv-jemalloc-ctl` dependencies
   - The memory_report already has jemalloc support (`AllocatorStats::read_jemalloc`)
   - This gives us `allocated`, `active`, `resident`, `mapped`, `retained` stats
   - `fragmentation_pct` will finally work: `(resident - allocated) / allocated`

2. **Improve heap estimates** where they are known to undercount
   - IMS `estimate_contract_data_heap_bytes()` uses XDR payload size, but Rust
     deserialized structs are 2-3x larger than XDR. Add `std::mem::size_of`-based
     estimates for the Arc payload
   - File: `crates/ledger/src/soroban_state.rs:987`

3. **Deploy and collect one memory report cycle** — the jemalloc stats will tell us:
   - How much memory is truly allocated vs RSS (glibc can retain 2-4x)
   - Whether switching to jemalloc alone reduces RSS (jemalloc returns memory
     to OS more aggressively than glibc)

**Expected outcome**: jemalloc alone may reduce RSS by 2-5 GB (glibc malloc
fragmentation is notoriously bad for long-running processes with many small
allocations). Plus we get real allocator data to guide subsequent phases.

---

## Phase 1: Reduce Module Cache (target: -1 GB+)

**The module_cache is 1,890 MB (38% of tracked heap)** for just 1,165 modules.
That's ~1.6 MB per compiled module — likely because the Soroban VM stores fully
compiled (Wasmi) modules which are much larger than source WASM.

### Changes

1. **Investigate module cache internals** — determine what's stored per module:
   - File: `crates/tx/src/soroban/` — find `PersistentModuleCache` definition
   - Is it storing compiled Wasmi modules? Can they be stored in a more compact form?
   - Can we use lazy compilation (compile on first use, evict LRU)?

2. **Add LRU eviction to module cache** — most contracts are rarely called.
   Keep only the top N most-used modules compiled; evict cold modules.
   - 100-200 hot modules would cover most execution while using ~200-300 MB
   - Cold modules recompile on demand (adds latency for rare contracts only)

3. **Consider disk-backed module cache** — store compiled modules on disk,
   mmap them on demand, let the OS manage residency.

**Estimated savings**: 1-1.5 GB (keep ~300 MB for hot modules vs current 1,890 MB)

---

## Phase 2: Compact Offer Store (target: -500 MB)

**The offer store uses 1,275 MB for 933K entries** (~1,366 bytes/offer).
This is higher than the theoretical ~740 bytes/offer estimate, suggesting
HashMap overhead and alignment waste are significant.

### Changes (ordered by impact)

1. **Replace OfferKey (44 bytes) with i64 offer_id (8 bytes)**
   - `offer_id` is globally unique in Stellar — the seller_id is redundant as a key
   - Eliminate the `by_id: HashMap<i64, OfferKey>` map entirely (merged into `offers`)
   - Shrinks keys in `offers`, `offer_locations`, and OrderBook BTree values
   - File: `crates/tx/src/state/offer_store.rs`, `crates/tx/src/state/offer_index.rs`
   - **Savings**: ~200 MB

2. **Intern assets — replace TrustLineAsset (52 bytes) with u32 asset_id (4 bytes)**
   - Only ~1000 unique assets on mainnet; intern them in a lookup table
   - `AssetPair` shrinks from 104 → 8 bytes, `TrustlineKey` from 88 → 40 bytes
   - Shrinks `order_books` keys, `offer_locations` values, `account_asset_index` keys
   - File: `crates/tx/src/state/offer_store.rs` (new `AssetInterner`)
   - **Savings**: ~170 MB

3. **Move sponsor to side map**
   - 95% of offers have no sponsor; `Option<AccountId>` wastes 40 bytes per entry
   - Store sponsors in `HashMap<i64, AccountId>` only for sponsored offers
   - **Savings**: ~35 MB

**Total estimated savings**: ~400-500 MB

---

## Phase 3: Reduce Bucket List Heap (target: -400 MB)

**bucket_list_heap is 612 MB** — these are InMemory buckets (entries < 10 MB threshold).

### Changes

1. **Lower DISK_BACKED_THRESHOLD from 10 MB to 1 MB**
   - More buckets use DiskBacked mode with compact indexes
   - File: `crates/bucket/src/manager.rs:218`
   - Trade-off: slightly slower lookups for small buckets (disk I/O vs RAM)
   - Mitigated by bloom filters (fast negative lookups) and OS page cache
   - **Savings**: ~200-400 MB

2. **Compact DiskIndex page ranges — store XDR bytes instead of LedgerKey structs**
   - `RangeEntry` stores two full `LedgerKey` objects (~200 bytes each)
   - Replace with `Vec<u8>` serialized bytes or upper-bound-only indexing
   - File: `crates/bucket/src/index.rs:54`
   - **Savings**: ~100-200 MB (for large DiskBacked buckets)

3. **Force hot archive buckets to DiskBacked**
   - hot_archive_heap = 90 MB; these entries are rarely accessed (only RestoreFootprint)
   - File: `crates/bucket/src/manager.rs` — `load_hot_archive_bucket`
   - **Savings**: ~80 MB

**Total estimated savings**: ~400-600 MB

---

## Phase 4: Compact InMemorySorobanState (target: -200 MB)

**soroban_data (539 MB) + soroban_code (473 MB) = 1,012 MB** for ~3M entries.

### Changes

1. **Replace Arc<LedgerEntry> with Box<LedgerEntry>** in IMS
   - IMS entries are protected by RwLock — Arc refcounting is unnecessary
   - Saves 16 bytes per entry (Arc refcount overhead) + reduces heap fragmentation
   - 3M entries × 16 bytes = ~48 MB direct savings
   - File: `crates/ledger/src/soroban_state.rs`
   - **Savings**: ~50 MB

2. **Store XDR bytes instead of deserialized structs** (higher risk)
   - `ContractDataMapEntry` stores full deserialized LedgerEntry
   - Rust structs are 2-3x larger than XDR bytes due to padding, Vec headers, enum tags
   - Replace with `Box<[u8]>` and deserialize on demand
   - Trade-off: CPU cost on hot path (Soroban execution reads these entries)
   - Could use a small LRU cache of deserialized entries for hot contracts
   - **Savings**: ~200-400 MB (if Rust structs are 2x XDR size)

3. **Separate TTL-only fast path** — for `is_archived_contract_entry()` checks,
   we only need TTL data, not the full entry. Store TTLs separately to avoid
   loading full entries just for archival checks.
   - Already partially done (TtlData is co-located), but the entry itself is
     still loaded into the HashMap even when only TTL is needed
   - **Savings**: Indirect (reduces working set, not direct memory)

**Total estimated savings**: 200-400 MB (conservative: 200 MB with Box only)

---

## Phase 5: Reduce Bucket List Cache Budget

**bucket_list_cache = 71 MB** but `memory_for_caching_mb` defaults to 1024 MB.
The cache is underutilized (71 MB used of 1024 MB budget).

### Change

- Lower default to 128 MB or make it auto-sizing based on working set
- File: `crates/common/src/config.rs:369`
- **Savings**: Negligible directly (cache is already underutilized), but
  reduces peak allocation during cache warmup

---

## Summary of Expected Savings

| Phase | Target | Estimated Savings | Complexity | Risk |
|-------|--------|------------------|------------|------|
| 0: jemalloc | Visibility + fragmentation | 2-5 GB (allocator switch) | Low | Low |
| 1: Module cache | Compiled WASM | 1-1.5 GB | Medium | Medium |
| 2: Offer store | 933K entries | 400-500 MB | Medium | Low |
| 3: Bucket list heap | InMemory buckets | 400-600 MB | Medium | Medium |
| 4: IMS compaction | 3M Soroban entries | 200-400 MB | Medium-High | Medium |
| 5: Cache budget | Config tuning | ~0 (already low) | Low | Low |
| **Total** | | **4-8 GB** | | |

**Path to 16 GB**: Phase 0 (jemalloc) likely gets us to ~20-22 GB by eliminating
glibc fragmentation. Phases 1-4 save another 2-3 GB of real allocations,
bringing us to ~17-19 GB. The remaining gap depends on what Phase 0's profiling
reveals about the unaccounted 19 GB — there may be additional large allocations
not yet tracked by the memory report.

---

## Verification

1. **After Phase 0**: Deploy with jemalloc, collect memory reports. Compare
   `jemalloc_allocated_mb` vs `heap_components_mb` to find untracked allocations.
   Compare `jemalloc_resident_mb` vs RSS to measure fragmentation.

2. **After each phase**: Run mainnet validator for 1+ hours, collect memory
   reports at steady state. Verify RSS decrease matches expectations.

3. **Performance regression check**: Run verify-execution on a 1000-ledger
   range before and after each phase. Ensure ledger close time doesn't regress
   by more than 10%.

4. **Correctness**: `cargo test --all` after each phase. Run mainnet node and
   verify no hash mismatches.

---

## Critical Files

- `crates/henyey/Cargo.toml` — jemalloc dependency
- `crates/ledger/src/memory_report.rs` — memory reporting (already has jemalloc support)
- `crates/ledger/src/soroban_state.rs` — InMemorySorobanState, heap estimates
- `crates/tx/src/state/offer_store.rs` — OfferStore with 4 data structures
- `crates/tx/src/state/offer_index.rs` — OfferIndex, OfferKey, AssetPair types
- `crates/tx/src/soroban/` — PersistentModuleCache
- `crates/bucket/src/manager.rs` — DISK_BACKED_THRESHOLD, bucket loading
- `crates/bucket/src/index.rs` — RangeEntry, DiskIndex, InMemoryIndex
- `crates/bucket/src/hot_archive.rs` — HotArchiveStorage modes
- `crates/common/src/config.rs` — BucketListDbConfig defaults

## Implementation Order

Start with Phase 0 (jemalloc) since it's low-risk, provides critical visibility,
and may itself deliver 2-5 GB savings. Then proceed through phases in order,
re-evaluating after each based on updated memory reports.
