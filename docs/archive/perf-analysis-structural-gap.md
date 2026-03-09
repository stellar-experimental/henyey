# Structural Performance Gap: Henyey vs stellar-core

## Summary

Henyey currently runs at ~330ms/ledger (mean) vs an estimated ~200ms for stellar-core on
identical traffic. The remaining gap breaks into three categories:

| Source | Est. gap | Status |
|--------|----------|--------|
| Cross-ledger cache miss on dynamic keys | ~10–20ms | P1: actionable |
| Top-N DEX offer seller prefetch | ~5–15ms | P2: actionable (functions exist) |
| Ed25519 per-op re-verification (C1) | ~5–10ms | P3: low-risk win |
| Deferred XDR serialization | ~10–15ms | NOT viable — O7 proved +43ms regression |
| stellar-core LedgerTxnRoot in-memory account index | ~20–30ms | Architectural; no short-term fix |

This doc describes actionable plans for P1–P3 and explains why the architectural gap
cannot be closed without a major structural change.

---

## Background: Why the Cache Hit Rate Is Low

### Per-ledger `prefetch_cache` lifecycle

`SnapshotHandle::prefetch_cache` is an `Arc<parking_lot::RwLock<HashMap<Vec<u8>, LedgerEntry>>>`.
It is constructed fresh for every ledger (all three `SnapshotHandle::new`/`with_lookup`/
`with_lookups_and_entries` constructors call `Arc::new(parking_lot::RwLock::new(HashMap::new()))`).

`run_transactions_on_executor` populates it in two ways:
1. **Static prefetch** (lines 109–127 of `tx_set.rs`): upfront bulk load of all
   statically-determinable keys (`keys_for_fee_processing` + `keys_for_apply` for every TX).
2. **Cache-through** (commit 8104131): every `get_entry` / `load_entries` miss writes the
   loaded entry back into `prefetch_cache`, so the second load within the same ledger is free.

After ledger N closes, the snapshot and its `prefetch_cache` are discarded. Ledger N+1 gets a
fresh snapshot with an empty cache. All dynamic keys (DEX offer sellers loaded during path
payment crossing, sponsor accounts, etc.) require bucket list scans again.

### What "dynamic" means

Static keys are those recoverable from the TX envelope at parse time (source accounts,
destinations, declared offer IDs, etc.). Dynamic keys are those discovered only during
execution: which DEX offer seller happens to be at the top of the book when a path payment
crosses, which sponsor account exists for a created account, etc.

Measured on ledger range 61348953–61349952:
- ~1021 bucket list misses/ledger (dynamic, cold)
- ~148 prefetch cache hits/ledger (12.6% hit rate)

The 87% miss rate is almost entirely dynamic keys not covered by static prefetch.

### What stellar-core does instead

stellar-core's `LedgerTxnRoot` maintains an in-RAM index of all ledger entries loaded since
node startup. Account X loaded in ledger 100 is a zero-cost hit in ledger 110. There is no
per-ledger reset. Henyey's snapshot model makes this impossible without an explicit cross-ledger
hot-key cache.

---

## Caching Layer Deep-Dive: stellar-core vs henyey

### Overview

Both systems read ledger entries from an on-disk data structure (stellar-core: SQLite + bucket
list; henyey: bucket list only). Both cache entries in RAM to amortize disk access across TXs
in the same ledger. The difference is in how many layers of cache exist, what each layer covers,
and which layers survive across ledger boundaries.

---

### stellar-core Caching Layers

stellar-core's cache hierarchy is built around the `LedgerTxn` MVCC tree rooted at
`LedgerTxnRoot`. For each ledger close, one outer `LedgerTxn` is opened; each TX gets a child
`LedgerTxn`. Reads and writes flow through this tree.

#### Layer 1 — `LedgerTxn::mEntry` (per-TX modified state)

**Scope:** Per-`LedgerTxn` instance (per TX and per ledger).
**Type:** `EntryMap` (unordered hash map of `InternalLedgerKey → InternalLedgerEntry`).
**Lifetime:** Duration of the `LedgerTxn` instance; merged UP to parent on `commitChild()`.

When TX N loads account X from root and modifies it, the modified entry goes into the child
`LedgerTxn::mEntry`. When TX N commits, `mEntry` is merged into the parent `LedgerTxn`'s
`mEntry`. TX N+1, reading account X through the parent, finds it in `mEntry` without any
cache lookup or bucket list access — just a hash map probe.

This is the MVCC chain: each layer checks its own `mEntry` first, then walks up to parent
layers, only reaching `LedgerTxnRoot` (and thus `mEntryCache` / bucket list) for entries
never touched by any ancestor TX.

At ledger commit, the outer `LedgerTxn::mEntry` (which now contains all TX changes) commits
to `LedgerTxnRoot`, persisting to SQL and clearing `mEntryCache`.

#### Layer 2 — `LedgerTxnRoot::mEntryCache` (cross-TX read cache within a ledger)

**Scope:** All TXs in a single ledger.
**Type:** `RandomEvictionCache<LedgerKey, CacheEntry>` — an LRU-eviction cache with configurable
capacity (`entryCacheSize` constructor parameter, set by `LedgerManagerImpl`).
**Lifetime:** Created at node startup; **cleared on every `commitChild`** (end of each ledger).

When `LedgerTxnRoot` loads an entry from SQL or bucket list, it goes into `mEntryCache` with
`LoadType::IMMEDIATE`. Prefetch calls populate it with `LoadType::PREFETCH`. Subsequent reads
for the same key within the ledger are `O(1)` hash map probes.

`LoadType` distinguishes entries loaded eagerly (IMMEDIATE) from batch-prefetched ones
(PREFETCH). If the entry cache throws (e.g., schema mismatch), it clears itself as a safety
measure (`mEntryCache.clear()` in the catch block).

**Critical detail:** `mEntryCache` is cleared per-ledger (on `commitChild`), not cross-ledger.
Both stellar-core and henyey have per-ledger read caches. The structural advantage is
`mEntryCache`'s size (configurable, can hold thousands of entries) and its automatic population
via `prefetch()` from the upfront `processFeesSeqNums` TX key scan.

#### Layer 3 — `LedgerTxnRoot::mBestOffers` (lazy offer book with seller prefetch)

**Scope:** All TXs in a single ledger.
**Type:** `UnorderedMap<AssetPair, BestOffersEntryPtr>` where each value is a
`std::deque<LedgerEntry>` of offers sorted by price, loaded in batches.
**Lifetime:** Cleared on `commitChild` (per-ledger).

When `getBestOffer(buying, selling)` is called during TX execution, it looks up the asset pair
in `mBestOffers`. If the deque is empty or exhausted, it loads the next batch of up to
`BATCH_SIZE` offers from the bucket list via `loadBestOffers()`. After each batch load,
`populateEntryCacheFromBestOffers()` is called, which scans the batch and inserts each seller's
account key and trustline keys (buying/selling asset) into `mEntryCache` via `prefetch()` —
provided they are not already cached. This is stellar-core's demand-driven seller prefetch.

```cpp
// LedgerTxn.cpp:3302 — called after each batch of offers is loaded
void LedgerTxnRoot::Impl::populateEntryCacheFromBestOffers(iter, end) {
    UnorderedSet<LedgerKey> toPrefetch;
    for (; iter != end; ++iter) {
        auto const& oe = iter->data.offer();
        toPrefetch.emplace(accountKey(oe.sellerID));
        if (oe.buying.type() != ASSET_TYPE_NATIVE)
            toPrefetch.emplace(trustlineKey(oe.sellerID, oe.buying));
        if (oe.selling.type() != ASSET_TYPE_NATIVE)
            toPrefetch.emplace(trustlineKey(oe.sellerID, oe.selling));
    }
    prefetch(toPrefetch);
}
```

In `allBucketsInMemory` mode (not the default), offers are not loaded via SQL; they go through
the bucket list directly and `loadOffer()` bypasses `mEntryCache` to use the BucketList
snapshot.

#### Layer 4 — `LedgerTxnRoot::mInMemorySorobanState` (Soroban state, persistent)

**Scope:** All TXs across all ledgers (node lifetime).
**Type:** A dedicated `InMemorySorobanState` object — a hash map of all live
`ContractData`, `ContractCode`, and `TTL` entries.
**Lifetime:** Rebuilt on node startup from the bucket list; updated incrementally on each
ledger commit. Never fully cleared during normal operation.

This is the authoritative in-memory mirror of all Soroban state. Reads for Soroban entries
bypass `mEntryCache` and the bucket list entirely — they are `O(1)` hash map lookups.

#### Layer 5 — `LedgerTxn::mMultiOrderBook` (per-child-TX order book overlay)

**Scope:** Per `LedgerTxn` child.
**Type:** `MultiOrderBook` (`UnorderedMap<Asset, UnorderedMap<Asset, OrderBook>>`) — an in-memory
sorted order book per asset pair, mirroring the parent's order book with child TX's modifications.

Used during offer crossing within a single TX. When a TX creates/modifies/deletes an offer,
`mMultiOrderBook` is updated. `getBestOffer()` checks `mMultiOrderBook` in the child before
falling through to `mBestOffers` in the root, implementing TX-level order book isolation.

---

### henyey Caching Layers

henyey's cache hierarchy is built around `SnapshotHandle` (a read-only point-in-time view of
the bucket list) and `TransactionExecutor` (stateful, persistent across ledgers). Reads go
through a stack of in-memory layers before hitting the bucket list.

#### Layer 1 — `LedgerStateManager` per-type maps (cross-TX within ledger, partially cross-ledger)

**Scope:** All TXs in a single ledger (most entry types); ALL ledgers for offers.
**Types:**
```
accounts: HashMap<[u8;32], AccountEntry>          // cleared per-ledger
trustlines: HashMap<TrustlineKey, TrustLineEntry>  // cleared per-ledger
offers: HashMap<OfferKey, OfferEntry>              // PRESERVED cross-ledger
offer_index: OfferIndex                            // PRESERVED cross-ledger
claimable_balances, liquidity_pools, ...           // cleared per-ledger
contract_data, contract_code, ttl                  // cleared per-ledger
```
**Lifetime:** `accounts`/`trustlines`/etc. are cleared by `clear_cached_entries_preserving_offers()`
between ledgers. `offers` and `offer_index` survive across ledgers (set by
`advance_to_ledger_preserving_offers`).

This is the primary in-flight state store. When TX1 loads account X from the bucket list into
`LedgerStateManager::accounts`, TX2 finds it there directly via `get_account()`. No bucket list
scan needed. This mirrors stellar-core's `LedgerTxn::mEntry` chain for within-ledger cross-TX
sharing, but with a flatter structure (one map per entry type rather than an MVCC tree).

Rollback is handled via `*_snapshots` maps (`account_snapshots`, `trustline_snapshots`, etc.)
that capture pre-TX state and are restored on rollback, without clearing the primary maps.

#### Layer 2 — `TransactionExecutor::loaded_accounts` (deduplication guard)

**Scope:** All TXs in a single ledger.
**Type:** `HashMap<[u8;32], bool>` — tracks which account IDs have been "attempted" from the
bucket list (whether found or not).
**Lifetime:** Cleared by `advance_to_ledger_preserving_offers` between ledgers.

This prevents redundant bucket list scans. If `load_account(account_id)` is called for an
account that was already attempted (even if not found), it short-circuits without hitting the
bucket list or prefetch_cache. Equivalent to stellar-core's `mEntryCache` existence check.

**Gap vs stellar-core:** `loaded_accounts` only guards `AccountEntry` loads. Trustlines,
claimable balances, liquidity pools, and other non-account entry types have no equivalent
deduplication guard outside of `LedgerStateManager`'s maps. If a trustline is not in state
(e.g., never modified), each attempted load goes through `get_entry_from_snapshot()` which
checks `SnapshotHandle::prefetch_cache` before the bucket list — so the cache-through mechanism
covers them, but only after the first miss.

#### Layer 3 — `SnapshotHandle::prefetch_cache` (bucket list hit cache)

**Scope:** All TXs in a single ledger (via shared `Arc<RwLock<...>>`).
**Type:** `Arc<parking_lot::RwLock<HashMap<Vec<u8>, LedgerEntry>>>` — a flat HashMap keyed by
XDR-serialized `LedgerKey` bytes.
**Lifetime:** Allocated fresh in each `SnapshotHandle` constructor → per-ledger.

Populated two ways:
1. **Static prefetch** (`snapshot.prefetch()` at ledger start in `run_transactions_on_executor`):
   bulk loads all statically-determinable keys for all TXs in one bucket list pass.
2. **Cache-through** (commit 8104131): every `get_entry()` and `load_entries()` bucket list
   miss writes the loaded entry back into `prefetch_cache`, so subsequent accesses within the
   same ledger are `O(1)` HashMap lookups.

The lookup chain in `get_entry()` is:
```
SnapshotHandle::inner (in-memory snapshot entries)
  → prefetch_cache (HashMap lookup by XDR key bytes)
    → bucket list (via lookup_fn / batch_lookup_fn)
      → write back to prefetch_cache on hit
```

No eviction — entries accumulate until the snapshot is discarded at ledger end. The HashMap is
sized by actual usage (~750–1050 entries/ledger on mainnet), not a fixed capacity.

**Gap vs stellar-core's `mEntryCache`:** Both are per-ledger read caches. The difference is
that stellar-core's `mEntryCache` is also populated by `populateEntryCacheFromBestOffers`
(automatic offer-seller prefetch after each batch) whereas henyey requires explicit prefetch
calls. The `prefetch_cache` also lacks `RandomEvictionCache`'s ability to bound memory usage,
though in practice the entry count is small enough this doesn't matter.

#### Layer 4 — `SnapshotHandle::inner` (ledger snapshot)

**Scope:** Entire ledger.
**Type:** `LedgerSnapshot` — an in-memory set of entries that were part of the ledger header
or explicitly included in the snapshot at construction time.
**Lifetime:** Per-ledger.

In the verify-execution flow, this holds entries from the CDP metadata. In the live-node flow,
it is typically empty (the bucket list is the source of truth). Checked before `prefetch_cache`
in the lookup chain.

#### Layer 5 — `SharedSorobanState` / `InMemorySorobanState` (Soroban state, persistent)

**Scope:** All TXs across all ledgers.
**Type:** HashMap of all live `ContractData`, `ContractCode`, and `TTL` entries with co-located
TTL data.
**Lifetime:** Persistent; rebuilt on startup, updated incrementally on each ledger commit.

henyey's direct equivalent of stellar-core's `InMemorySorobanState`. When `soroban_state` is
set on a `TransactionExecutor`, `load_soroban_footprint` reads from it instead of the bucket
list — `O(1)` lookups. Set by `execute_single_cluster` for the parallel Soroban path.

#### Layer 6 — `OfferIndex` in `LedgerStateManager` (offer book, persistent cross-ledger)

**Scope:** All TXs across all ledgers.
**Type:** `OfferIndex` — `HashMap<AssetPair, BTreeMap<OfferDescriptor, OfferKey>>` + reverse
index `HashMap<OfferKey, (AssetPair, OfferDescriptor)>`.
**Lifetime:** Loaded once at startup via `load_orderbook_offers()` (~911K offers, ~2.7s);
updated incrementally as TXs create/modify/delete offers; preserved across ledgers by
`advance_to_ledger_preserving_offers`.

This is henyey's equivalent of stellar-core's SQL offer table + `mBestOffers` combined.
`best_offer_key(buying, selling)` is `O(log n)` (BTreeMap `first_key_value()`).
`top_n_offer_keys(buying, selling, n)` is `O(n log m)` (BTreeMap iteration).

The key difference from stellar-core's `mBestOffers`: `OfferIndex` is always fully loaded
upfront; stellar-core's `mBestOffers` loads lazily in batches with automatic seller prefetch.
henyey must explicitly prefetch offer seller accounts/trustlines (P2).

---

### Layer-by-Layer Comparison

| Layer | stellar-core | henyey | Cross-ledger? |
|-------|-------------|--------|---------------|
| Per-TX modified state | `LedgerTxn::mEntry` (MVCC chain) | `LedgerStateManager` per-type maps | No (both per-ledger) |
| Cross-TX read cache | `LedgerTxnRoot::mEntryCache` (LRU, bounded) | `SnapshotHandle::prefetch_cache` (HashMap, unbounded) | No (both per-ledger) |
| Offer book | SQL table + `mBestOffers` (lazy batches) | `OfferIndex` (fully preloaded, in-memory BTreeMap) | stellar-core: no; henyey: YES |
| Offer seller prefetch | Automatic via `populateEntryCacheFromBestOffers` | Manual / P2 (not yet wired) | No |
| Account dedup guard | Implicit (mEntry + mEntryCache) | `loaded_accounts` (accounts only) | No |
| Non-account entry dedup | Implicit (mEntry + mEntryCache) | Cache-through (one miss then cached) | No |
| Soroban state | `InMemorySorobanState` (persistent HashMap) | `SharedSorobanState` (persistent HashMap) | Both yes |
| Cross-TX order book | `LedgerTxn::mMultiOrderBook` | `OfferIndex` (shared, no per-TX isolation layer) | N/A |

---

### Key Structural Differences

**1. Offer book strategy (henyey has the advantage here)**

stellar-core loads offers lazily in batches from SQL on `getBestOffer` calls; the batch size
starts at `min(max(prefetchBatchSize, 5), getMaxOffersToCross())`. Between ledgers, the SQL
table is the authoritative store — `mBestOffers` is cleared per-ledger and rebuilt lazily.

henyey loads all ~911K offers once at startup into `OfferIndex`, then maintains it incrementally.
No per-ledger reload. `best_offer_key()` is `O(log n)` with no I/O. This is henyey's biggest
structural advantage over stellar-core for offer-heavy traffic.

**2. Offer seller account caching (stellar-core has the advantage)**

stellar-core automatically prefetches seller accounts and trustlines for each batch of offers
loaded via `populateEntryCacheFromBestOffers`. henyey does not yet do this (P2). When a path
payment crosses an offer in stellar-core, the seller account is already in `mEntryCache`
(loaded with the preceding `getBestOffer` batch). In henyey, the seller account requires a
bucket list scan on first touch within a ledger.

**3. MVCC chain vs flat state maps**

stellar-core's `LedgerTxn::mEntry` chain provides automatic cross-TX visibility: TX N+1 sees
TX N's committed changes by walking parent layers. henyey's `LedgerStateManager` provides the
same within-ledger cross-TX sharing but with a flat structure (one map per entry type). The
semantic result is identical; the implementation differs in that stellar-core's chain also
tracks the original (pre-modification) value at the root level, enabling precise
`LedgerEntryChanges` construction without separate snapshot maps. henyey uses explicit
`*_snapshots` maps for rollback.

**4. Cross-ledger account caching (neither has it; see P1)**

Both `LedgerTxnRoot::mEntryCache` and henyey's `prefetch_cache` are cleared per-ledger.
Neither system caches non-offer, non-Soroban entries across ledger boundaries. The DEX offer
seller accounts that were loaded in ledger N must be re-fetched in ledger N+1 in both systems.

stellar-core's per-ledger cost is lower because its bucket list reads go through SQLite (with
OS page cache warmth) and because `populateEntryCacheFromBestOffers` front-loads seller lookups
before offer crossing begins. henyey's per-ledger cost is higher because seller accounts are
loaded scattered across TXs via cache-through (random access pattern, bucket list scan each).

P1 (cross-ledger hot-key prefetch) closes this specific gap by converting scattered per-TX
bucket list misses into a single upfront batch scan.

**5. `loaded_accounts` scope gap**

henyey's `loaded_accounts` only deduplicates `AccountEntry` lookups. If the same trustline is
looked up by two different path payment operations in the same ledger (and wasn't loaded via
static prefetch), the second lookup goes through `prefetch_cache` (cache hit after first
cache-through) rather than going to the bucket list again. So the deduplication still works
via `prefetch_cache`, but it's one level slower than the `loaded_accounts` short-circuit.
For accounts specifically, `loaded_accounts` avoids even the `prefetch_cache` lock acquisition.

---

## P1: Cross-Ledger Hot-Key Prefetch (HIGH PRIORITY, ~10–20ms)

### Idea

After ledger N's `run_transactions_on_executor` returns, extract all keys from the snapshot's
`prefetch_cache` that were loaded dynamically (i.e., via cache-through from the bucket list).
Store them in the persistent `TransactionExecutor`. At the start of ledger N+1's
`run_transactions_on_executor`, include them in the static prefetch call to pre-warm the new
ledger's `prefetch_cache` before any TX runs.

This mirrors stellar-core's cross-ledger caching without requiring an architectural overhaul.

### Why it works

- DEX offer sellers change slowly. The same 10–50 sellers are at the top of a given order
  book for many consecutive ledgers.
- The same fee-bump inner source accounts, sponsor accounts, and multi-hop payment
  intermediaries appear repeatedly across ledgers.
- By seeding the new ledger's prefetch with last ledger's hot keys, the static prefetch pass
  converts ~400–700 dynamic misses to static hits.

### Size estimate

The prefetch_cache after a typical ledger contains:
- Static prefetch entries: ~112 TXs × ~4 keys/TX = ~450 entries
- Dynamic cache-through entries: ~300–600 entries (offer sellers, sponsors, etc.)
- Total: ~750–1050 entries → `Vec<LedgerKey>` of ~750–1050 elements

This is small enough to keep in memory indefinitely and pass to the next `prefetch()` call.

### Implementation

#### Step 1: Add `take_prefetch_keys()` to `SnapshotHandle`

**File: `crates/ledger/src/snapshot.rs`**

```rust
/// Extract all keys currently in the prefetch cache.
///
/// Used at end-of-ledger to capture dynamically-loaded keys for cross-ledger warm-up.
/// Returns raw XDR-serialized key bytes (not decoded LedgerKey) to avoid
/// re-serialization cost on the next call to `prefetch()`.
pub fn prefetch_cache_keys(&self) -> Vec<LedgerKey> {
    let cache = self.prefetch_cache.read();
    cache
        .values()
        .filter_map(|entry| crate::delta::entry_to_key(entry).ok())
        .collect()
}
```

#### Step 2: Add `hot_keys` field to `TransactionExecutor`

**File: `crates/ledger/src/execution/mod.rs`**

```rust
pub struct TransactionExecutor {
    // ... existing fields ...
    /// Keys loaded dynamically in the previous ledger's execution.
    /// Used to pre-warm the next ledger's prefetch cache before TX execution.
    hot_keys: Vec<LedgerKey>,
}
```

Initialize as `Vec::new()` in `TransactionExecutor::new()`. Both `advance_to_ledger` and
`advance_to_ledger_preserving_offers` leave it unchanged (it will be overwritten by
`run_transactions_on_executor`).

#### Step 3: Include hot_keys in static prefetch pass

**File: `crates/ledger/src/execution/tx_set.rs` — `run_transactions_on_executor()`**

In the static prefetch block (after building `all_keys` from frame keys), add:

```rust
// Seed from previous ledger's dynamically-loaded keys (cross-ledger hot-key cache).
// These are keys not statically determinable from TX envelopes (DEX offer sellers,
// sponsor accounts, etc.) that are likely to be accessed again this ledger.
all_keys.extend(executor.hot_keys.iter().cloned());
```

This extends `all_keys` before the single `snapshot.prefetch(&keys_vec)` call. The `prefetch()`
implementation already deduplicates (skips keys already in cache), so adding stale or
overlapping keys is safe.

#### Step 4: Update `hot_keys` after execution

**File: `crates/ledger/src/execution/tx_set.rs` — end of `run_transactions_on_executor()`**

Before returning `tx_set_result`:

```rust
// Capture dynamically-loaded keys for the next ledger's hot-key prefetch.
executor.hot_keys = snapshot.prefetch_cache_keys();
tracing::debug!(
    hot_keys = executor.hot_keys.len(),
    "Captured hot keys for next ledger"
);
```

### Expected impact

- ~400–700 of the ~1021 dynamic misses/ledger converted to static prefetch hits
- Each converted miss saves ~150μs (bucket list scan time)
- Estimated savings: 400–700 × 150μs = **60–105ms of bucket list scan time**

However, the prefetch batch itself takes time (bulk bucket list scan). The P1 benefit comes
from converting per-TX scattered misses into a single upfront scan. The net improvement
depends on how many of the hot keys actually appear in the next ledger.

Conservative estimate: **~10–20ms net improvement** (accounting for prefetch overhead and
hot-key miss rate across ledger boundaries).

### Risks

- **Stale entries**: keys that existed in ledger N but are deleted in ledger N+1 will be
  prefetched but not found — this is fine, `prefetch()` only stores entries that exist.
- **Key set growth**: if hot_keys accumulates stale entries across many ledgers, it grows
  unboundedly. Mitigation: replace hot_keys entirely each ledger (overwrite, not append).
- **First ledger**: hot_keys is empty; behavior identical to current.

---

## P2: Top-N DEX Offer Seller Prefetch (MEDIUM PRIORITY, ~5–15ms)

### Current state

The infrastructure for this already exists (from commit 8104131, partially retained after
e570d28):

- `OfferIndex::top_n_offer_keys()` — returns top-N offer keys by price for an asset pair
  (in `crates/tx/src/state/offer_index.rs`)
- `TransactionExecutor::collect_seller_keys_for_pairs()` — given a set of (buying, selling)
  pairs, loads the top-N offers per pair and returns their seller account + trustline keys
  (in `crates/ledger/src/execution/mod.rs:1098`)
- `collect_dex_asset_pairs()` — extracts DEX asset pairs from a TX set
  (in `crates/ledger/src/execution/tx_set.rs:1118`, currently `#[cfg(test)]` only)

What was reverted in e570d28 was the wiring of these into `apply_transactions`
(`crates/ledger/src/manager.rs`). The revert message: "Revert offer seller bulk prefetch from
apply_transactions". Calling it from `apply_transactions` was wrong because:
- `apply_transactions` holds the mutex on the persistent executor; the prefetch adds
  latency to the critical path before the snapshot is even used.
- The prefetch was using the executor's stale offer state (from the previous ledger) to
  generate keys, but against the new ledger's snapshot.

### The correct approach

Wire `collect_dex_asset_pairs` + `collect_seller_keys_for_pairs` into
`run_transactions_on_executor` in `tx_set.rs`, **after** the existing static prefetch but
before fee processing. This is the demand-driven approach: only prefetch sellers for pairs
actually referenced by this ledger's TXs.

```rust
// Prefetch seller deps for top-N best offers in DEX pairs from this TX set.
{
    const TOP_N_OFFERS_PER_PAIR: usize = 10;
    let dex_pairs = collect_dex_asset_pairs(transactions);
    if !dex_pairs.is_empty() {
        let seller_keys = executor.collect_seller_keys_for_pairs(&dex_pairs, TOP_N_OFFERS_PER_PAIR);
        if !seller_keys.is_empty() {
            let stats = snapshot.prefetch(&seller_keys)?;
            tracing::debug!(
                pairs = dex_pairs.len(),
                requested = stats.requested,
                loaded = stats.loaded,
                "Prefetched DEX offer seller deps"
            );
        }
    }
}
```

Also remove `#[cfg(test)]` from `collect_dex_asset_pairs`.

### Interaction with P1

If P1 (cross-ledger hot-key prefetch) is implemented first, P2 may have diminishing returns:
the offer sellers from last ledger will already be in `hot_keys` and prefetched at the start
of the static pass. P2 adds coverage for new sellers who weren't present last ledger but are
at the top of the book this ledger (e.g., new offers posted in this ledger's TX set). The
marginal benefit of P2 on top of P1 is probably ~3–8ms.

### Regression risk

The previous regression (which led to e570d28) came from calling this from `apply_transactions`
rather than `run_transactions_on_executor`. Wiring into `run_transactions_on_executor` avoids
that issue. However, if TOP_N_OFFERS_PER_PAIR × number_of_pairs is large, the prefetch call
itself adds latency. Start with N=5 and benchmark; increase if the net is positive.

---

## P3: Skip Already-Verified Ed25519 Signatures (LOW PRIORITY, ~5–10ms)

### Problem

From `perf-analysis-classic-tx-execution.md` Finding 2: `check_operation_signatures` iterates
all TX signatures and calls `ed25519_dalek::verify` for each signature, for every operation in
the TX. For a 100-op TX with 1 signature, this means 100 Ed25519 verifications. Each takes
~40μs → ~4ms of crypto overhead per TX, ~80ms across 20 such TXs/ledger.

stellar-core has the same pattern, so this is not a henyey-specific regression.

### Fix

In `check_signature_from_signers` (`crates/ledger/src/execution/signatures.rs`), extend
`SignatureTracker` to cache the verified weight per signature on first verification:

```rust
for (sig_idx, sig) in tracker.signatures.iter().enumerate() {
    if tracker.used[sig_idx] {
        // Already verified and matched this signer; reuse cached weight.
        if let Some(weight) = tracker.verified_weights[sig_idx] {
            accumulated_weight += weight;
        }
        continue;
    }
    if let Ok(weight) = verify_signature_with_key(sig, &tracker.tx_hash, signer_key) {
        tracker.verified_weights[sig_idx] = Some(weight);
        tracker.used[sig_idx] = true;
        accumulated_weight += weight;
    }
}
```

This diverges from stellar-core in which verification calls fire, but the result (accumulated
weight, success/failure) must be identical. Requires careful testing on the multi-signer,
multi-threshold edge cases.

**Estimated savings**: 30–50% reduction in `fee_seq_us` for multi-op TXs
= ~5–10ms/ledger for the 20 high-op TXs.

---

## What Cannot Be Fixed (Structural Constraints)

### Deferred XDR serialization (O7, INVESTIGATED AND REJECTED)

O7 attempted to defer `build_entry_changes_with_state_overrides` from `pre_apply` to
`apply_body` to avoid XDR serialization of AccountEntry during the fee/seq hot path.
Result: +43ms regression. Root cause: the AccountEntry data is hot in CPU cache immediately
after `flush_modified_entries`; deferring to after op execution makes it cold (Soroban
WASM + contract data loads evict it). The lesson: eager is faster than lazy when the data
is cache-warm. See `perf-analysis-classic-tx-execution.md` § O7.

### stellar-core's LedgerTxnRoot global account index

stellar-core maintains a RAM-resident index of all loaded ledger entries since startup. Any
account loaded in ledger 100 is a zero-cost pointer dereference in ledger 110. Replicating
this in henyey would require abandoning the snapshot model and maintaining a mutable
in-memory ledger state across all ledger closes. This is a fundamental architectural
difference — P1 approximates it via explicit hot-key tracking without the full redesign.

---

## Implementation Priority

1. **P1 first**: cross-ledger hot-key prefetch is the most impactful, lowest-risk change.
   It requires only ~60 lines across 2 files and has no correctness risk (prefetch is
   read-only and idempotent).

2. **Measure P1**: benchmark against the 1000-ledger range before proceeding.

3. **P2 if P1 ≥ 5ms**: wire `collect_dex_asset_pairs` + `collect_seller_keys_for_pairs`
   into `run_transactions_on_executor`. The infrastructure already exists.

4. **P3 last**: requires careful correctness testing; only worth doing if P1+P2 leave
   a measurable gap.

---

## Benchmark Reference

Baseline (post-O7-revert, post-8104131 cache-through):
- Mean: ~330ms/ledger
- Median: ~396ms/ledger
- Range: 61348953–61349952 (1000 ledgers, protocol 25)

```bash
RUST_LOG=info ~/data/<session>/cargo-target/release/henyey --mainnet verify-execution \
  --from 61348953 --to 61349952 --cache-dir ~/data/mainnet/
```
