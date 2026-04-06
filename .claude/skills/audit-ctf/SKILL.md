---
name: audit-ctf
description: Run a CTF-style security audit on a crate or file, filing GitHub issues for confirmed findings
argument-hint: [--crate <name> | <file-path>] [--dry-run]
---

Parse `$ARGUMENTS`:
- If `--crate <name>` is present, set `$TARGET_CRATE` to the name. All `.rs`
  files under `crates/$TARGET_CRATE/src/` (excluding `tests/` subdirectories)
  will be audited.
- If a file path is given (e.g., `crates/tx/src/frame.rs`), set `$TARGET_FILE`
  to that path. Only that file will be audited.
- If neither is provided, audit **all crates** in priority order (see Step 3).
- If `--dry-run` is present, set `$DRY_RUN = true`. Findings are printed but
  no GitHub issues are filed.

# Security Audit

Perform a CTF-style security audit of the henyey codebase, validating each
finding against stellar-core and the actual production code paths before
reporting. File GitHub issues only for confirmed, high-confidence findings.

This skill supersedes `scripts/audit.sh` and `scripts/audit-opencode.sh`.

---

## Audit Context

**Read and internalize this entire section before auditing any code.** It
defines the project identity, crate risk tiers, and explicit false-positive
suppression rules derived from analysis of prior audits (97.5% false-positive
rate — 195 of 200 findings were invalid).

### Project Identity

Henyey is a **Rust re-implementation of stellar-core** (v25.x / protocol 25).
The stellar-core C++ source is available as a git submodule at `stellar-core/`
(pinned to v25.0.1).

**Behavior that matches stellar-core is correct, not a bug.** Consensus parity
is a hard requirement — every node on the Stellar network must produce
identical ledger state for identical inputs. Diverging from stellar-core
*introduces* bugs; matching it *prevents* them.

### Crate-to-Upstream Mapping

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
| `crates/rpc` | *(no upstream — henyey-specific)* |
| `crates/simulation` | *(no upstream — test infrastructure)* |

### Crate Risk Tiers

| Tier | Crates | Security Model |
|------|--------|----------------|
| **Consensus-critical** | tx, ledger, scp, herder, bucket | Determinism required. Parity with stellar-core is mandatory. Bugs here cause chain splits, double-spends, or network halts. |
| **Network-facing** | overlay, rpc | Handles untrusted external input. NOT consensus-critical (overlay delivers messages to herder/SCP which re-validate). |
| **Infrastructure** | app, history, historywork, crypto, common, db, clock | Supporting code. Bugs may cause crashes or data corruption but not consensus divergence directly. |
| **Test/development only** | simulation, work | NOT production code. Only file issues for CRITICAL severity findings. |

### What Is NOT a Finding

These are the **10 most common false-positive patterns** from prior audits.
Before filing any finding, verify it does not fall into one of these
categories.

#### Rule 1: Behavior that matches stellar-core

If the henyey code does the same thing as the corresponding stellar-core code,
it is **parity**, not a bug. This is the single most common false-positive
pattern (63% of all prior FPs).

**Before filing**: Read the upstream `.cpp`/`.h` in `stellar-core/src/` using
the crate-to-upstream mapping. If the behavior matches, it is not a finding.

Examples of correct parity that auditors previously flagged:
- Floating-point price comparison in offer ordering (matches `isBetterOffer`)
- Plain `i64` arithmetic without overflow checks (matches stellar-core's C++ arithmetic)
- `Ed25519SignedPayload` signer ordering rules
- Medium threshold for `SetOptions` flag changes
- Non-ASCII `ManageData` key rejection (only printable ASCII 0x20-0x7E)
- Singleton self-quorum for SCP Externalize statements

#### Rule 2: HashMap/HashSet in non-consensus paths

Nondeterministic iteration is **only** a finding if the iteration order
affects consensus output:
- `tx_set_result_hash` (hash of TransactionResultSet)
- `bucket_list_hash`
- Ledger header fields
- Order of ledger entry writes to the bucket list

Nondeterministic iteration in these contexts is **NOT** a finding:
- Internal caching and deduplication (e.g., key dedup before batch load)
- `TransactionMeta` / `LedgerCloseMeta` (not part of ledger hash)
- Logging, metrics, diagnostics
- Test-only helper methods (e.g., `written_entries()`, `deleted_entries()`)
- Internal data structures that feed into a sorted output (e.g., HashMap
  results collected into a BTreeMap before use)

#### Rule 3: `unwrap()` / `expect()` on protocol invariants

An `unwrap()` or `expect()` is a finding only if attacker-controlled input can
trigger it in production. It is **NOT** a finding when:
- The value is guaranteed by protocol constraints (e.g., id_pool overflow
  requires > `i64::MAX` operations per ledger — physically unreachable)
- The field is always `Some` during the execution phase where it is accessed
  (e.g., `offer_store` is always `Some` during offer operations, `None` only
  in Soroban-only paths that never touch offers)
- The invariant is documented with a `// INVARIANT:` comment
- The data comes from internally-validated sources (e.g., XDR that was already
  deserialized successfully — re-serialization cannot fail)

#### Rule 4: Dead code / test-only paths

Code that is never called in production is not a production vulnerability.
Common patterns:
- Library APIs with no production callers (e.g., `validate_full()` in
  `crates/tx/src/validation.rs` — the production path is
  `TransactionExecutor::validate_preconditions()`)
- `#[allow(dead_code)]` annotated functions kept for parity
- Methods only called from `#[cfg(test)]` modules
- `ResourceLimits::default()` — dead code; real budget comes from
  `Budget::try_from_configs()`

**Before filing**: Search for production callers. If zero exist outside tests,
it is not a finding (or at most LOW / informational).

#### Rule 5: Validation at a different layer

Input may be validated at a layer the auditor does not see when examining a
single file. Common validation boundaries:
- **Herder layer**: Slot range checks before messages reach SCP
- **Overlay frame decoder**: `MAX_MESSAGE_SIZE` bounds input before XDR decode
- **HTTP framework**: Body size limits before RPC handler sees input
- **serde deserialization**: Type-safe parsing rejects out-of-range values
- **Transaction validation phase**: Signatures, sequence numbers, fees, and
  preconditions validated before operation execution
- **Soroban host**: Entry-count limits enforced by the Soroban runtime

**Before filing**: Trace the production call chain back to the entry point.
If the input is bounded or validated before reaching the flagged code, it is
not a finding.

#### Rule 6: TransactionMeta is not consensus-critical

`TransactionMeta` and `LedgerCloseMeta` are produced for downstream consumers
(Horizon, RPC clients) but are **NOT** part of the ledger hash or
`tx_set_result_hash`. Nondeterministic ordering or formatting differences in
meta output do not affect consensus.

#### Rule 7: Simulation crate is test infrastructure

The `simulation` crate (`crates/simulation/`) is development/test
infrastructure. It is not deployed in production. Findings in this crate
should only be filed at CRITICAL severity (e.g., if test infrastructure could
mask real consensus bugs). MEDIUM/HIGH findings in simulation code are not
actionable.

Similarly, the `work` crate is an internal async task scheduler — not
consensus-critical.

#### Rule 8: "Unbounded" allocations with implicit bounds

Many data structures appear unbounded in isolation but are constrained by:
- **Authenticated peer count** (overlay: bounded by max peer config)
- **Flow control windows** (overlay: per-peer message limits)
- **Protocol limits** (tx: max operations per tx, max signers per account)
- **Network size** (peer universe is finite and known)
- **Total XLM supply** (~50 billion lumens fits in i64 with margin)

**Before filing**: Identify the actual bound. If one exists at a higher layer,
the allocation is not unbounded.

#### Rule 9: Protocol < 24 is out of scope

This project only supports protocol 24+. Findings about behavior under
earlier protocols, missing legacy protocol code paths, or `ContractLedgerCostExtV0`
defaults for protocol < 24 are not applicable.

#### Rule 10: `Limits::none()` after frame-level size check

Using `Limits::none()` for XDR deserialization is correct when the input
buffer is already bounded by the frame-level `MAX_MESSAGE_SIZE` check.
stellar-core similarly does not apply secondary XDR depth/size limits on peer
messages. Double-limiting would diverge from upstream.

### Code Annotations

The codebase uses two structured annotation patterns. If you encounter them,
read them before flagging the annotated code:

- **`// SECURITY: <what> at <where>`** — Documents that input is validated at
  a different layer. The comment explains what is validated and where.
- **`// INVARIANT: <why this holds>`** — Documents why an `unwrap()`,
  `expect()`, or panic path is unreachable in production.

---

## Step 1: Preparation

### 1a: Determine Target Files

Based on `$ARGUMENTS`, build the list of files to audit:

- **Single file** (`$TARGET_FILE`): Just that file.
- **Single crate** (`$TARGET_CRATE`): All `.rs` files under
  `crates/$TARGET_CRATE/src/`, excluding files under `tests/` subdirectories.
  Use Glob to enumerate them.
- **All crates**: Iterate crates in the priority order defined in Step 3.
  For each crate, enumerate `.rs` files as above.

### 1b: Read Crate Context (once per crate)

For each crate being audited, read these files if they exist:
- `crates/<crate>/PARITY_STATUS.md` — understand file mapping, parity status,
  architectural differences, and intentional omissions
- `crates/<crate>/README.md` — understand the crate's purpose and design

Store the crate context for reference while auditing individual files.

### 1c: Initialize Tracking

Use TodoWrite to create a task list tracking progress through the target files.
Group by crate if auditing multiple crates.

Initialize counters:
```
files_audited = 0
findings = []  # (severity, crate, title, body)
```

---

## Step 2: Per-File Audit

For each file in the target list:

### 2a: Read the File

Read the file contents using the Read tool.

If the file is very large (> 2000 lines), read it in sections. Focus on:
- Public functions and their implementations
- Error handling paths (`.unwrap()`, `.expect()`, `?`, `unwrap_or`)
- Unsafe blocks
- Arithmetic operations on financial values
- Data structure choices (HashMap vs BTreeMap)
- Serialization/deserialization boundaries

### 2b: Analyze for Vulnerabilities

Look for these vulnerability classes, ordered by severity:

**CRITICAL:**
- Consensus safety: equivocation, fork attacks, quorum manipulation, vote
  replay that could cause network splits or double-spends
- Transaction validation bypasses: missing checks that allow unauthorized
  operations, balance manipulation, or fee evasion
- Cryptographic misuse: timing side-channels in signature verification,
  weak/predictable randomness, key material leaks, nonce reuse
- Determinism violations: any code path where two honest nodes processing
  the same ledger could reach different states (floating point, HashMap
  iteration order, system clock usage, thread-dependent ordering)

**HIGH:**
- Integer overflow/underflow in financial calculations (balances, fees,
  offers, liquidity pools) — even with Rust's default overflow checks,
  look for wrapping ops, casts, or checked arithmetic that silently saturates
- Network/overlay attacks: eclipse attacks, amplification DoS, malformed
  message injection, authentication bypass, unbounded allocations from
  peer data
- Race conditions in async/concurrent code: TOCTOU bugs, lock ordering
  issues, missing atomicity in multi-step state mutations
- Unsafe Rust: memory safety violations, unsound abstractions

**MEDIUM:**
- RPC input validation: injection, unbounded queries, information leaks
- Resource exhaustion: unbounded Vec/HashMap growth from external input,
  missing size limits on deserialized XDR
- Error handling: panics on untrusted input, swallowed errors that hide
  corruption, unwrap() on fallible operations in non-test code
- Logic bugs: off-by-one in protocol-critical ranges, missing edge cases

### 2c: Validate Each Potential Finding

**This is the critical step that distinguishes this skill from the old scripts.**

For each potential finding, run through this validation checklist before
accepting it:

1. **Check the 10 suppression rules** (Section "What Is NOT a Finding").
   If any rule applies, discard the finding.

2. **Parity check** (for consensus-critical crates): Read the corresponding
   stellar-core code using the crate-to-upstream mapping. Use subagents
   (Task tool with `explore` type) to read the upstream `.h` and `.cpp` files.
   If the behavior matches, discard the finding.

3. **Call-site reachability check**: Search for production callers of the
   flagged function. Use subagents or Grep to find call sites outside
   `#[cfg(test)]` and `tests/` directories. If no production callers exist,
   discard or downgrade to LOW.

4. **Upstream guard check**: Trace the data flow back to the entry point.
   Is the input validated, bounded, or sanitized before reaching this code?
   If yes, discard.

5. **Annotation check**: Does the code have a `// SECURITY:` or
   `// INVARIANT:` comment explaining why it is safe? If yes, verify the
   claim and discard if valid.

6. **Prior issue check**: Search for an existing open GitHub issue with
   a matching title pattern:
   ```
   gh issue list --label security,audit --state open --json number,title --jq '.[].title' | grep -i "<key phrase>"
   ```
   If a matching issue already exists, skip to avoid duplicates.

Only findings that survive all 6 checks are accepted.

### 2d: Record Findings

For each accepted finding, record:
- **Severity**: CRITICAL / HIGH / MEDIUM / LOW
- **Crate**: the crate name
- **Title**: short descriptive title
- **Location**: function name, file path, approximate line numbers
- **Category**: from the vulnerability classes above
- **Description**: what the bug is, concretely
- **Exploit scenario**: how an attacker would trigger it
- **Suggested fix**: one-liner or short description

Update the TodoWrite progress tracker.

---

## Step 3: Crate Iteration Order

When auditing all crates (no `--crate` or file argument), process in this
priority order (highest risk first):

```
crypto scp overlay
tx ledger bucket
rpc herder app
history historywork db common work simulation clock henyey
```

For each crate, use subagents (Task tool with `explore` type) to read files
and gather context efficiently. Launch exploration agents to:
- Read all `.rs` files in the crate's `src/` directory
- Identify key data structures, public APIs, and state machines
- Flag potential vulnerability patterns for detailed analysis

Then perform the detailed per-file audit (Step 2) on files with potential
findings.

---

## Step 4: Deduplication and Consolidation

After all target files have been audited:

1. **Deduplicate**: If the same root cause was found in multiple files (e.g.,
   a shared helper function called from several operations), merge into one
   finding listing all affected locations.

2. **Consolidate**: Group related findings (e.g., "HashMap nondeterminism"
   in 3 locations → one finding with 3 affected locations, unless the impact
   differs per location).

3. **Re-rank**: After dedup, reassess severity. A pattern appearing in many
   locations may warrant a higher severity than any single instance.

4. **Final suppression pass**: Review the consolidated list one more time
   against the 10 suppression rules. Remove any findings that became
   obviously false-positive after seeing the full picture.

---

## Step 5: File GitHub Issues

Skip this step if `$DRY_RUN = true`. Instead, print each finding in the
format below and stop.

For each confirmed finding, file a GitHub issue:

```bash
gh issue create --title "[SEVERITY] [CRATE] Short title" \
  --label "security,audit,SEVERITY_LOWERCASE" \
  --body "$(cat <<'EOF'
## Audit Finding

**Source file**: `FILE_PATH`
**Crate**: `CRATE_NAME`
**Severity**: SEVERITY
**Source**: Automated security audit (skill: /audit)

---

**Findings**

FINDING_DESCRIPTION

**Location**: `function_name` (`file:line_start-line_end`)

**Description**: What the bug is, concretely.

**Exploit scenario**: How an attacker would trigger it.

**Suggested fix**: One-liner or short description.
EOF
)"
```

Severity label values: `critical`, `high`, `medium`, `low`.

If a finding spans multiple files, list all locations in the body.

Print each filed issue's URL as it is created.

---

## Step 6: Summary

After all issues are filed (or printed in dry-run mode), output a completion
summary:

```
=== Audit Complete ===
Target:        <crate name, file path, or "all crates">
Files audited: N
Findings:      X (Y CRITICAL, Z HIGH, W MEDIUM, V LOW)
Issues filed:  X  (or "0 (dry run)" if --dry-run)
```

If zero findings were produced, state:

```
=== Audit Complete ===
Target:        <target>
Files audited: N
Findings:      0 — No significant findings.
```

---

## Guidelines

- **Use subagents for exploration.** When you need to read stellar-core files,
  search for callers, or scan multiple files for a pattern, launch Task agents
  with `explore` type. Do not read everything sequentially.
- **When in doubt, read stellar-core.** The #1 source of false positives is
  flagging behavior that matches upstream. Spend the time to check parity
  rather than filing a dubious finding.
- **One issue per finding.** Do not bundle multiple unrelated findings into one
  issue. Related findings (same root cause, multiple locations) can be one
  issue.
- **Quality over quantity.** 5 true positives are worth more than 200 false
  positives. The prior audit had a 97.5% false-positive rate — do better.
- **Track progress with TodoWrite.** Update task status as you work through
  crates and files.
- **Respect the crate tier.** Apply appropriate severity thresholds:
  consensus-critical crates warrant thorough analysis; simulation/work crate
  findings below CRITICAL should be skipped.
- **Do not report style issues.** Missing docs, naming conventions, test
  coverage gaps, and code style are not security findings.
- **Do not report issues that require physical infeasibility.** Integer
  wraparound at `u32::MAX` that would take 680 years at current rates is not
  exploitable.
