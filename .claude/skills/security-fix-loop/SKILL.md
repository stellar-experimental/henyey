---
name: security-fix-loop
description: Process all open security audit issues in severity order — validate, test, fix, repeat
argument-hint: [--label <label>] [--dry-run]
---

Parse `$ARGUMENTS`:
- If `--label <label>` is present, set `$EXTRA_LABEL` to the value. Otherwise
  `$EXTRA_LABEL` is empty.
- If `--dry-run` is present, set `$DRY_RUN = true`. Otherwise `$DRY_RUN = false`.

# Security Fix Loop

Continuously pick the highest-severity open security audit issue, process it
with `/security-fix`, and repeat until no open issues remain.

## Step 1: Query Open Security Issues

Fetch all open audit issues:

```
gh issue list --label security,audit --state open --json number,title,labels --limit 500
```

If `$EXTRA_LABEL` is set, add it to the label filter:
```
gh issue list --label security,audit,$EXTRA_LABEL --state open --json number,title,labels --limit 500
```

### Severity Ordering

Parse the `labels` array for each issue to determine severity. Sort the full
list in this order:

1. **critical** — issues with the `critical` label
2. **high** — issues with the `high` label
3. **medium** — issues with the `medium` label
4. **low** — issues with the `low` label

Within the same severity tier, sort by issue number ascending (lowest number
first = oldest first).

If an issue has no recognized severity label, place it after `low`.

Store the sorted list as the **queue**.

## Step 2: Print the Queue

Print the queue as a table:

```
═══ SECURITY FIX QUEUE ═══
N issues to process

  #  | Issue | Severity | Title
-----|-------|----------|------
  1  | #28   | CRITICAL | [AUDIT-C1] Overlay auth bypass via...
  2  | #29   | CRITICAL | [AUDIT-C2] ...
  3  | #40   | HIGH     | [AUDIT-H1] ...
 ...
═══════════════════════════
```

If `$DRY_RUN = true`, **stop here**. Do not process any issues.

If the queue is empty, print:

```
═══ SECURITY FIX QUEUE ═══
No open security audit issues found.
═══════════════════════════
```

And **stop**.

## Step 3: Initialize Counters

```
total     = 0
fixed     = 0
false_pos = 0
already   = 0
skipped   = 0
```

## Step 4: Process Loop

Repeat until the queue is empty:

### 4a: Pick the Next Issue

Take the first issue from the queue (highest severity, lowest issue number).

Print a progress header:
```
═══ SECURITY FIX [<total + 1> / <queue_size_at_start>] ═══
Issue:    #<number>
Title:    <title>
Severity: <SEVERITY>
═══════════════════════════════════════════════════════════
```

### 4b: Invoke /security-fix

Run `/security-fix <issue-number>`.

This will do one of:
- **Close the issue as FIXED** — the code was changed, test written, committed,
  pushed, issue closed, and `/review-fix --apply` was run.
- **Close the issue as FALSE_POSITIVE** — the issue was not real, closed with
  an explanatory comment.
- **Close the issue as ALREADY_FIXED** — the issue was already addressed,
  closed with a comment citing the fix.
- **Fail** — could not complete the fix (test won't fail, fix breaks other
  tests, compilation errors, etc.).

### 4c: Classify the Outcome

After `/security-fix` returns, check the issue state:

```
gh issue view <number> --json state,labels
```

- If the issue is **closed**: Determine the reason from the closing comment:
  - Comment contains "Fixed in commit" → increment `fixed`
  - Comment contains "False Positive" → increment `false_pos`
  - Comment contains "Already Fixed" → increment `already`
- If the issue is still **open**: The fix failed. Handle per Step 4d.

Increment `total`.

### 4d: Handle Failure

If `/security-fix` did not close the issue (it's still open), this means the
fix could not be completed. Do the following:

1. **Revert any uncommitted changes** to prevent polluting the next fix:
   ```
   git checkout -- .
   git clean -fd
   ```

2. **Add the `needs-manual-review` label** to the issue:
   ```
   gh issue edit <number> --add-label needs-manual-review
   ```
   If the label doesn't exist yet, create it first:
   ```
   gh label create needs-manual-review --description "Security fix could not be automated" --color FBCA04
   ```

3. **Add a comment** documenting why it was skipped:
   ```
   gh issue comment <number> --body "$(cat <<'EOF'
   ## Automated Fix Skipped

   The `/security-fix` automation could not complete this fix.

   **Reason**: <brief explanation of what went wrong>

   This issue requires manual investigation and resolution.
   EOF
   )"
   ```

4. Increment `skipped`.

### 4e: Re-query Before Next Iteration

Before picking the next issue, re-query the open issues:

```
gh issue list --label security,audit --state open --json number,title,labels --limit 500
```

(Include `$EXTRA_LABEL` if set.)

This is necessary because:
- `/review-fix --apply` (invoked by `/security-fix`) may have found and fixed
  similar issues, closing additional GitHub issues.
- The issue we just processed is now closed (or labeled `needs-manual-review`
  and should be excluded from the queue).

Re-sort by severity and issue number. Filter out any issues labeled
`needs-manual-review` (they were already attempted and failed).

If the re-queried list is empty, exit the loop.

### 4f: Brief Progress Update

After each issue, print a one-line progress summary:

```
Progress: <total> processed (<fixed> fixed, <false_pos> false positive, <already> already fixed, <skipped> skipped) — <remaining> remaining
```

## Step 5: Completion Summary

After the loop exits (no more issues to process):

```
═══ SECURITY FIX LOOP COMPLETE ═══
Total processed:    <total>
  Fixed:            <fixed>
  False positive:   <false_pos>
  Already fixed:    <already>
  Skipped (manual): <skipped>

Remaining open:     <remaining>
═══════════════════════════════════
```

Where `<remaining>` is the count from a final query:
```
gh issue list --label security,audit --state open --json number --limit 500 | jq length
```

If `<remaining> > 0` and `<skipped> > 0`, also print:

```
Issues requiring manual review:
  #<number> — <title>
  #<number> — <title>
  ...
```

## Guidelines

- **One issue at a time.** Do not parallelize — each fix may affect the
  codebase in ways that impact subsequent fixes.
- **Re-query after each issue.** The queue is dynamic; `/review-fix --apply`
  may close related issues.
- **Clean state between issues.** Always revert uncommitted changes after a
  failure before proceeding.
- **Skip, don't block.** If a fix fails, label it and move on. Do not spend
  unlimited time retrying a single issue.
- **Severity first.** Always process the highest-severity remaining issue next.
  Critical vulnerabilities take priority over medium-severity code smells.
- **Track progress.** Use the TodoWrite tool to maintain a running task list
  of issues being processed.
- **Build once, test often.** `cargo test --all` runs at the end of each
  `/security-fix` invocation. If the full test suite is broken after a
  skipped issue's partial changes, the `git checkout -- .` in Step 4d
  restores a clean state.
