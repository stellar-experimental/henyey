---
name: perf-optimize-uftrace
description: uftrace-powered iterative performance optimization for the apply-load benchmark
argument-hint: <target-tps>
---

Parse `$ARGUMENTS`:
- Extract the target TPS number. If missing or not a valid number, ask the user
  to provide one (e.g. `/perf-optimize-uftrace 20000`).
- Store as `$TARGET_TPS`.

# uftrace Performance Optimization Workflow

Iteratively optimize henyey's apply-load single-shot benchmark to reach
`$TARGET_TPS` transactions per second, using **uftrace function tracing** for
precise, deterministic profiling at every stage.

**Hard constraint**: No protocol changes — observed behavior (transaction
results, ledger hashes, meta) must stay identical. Only internal implementation
performance is in scope.

**Major refactorings are allowed and encouraged** when they unlock performance
gains. Don't shy away from changing data structures, reworking function
signatures, or restructuring hot paths across crate boundaries. Correctness is
verified by the test suite, not by minimizing diff size.

---

## Phase 0: Prerequisites & Setup

1. Generate a session ID (8-char random hex). All session data goes under
   `~/data/<session-id>/`.

2. **Verify tools exist.** Check for each tool and install if missing:

   | Tool | Check | Install |
   |------|-------|---------|
   | `uftrace` | `which uftrace` | See https://github.com/namhyung/uftrace |
   | `rustfilt` | `which rustfilt` | `cargo install rustfilt` |
   | `inferno-flamegraph` | `which inferno-flamegraph` | `CARGO_TARGET_DIR=~/data/<session-id>/cargo-inferno cargo install inferno` |
   | Rust nightly | `rustup run nightly rustc --version` | `rustup toolchain install nightly` |

   If `uftrace` is not installed, stop and tell the user — it requires system
   installation (not a cargo package).

---

## Phase 1: Build Two Binaries

Build two separate release binaries with different cargo target directories.

### 1a. Measurement Binary (LTO)

Standard release build for accurate TPS measurement. Uses the workspace's
normal `[profile.release]` settings (LTO, codegen-units=1).

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target \
  cargo build --release -p henyey
cp ~/data/<session-id>/cargo-target/release/henyey \
  ~/data/<session-id>/henyey-measure
```

### 1b. Instrumented Binary (no-LTO, mcount)

Built with nightly for `-Z instrument-mcount` support. LTO is **disabled** so
that vendored crate functions (soroban-env-host, wasmi, ed25519-dalek, etc.)
are not inlined and remain visible as separate call frames in the trace.

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target-uftrace \
  RUSTFLAGS="-Z instrument-mcount" \
  CARGO_PROFILE_RELEASE_LTO=false \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo +nightly build --release -p henyey
cp ~/data/<session-id>/cargo-target-uftrace/release/henyey \
  ~/data/<session-id>/henyey-trace
```

**Why two builds?**
- LTO inlines cross-crate functions (especially SAC/soroban-env-host), making
  them invisible to uftrace. The no-LTO build preserves all call boundaries.
- The mcount instrumentation adds ~4-7x overhead, so TPS numbers from the
  instrumented binary are not meaningful for measuring optimization impact.
- The LTO binary matches production performance characteristics.

Verify the instrumented binary has mcount call sites:
```bash
nm ~/data/<session-id>/henyey-trace | grep -c "mcount"
# Expect 15,000-35,000 sites
```

---

## Phase 2: Baseline Measurement

Run the apply-load benchmark **3 times** with the measurement (LTO) binary:

```bash
~/data/<session-id>/henyey-measure apply-load \
  --mode single-shot --tx-count 50000 --clusters 4 --iterations 10
```

Parse `Average TPS: NNN` from each run. Record all 3 values and take the
**median** as `$BASELINE_TPS`.

If `$BASELINE_TPS >= $TARGET_TPS`, report success and stop.

---

## Phase 3: Trace Recording

Record a uftrace trace using the instrumented (no-LTO) binary.

```bash
uftrace record \
  -d ~/data/<session-id>/uftrace.data \
  -t 10us \
  --no-libcall \
  ~/data/<session-id>/henyey-trace apply-load \
    --mode single-shot --tx-count 50000 --clusters 4 --iterations 1
```

**Key flags and why they matter:**

| Flag | Purpose |
|------|---------|
| `-t 10us` | Filter out functions that complete in < 10 microseconds. Without this, the trace would be hundreds of gigabytes. This captures all meaningful work while keeping data to ~200-300MB. |
| `--no-libcall` | Skip tracing libc/PLT calls. We only care about application code. |
| `--iterations 1` | Single iteration keeps trace size manageable. The trace captures function-level timing, not statistical variance — one iteration is sufficient for profiling. |

Expected output: `~/data/<session-id>/uftrace.data/` containing thread-specific
`.dat` files totaling 200-400MB.

---

## Phase 4: Trace Analysis

### Step 1: Identify Thread Roles

List `.dat` files by size to classify threads:

```bash
ls -lhS ~/data/<session-id>/uftrace.data/*.dat | head -25
```

Thread classification by file size:
- **Main thread**: Largest file (200-350MB) — runs benchmark orchestration,
  ledger close, commit, bucket list operations
- **Worker threads**: ~30-40MB each — run `execute_single_cluster`, one per
  cluster (expect 4 for `--clusters 4`)
- **Merge threads**: ~10-20MB each — run background bucket merges
  (`merge_buckets_to_file_with_counters`)
- **Small threads**: < 1MB — tokio runtime, I/O, ignore these

Record the TIDs for: main thread, all worker threads, representative merge
thread.

### Step 2: Worker Thread Self-Time Report

The worker threads execute SAC transactions. Analyze one representative
worker thread:

```bash
uftrace report -d ~/data/<session-id>/uftrace.data/ \
  --tid <worker-tid> -t 10us -s self --no-pager 2>/dev/null \
  | rustfilt | head -80
```

The `self` column shows **exclusive** time spent in each function (not
including time in its children). This is the primary metric for identifying
optimization targets — functions with high self-time are doing the most
actual work.

Extract the top 50 functions. Categorize each into one of:

| Category | Example functions |
|----------|-------------------|
| Signature verification | `curve25519_dalek::*scalar_mul*`, `ed25519_dalek::*verify*`, `sqrt_ratio_i` |
| Host function setup/dispatch | `invoke_host_function_typed`, `invoke_host_function_core`, `call_n_internal`, `invoke_function`, `try_finish`, `build_storage_footprint_from_xdr`, `get_ledger_changes_typed` |
| SAC contract logic | `StellarAssetContract::transfer`, `receive_balance`, `spend_balance`, `is_authorized`, `write_contract_balance`, `read_asset`, `require_auth` |
| Storage operations | `load_soroban_footprint`, `apply_soroban_storage_changes`, `build_entry_changes_with_hot_archive`, `validate_footprint_entry`, `EntryStore::insert_created` |
| Bucket/snapshot lookup | `HotArchiveBucketList::get`, `BucketListSnapshot::load_keys_result`, `SnapshotHandle::prefetch` |
| XDR serialization | `Transaction::write_xdr`, `TransactionEnvelope::to_xdr`, `TransactionFrame::hash`, `inner_tx_size_bytes` |
| Metered XDR (soroban) | `LedgerEntryData::write_xdr` with `MeteredWrite`, `ScMap::write_xdr` |
| Event handling | `externalize` (events), `transfer_event`, `Budget::shadow_mode` |
| HashMap/allocation | `reserve_rehash`, `drop_in_place`, `__rust_realloc`, `__rust_dealloc` |
| Cleanup/drop | `drop_in_place::<TransactionExecutor>`, `drop_in_place::<InvokeHostFunctionTypedResult>` |
| Wasmi overhead | `Linker::clone`, `BTreeMap::clone::clone_subtree` |

Compute the percentage of total active self-time for each category.

### Step 3: Worker Thread Call Graph

Get the hierarchical call graph to understand where time accumulates:

```bash
uftrace graph -d ~/data/<session-id>/uftrace.data/ \
  --tid <worker-tid> -t 100us --no-pager 2>/dev/null \
  | rustfilt | head -120
```

This shows the call hierarchy with cumulative timing. Trace the critical path:
```
execute_single_cluster
  -> execute_transaction_with_request
    -> pre_apply_arc (signature verification, XDR size calc)
    -> apply_body
      -> execute_operation_with_soroban
        -> execute_invoke_host_function
          -> execute_host_function_p25
            -> invoke_host_function_typed
              -> invoke_host_function_core
                -> Host::invoke_function
                  -> Host::call_n_internal
                    -> StellarAssetContract::call
                      -> StellarAssetContract::transfer
```

### Step 4: Main Thread Analysis

The main thread runs commit/bucket operations. Use a higher time filter
since these are coarser-grained operations:

```bash
uftrace report -d ~/data/<session-id>/uftrace.data/ \
  --tid <main-tid> -t 1ms -s self --no-pager 2>/dev/null \
  | rustfilt | head -50
```

Look for: `BucketList::add_batch_impl`, `merge_buckets_to_file_with_counters`,
`LiveBucketIndex::from_entries_default_with_iter`, `LedgerDelta::merge`,
`commit`, `close_ledger`.

### Step 5: Generate Flamegraph SVGs

Generate demangled flamegraph SVGs for visualization. These are the primary
visual artifacts — interactive SVGs where you can click to zoom into call
stacks.

**Worker threads flamegraph** (all clusters combined):

```bash
for tid in <worker-tid-1> <worker-tid-2> <worker-tid-3> <worker-tid-4>; do
  uftrace dump --flame-graph \
    -d ~/data/<session-id>/uftrace.data/ \
    --tid $tid -t 100us 2>/dev/null
done | rustfilt > ~/data/<session-id>/flamegraph-workers.txt

inferno-flamegraph \
  --title "Henyey SAC Workers (<tx-count> TPS, <clusters> clusters)" \
  < ~/data/<session-id>/flamegraph-workers.txt \
  > ~/data/<session-id>/flamegraph-workers.svg
```

**Main thread flamegraph**:

```bash
uftrace dump --flame-graph \
  -d ~/data/<session-id>/uftrace.data/ \
  --tid <main-tid> -t 1ms 2>/dev/null \
  | rustfilt > ~/data/<session-id>/flamegraph-main.txt

inferno-flamegraph \
  --title "Henyey Main Thread (commit/bucket ops)" \
  < ~/data/<session-id>/flamegraph-main.txt \
  > ~/data/<session-id>/flamegraph-main.svg
```

**Detailed single-worker flamegraph** (lower filter for SAC internals):

```bash
uftrace dump --flame-graph \
  -d ~/data/<session-id>/uftrace.data/ \
  --tid <worker-tid> -t 10us 2>/dev/null \
  | rustfilt > ~/data/<session-id>/flamegraph-worker-detail.txt

inferno-flamegraph \
  --title "Henyey Worker Detail (SAC internals, 10us)" \
  < ~/data/<session-id>/flamegraph-worker-detail.txt \
  > ~/data/<session-id>/flamegraph-worker-detail.svg
```

Report the paths of all generated SVGs to the user.

### Step 6: Quantify Optimization Targets

For each category from Step 2, compute:
- Total self-time across all functions in the category
- Percentage of active work time (total wall minus idle/condvar wait)
- Per-transaction average (self-time / tx-count)

Rank categories by self-time. For the top 5 categories:
1. Read the source code of the highest self-time function(s) in that category
2. Identify the specific inefficiency (redundant work, allocations, etc.)
3. Estimate expected TPS gain: `(category_self_time / total_active_time) * current_TPS * improvement_fraction`
4. Generate a concrete hypothesis

---

## Phase 5: Hypothesis Document

Create or update `docs/perf-hypotheses-uftrace.md`:

```markdown
# Performance Hypotheses (uftrace)

Baseline: <BASELINE_TPS> TPS | Target: <TARGET_TPS> TPS | Date: <today>

## uftrace Profile Summary

| Category | Self-time | % of active | Per-TX avg |
|----------|-----------|-------------|------------|
| Host function setup | X.XXs | XX.X% | XXXus |
| SAC contract logic | X.XXs | XX.X% | XXXus |
| ... | ... | ... | ... |

## Hypotheses

| # | Hypothesis | Target function(s) | Self-time before | Status | Expected gain | Measured gain | TPS after |
|---|-----------|-------------------|------------------|--------|---------------|---------------|-----------|
| 1 | <description> | <function> | X.XXs | pending | +X% | | |
```

---

## Phase 6: Iterative Optimization Loop

For each hypothesis (highest expected gain first):

### Step A: Read & Plan

Read the source of the target function(s). Understand why self-time is high.
Look for:
- Redundant allocations or cloning
- Repeated XDR serialization/deserialization
- Lock contention or unnecessary synchronization
- Cache misses from data layout
- Unnecessary database round-trips or flushes
- Work that could be batched, cached, or parallelized
- Unnecessary work for SAC/builtin contracts (e.g., wasmi linker clone when
  no WASM is executed)

Compare with stellar-core if the function has a C++ counterpart.

### Step B: Implement

- Implement the optimization.
- Keep changes focused on one hypothesis at a time.
- **Do not change observable behavior** — transaction results, ledger hashes,
  and emitted meta must remain identical.

### Step C: Measure TPS

Rebuild the measurement (LTO) binary and run the benchmark 3 times:

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target \
  cargo build --release -p henyey
~/data/<session-id>/cargo-target/release/henyey apply-load \
  --mode single-shot --tx-count 50000 --clusters 4 --iterations 10
```

Record the median TPS and compute delta from previous best.

### Step D: Targeted Re-trace

Rebuild the instrumented (no-LTO) binary and record a targeted trace to
confirm the optimization reduced self-time in the target function(s).

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target-uftrace \
  RUSTFLAGS="-Z instrument-mcount" \
  CARGO_PROFILE_RELEASE_LTO=false \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo +nightly build --release -p henyey
```

Record a new trace (1 iteration, 10us filter):

```bash
uftrace record \
  -d ~/data/<session-id>/uftrace-after-<hypothesis-number>.data \
  -t 10us --no-libcall \
  ~/data/<session-id>/cargo-target-uftrace/release/henyey apply-load \
    --mode single-shot --tx-count 50000 --clusters 4 --iterations 1
```

Query the target function's self-time on a worker thread:

```bash
uftrace report \
  -d ~/data/<session-id>/uftrace-after-<N>.data/ \
  --tid <worker-tid> -t 10us -s self --no-pager 2>/dev/null \
  | rustfilt | grep -i "<target-function-pattern>"
```

Compare self-time before and after. Record both values.

### Step E: Decide

- **Accept** if TPS gain > 1% AND the target function's self-time decreased
  (or the function was eliminated entirely).
- **Reject** if no meaningful TPS gain.
- **Investigate** if TPS improved but self-time didn't change — the gain may
  have come from a different code path. Check the full self-time report for
  unexpected changes.

### Step F: Document & Commit

- Update the hypothesis table with measured gain, self-time before/after, and
  new TPS.
- If **accepted**: commit with message like
  `Optimize <what>: +X% TPS (<old> -> <new>)`.
- If **rejected**: revert the change, set status to `rejected`, note why.
- If significant gain (>3%): regenerate the worker flamegraph SVG to show the
  updated profile.
- If current TPS `>= $TARGET_TPS`: **stop** — target reached.

### Step G: Discover New Hypotheses

During implementation, if you discover new optimization opportunities:
- Add them to the hypothesis table with status `pending`
- Include the target function and estimated self-time from the trace
- Re-sort pending hypotheses by expected gain

### Repeat

Pick the next `pending` hypothesis and go to Step A. If all hypotheses are
exhausted and target is not reached, report the final TPS and remaining gap.

---

## Measurement Protocol

- **TPS measurement**: Always use the LTO binary. Run 3 times, take the median.
- **Trace recording**: Always use the no-LTO instrumented binary. 1 iteration,
  `-t 10us` filter, `--no-libcall`.
- **Thread analysis**: Always use `--tid <tid>` when running `uftrace report`
  or `uftrace graph`. Never run these commands on the full trace without a
  thread filter — the data volume will cause timeouts.
- **Demangling**: Always pipe uftrace output through `rustfilt`. Rust symbols
  are mangled and unreadable without it.
- **Time filters for output commands**:
  - `uftrace report`: `-t 10us` for workers, `-t 1ms` for main thread
  - `uftrace graph`: `-t 100us` for readable hierarchy
  - `uftrace dump --flame-graph`: `-t 100us` for workers, `-t 1ms` for main
  - `uftrace dump --flame-graph` (detail): `-t 10us` for single worker
- Ensure no other heavy processes are running during measurement.
- Report TPS with the transaction count and wall-clock time used to compute it.

---

## Common Pitfalls (from experience)

1. **Dynamic patching doesn't work on LTO'd Rust binaries.** Do not attempt
   `uftrace -P. --force` or `uftrace --patch` on the LTO binary — it will
   only capture a handful of libc calls. The `-Z instrument-mcount` nightly
   flag is required.

2. **LTO inlines vendored crate functions.** With LTO enabled, functions from
   `soroban-env-host`, `ed25519-dalek`, `wasmi`, etc. are inlined into their
   callers and become invisible to uftrace. The no-LTO build
   (`CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`)
   preserves all call boundaries.

3. **Full trace analysis times out.** Running `uftrace report` on a 300MB+
   trace without `--tid` will exceed the 2-minute command timeout. Always
   filter by thread ID.

4. **`uftrace graph -F` uses mangled names.** The `-F` (function filter) flag
   in `uftrace graph` operates on mangled symbol names, not demangled. If a
   filter fails with "failed to set filter", either drop the `-F` flag and
   grep the output instead, or find the exact mangled name with
   `nm <binary> | grep <pattern>`.

5. **Idle time dominates wall time.** Worker threads spend 60-70% of wall
   time waiting on condvars (tokio blocking pool). When computing percentages,
   subtract idle time to get the active work time that matters.

6. **The no-LTO binary is 4-7x slower.** Due to mcount instrumentation
   overhead and lack of cross-crate inlining. TPS numbers from the
   instrumented binary are meaningless for optimization measurement — they
   are only useful for relative self-time comparisons.

---

## Summary Report

When the loop terminates (target reached or hypotheses exhausted), print:

```
## Performance Optimization Summary (uftrace)

Baseline:     <BASELINE_TPS> TPS
Final:        <FINAL_TPS> TPS
Target:       <TARGET_TPS> TPS
Improvement:  +<PERCENT>%
Status:       <target reached | gap remaining>

Accepted optimizations:
- <hypothesis>: +X% (<old> -> <new> TPS)
  Self-time: <function> <before>ms -> <after>ms (-Y%)
- ...

Rejected hypotheses:
- <hypothesis>: <reason>
- ...

Artifacts:
- Baseline flamegraph:  ~/data/<session-id>/flamegraph-workers.svg
- Final flamegraph:     ~/data/<session-id>/flamegraph-workers-final.svg
- Main thread:          ~/data/<session-id>/flamegraph-main.svg
- Trace data:           ~/data/<session-id>/uftrace.data/
- Hypothesis document:  docs/perf-hypotheses-uftrace.md
```

Update `docs/perf-hypotheses-uftrace.md` with the final summary.
