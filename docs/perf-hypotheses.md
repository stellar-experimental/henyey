# Performance Hypotheses

## Round 3: Target 30,000 TPS (in progress)

Baseline: 25,764 TPS (perf) | Target: 30,000 TPS | Date: 2026-03-14

### Current Best: ~27,000 TPS (perf total: ~926ms, best run 891ms = 28,050 TPS)

| Phase | ms (baseline) | ms (current) | Savings |
|-------|---------------|-------------|---------|
| soroban_exec | 462 | 420-457 | -5 to -42ms |
| add_batch | 192 | 179 | -13ms |
| prepare | 95 | 96 | ~0 |
| fee_pre_deduct | 42 | 43 | ~0 |
| meta | 40 | 35 | -5ms |
| soroban_state | 24 | ~24 | ~0 |
| commit_setup | 5 | ~5 | ~0 |
| header_hash | 0.02 | 0.02 | ~0 |
| **total (perf)** | **~970** | **~926** | **-44ms** |

Need 833ms total perf for 30K TPS. Gap: ~93ms.

### Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 7 | Zero-alloc ValDeser charging in Soroban host | accepted | ~2% | minor (part of H8) | - |
| 8 | Reuse TTL key SHA-256 cache across TXs | accepted | ~3-5% | +3.6% | 25,723 |
| 11 | Arc-wrap TransactionEnvelope in TransactionFrame | accepted | ~2% | -14ms (prep+fee) | 25,764 |
| 16 | Incremental hash in bucket merge (avoid 2nd XDR pass) | accepted | ~3% | -13ms add_batch | ~26,500 |
| 17 | Reuse TransactionFrame in pre_apply (1 clone → 0) | accepted | ~2% | -34ms soroban_exec | ~26,500 |
| 18 | Thread Arc through execute_transaction hot path | accepted | ~2% | -10ms soroban_exec | ~27,000 |
| 12 | Cache Soroban cost params per ledger (avoid clone/TX) | pending | ~1-2% | | |
| 15 | Eliminate redundant XDR ser for size in host invocation | pending | ~3-5% | | |
| 9 | Skip per-TX footprint XDR ser via pre-computed key hash | pending | ~2-3% | | |
| 10 | Reduce prior_load overhead via Arc sharing | pending | ~1-2% | | |

### Per-TX Timing Analysis (from host instrumentation)

Soroban host execution: ~27µs per TX (very fast)
Henyey per-TX wrapper overhead: ~152µs → ~135µs per TX (after H11/H17/H18)
- validate_preconditions: ~36µs (down from ~50µs; frame no longer cloned)
- load_soroban_footprint: ~40µs (key XDR ser + SHA-256 + lookup)
- host invocation setup: ~30µs (typed entry building, budget creation)
- result building: ~20µs
- frame creation: ~1µs (was ~12µs; now Arc::clone)

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
Current best:      ~27,000 TPS (median), 28,050 TPS (best single run)
Improvement:       +138% from original

### Round 3 optimizations applied:
- H11: Arc-wrap TransactionEnvelope (-14ms prep+fee)
- H16: Incremental hash in bucket merge (-13ms add_batch)
- H17: Reuse TransactionFrame in pre_apply (-34ms soroban_exec)
- H18: Thread Arc through execute_transaction hot path (-10ms soroban_exec)
- Combined: ~970ms → ~926ms total perf (-4.5%)
