---
name: document
description: Create or update a crate's README documentation
argument-hint: <crate-path>
---

Parse `$ARGUMENTS`:
- The first argument is the crate path. Replace `$TARGET` with it.

# Crate Documentation

Analyze the Rust crate at `$TARGET` and create or update its `README.md`.

If a README already exists, preserve any manually written content that is
still accurate and fill in missing sections. If no README exists, create
one from scratch.

## README Structure

Follow this section order. Every section is required unless marked optional.

### 1. Title and Summary

```markdown
# henyey-<crate>

One-line description of the crate's purpose.
```

### 2. Overview

One paragraph explaining what the crate does and where it sits relative to
other henyey crates. Mention the key upstream stellar-core component it
corresponds to if applicable.

### 3. Architecture

A single diagram showing the crate's internal structure or its relationship
to adjacent crates. Pick the ONE type that best fits:

- **STATE MACHINE** — for crates with explicit state enums or modal behavior.
- **DATA FLOW** — for crates that transform or route data.
- **MODULE DEPENDENCY** — for crates with 5+ internal modules.
- **SEQUENCE** — for crates with complex multi-step protocols.

Use Mermaid syntax so diagrams render on GitHub. Keep diagrams focused —
max 15 nodes.

### 4. Key Types

A table of the crate's important public types:

```markdown
| Type | Description |
|------|-------------|
| `Foo` | ... |
```

Aim for 5-15 rows. Include traits, important enums, and config structs.
Omit internal helpers.

### 5. Usage

2-3 short code examples showing the main API patterns. Cover the most
common use cases: construction, the primary operation, and one edge case
or secondary pattern (e.g., catchup mode, error handling).

Use `rust` fenced code blocks. Examples should compile conceptually but
do not need to be runnable standalone.

### 6. Module Layout

A table mapping each source file to its purpose:

```markdown
| Module | Description |
|--------|-------------|
| `lib.rs` | ... |
| `foo.rs` | ... |
```

### 7. Design Notes *(optional)*

Include only if the crate has non-obvious design decisions that a new
contributor would find surprising. Examples: savepoint semantics, change
coalescing rules, determinism constraints, thread-safety approach.

Skip this section entirely if there is nothing surprising — do not pad
with obvious statements.

### 8. stellar-core Mapping

A table mapping Rust modules to upstream stellar-core C++ files:

```markdown
| Rust | stellar-core |
|------|--------------|
| `foo.rs` | `src/bar/Foo.cpp` |
```

This is critical for parity work. If the crate has no upstream equivalent,
say so and omit the table.

### 9. Parity Status

```markdown
## Parity Status

See [PARITY_STATUS.md](PARITY_STATUS.md) for detailed stellar-core parity analysis.
```

Only include if `$TARGET/PARITY_STATUS.md` exists.

## Guidelines

- Read all source files before writing. Understand the crate before
  documenting it.
- Be precise about types and APIs — verify names against the actual code.
- Do not document test code or `stellar-core/`.
- When updating an existing README, diff your changes mentally against
  what's there. Preserve accurate content, fix stale content, add missing
  sections.
- Keep the total README under 250 lines for small crates (<5 modules) and
  under 600 lines for large ones.
