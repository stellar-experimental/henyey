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

## Categories

For each finding, classify it into exactly one category:

### Structure
 1. **LARGE MODULE** — any single .rs file over 1000 non-test lines.
    Suggest reduction via extraction, deduplication, or dead-code removal.
    Only recommend a directory module (`foo/mod.rs`) when 3+ separable concerns
    each exceed ~200 lines. Never split solely to extract tests.
 2. **GOD FUNCTION** — any function over 150 lines or with cyclomatic complexity
    that makes it hard to follow. Suggest extraction points and names.
 3. **DEEP NESTING** — blocks indented 4+ levels. Suggest early returns, guard
    clauses, or extraction to flatten.
 4. **LONG PARAMETER LIST** — functions taking 7+ parameters. Suggest grouping
    into a context/config struct.

### Redundancy
 5. **DEAD CODE** — functions, fields, methods, or branches that are never used
    or always return a fixed value. Include evidence (e.g., "no callers found").
 6. **DUPLICATION** — identical or near-identical logic repeated in 2+ places.
    Show the locations and what a shared implementation would look like.
 7. **DUPLICATE STATE** — the same truth tracked in 2+ places that must be kept
    in sync manually. Suggest which copy to remove.
 8. **SCATTERED CONCERN** — a single logical operation performed in multiple call
    sites instead of one function.
 9. **UNNECESSARY CLONING** — values cloned where a borrow or move would suffice.
10. **TRIVIAL WRAPPER** — one-liner functions that only delegate with no added
    logic. Inline and remove, unless the wrapper provides meaningful abstraction
    (e.g., a public API shielding an internal signature).

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
