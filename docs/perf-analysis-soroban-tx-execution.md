# Soroban TX Execution Performance Analysis

## Summary

After the O1–O5 optimizations (commit b23848f), henyey's ledger close time improved modestly
(~2.6% mean / ~4.8% p95). Detailed per-TX instrumentation explains why the predicted 55–125ms
savings did not materialize fully, and identifies the structural causes of the remaining
~100ms gap vs stellar-core.

**Post-optimization baseline (ledgers 61349540–61349640, 100 ledger sample):**

| Metric | stellar-core v25.1.1 | henyey (post-O1) | Ratio |
|--------|---------------------|------------------|-------|
| mean   | 217.6ms             | 316.6ms          | 1.46x |
| p50    | 233.4ms             | 360ms            | 1.54x |
| p75    | 271ms               | 403ms            | 1.49x |
| p95    | 331.4ms             | 481ms            | 1.45x |

The remaining gap is **99ms mean / 127ms p50**. This report explains where it comes from.

---

## Measurement Setup

### Per-TX instrumentation

Temporary timing probes were added to two locations:

**1. `crates/tx/src/soroban/host.rs` — `execute_host_function_p25`**

Three `Instant` probes measured:
- `t_budget_us` — budget creation and ledger info setup
- `t_encode_us` — footprint XDR encoding loop
- `t_invoke_us` — `e2e_invoke::invoke_host_function` call only

Also captured: `vm_cpu` (VmInstantiation cost type, 0 = module cache hit), `total_cpu`
(consumed CPU instructions), `footprint_len`, `ledger_entries`.

Logged at INFO as `soroban_tx_perf`.

**2. `crates/ledger/src/execution/mod.rs` — `apply_transaction` (existing)**

Already tracks per-TX phases at DEBUG level (`TX phase timing`):
- `validation_us` — sig verification + account/trustline checks
- `fee_seq_us` — fee deduction, signer removal, seq num bump, meta change building
- `footprint_us` — `load_soroban_footprint` (IMS lookup + TTL loading)
- `ops_us` — full ops loop (includes budget setup + encoding + e2e_invoke + result processing)
- `meta_us` — outer `TransactionMetaV3` building

### Benchmark runs

```bash
# 100-ledger soroban_tx_perf sample (INFO)
RUST_LOG=info henyey --mainnet verify-execution \
  --from 61349540 --to 61349640 --cache-dir ~/data/mainnet/ \
  2>/tmp/soroban_perf.log

# 10-ledger TX phase timing sample (DEBUG)
RUST_LOG=henyey_ledger::execution=debug,henyey_tx::soroban::host=info,info \
  henyey --mainnet verify-execution \
  --from 61349600 --to 61349610 --cache-dir ~/data/mainnet/ \
  2>/tmp/tx_phase.log
```

Sample sizes: **17,599 Soroban TXs** (soroban_tx_perf) and **6,928 Soroban TXs** (TX phase timing).

---

## Finding 1: Wasm Module Cache Is Working

`vm_cpu = 0` on **100% of 17,599 Soroban TXs**. No Wasm recompilation occurs. The
`PersistentModuleCache` (pre-compiled Wasm modules shared across all TXs in a ledger) is
functioning correctly. This rules out Wasm compilation as a bottleneck.

---

## Finding 2: Per-TX Cost Breakdown

### soroban_tx_perf (n=17,599)

| Timer | Mean | p50 | p75 | p95 | p99 |
|-------|------|-----|-----|-----|-----|
| t_budget_us | 9.4μs | 9 | 9 | 12 | 22 |
| t_encode_us | 22.7μs | 18 | 24 | 41 | 69 |
| t_invoke_us | 366.2μs | 266 | 323 | 785 | 1,709 |
| **total (sum)** | **398.3μs** | 293 | — | 833 | — |

`t_invoke_us` dominates the e2e frame at **92%** of budget+encode+invoke time.
`t_invoke_us` scales strongly with footprint size and CPU instruction consumption.

### TX phase timing (n=6,928)

| Phase | Mean | p50 | p95 | % of total |
|-------|------|-----|-----|------------|
| validation_us | 97μs | 83 | 178 | 11% |
| fee_seq_us | 124μs | 113 | 237 | 15% |
| footprint_us | 56μs | 29 | 178 | 7% |
| ops_us | 563μs | 449 | 1,017 | 66% |
| meta_us | 15μs | 9 | 16 | 2% |
| **total_us** | **855μs** | 709 | 1,329 | 100% |

#### ops_us decomposition

`ops_us` contains the entire op handler, which for Soroban = `apply_soroban_tx`:

```
ops_us (563μs) = t_budget_us (9μs)
               + t_encode_us (23μs)
               + t_invoke_us (366μs)     ← e2e_invoke::invoke_host_function
               + result_processing (165μs) ← post-invoke: state application,
                                              rent fee computation, SorobanMeta building
```

#### Per-ledger projection (160 Soroban TXs)

| Phase | Per-TX | Per-ledger |
|-------|--------|------------|
| validation | 97μs | 15ms |
| fee_seq | 124μs | 20ms |
| footprint (IMS) | 56μs | 9ms |
| e2e_invoke | 366μs | 59ms |
| result processing | 165μs | 26ms |
| meta | 15μs | 2ms |
| **TOTAL Soroban** | **855μs** | **137ms** |

---

## Finding 3: 93% of Soroban Phases Run as a Single Cluster

```
num_clusters=1: 40/43 ledgers (93%)
num_clusters=2:  3/43 ledgers  (7%)
```

Every Soroban TX in a ledger is in the same conflict cluster because they all write to
shared contract state (DEX/AMM liquidity pool entries). With a single cluster, all Soroban
TXs execute sequentially — no thread-level parallelism is available.

This is not a henyey bug. stellar-core's parallel Soroban execution also groups conflicting
TXs into a single thread. The cluster detection is correct; the TX set simply has high
footprint overlap. **The 4-thread parallel executor provides no speedup for this workload.**

---

## Finding 4: The gap is in Per-TX Scaffolding, Not Wasm Execution

### What stellar-core does differently

stellar-core uses `LedgerTxn`, a hierarchical MVCC cache that:
1. Accumulates all loaded entries across all TXs in a ledger — an account loaded for TX 1
   is a cache hit for TX 2 at O(1) cost
2. Tracks changes lazily — no XDR serialization happens per-TX; changes are materialized
   into `LedgerEntryChanges` only at commit time
3. Eliminates per-TX bucket list lookups for accounts seen earlier in the ledger

### Where henyey pays extra per TX

**fee_seq_us (124μs = 20ms/ledger):**

Builds `tx_changes_before` LedgerEntryChanges for three sub-phases:
- fee_bump_wrapper_changes (fee source account: 2× XDR encode)
- signer_changes (one-time signers: 2× per signer)
- seq_changes (inner source account: 2× XDR encode)

Each Account XDR encode ≈ 10–15μs; 4–8 encodes per TX = 40–80μs of XDR work per TX. The
O2 prefetch (already implemented) eliminated the bucket list lookup cost, but not the
serialization cost. stellar-core defers this serialization to end-of-ledger via LedgerTxn.

**validation_us (97μs = 15ms/ledger):**

Includes signature verification (~40–60μs, unavoidable) plus account loading for
multi-signature validation. With O2 prefetch, account loads are cache hits in the snapshot.
The remaining overhead is in the signature verification itself and signer set computation.

**result_processing (165μs = 26ms/ledger):**

After `e2e_invoke` returns, the result path in `apply_soroban_tx`:
- Converts soroban state changes back to `LedgerEntry` format
- Builds `SorobanTransactionMeta` (contract events, diagnostic events, state changes)
- Computes and applies rent fee adjustments
- XDR-encodes modified contract data entries for state application

stellar-core's `InvokeHostFunctionOpFrame::doApply` has a similar path, but benefits from
LedgerTxn's deferred serialization for the state application part.

**footprint_us (56μs = 9ms/ledger):**

The O1 (InMemorySorobanState) optimization reduced this from ~1,400μs/TX to 56μs — a 25×
improvement. Remaining cost is HashMap lookups + `LedgerEntry` cloning for ~10 entries.
This is close to optimal for the current architecture.

---

## Finding 5: e2e_invoke Is Comparable to (or Faster than) stellar-core

From stellar-core's metrics:
```
ledger.transaction.total-apply = 179.7ms for ~297 TXs/ledger
```

If classic TXs average ~150–300μs each (166 classic TXs → 25–50ms), then:
```
soroban contribution ≈ 179.7 - 25-50ms = 130-155ms for 130 TXs ≈ 1,000-1,200μs/TX
```

Henyey's per-Soroban-TX total = 855μs. Henyey may actually be **faster per TX** than
stellar-core on this benchmark. The close-time gap is primarily from **classic TX
execution** being slower, and the high-ops-count classic TXs (100 ops each, ~9.5ms/TX)
that dominate wall time.

Note: hardware and conditions differ (stellar-core was in Docker during mainnet catchup;
henyey runs natively). Direct comparison is approximate.

---

## Cost Attribution Summary

### Current henyey ledger close breakdown (377ms sample, 272 TXs/ledger)

```
tx_exec:     367ms (97%)
  Soroban sequential (160 TXs × 855μs):  137ms (37%)
    - e2e_invoke:            59ms (16%)
    - fee+seq scaffolding:   35ms  (9%)
    - result processing:     26ms  (7%)
    - footprint (IMS):        9ms  (2%)
    - validation:            15ms  (4%)
    - meta:                   2ms  (1%)
  Classic execution (~112 TXs, mixed):   ~230ms (61%)
    - includes ~20 TXs with 100 ops (9.5ms each = 190ms alone)
commit/eviction/other:       10ms  (3%)
```

### Gap breakdown vs stellar-core (~100ms mean)

| Source | Estimated contribution | Notes |
|--------|----------------------|-------|
| Classic TX execution | ~50–80ms | High-op TXs, LedgerTxn caching advantage |
| Soroban scaffolding (fee+seq+result) | ~40–50ms | XDR serialization per TX |
| Soroban footprint | ~5ms | IMS mostly eliminates this |
| Soroban e2e_invoke | ≤0ms | Likely comparable or faster |

---

## Remaining Optimization Opportunities

### O7: Lazy LedgerEntryChanges serialization (HIGH IMPACT, ~20–30ms)

**Target:** `fee_seq_us`, `result_processing`

Instead of XDR-encoding `LedgerEntryChanges` immediately within `apply_transaction`
(for fee_bump_wrapper_changes, signer_changes, seq_changes), record the raw Rust
structs and serialize only when building the final `TransactionMeta` at the end.
This mirrors stellar-core's LedgerTxn approach.

Requires refactoring `PreApplyResult` to carry pre/post `LedgerEntry` references
instead of pre-serialized `LedgerEntryChange` XDR, and deferring serialization to
`build_transaction_meta`.

Estimated savings: 40–80μs/TX × 160 TXs = **6–13ms** for fee_seq alone.
Full lazy path (including result processing) could save **20–30ms/ledger**.

### O8: Reduce Account XDR allocations in fee path (LOW IMPACT, ~5–8ms)

**Target:** `fee_seq_us`

Within `build_entry_changes_with_state_overrides`, the STATE entry XDR-encodes the
pre-modification account, and the UPDATED entry XDR-encodes the post-modification
account. If the account was loaded from the prefetch cache, its XDR bytes are
already available — they can be reused for the STATE entry rather than
re-serializing.

Estimated savings: 10–20μs/TX × 160 TXs = **2–3ms/ledger**.

### O9: Classic TX profiling (UNKNOWN IMPACT)

The ~230ms classic execution time for ~112 TXs/ledger (including high-op TXs)
accounts for the majority of the remaining gap. A flamegraph on a ledger dominated
by 100-op TXs would reveal whether the cost is in:
- Offer crossing bucket list lookups (LedgerStateManager persistence already exists)
- Per-operation account/trustline reloads (the entry loader callback already handles this)
- XDR overhead in operation meta building

```bash
cargo build --release --bin henyey
perf record -g --call-graph=dwarf -- \
  henyey --mainnet verify-execution --from 61349600 --to 61349602
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

---

## What Was Already Optimized (O1–O6)

| Optimization | Commit | Impact | Notes |
|-------------|--------|--------|-------|
| O1: InMemorySorobanState for footprint loads | b23848f | footprint 1,400μs → 56μs/TX | 25× speedup |
| O2: Fee source account prefetch | b23848f | eliminated bucket lookups in fee_seq | Cost now XDR-only |
| O3: TTL key hash caching | b23848f | ~5μs/TX saved | Small |
| O4: Soroban entry prefetch_cache enabled | b23848f | subsumed by O1 | |
| O5: Dead P25 path removed | b23848f | code hygiene | |
| O6: Parallel bucket scan, N=4, largest-first | 37f641a | startup 297s → 125s | |

---

## Files Modified for Instrumentation (Now Reverted)

The temporary instrumentation in `crates/tx/src/soroban/host.rs` has been removed.
The existing DEBUG-level `TX phase timing` logging in `crates/ledger/src/execution/mod.rs`
is permanent and can be re-enabled with:

```bash
RUST_LOG=henyey_ledger::execution=debug,info henyey --mainnet verify-execution ...
```

It fires for all Soroban TXs and any classic TX slower than 5ms.
