# Ledger Close v2: Ground-Up Design

## Goal

Close ledgers in ≤200ms at mainnet traffic levels, matching stellar-core v25 throughput, while
preserving full protocol parity and henyey's architectural advantages (bucket-list-only state,
fully in-memory offer index, parallel Soroban).

Current baseline: ~330ms mean, ~396ms median (ledger range 61348953–61349952, protocol 25).
Target: ≤200ms mean — closing the ~130ms structural gap.

---

## Design Principles

1. **Read cost must be paid once.** Every entry read from disk should be cached in a layer that
   survives to the next access. No entry should require a bucket list scan twice within the same
   ledger close or across consecutive ledgers.

2. **Soroban state is always O(1).** `InMemorySorobanState` is the authoritative source for all
   ContractData/ContractCode/TTL reads. It is already wired into the execution path. No changes
   needed here.

3. **Classic entry reads amortize across ledgers.** Unlike Soroban (covered by persistent
   in-memory state), classic entries (accounts, trustlines, claimable balances, liquidity pools)
   are read from the bucket list on cache miss. Hot classic entries must be promoted to a
   cross-ledger warm tier to avoid repeated disk I/O.

4. **Nomination time is free execution time.** SCP nomination runs for 1–5 seconds before a
   tx_set is externalized. The entries needed to execute that tx_set are statically determinable
   from the envelope. Prefetch during nomination, not after.

5. **Persist off the critical path.** Bucket list merges, file I/O, and archive uploads do not
   block ledger close. They run concurrently with the next ledger's execution.

6. **Correctness is not optional.** All caches are read-only or copy-on-write. No cache entry
   is ever used to satisfy a write path without going through the `LedgerDelta`. Refunds are
   computed and applied post-phase, in the same order as stellar-core's
   `processPostTxSetApply`.

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                         Ledger Close Pipeline                         │
│                                                                        │
│  SCP Nomination              Externalization          Commit           │
│  ─────────────               ─────────────────        ──────          │
│  [speculative prefetch]  →   [apply tx_set]      →   [async persist] │
│      ↓                           ↓                        ↓           │
│  WarmCache               MaterializedView          BucketList merge   │
│  (pre-warm hot entries)  (O(1) read layer)         (off critical path)│
└──────────────────────────────────────────────────────────────────────┘
```

### Components

| Component | Scope | Replaces / Complements |
|-----------|-------|------------------------|
| `WarmCache` | Cross-ledger, classic entries | Complements `prefetch_cache`; fills disk I/O gap for DiskBacked levels |
| `MaterializedView` | Per-ledger | Replaces per-TX bucket list scans with a single pre-built HashMap |
| `SpeculativePrefetch` | Per-SCP round | Front-loads bucket list reads to nomination time |
| `InMemorySorobanState` | Node lifetime | Already implemented; O(1) for all Soroban reads |
| `OfferIndex` | Node lifetime | Already implemented; O(log n) DEX best-offer lookup |
| `OfferSellerStore` | Node lifetime | Extends OfferIndex; O(1) account+trustline reads for all offer sellers |
| `AsyncPersist` | Background | Decouples commit from execution latency |

---

## Component 1: WarmCache (Classic Entry Cross-Ledger Cache)

### Problem

The bucket list has 11 levels. Level 0 uses `BucketStorage::InMemory` — all entries modified in
the last few ledgers are in memory with an O(1) hash index. Levels 1–10 use `BucketStorage::DiskBacked`
— a compact hash→file_offset index is in memory, but the actual XDR entry data is on disk.

A classic entry that was accessed in ledger N but not modified since ledger N-K has drifted into
levels 2–4. Even though the disk index is O(1), every access requires loading the XDR from disk.
The OS page cache helps but is not reliable under memory pressure.

The `prefetch_cache` and `LedgerStateManager` are both per-ledger. Hot classic entries (DEX
offer sellers, sponsor accounts, popular payment destinations) must be re-fetched from disk on
every ledger. At ~150μs per bucket list scan and ~1021 dynamic misses/ledger, this costs ~150ms
of scattered disk access.

### Design

`WarmCache` is a bounded, eviction-safe LRU cache of `LedgerKey → LedgerEntry` scoped to classic
entry types (Account, Trustline, ClaimableBalance, LiquidityPool). It is separate from
`InMemorySorobanState` (which covers Soroban) and `OfferIndex` (which covers offers).

```rust
pub struct WarmCache {
    /// Random-eviction cache. Not LRU — random eviction prevents deterministic eviction
    /// attacks where an adversary submits TXs referencing N+1 unique entries to guarantee
    /// eviction of a specific hot entry. Matches stellar-core's RandomEvictionCache policy.
    entries: HashMap<LedgerKey, (LedgerEntry, u32)>, // entry + ledger_seq_last_seen
    capacity: usize,
    stats: WarmCacheStats,
}
```

**Capacity:** 200k entries × ~500 bytes average = ~100MB. Configurable via
`[ledger] warm_cache_entries = 200000` in the TOML config.

**Population:** At the end of each `run_transactions_on_executor`, iterate the
`prefetch_cache` and insert all cache-through entries (those loaded dynamically from the bucket
list) into `WarmCache`. Updated entries (modified by this ledger's delta) are also written back
to reflect the committed state.

**Lookup:** In `run_transactions_on_executor`, before building the static prefetch list,
check `WarmCache` for all static keys. For each hit, inject the entry directly into
`prefetch_cache` without a bucket list scan. Dynamic lookups (cache-through path) also check
`WarmCache` before the bucket list.

**Eviction:** Power-of-two-choices — when the cache is at capacity, pick two entries uniformly
at random and evict whichever has the older `last_seen` ledger_seq. This matches stellar-core's
`RandomEvictionCache` exactly (`RandomEvictionCache.h:72-90`): better than pure random (avoids
evicting a recently-promoted entry) without LRU's deterministic eviction path (an attacker
cannot guarantee eviction of a specific entry by constructing a particular access sequence).

Entries not seen in `warm_cache_ttl_ledgers` (default: 1000) are additionally eligible for
proactive eviction during the end-of-ledger update pass.

**Invalidation:** On ledger commit, entries modified by `LedgerDelta` are updated in `WarmCache`
with the committed value and current ledger_seq. Deleted entries are removed immediately.

### Relation to bucket list in-memory levels

WarmCache is NOT redundant with the bucket list's level 0 InMemory storage:

- Level 0 covers only entries **modified** in the last few ledgers (at ledger N, level 0 holds
  changes from ledgers N, N-1, N-2, etc. up to the last spill).
- WarmCache covers entries **read** frequently regardless of when they were last written.
- An account that was last modified 10,000 ledgers ago (in level 3, DiskBacked) but is the
  destination of a payment every ledger would not be in level 0 but would stay hot in WarmCache.
- WarmCache provides a single O(1) lookup that bypasses the full level scan (checking 22 bucket
  heads) and avoids disk I/O for DiskBacked levels.

The effective working set for WarmCache is the "frequently read, rarely written" slice of the
classic keyspace. For a mainnet validator this is primarily: fee-source accounts, DEX offer
seller accounts, trustlines for high-volume assets, and sponsor accounts.

### Expected impact

- Convert ~400–700 dynamic bucket list misses/ledger to WarmCache hits (O(1), no disk I/O)
- Net savings estimate: **15–30ms/ledger** (after amortizing cache population cost)

---

## Component 2: MaterializedView (Per-Ledger Read Layer)

### Problem

`run_transactions_on_executor` performs a static prefetch of all statically-determinable keys
upfront, then falls through to the bucket list for dynamic keys (with cache-through). The
prefetch populates `prefetch_cache` (a flat HashMap), which is consulted before every bucket
list access.

This works but has three inefficiencies:

1. The static prefetch and dynamic cache-through are separate code paths. Logic is duplicated
   in `SnapshotHandle::get_entry`, `load_entries`, and the fallback.
2. `prefetch_cache` stores `LedgerEntry` by XDR-serialized `LedgerKey` bytes (not by typed key),
   requiring XDR round-trips for key comparison.
3. There is no unified "current state" view that combines (a) entries in the prefetch cache,
   (b) modifications in the `LedgerDelta`, and (c) the bucket list fallback into a single O(1)
   read. Code that needs "what does entry X look like right now?" must compose these sources
   manually.

### Design

`MaterializedView` is a per-ledger, read-through, write-tracked view of ledger state:

```rust
pub struct MaterializedView {
    /// Baseline entries: prefetch cache + WarmCache hits + bucket list fallback.
    /// Populated upfront from static prefetch + seeded from WarmCache.
    baseline: HashMap<LedgerKey, Option<LedgerEntry>>, // None = confirmed absent
    /// Live delta: modifications applied so far this ledger.
    /// Checked before baseline on every read.
    delta_overlay: HashMap<LedgerKey, DeltaEntry>, // Create/Update/Delete
    /// Snapshot for fallback on bucket list miss.
    snapshot: SnapshotHandle,
}

impl MaterializedView {
    /// Get the current state of `key`, accounting for delta modifications.
    pub fn get(&mut self, key: &LedgerKey) -> Option<&LedgerEntry> {
        if let Some(entry) = self.delta_overlay.get(key) {
            return entry.current(); // None if deleted
        }
        self.baseline.get_or_load(key, &self.snapshot)
    }
}
```

**Construction:** At the start of `run_transactions_on_executor`:
1. Collect all static keys (fee sources, TX apply keys)
2. Resolve from WarmCache → prefetch_cache → bucket list (single batched pass)
3. Insert resolved entries into `MaterializedView::baseline`

**During execution:** TX code calls `view.get(key)` instead of the current mixture of
`executor.state.get_account()`, `snapshot.get_entry()`, etc. The view handles the priority chain
internally.

**Delta writes:** `LedgerDelta` writes are mirrored into `delta_overlay` so subsequent reads
within the same ledger see the committed state immediately.

**At ledger end:** `baseline` entries that were loaded dynamically (via bucket list fallback) are
promoted to `WarmCache`. The view is dropped; WarmCache retains the hot entries.

### Delta coalescing in MaterializedView

`delta_overlay` applies the same coalescing rules as the current `LedgerDelta`:
- Create + Update → Create (with latest value)
- Create + Delete → absent (removed from overlay)
- Update + Delete → Delete

This means `MaterializedView::get` always returns the correct current state for any entry
regardless of how many times it has been modified within the ledger.

### Expected impact

- Eliminates the dual-path complexity (prefetch_cache + fallback), reducing maintenance risk
- Enables cleaner implementation of speculative prefetch (Component 3)
- Minor direct performance gain (~5ms) from eliminating XDR re-serialization for key lookups
- Main value: structural foundation for Components 3 and correct cache coherence

---

## Component 3: Speculative Prefetch During SCP Nomination

### Problem

The current pipeline is:

```
tx_set externalized → prefetch all static keys → execute TXs
```

The prefetch (~20–40ms) happens after the tx_set is known. SCP nomination takes 1–5 seconds
before externalization. During nomination, the node knows the candidate tx_sets (via
`nominate()` calls). The entries needed to execute those TXs are determinable from envelopes.

### Design

When `Herder::nominate()` is called for a candidate tx_set, extract static keys (fee sources,
apply keys) in a background task and pre-warm them into WarmCache:

```rust
// In Herder::nominate(), after validating the tx_set:
let keys = collect_static_keys_for_txset(&candidate_txs);
self.prefetch_scheduler.submit(keys, self.ledger_seq);
```

`PrefetchScheduler` runs as a background Tokio task. It batches keys from successive nomination
rounds (deduplicated), issues a single bulk bucket list scan, and writes results to WarmCache.

**Key constraint:** The speculative prefetch writes only to WarmCache, never to any execution
state. WarmCache is read-only during execution (writes only happen at ledger start from the
static prefetch resolution and at ledger end from cache-through promotion). This ensures that
a mismatch between the nominated and externalized tx_set does not corrupt execution state.

**When tx_set is externalized:** `MaterializedView` construction reads from WarmCache, finding
the speculatively pre-warmed entries. The bulk prefetch scan at execution time becomes a
WarmCache hit scan — effectively free.

**Coverage:** Speculative prefetch covers ~80–90% of static keys (fee sources, source accounts,
declared destinations). Dynamic keys (DEX offer sellers discovered during crossing) are not
covered but are handled by WarmCache + cross-ledger hot-key tracking.

### Expected impact

- Moves the ~20–40ms static prefetch off the critical path (overlaps with SCP nomination)
- Combined with WarmCache, the execution-time prefetch becomes O(1) WarmCache lookups
- Net savings estimate: **20–40ms/ledger** (on top of WarmCache savings)

---

## Component 3b: OfferSellerStore (Extend OfferIndex with Seller Deps)

### Problem

`OfferIndex` knows the best offer for every asset pair at O(log n), but knows nothing about the
seller behind that offer. When offer crossing begins, `apply_manage_offer` (and the path-payment
engine) must load the seller's AccountEntry and up to two TrustLineEntries (buying + selling
assets, if non-native) from the bucket list. These are pure dynamic keys — not determinable
from the TX envelope — so they miss the static prefetch and arrive as scattered cache-through
loads during execution.

This is exactly the gap that stellar-core's `populateEntryCacheFromBestOffers` addresses: after
each batch of offers is loaded from SQL, it prefetches seller accounts + trustlines into
`mEntryCache`. henyey's P2 proposal (demand-driven prefetch at ledger start) approximates this.
`OfferSellerStore` does it permanently.

### Design: full-coverage (all sellers)

Cache accounts and trustlines for **every account that has at least one live offer** in
`OfferIndex`. The invariant is simple: `OfferSellerStore` membership mirrors `OfferIndex`
membership. Any offer crossing — regardless of depth, pair, or frequency — finds its seller
deps in RAM.

```rust
/// Per-seller dependency bundle: one account + all trustlines needed to cross this seller's offers.
pub struct SellerDeps {
    pub account: AccountEntry,
    /// Keyed by asset — only assets that appear in this seller's offers.
    pub trustlines: HashMap<Asset, TrustLineEntry>,
}

/// Companion to OfferIndex. Maintains account + trustline entries for every live offer seller.
/// Membership mirrors OfferIndex: an entry exists iff the seller has at least one live offer.
pub struct OfferSellerStore {
    /// seller_id bytes → deps
    entries: HashMap<[u8; 32], SellerDeps>,
}
```

`OfferSellerStore` is a field of `TransactionExecutor` alongside `OfferIndex`. It is populated
from the same `load_orderbook_offers()` call at startup and updated by the same delta-apply
path.

### Memory cost

- ~150K unique offer sellers on mainnet
- Per seller: AccountEntry ~800B + avg 2 TrustLineEntries × ~600B = ~2KB
- Total: 150K × 2KB ≈ **~300–400MB**

This is comparable to `OfferIndex` (~180MB) and well within the budget of a validator node
(typically 32–64GB RAM). The full-coverage variant eliminates the partial-coverage edge cases
of a top-K design without requiring WarmCache as a safety net for the long tail.

### Startup cost

At `load_orderbook_offers()`, after inserting all offers into `OfferIndex`, collect all unique
seller IDs and bulk-fetch their accounts and trustlines in a single batched bucket list scan.
At ~150K sellers × 3 entries = ~450K entries, this adds **~1–2s to startup** — small against
the existing ~125s startup time, and can be parallelized with existing scan passes.

### Maintenance (per-ledger delta apply)

```rust
impl OfferSellerStore {
    /// Called after delta is committed. Updates deps for modified sellers and
    /// adds/removes entries as offers are created or fully deleted.
    pub fn update_from_delta(&mut self, delta: &LedgerDelta, offer_index: &OfferIndex) {
        // 1. Update account entries for modified accounts that are known sellers.
        for (id, account) in delta.modified_accounts() {
            if let Some(deps) = self.entries.get_mut(id) {
                deps.account = account.clone();
            }
        }
        // 2. Update trustlines for modified trustlines owned by known sellers.
        for (key, tl) in delta.modified_trustlines() {
            if let Some(deps) = self.entries.get_mut(&key.account_id) {
                deps.trustlines.insert(key.asset.clone(), tl.clone());
            }
        }
        // 3. Add newly-seen sellers (offers created this ledger).
        for seller_id in delta.new_offer_sellers() {
            if !self.entries.contains_key(seller_id) {
                // Fetch from snapshot; seller account was just created/loaded this ledger.
                if let Some(deps) = fetch_seller_deps(seller_id, snapshot) {
                    self.entries.insert(*seller_id, deps);
                }
            }
        }
        // 4. Remove sellers whose last offer was deleted this ledger.
        for seller_id in delta.deleted_offer_sellers() {
            if !offer_index.has_offers_for(seller_id) {
                self.entries.remove(seller_id);
            }
        }
    }
}
```

Per-ledger overhead: O(|delta.modified_accounts| + |delta.modified_trustlines| + |new/deleted sellers|).
For a typical 300-TX ledger: ~100 modified accounts/trustlines + ~10 new/deleted sellers = O(120)
operations — negligible.

### Read path

`apply_manage_offer` and `apply_path_payment_*` call `load_account(snapshot, seller_id)` and
`load_trustline(snapshot, seller_id, asset)`. These currently fall through to the bucket list
on miss. With `OfferSellerStore`, the call chain becomes:

```
load_account(seller_id)
  → LedgerStateManager (if already loaded this ledger)
  → OfferSellerStore::get_account(seller_id)   ← new, O(1), covers all sellers
  → WarmCache
  → bucket list (fallback, only for non-seller classic entries)
```

No change to the call sites — `load_account` / `load_trustline` in `TransactionExecutor` are
the bottleneck; the store lookup is inserted there.

### Relation to P2 (demand-driven prefetch)

`OfferSellerStore` supersedes P2. P2 prefetches top-N seller accounts/trustlines into the
snapshot's `prefetch_cache` at ledger start (paying a bucket list scan). With `OfferSellerStore`,
those deps are already in memory — the P2 prefetch scan becomes unnecessary. P2 should **not**
be implemented if `OfferSellerStore` is.

### Expected impact

- **Eliminates all bucket list scans for seller accounts/trustlines** — not just the top-K hot path
- Replaces P2 entirely with zero per-ledger prefetch work
- No dependency on WarmCache for seller coverage; WarmCache focuses on other hot classic entries
- Net savings estimate: **8–15ms/ledger**
- Memory cost: ~300–400MB

---

## Component 4: InMemorySorobanState (Existing, Wired)

**Status: Already implemented and wired into the execution path.**

`InMemorySorobanState` provides O(1) HashMap lookup for all ContractData, ContractCode, and TTL
entries. It is set on each `TransactionExecutor` via `set_soroban_state()` and consulted in
`load_soroban_footprint` before falling back to the bucket list.

Key facts:
- Rebuilt from the bucket list at node startup
- Updated incrementally on each ledger commit via `update_soroban_state()`
- Shared across all parallel Soroban clusters via `Arc<RwLock<InMemorySorobanState>>`
- TTL data is co-located with entries: `get_entry()` returns entry + TTL in a single lookup

No design changes needed here. The Soroban read path is already optimal.

---

## Component 5: Async Persist

### Current state

`commit_close()` in `LedgerManager` triggers bucket list `add_batch()` (writes modified entries
to the level-0 pending merge), `build_meta()` (constructs LedgerEntryChanges), and eventually
the level-merging background tasks. `add_batch` blocks synchronously on the bucket list lock.

At 14ms per ledger (henyey's commit is already faster than stellar-core's 37ms due to no
SQLite), this is low-priority but non-zero.

### Design

Decouple the persist phase from the critical path:

1. **Delta snapshot:** At ledger close, capture the committed `LedgerDelta` as an immutable
   snapshot (cheap clone of the entry maps — they are already `Arc`-wrapped).

2. **Background writer:** A dedicated `PersistTask` Tokio task receives the delta snapshot via
   a channel and applies it to the bucket list asynchronously:
   ```
   apply_transactions returns → ledger_seq+1 starts executing → PersistTask applies delta for ledger_seq
   ```

3. **Snapshot consistency:** The `SnapshotHandle` for ledger_seq+1 is constructed from the
   bucket list state AFTER ledger_seq's delta is applied. This requires the `PersistTask` to
   complete before `SnapshotHandle::new` for the next ledger. In practice: `PersistTask` for
   ledger N runs during SCP round N+1 nomination, completing before tx_set N+1 is externalized.

4. **Backpressure:** If `PersistTask` falls behind (delta channel depth > 1), revert to
   synchronous persist to avoid unbounded memory accumulation.

### Expected impact

- Removes ~14ms from the critical path on the nominal path (SCP round > 14ms, which is always
  true at mainnet — rounds take 3–7 seconds)
- No impact during catchup (where there is no SCP nomination gap)
- Net savings (live node): **~14ms/ledger**

---

## Fee Refund Processing (Existing Design)

**Status: Already implemented correctly.**

Fee refunds are computed per-TX during execution (`RefundableFeeTracker::refund_amount()`) and
applied in bulk after all phases complete (post-tx-set), in `crates/ledger/src/execution/tx_set.rs`:

```
Phase 1 (classic) executes → Phase 2 (Soroban) executes → post-phase refund loop applies all refunds
```

The post-phase refund loop (analogous to stellar-core's `processPostTxSetApply`):
- Classic: `delta.apply_refund_to_account()` + `delta.record_fee_pool_delta(-total_refunds)`
- Soroban: `executor.state.apply_refund_to_delta()` per cluster

No changes needed here. Refund ordering matches stellar-core.

---

## Read Path: Unified Lookup Priority

The complete read priority chain for a `LedgerKey` lookup during execution:

```
1. LedgerStateManager per-type map (current in-flight TX state)          [O(1), no I/O]
2. LedgerDelta delta_overlay (MaterializedView, committed this ledger)    [O(1), no I/O]
3. MaterializedView baseline (pre-materialized at ledger start)           [O(1), no I/O]
4. WarmCache (cross-ledger hot entries, classic only)                     [O(1), no I/O]
5. OfferSellerStore (account+trustline for all offer sellers)             [O(1), no I/O]
6. InMemorySorobanState (Soroban entries only)                            [O(1), no I/O]
7. OfferIndex (offer entries only)                                        [O(log n), no I/O]
8. Bucket list snapshot (fallback; disk I/O for DiskBacked levels)        [O(levels), disk]
   └─ On hit: write through to WarmCache + MaterializedView baseline
```

Steps 1–7 should cover >99% of accesses in steady state. Step 8 is the fallback for cold starts
and rare cache misses.

---

## Data Flow Through Ledger Close

```
SCP Nomination (1–5s)
  ├─ collect_static_keys(candidate_txset)
  └─ PrefetchScheduler → batch bucket list scan → WarmCache (background)

tx_set Externalized
  ├─ MaterializedView::new(static_keys, WarmCache, snapshot)   [~5ms, mostly WarmCache hits]
  ├─ pre_deduct_all_fees_on_delta()                            [~10ms, fee sources in MV]
  ├─ run_classic_sequential()                                  [~96ms, unchanged]
  │    └─ reads via MaterializedView; writes to LedgerDelta
  ├─ run_soroban_clusters_parallel()                           [~150ms, Soroban unchanged]
  │    └─ reads via InMemorySorobanState; writes to LedgerDelta
  ├─ post_phase_refunds()                                      [~1ms, already implemented]
  ├─ build_header() + build_meta()                             [~2ms]
  └─ commit_close() → PersistTask channel (async)             [~1ms to enqueue]

Background (overlaps with next SCP round)
  └─ PersistTask: apply LedgerDelta to bucket list             [~13ms, off critical path]

End of next SCP round
  └─ WarmCache: promote dynamic cache-through entries          [~1ms]
```

---

## Performance Projection

| Component | Mechanism | Estimated Savings | Confidence |
|-----------|-----------|------------------|------------|
| WarmCache | Cross-ledger classic entry cache; avoid ~500 disk reads/ledger | 15–30ms | Medium |
| OfferSellerStore | O(1) seller account+trustline for all sellers; eliminates entire class of misses | 8–15ms | High |
| Speculative prefetch | Move static prefetch to nomination time | 20–40ms | Medium |
| Async persist | Move bucket list write off critical path | ~14ms | High |
| MaterializedView | Structural; minor direct gain | ~5ms | High |
| **Total** | | **62–104ms** | Medium |

After all components, projected mean: 330 − 62 to 330 − 104 = **226–268ms**.

This approaches the stellar-core baseline of ~218ms. Classic execution remains sequential;
the remaining gap to stellar-core is expected to close further through WarmCache hits on the
classic path.

---

## Implementation Order

### Phase 1 (highest ROI, lowest risk)

1. **WarmCache** — implement `WarmCache` struct; wire into static prefetch resolution and
   cache-through promotion at ledger end. No changes to execution path.
2. **OfferSellerStore** — extend `OfferIndex` with `OfferSellerStore`; populate at
   `load_orderbook_offers()` startup; wire into `load_account`/`load_trustline` in
   `TransactionExecutor`; update from delta each ledger. Do NOT implement P2 — this supersedes it.
3. **Async persist** — add `PersistTask`; confirm snapshot consistency before wiring.
4. **Benchmark** — run 1000-ledger verify-execution before Phase 2.

### Phase 2 (medium risk)

4. **MaterializedView** — replace `prefetch_cache` + `LedgerStateManager` read paths with a
   unified `MaterializedView`. Keep `SnapshotHandle` as the bucket list fallback.
5. **Speculative prefetch** — wire `PrefetchScheduler` into `Herder::nominate`; validate that
   mismatched tx_sets (nominated but not externalized) don't corrupt state.

---

## Non-Goals

- **SQLite reintroduction.** The bucket-list-only state model is a feature, not a bug.
  stellar-core's SQLite layer adds commit latency and complexity; henyey's 14ms commit vs
  stellar-core's 37ms proves this out.

- **Protocol divergence.** All execution semantics, fee calculations, refund ordering, and
  ledger entry change encoding must remain identical to stellar-core p25.

- **WarmCache for Soroban entries.** Soroban entries are already covered by `InMemorySorobanState`
  at O(1) with zero disk I/O. Adding them to WarmCache would be redundant.

- **WarmCache for offers.** Offers are covered by `OfferIndex` (fully in-memory, incrementally
  maintained). Adding them to WarmCache would be redundant.

- **Parallel classic execution.** Classic TXs have no declared footprint. DEX operations (83%
  of mainnet classic TXs) have dynamic write sets (offer crossing, path routing through
  intermediate pairs). Conservative static clustering using asset-pair keys captures ~2.3x
  theoretical parallelism but misses intermediate path hops, and the DEX cluster bottleneck
  means realistic gain is ~15–25ms at high implementation cost. Not worth the complexity.

- **P2 demand-driven seller prefetch (from perf-analysis-structural-gap.md).** `OfferSellerStore`
  makes P2 redundant. P2 pays a per-ledger bucket list scan to prefetch top-N sellers; the store
  has those deps already in memory with no per-ledger work. Do not implement P2.

- **Top-K OfferSellerStore variant.** A hot-sellers-only (top-K per pair, ~16MB) design was
  considered but rejected in favour of full coverage. At ~300–400MB the full set of offer sellers
  fits comfortably in RAM, eliminates partial-coverage edge cases, and removes the dependency on
  WarmCache as a safety net for the long tail.

---

## Open Questions

1. **WarmCache capacity tuning.** 200k entries (~100MB) is the initial target. The right value
   depends on the hot entry count at mainnet scale. Instrument cache hit rate and tune after
   Phase 1 lands.

2. **Speculative prefetch false positive rate.** In practice, ~95% of nominated tx_sets are
   externalized unchanged. The 5% that change (due to quorum disagreement or tx expiry) generate
   wasted prefetches. These are harmless (WarmCache is idempotent) but consume I/O bandwidth.

3. **MaterializedView vs prefetch_cache migration.** The current `prefetch_cache` + `LedgerStateManager`
   combination is battle-tested. The migration to `MaterializedView` should preserve all existing
   correctness invariants. A full ledger range verify-execution run is required before declaring
   the migration complete.

---

## Benchmark Protocol

Every step is benchmarked using the same method before merging:

```bash
# Build release binary from the feature branch
CARGO_TARGET_DIR=~/data/ledger-close-v2/cargo-target \
  cargo build --release --bin henyey

# Run 100-ledger benchmark (ledgers 61349540–61349640, protocol 25)
RUST_LOG=info ~/data/ledger-close-v2/cargo-target/release/henyey \
  --mainnet verify-execution \
  --from 61349540 --to 61349640 \
  --cache-dir ~/data/ledger-close-v2/cache 2>&1 \
  | grep " INFO henyey_ledger" | grep -v "details" \
  | awk 'match($0,/ledger_seq=([0-9]+)/,a) && match($0,/close_time_ms=([0-9]+)/,b) {print a[1],b[1]}' \
  | awk '$1>=61349540 && $1<=61349640 {print $2}' | sort -n \
  | awk 'BEGIN{s=0;n=0} {a[n]=$1;s+=$1;n++} END{printf "n=%d mean=%.0f p50=%d p75=%d p95=%d\n",n,s/n,a[int(n*.5)],a[int(n*.75)],a[int(n*.95)]}'
```

Notes:
- First ledger after checkpoint load is discarded (cold startup).  The 101-ledger window
  61349540–61349640 gives 100 comparable closes after the executor and WarmCache have a
  chance to warm up.
- The WarmCache is empty at startup and warms naturally over the first ~20 ledgers.  Numbers
  stabilise after that.
- Compare `classic_exec_us` and `add_batch_us` from `RUST_LOG=debug` timing lines to isolate
  which phase improved.

---

## Benchmark Results

Benchmark window: ledgers **61349540–61349640** (101 closes, protocol 25).
Binary: release build, single-threaded executor, `--cache-dir` on local disk.

| Step | Branch / Commit | mean | p50 | p75 | p95 | Notes |
|------|----------------|------|-----|-----|-----|-------|
| Step 0: Baseline (main) | `bd8f3f7` | **314ms** | 377ms | 411ms | 533ms | Measured 2026-03-09 |
| Step 1: WarmCache | `5047e3b` | **303ms** | 374ms | 409ms | 490ms | −11ms mean; cache ~50% warm over window |
| Step 2: OfferSellerStore | pending | **304ms** | — | — | — | −10ms cumulative; 68K sellers, 691K entries; marginal over WarmCache in 101-ledger window |
| Step 3: Benchmark checkpoint | pending | TBD | TBD | TBD | TBD | |
| Step 4: MaterializedView | pending | TBD | TBD | TBD | TBD | |
| Step 5: SpeculativePrefetch | pending | TBD | TBD | TBD | TBD | live-validator only |
| Step 6: AsyncPersist | pending | TBD | TBD | TBD | TBD | |
