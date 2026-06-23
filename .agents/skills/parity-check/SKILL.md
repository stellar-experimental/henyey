---
name: parity-check
description: Analyze or update a crate's PARITY_STATUS.md for stellar-core parity
argument-hint: <crate-path> [--apply]
---

Parse `$ARGUMENTS`:
- The first argument is the crate path. Replace `$TARGET` with it.
- If `--apply` is present, set `$MODE = apply`. Otherwise set `$MODE = review`.

# Parity Check

Analyze the Rust crate at `$TARGET` for stellar-core parity and produce a
standardized `PARITY_STATUS.md`.

## Mode

- **`$MODE = review`** (default): Report findings to the conversation. Do NOT
  write or modify any files.
- **`$MODE = apply`**: Write or update `$TARGET/PARITY_STATUS.md` using the
  standardized format below. Also update the parity column in the main
  `README.md` Crate Overview table if the computed percentage changed.

## Crate-to-Upstream Mapping

Use this table to find the upstream stellar-core directory for each crate.

| Crate | Upstream Directory |
|-------|--------------------|
| `crates/tx` | `stellar-core/src/transactions/` |
| `crates/scp` | `stellar-core/src/scp/` |
| `crates/db` | `stellar-core/src/database/` |
| `crates/common` | `stellar-core/src/util/` |
| `crates/crypto` | `stellar-core/src/crypto/` |
| `crates/ledger` | `stellar-core/src/ledger/` |
| `crates/bucket` | `stellar-core/src/bucket/` |
| `crates/herder` | `stellar-core/src/herder/` |
| `crates/overlay` | `stellar-core/src/overlay/` |
| `crates/history` | `stellar-core/src/history/` |
| `crates/historywork` | `stellar-core/src/historywork/` |
| `crates/work` | `stellar-core/src/work/` |
| `crates/app` | `stellar-core/src/main/` |
| `crates/henyey` | `stellar-core/src/main/` (CLI subset) |

Some crates may reference additional upstream directories (e.g., `crates/db`
also covers SQL operations in `src/overlay/PeerManager.*`,
`src/herder/HerderPersistence*.*`, etc.). Note these in the File Mapping
section.

## Analysis Process

Follow these six steps in order.

### Step 1: Identify Upstream Directory

Look up the crate in the mapping table above. If the crate spans multiple
upstream directories, note all of them.

### Step 2: Read Upstream Headers

Read `.h` files in the upstream directory first. These define the API surface:
classes, public methods, enums, and constants. This gives you the complete
list of functionality to check against.

### Step 3: Read Rust Source

Read all `.rs` files in `$TARGET/src/`. For each upstream class/function
identified in Step 2, determine whether there is a Rust equivalent.
Classify each as:

- **Full** — functionally equivalent implementation exists
- **Partial** — some behavior implemented but incomplete
- **None** — not implemented

### Step 4: Read Upstream Tests

Scan upstream test files (`*Tests.cpp`, `*Test.cpp`) to understand test
coverage expectations. Count `TEST_CASE` and `SECTION` macros for each
test file.

### Step 5: Compute Parity

Count the functions/components from Step 3:
- `implemented` = items marked Full
- `gaps` = items marked None or Partial that are NOT intentional omissions
- `omissions` = items deliberately excluded with documented rationale

Parity % = `implemented / (implemented + gaps)` (omissions excluded from
both sides).

### Step 6: Assess Test Coverage

Compare Rust `#[test]` functions against upstream test coverage from Step 4.
Identify areas where the Rust crate has significantly fewer tests than upstream.

## Reading Strategy

- Read `.h` files first — they define the API surface concisely.
- Read Rust source second — match against the upstream API.
- Read `.cpp` files only when the header is ambiguous or you need to
  understand specific behavior to classify parity accurately.
- Do NOT read `stellar-core/` test files exhaustively. Scan for `TEST_CASE`
  and `SECTION` counts to characterize coverage.

## Standardized Format

The output `PARITY_STATUS.md` MUST use this structure. All sections are
required unless marked optional.

```markdown
# stellar-core Parity Status

**Crate**: `henyey-<name>`
**Upstream**: `stellar-core/src/<dir>/`
**Overall Parity**: N%
**Last Updated**: YYYY-MM-DD

## Summary

| Area | Status | Notes |
|------|--------|-------|
| ... | Full / Partial / None | ... |

(5-15 rows covering major functional areas)

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Foo.h` / `Foo.cpp` | `foo.rs` | ... |

## Component Mapping

### <module_name> (`module.rs`)

Corresponds to: `UpstreamFile.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `functionName()` | `function_name()` | Full |

(Repeat subsection per module)

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `FooBar` | Not needed because ... |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `BazQux` | High / Medium / Low | ... |

(If no gaps, write "No known gaps.")

## Architectural Differences

1. **<Topic>**
   - **stellar-core**: ...
   - **Rust**: ...
   - **Rationale**: ...

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| ... | N TEST_CASE / M SECTION | K #[test] | ... |

### Test Gaps

(List specific upstream test areas with limited Rust equivalents)

## Verification Results

*(Optional — include only when concrete verification data exists)*

Evidence of parity: testnet verification results, hash matches,
transaction replay statistics, etc.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | N |
| Gaps (None + Partial) | M |
| Intentional Omissions | O |
| **Parity** | **N / (N + M) = X%** |
```

## Section Guidelines

### Header
- Use the computed parity %, not a vibes-based estimate.
- Set "Last Updated" to today's date.

### Summary
- 5-15 rows. Each row is a major functional area, not individual functions.
- Status is one of: `Full`, `Partial`, `None`.
- Keep notes brief (< 15 words).

### File Mapping
- One row per upstream `.h` file (or `.h`/`.cpp` pair).
- If a single Rust module covers multiple upstream files, list each upstream
  file on its own row pointing to the same Rust module.
- Omit upstream files that are intentionally excluded (list those in
  Intentional Omissions instead).

### Component Mapping
- Group by Rust module. One subsection per module.
- Include the "Corresponds to" line linking to the upstream header.
- List every public function/method from the upstream header.
- For large modules (20+ functions), group related functions under
  sub-headings within the subsection.

### Intentional Omissions
- Must include a concrete rationale for each omission.
- Common rationales: "SQLite only", "sequential execution only",
  "handled by <other crate>", "deprecated in protocol 23+".

### Gaps
- Priority levels: High (blocks correctness), Medium (affects completeness),
  Low (nice to have).
- If there are no gaps, say so explicitly.

### Architectural Differences
- Numbered list. Each item has stellar-core approach, Rust approach, and
  rationale.
- Focus on differences that affect how someone reads or maintains the code.
- Do not list trivial language differences (e.g., "C++ uses classes, Rust
  uses structs").

### Test Coverage
- Compare at the module/file level, not individual test functions.
- Use `TEST_CASE` / `SECTION` counts for upstream, `#[test]` counts for Rust.
- Test Gaps subsection lists areas where Rust coverage is notably thinner.

### Verification Results
- Only include if concrete evidence exists (testnet runs, hash comparisons).
- Include dates and specific metrics.
- If no verification has been done, omit this section entirely.

### Parity Calculation
- Show the arithmetic explicitly.
- The counts must match what's in the Component Mapping tables.

## Review Mode Output (`$MODE = review`)

Present findings as a structured report in the conversation:

```
## Parity Review: henyey-<name>

**Computed Parity**: N% (X implemented, Y gaps, Z omissions)

### Key Findings
- ...

### Summary Table
| Area | Status | Notes |
|------|--------|-------|

### Gaps
- ...

### Recommendations
- ...
```

Do not write any files in review mode.

## Apply Mode Guidelines (`$MODE = apply`)

When `$MODE = apply`:

1. Write `$TARGET/PARITY_STATUS.md` using the standardized format above.
2. If a `PARITY_STATUS.md` already exists, preserve any **Verification
   Results** data (testnet runs, hash matches, resolved issues). Reformat
   everything else to match the standard.
3. Update the parity column in `README.md` (lines ~192-227) if the computed
   percentage differs from what's currently shown. Use the exact computed
   percentage (not a tilde approximation).
4. Do not modify any source code files.
