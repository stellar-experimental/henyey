---
name: propose-execute-loop
description: Iterate over all open GitHub issues, AI-triage each for readiness, and call /propose-execute on actionable ones
argument-hint: "[--label <label>] [--model <model>] [--batch-size N] [--max-proposal-rounds N] [--max-review-rounds N] [--dry-run]"
---

Parse `$ARGUMENTS`:
- `--label <label>`: Optional extra label to filter issues (e.g., `proposal`).
  If omitted, all open issues are considered.
- `--model <model>`: Model for triage, critic, and review agents
  (default: `"gpt-5.4"`). Passed through to `/propose-execute`.
- `--batch-size N`: Number of issues to fetch per batch (default: 20).
  Each batch is fetched, triaged, and executed before the next batch is
  fetched.
- `--max-proposal-rounds N`: Passed through to `/propose-execute` (default: 5).
- `--max-review-rounds N`: Passed through to `/propose-execute` (default: 3).
- `--dry-run`: Print the triaged queue only; do not execute anything.

# Propose-Execute Loop

Iterate over open GitHub issues in batches. For each batch: fetch a page of
issues, use AI triage agents to determine which are actionable, then call
`/propose-execute` on each actionable issue sequentially. Repeat until no
more issues remain.

The orchestrator (you) manages the queue, spawns triage agents, invokes
`/propose-execute`, handles failures, and tracks progress.

---

## Pre-flight: Ensure Labels Exist

Ensure the `propose-execute-failed` label exists (create it if not):
```bash
gh label create propose-execute-failed --description "propose-execute-loop attempted and failed" --color D93F0B 2>/dev/null || true
```

Initialize cumulative counters:
- `total_triaged = 0`
- `total_actionable = 0`
- `total_skipped = 0`
- `total_completed = 0`
- `total_failed = 0`
- `batch_number = 0`
- `pagination_cursor = ""` (empty string = start from beginning)
- `all_commits = []`

---

## Batch Loop

Repeat the following steps until a batch returns zero issues:

### Step 1: Fetch Next Batch of Issues

Increment `batch_number`.

Build the fetch command with search-based pagination:

```bash
# Base command
gh issue list --state open --json number,title,body,labels,assignees,createdAt --limit $BATCH_SIZE --search "sort:created-asc"
```

If `$LABEL` is set, add it to the filter:
```bash
gh issue list --state open --label $LABEL --json number,title,body,labels,assignees,createdAt --limit $BATCH_SIZE --search "sort:created-asc"
```

If `pagination_cursor` is not empty (i.e., not the first batch), append a
date filter to skip already-seen issues:
```bash
# Append to --search: created:>$PAGINATION_CURSOR
gh issue list --state open --json number,title,body,labels,assignees,createdAt --limit $BATCH_SIZE --search "sort:created-asc created:>$PAGINATION_CURSOR"
```

**Pagination cursor update:** After fetching, set `pagination_cursor` to the
`createdAt` value of the **last issue** in the returned batch. This becomes
the lower bound for the next fetch.

**Exit condition:** If the fetch returns zero issues, exit the batch loop
and proceed to the Completion Summary.

#### Filtering and Sorting

From the fetched batch, exclude:
- Issues that already have the `propose-execute-failed` label.

Sort the remaining issues by priority within this batch:

1. **critical** — issues with the `critical` label
2. **high** — issues with the `high` label
3. **medium** — issues with the `medium` label
4. **low** — issues with the `low` label
5. **informational** — issues with the `informational` label
6. **unlabeled** — issues with no priority label

Within the same priority tier, sort by issue number ascending (lowest number
first = oldest first).

---

### Step 2: Print Batch Queue

Print the batch queue as a table:

```
═══ BATCH $BATCH_NUMBER ═══
N issues to triage

  #  | Issue | Priority | Title
-----|-------|----------|------
  1  | #42   | HIGH     | Implement bucket merge optimization
  2  | #55   | MEDIUM   | Add missing validation for...
  3  | #12   | —        | Refactor overlay flow control
 ...
════════════════════════════
```

---

### Step 3: Triage Batch

For each issue in the batch, spawn triage agents to determine if the issue
is well-defined and ready for implementation.

Process triage in batches of up to **10 issues at a time** to avoid
overwhelming the system. Wait for each triage sub-batch to complete before
launching the next.

#### 3a: Spawn Triage Agent

Launch an agent using the Task tool:
- **agent_type**: `"explore"`
- **model**: `$MODEL`
- **name**: `"triage-{issue_number}"`
- **description**: `"Triage issue #{issue_number}"`
- **mode**: `"background"`

The triage agent prompt:

```
You are triaging a GitHub issue for the henyey project (a Rust implementation
of stellar-core) to determine if it is well-defined and ready for
implementation.

## The Issue

**#{issue_number}: {title}**

{issue_body}

## Your Task

Evaluate whether this issue is **actionable** — meaning it is concrete,
well-scoped, and has a clear benefit. Specifically:

1. **Concrete**: The issue describes a specific code change with file paths,
   function names, or code references. A developer would know exactly what
   to change.
2. **Well-scoped**: The scope is bounded. "Refactor X to use struct Y" is
   well-scoped; "improve the overlay layer" is not.
3. **Clear benefit**: The change has an obvious payoff — reduced duplication,
   better readability, fewer parameters, stronger types, etc.

That's it. Do NOT skip issues because:
- They touch parity-sensitive or consensus-critical code
- They seem large or time-consuming
- They require cross-crate changes
- They involve complex refactors
- The code is "good enough" as-is

The bar is: concrete + well-scoped + clear benefit = ACTIONABLE.

## Output

You MUST end your response with exactly one of:

VERDICT: ACTIONABLE
(The issue is concrete, well-scoped, and has a clear benefit.)

VERDICT: SKIP — <one-line reason>
(The issue is not ready. State the specific reason after the dash.)

Examples of SKIP reasons:
- "Too vague — no concrete acceptance criteria"
- "Discussion thread, not an implementation request"
- "Blocked on decision about X"
- "Already implemented (should be closed)"
- "Duplicate of #N"
- "False positive — the code is correct as described"
```

#### 3b: Process Triage Result

Read the agent result. Extract the verdict.

**If `VERDICT: ACTIONABLE`**:
- Add the issue to the batch's **execution queue**.

**If `VERDICT: SKIP`**:
- Record the skip reason.
- Print: `  SKIP #<number> — <reason>`

**If verdict unclear** (agent error):
- Treat as `SKIP — triage agent did not produce a clear verdict`.

#### 3c: Print Batch Triage Summary

After all issues in this batch are triaged:

```
═══ BATCH $BATCH_NUMBER TRIAGE COMPLETE ═══
Batch issues:    N
Actionable:      A
Skipped:         S

Execution order:
  1. #42 — Implement bucket merge optimization
  2. #55 — Add missing validation for...
 ...
════════════════════════════════════════════
```

Update cumulative counters:
- `total_triaged += N`
- `total_actionable += A`
- `total_skipped += S`

If `--dry-run` is set, **skip Step 4** and continue to the next batch.

---

### Step 4: Execute Batch

Process each actionable issue from this batch sequentially.

#### 4a: Pre-flight Check

Before each issue:
1. Ensure the working tree is clean: `git status --porcelain`
   - If dirty, **stash** the changes rather than discarding them:
     ```bash
     git stash push -m "propose-execute-loop: stashed before issue #<number>"
     ```
   - If stash fails, abort the loop and report the dirty state to the user.
2. Pull latest: `git pull --rebase`
3. Re-check that the issue is still open and not assigned to someone else:
   ```bash
   gh issue view <number> --json state,assignees
   ```
   - If the issue is **closed**, skip it and print:
     `  SKIP #<number> — closed since triage`
   - If the issue has **assignees** (and was unassigned at triage time), skip
     it and print:
     `  SKIP #<number> — assigned to @<login> since triage`
4. Self-assign the issue to signal to other agents that work is in progress:
   ```bash
   gh issue edit <number> --add-assignee @me
   ```

#### 4b: Invoke /propose-execute

Call the `propose-execute` skill with the issue number and all pass-through
flags:

```
/propose-execute <issue_number> --model $MODEL --max-proposal-rounds $MAX_PROPOSAL_ROUNDS --max-review-rounds $MAX_REVIEW_ROUNDS
```

This means: follow the full `propose-execute` skill protocol — fetch the
issue, run proposal convergence, implement, run review-fix loop, and post
completion comment.

#### 4c: Handle Success

If `/propose-execute` completes successfully:
- Record the issue as completed.
- Increment `total_completed`.
- Collect any new commits into `all_commits`.
- Print: `  ✓ #<number> — completed`

#### 4c′: Timeout Guidance

If `/propose-execute` has been running for an unreasonably long time (e.g.,
more than 60 minutes on a single issue with no progress), treat it as a
failure and follow Step 4d.

#### 4d: Handle Failure

If `/propose-execute` fails (build breaks, tests fail, review loop exhausted,
or any other error):

1. Revert uncommitted changes (only tracked files and files generated by the
   build — do **not** remove pre-existing untracked files):
   ```bash
   git checkout -- .
   git clean -fd --exclude='*.log' --exclude='local-data/'
   ```
2. Label the issue so it is excluded from future runs:
   ```bash
   gh issue edit <number> --add-label "propose-execute-failed"
   ```
3. Post a comment explaining the failure:
   ```bash
   gh issue comment <number> --body "## propose-execute-loop: Failed

   This issue was attempted by the propose-execute-loop skill but failed.

   **Reason:** <brief description of what went wrong>

   The issue has been labeled \`propose-execute-failed\` and will be skipped
   in future loop runs. Remove the label to retry.

   ---
   *Automated by the \`propose-execute-loop\` skill.*"
   ```
4. Increment `total_failed`.
5. Print: `  ✗ #<number> — failed: <reason>`
6. Continue to the next issue.

#### 4e: Progress Update

After each issue in the batch, print a progress summary:

```
Batch $BATCH_NUMBER progress: <batch_processed>/<batch_actionable>
Cumulative: <total_completed> completed, <total_failed> failed, <total_skipped> skipped
```

---

*End of batch loop — go back to Step 1 to fetch the next batch.*

---

## Completion Summary

After the batch loop exits (no more issues to fetch):

```
═══ PROPOSE-EXECUTE LOOP COMPLETE ═══
Batches processed:       B
Total issues triaged:    N
  Actionable:            A
  Skipped:               S

Actionable issues processed:  A
  Completed:             C
  Failed:                F

Commits:
  <hash1> — <message>
  <hash2> — <message>
  ...
══════════════════════════════════════
```

If there are failed issues, also print:

```
Failed issues (labeled propose-execute-failed):
  #<number> — <title> — <failure reason>
  ...
```

---

## Guidelines

- **One issue at a time.** Do not parallelize execution — each
  `/propose-execute` may modify the codebase in ways that affect subsequent
  issues.
- **Triage can be parallel.** Triage agents are read-only, so multiple triage
  agents may run concurrently for efficiency (up to 10 at a time).
- **Clean state between issues.** Always ensure a clean working tree before
  starting the next issue. Revert any uncommitted changes from failures.
- **Pull between issues.** Run `git pull --rebase` before each issue to
  incorporate changes from the previous issue's push.
- **Label failures, don't retry.** If an issue fails, label it and move on.
  A human or future run (after removing the label) can retry.
- **Per-batch priority ordering.** Issues are sorted by priority within each
  batch. Batches are fetched in chronological order (oldest issues first).
  This means the oldest issues get processed first, with priority ordering
  within each batch.
- **Batch size controls resource usage.** The `--batch-size` flag controls
  how many issues are fetched and triaged at a time. Smaller batches reduce
  upfront triage cost and keep triage results fresh.
- **Self-assign before starting.** Assign yourself to the issue before
  invoking `/propose-execute` to prevent concurrent agents from picking
  the same issue.
- **Triage in sub-batches.** Within each fetched batch, triage at most 10
  issues concurrently. Wait for each triage sub-batch before launching the
  next.
- **Pass through flags.** All `--model`, `--max-proposal-rounds`, and
  `--max-review-rounds` flags are passed to each `/propose-execute`
  invocation.
- **The triage agent is the gatekeeper.** Only issues the triage agent
  deems ACTIONABLE get processed. This prevents wasting time on
  under-specified or discussion-only issues.
- **Do not over-filter.** The triage criteria are intentionally minimal:
  concrete, well-scoped, and clear benefit. Do not add extra criteria
  like time estimates, safety concerns, or complexity limits. Large
  refactors, parity-sensitive changes, and cross-crate API modifications
  are all fair game if they meet the three criteria.
- **Pagination is date-based.** The loop paginates using
  `created:>LAST_DATE` in the GitHub search query, advancing through
  issues in chronological order. This avoids re-fetching already-processed
  issues.
