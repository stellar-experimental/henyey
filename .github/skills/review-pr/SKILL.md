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
  blocks after 3 bounce-backs on the same code state OR 6 lifetime bounces
  (since last `## Review: Reset`), whichever trips first. Use when invoked by
  /project-tick with an issue in in-review, or manually as /review-pr <issue>.
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

```bash
# Use raw GraphQL — `gh issue view --json closedByPullRequestsReferences` silently
# omits the `state` subfield, so `select(.state == "OPEN")` would never match and
# PR_NUM would always be empty even when an OPEN PR exists.
PR_NUM=$(gh api graphql -f query='{
  repository(owner: "stellar-experimental", name: "henyey") {
    issue(number: '"$ISSUE"') {
      closedByPullRequestsReferences(first: 5) { nodes { number state } }
    }
  }
}' --jq '.data.repository.issue.closedByPullRequestsReferences.nodes | map(select(.state == "OPEN")) | .[0].number // empty')
```

If `PR_NUM` is empty, the issue is in `in-review` but has no open PR — that's a bug in `/do`. Post `## Review: No PR Linked` and move the issue back to `ready-for-doing`. Unassign. Exit.

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

- If `LIFETIME_COUNT >= 6`, the PR has cycled too many times across multiple code states. Post `## Review: Lifetime Cycle Cap Reached` (see message template in the Block section below), move to `blocked`, unassign, and exit. This catches runaway loops where each fresh `/do` push resets the head-scoped counter but the underlying disagreement never converges.
- Otherwise, if `HEAD_COUNT >= 3`, this is the 4th cycle on the current code. Post `## Review: Cycle Cap Reached`, move to `blocked`, unassign, and exit.
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

### Reviewer workspace contract

**All reviewer scratch work must live only under `$HOME/data`.** Reviewers should prefer the existing checkout for read-only inspection. If a reviewer needs a scratch checkout (e.g. to run a test or verify a claim), it must live under `$HOME/data`, never in the repo root, the repo parent, or anywhere outside `$HOME/data`. Each reviewer derives its workspace as follows:

```bash
SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%Y%m%d-%H%M%S)}"

# Validate pre-seeded overrides: must canonicalize under $HOME/data.
_validate_under_home_data() {
  local varname="$1" candidate="$2"
  local resolved
  resolved="$(realpath --canonicalize-missing "$candidate" 2>/dev/null || readlink -f "$candidate" 2>/dev/null || true)"
  if [ -z "$resolved" ]; then
    echo "ERROR: Cannot canonicalize $varname='$candidate' — neither realpath nor readlink -f available. Aborting." >&2
    exit 1
  fi
  case "$resolved" in
    "$HOME/data"/*) return 0 ;;
    "$HOME/data") return 0 ;;
    *) echo "ERROR: $varname='$candidate' (resolves to '$resolved') is outside \$HOME/data. Aborting." >&2; exit 1 ;;
  esac
}

if [ -n "${WORKTREE_BASE:-}" ]; then
  _validate_under_home_data WORKTREE_BASE "$WORKTREE_BASE"
fi
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  _validate_under_home_data CARGO_TARGET_DIR "$CARGO_TARGET_DIR"
fi

WORKTREE_BASE="${WORKTREE_BASE:-$HOME/data/$SESSION_ID/review-pr-$PR_NUM}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORKTREE_BASE/cargo-target}"

# Reviewer-specific scratch dir (only if needed for checkout/build):
REVIEWER_WORKTREE="$WORKTREE_BASE/reviewer-a"  # or reviewer-b
mkdir -p "$REVIEWER_WORKTREE"
```

Include this bootstrap verbatim in each reviewer's prompt so the sub-agent knows where to place any checkout or build output. The bootstrap is self-seeding: if the parent runtime pre-sets `WORKTREE_BASE` or `CARGO_TARGET_DIR`, the reviewer respects those only if they canonicalize under `$HOME/data`; otherwise it fails loudly. Without pre-seeded values, it falls back to `$HOME/data/$SESSION_ID/review-pr-$PR_NUM/...`.

## Step 4 — Spawn 2 reviewers in parallel

Launch both as `general-purpose` foreground sub-agents. Do not wait between them. **Each reviewer must be spawned with `--model gpt-5.4`** (or equivalent model parameter) explicitly — do not inherit from the parent. Cross-model diversity catches issues a same-model pipeline would miss.

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

### Reviewer A — Correctness (always)

> Invoke /review on PR #$PR_NUM in stellar-experimental/henyey. Focus on:
> correctness of the diff, test coverage, readability, error handling.
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

**If non-parity (risk lens):**

> Review PR #$PR_NUM in stellar-experimental/henyey for risk: regressions in
> existing behavior, performance impact, breaking changes to APIs or data
> formats, security implications, operational concerns (config, migrations).
> Reviewer A is doing correctness; you focus only on risk. Post your verdict
> as a single PR-level comment via `gh pr comment`, headed
> `## 🔍 Reviewer: Risk`, with `**Verdict:**` on its own line.

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
else
  CI_STATE="green"
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

```bash
gh pr merge $PR_NUM --repo stellar-experimental/henyey --squash --admin
```

The `--admin` flag means the agent must be authenticated as a repo admin. If your token doesn't have admin, the merge will fail — at which point operator intervention is needed (file a follow-up issue documenting the merge-permission gap; do NOT downgrade to non-admin merge that might silently bypass CI).

#### 7.4 Clean up

```bash
bash .github/skills/shared/scripts/move-issue-status.sh $ISSUE done
gh issue edit $ISSUE --repo stellar-experimental/henyey --remove-assignee @me

REPO_ROOT="$(git rev-parse --show-toplevel)"

# Recover session ID from the sidecar /do persisted (see do/SKILL.md A.2).
# Build cache lives at $HOME/data/<session-id>/do-$ISSUE/cargo-target/ — can be
# 25-50 GB per issue. Clean it up here; otherwise nothing else will.
if [ -f "$REPO_ROOT/data/do-$ISSUE/.session-id" ]; then
  SESSION_ID=$(cat "$REPO_ROOT/data/do-$ISSUE/.session-id")
  if [ -n "$SESSION_ID" ] && [ -d "$HOME/data/$SESSION_ID/do-$ISSUE" ]; then
    rm -rf "$HOME/data/$SESSION_ID/do-$ISSUE"
  fi
fi

# Worktree dir cleanup.
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
## Review: Lifetime Cycle Cap Reached

**Lifetime bounce count:** <LIFETIME_COUNT>
**Status:** blocked

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
- **Do not** merge if any of the three signals is not green. The matrix is the rule.
- **Do not** wait synchronously on long-running CI. If CI is `running`, unassign and exit — the next tick re-picks the issue.
- **Do not** use `gh pr review --approve` — GH silently downgrades it to a comment because the agent is the PR author. Use structured PR comments via `gh pr comment` instead.

## Failure handling

| Failure | Action |
|---|---|
| Reviewer sub-agent fails to post | Retry once. If still failing, treat as `CHANGES_REQUESTED` and bounce. |
| Reviewer's comment doesn't match the expected header/verdict shape | Treat as `pending`; if it stays malformed after Step 4 completes, bounce with a `## Review: Malformed Verdict` note. |
| No PR linked | Bounce to `ready-for-doing` with `## Review: No PR Linked`. |
| `gh pr merge --admin` fails (token lacks admin) | Leave the issue in `in-review`; file a follow-up issue documenting the gap; do NOT degrade to a non-admin merge that might bypass CI gates. |
| GH API failure | Retry once after 5s; if still failing, leave assigned and exit non-zero. |

## Workspace contract

The `/review-pr` skill must never create worktrees outside `$HOME/data`. All scratch
state lives under `$HOME/data/$SESSION_ID/review-pr-$ISSUE/`:

```bash
SESSION_ID="${CLAUDE_SESSION_ID:-$(date +%Y%m%d-%H%M%S)}"
export CARGO_TARGET_DIR="$HOME/data/$SESSION_ID/review-pr-$ISSUE/cargo-target"
```

This ensures build artifacts are isolated per-session and automatically discoverable
for cleanup. The skill itself is read-only (no code changes), so it rarely needs a
build cache — but if a reviewer sub-agent builds to verify, the target dir must
resolve under `$HOME/data`.

**Never create worktrees at the repo root or outside `$HOME/data`.** All worktrees
must be under `$HOME/data/$SESSION_ID/` to avoid polluting the shared checkout.

## Branch protection

Because the pipeline gates merges on its own parsed verdicts (not GH-recognized approvals), `main` branch protection should NOT require pull-request approvals — that gate is impossible to satisfy when the agent is the PR author. Recommended branch protection:

- **Required:** all CI checks green (the names of every check in `.github/workflows/ci.yml`).
- **Required:** branch up-to-date with base.
- **Required approvals:** **0** — the pipeline's own reviewer verdicts gate merge.
- The agent's token must have `Pull requests: Read and write` and the user must be a repo admin for `--admin` merges to succeed.

The `pdr-managed` label on every pipeline-created PR distinguishes them from human PRs if you want to scope additional automation to managed ones only.
