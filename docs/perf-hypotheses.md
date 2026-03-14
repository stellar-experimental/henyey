# Performance Hypotheses

## Round 3: Target 30,000 TPS (in progress)

Baseline: 24,826 TPS | Target: 30,000 TPS | Date: 2026-03-14

### Current Best: ~25,700 TPS (perf total: ~883ms)

| Phase | ms | % |
|-------|-----|---|
| soroban_exec | 420 | 48% |
| add_batch | 173 | 20% |
| prepare | 103 | 12% |
| fee_pre_deduct | 48 | 5% |
| meta | 32 | 4% |
| soroban_state | 23 | 3% |
| commit_setup | 5 | 1% |
| header_hash | 0.02 | 0% |
| **total (perf)** | **~883** | |

### Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 7 | Zero-alloc ValDeser charging in Soroban host | accepted | ~2% | minor (part of H8) | - |
| 8 | Reuse TTL key SHA-256 cache across TXs | accepted | ~3-5% | +3.6% | 25,723 |
| 9 | Skip per-TX footprint XDR ser via pre-computed key hash | pending | ~3% | | |
| 10 | Reduce prior_load overhead via Arc sharing | pending | ~2-3% | | |
| 11 | Eliminate redundant TX envelope clones in fee_pre_deduct | pending | ~2% | | |

### Per-TX Timing Analysis (from host instrumentation)

Soroban host execution: ~27µs per TX (very fast)
Henyey per-TX wrapper overhead: ~152µs per TX
- validate_preconditions: ~50µs (TX frame clone, account load, sig check)
- load_soroban_footprint: ~40µs (key XDR ser + SHA-256 + lookup)
- host invocation setup: ~30µs (typed entry building, budget creation)
- result building: ~20µs
- frame creation: ~12µs

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
Current best:      25,723 TPS
Improvement:       +127% from original
