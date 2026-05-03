---
name: review-fix-oc
description: Review a committed fix for correctness, test coverage, and similar issues (opencode variant — uses glm-5.1 for review)
argument-hint: <commit-hash>
---

Parse `$ARGUMENTS`:
- The first argument is the commit hash (short or full). Replace `$COMMIT` with it.

> **Model configuration.** This opencode variant hardcodes the review
> sub-agent to use `glm-5.1`. The orchestrator (you) runs as `kimi-k2.6`.
> There is no `--model` flag — the model is fixed.

# Fix Review (opencode variant)

Review the fix in commit `$COMMIT` to assess correctness, test coverage, and
whether similar issues exist elsewhere in the codebase. This formalizes the
post-fix review process described in AGENTS.md:

> After committing a fix, review and consider: Is it true to the design of the
> system? Can there be similar issues? Can we redesign the system to avoid these
> category of issues.

## Analysis Process

### Step 1: Read the Commit

Run:
```
git log -1 --format=fuller $COMMIT
git show $COMMIT --stat
git diff $COMMIT~1..$COMMIT
```

Identify: the commit message, author, files changed, lines added/removed, and
the full diff. Read the complete current state of every file touched by the
commit to understand surrounding context.

### Step 2: Analyze the Problem

From the diff (what was removed or changed) and the commit message, reconstruct:

- **What was the bug?** Describe the incorrect behavior in concrete terms.
- **What was the root cause?** Why did the original code produce incorrect
  behavior? Trace to the specific logical error, missing condition, wrong
  assumption, or misunderstood invariant.
- **What was the impact?** What observable behavior was affected? Could this
  have caused state divergence, consensus failure, data corruption, or a crash?

Read the code before and after the fix. Do not guess — if the root cause is
unclear, read callers, callees, and related types until you understand it.

### Step 3: Analyze the Fix

Evaluate the fix along these dimensions:

- **Root cause vs. symptom**: Does the fix address the root cause, or does it
  paper over a symptom? A fix that only handles one manifestation of a deeper
  problem is incomplete.
- **Design fit**: Is the fix consistent with the system's architecture and
  conventions? Or does it introduce a special case, workaround, or pattern that
  diverges from the surrounding code?
- **Correctness**: Is the fix logically correct in all cases? Work through edge
  cases: empty inputs, zero values, overflow, maximum sizes, concurrent access,
  error paths.
- **Side effects**: Could the fix change behavior in any code path other than
  the one it targets? Check all callers of modified functions and all consumers
  of modified types.
- **Parity**: If the fix touches protocol, consensus, or ledger logic, verify
  that the fixed behavior matches stellar-core. Read the corresponding
  stellar-core code to confirm.

Classify the fix:
- **SOUND**: Correctly addresses the root cause with no concerns.
- **CONCERNS**: Fix is directionally correct but has issues that need attention.
- **INCOMPLETE**: Fix does not fully address the root cause.
- **WRONG**: Fix introduces new incorrect behavior.

### Step 4: Verify Test Coverage

Check whether the commit includes regression tests:

- **If tests are included**: Read each test. Would it have failed before the fix
  and passed after? A test that passes both before and after is not a regression
  test. Assess whether the tests cover the specific edge case that triggered the
  bug, or only the happy path.
- **If tests are NOT included**: Flag this as a gap. Describe what test(s)
  should exist: the setup, the operation, and the assertion.

Also assess existing test coverage of the affected code:
- Run `cargo test -p <crate> -- --list` to see what tests exist for the crate.
- Read tests in the affected module to understand what paths are exercised.
- Identify any untested code paths through the fixed code.

### Step 5: Search for Similar Issues

Use subagents (Task tool with `explore` type) to scan the codebase for patterns
similar to the one that was buggy. The search strategy depends on the root cause:

- **Same function pattern**: If the bug was a wrong condition or missing check,
  search for the same pattern in other locations.
- **Same API misuse**: If the bug was incorrect use of an API or type, search
  for other callers of that API.
- **Same category of mistake**: If the bug was (e.g.) an off-by-one, integer
  overflow, missing None check, wrong enum variant, or stale cache, search for
  the same class of mistake across the codebase.

For each potential similar issue found, assess:
- Is it actually the same class of bug, or superficially similar but correct?
- What is the risk if it is a real bug?
- What file:line is it at?

Do not report false positives. Read the surrounding code to confirm before
including a finding.

### Step 6: Identify Refactoring Opportunities

Consider whether the code can be restructured to make this category of bug
impossible or unlikely:

- **Type-system enforcement**: Could a newtype, enum, or const generic prevent
  the invalid state that caused the bug?
- **Shared helper**: Could the correct logic be extracted into a single function
  that all call sites use, eliminating the chance of one site getting it wrong?
- **Invariant enforcement**: Could a debug assertion, runtime check, or
  constructor invariant catch this class of error early?
- **API redesign**: Could the API be changed so that the incorrect usage is not
  expressible? (e.g., builder pattern, state machine types)

Only suggest refactors that are proportionate to the risk. A one-off typo does
not justify a type-system overhaul.

## Output Format (review mode)

```
# Fix Review: $COMMIT_SHORT_HASH

## Commit Summary
- **Hash**: full hash
- **Message**: commit message
- **Author**: author
- **Files changed**: list with line counts

## Problem Analysis
- **Bug**: What was wrong
- **Root cause**: Why the original code was incorrect
- **Impact**: What observable behavior was affected

## Fix Analysis
- **Approach**: What the fix does
- **Correctness**: Does it address the root cause?
- **Design fit**: Is it consistent with the system's design?
- **Edge cases**: Any cases not covered?
- **Parity**: Does it maintain stellar-core parity? (if applicable)
- **Side effects**: Any unintended behavioral changes?
- **Verdict**: SOUND / CONCERNS / INCOMPLETE / WRONG — summary

## Test Coverage
- **Regression test included**: Yes/No
- **Test quality**: Would it have caught the original bug?
- **Existing coverage**: Are other paths through this code tested?
- **Gaps**: Any untested scenarios that should be covered

## Similar Issues

| # | Location | Pattern | Risk | Confirmed |
|---|----------|---------|------|-----------|

(Locations in the codebase with the same pattern that might harbor the same
class of bug. "Confirmed" = Yes if you read the code and verified it is a real
issue, Likely if the pattern matches but you could not fully confirm.)

If no similar issues found, state: "No similar issues identified."

## Refactoring Opportunities

(Concrete suggestions for redesigning the code to prevent this category of
issue. Include file:line references and sketch the proposed change.)

If no refactoring warranted, state: "No refactoring needed — the fix is
proportionate and the pattern is isolated."

## Recommendations

Prioritized list of follow-up actions (if any).
```

## Guidelines

- Be precise. Cite `file:line` for every claim.
- Do not speculate — if you cannot determine whether something is a bug, read
  more code until you can. If you still cannot, say so explicitly.
- Read the actual code, not just the diff. The diff shows what changed; the
  surrounding code shows whether the change is correct.
- Focus on observable behavior. Different code structure with identical behavior
  is not a finding.
- If the commit touches protocol, consensus, or ledger logic, always verify
  against stellar-core.
- Do not inflate findings. If the fix is sound, the tests are adequate, and
  there are no similar issues, say so. A clean report is a valid outcome.
- Use subagents for codebase-wide searches to keep the analysis thorough without
  overwhelming context.
- **Model configuration.** When spawning review sub-agents via the Task tool,
  use `"model": "glm-5.1"` and `"subagent_type": "reviewer"`.