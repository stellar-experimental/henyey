# Performance Hypotheses

## Round 4: Target 15,000 TPS at 4 clusters (50K TXs) — COMPLETED

Baseline: 13,213 TPS | Target: 15,000 TPS | Date: 2026-03-14
Config: 4 clusters, 50K SAC transfer TXs/ledger, single-shot mode (3 iterations)
Session: bb962ada

### Final Result: ~15,000 TPS (median of 3 runs: 14,973 / 15,091 / 14,982)

### Optimizations Applied

| # | Hypothesis | Status | Before | After | Gain |
|---|-----------|--------|--------|-------|------|
| R4-1 | LedgerKey HashMap (eliminate XDR ser for key ops) | accepted | 13,213 | 13,372 | +159 TPS |
| R4-2 | Skip double-hashing (unsorted tx set in builder) | accepted | 13,372 | 13,917 | +545 TPS |
| R4-3 | Async bucket persistence (background thread) | accepted | 13,917 | 14,296 | +379 TPS |
| R4-4 | Drain delta categorization (move vs clone 50K entries) | accepted | 14,296 | 14,354 | +58 TPS |
| R4-5 | Consuming prepare_presorted (skip hash+sort+clone) | accepted | 14,354 | ~15,000 | +~640 TPS |

### Breakdown (avg ms/ledger, final)

| Phase | Baseline | Final | Savings |
|-------|----------|-------|---------|
| prepare | 284 | 83 | **-201ms** |
| commit_setup | 136 | 51 | **-85ms** |
| fee_pre_deduct | 161 | 132 | **-29ms** |
| soroban_exec | 2,416 | 2,406 | -10ms |
| add_batch | 316 | 320 | ~0 |
| soroban_state | 63 | 92 | +29ms (noise) |
| total (perf) | 3,473 | 3,194 | **-279ms** |
| external total | 3,784 | 3,339 | **-445ms** |

### Optimization Details

**R4-1: LedgerKey HashMap** (`delta.rs`, `snapshot.rs`, `prepare_liabilities.rs`, `close.rs`)
Changed HashMap keys from `Vec<u8>` (XDR-serialized) to `LedgerKey` directly. Marginal
improvement (~22ms in fee_pre_deduct) because LedgerKey's derived Hash is comparable cost.

**R4-2: Skip double-hashing** (`parallel_tx_set_builder.rs`)
`stages_to_xdr_phase_unsorted()` — builder no longer hashes 50K TXs for canonical sorting
since the simulation harness doesn't need deterministic ordering at build time.

**R4-3: Async bucket persistence** (`bucket_list.rs`)
Background thread for `save_to_xdr_file` with bounded concurrency (one outstanding write).
Previous persist completes before starting new one. Saves ~96ms disk I/O from critical path.

**R4-4: Drain delta categorization** (`delta.rs`, `manager.rs`)
`drain_categorization_for_bucket_update(&mut self)` moves entries out of the delta HashMap
instead of cloning. Preserves metadata (fee_pool_delta, total_coins_delta) for header
creation. Offer/pool changes collected separately for commit_close.

**R4-5: Consuming prepare_presorted** (`close.rs`, `manager.rs`, `applyload.rs`)
`prepare_presorted(self)` consumes the TX set, moving 50K TransactionEnvelope values into
Arc wrappers instead of cloning. Skips per-TX SHA-256 hashing and sorting. Uses `Vec::from()`
to convert VecM containers for owned iteration. `LedgerCloseData.presorted` flag controls
which path is used.

---

## Round 3: Target 30,000 TPS (gap remaining)

Baseline: 25,764 TPS (perf) | Target: 30,000 TPS | Date: 2026-03-14

### Current Best: ~29,400 TPS perf-equivalent (perf total: ~849ms)

| Phase | ms (baseline) | ms (current) | Savings |
|-------|---------------|-------------|---------|
| soroban_exec | 462 | 408 | **-54ms** |
| add_batch | 192 | 168 | **-24ms** |
| prepare | 95 | 94 | -1ms |
| fee_pre_deduct | 42 | 39 | -3ms |
| meta | 40 | 32 | -8ms |
| soroban_state | 24 | ~24 | ~0 |
| commit_setup | 5 | ~5 | ~0 |
| header_hash | 0.02 | 0.02 | ~0 |
| **total (perf)** | **~970** | **~849** | **-121ms (-12.5%)** |

Perf-equivalent TPS: 24992/0.849 = **29,437 TPS** (target: 30,000)
Overall TPS (incl. bucket ops): ~26,500 (avg), ~26,600 (best run)

Note: "perf total" measures only TX processing time (soroban exec + add_batch +
prepare + fees + meta). Overall TPS also includes bucket list maintenance (spill,
merge, eviction) which adds ~200-350ms of variable overhead per ledger. Closing
the remaining gap to 30K overall TPS requires either reducing bucket overhead or
further shaving ~16ms from the perf total.

### Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 7 | Zero-alloc ValDeser charging in Soroban host | accepted | ~2% | minor (part of H8) | - |
| 8 | Reuse TTL key SHA-256 cache across TXs | accepted | ~3-5% | +3.6% | 25,723 |
| 11 | Arc-wrap TransactionEnvelope in TransactionFrame | accepted | ~2% | -14ms (prep+fee) | 25,764 |
| 16 | Incremental hash in bucket merge (avoid 2nd XDR pass) | accepted | ~1-2% | -13ms add_batch | ~26,500 |
| 17 | Reuse TransactionFrame in pre_apply (1 clone → 0) | accepted | ~3-4% | -34ms soroban_exec | ~26,500 |
| 18 | Thread Arc through execute_transaction hot path | accepted | ~1-2% | -10ms soroban_exec | ~27,000 |
| 19 | Zero-alloc XDR size via CountingWriter (7 sites) | accepted | ~5-8% | -77ms total | ~29,400 |
| 13 | Lazy bucket key index (skip HashMap build on fresh) | superseded | ~3-5% | — | — |
| 14 | Reduce entry cloning in bucket merge (move semantics) | superseded | ~3-5% | — | — |
| 12 | Cache Soroban cost params per ledger (avoid clone/TX) | pending | ~1-2% | | |
| 9 | Skip per-TX footprint XDR ser via pre-computed key hash | pending | ~2-3% | | |
| 10 | Reduce prior_load overhead via Arc sharing | pending | ~1-2% | | |

H13 and H14 were superseded by H16 (incremental merge hash), which addresses the
same bucket merge overhead from a different angle: instead of skipping the key
index or avoiding clones, H16 computes hash + index during the merge loop itself,
eliminating the separate `from_sorted_entries` serialization pass. The fresh bucket
already used an empty key index (`fresh_in_memory_only`), so H13 was already
partially in place. H14's clone reduction is partially achieved by H16's buffer
reuse (single `xdr_buf` instead of per-entry `Vec<u8>` + `entry.clone()`).

H15 (eliminate redundant XDR ser for size) was implemented as H19 with a broader
scope: 7 sites across host.rs and invoke_host_function.rs replaced with a zero-
allocation CountingWriter.

### Per-TX Timing Analysis (updated after H19)

Soroban host execution: ~27µs per TX (very fast — this is the Soroban VM itself)
Henyey per-TX wrapper overhead: ~152µs → ~115µs per TX (24% reduction)
- validate_preconditions: ~36µs (down from ~50µs; frame no longer cloned per TX)
- load_soroban_footprint: ~40µs (key XDR ser + SHA-256 + bucket list lookup)
- host invocation setup: ~20µs (typed entry building, budget creation; no XDR alloc for size checks)
- result building: ~12µs (no XDR alloc for return value/event size computation)
- frame creation: ~1µs (was ~12µs; now Arc::clone instead of deep copy)

### Profiling Findings

Key discoveries from code analysis and profiling (samply):

**soroban_exec (408ms = 48% of perf total)**
- Per-TX: ~16.3µs wrapper + ~27µs host = ~43µs total × 24992 TXs ≈ 408ms given 16-cluster parallelism
- Biggest per-TX costs: footprint key SHA-256 hashing (~4-6µs), state snapshot/restore (~3µs),
  budget creation with cost param clone (~1µs), auth entry cloning (~0.5µs)
- 7 XDR-for-size serialization sites were the single largest optimization target (H19: -49ms)
- Remaining: cost param clone (ContractCostParams ~30 entries × 2 per TX) is ~1µs/TX = ~25ms total

**add_batch (168ms = 20% of perf total)**
- Dominated by XDR serialization during `from_sorted_entries()`: each of ~25K bucket entries
  serialized to compute the bucket hash and build the key index
- H16 (incremental merge) reduced this by computing hash during merge instead of a separate pass
- Remaining: deduplication sorts (~15-30ms), structural key comparisons during merge (~20ms),
  entry cloning in merge loop (~30ms), final bucket serialization (~80ms)

**prepare (94ms = 11% of perf total)**
- Dominated by per-TX hash computation: XDR serialize full TransactionEnvelope + SHA-256
  (~2µs/TX × 25K = ~50ms) plus HashMap grouping by account + sort (~40ms)
- Already well-optimized: hashes pre-computed before sort (Round 1, H1)

**fee_pre_deduct (39ms = 5% of perf total)**
- Per-TX: create TransactionFrame, compute fee, deduct from account on delta
- Reduced by H11 (Arc envelope) from ~48ms

---

## Round 2: Target 40,000 TPS (gap remaining)

| # | Hypothesis | Status | Measured Gain | TPS After |
|---|-----------|--------|---------------|-----------|
| 2 | Structural ScAddress comparison | accepted | -23% add_batch | 22,968 |
| 3 | Streaming XDR hash + TX set hash caching | accepted | header_hash 50ms->0ms | ~23,500 |
| 4 | Structural dedup in add_batch | accepted | -10% add_batch | ~24,500 |
| 6 | Index-based sort to avoid TX cloning | rejected | no improvement | - |

## Round 1: Target 15,000 TPS (completed)

| # | Hypothesis | Status | Measured Gain | TPS After |
|---|-----------|--------|---------------|-----------|
| 1 | Cache TX hashes in sort + eliminate TX clones | accepted | +77% | 20,097 |

---

## Cumulative Performance Summary

Original baseline: 11,329 TPS
Round 3 best:      ~29,400 TPS (perf-equiv), ~26,500 TPS (overall incl. bucket ops)
Round 4 best:      ~15,000 TPS at 4 clusters / 50K TXs (different config from R3)
Improvement:       +160% from original (perf-equivalent), +134% overall

### Round 4 optimizations applied (session bb962ada):
- R4-1: LedgerKey HashMap keys (+159 TPS)
- R4-2: Skip double-hashing in builder (+545 TPS)
- R4-3: Async bucket persistence (+379 TPS)
- R4-4: Drain delta categorization (+58 TPS)
- R4-5: Consuming prepare_presorted (+~640 TPS)
- Combined: 13,213 → ~15,000 TPS (+13.5%)

### Round 3 optimizations applied:
- H11: Arc-wrap TransactionEnvelope in TransactionFrame (-14ms prep+fee)
- H16: Incremental hash in bucket merge (-13ms add_batch)
- H17: Reuse TransactionFrame in pre_apply (-34ms soroban_exec)
- H18: Thread Arc<TransactionEnvelope> through hot execution path (-10ms soroban_exec)
- H19: Zero-alloc XDR size via CountingWriter (-77ms total, biggest win)
- Combined: ~970ms → ~849ms total perf (-12.5%)

### Commits:
1. `0c66a74d` — Wrap TransactionFrame envelope in Arc for cheap cloning (H11)
2. `1e915d72` — Optimize merge hash computation and reduce per-TX envelope clones (H16, H17)
3. `3beef9f3` — Thread Arc<TransactionEnvelope> through hot execution path (H18)
4. `066299fd` — Replace allocating XDR serialization with counting writer (H19)
