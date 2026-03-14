# Performance Hypotheses

## Round 2: Target 40,000 TPS

Baseline: 21,824 TPS | Target: 40,000 TPS | Date: 2026-03-14

### Perf Breakdown (avg ms/ledger, 24,992 TXs)

| Phase | ms | % |
|-------|-----|---|
| soroban_exec | 409 | 40% |
| add_batch | 256 | 25% |
| prepare | 125 | 12% |
| header_hash | 50 | 5% |
| fee_pre_deduct | 47 | 5% |
| meta | 31 | 3% |
| soroban_state | 26 | 3% |
| commit_setup | 6 | 1% |
| **total (perf)** | **1032** | |

Need: ~625ms/ledger for 40K TPS → cut ~400ms

### Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 2 | Eliminate XDR serialization in bucket entry comparison (compare_sc_address/compare_sc_val) | pending | ~15-20% (save 50-100ms in add_batch sort) | | |
| 3 | Cache key bytes in add_batch deduplication to avoid 150K+ redundant XDR serializations | pending | ~10-15% | | |
| 4 | Optimize prepare phase: cache TX hashes, eliminate redundant sorting | pending | ~5-8% | | |
| 5 | Reduce fee_pre_deduct: reuse TransactionFrame from prepare | pending | ~3-5% | | |
| 6 | Optimize soroban_exec per-TX overhead (footprint prefetch, snapshot_delta) | pending | ~5-10% | | |

---

## Round 1: Target 15,000 TPS (completed)

Baseline: 11,329 TPS | Target: 15,000 TPS

| # | Hypothesis | Status | Measured Gain | TPS After |
|---|-----------|--------|---------------|-----------|
| 1 | Cache TX hashes in sort + eliminate TX clones in txset build | accepted | +77% | 20,097 |
