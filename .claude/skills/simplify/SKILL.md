---
name: simplify
description: Review or apply code simplifications to a crate
argument-hint: <crate-path> [--apply]
---

Parse `$ARGUMENTS`:
- The first argument is the crate path. Replace `$TARGET` with it.
- If `--apply` is present, set `$MODE = apply`. Otherwise set `$MODE = review`.

# Code Simplification

Review the Rust crate at `$TARGET` and identify concrete simplifications.

## Mode

- **`$MODE = review`** (default): Produce a ranked list of findings with
  file:line references. Do NOT make any changes. Cap at **15 findings** per
  crate — if you find more, keep only the highest-impact ones.
- **`$MODE = apply`**: Perform the simplifications directly. For each change,
  briefly state what you changed and why. Run `cargo clippy -p <crate>` and
  `cargo test -p <crate>` after each logical group of changes to verify
  correctness. Stop after **10 changes** or when remaining findings are
  low-impact.

## Parity Filter

This codebase mirrors stellar-core for determinism. Before reporting any
finding, check whether the code structurally mirrors a stellar-core counterpart
by looking in `stellar-core/src/`. **Suppress the finding** if refactoring
would make it harder to verify parity. Signs of parity-driven structure:

- The file/function name matches a stellar-core `.cpp`/`.h` file or function.
- The control flow (match arms, if-else chains) follows stellar-core's ordering.
- Constants, parameter lists, or duplicated logic mirrors stellar-core's own
  structure (including stellar-core's own duplication).

This filter applies most often to: LARGE MODULE, GOD FUNCTION, DEEP NESTING,
LONG PARAMETER LIST, DUPLICATION, and MAGIC NUMBERS.

### Strengthened parity rules

These specific patterns should **always be suppressed**:

- **1:1 module mapping**: If a `.rs` file maps directly to a stellar-core `.cpp`
  file (e.g., `bucket_list.rs` ↔ `BucketList.cpp`), do NOT flag it as LARGE
  MODULE. The module's size reflects the upstream structure.
- **Orchestration methods**: Methods on structs that mirror stellar-core classes
  (e.g., `App`, `LedgerManager`, `Herder`) that coordinate multiple subsystems
  are exempt from GOD FUNCTION. These methods require broad field access; extraction
  would introduce parameter bloat or artificial state structs.
- **Protocol-version functions**: Functions with version suffixes (`_p24`, `_p25`,
  `_p26`) are exempt from GOD FUNCTION and DUPLICATION. Each version has
  intentional subtle differences that are safer to keep explicit than to abstract.

**Exception**: The Idiom categories (C-STYLE MUTATION, C-STYLE OUTPUT PARAMETER,
C-STYLE OPTION HANDLING) are exempt — always report them regardless of parity.

## Categories

For each finding, classify it into exactly one category:

### Structure
 1. **LARGE MODULE** — any single .rs file over 1000 non-test lines.
    Suggest reduction via extraction, deduplication, or dead-code removal.
    Only recommend a directory module (`foo/mod.rs`) when 3+ separable concerns
    each exceed ~200 lines. Never split solely to extract tests.
    **Suppress if**: the module is a cohesive unit — well-named functions with
    clear internal structure but no obvious separation boundary. Being large
    alone is not a finding; the module must have identifiable separable concerns.
 2. **GOD FUNCTION** — any function over 200 lines or with cyclomatic complexity
    that makes it hard to follow. Suggest extraction points and names.
    **Suppress if**: the function is a sequential pipeline (≤2 nesting levels)
    with clearly labeled phases (setup → process → cleanup), or a flat mapping
    function (e.g., config translation). High line count with low nesting and
    linear flow is not complexity.
 3. **DEEP NESTING** — blocks indented 4+ levels. Suggest early returns, guard
    clauses, or extraction to flatten.
 4. **LONG PARAMETER LIST** — functions taking 7+ parameters. Suggest grouping
    into a context/config struct. **Reinforce parity check**: if the parameter
    list mirrors the corresponding stellar-core function, suppress.

### Redundancy
 5. **DEAD CODE** — functions, fields, methods, or branches that are never used
    or always return a fixed value. Include evidence (e.g., "no callers found").
 6. **DUPLICATION** — identical or near-identical logic repeated in 2+ places.
    Show the locations and what a shared implementation would look like.
 7. **DUPLICATE STATE** — the same truth tracked in 2+ places that must be kept
    in sync manually. Suggest which copy to remove.
 8. **SCATTERED CONCERN** — a single logical operation performed in multiple call
    sites instead of one function. **Only report** when consolidation would be a
    net improvement (fewer total lines, clearer intent). Do not report when the
    duplication is incidental or the call sites have meaningful differences.
 9. **UNNECESSARY CLONING** — values cloned where a borrow or move would suffice.
    **Skip** when the cloned type is small (≤64 bytes), implements or could
    implement `Copy`, or the clone is in a cold path — the performance cost is
    negligible and removing it adds no clarity.
10. **TRIVIAL WRAPPER** — one-liner functions that only delegate with no added
    logic. Inline and remove, unless the wrapper provides meaningful abstraction
    (e.g., a public API shielding an internal signature, semantic naming that
    improves call-site readability, or API stability for a frequently-changing
    internal function).

### Naming & Constants
11. **MISLEADING NAMES** — identifiers whose name does not match their actual
    semantics. Suggest a better name.
12. **MAGIC NUMBERS** — hardcoded numeric or string literals that should be
    named constants. Skip constants that mirror stellar-core values or are
    defined by the XDR specification.

### Visibility
13. **OVERLY BROAD VISIBILITY** — `pub` items only used within the current
    module or crate. Suggest narrowing to `pub(crate)`, `pub(super)`, or
    private.

### Clippy
14. **CLIPPY SUPPRESSIONS** — `#[allow(clippy::...)]` or `#[allow(dead_code)]`
    where the underlying issue can be fixed. Do not report suppressions that
    are genuinely necessary (false positive, upstream requirement, or parity
    with stellar-core). In particular, `#[allow(clippy::too_many_arguments)]`
    on parity functions is expected — skip these.

### Documentation
15. **STALE COMMENTS** — comments that no longer match the code. Fix or remove.
16. **COMMENTED-OUT CODE** — dead code left as comments. Remove (git preserves
    history).

### Idiom (exempt from Parity Filter)

These categories target C/C++ idioms carried over from stellar-core. They are
**exempt from the Parity Filter** — always report them, even when the code
structurally mirrors stellar-core. The goal is to make the Rust codebase
idiomatically Rust, not a transliteration of C++.

17. **C-STYLE MUTATION** — functions that take `&mut T` and return `bool` for
    success/failure. The Rust idiom is `Result<(), E>` (or `Option<T>` when
    there is no meaningful error context). Callers benefit from `?` propagation
    instead of `if !func(...) { return error; }` chains.
    *Example*: `add_account_balance(account: &mut AccountEntry, delta: i64) -> bool`
    → `fn add_account_balance(account: &mut AccountEntry, delta: i64) -> Result<(), BalanceError>`.
    **Skip**: In-place slice sorting (`sort_by`, `sort_unstable_by`) and mutable
    slice operations are idiomatic Rust, not C-isms.
18. **C-STYLE OUTPUT PARAMETER** — functions that take `&mut Vec<T>` (or similar
    containers) as output parameters instead of returning a collection. The Rust
    idiom is to return the collected value, letting the caller decide how to
    combine it.
    *Example*: `collect_bucket_hashes(out: &mut Vec<Hash256>)`
    → `fn collect_bucket_hashes() -> Vec<Hash256>`.
19. **C-STYLE OPTION HANDLING** — manual `is_none()` / `is_some()` checks
    followed by `.unwrap()` instead of pattern matching (`let Some(x) = … else`),
    `match`, or combinators (`.map()`, `.and_then()`, `.ok_or()`). The
    check-then-unwrap pattern risks panic if a future edit separates the check
    from the unwrap.

## Ranking

Rank findings by impact: how much each fix would reduce complexity, improve
readability, or prevent bugs. High-impact first.

## Conventions

- **Inline tests**: Unit tests belong in `#[cfg(test)] mod tests { }` at the
  bottom of the source file. Do not extract tests into separate files.

## Scope

- Do not flag issues **within** test code (`#[cfg(test)]`) or in `stellar-core/`.
- Test code **may** be referenced as evidence (e.g., to show a function is only
  called from tests when evaluating dead code or visibility).

## Output Format (review mode only)

Per finding:

```
### [RANK]. [CATEGORY] — one-line summary
- **Location**: file:line (and file:line if duplicated)
- **Evidence**: why this qualifies
- **Suggestion**: concrete fix (keep it brief)
```

## Apply Mode Guidelines

When `$MODE = apply`:
- Work through findings in rank order (highest impact first).
- Make one logical change at a time — do not batch unrelated refactors.
- After each change, verify with `cargo clippy -p <crate>` and `cargo test -p <crate>`.
- If a change breaks tests or introduces warnings, revert it and move on.
- Stop and report if a change would alter observable behavior.
