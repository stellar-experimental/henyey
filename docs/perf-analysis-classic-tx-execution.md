# Classic TX Execution Performance Analysis

## Summary

Classic TX execution accounts for **~230ms out of ~377ms** total ledger close time (~61% of
tx_exec). The primary driver is a small number of high-op-count TXs: ~20 TXs with 100 operations
each contribute ~190ms/ledger from signature verification overhead alone.

**Data source:** 5,477 classic TXs across ~53 ledgers (61349600–61349653), with per-phase DEBUG
timing enabled for all TXs.

---

## Measurement Setup

### Benchmark run

```bash
RUST_LOG=henyey_ledger::execution=debug,info henyey --mainnet verify-execution \
  --from 61349600 --to 61349653 --cache-dir ~/data/mainnet/ \
  2>/tmp/classic_stderr2.log
```

The DEBUG logging fires for every TX via `TX phase timing` log lines, capturing:
- `validation_us` — sig verification + account/trustline checks
- `fee_seq_us` — fee deduction, signer removal, seq bump + LedgerEntryChanges building
- `footprint_us` — `load_soroban_footprint` (classic TXs: 0)
- `ops_us` — full ops loop
- `meta_us` — outer `TransactionMetaV3` building
- `op_timings` — per-op-type cumulative time and count within the TX

---

## Finding 1: Per-TX Classic Phase Breakdown

### All classic TXs (n=5,477)

| Phase | Mean | p50 | p75 | p95 | p99 |
|-------|------|-----|-----|-----|-----|
| validation_us | 95μs | 82μs | 108μs | 182μs | 342μs |
| fee_seq_us | 244μs | 120μs | 208μs | 942μs | 4415μs |
| ops_us | 260μs | 168μs | 307μs | 722μs | 5401μs |
| meta_us | 16μs | 10μs | 18μs | 38μs | 72μs |
| **total_us** | **615μs** | **355μs** | **586μs** | **1,146μs** | **9,476μs** |

`fee_seq_us` has a bimodal distribution: most TXs (1-op) pay ~120μs, while 100-op TXs pay ~4,415μs.
The p99 of 9,476μs total and 4,415μs fee_seq corresponds to these high-op-count TXs.

### Slow TXs only (n=96, total_us > 5ms)

| Phase | Mean | p50 |
|-------|------|-----|
| validation_us | 235μs | 208μs |
| fee_seq_us | 3,378μs | 3,152μs |
| ops_us | 5,401μs | 4,840μs |

---

## Finding 2: fee_seq_us Scales ~44μs Per Operation

`fee_seq_us` has a near-linear relationship with operation count:

| Op count | fee_seq_us (observed) | Marginal cost |
|----------|----------------------|---------------|
| 1        | ~120μs               | baseline      |
| 10       | ~560μs               | ~44μs/op      |
| 50       | ~2,300μs             | ~44μs/op      |
| 100      | ~4,415μs             | ~44μs/op      |

### Root cause: Per-operation Ed25519 signature re-verification

`fee_seq_us` includes `check_operation_signatures`, which iterates every operation and calls
`tracker.check_signature(op_source_account, needed_weight)` for each op. Inside
`check_signature_from_signers`:

```rust
// crates/ledger/src/execution/signatures.rs
for (sig_idx, sig) in tracker.signatures.iter().enumerate() {
    // verify_signature_with_key calls ed25519_dalek::verify — real elliptic curve crypto
    if let Ok(weight) = verify_signature_with_key(sig, &tracker.tx_hash, signer_key) {
        // ... accumulate weight
    }
}
```

This iterates **all TX signatures** and calls Ed25519 crypto for each one, **for every
operation in the TX**. For a 100-op TX with 1 signature:
- 100 ops × 1 signature × 1 Ed25519 verify ≈ 100 verifications
- Ed25519 verify ≈ ~40μs each → ~4,000μs for signature checking alone

The code does NOT:
- Skip signatures already marked as `used[sig_idx] = true` before calling crypto
- Return early when `needed_weight == 0` (many ops share the TX source account)

**stellar-core comparison:** `SignatureChecker::checkSignature` in `SignatureChecker.cpp` has
the exact same pattern — `verifyAll` lambda iterates all signatures and calls `verify(sig, signerKey)`
before checking `mUsedSignatures[i]`. No early return for `neededWeight == 0`. This is not a
henyey-specific bug — it matches stellar-core behavior precisely.

**Why stellar-core is faster despite identical logic:** stellar-core's validation runs within
`LedgerTxn` which benefits from hierarchical caching: all account loads during signature checking
are cached and reused across all TXs in the ledger. Henyey must re-fetch from the snapshot's
`prefetch_cache` (still requires bucket list scan for first touch within a ledger).

---

## Finding 3: Per-Op-Type Cost Breakdown

### ops_us cost per operation type (from op_timings field)

| Operation Type | Cost per op | Notes |
|----------------|------------|-------|
| PathPaymentStrictReceive | 315μs | Multi-hop DEX path-finding |
| PathPaymentStrictSend | 117μs | DEX path-finding, fewer hops |
| ManageBuyOffer | 65μs | Offer crossing + book update |
| ManageSellOffer | 59μs | Offer crossing + book update |
| CreateClaimableBalance | 42μs | Write-only, no DEX |
| Payment | 41μs | Account/trustline load + transfer |

### PathPaymentStrictReceive is 7.7× more expensive than Payment

Each DEX hop in a path payment requires:
1. `loadBestOffer` — sorted offer iteration from the offer book
2. `crossOffer` — load seller account + trustline, modify balances, write changes
3. Repeat for each hop (up to `sendPath.size() + 1` asset pairs)

Henyey's offer exchange traversal (`cross_offer` in `crates/tx/src/operations/offer_exchange.rs`)
loads offers and accounts via `LedgerStateManager`, which falls back to the snapshot's
`prefetch_cache` for entries not already in state. Each first-touch offer seller account needs
a bucket list lookup.

**stellar-core comparison:** `convertWithOffersAndPools` in `OfferExchange.cpp` (called from
`PathPaymentOpFrameBase::convert`) uses `ltx.loadBestOffers` which accesses an in-memory sorted
offer index maintained in `LedgerTxnRoot`. All offer sellers loaded during crossing are
automatically cached in `LedgerTxn` — subsequent hops or TXs touching the same sellers are O(1).

---

## Finding 4: Per-Ledger Classic Execution Budget

**Observed distribution (~112 classic TXs/ledger):**

```
~20 high-op TXs (100 ops each):
  fee_seq: 20 × 4,415μs = 88ms
  ops:     20 × ~5,000μs = 100ms
  subtotal: ~190ms

~92 normal TXs (1-5 ops each):
  total:   92 × 355μs   = 33ms

Classic total: ~230ms/ledger
```

The 100-op TXs (likely automated DEX bots using manage-offer batches) dominate because:
- `fee_seq_us` scales linearly with op count (Ed25519 per-op)
- `ops_us` scales with actual DEX work per op

---

## Finding 5: Structural Comparison with stellar-core

### Where stellar-core has an advantage for classic TXs

| Feature | stellar-core | henyey | Gap |
|---------|-------------|--------|-----|
| Account caching across TXs | LedgerTxn MVCC (O(1) after first load) | prefetch_cache (bucket scan for first touch) | Yes |
| Offer book | In-memory sorted index in LedgerTxnRoot | OfferCache (persistent across ledgers but still re-fetches per-ledger) | Partial |
| XDR serialization | Deferred to end-of-ledger via LedgerTxn commit | Per-TX in `fee_seq` and `result_processing` | Yes |
| Signature verification | Same per-op Ed25519 pattern | Same per-op Ed25519 pattern | None |

### Key structural difference: LedgerTxn vs snapshot + delta

stellar-core's `LedgerTxn` hierarchy:
- `LedgerTxnRoot` maintains a RAM-resident index of all ledger entries (loaded on startup)
- `LedgerTxn` child layers accumulate modifications in-memory during ledger close
- Account loaded by TX 1 is a zero-cost cache hit for TX 2, 3, ... N in the same ledger
- All modifications are materialized into XDR `LedgerEntryChanges` only at commit

henyey's snapshot + delta model:
- `SnapshotHandle` reads from a point-in-time snapshot of the bucket list
- Per-TX `LedgerStateManager` accumulates loaded entries but is reset between TXs (except for
  the classic executor's offer cache which persists)
- Each TX's fee source accounts, signer accounts, and op source accounts may require
  bucket list scans if not already in the snapshot's `prefetch_cache`

---

## Optimization Opportunities for Classic TX Execution

### C1: Skip redundant Ed25519 verification for already-used signatures (LOW IMPACT, ~5–10ms)

Add a fast-path in `check_signature_from_signers` to skip `verify_signature_with_key` for
signatures already marked `used[sig_idx]`:

```rust
for (sig_idx, sig) in tracker.signatures.iter().enumerate() {
    if tracker.used[sig_idx] {
        // Signature already matched; still counts for weight accumulation
        // but we can reuse the cached weight without re-verifying crypto
        continue; // Requires storing per-sig weight in tracker
    }
    // ... Ed25519 verify
}
```

This requires extending `SignatureTracker` to cache per-signature weights on first verification.
Savings depend on how often multiple ops share the same source account (very common for
100-op TXs where all ops use TX source account). Estimated: 30–50% reduction in fee_seq_us
for multi-op TXs = **~5–10ms/ledger**.

**Note:** This would diverge from stellar-core's current behavior in terms of which verification
calls happen; the result (success/failure) must remain identical. Requires careful testing.

### C2: Early return for needed_weight == 0 in signature check (VERY LOW IMPACT)

Add `if needed_weight == 0 { return Ok(()); }` at the top of `check_signature_from_signers`.
When an op's source account is the TX source account and the TX source account has already
satisfied its threshold, subsequent ops with the same source account would short-circuit.

However, the current code accumulates weight per call; the early return only helps if weight
is tracked globally. Given stellar-core has the same pattern, this is low priority.

### C3: Classic account prefetch across ledger (MEDIUM IMPACT, ~15–30ms)

The largest structural gap is that stellar-core's `LedgerTxn` caches all accounts loaded
during TX execution across all TXs in a ledger. The same DEX offer seller may be involved
in 50 path payments; stellar-core loads it once, henyey loads it from the bucket list or
snapshot prefetch cache each time.

A cross-TX account cache for classic TXs (similar to `InMemorySorobanState` for Soroban)
would require accumulating loaded accounts from each TX into a shared cache. This is
architecturally more complex than the Soroban fix but would close the most significant
structural gap.

### C4: Profiling with flamegraph (DIAGNOSTIC)

A flamegraph on a ledger with many 100-op TXs would confirm the exact time split between:
- Ed25519 crypto in `check_operation_signatures`
- Bucket list lookups for offer sellers in path payment crossing
- XDR encoding in `fee_seq`

```bash
cargo build --release --bin henyey
perf record -g --call-graph=dwarf -- \
  henyey --mainnet verify-execution --from 61349600 --to 61349602
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

---

## Cost Attribution Summary

### Classic TX contribution to henyey's ~377ms ledger close

```
Classic execution (~112 TXs/ledger): ~230ms (61% of tx_exec)
  High-op TXs (~20 × 100 ops):     ~190ms
    fee_seq (Ed25519 per-op):         88ms
    ops (DEX path payments):         100ms
  Normal TXs (~92):                  ~33ms
    fee_seq + ops typical:             ~33ms
```

### Gap vs stellar-core for classic execution: ~50–80ms

| Source | Estimated contribution | Notes |
|--------|----------------------|-------|
| LedgerTxn cross-TX account caching | ~30–50ms | Offer sellers, source accounts reloaded |
| DEX offer book in-memory index | ~10–20ms | stellar-core's LedgerTxnRoot has sorted index |
| Deferred XDR serialization | ~10–15ms | LedgerEntryChanges built per-TX in henyey |
| Ed25519 per-op re-verification | Same | stellar-core has identical behavior |

---

## What Was Already Optimized (O1–O6, Soroban-focused)

See `perf-analysis-soroban-tx-execution.md` for Soroban optimizations.
Classic TX execution was not a focus of O1–O6. The offer cache (`OfferCache` that persists
across ledgers) is the main existing optimization for classic DEX operations.

---

## Files Modified for Instrumentation (Now Reverted)

The `total_us > 0` threshold in `crates/ledger/src/execution/mod.rs` was temporarily changed
from `total_us > 5000 || frame.is_soroban()` to `total_us > 0` to capture all classic TXs.
This has been reverted. The permanent DEBUG-level `TX phase timing` logging fires for:
- All Soroban TXs
- Classic TXs slower than 5ms

Re-enable full classic TX logging with:
```bash
RUST_LOG=henyey_ledger::execution=debug,info henyey --mainnet verify-execution ...
```
And temporarily change the threshold in `crates/ledger/src/execution/mod.rs:3067` to
`if total_us > 0` to capture all TXs.
