# Validation + Fee/Seq Sub-Component Analysis

**Date**: 2026-03-12
**Benchmark**: 1002 mainnet ledgers (61606000–61607000), release build
**Result**: 0 mismatches, 164.2ms/ledger avg, slowest 337.4ms

## Overview

Instrumented `validate_preconditions()` and the fee/seq block of `pre_apply()` in
`crates/ledger/src/execution/mod.rs` with sub-component timers to identify optimization
targets within the 30ms/ledger validation+fee/seq overhead.

## Sub-Component Breakdown (per-ledger averages, ~309 TXs/ledger)

### Validation (`validate_preconditions`)

| Timer | What it measures | us/ledger | % of total |
|-------|-----------------|-----------|------------|
| `val_ed25519` | `has_sufficient_signer_weight()` — ed25519 verify + weight check | 13,484 | 44.1% |
| `val_other` | Structure check, fee check, precondition/seq validation | 1,640 | 5.4% |
| `val_tx_hash` | `frame.hash()` — XDR serialize + SHA256 | 656 | 2.1% |
| `val_account_load` | `load_account()` for fee source + inner source | 356 | 1.2% |
| **Total validation** | | **18,208** | **59.5%** |

### Fee/Seq (`pre_apply` post-validation)

| Timer | What it measures | us/ledger | % of total |
|-------|-----------------|-----------|------------|
| `op_sig_check` | `check_operation_signatures()` + per-op source loading | 9,947 | 32.5% |
| `seq_bump` | Sequence bump + flush + metadata + commit | 855 | 2.8% |
| `signer_removal` | One-time signer removal + flush + metadata | 594 | 1.9% |
| `fee_deduct` | Delta snapshot + fee deduction + flush + entry changes | 302 | 1.0% |
| **Total fee/seq** | | **12,385** | **40.5%** |

### Combined

| | us/ledger | % of tx_exec |
|--|-----------|-------------|
| **Validation + Fee/Seq** | **30,593** | **20.0%** |
| tx_exec total | 153,210 | 100% |

## Key Finding

**Ed25519 signature verification dominates**: `val_ed25519` (13.5ms) + `op_sig_check` (9.9ms) =
**23.4ms/ledger** — **76.5%** of validation+fee/seq time and **15.3%** of total tx_exec.

Everything else is negligible:
- Account loading: 0.4ms
- TX hashing: 0.7ms
- Fee deduction: 0.3ms
- Signer removal: 0.6ms
- Sequence bump: 0.9ms

## Optimization Targets

1. **Signature verification caching/batching** (23.4ms potential): The same TX hash + signatures
   are verified twice — once in `validate_preconditions` (`has_sufficient_signer_weight`) and
   again in `pre_apply` (`check_operation_signatures`). Caching verified signatures or batch
   verification could eliminate redundant ed25519 work.

2. **TX hash caching** (0.7ms potential): `frame.hash()` serializes the full TX envelope to XDR
   then SHA256s it. If the hash is computed once and reused, this is a minor win.

## Instrumentation Details

### Per-TX debug log
```
TX phase timing ledger_seq=X total_us=Y validation_us=... val_account_load_us=...
    val_tx_hash_us=... val_ed25519_us=... val_other_us=... fee_seq_us=...
    fee_deduct_us=... op_sig_check_us=... signer_removal_us=... seq_bump_us=...
```

### Per-ledger aggregate (PROFILE apply_txs)
Adds `agg_val_account_load_us`, `agg_val_tx_hash_us`, `agg_val_ed25519_us`, `agg_val_other_us`,
`agg_fee_deduct_us`, `agg_op_sig_check_us`, `agg_signer_removal_us`, `agg_seq_bump_us`.

## Files Changed

- `crates/ledger/src/execution/mod.rs` — sub-component timers in `validate_preconditions()` and `pre_apply()`
- `crates/ledger/src/manager.rs` — aggregate sub-component timings in `PROFILE apply_txs`
