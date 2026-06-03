---
name: review-pr
description: |
  Run two parallel adversarial PR reviewers and combine their verdicts with
  external PR reviews (GH Copilot bot, humans, other bots) and CI state into a
  merge decision. Agent reviewers post structured comment verdicts (since the
  agent is the PR author and cannot self-approve via GH native review).
  External CHANGES_REQUESTED reviews block merge identically to agent
  CHANGES_REQUESTED. Operates on issues in `in-review`. Auto-merges with
  --admin on all-green (after filing follow-up issues for unaddressed inline
  review comments, so non-critical feedback is preserved as backlog instead of
  dropped); bounces to `ready-for-doing` on any request-changes or CI red;
  blocks after 3 bounce-backs on the same code state. At the lifetime cap
  (6 bounces since last `## Review: Reset`) the PR enters **force-converge
  mode**: if CI is green, the PR auto-merges and unresolved reviewer concerns
  become follow-up issues; only red/pending CI at the cap still blocks. Use
  when invoked by /project-tick with an issue in in-review, or manually as
  /review-pr <issue>.
model: gpt-5.4
---

# /review-pr <issue> — adversarial PR review

You are the PR-level gate. Three independent signals decide the merge: **reviewer A**, **reviewer B**, and **CI**. All three must be green to auto-merge. Any red bounces.

You do **not** review the code yourself — you orchestrate two independent reviewer sub-agents that use the existing `/review` and `/spec-adhere` skills (or `/review-fix` in review mode for risk-focused review), then combine their PR reviews with CI state.

## Inputs

- `$ISSUE` — issue number.
- The linked open PR (fetched from the issue).
- The PR's diff, CI run state, and any prior reviews.

## Step 0 — Find the PR

Source the shared merge helper for linked-PR state classification and merge execution:

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)"
source "$REPO_ROOT/scripts/lib/review-pr-merge.sh"
```

Classify the linked PR state (distinguishes OPEN, MERGED, and CLOSED-without-merge):

```bash
if ! PR_STATE=$(classify_linked_pr_state $ISSUE); then
  # API failure — retry once, then block
  sleep 5
  if ! PR_STATE=$(classify_linked_pr_state $ISSUE); then
    echo "## Review: API Failure" && exit 1
  fi
fi
```

Extract `PR_NUM` and route based on state:

```bash
case "$PR_STATE" in
  open:*)
    PR_NUM="${PR_STATE#open:}"
    # Proceed to Step 0.5
    ;;
  merged:*)
    PR_NUM="${PR_STATE#merged:}"
    # PR already merged — recover merge SHA and follow-up issues, then finalize
    MERGE_SHA=$(gh pr view "$PR_NUM" --repo stellar-experimental/henyey \
      --json mergeCommit --jq '.mergeCommit.oid')
    # Recover previously filed follow-up issue numbers from the armed comment
    FOLLOWUP_ISSUES=$(gh pr view "$PR_NUM" --repo stellar-experimental/henyey \
      --json comments --jq '
        [.comments[].body
         | select(contains("## Review: Auto-merge armed"))
         | capture("Follow-up issues filed:\\*\\* (?<nums>[^\n]+)"; "g") | .nums]
        | last // "none"')
    # Run Step 7.4 cleanup/done path with recovered metadata and exit
    ;;
  closed:*)
    # PR was closed without merge — bug in /do
    # Post ## Review: No PR Linked, move to ready-for-doing, unassign, exit
    ;;
  missing)
    # No linked PRs — same handling as closed
    ;;
esac
```

Handle each state:

- **`open:<number>`** → `PR_NUM` extracted above; proceed to Step 0.5.
- **`merged:<number>`** → the PR was already merged (e.g. by a previous `/review-pr` tick that armed auto-merge and GH completed it). Recover the merge SHA via `gh pr view <number> --json mergeCommit --jq '.mergeCommit.oid'` and follow-up issue numbers from the `## Review: Auto-merge armed` comment body. Run the Step 7.4 cleanup/done path and exit.
- **`closed:<number>`** → PR was closed without merge. That's a bug in `/do`. Post `## Review: No PR Linked` and move the issue back to `ready-for-doing`. Unassign. Exit.
- **`missing`** → no linked PRs at all. Same handling as `closed`.
- **`error:<message>`** → API failure. Retry once after 5s; if still failing, leave assigned and exit non-zero.

### Step 0.5 — Check if auto-merge is already armed

Before spawning reviewers or filing follow-up issues, check whether this PR already has auto-merge enabled from a previous tick:

```bash
if ! AUTO_MERGE_STATE=$(is_auto_merge_armed $PR_NUM); then
  # API failure checking auto-merge state — safe to proceed with normal review
  # since worst case is a redundant reviewer run. Log the issue.
  echo "Warning: could not check auto-merge state (API failure)" >&2
elif [[ "$AUTO_MERGE_STATE" == "true" ]]; then
  # PR is OPEN and auto-merge is armed. Before short-circuiting, verify that
  # CI is healthy — if CI went red or stuck while waiting, we must bounce or
  # block rather than wait indefinitely.
  ARMED_HEALTH=$(check_armed_pr_health $PR_NUM) || ARMED_HEALTH="error"
  case "$ARMED_HEALTH" in
    healthy)
      # CI green or still running within budget. Don't re-run reviewers or
      # re-file follow-up issues — just ensure a waiting comment exists and exit.
      ALREADY_COMMENTED=$(has_armed_waiting_comment $PR_NUM)
      if [[ "$ALREADY_COMMENTED" != "true" ]]; then
        gh pr comment $PR_NUM --repo stellar-experimental/henyey \
          --body "## Review: Auto-merge armed — waiting

Auto-merge was previously enabled on this PR. GitHub will merge automatically once all branch protection checks pass. Re-picking on next tick to verify completion.

CI state: healthy. Next check on next tick."
      fi
      gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
      exit 0
      ;;
    ci-red)
      # CI failed while auto-merge was armed. GitHub won't merge it. Cancel
      # auto-merge and bounce to /do for investigation.
      gh pr merge $PR_NUM --repo stellar-experimental/henyey --disable-auto
      gh pr comment $PR_NUM --repo stellar-experimental/henyey \
        --body "## Review: Auto-merge cancelled — CI red

Auto-merge was armed but CI has failures. Disabling auto-merge and bouncing back to \`/do\` for investigation."
      bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
      gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
      exit 0
      ;;
    ci-stuck)
      # CI running but exceeded wall-clock budget. Block.
      gh pr merge $PR_NUM --repo stellar-experimental/henyey --disable-auto
      gh pr comment $PR_NUM --repo stellar-experimental/henyey \
        --body "## Review: Auto-merge cancelled — CI stuck

Auto-merge was armed but CI has been running past the stuck threshold. Blocking for operator investigation."
      bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE blocked
      gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
      exit 0
      ;;
    not-mergeable)
      # PR has merge conflicts. Cancel auto-merge and bounce.
      gh pr merge $PR_NUM --repo stellar-experimental/henyey --disable-auto
      gh pr comment $PR_NUM --repo stellar-experimental/henyey \
        --body "## Review: Auto-merge cancelled — merge conflicts

Auto-merge was armed but the PR now has merge conflicts. Bouncing back to \`/do\` for rebase."
      bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
      gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
      exit 0
      ;;
    no-ci)
      # No CI checks detected — cannot treat as green. This may indicate a
      # workflow misconfiguration, disabled Actions, or permissions issue.
      # Block for operator investigation.
      gh pr merge $PR_NUM --repo stellar-experimental/henyey --disable-auto
      gh pr comment $PR_NUM --repo stellar-experimental/henyey \
        --body "## Review: Auto-merge cancelled — no CI detected

Auto-merge was armed but no CI checks are present on this PR. This may indicate a workflow misconfiguration or disabled Actions. Blocking for operator investigation."
      bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE blocked
      gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
      exit 0
      ;;
    error)
      # Could not check health — proceed with normal review as a safe fallback.
      echo "Warning: could not check armed PR health (API failure), proceeding with full review" >&2
      ;;
  esac
fi
```

This short-circuit prevents duplicate reviewer runs and duplicate follow-up issue filing on subsequent ticks while waiting for GitHub to complete the deferred merge. The `has_armed_waiting_comment` check ensures idempotency: if a waiting comment was already posted on a previous tick, no duplicate is created. Critically, it still checks CI state on every tick — if CI goes red or gets stuck while auto-merge is armed, the skill cancels auto-merge and routes the issue appropriately rather than waiting indefinitely.

## Step 1 — Read the PR + CI state

```bash
# PR metadata, files changed, current reviews.
gh pr view $PR_NUM --repo stellar-experimental/henyey \
  --json title,body,baseRefName,headRefName,reviews,files,statusCheckRollup,mergeable

# CI runs.
gh run list --repo stellar-experimental/henyey --branch <head-ref> --limit 5 \
  --json conclusion,status,name,databaseId
```

Capture:

- **Files changed** — for parity-vs-risk reviewer auto-detection.
- **Current CI status:** `green` | `failing` | `running` | `not_started`.
- **Existing reviews:** for bounce-back counting.

## Step 2 — Count prior bounce-backs (head-scoped + lifetime cap, both with Reset escape hatch)

Two caps run together:

1. **Head-scoped cap (3 bounces per code state):** old bounces against earlier commits don't carry forward — they represent failed merge attempts on code that has since been rebased or replaced. Fresh push naturally resets this counter.
2. **Lifetime cap (6 bounces since last Reset):** catches the runaway pattern where `/do` keeps pushing fresh commits and the head-scoped counter resets every cycle, allowing indefinite churn. Only a `## Review: Reset` resets this counter.

Compute the **head-scoped baseline timestamp**:

```bash
# When was the current PR head commit pushed/committed? Fresh push = new baseline.
HEAD_PUSHED_ISO=$(gh pr view $PR_NUM --repo stellar-experimental/henyey \
  --json commits --jq '.commits | sort_by(.committedDate) | last | .committedDate')

# Has the operator (or recovery script) posted a Reset marker?
# This is the escape hatch that resets BOTH caps when the cause has cleared.
RESET_AT_ISO=$(gh api repos/stellar-experimental/henyey/issues/$ISSUE/comments --paginate \
  --jq '[.[] | select(.body | startswith("## Review: Reset"))] | sort_by(.created_at) | last.created_at // ""')

# Convert to epoch seconds for unambiguous numeric comparison
# (ISO strings can drift in format — fractional seconds, +00:00 vs Z, etc.).
HEAD_PUSHED_EPOCH=$(date -u -d "$HEAD_PUSHED_ISO" +%s)
if [ -n "$RESET_AT_ISO" ]; then
  RESET_AT_EPOCH=$(date -u -d "$RESET_AT_ISO" +%s)
else
  RESET_AT_EPOCH=0
fi

# Head-scoped baseline = max(HEAD_PUSHED, RESET_AT). Lifetime baseline = RESET_AT only.
if [ "$RESET_AT_EPOCH" -gt "$HEAD_PUSHED_EPOCH" ]; then
  HEAD_BASELINE_EPOCH=$RESET_AT_EPOCH
else
  HEAD_BASELINE_EPOCH=$HEAD_PUSHED_EPOCH
fi
LIFETIME_BASELINE_EPOCH=$RESET_AT_EPOCH
```

Then count bounce comments for each cap (jq's `fromdate` parses ISO-8601 into epoch seconds, so the comparison is numeric, not lexical):

```bash
ALL_BOUNCES_JSON=$(gh api repos/stellar-experimental/henyey/issues/$ISSUE/comments --paginate \
  --jq "[.[] | select(.body | startswith(\"## Review: Bounce-Back Cycle\")) | .created_at]")

HEAD_COUNT=$(echo "$ALL_BOUNCES_JSON" | jq --argjson base "$HEAD_BASELINE_EPOCH" \
  '[.[] | select((. | fromdate) > $base)] | length')

LIFETIME_COUNT=$(echo "$ALL_BOUNCES_JSON" | jq --argjson base "$LIFETIME_BASELINE_EPOCH" \
  '[.[] | select((. | fromdate) > $base)] | length')
```

**Cap checks (apply in order):**

- If `LIFETIME_COUNT >= 6`, the PR has cycled too many times across multiple code states. Enter **force-converge mode**: `export FORCE_CONVERGE=1` and continue with the normal review flow. The matrix in Step 6 will, if CI is green, route to a force-merge that files follow-up issues for any remaining CHANGES_REQUESTED concerns. If CI is still red/pending at the lifetime cap, the matrix routes to `blocked` (force-converge cannot merge unsafe code). This replaces the older "always block at lifetime cap" rule, which created a graveyard of PRs nobody could land.
- Otherwise, if `HEAD_COUNT >= 3`, this is the 4th cycle on the current code. Post `## Review: Cycle Cap Reached`, move to `blocked`, unassign, and exit. (Head-scoped cap still blocks; force-converge applies only to the lifetime cap.)
- Otherwise (both under cap) → proceed with the review.

**Recovery semantics:**

- **Fresh `/do` Mode B push** → new commit `committedDate` advances the head-scoped baseline → head counter resets to 0. Lifetime counter is **unaffected** — it keeps climbing across pushes until a Reset.
- **`## Review: Reset` comment** → manual escape hatch that resets **both** counters. Operator (or recovery script) posts this when the underlying cause has cleared (broken main, flaky CI, reviewer disagreement resolved out-of-band). Everything before the Reset comment is excluded from both counts. Post format:
  ```markdown
  ## Review: Reset

  <one-line reason — e.g. "Quickstart on main was broken; now green. Resetting bounce counter so this PR can re-attempt.">
  ```
- **Old bounce comments aren't deleted** — they remain in the audit trail. They just don't count against the current attempt.

## Step 3 — Auto-detect the second reviewer's lens

Inspect the PR's changed files:

```bash
gh pr diff $PR_NUM --repo stellar-experimental/henyey --name-only
```

If any path matches one of these prefixes, the PR is **parity-critical** — Reviewer B uses parity lens:

- `crates/scp/`
- `crates/herder/`
- `crates/ledger/`
- `crates/tx/`
- `crates/overlay/`

Otherwise, the PR is **non-parity** — Reviewer B uses risk lens.

## Step 4 — Spawn 2 reviewers in parallel

Launch both as `general-purpose` foreground sub-agents. Do not wait between them. **Each reviewer must be spawned with `--model gpt-5.4`** (or equivalent model parameter) explicitly — do not inherit from the parent. Cross-model diversity catches issues a same-model pipeline would miss.

**Workspace binding (issue #2843 — pass into every reviewer prompt).** Before spawning, run the Workspace-contract bootstrap to derive `$REVIEWER_WORKTREE` (under `~/data/$SESSION_ID/review-pr-$ISSUE/reviewer`). Pass that exact path into each reviewer prompt and require it as the ONLY scratch location:

> Your working directory and any checkout/build scratch MUST be `$REVIEWER_WORKTREE` (`<resolved-path>`). If you need a git worktree, run `git worktree add "$REVIEWER_WORKTREE/<sub>"`; if you build, set `CARGO_TARGET_DIR="$REVIEWER_WORKTREE/cargo-target"`. You are FORBIDDEN from creating `.review-data/`, `.review-worktrees/`, `.worktrees/`, `.copilot-tmp/`, `.opencode/worktrees/`, any `<repo>-pr<N>` sibling, or any path under `/tmp`. These are the disk-leak patterns from #2843 and have repeatedly filled the root FS.

**Why structured comments, not `gh pr review --approve`:** the authenticated GH user is the PR author (the same user opened the PR via `/do` and now reviews it). GitHub disallows author self-approval, so `gh pr review --approve` is silently downgraded to a comment by `gh`. Instead, each reviewer posts a structured comment with a verdict marker that `/review-pr` parses in Step 6.

**Verdict comment format** — each reviewer MUST post exactly one comment with this exact shape:

```markdown
## 🔍 Reviewer: <Correctness|Parity|Risk>

**Verdict:** APPROVE | CHANGES_REQUESTED

**Summary:** <one or two lines>

<details>
<summary>Full review</summary>

<bulleted list of concerns, inline references, alternate approaches.
For CHANGES_REQUESTED, list every specific change `/do` Mode B should make.
Keep under 400 lines.>
</details>
```

Post via `gh pr comment $PR_NUM --repo stellar-experimental/henyey --body-file <tmpfile>` so multi-line bodies survive intact. Reviewers MAY also post inline line comments via `gh api repos/.../pulls/$PR_NUM/comments` for specific concerns — those don't count toward the verdict; only the top-level structured comment does.

**Inline-comment convention:** if a reviewer's concern is non-blocking (would be a `MINOR` note), they should APPROVE at the top level AND leave the concern as an inline comment. `/review-pr` will auto-file a follow-up issue for every unaddressed inline comment at merge time (see Step 7.2), so non-critical feedback is preserved as actionable backlog without blocking the merge. If a concern is blocking, use `**Verdict:** CHANGES_REQUESTED` at the top level; `/do` Mode B will address every inline in that case.

**Concern-class discipline (kills whack-a-mole bouncing):** Every concern in a CHANGES_REQUESTED verdict MUST be labeled with a **concern class** — a 1–3 word category like `test-coverage`, `workspace-contract`, `parity-gap`, `regression-risk`, `doc-drift`, `error-handling`, `api-shape`, `ci-failure`. The reviewer prompts below enforce two rules:

- **Cycle 1** (no prior `## 🔍 Reviewer: <YourName>` comment on this PR): produce a COMPLETE change-list. Don't hold concerns back for later cycles. Every concern is labeled with its class. The implicit contract: a class not raised in cycle 1 should not be raised in cycle 2+.
- **Cycle N≥2** (prior reviewer comments exist): you read your latest prior verdict, enumerate the classes you previously raised, and stick to them. Verify each prior concern is addressed; you may add new specific bullets within an existing class. If you genuinely identify a concern in a class you did NOT previously raise, you MUST flag the section with `**NEW CLASS DISCOVERED:** <class-name>` and explain why cycle 1 missed it. This becomes audit-trail evidence that cycle 1 was incomplete and signals the orchestrator/operator that the PR may be larger in scope than was understood. (It still counts as a normal bounce — the goal is transparency, not blocking.)

### Reviewer A — Correctness (always)

> Invoke /review on PR #$PR_NUM in stellar-experimental/henyey. Focus on:
> correctness of the diff, test coverage, readability, error handling.
>
> **Cycle-awareness (kills whack-a-mole bouncing) — do this BEFORE writing
> your review:**
>
> 1. Fetch your own prior comments on this PR:
>    \`\`\`bash
>    gh pr view $PR_NUM --repo stellar-experimental/henyey --comments \\
>      --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Correctness")) | .body'
>    \`\`\`
> 2. If empty → this is **cycle 1**. Produce a COMPLETE change-list. Every
>    concern must be labeled with a **concern class** (a 1–3 word category,
>    e.g. \`test-coverage\`, \`workspace-contract\`, \`regression-risk\`,
>    \`error-handling\`, \`api-shape\`, \`ci-failure\`). Group concerns by class
>    in the verdict body. Don't hold concerns back hoping the doer figures
>    them out — your cycle-1 list is the contract for the whole review arc.
> 3. If non-empty → this is **cycle N≥2**. Read your latest prior verdict.
>    Enumerate the classes you raised. This cycle, you must:
>    - Verify each prior concern is addressed (re-evaluating within the same
>      class is fine — e.g. if you raised \`test-coverage\` and the test was
>      added but is wrong, that's still \`test-coverage\`).
>    - Stick to those classes. New specific bullets within an existing class
>      are fine.
>    - If you genuinely identify a concern in a class you did NOT raise
>      previously, you MAY add it, but you MUST flag the section
>      \`**NEW CLASS DISCOVERED:** <class-name>\` and explain in 1–2 sentences
>      why cycle 1 missed it. (This still counts as a normal bounce; the
>      flag is audit-trail evidence, not a block.)
>
> **Test verification (REQUEST_CHANGES if any of these fails):**
>
> **Test verification (REQUEST_CHANGES if any of these fails):**
>
> 1. Find the linked issue's `kind:` from its `## Triage Report` comment.
> 2. For `kind: bug-fix`:
>    - The PR must include a regression test. Find it by reading the PR body's
>      `## Regression test` section (which /do should have populated).
>    - **Verify the regression test would have caught the bug.** Walk the PR
>      commit list (\`gh pr view $PR_NUM --json commits\`). The test should
>      have been committed BEFORE the fix. Check out the parent of the fix
>      commit and run the test:
>      \`\`\`bash
>      git fetch origin pull/$PR_NUM/head:pr-$PR_NUM
>      git checkout <test-commit-sha>
>      cargo test -p henyey-<crate> <test_fn> 2>&1 | tail -10
>      \`\`\`
>      Confirm the test FAILS at that point. If the test passes at the test-
>      commit, the regression test doesn't actually capture the bug → bounce.
>    - If the PR body has no \`## Regression test\` section, or the section's
>      claims don't match what's in the diff, → bounce.
> 3. For `kind: feature`:
>    - Every new public function in the diff (search for new \`pub fn\`,
>      \`pub struct\`, etc. lines) must have at least one test exercising it.
>      Use \`gh pr diff $PR_NUM\` and grep for new public surface. Cross-check
>      against the test files in the diff. Untested new public surface →
>      bounce.
> 4. For `kind: refactor` / `docs` / `test-only`: existing tests must still
>    pass (CI will catch this) and the plan's "Existing tests preserved" list
>    must all be green in CI.
>
> Then evaluate logic, error handling, readability per usual.
>
> Post your verdict as a single PR-level comment using \`gh pr comment\`,
> headed \`## 🔍 Reviewer: Correctness\`, with \`**Verdict:** APPROVE\` or
> \`**Verdict:** CHANGES_REQUESTED\` on its own line. Inline line comments
> via \`gh api\` are welcome for specific concerns.

### Reviewer B — Parity OR Risk (auto-detected)

**If parity-critical:**

> Invoke /spec-adhere style audit on PR #$PR_NUM in stellar-experimental/henyey.
> Focus on: does the change match stellar-core's behavior on this path?
> Consult the `stellar-core/` submodule for the matching C++ implementation.
> Identify any divergence in semantics, edge cases, or sequencing. Post your
> verdict as a single PR-level comment via `gh pr comment`, headed
> `## 🔍 Reviewer: Parity`, with `**Verdict:**` on its own line. Reviewer A
> is doing correctness; you focus only on parity.
>
> **Cycle-awareness (same discipline as Reviewer A):** Before writing your
> review, fetch your own prior comments via
> \`gh pr view $PR_NUM --comments --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Parity")) | .body'\`.
> If empty → cycle 1: produce a complete change-list with every concern
> labeled by a concern class (e.g. \`parity-gap\`, \`spec-divergence\`,
> \`sequencing\`, \`edge-case\`). If non-empty → cycle N≥2: stick to the
> classes you raised before; only add a new class with an explicit
> \`**NEW CLASS DISCOVERED:**\` flag and rationale. This stops the
> parity-vs-correctness bounce-orbit that historically pushed PRs into
> the lifetime cap.

**If non-parity (risk lens):**

> Review PR #$PR_NUM in stellar-experimental/henyey for risk: regressions in
> existing behavior, performance impact, breaking changes to APIs or data
> formats, security implications, operational concerns (config, migrations).
> Reviewer A is doing correctness; you focus only on risk. Post your verdict
> as a single PR-level comment via `gh pr comment`, headed
> `## 🔍 Reviewer: Risk`, with `**Verdict:**` on its own line.
>
> **Cycle-awareness (same discipline as Reviewer A):** Before writing your
> review, fetch your own prior comments via
> \`gh pr view $PR_NUM --comments --json comments --jq '.comments[] | select(.body | startswith("## 🔍 Reviewer: Risk")) | .body'\`.
> Cycle 1 → complete change-list, concerns labeled by class
> (e.g. \`regression-risk\`, \`perf-regression\`, \`api-break\`,
> \`migration-risk\`, \`config-risk\`). Cycle N≥2 → stick to your
> previously-raised classes; only add a new class with an explicit
> \`**NEW CLASS DISCOVERED:**\` flag and rationale.

Wait for both reviewers to post.

## Step 5 — Recheck CI

Reviewers run in parallel with CI. By the time both have posted their reviews, CI may have finished. Re-query:

```bash
ROLLUP=$(gh pr view $PR_NUM --repo stellar-experimental/henyey \
  --json statusCheckRollup --jq '.statusCheckRollup')
CI_TOTAL=$(echo "$ROLLUP" | jq 'length')
```

CI state buckets — apply in this order:

- **Empty rollup** (`CI_TOTAL == 0`): the PR has NO CI runs at all. Could be a misconfigured workflow file, a fork PR with workflows gated, or a workflow_dispatch-only repo. **NEVER classify this as green.** Block with `## Review: No CI Detected` and a note that the operator needs to investigate why CI didn't trigger.
- **Red** — at least one entry has `conclusion: FAILURE | CANCELLED | TIMED_OUT` (or `state: FAILURE | ERROR` for StatusContext).
- **Running** — entries exist, none failed, but at least one is `status != COMPLETED` (or for StatusContext: `state == PENDING`).
- **Green** — `CI_TOTAL > 0` AND every entry has `conclusion: SUCCESS | SKIPPED | NEUTRAL` (or `state: SUCCESS` for StatusContext). Requires positive evidence of completion, never vacuous.

```bash
# Classification (apply top-down):
if [ "$CI_TOTAL" -eq 0 ]; then
  CI_STATE="empty"
elif [ "$(echo "$ROLLUP" | jq '[.[] | select(
       ((.conclusion // "") | ascii_upcase) as $c |
       $c == "FAILURE" or $c == "CANCELLED" or $c == "TIMED_OUT"
       or ((.state // "") | ascii_upcase) as $s | $s == "FAILURE" or $s == "ERROR"
     )] | length')" -gt 0 ]; then
  CI_STATE="red"
elif [ "$(echo "$ROLLUP" | jq '[.[] | select(
       (.status != null and (.status | ascii_upcase) != "COMPLETED")
       or (.status == null and (.state | ascii_upcase) == "PENDING")
     )] | length')" -gt 0 ]; then
  CI_STATE="running"
elif [ "$(echo "$ROLLUP" | jq '[.[] | select(
       ((.conclusion // "") | ascii_upcase) as $c |
       $c == "SUCCESS" or $c == "SKIPPED" or $c == "NEUTRAL"
       or ((.state // "") | ascii_upcase) == "SUCCESS"
     )] | length')" -eq "$CI_TOTAL" ]; then
  CI_STATE="green"
else
  # Unexpected conclusions (ACTION_REQUIRED, STARTUP_FAILURE, etc.) — treat as red
  CI_STATE="red"
fi
```

## Step 6 — Decide

### 6.1 Parse the reviewer verdicts from PR comments

Fetch all PR-level comments and find the latest one matching each reviewer header. Use the most recent comment per reviewer (in case a reviewer posted, then re-posted):

```bash
gh api repos/stellar-experimental/henyey/issues/$PR_NUM/comments \
  --paginate --jq '.[] | select(.body | startswith("## 🔍 Reviewer:")) |
                   {created_at, body}' | jq -s 'sort_by(.created_at)'
```

For each comment, extract the reviewer name from the first line (`## 🔍 Reviewer: Correctness` etc.) and the verdict from the `**Verdict:**` line. Keep only the LATEST comment per reviewer name. The two reviewers we expect:

- `Correctness` (always)
- `Parity` or `Risk` (depending on parity-critical detection from Step 3)

If only one verdict is found, **treat the missing one as `CHANGES_REQUESTED` and bounce.** A missing verdict means a reviewer sub-agent failed to post — that's the same failure mode as the "Reviewer sub-agent fails to post" entry in the failure-handling table below. Do NOT wait indefinitely on a missing verdict; bounce so `/do` Mode B can retry, and the next `/review-pr` cycle will spawn fresh reviewers. If both are present, use them. If a reviewer posted twice (e.g. revised verdict), the latest comment wins.

### 6.1b Parse external reviewer verdicts (GH Copilot bot, humans, other bots)

The agent reviewers post structured PR comments (Step 4); but GitHub also tracks native PR reviews from anyone else — GH's Copilot auto-reviewer, human reviewers, third-party bots. Fetch them too and include in the matrix:

```bash
ME=$(gh api user --jq .login)

# Get each distinct external reviewer's LATEST review state. Exclude the
# agent's own user since gh would have downgraded any --approve to
# COMMENTED (and the agent uses structured comments for its real verdicts).
gh api repos/stellar-experimental/henyey/pulls/$PR_NUM/reviews \
  --paginate --jq --arg me "$ME" '
    [.[] | select(.user.login != $me)] |
    group_by(.user.login) |
    map(max_by(.submitted_at) | {user: .user.login, state, body_head: ((.body // "") | split("\n")[0])})
  '
```

Each external reviewer's verdict is the `state` of their latest review:

- `APPROVED` → external approve (nice-to-have, doesn't gate by itself)
- `CHANGES_REQUESTED` → blocker, treated identically to an agent reviewer's CHANGES_REQUESTED
- `COMMENTED` → neutral (notes; doesn't gate). The body still gets captured at merge time via the inline-comment follow-up logic in Step 7.

### 6.2 Combine all signals

Required signals — both must be APPROVE or the matrix bounces / waits:

- **Reviewer A verdict** (agent, Correctness): APPROVE / CHANGES_REQUESTED / pending.
- **Reviewer B verdict** (agent, Parity-or-Risk): APPROVE / CHANGES_REQUESTED / pending.

Additional gate — any external CHANGES_REQUESTED is a blocker:

- **External reviewer states**: union over all non-agent reviewers' latest states. If ANY is `CHANGES_REQUESTED` → bounce as if an agent had CHANGES_REQUESTED. External `APPROVED` and `COMMENTED` are non-blocking.

CI state — `empty` / `green` / `red` / `running` (per the bucket rules in Step 5).

Apply the outcome matrix (top-to-bottom, first match wins):

### Force-converge override (lifetime cap reached, CI green)

If Step 2 set `FORCE_CONVERGE=1` (lifetime bounce cap was reached), the matrix is short-circuited:

| FORCE_CONVERGE | CI state | Action |
|---|---|---|
| 1 | `green` | **Force-merge** via Step 7-bis. File one follow-up issue per CHANGES_REQUESTED concern bullet from the latest reviewer comments; merge with `--admin`; post `## ⚠️ Force-Converged (Lifetime Cap)`. |
| 1 | `running` | Wait (`## Review: Waiting on CI` — force-converge still needs green CI). Re-pick next tick. |
| 1 | `red` / `empty` | **Block.** Post `## Review: Lifetime Cap + Unsafe CI` (force-converge cannot merge red/missing CI; this requires operator). |

The reviewer-verdict outputs are still used (to produce the follow-up list), but they no longer gate the merge. This is intentional: at the lifetime cap, the reviewer↔doer loop has demonstrated it cannot converge, so we drain the PR with CI as the sole final safety net and preserve the unresolved concerns as backlog.

If `FORCE_CONVERGE` is unset, fall through to the normal matrix below.

### Block immediately on suspicious CI state

| CI state | Action |
|---|---|
| `empty` (zero rollup entries — no CI ever started) | **Block.** Post `## Review: No CI Detected` explaining the situation; move to `blocked`. Operator investigates whether the workflow file is broken or whether this is a fork-PR / dispatch-only scenario. Never auto-merge without positive CI evidence. |

### Auto-merge (triple-green)

| A | B | CI | Action |
|---|---|---|---|
| APPROVE | APPROVE | `green` | Auto-merge (see Step 7) |

### Wait (re-pick next tick)

| A | B | CI | Action |
|---|---|---|---|
| APPROVE | APPROVE | running | Wait. Comment `## Review: Waiting on CI`. Unassign so next tick re-picks. |
| (other waiting cases below) | | | |

### Bounce (PR has issues to address)

| A | B | External | CI | Action |
|---|---|---|---|---|
| CHANGES_REQUESTED | (any) | (any) | (any) | Bounce — A has concerns. |
| (any) | CHANGES_REQUESTED | (any) | (any) | Bounce — B has concerns. |
| (any) | (any) | any CHANGES_REQUESTED | (any) | Bounce — external reviewer (bot or human) has concerns. |
| APPROVE | APPROVE | none CHANGES_REQUESTED | red (diff-attributable) | Bounce — CI is a reviewer too. |

For diff-attributable vs. unrelated CI red, inspect the failing check's logs:

```bash
gh run view <run-id> --log-failed
```

If failures reference code in the PR's diff → diff-attributable. If failures look upstream (e.g. a shared dependency, an unrelated test on `main`) → unrelated.

**Unrelated CI red:**

| A | B | CI | Action |
|---|---|---|---|
| APPROVE | APPROVE | red (unrelated) | Bounce with note. `/do` will rebase on `origin/main` and retry. If still red after rebase, the next `/review-pr` will mark `blocked`. |

### Block (cycle cap or CI genuinely stuck)

| A | B | CI | Action |
|---|---|---|---|
| APPROVE | APPROVE | running > **CI_STUCK_AFTER_MINUTES** (default 60) wall-clock since the oldest in-progress check started | `blocked` — CI is genuinely stuck (see Step 6.0 for the wall-clock check). |
| (any) | (any) | (any) | If this would be the 4th bounce → `blocked` (handled at Step 2). |

**Why wall-clock, not tick count:** henyey integration tests routinely take 25+ minutes. A tick-count threshold (e.g. "3 ticks") is too aggressive when the tick rate is faster than CI completion — it produces false-positive blocks on perfectly healthy slow CI. Use the actual start time of the oldest still-in-progress CI check; only block if the run has exceeded a generous wall-clock budget.

#### Step 6.0 — Compute CI age before applying the matrix

If CI is in the **running** bucket (Step 5), determine its wall-clock age via the oldest in-progress check's `startedAt`:

```bash
HEAD_REF=$(gh pr view $PR_NUM --repo stellar-experimental/henyey --json headRefName --jq '.headRefName')

CI_OLDEST_START=$(gh run list --repo stellar-experimental/henyey \
  --branch "$HEAD_REF" --limit 20 \
  --json startedAt,conclusion,status \
  --jq '[.[] | select(.status != "completed")] | min_by(.startedAt) | .startedAt // ""')

CI_AGE_MIN=0
if [ -n "$CI_OLDEST_START" ]; then
  NOW_EPOCH=$(date -u +%s)
  CI_START_EPOCH=$(date -u -d "$CI_OLDEST_START" +%s)
  CI_AGE_MIN=$(( (NOW_EPOCH - CI_START_EPOCH) / 60 ))
fi

# Default budget. Override with CI_STUCK_AFTER_MINUTES env var if running ops
# tests for short-budget environments. Production budget is 60 min.
CI_STUCK_AFTER_MINUTES="${CI_STUCK_AFTER_MINUTES:-60}"
```

Apply to the matrix: if `CI_AGE_MIN > CI_STUCK_AFTER_MINUTES` AND both reviewers APPROVE AND CI is still running → block. Otherwise → wait (re-pick next tick).

## Step 7 — Execute the decision

### Auto-merge path

The pipeline gates merges on its OWN signals (parsed verdicts + CI state), not GitHub's review-approval count. The reviewer comments are advisory metadata only — GitHub does not "know" they're approvals, because GitHub disallows author self-approval. So we merge with `--admin` to bypass GitHub's review-required gate (CI gates still apply via branch protection if configured).

**Before merging**, file follow-up issues for any UNADDRESSED inline (line-level) review comments. Reviewers can flag non-critical concerns inline without blocking the merge with `CHANGES_REQUESTED` — those concerns must be preserved as actionable backlog, not lost.

#### 7.1 Identify unaddressed inline comments

Fetch all inline-review threads via GraphQL (REST doesn't expose `isResolved`):

```bash
gh api graphql -f query='
query($owner: String!, $repo: String!, $pr: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 50) {
            nodes {
              databaseId
              author { login }
              body
              path
              line
              url
            }
          }
        }
      }
    }
  }
}' -f owner=stellar-experimental -f repo=henyey -F pr=$PR_NUM \
  --jq '.data.repository.pullRequest.reviewThreads.nodes'
```

For each thread, classify it as **addressed** if any of:

- `isResolved == true` (GitHub-side thread resolution).
- The thread has at least one reply after the original comment AND the reply body contains any of: `Addressed in `, `Fixed in `, `Done in `, or a `<commit-sha>` reference.

Otherwise, classify as **unaddressed**.

#### 7.2 File one follow-up issue per unaddressed thread

For each unaddressed thread, create an issue:

```bash
gh issue create --repo stellar-experimental/henyey \
  --title "<short summary derived from first line of comment body, ≤80 chars>" \
  --body "$(cat <<EOF
Follow-up from PR #$PR_NUM (issue #$ISSUE). Non-critical inline review comment that was not addressed before merge.

## Concern

\`<path>:<line>\` — <full original comment body, indented or as-is>

## Source

[Original review comment](<thread.comments.nodes[0].url>) on PR #$PR_NUM.

## Severity

Low / non-blocking — reviewer chose to APPROVE rather than request changes. Filing as backlog so the concern isn't lost.
EOF
)" \
  --label "enhancement,low,follow-up"
```

If the comment's `path` matches a known crate prefix (`crates/<name>/...`), also add the `crate:<name>` label.

Collect the list of newly-filed issue numbers — they'll be referenced in the merge comment.

#### 7.3 Merge

Use the shared merge helper which handles the deferred-merge path. The helper
requires `REVIEW_PR_SCRATCH_DIR` to be set to a session-scoped directory under
the workspace contract (all scratch state under `~/data/$SESSION_ID/review-pr-$ISSUE/`):

```bash
# Derive scratch dir from the workspace contract. review_pr_bootstrap exports
# WORKTREE_BASE = ~/data/$SESSION_ID/review-pr-$ISSUE — reuse it for merge scratch.
export REVIEW_PR_SCRATCH_DIR="${WORKTREE_BASE:-$(eval echo "~$(id -un)")/data/${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}/review-pr-$ISSUE}"
mkdir -p "$REVIEW_PR_SCRATCH_DIR"

if ! MERGE_RESULT=$(attempt_merge $PR_NUM); then
  MERGE_RESULT="hard-failure:attempt_merge returned nonzero"
fi
```

Handle the result:

- **`merged`** → immediate merge succeeded. Proceed to Step 7.4 cleanup.
- **`auto-merge-armed`** → GitHub cannot create the merge commit cleanly right now but will merge automatically once conditions are met. Post a waiting comment recording the follow-up issue numbers filed in Step 7.2:

  ```markdown
  ## Review: Auto-merge armed

  Enabled deferred auto-merge (squash). GitHub will merge once branch protection checks pass.

  **Follow-up issues filed:** #N1, #N2 (or "none")

  Re-picking on next tick to verify merge completion.
  ```

  Unassign and exit. The next `/review-pr` tick will detect the `MERGED` state via `classify_linked_pr_state` and run cleanup, or detect `OPEN + armed` via `is_auto_merge_armed` and short-circuit to waiting.

- **`hard-failure:<stderr>`** → merge failed for a reason other than the auto-merge hint. Leave the issue in `in-review`; file a follow-up issue documenting the merge-permission gap; do NOT downgrade to non-admin merge that might silently bypass CI. Post `## Review: Merge Failed` with the stderr content.

#### 7.4 Clean up

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE done
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me

REPO_ROOT="$(git rev-parse --show-toplevel)"

# Reap the /do session workspace (worktree + cargo target — 25-50 GB per issue).
# /do persists the fully-resolved ABSOLUTE workspace path in a flat, per-issue,
# session-independent marker under <real-home>/data/do-$ISSUE.workspace (see
# do/SKILL.md A.2). We reap that exact string verbatim — no recombination from a
# session id or $HOME — so the write path and the read path cannot diverge
# (#2979, #2978).
#
# Boundary guard: only `rm -rf` paths under <real-home>/data, where <real-home>
# is the PASSWD-derived home (NOT $HOME — $HOME may be poisoned). This fails
# closed if a marker ever contains a path outside the contract boundary.
PW_HOME="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
[ -z "$PW_HOME" ] && PW_HOME="$(eval echo "~$(id -un)")"

reap_workspace() {  # $1 = absolute dir to remove
  local ws="$1"
  [ -z "$ws" ] && return 0
  # Canonicalize BEFORE the boundary prefix check (defense-in-depth, #2990).
  # A raw string-prefix `case` would accept a marker like "$PW_HOME/data/../etc"
  # — it begins with "$PW_HOME/data/" yet resolves outside the boundary. We
  # resolve `..`/symlinks first (realpath -m, then a Python fallback at known
  # absolute paths so PATH poisoning can't substitute realpath), and compare the
  # RESOLVED path against the RESOLVED boundary so the read side is independently
  # safe even if a malformed marker is ever introduced.
  local canon home_data
  canon="$(realpath -m "$ws" 2>/dev/null)" || canon=""
  home_data="$(realpath -m "$PW_HOME/data" 2>/dev/null)" || home_data=""
  if [ -z "$canon" ] || [ -z "$home_data" ]; then
    local py
    for py in /usr/bin/python3 /usr/local/bin/python3 /bin/python3; do
      [ -x "$py" ] || continue
      canon="$("$py" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$ws" 2>/dev/null)"
      home_data="$("$py" -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$PW_HOME/data" 2>/dev/null)"
      break
    done
  fi
  if [ -z "$canon" ] || [ -z "$home_data" ]; then
    echo "WARN: refusing to reap '$ws' — could not canonicalize for boundary check" >&2
    return 0
  fi
  # Directory-boundary check on the RESOLVED paths: accept exact <home>/data or
  # paths strictly under it. Rejects siblings like ~/data-evil and ../ escapes.
  case "$canon" in
    "$home_data" | "$home_data"/*) [ -d "$canon" ] && rm -rf "$canon" ;;
    *) echo "WARN: refusing to reap '$ws' — resolves to '$canon', outside '$home_data'" >&2 ;;
  esac
}

WORKSPACE_MARKER="$PW_HOME/data/do-$ISSUE.workspace"
if [ -f "$WORKSPACE_MARKER" ]; then
  # Current marker: contents are the resolved absolute workspace dir. Reap it
  # verbatim, then remove the marker itself.
  reap_workspace "$(cat "$WORKSPACE_MARKER")"
  rm -f "$WORKSPACE_MARKER"
elif [ -f "$REPO_ROOT/data/do-$ISSUE/.session-id" ]; then
  # Legacy marker (pre-#2979 /do wrote a bare session id into the repo tree).
  # Reconstruct <real-home>/data/<id>/do-$ISSUE using the SAME passwd-home
  # boundary guard (NOT $HOME) before any rm -rf. Handles in-flight PRs opened
  # by the pre-fix /do.
  SESSION_ID=$(cat "$REPO_ROOT/data/do-$ISSUE/.session-id")
  [ -n "$SESSION_ID" ] && reap_workspace "$PW_HOME/data/$SESSION_ID/do-$ISSUE"
fi

# Repo-tree cleanup: legacy marker dir (if any) + any pruned worktree refs.
rm -rf "$REPO_ROOT/data/do-$ISSUE"
git worktree prune
```

Post a `## ✅ Merged` comment with the merge commit SHA AND the list of follow-up issues filed:

```markdown
## ✅ Merged

**Commit:** <merge-commit-sha>

**Follow-up issues filed for unaddressed inline review comments:** #N1, #N2

(none if all inline comments were addressed)
```

Exit.

## Step 7-bis — Force-converge merge (lifetime cap, CI green)

Only entered if Step 6's force-converge override fired. The flow is parallel to Step 7 but takes its follow-up source from the **top-level reviewer CHANGES_REQUESTED concern bullets** instead of (or in addition to) unaddressed inline comments. CI is green by precondition of this branch.

#### 7-bis.1 Collect unresolved concern bullets

Parse the latest `## 🔍 Reviewer: Correctness` and `## 🔍 Reviewer: <Parity|Risk>` PR comments. If a reviewer's verdict is `CHANGES_REQUESTED`, extract each bulleted concern from the `<details><summary>Full review</summary>` block. Group by the `class:` label on each bullet (or "(unlabeled)" if missing — older comments may predate the discipline). External reviewer CHANGES_REQUESTED bodies are processed the same way.

Also fetch unaddressed inline comments via the Step 7.1 mechanism — those still file follow-ups too.

#### 7-bis.2 File one follow-up issue per unresolved concern bullet

For each concern bullet:

```bash
gh issue create --repo stellar-experimental/henyey \
  --title "<class>: <short summary derived from concern bullet, ≤80 chars>" \
  --body "$(cat <<EOF
Follow-up from PR #$PR_NUM (issue #$ISSUE). PR was force-converged at the lifetime bounce cap; this concern was raised by a reviewer but not resolved before merge.

## Concern class

\`<class-name>\` (from reviewer cycle-1 change-list discipline)

## Source

[Reviewer comment](<link-to-reviewer-PR-comment>) on PR #$PR_NUM.

Reviewer: <Correctness|Parity|Risk|external-username>
Verdict: CHANGES_REQUESTED

## Detail

<full concern bullet body — quote the reviewer's text verbatim>

## Why this is a follow-up, not a merge-blocker

PR #$PR_NUM reached the lifetime bounce cap (6 cycles since last \`## Review: Reset\`) without converging. Per /review-pr force-converge policy, CI being green is sufficient to land the change and preserve unresolved concerns as backlog. Operator should triage this issue — close as won't-fix if the reviewer was over-strict, or schedule a follow-up PR if the concern is real.
EOF
)" \
  --label "follow-up,force-converge"
```

Also apply any `crate:<name>` label inferred from file paths in the concern text.

Collect the new issue numbers; they'll be referenced in the force-converge comment.

#### 7-bis.3 Merge

```bash
gh pr merge $PR_NUM --repo stellar-experimental/henyey --squash --admin
```

If `--admin` fails (no admin token), still file the follow-ups, but leave the PR open and post `## Review: Force-Converge Permission Gap` so the operator can land it manually. Do NOT degrade to a non-admin merge.

#### 7-bis.4 Clean up + post

Same cleanup as Step 7.4 (move issue to `done`, unassign, prune worktree + build cache). Then post:

```markdown
## ⚠️ Force-Converged (Lifetime Cap)

**Commit:** <merge-commit-sha>
**Lifetime bounce count at force-converge:** <N>
**CI at merge:** green

This PR cycled 6+ times across multiple code states without convergence between reviewers and `/do`. Rather than blocking indefinitely, the pipeline force-merged on the strength of green CI alone, preserving each unresolved reviewer concern as a follow-up issue for operator triage.

**Unresolved concern follow-ups (by class):**
- `<class-A>`: #N1, #N2
- `<class-B>`: #N3
- `(unlabeled)`: #N4 *(reviewer didn't follow cycle-1 class-labeling discipline)*

**Inline-comment follow-ups (if any):** #N5, #N6

To prevent this in future cycles: reviewers should produce a COMPLETE cycle-1 change-list grouped by concern class. New classes appearing in cycle N≥2 should be flagged `**NEW CLASS DISCOVERED:**` to surface incomplete cycle-1 reviews early.
```

Exit.

### Wait path

Post:

```markdown
## Review: Waiting on CI

CI is still running (checks: <names>). Re-picking this PR on the next tick.
CI age: <CI_AGE_MIN> min / budget <CI_STUCK_AFTER_MINUTES> min.
Bounce-back count: <N>/3.
```

Unassign yourself so the next tick re-picks this issue. Exit.

### Bounce path

Post:

```markdown
## Review: Bounce-Back Cycle <N+1>

**Reason:** <"Reviewer A requested changes" | "Reviewer B requested changes" | "<external-user> requested changes" | "CI failed (diff-attributable)" | "CI failed (unrelated, will rebase)">

**Reviewer A:** APPROVE | CHANGES_REQUESTED — <one-line summary>
**Reviewer B:** APPROVE | CHANGES_REQUESTED — <one-line summary>
**External reviewers:** <list "user: STATE" for each non-agent reviewer with a recent review; omit the section if none>
**CI:** green | red | running

<If an external reviewer's CHANGES_REQUESTED triggered the bounce, paste the relevant body excerpt and a link to the review so /do Mode B can address it the same way it addresses agent feedback.>

<If CI red, paste the relevant failed-check excerpt via gh run view --log-failed>

Routing back to `ready-for-doing` for `/do` Mode B.
```

Move state and unassign:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE ready-for-doing
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

Exit.

### Block path

Post one of these (whichever cap tripped):

```markdown
## Review: Cycle Cap Reached / CI Stuck

**Bounce count (head-scoped):** 3
**Status:** blocked

**Pattern:** <summary of the disagreement: which reviewer kept failing, what
they kept asking for, what /do kept doing. This is what humans need to break
the tie.>

This PR has cycled 3 times on the current code state without converging. Human review required.
```

```markdown
## Review: Lifetime Cap + Unsafe CI

**Lifetime bounce count:** <LIFETIME_COUNT>
**CI state at cap:** red | empty | running-too-long
**Status:** blocked

The lifetime bounce cap was reached, but CI is not currently green so force-converge cannot proceed. Force-converge requires green CI as its sole final safety net; without it the pipeline cannot guarantee the merge is safe.

**Pattern:** <summary across the lifetime of this PR: what concerns kept recurring
across multiple /do pushes. This is the runaway-loop pattern — /do produces a
fix, /review-pr finds different (or same) issues, /do tries again, indefinitely.
This is what humans need to break the underlying disagreement.>

This PR has cycled 6+ times across multiple code states without converging. The head-scoped cap kept resetting on each fresh push but the underlying issue never resolved. Human review required to either:
- Approve manually (if reviewer concerns are over-strict),
- Reframe the issue / plan (if /do is missing context),
- Or close the PR and re-plan.

To retry after addressing root cause, post `## Review: Reset` with a one-line reason.
```

Move state:

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE blocked
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me
```

Exit.

---

## What you do NOT do

- **Do not** post a review yourself. Spawn sub-agents that post their own structured comments — you only orchestrate and combine.
- **Do not** override or summarize the reviewers' verdicts. Their `**Verdict:**` line is the verdict. You read it; you don't rewrite it.
- **Do not** merge if any of the three signals is not green. The matrix is the rule. *(Exception: the force-converge override at the lifetime cap merges on green CI alone — see Step 7-bis. Reviewer verdicts become follow-up issues rather than merge gates.)*
- **Do not** wait synchronously on long-running CI. If CI is `running`, unassign and exit — the next tick re-picks the issue.
- **Do not** use `gh pr review --approve` — GH silently downgrades it to a comment because the agent is the PR author. Use structured PR comments via `gh pr comment` instead.

## Failure handling

| Failure | Action |
|---|---|
| Reviewer sub-agent fails to post | Retry once. If still failing, treat as `CHANGES_REQUESTED` and bounce. |
| Reviewer's comment doesn't match the expected header/verdict shape | Treat as `pending`; if it stays malformed after Step 4 completes, bounce with a `## Review: Malformed Verdict` note. |
| No PR linked | Bounce to `ready-for-doing` with `## Review: No PR Linked`. |
| `gh pr merge --admin` fails with auto-merge hint | Retry with `--auto` (deferred merge). If `--auto` also fails, hard failure. See Step 7.3. |
| `gh pr merge --admin` fails (other error, e.g. token lacks admin) | Leave the issue in `in-review`; file a follow-up issue documenting the gap; do NOT degrade to a non-admin merge that might bypass CI gates. |
| GH API failure | Retry once after 5s; if still failing, leave assigned and exit non-zero. |

## Workspace contract

The `/review-pr` skill must never create worktrees outside `~/data` (the real home
directory's `data/` subdirectory, derived from the passwd entry — immune to `$HOME`
poisoning). All scratch state lives under `~/data/$SESSION_ID/review-pr-$ISSUE/`.
Reviewers derive their workspace by sourcing the shared contract helper:

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)"
source "$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

# review_pr_bootstrap validates and exports WORKTREE_BASE, CARGO_TARGET_DIR,
# and REVIEWER_WORKTREE. Pre-seeded values are accepted only if they resolve
# under the real ~/data AND match the expected session/issue prefix
# (~/data/$SESSION_ID/review-pr-$ISSUE/...). Hostile values (outside ~/data,
# ../ traversal, HOME poisoning, or cross-session shared directories) are
# rejected with a non-zero exit.
review_pr_bootstrap "$ISSUE" || exit 1
mkdir -p "$REVIEWER_WORKTREE"
```

This ensures build artifacts are isolated per-session and automatically discoverable
for cleanup. The skill itself is read-only (no code changes), so it rarely needs a
build cache — but if a reviewer sub-agent builds to verify, the target dir must
resolve under `~/data` and within the session prefix.

**Never create worktrees at the repo root or outside `~/data`.** All worktrees
must be under `~/data/$SESSION_ID/` to avoid polluting the shared checkout.

**Forbidden scratch patterns (issue #2843 — hard requirement).** Every reviewer
sub-agent (and any `/review`, `/spec-adhere`, `/review-fix` it invokes) is bound
to `$REVIEWER_WORKTREE` and must NEVER create any of these observed disk-leak
patterns — not in the repo tree, not as a `<repo>-pr<N>` sibling, not under `/tmp`:

- `.review-data/`
- `.review-worktrees/`
- `.worktrees/`
- `.copilot-tmp/`
- `.opencode/worktrees/`
- any path under `/tmp` (e.g. `/tmp/pr<N>-review`)

These are the exact paths prior review cycles leaked (an 11 GB `/tmp/pr2797-review`
near-OOM'd the root FS). On every exit path — including the Step 7 cleanup of
`WORKTREE_BASE` — assert the repo tree is clean:

```bash
assert_no_repo_tree_scratch "$REPO_ROOT" || {
  echo "Reviewer scratch leaked into the repo tree or a sibling — clean it now." >&2
}
```

## Branch protection

Because the pipeline gates merges on its own parsed verdicts (not GH-recognized approvals), `main` branch protection should NOT require pull-request approvals — that gate is impossible to satisfy when the agent is the PR author. Recommended branch protection:

- **Required:** all CI checks green (the names of every check in `.github/workflows/ci.yml`).
- **Required:** branch up-to-date with base.
- **Required approvals:** **0** — the pipeline's own reviewer verdicts gate merge.
- The agent's token must have `Pull requests: Read and write` and the user must be a repo admin for `--admin` merges to succeed.

The `pdr-managed` label on every pipeline-created PR distinguishes them from human PRs if you want to scope additional automation to managed ones only.
