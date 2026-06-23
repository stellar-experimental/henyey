---
name: perf-optimize
description: Iterative performance optimization workflow for the apply-load benchmark
argument-hint: <target-tps>
---

Parse `$ARGUMENTS`:
- Extract the target TPS number. If missing or not a valid number, ask the user to provide one (e.g. `/perf-optimize 5000`).
- Store as `$TARGET_TPS`.

# Performance Optimization Workflow

Iteratively optimize henyey's apply-load single-shot benchmark to reach
`$TARGET_TPS` transactions per second.

**Hard constraint**: No protocol changes — observed behavior (transaction
results, ledger hashes, meta) must stay identical. Only internal implementation
performance is in scope.

**Major refactorings are allowed and encouraged** when they unlock performance
gains. Don't shy away from changing data structures (e.g. `Vec` → `Arc`),
reworking function signatures, or restructuring hot paths across crate
boundaries. Correctness is verified by the test suite, not by minimizing diff
size.

## Phase 1: Baseline Measurement

1. Generate a session ID (8-char random hex). All session data goes under
   `~/data/<session-id>/`.
2. Build a release binary:
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
   ```
3. Run the apply-load benchmark **three times** and record each result:
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo test --release -p henyey-ledger --test apply_load -- --nocapture
   ```
   If `apply_load` doesn't exist yet, look for the closest benchmark test
   (grep for `apply_load`, `bench`, or `perf` in `crates/`). If none exists,
   tell the user and stop.
4. Record the median TPS as `$BASELINE_TPS`.
5. If `$BASELINE_TPS >= $TARGET_TPS`, report success and stop — target already
   met.

## Phase 2: Hypothesis Generation

Create or update `docs/perf-hypotheses.md` with this format:

```markdown
# Performance Hypotheses

Baseline: <BASELINE_TPS> TPS | Target: <TARGET_TPS> TPS | Date: <today>

## Hypotheses

| # | Hypothesis | Status | Expected Gain | Measured Gain | TPS After |
|---|-----------|--------|---------------|---------------|-----------|
| 1 | <description> | pending | | | |
```

To generate initial hypotheses:

1. **Profile**: Run the benchmark under a sampling profiler if available
   (`cargo instruments`, `samply`, or `perf`). Identify the top 5 hotspots
   by cumulative time.
2. **Review hot paths**: Read the source of the hottest functions. Look for:
   - Redundant allocations or cloning
   - Repeated serialization/deserialization (especially XDR encode/decode)
   - Lock contention or unnecessary synchronization
   - Cache misses from data layout
   - Unnecessary database round-trips or flushes
   - Work that could be batched or parallelized
3. **Compare with stellar-core**: Check if stellar-core uses a faster approach
   for the same logic (e.g. arena allocation, batch DB writes, lazy evaluation).
4. List each hypothesis in the table with status `pending`.

## Phase 3: Iterative Optimization Loop

For each hypothesis (in priority order — highest expected gain first):

### Step A: Instrument
- Add targeted timing instrumentation around the relevant code path.
- Run the benchmark once to get a precise measurement of that path's cost.
- Record the timing in the hypothesis doc.

### Step B: Prototype
- Implement the optimization on a **branch** (or stash-able change set).
- Keep changes minimal and focused on one hypothesis at a time.
- **Do not change observable behavior** — transaction results, ledger hashes,
  and emitted meta must remain identical.

### Step C: Measure
- Run the benchmark **three times** with the optimization applied.
- Record the median TPS and the delta from the previous best.
- Update the hypothesis table:
  - `measured_gain`: percentage improvement
  - `tps_after`: new median TPS
  - `status`: `accepted` if gain > 1%, `rejected` if not

### Step D: Document & Decide
- If **accepted**: commit the change with a message like
  `Optimize <what>: +X% TPS (<old> -> <new>)`. Update the hypothesis status.
- If **rejected**: revert the change, set status to `rejected`, note why.
- If the current TPS `>= $TARGET_TPS`: **stop** — target reached. Print a
  summary and update the hypothesis doc with final results.

### Step E: Discover New Hypotheses
- During instrumentation or prototyping, if you discover a new optimization
  opportunity, **add it to the hypothesis table** with status `pending`.
- Re-sort pending hypotheses by expected gain before picking the next one.

### Repeat
- Pick the next `pending` hypothesis and go to Step A.
- If all hypotheses are exhausted and target is not reached, report the final
  TPS achieved and the remaining gap.

## Measurement Protocol

- Always build with `--release`.
- Run each measurement **3 times**, take the **median**.
- Ensure no other heavy processes are running during measurement.
- Report TPS with the transaction count and wall-clock time used to compute it.
- Between runs, drop filesystem caches if possible (`purge` on macOS).

## Summary Report

When the loop terminates (target reached or hypotheses exhausted), print:

```
## Performance Optimization Summary

Baseline:  <BASELINE_TPS> TPS
Final:     <FINAL_TPS> TPS
Target:    <TARGET_TPS> TPS
Improvement: +<PERCENT>%
Status:    <target reached | gap remaining>

Accepted optimizations:
- <hypothesis>: +X% (<old> -> <new> TPS)
- ...

Rejected hypotheses:
- <hypothesis>: <reason>
- ...
```

Update `docs/perf-hypotheses.md` with the final summary.
