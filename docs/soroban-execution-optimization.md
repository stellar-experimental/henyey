# Soroban Execution Optimization Plan

## Problem Statement

Henyey's ledger close time is 1.88x slower than stellar-core v25.2.0 on the same
hardware, same ledger range. The gap is **149ms** per ledger (317.5ms vs 168.5ms).

## Root Cause

Linear regression on 136 mainnet ledgers (61349505–61349640) decomposes the gap:

| Component | stellar-core | henyey | Gap at mean workload |
|-----------|-------------|--------|---------------------|
| Per classic-op | 0.023ms | 0.054ms | +16ms (2.4x) |
| Per soroban-op | 0.241ms | 0.830ms | +152ms (3.5x) |
| Fixed overhead | 95.4ms | 77.0ms | −18ms |
| **Total** | **168.5ms** | **317.5ms** | **+149ms** |

**Soroban per-op cost is 3.5x slower, accounting for 152ms of the 149ms gap.**

Mean workload per ledger: ~500 classic ops, ~257 soroban ops (~130 soroban TXs).

## Profiling Breakdown

Granular profiling (Step 1 commit `a1db12f`, `RUST_LOG=debug` on release build)
reveals where per-Soroban-TX time goes:

### Per Soroban TX (~1100μs total, ~130 TXs/ledger = ~143ms)

| Component | Avg (μs) | % | Per ledger | Optimizable? |
|-----------|---------|---|------------|--------------|
| e2e_invoke (WASM host) | 360 | 33% | 47ms | No (upstream soroban-env-host) |
| Per-op bookkeeping | 480 | 44% | 62ms | **Yes** — delta snapshots, change tracking, savepoints |
| fee_seq (fees, seq bump, signers) | 112 | 10% | 15ms | Partially |
| apply_storage_changes | 69 | 6% | 9ms | Partially |
| validation | 71 | 6% | 9ms | Partially |
| footprint loading | 45 | 4% | 6ms | Minor |
| XDR encode + extract | 45 | 4% | 6ms | Not worth it |

**Per-op bookkeeping** is the per-operation overhead in `execute_single_transaction`:
2× `delta_snapshot()`, `delta_changes_between()`, `flush_modified_entries()`,
`begin_op_snapshot()` / `end_op_snapshot()`, savepoint management, entry change
building with state overrides. For Soroban TXs with exactly 1 operation, much of
this bookkeeping is redundant — the "operation changes" ARE the "transaction changes".

### Per-ledger setup/teardown (~36ms)

| Component | Time | Optimizable? |
|-----------|------|-------------|
| executor_setup (HashMap retain for offers) | 10.5ms | **Yes** — offer/non-offer map split |
| post_exec (fee event generation) | 7.9ms | **Yes** — reuse parsed TX data |
| tx_parse (XDR deserialization) | 7.4ms | **Yes** — unified TX set parsing |
| fee_deduct + preload | 5.0ms | Minor |
| phase_parse (soroban phase structure) | 4.4ms | **Yes** — unified TX set parsing |

### What's NOT the bottleneck

- **XDR entry serialization** (encode + extract): Only 45μs/TX total (6ms/ledger).
  For P25, `disk_read_bytes_exceeded` skips Soroban entries entirely — no duplicate
  serialization. The original plan's Steps 2–3 targeted this, but they would yield <6ms.
- **e2e_invoke host execution**: 360μs/TX (47ms/ledger). This is the upstream
  soroban-env-host crate — same Rust code as stellar-core. Cannot be optimized here.

---

## Benchmark Protocol

All measurements use:
- **Binary**: release build (`cargo build --release --bin henyey -p henyey`)
- **Command**: `verify-execution --from 61349540 --to 61349640` (101 closes, protocol 25)
- **Cache**: `--cache-dir ~/data/<session>/cache` (pre-warmed from prior run)
- **Logging**: `RUST_LOG=info` for timing, `RUST_LOG=debug` for phase breakdown
- **Machine**: same host for all runs (no cross-machine comparisons)
- **Repetitions**: 3 runs, report median of means

### Baseline

| Metric | Value |
|--------|-------|
| Mean | 317.5ms |
| p50 | 385ms |
| p95 | 507ms |
| stellar-core reference | 168.5ms mean |

### Acceptance Criteria

The optimization is considered successful when:

1. **Performance**: Mean ledger close ≤ 220ms on the benchmark range (1.3x stellar-core)
2. **Correctness**: Hash parity on ≥1000 consecutive mainnet ledgers with `verify-execution`
3. **No RSS regression**: Peak RSS increase ≤ 200MB over baseline
4. **All tests pass**: `cargo test --all` + `cargo clippy --all` clean

Stretch goal: ≤ 190ms mean (1.13x stellar-core).

---

## Optimization Steps

### Step 1: TTL Key Hash Caching ✅ DONE

**Commit**: `a1db12f` | **Result**: −10.4ms (307.1ms)

Built `TtlKeyCache` (`HashMap<LedgerKey, Hash>`) during `load_soroban_footprint`,
threaded it through all Soroban validation/execution functions. Eliminates ~15K
redundant `key.to_xdr() + SHA256` computations per ledger.

Original estimate was −60 to −80ms. Actual: −10.4ms. SHA-256 of small keys
(~100-200 bytes) takes <1μs each — the hash computation was never the real
bottleneck. The profiling done after this step revealed the true cost structure
(see Profiling Breakdown above).

---

### Step 2: Fast-Path Single-Op Soroban TXs (Expected: −30 to −50ms)

**Problem**: Every Soroban TX executes exactly 1 operation (InvokeHostFunction),
but the per-operation bookkeeping loop treats it identically to multi-op classic
TXs. This adds ~480μs of overhead per TX:

1. `delta_snapshot()` before op — clones delta state (~50-100μs)
2. `begin_op_snapshot()` — sets up operation-level state tracking
3. `load_operation_accounts()` — loads per-op source accounts from snapshot
4. `create_savepoint()` — creates rollback savepoint
5. (actual operation execution — already measured separately)
6. `flush_modified_entries()` — synchronizes state
7. `delta_snapshot()` after op — clones delta state again
8. `delta_changes_between()` — diffs the two snapshots
9. `end_op_snapshot()` — extracts per-op state snapshots
10. Entry change building with state overrides

For a single-op TX, the "operation-level changes" are identical to the
"transaction-level changes". The before/after delta snapshots, the change diffing,
and the savepoint (which is never rolled back for a 1-op TX) are all redundant.

**How stellar-core handles it**: In `doApply()`, single-op TXs take essentially
the same path, but `LedgerTxn` is a lightweight stack-of-overlays — pushing/popping
an overlay is O(1). The cost difference is that our delta snapshot cloning is O(n)
where n = number of modified entries.

**Solution**: Detect single-op Soroban TXs early and bypass the per-operation
bookkeeping loop. Instead:
- Skip `delta_snapshot()` before/after — derive operation changes directly from
  the transaction-level delta
- Skip `create_savepoint()` — single-op success/failure maps to TX success/failure
- Skip `begin_op_snapshot()` / `end_op_snapshot()` — use TX-level state directly
- Skip `delta_changes_between()` — the TX delta IS the op delta

This is a code-path optimization, not a behavioral change. The resulting metadata
must be identical.

**Files to modify**:
- `crates/ledger/src/execution/mod.rs` — add fast-path in `execute_single_transaction`
  for `frame.operations().len() == 1 && op_type.is_soroban()`

**Constraints**:
- Output metadata (tx_changes_before, operation changes, events) must be byte-identical
- Rollback behavior for failed operations must be preserved
- The fee_seq changes (fee deduction, seq bump, signer removal) happen BEFORE the
  op loop and are unaffected

**Benchmark gate**: Run benchmark protocol. Expected: mean ≤ 267ms (−30 to −50ms
from Step 1). If improvement < 15ms, investigate.

---

### Step 3: Executor Setup — Offer/Non-Offer Map Split (Expected: −8 to −10ms)

**Problem**: `clear_cached_entries_preserving_offers()` calls `.retain()` on three
maps (`entry_sponsorships`, `entry_sponsorship_ext`, `entry_last_modified`), iterating
all entries to keep only Offer keys. These maps accumulate entries of all types
(accounts, trustlines, contracts, etc.) during a ledger. The `.retain()` cost is
O(total entries) regardless of how many are offers. Measured at 10.5ms per ledger.

**How stellar-core solves it**: No equivalent cost. State is ephemeral per-ledger
via scope-based `LedgerTxn`.

**Solution**: Split each map into offer-specific and non-offer maps:
- On insert, route based on `LedgerKey::Offer(_)` match
- On lookup, check both maps
- On `clear_cached_entries_preserving_offers()`, call `.clear()` on non-offer maps
  (O(1) amortized) and leave offer maps untouched

**Files to modify**:
- `crates/tx/src/state/mod.rs` — split the three maps, update insert/get/remove

**Benchmark gate**: Expected: mean ≤ 259ms. If improvement < 4ms, investigate.

---

### Step 4: Unified TX Set Parsing (Expected: −10 to −15ms)

**Problem**: Three separate passes parse the `GeneralizedTransactionSet`:
1. `transactions_with_base_fee()` (7.4ms) — parses all TXs, computes base fees
2. `soroban_phase_structure()` (4.4ms) — re-parses for Soroban phase/stage/cluster
3. Post-execution fee event generation (part of 7.9ms post_exec) — calls
   `transactions_with_base_fee()` again, constructs new `TransactionFrame` per TX

**How stellar-core solves it**: `prepareForApply()` parses once into cached
`TransactionFrame` objects organized by phase/stage/cluster. Fee events are emitted
during TX execution using the already-parsed frames.

**Solution**: Parse TX set once into a `PreparedTxSet` struct:
- Pre-sorted classic TXs with base fees
- Pre-parsed Soroban phase structure (stages, clusters)
- Pre-extracted fee source `AccountId` per TX
- All consumers read from the cached data

**Files to modify**:
- `crates/ledger/src/execution/tx_set.rs` — add `PreparedTxSet`
- `crates/ledger/src/manager.rs` — fee event generation uses cached data

**Benchmark gate**: Expected: mean ≤ 247ms. If improvement < 5ms, investigate.

---

### Step 5: Streamline fee_seq Processing (Expected: −5 to −10ms)

**Problem**: The pre-apply phase (`fee_seq_us`) takes 112μs per Soroban TX (15ms/ledger).
This includes fee deduction, sequence bump, one-time signer removal, and 3 rounds of
`delta_snapshot()` + `delta_changes_between()` + `build_entry_changes_with_state_overrides()`
to track metadata changes for each sub-phase (fee, signers, seq).

**How stellar-core handles it**: `LedgerTxn` sub-transactions are O(1) push/pop. Change
tracking is implicit in the overlay stack. No explicit delta cloning.

**Solution**: For Soroban TXs (which have no per-op source accounts and typically no
PreAuthTx signers):
- Combine the three delta-tracking phases into a single phase where possible
- Skip signer removal iteration when the TX has no PreAuthTx signatures
- Use direct state mutation tracking instead of before/after snapshot diffing

**Files to modify**:
- `crates/ledger/src/execution/mod.rs` — optimize `execute_single_transaction` pre-apply

**Benchmark gate**: Expected: mean ≤ 240ms. If improvement < 3ms, investigate.

---

### Step 6: Reduce apply_storage_changes Overhead (Expected: −3 to −5ms)

**Problem**: `apply_soroban_storage_changes()` takes 69μs per TX (9ms/ledger). For
each storage change, it calls `get_or_compute_key_hash()` (now cached), looks up/mutates
state, and handles TTL updates. The function also iterates the full read-write footprint
to delete entries not returned by the host (the "erase-RW" loop).

**Solution**:
- The TTL key hash is already cached (Step 1)
- Pre-build the "seen keys" HashSet outside the loop to avoid per-iteration allocation
- Consider batch state mutations instead of per-change individual mutations

**Files to modify**:
- `crates/tx/src/operations/execute/invoke_host_function.rs` — optimize
  `apply_soroban_storage_changes` and `apply_soroban_storage_change`

**Benchmark gate**: Expected: mean ≤ 237ms. If improvement < 2ms, skip.

---

## Execution Protocol

For each step:

1. **Implement** the optimization
2. **Verify correctness**: `cargo test --all` + `cargo clippy --all` clean
3. **Verify parity**: `verify-execution` on ≥1000 consecutive mainnet ledgers
4. **Run benchmark**: benchmark protocol (3 runs, median of means)
5. **Evaluate**:
   - If improvement meets or exceeds the step's expected range → document results
     in the table below, commit, push, and proceed to next step
   - If improvement is below the step's minimum threshold → investigate root cause,
     attempt to fix. If fixed, re-benchmark and proceed
   - If not fixable → stop, document findings, alert human and wait for instructions

---

## Results

| Step | Commit | Mean | Δ from prev | Δ from baseline | Notes |
|------|--------|------|-------------|-----------------|-------|
| Baseline | `bd8f3f7` | 317.5ms | — | — | |
| 1: TTL key hash caching | `a1db12f` | 307.1ms | −10.4ms | −10.4ms | SHA-256 was <1μs/call |
| 2: Fast-path single-op | | | | | |
| 3: Offer/non-offer maps | | | | | |
| 4: Unified TX set parsing | | | | | |
| 5: Streamline fee_seq | | | | | |
| 6: Reduce apply_storage | | | | | |

---

## Projected Results

| Step | Expected Gain | Cumulative | Ratio vs stellar-core |
|------|--------------|------------|----------------------|
| Baseline | — | 317.5ms | 1.88x |
| 1: TTL key hash caching | −10ms | 307ms | 1.82x |
| 2: Fast-path single-op | −30 to −50ms | 257–277ms | 1.53–1.64x |
| 3: Offer/non-offer maps | −8 to −10ms | 247–269ms | 1.47–1.60x |
| 4: Unified TX set parsing | −10 to −15ms | 232–259ms | 1.38–1.54x |
| 5: Streamline fee_seq | −5 to −10ms | 222–254ms | 1.32–1.51x |
| 6: Reduce apply_storage | −3 to −5ms | 217–251ms | 1.29–1.49x |

**Projected best case: ~217ms (1.29x stellar-core)**
**Projected worst case: ~251ms (1.49x stellar-core)**

The 220ms acceptance target is achievable in the best case. The stretch goal of 190ms
is not reachable without either optimizing the upstream soroban-env-host crate or
fundamentally restructuring the state management layer (replacing delta snapshot
cloning with a lightweight overlay stack like stellar-core's LedgerTxn).

---

## Methodology Notes

### How the baseline was established

1. Built henyey release binary from commit `bd8f3f7` (pre-optimization main branch)
2. Ran `verify-execution --from 61349540 --to 61349640` with pre-warmed cache
3. Parsed `RUST_LOG=debug` output for per-ledger `apply_transactions` timing
4. Excluded first ledger (cold start: loads ~911K offers)
5. Computed mean/p50/p95 over remaining 136 ledgers

### How stellar-core reference was established

1. Ran stellar-core v25.2.0 (Docker `stellar/stellar-core:latest`) catchup on same
   ledger range: `catchup 61349640/101`
2. Parsed "applying ledger" → "Ledger close complete" timestamp pairs
3. Excluded first ledger, computed stats over 136 ledgers

### Linear regression methodology

Fit `time = a * classic_ops + b * soroban_ops + c` for both stellar-core and henyey.
Op counts from stellar-core's "applying ledger" log lines. Regression coefficients
decompose the gap into per-classic-op, per-soroban-op, and fixed overhead components.

### Profiling methodology (post-Step 1)

Added `std::time::Instant` instrumentation inside `execute_host_function_p25`
(encode/invoke/extract phases) and `execute_contract_invocation` (pre_checks/host/
apply/hash phases). Aggregated across ~5400 Soroban TXs on the benchmark range.
Instrumentation was temporary (reverted after data collection).
