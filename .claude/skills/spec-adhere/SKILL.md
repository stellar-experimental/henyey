---
name: spec-adhere
description: Audit a Rust crate's adherence to its Stellar protocol spec (spec-driven, walks every normative claim)
argument-hint: [<SPEC> | <crate-path>] [--apply]
---

Parse `$ARGUMENTS`:

- If any token matches one of
  `{SCP_SPEC, OVERLAY_SPEC, HERDER_SPEC, LEDGER_SPEC, TX_SPEC,
  BUCKETLISTDB_SPEC, CATCHUP_SPEC}` (case-insensitive, with or
  without `.md`), set `$TARGET` to the spec name. The skill resolves
  to the corresponding Rust crate via the mapping below.
- Else if any token matches a Rust crate path (e.g., `crates/scp`,
  `crates/tx`), set `$TARGET` to the crate. The skill resolves to
  the corresponding spec.
- Else `$TARGET = all`: run every (spec, crate) pair in parallel.
- If `--apply` is present, `$MODE = apply`. Otherwise `$MODE = review`.

# Spec Adherence Audit

Walk every normative claim in a Stellar protocol spec under
`stellar-specs/` and verify that the corresponding Rust
implementation in `crates/<crate>/` enforces it. **Spec-driven**:
every MUST / SHALL / MUST NOT / SHALL NOT statement, every
`INV-<X><N>` invariant, every numbered validation rule, every
protocol-version branch, and every error-code mapping is checked.
This is the inverse of `/spec-from-core` and complements
`/parity-check` (which compares Rust against C++ headers, not
against the specs). Both serve the observable / interop parity
contract defined in `docs/PARITY.md`.

## Spec ↔ Crate Mapping

| Spec | Primary Rust crate | Notes |
|------|--------------------|-------|
| `SCP_SPEC` | `crates/scp` | Driver/wiring also in `crates/app` (event loop owns SCP message dispatch & consensus trigger) |
| `OVERLAY_SPEC` | `crates/overlay` | Dispatch/wiring also in `crates/app` (event loop: `survey_impl.rs`, `tx_flooding.rs`, `peers.rs`, `lifecycle.rs`) |
| `HERDER_SPEC` | `crates/herder` | Integration/wiring also in `crates/app` (event-loop dispatch, consensus trigger) |
| `LEDGER_SPEC` | `crates/ledger` | Some integration in `crates/app` |
| `TX_SPEC` | `crates/tx` | Apply path also touches `crates/app` |
| `BUCKETLISTDB_SPEC` | `crates/bucket` | |
| `CATCHUP_SPEC` | `crates/history` | Also `crates/historywork` |

> **Cross-crate caveat (the #3158 blind spot).** `crates/app` owns the live
> overlay/herder/scp **wiring** — the event loop in `crates/app/src/app/`
> (`lifecycle.rs`, `survey_impl.rs`, `tx_flooding.rs`, `peers.rs`) is where
> message dispatch, survey processing, flood scheduling, and consensus triggers
> actually run. The spec-named library crate (`crates/overlay`, `crates/herder`,
> `crates/scp`) may contain **dead / unwired code**: a symbol can be `pub` and
> re-exported yet never instantiated outside `#[cfg(test)]` (e.g.
> `crates/overlay/src/survey.rs`'s `SurveyManager`, exported at `lib.rs` but only
> constructed in tests — the real survey logic lives in `crates/app`). Therefore:
> **never declare an OVERLAY/HERDER/SCP feature "Absent" after searching only the
> named crate.** Grep `crates/app` (and the rest of the `crates/` tree) for the
> wiring first. Conversely, do not treat an exported-but-never-instantiated
> library symbol as "the implementation" — confirm it is reachable from
> production code before classifying it Full.

Output file (apply mode):
`/Users/tomer/dev/henyey/crates/<primary-crate>/SPEC_ADHERENCE.md`

## Procedure (per spec ↔ crate pair)

### Step 1 — Enumerate normative claims from the spec

Read `/Users/tomer/dev/henyey/stellar-specs/<SPEC>.md` end-to-end.
Build a claim inventory:

1. **RFC 2119 statements**: every sentence containing ALL-CAPS
   MUST / SHALL / MUST NOT / SHALL NOT / SHOULD / SHALL NOT.
   Normative-strong (MUST / SHALL / MUST NOT / SHALL NOT) are
   required to adhere; SHOULD / SHOULD NOT are recommended.
2. **Invariants**: every `INV-<X><N>` ID and its body.
3. **Numbered validation rules**: numbered lists where each item
   is a check that produces a result code or a state mutation —
   ORDER MATTERS.
4. **Protocol-version branches**: every `@version(...)` annotation.
5. **Error-code mappings**: every reference to a SCREAMING_SNAKE
   XDR enum value (e.g., `MANAGE_SELL_OFFER_MALFORMED`) tied to a
   specific check.
6. **Constants**: every named constant in the Constants section.

Each claim gets a stable ID (use spec section + a sequence number,
e.g., `SCP §4.2.3-1` for the first claim in §4.2.3).

### Step 2 — Build a search index of the Rust crate

For the primary crate (plus any secondary crates per the mapping
table), read all `.rs` files under `src/`. Skip `tests/`,
`benches/`, and `#[cfg(test)]` modules — adherence is about
production code.

**Search ALL crates, not just the name-matched one (the #3158 fix).**
Before classifying any claim, run a grep-first pass over the **entire
`crates/` tree** for the spec-bound symbols (function names, constants,
result-code enum values, dispatch match-arms) — do **not** scope the
search to the primary crate alone. Henyey's event loop in `crates/app`
owns much overlay/herder/scp **wiring** (dispatch, survey processing,
flood scheduling, consensus trigger), so a symbol absent from the
name-matched crate is **not** absent from the codebase. Keep the
primary crate as the deep-read target; grep-target the rest of the
crates (especially `crates/app`) and read only the matching files.
Concrete command:

```bash
# Locate candidate implementations across ALL crates before reading.
grep -rni --include='*.rs' -e '<symbol>' -e '<CONST>' -e '<RESULT_CODE>' crates/
```

Build a lightweight index of:
- Public function names and their containing modules
- Result-code enum values (Rust-side names; usually
  `SCREAMING_SNAKE_CASE` or `PascalCase` per Rust convention)
- Constants and `const fn`
- Protocol-version checks: search for
  `protocol_version_starts_from`, `protocol_version_is_before`,
  `>= ProtocolVersion::`, or equivalent helpers
- Existing `// Spec:` anchors (already 16 across the workspace as
  of v26.0.1 regenerate)

### Step 3 — For each claim, locate and classify

For every claim from Step 1, attempt to locate the Rust enforcement:

1. **Anchor-first**: if any `// Spec: <SPEC> §N` comment cites the
   claim's section, follow it.
2. **Symbol search**: search the Rust index for function names,
   error codes, or key terms from the claim text.
3. **Result-code match**: if the claim ties a check to an XDR
   result code, search for that enum value in the crate.
4. **Free-text grep**: fall back to grep for distinctive phrases.
   Search **all crates** (`grep -rn --include='*.rs' … crates/`), not
   only the name-matched crate.

**Reachability check (the #3158 fix) — run at every Full/Absent
decision point.** Before recording a found symbol as **Full**, verify it
is actually **instantiated / called in the production control path**:
grep for a non-`#[cfg(test)]` caller or constructor.

```bash
# Is the candidate reachable from production code (not just tests)?
grep -rn --include='*.rs' '<symbol>' crates/ | grep -v -e '#\[cfg(test)\]' -e '/tests/'
```

If the only references are tests or re-exports — the
`crates/overlay/src/survey.rs` `SurveyManager` **dead-scaffold** case
(exported at `lib.rs` but constructed only under `#[cfg(test)]`) — treat
it as dead/unwired code and **keep searching** (the live implementation
is typically the event-loop wiring in `crates/app`) rather than reporting
it as "the implementation." Symmetrically, **never record Absent without
an all-crates grep**: confirm the symbol/result-code is missing from the
**entire `crates/` tree** (especially `crates/app`), using two search
strategies across all crates, before classifying Absent.

Classify the finding:

- **Full** — implementation is present and enforces the claim. For
  numbered validation rules, the **order** matches the spec.
- **Partial** — implementation exists but is incomplete (missing
  a version branch, missing a sub-check, wrong result code,
  reordered guard sequence).
- **Absent** — no Rust enforcement found. (For SHOULD claims this
  may be acceptable; flag as `Absent (SHOULD)` and note.)
- **Drift** — Rust does something different from the spec. Flag
  for human review — either the spec is wrong, or the Rust is
  wrong.
- **N/A** — claim is about implementation-internal behavior that
  the Rust port deliberately omits (e.g., a stellar-core thread
  invariant that doesn't apply to a single-threaded Rust port).

For each finding, record:
- Claim ID and spec section
- Brief quote of the claim
- Rust location (`path/to/file.rs:LINE`) or `not found`
- Classification
- One-line notes (especially for Partial / Drift)

### Step 4 — Detect dangling anchors

Cross-check every `// Spec: <SPEC> §N` comment in
`crates/*/src/*.rs` against the regenerated spec sections. If a
cited section no longer exists or has been renumbered, list it as
a dangling anchor for fix-up.

### Step 5 — Compute adherence summary

```
adherence_pct = Full / (Full + Partial + Absent) * 100
```

Partial and Absent count against adherence; N/A is excluded.
Drift items are surfaced separately and not counted (they require
human decision).

## Output

### `$MODE = review` (default)

Report to chat in this structure:

```
# Spec Adherence: <SPEC> ↔ crates/<crate>

**Adherence:** X% (Full N | Partial M | Absent K | Drift D | N/A J)

## Summary table (top section, ≤ 15 rows)
| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §4.2 | Quorum slice test | Full | scp/quorum.rs:142 |
| §8.5 step 7 | Upgrade stripping | Absent | not found |
| ... |

## Detailed findings (grouped by spec section)
### §4.2 — Quorum Slice Test
- **Claim SCP §4.2-1** (MUST): "A quorum slice U of node N MUST..."
- **Rust**: `crates/scp/src/quorum.rs:142` `fn is_quorum_slice(...)`
- **Status**: Full
- **Notes**: Includes the v-blocking edge case in spec.

(repeat per section that has findings; collapse Full-only sections
to a single line)

## Invariant coverage
| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-S1 (Phase monotonicity) | Full | scp/phase.rs:apply (asserts) |
| INV-S11 (Singleton qset for EXTERNALIZE) | Absent | grep returned nothing |
| ... |

## Dangling Spec anchors
- `crates/tx/src/validation.rs:553` → `TX_SPEC §4.2.3` — section
  not found in current spec (renumbered to §5.2.3?).

## Drift items (require human review)
- §6.2 step 4: spec says `nC != 0 → nH != 0 AND ballot.counter >=
  nH AND nH >= nC`. Rust implements `nH > nC` (strict). Likely
  the Rust is wrong; verify against stellar-core.

## Recommendations
1. Add enforcement for INV-S11 (correctness-critical).
2. Fix dangling anchors.
3. Investigate drift items.
```

Do not write any files in review mode.

### `$MODE = apply`

Write the report to
`/Users/tomer/dev/henyey/crates/<primary-crate>/SPEC_ADHERENCE.md`
using the same format. Bump (or set) a `**Last Updated:**` line.

If `SPEC_ADHERENCE.md` already exists, preserve any
**Human-Verified** annotations (a future convention — items the
reviewer marked as "verified Full", "verified Drift", or
"deliberately Absent"). Reformat everything else.

If the parity column in the main `README.md` Crate Overview also
tracks a "Spec Adherence" percentage in the future, update it.
(Today the column tracks `/parity-check` percentage; do not change
it.)

## Scope

- `$TARGET = <SPEC name>`: run the spec ↔ crate pair for that spec.
- `$TARGET = <crate path>`: resolve to the spec mapped to that crate.
- `$TARGET = all`: run all 7 spec ↔ crate pairs. Recommend
  parallel subagents (one per pair) since each is independent.
  Dispatch each with an explicit `model="sonnet"` on the Agent
  call — spec ↔ crate cross-referencing is a mechanical
  read-and-classify task that does not need a top-tier model.

## Reading strategy

- Read the spec end-to-end (cannot shortcut — every section may
  carry normative claims).
- Read every production `.rs` file in the primary crate; skim
  secondary crates if mapped.
- Skip Rust test files (`tests/`, `#[cfg(test)]`).
- For very large specs (TX_SPEC at 2385 lines, OVERLAY at 2037),
  budget tool uses carefully. Use `grep` to locate candidate
  implementations before reading full files.
- Source-level `// Spec:` anchors are accelerators — start there
  to seed the search.

## Guardrails

- Do NOT edit any source code in `crates/`. This skill audits, it
  does not fix.
- Do NOT edit the spec files. Drift findings go in the report;
  fixing them is a separate decision.
- Be precise with classifications. "I couldn't find it in a quick
  grep" is **not** Absent — confirm with at least two search
  strategies (anchor + symbol, or symbol + result-code) **across all
  crates** (grep the entire `crates/` tree, especially `crates/app`,
  not just the name-matched crate) before marking Absent. This is the
  #3158 cross-crate fix: overlay/herder/scp wiring lives in `crates/app`.
- For SHOULD claims, default to noting them in the report but
  excluding from the adherence percentage calculation. Many
  SHOULDs are operational defaults (e.g., recommended timeouts),
  not implementation requirements.
- Implementation-internal differences (memory management,
  threading, error-wrapping style) are N/A — they're outside the
  spec's normative scope.

## Relationship to other skills

- `/spec-from-core` — generates / updates specs from C++. Run when
  stellar-core changes or canon evolves.
- `/parity-check` — structural parity (Rust API vs C++ headers).
  Use when you want a function-by-function inventory.
- `/spec-adhere` — behavioral parity (Rust vs Spec). Use when you
  want to verify implementation correctness against the contract.

The three skills form a triangle: **C++ ↔ Spec ↔ Rust ↔ C++**. Each
edge is checked by one skill.
