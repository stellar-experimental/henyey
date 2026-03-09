# Ledger Close Performance Analysis: Henyey vs Stellar-Core

## Summary

Henyey closes ledgers **55% slower on mean** and **74% slower on median** than stellar-core v25.1.1,
when processing the same mainnet ledgers (61348953–61349952).

| Metric    | stellar-core | henyey  | Ratio  |
|-----------|-------------|---------|--------|
| mean      | 217.6ms     | 337.9ms | 1.55x  |
| median    | 233.4ms     | 405ms   | 1.74x  |
| p75       | 271ms       | 447ms   | 1.65x  |
| p95       | 331ms       | 514ms   | 1.55x  |

The bottleneck is **tx execution** (97% of henyey's time). Henyey's **commit phase is actually faster**
than stellar-core (14ms vs ~37ms) because stellar-core writes to SQLite while henyey writes only
to the bucket list.

---

## Measurement Setup

### stellar-core v25.1.1 (docker, mainnet catchup)

```
stellar-core catchup --metric ledger.ledger.close --metric ledger.transaction.total-apply \
  61349952/1025
```

Key metrics:
```
ledger.ledger.close:
  count=1025  mean=217.612ms  median=233.437ms  p75=271.148ms  p95=331.37ms
  min=63.0ms  max=504.2ms

ledger.transaction.total-apply:
  mean=179.723ms  median=199.008ms
```

### henyey (verify-execution, same ledger range)

```
RUST_LOG=info henyey --mainnet verify-execution \
  --from 61348953 --to 61349952 --cache-dir ~/data/b5e87aee/cache-1
```

Key stats (Python analysis of log):
```
count=1000  min=62ms  max=766ms  mean=337.9ms  median=405ms
p75=447ms   p95=514ms  p99=649ms
avg tx/ledger=296.9
```

### henyey per-phase breakdown (111 ledger sample, RUST_LOG=debug)

```
TOTAL:                403.1ms avg
  tx_exec (all txs):  389.1ms (97%)
    classic_exec:      95.8ms (24% of tx_exec)
    soroban_exec:     227.9ms (57% of tx_exec)
    overhead*:         ~65ms  (16% of tx_exec)
  commit_setup:         1.1ms
  eviction:             3.4ms
  soroban_state_update: 1.3ms
  add_batch:            3.7ms
  hot_archive:          0.0ms
  build_header:         0.9ms
  commit_close:         0.8ms
  build_meta:           1.8ms
  bucket_lock_wait:     0.0ms
```

*overhead = tx_exec_total − classic_exec − soroban_exec; includes pre-deduct fees pass.

---

## Phase-by-Phase Comparison

### Transaction execution (the bottleneck)

| Phase             | stellar-core | henyey  | Delta    |
|-------------------|-------------|---------|----------|
| Fee deduction     | ~30ms (est) | ~65ms   | +35ms    |
| Classic tx exec   | ~72ms (est) | 95.8ms  | +24ms    |
| Soroban tx exec   | ~78ms (est) | 227.9ms | +150ms   |
| **Total tx_exec** | **~180ms**  | **389ms** | **+209ms** |

stellar-core estimates derived from `total-apply=180ms` and known classic/Soroban split.

### Commit phase

| Phase             | stellar-core | henyey  | Delta   |
|-------------------|-------------|---------|---------|
| Commit (est)      | ~37ms       | ~14ms   | **−23ms** |

Henyey's commit is faster because stellar-core must write to SQLite + bucket list while henyey
writes only to the bucket list.

---

## Root Cause Analysis

### R1: InMemorySorobanState not wired into execution reads (HIGH IMPACT)

**Observed:** Every Soroban TX calls `load_soroban_footprint(snapshot, footprint)` which calls
`snapshot.load_entries(all_keys)`. For Soroban keys (ContractData, ContractCode, Ttl),
`load_entries()` explicitly skips the `prefetch_cache` and goes directly to the bucket list:

```rust
// SnapshotHandle::load_entries (snapshot.rs:394)
} else if !is_soroban_key(key) {
    // check prefetch_cache for non-soroban keys...
} else {
    remaining.push(key.clone()); // soroban keys ALWAYS go to batch_lookup_fn
}
```

Similarly, `SnapshotHandle::prefetch()` skips all Soroban keys:

```rust
// snapshot.rs:499
if is_soroban_key(key) {
    continue; // "in-memory via InMemorySorobanState"
}
```

**The problem:** The comment "in-memory via InMemorySorobanState" was **never fully implemented**.
`InMemorySorobanState` is maintained in `LedgerManager::soroban_state` but is **not accessible from
the execution path**. So every Soroban footprint read does a full 22-bucket scan (with bloom filter
optimization):

- Per footprint key: check 22 buckets × bloom filter + (1 bucket) × index lookup + mmap read
- Even with OS page cache hot: ~15–30μs per key for the scan machinery
- Typical Soroban TX footprint: 10–20 keys (data + code + TTL entries)
- Per TX overhead: ~200–600μs from bucket scans alone
- For 119 Soroban TXs/ledger: **24–71ms** wasted per ledger

**stellar-core's approach:** `InMemorySorobanState` is the primary Soroban state backend during
execution. Reads are O(1) HashMap lookups. The bucket list is only consulted for entries not in
the in-memory state (rare).

**Fix:** Pass a reference to the ledger's `InMemorySorobanState` (as a read guard) into
`load_soroban_footprint`. Use it as the primary lookup for ContractData/Code entries. Fall back to
the bucket list only for entries not found there (entries created within this ledger that haven't
yet been committed to InMemorySorobanState).

---

### R2: Duplicate SHA-256 key hashing per footprint entry (MEDIUM IMPACT)

`load_soroban_footprint` computes SHA-256(XDR(key)) for each ContractData/Code key to derive the
TTL key:

```rust
// mod.rs:1248
let key_bytes = key.to_xdr(...)?;
let key_hash = Hash(Sha256::digest(&key_bytes).into());
let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });
```

Then, inside `execute_host_function_p25` → `snapshot.get_local(key)` → `get_entry_ttl()`:

```rust
// host.rs:619 (called for EACH footprint key again)
fn get_entry_ttl(state: &LedgerStateManager, key: &LedgerKey, ...) -> Option<u32> {
    let key_hash = compute_key_hash(key); // XDR serialize + SHA-256 again!
    state.get_ttl_at_ledger_start(&key_hash)
}
```

The SHA-256 is computed at least twice per Soroban footprint entry (ContractData/Code):
1. In `load_soroban_footprint` to generate the TTL key
2. In `get_entry_ttl` called from `execute_host_function_p25`

At ~3μs per SHA-256-of-XDR and 10 Soroban keys per TX: 10 × 2 × 3μs = 60μs per TX,
or **~7ms per ledger** for 119 Soroban TXs. Small but free to fix.

---

### R3: Fee deduction pass lacks prefetch (MEDIUM IMPACT)

`pre_deduct_all_fees_on_delta()` processes all ~297 TXs' fee sources before any TX body executes.
It modifies the `LedgerDelta` directly. The fee source accounts must be loaded from the snapshot.

Unlike the classic/Soroban per-phase prefetch that calls `snapshot.prefetch(&keys)` first, the
global fee pass does not have its own bulk prefetch. Each source account lookup may fall through
to the bucket list.

In stellar-core, `processFeesSeqNums()` uses `LedgerTxn` which accumulates loaded entries across
all TXs — once account A is loaded for TX 1's fee, TX 2's fee deduction finds it in `LedgerTxn`
cache at O(1) cost. In henyey, each source account needs to be loaded from the snapshot's
`prefetch_cache`, which is populated lazily.

**Fix:** Add a dedicated prefetch pass for all fee source accounts before calling
`pre_deduct_all_fees_on_delta()`.

---

### R4: Classic executor state doesn't persist useful Soroban context (LOW IMPACT)

The classic executor (`executor_ref`) persists across ledgers (for the offer cache) but its
non-offer state is cleared via `advance_to_ledger_preserving_offers`. Account entries loaded
during fee processing are available during classic TX execution (accumulated in
`LedgerStateManager`), but not shared with Soroban clusters.

Each Soroban cluster gets a fresh `LedgerStateManager` containing only `prior_stage.entries`.
This means fee-deducted source accounts (already in the classic executor) must be re-fetched from
the snapshot for Soroban fee processing.

---

### R5: `load_soroban_footprint` cannot reuse entries across clusters (LOW IMPACT)

Each cluster's `LedgerStateManager` starts empty (except for prior_stage entries). When cluster A
and cluster B both need ContractData entry X, both clusters independently fetch X from the bucket
list. This is correct (clusters are independent) but wastes I/O.

A shared read-only InMemorySorobanState solves this — both clusters would find X in the shared
in-memory map.

---

### R6: Two duplicate P25 invoke_host_function implementations (TECHNICAL DEBT)

There are two separate P25 implementations:
- `crates/tx/src/soroban/protocol/p25.rs::invoke_host_function()` — passes `None` for module cache
- `crates/tx/src/soroban/host.rs::execute_host_function_p25()` — correctly passes module cache

The execution path uses `host.rs` (correct). The `p25.rs` standalone function appears unused in
the normal execution path but creates confusion and maintenance risk. It should be removed or
clearly marked as test-only.

---

## Optimization Plan

### O1: Wire InMemorySorobanState into execution reads (HIGH)

**Estimated savings: 30–70ms per ledger**

Pass `Arc<SharedSorobanState>` (a read guard wrapper) into the execution context. In
`load_soroban_footprint`, look up ContractData and ContractCode entries from `InMemorySorobanState`
first, falling back to the bucket list only for entries created within this ledger (which are
already in `executor.state` anyway).

```
Before: load_soroban_footprint → snapshot.load_entries → bucket_list_scan (22 buckets)
After:  load_soroban_footprint → InMemorySorobanState (O(1) HashMap) → bucket_list (fallback)
```

For TTL lookups in `get_entry_ttl()`, use `InMemorySorobanState::ttl_data` directly instead of
going to `LedgerStateManager::get_ttl_at_ledger_start()` (which needs the entry to be loaded from
the bucket list first).

**Implementation steps:**
1. Add `soroban_state: Arc<SharedSorobanState>` to `SorobanContext`
2. In `apply_transactions`, take `self.manager.soroban_state.read()` and pass via `SorobanContext`
3. In `load_soroban_footprint`, accept optional `&InMemorySorobanState` and use it for lookups
4. In `execute_host_function_p25`, pass `InMemorySorobanState` into `get_entry_ttl` and
   `get_local` to avoid SHA-256 and bucket list queries

---

### O2: Add Soroban entry prefetch before fee deduction pass (MEDIUM)

**Estimated savings: 15–30ms per ledger**

Before `pre_deduct_all_fees_on_delta()`, collect all fee source account keys for all TXs
(both classic and Soroban phases) and batch-load them in a single bucket list pass.

```rust
// Before pre_deduct_all_fees_on_delta, add:
let fee_keys: Vec<LedgerKey> = collect_all_fee_source_keys(&classic_txs, &soroban_phase);
snapshot.prefetch(&fee_keys)?;
```

This mirrors stellar-core's `processFeesSeqNums()` which benefits from `LedgerTxn`'s lazy-load
caching — the first fee deduction loads the account, all subsequent fee deductions for the same
account are cache hits.

---

### O3: Cache TTL key hash per footprint entry (LOW)

**Estimated savings: 5–10ms per ledger**

Compute SHA-256(XDR(key)) once per footprint entry and pass it through. Currently computed
separately in `load_soroban_footprint` (to build TTL key) and again in `get_entry_ttl` (for TTL
lookup). Pass the pre-computed hash to avoid duplicate computation.

---

### O4: Allow Soroban entries in SnapshotHandle prefetch_cache (LOW-MEDIUM)

**Estimated savings: 5–15ms per ledger**

Remove the `is_soroban_key` bypass in `SnapshotHandle::load_entries()` and `prefetch()`. The
per-stage prefetch (`snapshot.prefetch(&keys_vec)` in `execute_soroban_parallel_phase`) would then
batch-load all Soroban footprint entries in one bucket list pass, populating the `prefetch_cache`.
Subsequent per-TX loads would be cache hits.

This is simpler than O1 but has higher overhead (still does one bucket scan per stage vs O1's
O(1) InMemorySorobanState lookup).

---

### O5: Remove or isolate dead-code P25 path (CLEANUP)

Remove `crates/tx/src/soroban/protocol/p25.rs::invoke_host_function()` if unused, or add a clear
`#[cfg(test)]` gate. The correct P25 path is `execute_host_function_p25` in `host.rs`.

---

### O6: Profile with flamegraph/perf (DIAGNOSTIC)

Add cargo feature flags to enable `perf` profiling:

```
cargo build --release --bin henyey
perf record -g --call-graph=dwarf -- \
  henyey --mainnet verify-execution --from 61348953 --to 61349952
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

This will reveal whether:
- The Soroban host Wasm execution itself accounts for most time (expected)
- The SHA-256 hashing is a hotspot
- The bucket scan appears in the flame graph

---

## Expected Impact

| Optimization | Estimated Savings | Confidence |
|-------------|------------------|------------|
| O1: InMemorySorobanState reads | 30–70ms | Medium |
| O2: Pre-deduct fee prefetch | 15–30ms | Medium |
| O3: Cache TTL key hash | 5–10ms | High |
| O4: Soroban entry prefetch_cache | 5–15ms | Medium |
| **Total** | **55–125ms** | Medium |

After all changes, expected ledger close time:
- Best case: 338 − 125 = **213ms** (matching stellar-core)
- Likely case: 338 − 80 = **258ms** (still ~18% slower)

The remaining gap (if any) is likely from:
- stellar-core's `processFeesSeqNums` using LedgerTxn's hierarchical caching (persistent across
  TXs within a ledger, impossible to fully replicate without refactoring to a similar model)
- Different thread pool scheduling (stellar-core uses work-stealing; henyey uses Tokio spawn_blocking)
- Profiling may reveal additional hotspots

---

## Implementation Order

1. **O6 (profile)** — take one flamegraph run before any changes to validate this analysis
2. **O3 (SHA-256 cache)** — trivial change, immediate measurable savings
3. **O2 (fee prefetch)** — self-contained, moderate savings
4. **O4 (Soroban prefetch_cache)** — remove is_soroban_key bypasses, confirm correctness
5. **O1 (InMemorySorobanState)** — highest ROI, most architectural work, do last
6. **O5 (cleanup)** — remove dead P25 path after O1 lands

---

## Files to Modify

| File | Change |
|------|--------|
| `crates/ledger/src/snapshot.rs` | O4: remove is_soroban_key bypasses in prefetch/load_entries |
| `crates/ledger/src/execution/mod.rs` | O1: pass InMemorySorobanState to load_soroban_footprint; O3: cache key hash |
| `crates/ledger/src/execution/tx_set.rs` | O2: add fee key prefetch; O1: pass soroban_state via SorobanContext |
| `crates/ledger/src/manager.rs` | O1: pass soroban_state read guard into apply_transactions |
| `crates/tx/src/soroban/host.rs` | O1: accept InMemorySorobanState in get_entry_ttl/get_local |
| `crates/tx/src/soroban/protocol/p25.rs` | O5: remove or gate with #[cfg(test)] |
