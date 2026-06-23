---
name: review-fix
description: Review a committed fix for correctness, plan follow-through, test coverage, and similar issues
argument-hint: <commit-hash> [plan-path-or-issue]
---

Parse `$ARGUMENTS`:
- The first argument is the commit hash (short or full). Replace `$COMMIT` with it.
- The second argument, if present, is optional plan context. It may be a local
  plan file path, a GitHub issue number, or a GitHub issue URL.

# Fix Review

Review the fix in commit `$COMMIT` to assess correctness, plan follow-through
when an implementation plan is available, test coverage, and whether similar
issues exist elsewhere in the codebase. This formalizes the post-fix review
process described in AGENTS.md:

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

### Step 2: Locate the Implementation Plan

Determine whether an implementation plan is available. Check, in order:

1. The optional second argument:
   - If it is a file path, read that file.
   - If it is a GitHub issue number or URL, fetch the issue body and comments.
2. Any explicit plan text included in the prompt or surrounding invocation
   context.
3. A related GitHub issue mentioned in the prompt, commit message, or commit
   trailers (for example, "issue #123", "Closes #123", or "Fixes #123"). Fetch
   the issue body and comments.
4. Local plan-do-review artifacts for that issue, such as
   `data/pdr-$ISSUE/proposal_final.md`, if present in the current checkout.

When reading a GitHub issue, prefer the latest `## Converged Proposal` comment.
If there is no converged proposal, use the issue body only if it is clearly a
proposal or implementation plan. Do not treat brainstorming comments, rejected
drafts, or stale proposal rounds as the final plan when a converged proposal is
available.

If no reliable plan is available, set the plan follow-through verdict to
`NOT_AVAILABLE` and continue the rest of the review unchanged. Absence of a plan
is not itself a finding.

### Step 3: Analyze the Problem

From the diff (what was removed or changed) and the commit message, reconstruct:

- **What was the bug?** Describe the incorrect behavior in concrete terms.
- **What was the root cause?** Why did the original code produce incorrect
  behavior? Trace to the specific logical error, missing condition, wrong
  assumption, or misunderstood invariant.
- **What was the impact?** What observable behavior was affected? Could this
  have caused state divergence, consensus failure, data corruption, or a crash?

Read the code before and after the fix. Do not guess — if the root cause is
unclear, read callers, callees, and related types until you understand it.

### Step 4: Analyze the Fix

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

### Step 5: Evaluate Plan Follow-through

If an implementation plan is available, compare it against the commit and the
current code. Extract from the plan:

- **Intended outcome**: What behavior or capability the plan promised.
- **Required work items**: Concrete tasks, code paths, tests, docs, parity checks,
  or operational changes the plan said to perform.
- **Constraints and non-goals**: Explicit limits, sequencing requirements, or
  things the plan said not to do.
- **Deferred work**: Items the plan intentionally left for follow-up.

Evaluate:

- **Coverage**: Did the commit implement every material required work item?
- **Deviations**: Did the commit take a different approach than the plan? If so,
  is the deviation justified by evidence discovered during implementation?
- **Scope creep**: Did the commit add unrelated behavior not supported by the
  plan or necessary for correctness?
- **Deferrals**: Are intentionally deferred items called out clearly, and are
  follow-up issues or recommendations present where needed?
- **Consistency**: Do tests, docs, and parity checks promised by the plan appear
  in the commit or have a justified omission?

Classify plan follow-through separately from fix correctness:

- **FOLLOWED**: The commit implements all material plan items with no unjustified
  deviations.
- **PARTIAL**: The commit implements the core plan but misses, silently defers,
  or weakens one or more material items.
- **DIVERGED**: The commit implements a materially different approach, violates a
  plan constraint, or leaves the plan's intended outcome unsatisfied.
- **NOT_AVAILABLE**: No reliable implementation plan was found.

A fix can be logically sound while still `PARTIAL` or `DIVERGED` relative to its
plan. Conversely, if the plan itself was wrong or unsafe, do not reward blind
compliance — explain the plan problem and evaluate whether the commit made a
justified correction.

### Step 6: Verify Test Coverage

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

### Step 7: Search for Similar Issues

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

### Step 8: Identify Refactoring Opportunities

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

## Plan Follow-through
- **Plan source**: Prompt context / file path / GitHub issue/comment / Not available
- **Plan verdict**: FOLLOWED / PARTIAL / DIVERGED / NOT_AVAILABLE
- **Planned work**: Material plan items extracted from the plan
- **Implemented work**: Which planned items the commit completed
- **Missing or changed work**: Planned items omitted, weakened, or implemented differently
- **Justified deviations**: Deviations or deferrals that are supported by evidence

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

Prioritized list of follow-up actions (if any), including any missing plan items
or unjustified deviations that should be fixed or filed as follow-up work.
```

## Guidelines

- Be precise. Cite `file:line` for every claim.
- Do not speculate — if you cannot determine whether something is a bug, read
  more code until you can. If you still cannot, say so explicitly.
- Read the actual code, not just the diff. The diff shows what changed; the
  surrounding code shows whether the change is correct.
- Treat the implementation plan as review evidence, not as a substitute for
  correctness. If following the plan would be wrong, flag the plan issue and
  assess whether the commit's deviation is justified.
- Cite the plan source for every plan follow-through claim. If the source is a
  GitHub issue comment, cite the issue/comment context; if it is a file, cite
  `file:line`.
- Focus on observable behavior. Different code structure with identical behavior
  is not a finding.
- If the commit touches protocol, consensus, or ledger logic, always verify
  against stellar-core.
- Do not inflate findings. If the fix is sound, the tests are adequate, and
  there are no similar issues, say so. A clean report is a valid outcome.
- Use subagents for codebase-wide searches to keep the analysis thorough without
  overwhelming context.
