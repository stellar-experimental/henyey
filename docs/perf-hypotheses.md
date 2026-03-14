# Performance Hypotheses

## Round 2: Target 40,000 TPS

Baseline: 21,824 TPS | Target: 40,000 TPS | Date: 2026-03-14

### Current Best: ~24,500 TPS (perf total: ~930ms)

| Phase | ms | % |
|-------|-----|---|
| soroban_exec | 445 | 48% |
| add_batch | 176 | 19% |
| prepare | 110 | 12% |
| fee_pre_deduct | 51 | 5% |
| meta | 32 | 3% |
| soroban_state | 24 | 3% |
| commit_setup | 5 | 1% |
| header_hash | 0.02 | 0% |
| **total (perf)** | **~930** | |

### Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 2 | Eliminate XDR serialization in bucket entry comparison | accepted | ~15-20% | -23% add_batch | 22,968 |
| 3 | Streaming XDR hash + TX set hash caching | accepted | ~5% | -50ms header_hash | ~23,500 |
| 4 | Structural dedup in add_batch (no XDR serialization) | accepted | ~10% | -10% add_batch | ~24,500 |
| 5 | Derived Ord for LedgerKey comparison (simplified) | accepted | ~5% | merged with H2 | - |
| 6 | Index-based sort to avoid TX cloning in prepare | rejected | ~5% | no improvement | - |
| 7 | Optimize soroban_exec per-TX overhead | not attempted | ~5-10% | - | - |

### Remaining Gap Analysis

To reach 40K TPS (625ms/ledger), we'd need to cut ~305ms from the current ~930ms.

**soroban_exec (445ms, 48%)** dominates. This is actual Soroban host execution of
SAC payment contracts across 16 parallel clusters. Optimizing this requires either:
- Faster Soroban host (external dependency)
- Better parallelism (32 clusters was slower due to overhead)
- Reducing per-TX snapshot/rollback overhead (~50-100ms possible)

**add_batch (176ms, 19%)** is mostly the merge hash computation: every output entry
must be XDR-serialized and SHA-256 hashed. This is a protocol requirement for
bucket hash verification.

**prepare (110ms, 12%)** is per-TX hash computation (25K × XDR+SHA256) + sorting.
This is required for canonical transaction ordering.

---

## Round 1: Target 15,000 TPS (completed)

Baseline: 11,329 TPS | Target: 15,000 TPS

| # | Hypothesis | Status | Measured Gain | TPS After |
|---|-----------|--------|---------------|-----------|
| 1 | Cache TX hashes in sort + eliminate TX clones in txset build | accepted | +77% | 20,097 |

---

## Performance Optimization Summary

Baseline:    11,329 TPS (original)
Round 1:     21,824 TPS (after H1)
Final:       ~24,500 TPS (after H2-H5)
Target:      40,000 TPS
Improvement: +116% from original baseline
Status:      gap remaining (soroban_exec dominates at 48% of total)

### Accepted optimizations (cumulative):
- H1: sort_by_cached_key in stages_to_xdr_phase + eliminate TX clones: +77%
- H2: Structural ScAddress comparison (no XDR serialization): +5%
- H3: Streaming XDR hash + OnceCell TX set hash caching: header_hash 50ms -> 0ms
- H4: Sort-based dedup instead of XDR-serialize HashMap dedup: -10% add_batch
- H5: Derived Ord for LedgerKey (simplified compare_keys): included in H2

### Rejected:
- H6: Index-based sort to avoid TX cloning: no improvement (indirection overhead)
