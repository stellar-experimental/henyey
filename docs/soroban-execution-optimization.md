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

Mean workload per ledger: ~500 classic ops, ~257 soroban ops.

### Soroban Hot-Path Analysis

Each Soroban TX performs repeated XDR serialization + SHA256 hashing on the same keys:

1. **`compute_key_hash()`** — called for every ContractData/ContractCode key in the
   footprint, 5–10 times per key across different validation passes:
   - `load_soroban_footprint` (mod.rs:1315–1318)
   - `is_archived_contract_entry` (invoke_host_function.rs:1031)
   - `disk_read_bytes_exceeded` (invoke_host_function.rs:494)
   - `apply_soroban_storage_changes` (invoke_host_function.rs:681,689,813,887,928,933)

   Each call does: `key.to_xdr(Limits::none())` → heap alloc + serialize → `Sha256::digest()`.

2. **Entry XDR serialization for size measurement** — `disk_read_bytes_exceeded`
   serializes the full `LedgerEntry` to `Vec<u8>` just to get `.len()`.

3. **Entry XDR serialization for host setup** — `execute_host_function_p25`
   encodes host_function, resources, source, auth entries, and every footprint entry
   into XDR bytes before passing to the Soroban host.

### Setup/Teardown Overhead (31ms/ledger)

| Sub-phase | Time | Description |
|-----------|------|-------------|
| tx_parse | 7.4ms | `transactions_with_base_fee()` XDR deserialization |
| executor_setup | 10.5ms | `advance_to_ledger_preserving_offers()` + HashMap retain |
| phase_parse | 4.4ms | `soroban_phase_structure()` XDR parsing |
| post_exec | 7.9ms | Fee event generation + per-TX perf collection |
| fee_deduct + preload | 5.0ms | `pre_deduct_all_fees_on_delta()` + account delta load |

---

## Benchmark Protocol

All measurements use:
- **Binary**: release build (`cargo build --release --bin henyey -p henyey`)
- **Command**: `verify-execution --from 61349540 --to 61349640` (101 closes, protocol 25)
- **Cache**: `--cache-dir ~/data/<session>/cache` (pre-warmed from prior run)
- **Logging**: `RUST_LOG=info` for timing, `RUST_LOG=debug` for phase breakdown
- **Machine**: same host for all runs (no cross-machine comparisons)
- **Repetitions**: 3 runs, report median of means (first ledger excluded as cold start)

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

### Step 1: Embed TTL Key Hash in LedgerKey (Expected: −60 to −80ms)

**Problem**: `compute_key_hash()` does `key.to_xdr() + SHA256` for every
ContractData/ContractCode key. A typical TX has ~20 footprint keys, and each key is
hashed 5–10 times across validation passes. That's 100–200 XDR+SHA256 per TX, ~130
Soroban TXs per ledger = ~15,000 redundant computations.

**How stellar-core solves it**: `getTTLKey()` in `LedgerTypeUtils.cpp:30–38` computes
`sha256(xdr::xdr_to_opaque(e))` **once per key** and embeds the result directly in the
returned `LedgerKey::ttl().keyHash` field. In `InvokeHostFunctionOpFrame::addReads()`
(lines 360–505), `getTTLKey(lk)` is called once per footprint key. The loaded TTL entry
(with hash already inside the key) is cached in a local variable and reused for all
subsequent operations — archive checks, size metering, host buffer preparation. The hash
is never recomputed.

**Solution**: Match stellar-core's structural approach. During footprint loading in
`load_soroban_footprint`, compute each key's TTL `LedgerKey` once (including the hash).
Store the pre-built TTL key alongside the loaded entry. All downstream code
(`is_archived_contract_entry`, `disk_read_bytes_exceeded`, `apply_soroban_storage_changes`,
host setup) uses the pre-built TTL key instead of calling `compute_key_hash()`.

This means replacing the pattern:
```rust
let key_hash = compute_key_hash(key);  // called 5-10x per key
let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });
```
with a single pre-computed TTL key stored per footprint entry and threaded through
all call sites.

**Files to modify**:
- `crates/tx/src/operations/execute/invoke_host_function.rs` — all 18 `compute_key_hash()`
  call sites → use pre-built TTL key from footprint context
- `crates/tx/src/soroban/host.rs` — all 8 `compute_key_hash()` call sites → same
- `crates/ledger/src/execution/mod.rs` (`load_soroban_footprint`) — compute and store
  TTL keys during footprint loading

**Constraints**:
- TTL key must be computed from the original `LedgerKey` as loaded, not a mutated version
- Must not change observable behavior — only eliminates redundant computation
- Pre-built TTL keys need to flow through `execute_contract_invocation()`,
  `execute_host_function_p25()`, and `apply_soroban_storage_changes()`

**Benchmark gate**: Run benchmark protocol. Expected: −60 to −80ms (mean ≤ 258ms).
If improvement < 30ms, investigate before proceeding. If not fixable, stop and alert.

---

### Step 2: Serialize Entries Once, Reuse for Metering and Host (Expected: −20 to −30ms)

**Problem**: `disk_read_bytes_exceeded` (invoke_host_function.rs:428–431) serializes
every `LedgerEntry` in the footprint to XDR just to measure byte count. Later,
`execute_host_function_p25` serializes the same entries *again* to pass to the Soroban
host. Each entry is serialized at least twice.

**How stellar-core solves it**: In `addReads()` (lines 451–467), entries are serialized
once into a `CxxBuf` via `toCxxBuf(xdr::xdr_to_opaque(*entryOpt))`. The buffer's
`.data->size()` is read for disk read metering. The **same buffer** is stored in
`mLedgerEntryCxxBufs` and passed directly to the Soroban host for execution. For keys,
`xdr::xdr_size(lk)` computes size without full serialization. Zero redundant
serializations — one buffer serves both metering and host invocation.

**Solution**: During footprint loading, serialize each entry to `Vec<u8>` once and store
the buffer alongside the entry. `disk_read_bytes_exceeded` reads `.len()` from the cached
buffer. `execute_host_function_p25` passes the cached buffer to the host instead of
re-serializing. This eliminates all redundant entry serialization.

**Files to modify**:
- `crates/tx/src/operations/execute/invoke_host_function.rs` — `disk_read_bytes_exceeded`
  reads cached sizes; `execute_host_function_p25` uses cached buffers
- `crates/ledger/src/execution/mod.rs` (`load_soroban_footprint`) or a new per-TX
  footprint context struct — store `(LedgerEntry, Vec<u8>, u32)` tuples

**Constraints**:
- Size must be computed from the entry as loaded (pre-execution state), not after
  modification — `disk_read_bytes_exceeded` measures pre-execution sizes
- Buffers passed to the host must be the same bytes that `to_xdr()` would produce
- Memory cost: ~20 footprint entries × ~500 bytes avg = ~10KB per TX (negligible)

**Benchmark gate**: Run benchmark protocol. Expected: −20 to −30ms cumulative from
baseline (mean ≤ 238ms). If improvement < 10ms over Step 1, investigate before
proceeding. If not fixable, stop and alert.

---

### Step 3: Single-Pass Footprint Validation (Expected: −10 to −15ms)

**Problem**: Two separate loops iterate the full footprint before host invocation:
1. `footprint_has_unrestored_archived_entries()` — loops all footprint keys, checks
   archive status via `is_archived_contract_entry()` (which calls `compute_key_hash()`)
2. `disk_read_bytes_exceeded()` — loops all footprint keys again, serializes entries
   for size metering

A third pass (`validate_and_compute_write_bytes()`) runs post-execution on storage
changes — this one is inherently separate since it needs host output.

**How stellar-core solves it**: `addReads()` (lines 360–505) is a **single pass** over
the footprint. For each key in one iteration: loads the entry, computes `getTTLKey()`,
checks archive status, calls `validateContractLedgerEntry()` for size limits, meters
disk read bytes, and prepares the host buffer. Write bytes are validated separately
post-execution in `recordStorageChanges()`.

**Notable**: stellar-core calls `validateContractLedgerEntry()` on **every** footprint
entry (read-only + read-write) during `addReads()`. Henyey only validates entry sizes
on written entries post-execution. This is a potential parity issue to investigate.

**Solution**: Merge the two pre-execution validation passes into a single
`process_footprint()` function that, for each key:
1. Loads the entry (already done in `load_soroban_footprint`)
2. Checks archive status (using pre-built TTL key from Step 1)
3. Meters disk read bytes (using cached buffer size from Step 2)
4. Validates entry size limits (matching stellar-core's per-entry check)

This naturally composes with Steps 1 and 2 — the pre-built TTL key and cached buffer
are produced during the single pass and consumed by all checks.

**Files to modify**:
- `crates/tx/src/operations/execute/invoke_host_function.rs` — replace
  `footprint_has_unrestored_archived_entries()` + `disk_read_bytes_exceeded()` with a
  single `process_footprint()` that returns `(has_archived, read_bytes_exceeded,
  entry_size_exceeded)`

**Constraints**:
- Error priority must match stellar-core: archived check → entry size → disk read bytes
- Post-execution write validation remains a separate pass (needs host output)
- Investigate the `validateContractLedgerEntry()` parity gap on read-only entries

**Benchmark gate**: Run benchmark protocol. Expected: −10 to −15ms cumulative from
Step 2 (mean ≤ 228ms). If improvement < 5ms over Step 2, investigate before proceeding.
If not fixable, stop and alert.

---

### Step 4: Unified TX Set Parsing (Expected: −10ms)

**Problem**: `transactions_with_base_fee()` (7.4ms) and `soroban_phase_structure()`
(4.4ms) each parse/iterate the `GeneralizedTransactionSet` XDR independently.
Called on every `apply_transactions()` invocation. Post-execution fee event generation
calls `transactions_with_base_fee()` a second time and constructs a new
`TransactionFrame` per TX just to extract fee source accounts.

**How stellar-core solves it**: TX set is parsed once in
`ApplicableTxSetFrame::prepareForApply()`, which creates `TransactionFrame` objects
and organizes them into phases/stages/clusters. The result is cached in the
`ApplicableTxSetFrame`. `getPhasesInApplyOrder()` is lazy (mutable field, computed
once on first access). Fee events are emitted during TX execution using the
already-constructed `TransactionFrame` — no post-execution re-parsing.

**Solution**: Parse the TX set once into a cached `PreparedTxSet` struct stored on
`CloseData`. Contains pre-sorted classic TXs with base fees, pre-parsed Soroban
phase structure, and pre-extracted fee source `AccountId` per TX. All consumers
(`transactions_with_base_fee()`, `soroban_phase_structure()`, fee event generation)
read from the cached data.

**Files to modify**:
- `crates/ledger/src/execution/tx_set.rs` — add `PreparedTxSet` with lazy
  initialization; `transactions_with_base_fee()` and `soroban_phase_structure()`
  delegate to it
- `crates/ledger/src/manager.rs` — fee event generation uses cached fee source
  accounts from `PreparedTxSet` instead of re-parsing

**Benchmark gate**: Run benchmark protocol. Expected: −10ms cumulative from Step 3
(mean ≤ 218ms). If improvement < 5ms over Step 3, investigate before proceeding.
If not fixable, stop and alert.

---

### Step 5: Separate Offer and Non-Offer Metadata Maps (Expected: −10ms)

**Problem**: `clear_cached_entries_preserving_offers()` calls `.retain()` on three
maps (`entry_sponsorships`, `entry_sponsorship_ext`, `entry_last_modified`), iterating
all entries to keep only Offer keys. These maps can contain entries for all types
(accounts, trustlines, contracts, etc.) accumulated during a ledger. The `.retain()`
cost is O(total entries) regardless of how many are offers.

**How stellar-core solves it**: No equivalent cost. State is ephemeral per-ledger via
scope-based `LedgerTxn`. No clearing or retention needed between ledgers. stellar-core
pays for this with per-access SQL overhead instead.

**Solution**: Split each of the three maps into an offer-specific map and a non-offer
map:
```
entry_sponsorships       → offer_sponsorships + non_offer_sponsorships
entry_sponsorship_ext    → offer_sponsorship_ext + non_offer_sponsorship_ext
entry_last_modified      → offer_last_modified + non_offer_last_modified
```

On insert, route to the correct map based on `LedgerKey::Offer(_)` match. On lookup,
check both maps. On `clear_cached_entries_preserving_offers()`, drop the non-offer
maps with `.clear()` (O(1) amortized) and leave the offer maps untouched. No
`.retain()` iteration needed.

**Files to modify**:
- `crates/tx/src/state/mod.rs` — split the three maps, update all insert/get/remove
  sites, simplify `clear_cached_entries_inner()`

**Benchmark gate**: Run benchmark protocol. Expected: −10ms cumulative from Step 4
(mean ≤ 213ms). If improvement < 5ms over Step 4, investigate before proceeding.
If not fixable, stop and alert.

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
| 1: TTL key embedding | | | | | |
| 2: Entry buffer caching | | | | | |
| 3: Single-pass validation | | | | | |
| 4: Unified TX set parsing | | | | | |
| 5: Offer/non-offer map split | | | | | |

---

## Projected Results

| Step | Expected Gain | Cumulative | Ratio vs stellar-core |
|------|--------------|------------|----------------------|
| Baseline | — | 317.5ms | 1.88x |
| 1: TTL key embedding | −60 to −80ms | ~240–258ms | 1.42–1.53x |
| 2: Entry buffer caching | −20 to −30ms | ~210–238ms | 1.25–1.41x |
| 3: Single-pass validation | −10 to −15ms | ~195–228ms | 1.16–1.35x |
| 4: Unified TX set parsing | −10ms | ~185–218ms | 1.10–1.29x |
| 5: Offer/non-offer map split | −10ms | ~175–213ms | 1.04–1.26x |

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
