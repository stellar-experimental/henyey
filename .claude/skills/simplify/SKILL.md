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
  file:line references. Do NOT make any changes.
- **`$MODE = apply`**: Perform the simplifications directly. For each change,
  briefly state what you changed and why. Run `cargo clippy -p <crate>` and
  `cargo test -p <crate>` after each logical group of changes to verify
  correctness.

## Categories

For each finding, classify it into exactly one category:

### Structure
 1. **LARGE MODULE** — any single .rs file over 500 lines (excluding tests).
    Suggest how to reduce size via extraction of helpers, deduplication, or
    dead-code removal. Only recommend converting to a directory module
    (`foo/mod.rs`) when the *implementation* has 3+ clearly separable concerns
    that each exceed ~200 lines. Never split a file solely to extract tests.
 2. **GOD FUNCTION** — any function over 80 lines or with cyclomatic complexity
    that makes it hard to follow. Suggest extraction points and names for the
    extracted functions.
 3. **DEEP NESTING** — blocks indented 4+ levels. Suggest early returns, guard
    clauses, or extraction to flatten them.
 4. **LONG PARAMETER LIST** — functions taking 5+ parameters. Suggest grouping
    into a context/config struct.

### Redundancy
 5. **DEAD CODE** — functions, fields, methods, or branches that are never used
    or always return a fixed value. Include evidence (e.g., "no callers found").
 6. **DUPLICATION** — identical or near-identical logic repeated in multiple places.
    Show the locations and what a single shared implementation would look like.
 7. **DUPLICATE STATE** — the same truth tracked in two or more places that must
    be kept in sync manually. Suggest which copy to remove.
 8. **SCATTERED CONCERN** — a single logical operation (e.g., resetting tracking
    state) performed in multiple call sites instead of one function.
 9. **UNNECESSARY CLONING** — values cloned where a borrow or move would suffice.
 9b. **TRIVIAL WRAPPER** — one-liner functions that only delegate to another
    function with no added logic (e.g., `fn foo(x: T) { bar(x) }`). Inline the
    call at the call site and remove the wrapper, unless it provides meaningful
    abstraction (e.g., a public API shielding an internal signature).

### Naming & Constants
10. **MISLEADING NAMES** — identifiers whose name does not match their actual
    semantics. Suggest a better name.
11. **MAGIC NUMBERS** — hardcoded numeric or string literals that should be
    named constants.

### Visibility
12. **OVERLY BROAD VISIBILITY** — functions, methods, or types marked `pub` that
    are only used within the current module or crate. Suggest narrowing to
    `pub(crate)`, `pub(super)`, or private. Every `pub` item should be
    justifiably public — if no external crate uses it, restrict its scope.

### Clippy & Types
13. **CLIPPY SUPPRESSIONS** — any `#[allow(clippy::...)]` or `#[allow(dead_code)]`.
    For each, determine whether the underlying issue can be fixed so the
    suppression can be removed. If the suppression is genuinely necessary
    (e.g., false positive, upstream requirement), note why.
14. **TYPE COMPLEXITY** — types that are hard to read at a glance, especially
    those marked `#[allow(clippy::type_complexity)]`. Suggest type aliases,
    wrapper structs, or simplified signatures.

### Documentation
15. **STALE COMMENTS** — comments that no longer match the code they describe,
    or that reference removed/renamed items. Fix or remove.
16. **COMMENTED-OUT CODE** — dead code left as comments instead of being deleted.
    Remove it (git preserves history).
17. **TODO/FIXME/HACK** — unresolved markers. For each: still relevant? If yes,
    describe the fix. If no, remove the marker.
18. **MISSING MODULE DOC** — .rs files over 100 lines with no top-level `//!` doc
    comment explaining the module's purpose. Suggest a one-line summary.

## Ranking

Rank findings by impact: how much each fix would reduce complexity, improve
readability, or prevent bugs. High-impact first.

## Conventions

- **Inline tests**: Unit tests belong in `#[cfg(test)] mod tests { }` at the
  bottom of the source file. Do not extract tests into separate files or
  directory modules.

## Scope

Ignore test code and `stellar-core/`.

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
