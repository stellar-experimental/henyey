---
name: review-perf-fix
description: Review a performance fix for correctness and measure its necessity
argument-hint: <commit-or-range> [--apply]
---

Parse `$ARGUMENTS`:
- If the argument contains `..`, treat it as a range `$START..$END`. Expand to
  the list of commits with `git rev-list --reverse $START..$END` and set
  `$MODE_BATCH = true`. Each commit is reviewed individually; a summary table is
  appended at the end.
- Otherwise, the first argument is a single commit hash. Replace `$COMMIT` with it.
  Set `$MODE_BATCH = false`.
- If `--apply` is present, set `$APPLY = true`. Otherwise `$APPLY = false`.

# Performance Fix Review

Review a performance optimization commit for correctness (Phase 1) and measure
whether it actually improves throughput (Phase 2). Produce a necessity judgment
combining measured impact, code complexity, and risk.

**Benchmark command** (fixed for all measurements):
```
<binary> apply-load --mode single-shot --tx-count 50000 --clusters 4 --iterations 10
```

---

## Setup

1. Generate a session ID (8-char random hex). All session artifacts go under
   `~/data/<session-id>/`.
2. Record the current HEAD hash as `$HEAD`.
3. Ensure the working tree is clean (`git status --porcelain` must be empty).
   If not, abort with an error.

---

## Phase 1: Correctness Review

For each `$COMMIT`:

### Step 1: Read the Commit

```bash
git log -1 --format=fuller $COMMIT
git show $COMMIT --stat
git diff $COMMIT~1..$COMMIT
```

Identify: commit message, author, files changed, lines added/removed, and the
full diff. Read the complete current state of every file touched by the commit
to understand surrounding context.

### Step 2: Understand the Optimization

From the diff and commit message, reconstruct:

- **What hot path does it target?** Name the function(s) and crate(s).
- **What was the performance problem?** Describe the inefficiency in concrete
  terms (e.g., "O(n) scan on every TX commit", "redundant XDR serialization",
  "unnecessary clone of 2 KB struct").
- **What is the optimization strategy?** Categorize: caching, batching,
  parallelism, data structure change, algorithm change, allocation elimination,
  lazy evaluation, structural comparison, etc.

Read the code before and after. Do not guess — if the strategy is unclear, read
callers, callees, and related types until you understand it.

### Step 3: Analyze Correctness

Evaluate along these dimensions:

- **Semantic preservation**: Does the optimization change any observable
  behavior? Transaction results, ledger hashes, emitted meta, and error
  semantics must remain identical. If it changes internal ordering or timing,
  verify that no downstream code depends on that.
- **Edge cases**: Work through: empty inputs, zero values, overflow, maximum
  sizes, concurrent access, error/rollback paths. Caching optimizations: verify
  invalidation is correct. Parallelism: verify no data races or ordering
  dependencies.
- **Parity**: If the optimization touches protocol, consensus, or ledger logic,
  verify that the optimized behavior still matches stellar-core. Read the
  corresponding stellar-core code.
- **Side effects**: Could the optimization change behavior in any code path
  other than the targeted hot path? Check all callers of modified functions.

Classify correctness:
- **SOUND**: No correctness concerns.
- **CONCERNS**: Optimization is likely correct but has issues needing attention.
- **INCOMPLETE**: Missing edge case handling or invalidation logic.
- **WRONG**: Introduces incorrect behavior.

### Step 4: Verify Test Coverage

- Are there tests that exercise the optimized code path?
- Would existing tests catch a regression if the optimization introduced a
  subtle bug (e.g., stale cache, wrong comparison)?
- If test coverage is inadequate, describe what tests should exist.

### Step 5: Search for Similar Opportunities

Use subagents (Task tool with `explore` type) to find:
- Other locations with the same inefficiency pattern (e.g., if the fix caches
  a hash, are there other places that recompute the same hash repeatedly?).
- Only report confirmed opportunities — read the code to verify.

---

## Phase 2: Performance Measurement

**IMPORTANT — Measurement Isolation**: Baseline and without-fix measurements
MUST be taken sequentially in the same session. Do NOT reuse baseline
measurements from a prior session or a different day. Environmental variance
(CPU thermals, background load, OS scheduling) can cause 5–7% drift between
sessions on the same machine with the same binary. Always measure:
baseline → without-fix (or vice versa) back-to-back, with no significant
time gap between them.

### Step 6: Build Baseline Binary

Build the current HEAD as the baseline:

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
cp ~/data/<session-id>/cargo-target/release/henyey ~/data/<session-id>/baseline
```

### Step 7: Measure Baseline TPS

Run the benchmark **3 times** and record each result:

```bash
~/data/<session-id>/baseline apply-load --mode single-shot --tx-count 50000 --clusters 4 --iterations 10
```

Parse "Average TPS: NNN" from the output. Record all 3 values and compute
the **median** as `$BASELINE_TPS`.

### Step 8: Isolate and Remove the Fix

The goal is to create a version of current HEAD **without** this specific
optimization, so we can measure TPS without it.

Strategy — reverse-patch the commit onto HEAD:

```bash
git stash  # safety, even though we checked clean
git revert --no-commit $COMMIT
```

If `git revert` succeeds cleanly, proceed to Step 9.

If `git revert` has conflicts:

1. Examine each conflicting file. The conflicts arise because subsequent
   commits built on top of `$COMMIT`'s changes.
2. For each conflict, determine the correct resolution: the goal is to
   **remove the optimization's effect** while keeping all subsequent work
   intact. This means:
   - If the optimization changed a data structure (e.g., `Vec` → `HashMap`),
     and later commits use the new structure, the revert will break. In this
     case, you must adapt the subsequent code to work with the old structure.
   - If the optimization added a cache and later commits read from that cache,
     remove the cache reads too and restore the original computation.
   - If the conflict is a trivial context mismatch (surrounding lines changed),
     resolve by keeping the surrounding changes and only removing the
     optimization.
3. After resolving all conflicts, verify the result compiles:
   ```bash
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
   ```
4. If the build fails, attempt to fix compilation errors (missing fields,
   changed types, etc.) that result from removing the optimization.
5. If after reasonable effort (up to ~15 minutes of conflict resolution) the
   code still doesn't compile, **abort the measurement** for this commit:
   - `git checkout .` to restore HEAD
   - Report: "Unable to isolate — conflicts too deep to resolve cleanly"
   - The correctness review (Phase 1) is still valid; only the measurement
     is skipped.

### Step 9: Build Without-Fix Binary

```bash
CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
cp ~/data/<session-id>/cargo-target/release/henyey ~/data/<session-id>/without-fix
```

### Step 10: Measure Without-Fix TPS

Run the same benchmark 3 times with the without-fix binary:

```bash
~/data/<session-id>/without-fix apply-load --mode single-shot --tx-count 50000 --clusters 4 --iterations 10
```

Record all 3 values, compute median as `$WITHOUT_FIX_TPS`.

### Step 11: Restore and Clean Up

```bash
git checkout .
git clean -fd
```

Verify HEAD is back to `$HEAD`.

---

## Phase 3: Necessity Judgment

### Step 12: Compute Metrics

- **TPS delta**: `$BASELINE_TPS - $WITHOUT_FIX_TPS`
- **Percentage gain**: `(delta / $WITHOUT_FIX_TPS) * 100`
- **Complexity cost**: lines changed (from `git show --stat`), number of files
  touched, new abstractions introduced (new types, traits, caches, threads)
- **Risk level**: derived from Phase 1 correctness classification
  - SOUND → low risk
  - CONCERNS → medium risk
  - INCOMPLETE/WRONG → high risk

### Step 13: Render Verdict

Make a judgment call considering all three dimensions:

- **ESSENTIAL**: Large, clear TPS gain. Low complexity or complexity is well
  justified. No correctness concerns. Removing this would be a meaningful
  regression.
- **WORTHWHILE**: Moderate TPS gain that justifies the added complexity. Minor
  or no correctness concerns. Good engineering trade-off.
- **MARGINAL**: Small TPS gain. The optimization is correct and simple, so
  it doesn't hurt — but it wouldn't be missed if removed. Keep it, but don't
  build further complexity on top of it.
- **UNNECESSARY**: No measurable TPS gain, or the gain does not justify the
  complexity and/or correctness risk. Candidate for removal.

The verdict is a judgment call, not a mechanical threshold. A 0.5% gain from a
2-line change is WORTHWHILE; a 2% gain from a 500-line refactor with race
condition concerns might be UNNECESSARY. Explain your reasoning.

---

## Phase 4: Revert Recommendation (MARGINAL commits only)

For each commit with a **MARGINAL** verdict, assess whether it should be
reverted based on complexity cost vs. the value it provides.

### Step 14: Assess Complexity Cost

Evaluate:

- **Lines added/removed**: Net lines of code added. Large positive deltas
  increase maintenance burden.
- **Conceptual complexity**: New types, traits, abstractions, threading
  patterns, lifetime annotations, or API surface. Code that requires
  understanding a new concept to maintain is more expensive than mechanical
  changes.
- **Signature pollution**: Functions that gained new parameters (especially
  threaded-through `Option<&Cache>` patterns) impose cost on every caller.
- **Maintenance burden**: Will this code need updates when surrounding code
  changes? Does it add invariants that must be maintained?

### Step 15: Check Supersession

Determine whether a later commit in the series (or a subsequent change)
makes this optimization redundant:

- Does a later commit solve the same problem more completely?
- Does a later commit bypass the code path this optimization targets?
- If superseded, would removing this commit break the later one?

### Step 16: Render Revert Recommendation

Categorize each MARGINAL commit:

- **REVERT**: Complexity clearly outweighs value. The optimization is
  superseded, adds significant code surface, or introduces patterns that
  complicate maintenance. Remove it.
- **CONSIDER REVERTING**: Borderline. The optimization is partially
  superseded or has moderate complexity for no measurable gain. Decision
  depends on whether the non-benchmark production path exercises the code.
- **KEEP**: Low complexity (trivial changes, net-deletion, or purely
  mechanical improvements). The code is better with the change regardless
  of performance. Good Rust idioms, code cleanup, or production-path value.

Include a summary table grouping all MARGINAL commits by recommendation,
with commit hash, line counts, and a one-line reason for each.

---

## Output Format

For each commit, produce:

```
# Perf Fix Review: $COMMIT_SHORT

## Commit Summary
- **Hash**: full hash
- **Message**: commit message
- **Files changed**: list with line counts
- **Optimization category**: caching / parallelism / allocation / algorithm / etc.

## Correctness Review
- **Hot path**: function(s) and crate(s) targeted
- **Problem**: what was inefficient
- **Strategy**: what the optimization does
- **Semantic preservation**: any observable behavior changes?
- **Edge cases**: concerns or confirmation that they're handled
- **Parity**: stellar-core alignment (if applicable)
- **Test coverage**: adequate / gaps identified
- **Correctness verdict**: SOUND / CONCERNS / INCOMPLETE / WRONG

## Performance Measurement
- **Baseline (HEAD)**: X TPS (runs: a, b, c)
- **Without fix**: Y TPS (runs: a, b, c)
- **Delta**: +Z TPS (+P%)
- **Measurement notes**: any caveats (e.g., high variance, unable to isolate)

If measurement was skipped:
- **Measurement**: SKIPPED — <reason>

## Necessity Judgment
- **TPS gain**: +P% (+Z TPS)
- **Complexity**: N lines, M files, description of added abstractions
- **Risk**: low / medium / high
- **Verdict**: ESSENTIAL / WORTHWHILE / MARGINAL / UNNECESSARY
- **Rationale**: 2-3 sentences explaining the judgment

## Similar Opportunities
(Other locations that could benefit from the same optimization pattern.
If none: "No similar opportunities identified.")

## Recommendations
(Prioritized follow-up actions, if any.)

## Revert Recommendation (MARGINAL only)
- **Complexity cost**: lines, abstractions, signature pollution
- **Superseded by**: commit(s) that make this redundant, or "N/A"
- **Recommendation**: REVERT / CONSIDER REVERTING / KEEP
- **Rationale**: 1-2 sentences
```

### Batch Summary (range mode only)

After all individual reviews, append:

```
# Batch Summary

| # | Commit | Description | Baseline | Without | Delta | Correctness | Necessity |
|---|--------|-------------|----------|---------|-------|-------------|-----------|
| 1 | abc123 | Cache TTL hashes | 12000 | 11800 | +1.7% | SOUND | WORTHWHILE |
| 2 | def456 | ... | ... | ... | ... | ... | ... |

**Cumulative**: If all UNNECESSARY fixes were removed, estimated TPS would be ~X.

# Revert Recommendations (MARGINAL commits)

| Recommendation | Commit | # | Lines | Reason |
|----------------|--------|---|-------|--------|
| REVERT | abc123 | 1 | +134/−43 | High signature pollution, superseded by #15 |
| CONSIDER REVERTING | ... | ... | ... | ... |
| KEEP | ... | ... | ... | ... |
```

---

## Apply Mode

When `$APPLY = true`, after producing the full review:

### 1. Fix Correctness Issues

For commits rated CONCERNS or INCOMPLETE:
- Fix the identified issues (cache invalidation bugs, missing edge cases, etc.)
- Run `cargo clippy --all` and `cargo test --all` after each fix
- Commit each fix separately

### 2. Add Missing Tests

For commits with inadequate test coverage:
- Write tests that exercise the optimized path and would catch regressions
- Run `cargo test -p <crate>` to verify
- Commit: "Add regression test for <optimization description>"

### 3. Propose Removals

For commits rated UNNECESSARY:
- Do NOT auto-revert. Instead, print a clear recommendation:
  ```
  RECOMMENDATION: Consider reverting $COMMIT (<description>).
  Measured impact: +X TPS (+P%), but complexity cost is <description>.
  To revert: git revert $COMMIT
  ```
- Only revert if the user explicitly confirms.

### Apply Mode Rules

- One logical change at a time.
- Run `cargo clippy --all` and `cargo test --all` after each change.
- If a change breaks tests or introduces warnings, revert and move on.
- Each commit must include `Co-authored-by` trailers per AGENTS.md.

---

## Cleanup

When all reviews are complete (or on abort):

1. Ensure the working tree is clean and HEAD matches `$HEAD`.
2. Keep the session directory `~/data/<session-id>/` until the user confirms
   cleanup (it contains the binaries and benchmark logs for reproducibility).
3. Print: `Session artifacts: ~/data/<session-id>/`

---

## Guidelines

- Be precise. Cite `file:line` for every claim.
- Do not speculate — read the code until you understand the optimization.
- The benchmark is noisy. A <1% delta with overlapping run ranges is
  indistinguishable from noise. Say so explicitly rather than claiming a gain.
- For batch reviews, reuse the baseline binary across commits but
  **re-measure baseline TPS** at the start of each measurement session.
  If the session spans multiple hours or days, re-measure baseline before
  each batch of without-fix measurements. Each commit gets its own
  without-fix binary. Compare each without-fix result against the baseline
  measured in the same session.
- If a commit contains both perf and non-perf changes, isolate only the
  perf-relevant hunks when generating the reverse patch.
- Use subagents for codebase-wide searches to keep analysis thorough without
  overwhelming context.
- Time budget: ~15 minutes max on conflict resolution per commit. If it's not
  cleanly isolatable in that time, skip the measurement.
