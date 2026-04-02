---
name: audit-deferred-init
description: Detect deferred initialization with silent fallbacks in a crate or the full workspace
argument-hint: <crate-path|--all> [--apply]
---

Parse `$ARGUMENTS`:
- If the first argument is `--all`, set `$TARGET = crates/` (full workspace).
  Otherwise, the first argument is the crate path. Replace `$TARGET` with it.
- If `--apply` is present, set `$MODE = apply`. Otherwise set `$MODE = review`.

# Audit: Deferred Initialization with Silent Fallbacks

Scan Rust code at `$TARGET` for the "deferred initialization with silent
fallback" anti-pattern: subsystem dependencies stored as `Option<T>` that are
`None` at construction time and populated later via a setter, where code that
encounters `None` silently degrades instead of failing loudly.

This pattern is dangerous because:
- It creates two code paths (real subsystem vs fallback) that must stay in sync.
- The fallback path is rarely tested under realistic conditions.
- Silent degradation masks bugs: the system appears to work but produces
  subtly wrong results (skipped validation, default values, no-ops).
- If the setter is never wired up, the fallback becomes the permanent
  production behavior — a silent parity gap.

## Mode

- **`$MODE = review`** (default): Produce a structured report with findings
  ranked by risk. Do NOT make any changes.
- **`$MODE = apply`**: After producing the review, fix findings in priority
  order. Commit each logical change separately.

## What to Look For

### Pattern 1: Option-wrapped subsystem fields

Fields of these shapes on structs that represent long-lived components (not
short-lived builders or iterators):

```rust
field: Option<Arc<T>>
field: RwLock<Option<Arc<T>>>
field: Mutex<Option<T>>
field: Option<Box<dyn Trait>>
```

Where `T` is a subsystem, manager, provider, or trait object — not a cache
entry, runtime state flag, or feature toggle.

**How to distinguish subsystems from legitimate Option uses:**
- **Subsystem** (flag): the `None` case triggers fallback logic that
  approximates what the real implementation would do. There is a `set_*` or
  `init_*` method to populate it later.
- **Cache/state** (OK): `None` means "not yet computed" or "not applicable."
  The code's behavior when `None` is the intended behavior, not a fallback.
- **Feature toggle** (OK): `None` means the feature is disabled by
  configuration. The `None` path is intentionally different from the `Some`
  path and does not attempt to approximate it.

Only flag subsystem patterns.

### Pattern 2: Deferred setter methods

Methods matching `set_*`, `init_*`, or `register_*` that populate an
`Option` field after construction:

```rust
pub fn set_ledger_manager(&self, manager: Arc<LedgerManager>) {
    *self.ledger_manager.write() = Some(manager);
}
```

For each setter found, determine:
- Is it called from production code? (Search for call sites outside `#[cfg(test)]`)
- When is it called relative to the field being read? (Before any reads? After
  some reads could have already occurred?)
- Could it fail or be skipped? (Behind a conditional, error path, or feature flag?)

### Pattern 3: Silent fallback in the None branch

Code that encounters `None` and silently degrades:

```rust
// Silent return — skips the entire check
if let Some(ref lm) = *self.ledger_manager.read() {
    // ... real validation ...
} else {
    return true;  // SILENT FALLBACK: accepts without checking
}

// Silent default — substitutes a dummy value
let version = self.manager.read()
    .as_ref()
    .map(|m| m.version())
    .unwrap_or(0);  // SILENT FALLBACK: uses dummy value

// Silent no-op — drops the operation
let Some(overlay) = self.overlay() else {
    return;  // SILENT FALLBACK: operation silently not performed
};
```

**What counts as a silent fallback:**
- `return true` / `return false` / `return Ok(())` — skipping validation
- `return DEFAULT_VALUE` — substituting a dummy
- `return` / `return Vec::new()` / `return None` — dropping an operation
- `unwrap_or(DUMMY)` / `unwrap_or_default()` — using a sentinel value
- Empty else branch or missing else — silently ignoring the None case

**What does NOT count:**
- `panic!` / `unreachable!` / `.expect("message")` — fails loudly
- `return Err(...)` — propagates the error
- `log::warn!(...); return` — at least alerts, though still a finding if the
  fallback changes behavior
- Early return during startup before the subsystem could be used — legitimate
  if the window is provably safe

### Pattern 4: Never-called setters (UNWIRED)

The most severe variant: a `set_*` method exists but is never called from
production code. The `Option` field is permanently `None`, and the fallback
path is the permanent production behavior.

Search for each `set_*` method's call sites. Exclude:
- `#[cfg(test)]` modules
- Test files in `tests/` directories
- Doc comments and doc tests

If zero production call sites exist, classify as **UNWIRED**.

## Analysis Process

### Step 1: Collect Candidates

Use subagents (Task tool with `explore` type) to search `$TARGET` for:

1. Struct fields matching `Option<Arc<`, `Option<Box<dyn`, `RwLock<Option<`,
   `Mutex<Option<` — excluding test modules.
2. Methods matching `pub fn set_` or `pub fn init_` or `pub fn register_` that
   write to an `Option` field.
3. For each candidate field, all read sites (patterns: `if let Some`,
   `.as_ref()`, `.is_some()`, `.is_none()`, `.unwrap_or`, `.map_or`).

### Step 2: Classify Each Finding

For each `Option`-wrapped field, determine:

- **UNWIRED**: Setter exists but is never called from production code. The
  field is permanently `None`. This is the highest risk — the fallback IS the
  production behavior.
- **DEFERRED-RISKY**: Setter is called from production code, but there is a
  window where reads can occur before the setter runs (e.g., setter is called
  from an async task, or after a method that already reads the field).
- **DEFERRED-SAFE**: Setter is called synchronously during initialization,
  before any code path reads the field. The `None` fallback is only reachable
  in tests.
- **LEGITIMATE**: The `Option` represents genuine optionality (cache, state,
  feature toggle), not a deferred subsystem dependency.

Only report UNWIRED, DEFERRED-RISKY, and DEFERRED-SAFE findings. Skip
LEGITIMATE uses.

### Step 3: Assess Risk for Each Finding

For each non-LEGITIMATE finding, document:

1. **Field**: struct name, field name, type, file:line
2. **Setter**: method name, file:line, production call sites (with file:line),
   or "NONE" if unwired
3. **Fallback sites**: For each place the `None` branch is taken, list:
   - file:line
   - What the fallback does (return value, side effect, or no-op)
   - What the real subsystem would do instead
   - Whether the difference is observable in production
4. **Classification**: UNWIRED / DEFERRED-RISKY / DEFERRED-SAFE
5. **Production impact**: What goes wrong if the fallback runs in production?
   "None — test only" is a valid answer for DEFERRED-SAFE.

### Step 4: Rank by Risk

Order findings:
1. UNWIRED (always running the fallback in production)
2. DEFERRED-RISKY (could run the fallback in production under certain conditions)
3. DEFERRED-SAFE with dangerous fallbacks (test-only, but fallback silently
   accepts/skips validation — masks test quality issues)
4. DEFERRED-SAFE with benign fallbacks (test-only, fallback is a reasonable default)

## Output Format (review mode)

```
# Deferred Initialization Audit: $TARGET

## Summary

- **Scanned**: N files, M structs
- **Findings**: X total (A UNWIRED, B DEFERRED-RISKY, C DEFERRED-SAFE)

## Findings

### [RANK]. [CLASSIFICATION] — field_name on StructName

- **Field**: `struct_name.field_name: Type` at file:line
- **Setter**: `set_method()` at file:line
  - Production call sites: file:line, file:line (or NONE)
- **Fallback sites**:
  | # | Location | Fallback behavior | Real behavior | Observable? |
  |---|----------|-------------------|---------------|-------------|
- **Production impact**: description
- **Recommendation**: what to do about it

(Repeat for each finding)

## Recommendations

Prioritized list of actions to eliminate the highest-risk patterns.
```

## Apply Mode

When `$MODE = apply`, first produce the full review report above. Then fix
findings in priority order (UNWIRED first, then DEFERRED-RISKY, then
DEFERRED-SAFE).

### Fix Strategies (in order of preference)

**Strategy 1: Require at construction time.** If the dependency is always
available when the struct is created in production, change the field from
`Option<T>` to `T` and require it as a constructor parameter. This eliminates
the `Option` entirely and makes all fallback paths dead code that can be
removed.

Example: `scp: Option<SCP>` became `scp: SCP` in the herder refactor.

Use this when:
- The dependency is always set immediately after construction.
- Changing the constructor signature is feasible (not too many call sites).
- Tests can be updated to provide the dependency (possibly via a test helper).

**Strategy 2: Replace fallback with panic.** If the field must remain `Option`
(e.g., circular dependency prevents passing it at construction), replace silent
fallbacks with `.expect("message")` or explicit panics. The message should
explain when the field is expected to be set.

Example:
```rust
// Before: silent fallback
let version = self.manager.read().as_ref().map(|m| m.version()).unwrap_or(0);

// After: loud failure
let version = self.manager.read().as_ref()
    .expect("ledger_manager must be set via set_ledger_manager() before consensus starts")
    .version();
```

Use this when:
- The dependency cannot be provided at construction time.
- The `None` case should never occur in production.
- A panic is preferable to silent incorrect behavior.

**Strategy 3: Wire up the missing setter.** For UNWIRED findings where the
infrastructure exists but was never connected, implement the production wiring.

Use this when:
- The trait/interface exists.
- A production implementation exists or is straightforward to write.
- The setter call site is obvious (e.g., alongside other `set_*` calls in
  `App::new()`).

If the production implementation requires significant new code, file a GitHub
issue instead of implementing it inline.

**Strategy 4: Make the Option explicit in the API.** If `None` is a legitimate
runtime state (not just deferred initialization), make callers handle it
explicitly by returning `Result` or `Option` instead of silently degrading.

### Apply Mode Rules

- Work through findings in rank order (highest risk first).
- Make one logical change at a time — do not batch unrelated changes.
- After each change, run `cargo clippy -p <crate>` and `cargo test -p <crate>`.
- If a change breaks tests, fix the tests (they were relying on the silent
  fallback and need to provide the dependency).
- If a change would require significant new infrastructure (new trait
  implementations, new production wiring), file a GitHub issue and move on.
- Each commit must include `Co-authored-by` trailers per AGENTS.md.
- Commit messages: "Require <dependency> at construction time for <Struct>" or
  "Replace silent fallback with panic for <Struct>.<field>" or
  "Wire up <setter> in production for <Struct>".

## Guidelines

- Be precise. Cite `file:line` for every claim.
- Do not flag legitimate `Option` uses (caches, state, feature toggles).
- For each finding, read enough surrounding code to confirm the classification.
  Do not guess — if you cannot determine whether a setter is called in
  production, search the full codebase.
- Use subagents (Task tool with `explore` type) for codebase-wide searches.
- Focus on behavioral impact. An `Option` field that is always `Some` in
  production and whose `None` fallback is only exercised in tests is low risk
  — but still worth noting if the fallback silently skips validation (it
  means the tests are not exercising the real validation path).
- Do not inflate findings. If no deferred-init patterns exist, say so.
