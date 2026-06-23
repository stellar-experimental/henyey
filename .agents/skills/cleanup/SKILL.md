---
name: cleanup
description: Crate-scoped Rust cleanup audit and optionally apply, with parity-aware filtering and adversarial verification
argument-hint: <crate-path> [--mode=review|plan|apply]
---

Parse `$ARGUMENTS`:
- The first argument is the crate path (e.g. `crates/bucket`). Replace `$TARGET` with it.
- `--mode=review` (default), `--mode=plan`, `--mode=apply`. `--apply` is a back-compat alias for `--mode=apply`.

# Cleanup

A crate-scoped Rust audit for the henyey codebase. For diff-scoped cleanup
(typically as part of a PR review), use `/simplify` instead — this skill is for
whole-crate audits, intended for maintenance passes, not PR work.

## Modes

- **`review`** — read-only ranked list. Skips adversarial verify (cheap pass).
- **`plan`** — read-only, full pipeline including adversarial verify. Output is
  a ready-to-execute action list with per-finding verdicts. Use when a human
  will apply the fixes.
- **`apply`** — full pipeline + apply confirmed findings + commit per lane.

## Pipeline overview

```
Stage 1 (deterministic facts)
  → Stage 2 (4 finding lanes in parallel)
    → Stage 3 (adversarial verify, plan + apply only)
      → Stage 4 (rank + report)
        → Stage 5 (apply, apply mode only)
          → Stage 6 (commit, apply mode only)
            → Stage 7 (final report)
```

Lanes:

- **Lane A — Clippy backlog** (deterministic input, parity-aware)
- **Lane B — Idiom** (parity-exempt, Rust-idiomatic fixes)
- **Lane C — Structure & dead code** (parity-filtered, LLM-judged)
- **Lane D — Cross-crate** (advisory only, never auto-applied)

---

## Stage 1 — Establish ground truth

Run these *before* any LLM judgment. The outputs feed every later stage. **No
interpretation, no LLM calls yet** — just collect facts.

1. **Read the parity map.** `Read crates/$TARGET/PARITY_STATUS.md`. Extract the
   **File Mapping** table (`stellar-core File | Rust Module | Notes`). This is
   the authoritative `<rs file> ↔ <cpp file>` list — used throughout the
   pipeline as the source of truth for which code is parity-protected. Do NOT
   substitute filename heuristics; use this table.
   - If `PARITY_STATUS.md` doesn't exist for the crate, note that explicitly in
     the report and treat **all** files as not parity-mapped (no parity
     suppression applies). Surface "PARITY_STATUS.md missing" as a Lane C
     finding.
   - If `PARITY_STATUS.md` exists and declares `Upstream: No direct stellar-core
     source equivalent` (the "scoped contract" case — e.g. `henyey-clock`),
     treat **all** files as not parity-mapped for Lane C structural suppression.
     The crate's scoped API surface is still a contract (changes to the public
     trait shape may be a Lane B/C finding, not protected), but there is no
     upstream C++ structure to preserve. Do NOT surface this as a Lane C
     finding — it's by design.

2. **Project clippy baseline.** Run:
   ```
   cargo clippy -p <package-name> --all-targets 2>&1 | tee /tmp/cleanup-clippy-before.txt
   ```
   `<package-name>` is the Cargo package name (e.g. `henyey-bucket` for
   `crates/bucket`), not the directory. Look it up in `crates/$TARGET/Cargo.toml`.
   This honors `.cargo/config.toml` — which sets `-Dwarnings -Aclippy::style`.
   **Do NOT add `-W clippy::pedantic` or `-W clippy::nursery`.** The project has
   deliberately opted out of style lints; respecting that is the whole point.
   Pedantic generates hundreds of low-value warnings (e.g., dominated by
   `cast_possible_truncation`).

3. **Test baseline.** Run:
   ```
   cargo test -p <package-name> --no-fail-fast 2>&1 | tee /tmp/cleanup-test-baseline.txt
   ```
   Record which tests pass/fail BEFORE any change. Used in Stage 5 to attribute
   later failures correctly. A test that was already failing is not your
   regression.

4. **Workspace structure.** Run once (cached for Lane D):
   ```
   cargo metadata --format-version 1 --no-deps | jq -r '.packages[].name' | sort
   ```
   This is the universe of crates Lane D may suggest moving code into.

5. **Line counts.** Run:
   ```
   wc -l crates/$TARGET/src/**/*.rs 2>/dev/null | sort -n | tail
   ```
   For LARGE MODULE candidates in Lane C.

State the facts collected up front in the response as an explicit table — it's
the operational summary that drives the size-aware fast path in Stage 2 and
the regression checks in Stage 5. Use this format:

```
| Fact                 | Value                                                |
|----------------------|------------------------------------------------------|
| Package              | <name>                                               |
| Total LOC            | <sum> (across <N> source files)                      |
| Non-test LOC         | <sum, excluding `#[cfg(test)]` modules>              |
| Parity-mapped files  | <N> (of <total>); or "0 — scoped contract / missing" |
| Clippy warnings      | <N>                                                  |
| Tests                | <pass>/<fail>/<ignored>                              |
| Largest module       | <file> (<lines> lines)                               |
```

---

## Stage 2 — Fan out four finding lanes (parallel)

### Size-aware fast path

If the Stage 1 facts satisfy **all** of:

- Non-test LOC **< 200**, AND
- Clippy warnings **== 0**, AND
- Tests **0 fail** (vs baseline)

then **run all four lanes inline** in a single pass rather than fanning out
four agents. Fanning out has fixed overhead per agent (each independently
re-reads the same handful of files), which dominates the work when the crate
is small and clean. The output format is unchanged — still emit the same Stage
4 report shape. Note "inline lanes (small crate fast path)" in the report so
the deviation is visible.

Otherwise, fan out:

Launch four `Agent` invocations in a single message so they run concurrently.
Each agent gets: the crate path, the Stage 1 facts packet (parity map summary,
clippy log path, baseline log path, workspace crate list), and its lane's
specific brief below.

Each lane returns findings as structured items with these fields:

```
{ lane: "A" | "B" | "C" | "D",
  finding_id: "<lane>-<n>",
  file: "crates/<crate>/src/<path>:<line>",
  category: "<lane-specific>",
  summary: "<one-line>",
  evidence: "<concrete observation — quoted code, grep result, clippy line>",
  suggested_fix: "<brief fix description>",
  confidence: "high" | "medium" | "low",
  parity_relevant: true | false,
  parity_mapped_file: "<cpp filename from PARITY_STATUS.md>" | null }
```

**No silent cap per lane.** Lanes return everything they find. Ranking and
truncation happen in Stage 4, with full visibility.

### Lane A — Clippy backlog (deterministic)

If the Stage 1 clippy warning count is **0**, short-circuit: emit Lane A as
"skipped — clippy clean" in the Stage 4 report and do not spawn an agent.
Otherwise, read `/tmp/cleanup-clippy-before.txt`. For each warning:

- Classify into one of:
  - `auto-fixable` — `cargo clippy --fix` would resolve it (e.g.,
    `needless_clone`, `redundant_pattern_matching`, `manual_let_else`,
    `question_mark`, `needless_return`).
  - `manual-fixable` — requires human judgment but the fix is local
    (e.g., `cast_possible_truncation` where a narrower type clearly works).
  - `parity-protected` — the warning is on a file listed in the PARITY_STATUS.md
    File Mapping table, AND the fix would alter structure (e.g.,
    `too_many_arguments` on a parity-mapped function). Suppress with a noted
    `#[allow(...)]` reason if not already suppressed.
  - `false-positive` — the lint is wrong for this code (rare; require evidence).
- Set `parity_relevant: true` if the warning's file appears in the File Mapping
  table.
- Confidence is `high` for clippy-backed findings — the LLM is categorizing, not
  rediscovering.

### Lane B — Idiom (parity-exempt)

The goal is Rust-idiomatic code, not a C++ transliteration. Parity filter does
**not** apply to this lane — even parity-mapped files get idiom fixes, because
the C++ structure of the function is preserved while the *expression* of it
becomes Rust-native.

Categories:

- **C-STYLE MUTATION** — `&mut T -> bool` for success/failure. Rust idiom:
  `Result<(), E>` or `Option<T>`. *Skip* idiomatic in-place slice operations
  (`sort_by`, `sort_unstable_by`, `swap`).
- **C-STYLE OUTPUT PARAMETER** — `&mut Vec<T>` as output param. Rust idiom: return
  the value.
- **C-STYLE OPTION HANDLING** — `is_none()`/`is_some()` check followed by
  `.unwrap()`. Rust idiom: pattern matching, `let-else`, combinators.
- **VERBOSE ERROR PROPAGATION** — manual `match Ok/Err` chains, no-op
  `.map_err(|e| e)`. Rust idiom: `?`, `.map_err(Into::into)?`.
- **RUST API ANTI-PATTERNS** — `&Vec<T>` (use `&[T]`), `&String` (use `&str`),
  `Box<T>` for `Copy`-able T, missing `#[derive(Default)]` / `#[derive(Clone)]`
  where it would be uncontroversial.
- **CONCURRENCY ANTI-PATTERNS** — `Arc<Mutex<T>>` where `RwLock`, `&T`, or
  `Arc<T>` suffices; `MutexGuard` held across `.await`; `tokio::spawn` capturing
  more than the closure body needs.

Set `parity_relevant: false` on all Lane B findings.

### Lane C — Structure & dead code (parity-filtered)

Apply the parity filter from the PARITY_STATUS.md File Mapping table (Stage 1):

- If the file appears in the File Mapping table → set `parity_mapped_file` to
  the cpp counterpart and `parity_relevant: true`.
- **Suppress** findings on parity-mapped files for these categories: LARGE
  MODULE, GOD FUNCTION, DEEP NESTING, LONG PARAMETER LIST, DUPLICATION, MAGIC
  NUMBERS — these are structural choices that mirror upstream. Suppression
  reason in evidence field: "mirrors `<cpp file>` from PARITY_STATUS.md File
  Mapping".
- **Always report** regardless of parity: DEAD CODE, STALE COMMENTS,
  COMMENTED-OUT CODE, MISLEADING NAMES, OVERLY BROAD VISIBILITY.

Categories:

- **LARGE MODULE** — `.rs` over 1500 non-test lines. Require 3+ separable
  concerns, each ≥200 lines, to recommend a split. Never split solely to extract
  tests.
- **GOD FUNCTION** — function over 200 lines with non-trivial branching. *Skip*
  flat pipelines and config-translation functions (high line count but linear).
- **DEEP NESTING** — 4+ levels of indentation. Suggest early returns / guard
  clauses / extraction.
- **LONG PARAMETER LIST** — 7+ parameters.
- **DEAD CODE** — claims require workspace-wide evidence. Use:
  ```
  rg --type rust '\b<symbol>\b' /Users/tomer/dev/henyey/crates/
  ```
  to verify zero non-definition hits. Without workspace-wide evidence, demote
  to `confidence: low` and route to "Unverified candidates" in Stage 4 —
  do NOT emit as a confirmed finding.
- **DUPLICATION** — identical/near-identical logic in 2+ places. Show
  locations; if a shared form exists in `henyey-common` or sibling crates,
  cross-reference (and prefer emitting it as Lane D).
- **DUPLICATE STATE** — same truth tracked in 2+ places.
- **SCATTERED CONCERN** — logical operation done in N call sites instead of one.
  *Only emit* when consolidation is a net improvement (fewer lines, clearer
  intent).
- **OVERLY BROAD VISIBILITY** — `pub` items only used within crate (narrow to
  `pub(crate)`), `pub(crate)` items only used in one module. *Verify* downstream
  use with workspace-wide grep before flagging public re-exports; the crate's
  public API is not a finding.
- **MAGIC NUMBERS** — hardcoded literals that should be named. Skip values that
  match XDR-spec constants or stellar-core named constants.
- **MISLEADING NAMES** — identifier name does not match semantics. Suggest a
  rename.
- **STALE COMMENTS** — comment no longer matches code.
- **COMMENTED-OUT CODE** — dead code left as comments.
- **TRIVIAL WRAPPER** — one-line delegate with no added abstraction. Skip
  wrappers that shield an internal signature or provide semantic naming for
  call sites.
- **[PARITY+]** — a cleanup that would make parity *easier* to verify (e.g.,
  renaming a function to match stellar-core's name, extracting a function that
  maps 1:1 to an upstream function). Allowed even when touching parity-mapped
  code. Tag the finding's category with the `[PARITY+]` prefix.

### Lane D — Cross-crate (advisory only)

Findings in this lane are **never auto-applied**, regardless of mode. They are
emitted as follow-ups for human review.

Categories:

- Helpers in this crate that duplicate or could move to `henyey-common`,
  `henyey-crypto`, `henyey-db`, or other shared crates.
- `pub use` reorganization across the crate boundary (e.g., expose at workspace
  root, or stop re-exporting).
- Types defined in this crate that would fit better in a sibling crate.

Output as a `## Cross-crate follow-ups` section. In `apply` mode, additionally
note "to be filed as separate issue / PR" — do not edit other crates.

### Scope (all lanes)

- Do NOT flag issues within `#[cfg(test)]` modules or under `stellar-core/`.
- Do NOT flag issues in code emitted by `build.rs`. Empirically only
  `crates/henyey/build.rs` exists and it doesn't produce in-tree files; if
  generated files exist in a crate, they typically live under `target/` and
  are excluded by `wc -l` already.
- Test code MAY be referenced as evidence (e.g., to show a function is only
  called from tests when evaluating DEAD CODE or OVERLY BROAD VISIBILITY).

---

## Stage 3 — Adversarial verify (plan + apply modes only)

In `review` mode, skip this stage and go straight to Stage 4.

For every Lane B, C, or D finding with `confidence != low`, spawn a verify
agent. Lane A findings skip verify — clippy has already verified them
mechanically.

Each verify agent receives: the finding, the surrounding code, the relevant
PARITY_STATUS.md row (if any), and instructions to **argue against** the
finding. Each returns exactly one verdict:

- **`confirmed`** — finding is real, the proposed fix is feasible, no
  parity or behavior risk.
- **`parity-risk`** — touches a parity-mapped section in a way that would
  diverge from stellar-core. Reject.
- **`behavior-change`** — the fix is correctness-significant, not pure
  cleanup. Route to `/code-review` instead.
- **`infeasible`** — borrow checker, lifetimes, Send/Sync, or other
  type-level constraint blocks the proposed fix.
- **`false-positive`** — duplicates another finding, misreads the code, or
  the "issue" is intentional.

Verdict is recorded on the finding. `confirmed` findings move forward;
others are reported in the rejected section with the verdict reason.

The verify pass is the single biggest correctness defense in this skill. Do
not skip it in `plan` or `apply` mode.

---

## Stage 4 — Rank and report

Sort confirmed findings within each lane by impact (lines reduced, allocations
avoided, [PARITY+] bonus, then category severity). Output structure:

```
## Facts (from Stage 1)
- Package: <name>
- Parity-mapped files: <count> (of <total> in src/)
- Clippy warnings: <count>
- Tests: <pass>/<fail>/<ignored>
- Largest module: <file> (<lines> lines)

## Confirmed findings (will apply / safe to apply)
### Lane A — Clippy backlog (<N> findings)
<per-finding block, ranked>
### Lane B — Idiom (<N> findings)
<per-finding block, ranked>
### Lane C — Structure & dead code (<N> findings)
<per-finding block, ranked>

## Cross-crate follow-ups (Lane D, advisory only)
<per-finding block>

## Rejected by verify
### parity-risk (<N>)
### behavior-change (<N>)
### infeasible (<N>)
### false-positive (<N>)

## Unverified candidates
<low-confidence findings, e.g., DEAD CODE without workspace evidence>
```

Per-finding block format:

```
#### [LANE-N] CATEGORY — one-line summary
- **Location**: `file:line`
- **Parity**: <mapped cpp file> | not mapped | parity-exempt (Lane B)
- **Evidence**: <concrete observation>
- **Fix**: <brief>
```

In `review` and `plan` modes, stop here. The plan mode output is exactly the
"Confirmed findings" section above; the user/operator runs the fixes.

---

## Stage 5 — Apply (apply mode only)

Apply confirmed Lane A → Lane B → Lane C, in that order (least → most risky).
Each lane becomes one commit at the end.

**Per-change loop:**

1. Make the edit.
2. Run `cargo check -p <package-name>` (seconds).
3. If it fails: do NOT auto-revert. Read the error. Decide: fix-forward (the
   error is a minor follow-on, fix and continue) or revert this finding (the
   fix isn't viable as-is — record the verdict reason and move on).

**Per-lane verify (after all confirmed findings in the lane are applied):**

1. `cargo clippy -p <package-name> --all-targets 2>&1 | tee /tmp/cleanup-clippy-after-<lane>.txt`
2. `cargo test -p <package-name> --no-fail-fast 2>&1 | tee /tmp/cleanup-test-after-<lane>.txt`
3. **Compare against the Stage 1 baseline.** A test that was failing before is
   not your regression. Only new failures block the lane. New clippy warnings
   on lines you touched block; pre-existing warnings on unrelated lines do not.
4. If a real regression is found: bisect within the lane's changes to find the
   culprit; revert just that one and continue.

**End-of-run verify:**

```
cargo test --all 2>&1 | tail -50
```

This catches downstream crate breakage that `cargo test -p <crate>` misses
entirely. Per `AGENTS.md`, this is required before submitting. Do not skip.

**Termination conditions:**

- All confirmed findings applied, OR
- Clippy warning count stops decreasing across two lanes (the remaining warnings
  are intentional/parity-protected), OR
- Wall-clock budget exhausted (default: stop after 20 changes total). The
  current skill's "10 changes" cap was arbitrary; 20 is generous enough that
  the natural conditions usually fire first.

---

## Stage 6 — Commit (apply mode only)

One commit per lane (skip lanes with zero applied changes). Per `AGENTS.md`:

- Imperative, sentence case. Examples:
  - `Clean up henyey-bucket: fix clippy backlog`
  - `Clean up henyey-bucket: idiomatic Rust pass`
  - `Clean up henyey-bucket: structural simplification`
- Trailer: `Co-authored-by: Codex <Codex@anthropic.com>`
- If Lane C removed code that was listed in `crates/$TARGET/PARITY_STATUS.md`'s
  File Mapping or Component Mapping tables, update those tables in the same
  commit (per `AGENTS.md`'s parity-status maintenance rule).
- Before each commit: verify the `stellar-specs` submodule pointer didn't drift
  (per project convention — rebases have silently reverted it in the past).
  Check with `git diff --staged stellar-specs` and `git submodule status` —
  unstage any unintended submodule pointer change.
- Push after the run completes (not after each commit), once `cargo test --all`
  has passed. If push is rejected, pull with rebase and retry (per `AGENTS.md`).

---

## Stage 7 — Final report

```
## Cleanup summary for <package-name>

### Metrics
- Clippy warnings: <before> → <after>
- Tests (vs baseline): <new failures: 0 | N>, <newly passing: M>
- LOC delta: <±N>

### Applied
- Lane A: <count> findings → commit <sha> "<message>"
- Lane B: <count> findings → commit <sha> "<message>"
- Lane C: <count> findings → commit <sha> "<message>"
- PARITY_STATUS.md updates: <N rows touched> | none

### Deferred
- Cross-crate follow-ups: <count> (Lane D)
- Rejected by verify: <count> (see report above for breakdown)
- Unverified candidates: <count> (DEAD CODE without evidence, low-confidence)

### Final tests
- cargo test --all: <PASS | FAIL: tail of output>
- Push status: <pushed: sha | not pushed: reason>
```

---

## Notes on tools and dependencies

- `cargo-machete` and `cargo-udeps` are **not** required and are not installed
  in the project as of this writing. If either is available locally,
  the Lane C DEAD CODE detection may use them as additional evidence sources,
  but absence is not a blocker.
- `rg` (ripgrep) is required for workspace-wide grep evidence in Lane C. If not
  available, fall back to `grep -rn` from the workspace root.
- All commands run from the workspace root (`/Users/tomer/dev/henyey`).
- Artifacts (clippy logs, baselines) go in `/tmp/cleanup-*` so multiple runs are
  isolated. Per `AGENTS.md`, large/persistent artifacts should go in
  `~/data/<session-id>/` — these are small and short-lived, so `/tmp` is fine.

## Reproducibility note

Lane A output is fully deterministic (clippy → categorize). Lanes B/C/D are
LLM-judged and will vary slightly run-to-run. The adversarial verify in Stage 3
is the stabilizing layer — a finding that survives verify in one run is very
likely to survive in another. For audit / before-and-after comparisons, prefer
the Stage 4 "Confirmed findings" section over raw lane output.
