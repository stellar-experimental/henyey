---
name: project-tick
description: |
  Dispatcher for the henyey project pipeline. One tick = pick one unassigned issue
  from the project board, assign yourself, and invoke the right specialist skill
  for its current state. Safe to run in parallel — concurrency via GitHub assignee
  race. Use proactively when the user asks to "run a tick", "pick up an issue",
  "process the board", or via the loop driver at scripts/project-tick-loop.sh.
model: claude-haiku-4.5
---

# /project-tick — pipeline dispatcher

You are the dispatcher for the henyey project management pipeline. Your job is **not** to plan, implement, or review anything — that is what the specialist skills do. Your job is to:

1. Read the project board.
2. Pick exactly one issue that is ready for work.
3. Acquire it (assign yourself).
4. Dispatch the right specialist skill based on its current state.
5. Stop.

Multiple `/project-tick` invocations run in parallel safely. The GitHub assignee race ensures each tick grabs a distinct issue.

## Project board

- Repo: `stellar-experimental/henyey`
- Project: number `2`, ID `PVT_kwDOD-vqsM4BWQnL`
- Status field ID: `PVTSSF_lADOD-vqsM4BWQnLzhRmYgI`
- States (lowercase): `backlog`, `ready-for-planning`, `planning`, `ready-for-doing`, `doing`, `in-review`, `done`, `blocked`

## Dispatch table

| Status | Specialist | What it does |
|---|---|---|
| `backlog` | `/triage` | Validates the issue, labels it, advances to `ready-for-planning` (or `ready-for-doing` if trivial, or `blocked`) |
| `ready-for-planning` | `/plan` | Picks up the work; transitions to `planning` while drafting with parallel critics, then to `ready-for-doing` on convergence |
| `planning` | (no-op — actively assigned) | A `/plan` agent is currently drafting + running critics. Items in `planning` are always assigned; ticks filter them out automatically. |
| `ready-for-doing` | `/do` | Picks up the work; transitions to `doing` while implementing, then to `in-review` when PR is open |
| `doing` | (no-op — actively assigned) | A `/do` agent is currently implementing. Items in `doing` are always assigned; ticks filter them out automatically. |
| `in-review` | `/review-pr` | Two parallel reviewers + CI; auto-merges on triple-green; bounces back or blocks otherwise |
| `done`, `blocked` | (no-op) | Terminal / human-triaged |

## Algorithm

### Step 1 — Query the board

Single GraphQL call to fetch every open issue on the project with: assignees, status, labels, createdAt, linked PRs.

```bash
# Paginate — the project has >100 items once `done`/`blocked` accumulates,
# and without `--paginate` the picker silently goes blind to anything past
# the first page. See #2793-followup.
gh api graphql --paginate -f query='
  query($endCursor: String) {
    organization(login: "stellar-experimental") {
      projectV2(number: 2) {
        items(first: 100, after: $endCursor) {
          pageInfo { hasNextPage endCursor }
          nodes {
            id
            content {
              ... on Issue {
                number
                title
                createdAt
                assignees(first: 5) { nodes { login } }
                labels(first: 20) { nodes { name } }
                closedByPullRequestsReferences(first: 5) { nodes { number state url } }
                state
              }
            }
            fieldValueByName(name: "Status") {
              ... on ProjectV2ItemFieldSingleSelectValue { name }
            }
          }
        }
      }
    }
  }
' --jq '.data.organization.projectV2.items.nodes'
```

If the query fails, retry once after 5 seconds. If still failing, exit non-zero — operator will see the failure in the loop log.

### Step 2 — Filter to actionable items

An item is **actionable** if all of:

- `content.state == "OPEN"` (don't act on closed issues)
- `fieldValueByName.name ∈ { backlog, ready-for-planning, ready-for-doing, in-review }`
- `assignees.nodes` is empty (nobody is working on it)

Skip items where any check fails. Skip items whose status is `planning` (always assigned), `doing` (always assigned), `done`, or `blocked`.

#### Step 2b — Skip in-review items whose CI is still pending

`/review-pr`'s only useful work when CI is pending is to post "Waiting on CI" and unassign. Picking such items burns 2 reviewer-agent spawns per tick (~2M tokens) just to find CI hasn't finished — wasteful and amplifies on multi-loop deployments. Filter them at the orchestrator:

For each in-review candidate after the actionability filter above, look up the linked PR's CI summary and skip the item if CI is still running:

```bash
for ISSUE in <in-review candidates>; do
  PR_NUM=$(gh issue view "$ISSUE" --repo stellar-experimental/henyey \
    --json closedByPullRequestsReferences \
    --jq '.closedByPullRequestsReferences | map(select(.state == "OPEN")) | .[0].number // empty')

  # No PR linked = broken state; let /review-pr handle the recovery.
  [ -z "$PR_NUM" ] && continue

  # Fetch the rollup once.
  ROLLUP=$(gh pr view "$PR_NUM" --repo stellar-experimental/henyey \
    --json statusCheckRollup --jq '.statusCheckRollup')

  # Count rollup entries (must be > 0 — empty rollup is suspicious, not "green").
  CI_TOTAL=$(echo "$ROLLUP" | jq 'length')

  # Pending: anything not yet completed. Handle BOTH casings AND StatusContext.
  # GH Actions CheckRun: .status in [QUEUED, IN_PROGRESS, COMPLETED] (uppercase)
  # StatusContext (legacy commit status): .status is null; .state in [PENDING, SUCCESS, FAILURE, ERROR]
  CI_PENDING=$(echo "$ROLLUP" | jq '[.[] |
    select(
      (.status != null and (.status | ascii_upcase) != "COMPLETED")
      or
      (.status == null and (.state | ascii_upcase) == "PENDING")
    )] | length')

  # Failed: any failure / cancellation / error / timed out.
  CI_FAILED=$(echo "$ROLLUP" | jq '[.[] |
    select(
      ((.conclusion // "") | ascii_upcase) as $c |
      $c == "FAILURE" or $c == "CANCELLED" or $c == "TIMED_OUT"
      or ((.state // "") | ascii_upcase) as $s | $s == "FAILURE" or $s == "ERROR"
    )] | length')

  # Skip this tick if CI is genuinely in-progress (entries exist AND some are pending AND none failed).
  if [ "$CI_TOTAL" -gt 0 ] && [ "$CI_PENDING" -gt 0 ] && [ "$CI_FAILED" -eq 0 ]; then
    SKIP_THIS_ISSUE=true
  fi

  # NOTE: empty rollup (CI_TOTAL == 0) → keep actionable; /review-pr Step 5 will refuse to classify
  # it as green (see "Empty rollup is NOT green" rule there) and bounce/block as appropriate.
done
```

Rule summary:

- CI green (entries exist, none pending, none failed) → actionable (`/review-pr` will merge).
- CI red (any failure / cancellation / error) → actionable (`/review-pr` will bounce).
- CI still running (entries exist, some pending, none failed) → **NOT actionable this tick**. Next tick re-evaluates.
- Empty rollup (zero entries — workflow never started, broken config, fork PR gated) → actionable so `/review-pr` can detect and either block or bounce. **Never treat empty rollup as "green".**
- No PR linked → actionable (`/review-pr`'s no-PR recovery path).

This single change eliminates the wasted reviewer-spawn-during-CI-wait pattern. Wall-clock latency for the first review is unchanged in expectation because CI (10–30 min) dominates reviewer-agent time (2–3 min) — reviewers running in parallel with CI was an optimization the cost didn't justify.

### Step 3 — Pick one issue

Order actionable items by:

1. **Close-WIP-first state priority** — descending: `in-review` > `ready-for-doing` > `ready-for-planning` > `backlog`. Reason: prevents PRs from rotting in review while fresh backlog items pile up. (`planning` and `doing` items are never picked — they are always assigned and filtered out.)
2. **Label priority** within state — descending: `urgent` > `high` > `medium` > `low` > (no priority label).
3. **Age** within priority tier — oldest `createdAt` first.

Before picking the head of the sorted list, **filter out any issues this loop recently lost the sentinel race for** (per-loop cooldown — see #2822). The cooldown prevents the losing loop from re-picking the same issue every 30s while a concurrent loop is still working it, which historically wedged one loop's capacity for the entire duration of the other's `/review-pr` or `/do` cycle. The cooldown file is per-loop (keyed by `$LOOP_PID` exported by `scripts/project-tick-loop.sh`); each entry is `<issue> <expiry_epoch>` and the picker skips issues whose expiry is still in the future:

```bash
COOLDOWN_FILE="/tmp/project-tick-cooldown-${LOOP_PID:-default}"
COOLED_DOWN=""
if [ -f "$COOLDOWN_FILE" ]; then
  NOW=$(date +%s)
  # Prune expired entries in place; keep only still-active cooldowns.
  awk -v now="$NOW" '$2 > now' "$COOLDOWN_FILE" > "$COOLDOWN_FILE.tmp" \
    && mv "$COOLDOWN_FILE.tmp" "$COOLDOWN_FILE"
  COOLED_DOWN=$(awk '{print $1}' "$COOLDOWN_FILE" | sort -u | paste -sd, -)
fi
# Then filter the sorted actionable list: drop any issue whose number is in
# $COOLED_DOWN (comma-separated) before picking the head.
```

Pick the head of the post-filter sorted list. If the list is empty, print `no actionable issues` and exit 0.

### Step 4 — Acquire the issue (sentinel-comment lock)

The assignee field alone is NOT enough to detect a race when multiple loops run as the same GitHub user — both can self-assign and both think they won (see #2739). The fix is a **sentinel-comment lock**: each tick posts a uniquely-tagged comment, then verifies via comment ordering that its comment was the earliest one posted within a short grace window.

```bash
# Generate a unique tick ID. Includes PID and nanosecond timestamp so two
# ticks in the same second still differ.
TICK_ID="tick-$(date +%s%N)-$$"

# Self-assign. This is necessary (so /project-tick filters us out of future
# picker runs) but no longer SUFFICIENT for race detection.
gh issue edit "$ISSUE" --repo stellar-experimental/henyey --add-assignee @me

# Post the sentinel comment. Capture the comment ID so we can clean it up.
SENTINEL_ID=$(gh api "repos/stellar-experimental/henyey/issues/$ISSUE/comments" \
  --method POST \
  -f body="## 🔒 acquired-by:$TICK_ID

posted=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ), host=$(hostname), pid=$$" \
  --jq '.id')

if [ -z "$SENTINEL_ID" ]; then
  echo "Sentinel post failed — backing off"
  gh issue edit "$ISSUE" --repo stellar-experimental/henyey --remove-assignee @me 2>/dev/null || true
  exit 0
fi

# Grace window — sleep long enough for any concurrent tick to also post
# its sentinel. 5 seconds is enough; the cost is bounded per tick.
sleep 5

# Fetch all recent sentinel comments and find the earliest one within the
# past 60 seconds. Tie-break by comment ID (which is monotonic at the API
# level). If our sentinel is the earliest, we won.
WINNER_TICK=$(gh api "repos/stellar-experimental/henyey/issues/$ISSUE/comments" \
  --paginate \
  --jq '[.[] | select(.body | startswith("## 🔒 acquired-by:")) |
    select(.created_at | fromdate > (now - 60)) |
    {id, created_at, tick: (.body | split("\n")[0] | sub("^## 🔒 acquired-by:"; ""))}] |
    sort_by(.created_at, .id) | .[0].tick // ""')

if [ "$WINNER_TICK" != "$TICK_ID" ]; then
  echo "race lost on #$ISSUE (winner: $WINNER_TICK, us: $TICK_ID) — exiting"
  # Clean up our sentinel only. DO NOT unassign — multiple ticks running as the
  # same GitHub user share one assignment record, so removing it would yank
  # the winner's assignment out from under it (see #2787 / audit M1). The
  # winner retains the assignment and proceeds; we just step back.
  gh api "repos/stellar-experimental/henyey/issues/comments/$SENTINEL_ID" --method DELETE 2>/dev/null || true
  # Record a 5-minute cooldown so the next tick from this loop skips this
  # issue and falls through to lower-priority actionable items (#2822). The
  # foreign loop's specialist typically runs 5–30 min; if it finishes before
  # the cooldown expires, the natural state transition removes the item from
  # the actionable list anyway and the cooldown entry harmlessly expires.
  COOLDOWN_FILE="/tmp/project-tick-cooldown-${LOOP_PID:-default}"
  echo "$ISSUE $(( $(date +%s) + 300 ))" >> "$COOLDOWN_FILE"
  exit 0
fi

echo "Won race on #$ISSUE (sentinel $TICK_ID is earliest). Proceeding."
```

If we lose the race, exit cleanly — the next `/project-tick` will pick a different issue.

**Sentinel cleanup** is important to avoid issues accumulating dozens of `## 🔒` comments over time. The losing tick deletes its sentinel immediately. The winning tick MUST delete its sentinel after the specialist returns (or on any exit path) — see Step 6.

### Step 5 — Dispatch

Based on the issue's status, invoke the specialist skill **as a foreground sub-agent** so its work stays in its own context window AND the parent waits for it. Use the `general-purpose` agent type with explicit instructions to run the slash command, and **specify the model explicitly for the stage** (cost/capability balance):

| Status | Model | Sub-agent invocation |
|---|---|---|
| `backlog` | `claude-haiku-4.5` | `Run /triage $ISSUE. Report the final state transition.` |
| `ready-for-planning` | `gpt-5.4` | `Run /plan $ISSUE. Report the final state transition.` |
| `ready-for-doing` | `claude-opus-4.6` | `Run /do $ISSUE. Report the final state transition.` |
| `in-review` | `gpt-5.4` | `Run /review-pr $ISSUE. Report the final state transition.` |

Pass the model explicitly when invoking the sub-agent (e.g. via `--model <model>` on copilot agent dispatch, or the `model:` parameter on the Agent tool call). Don't let it inherit from the orchestrator's model.

**Rationale:** triage and orchestration are simple decision tasks (haiku is plenty). Implementation needs strong code-writing (opus). Plan-critics and PR-reviewers benefit from cross-model diversity (gpt-5.4 catches what an all-claude pipeline might miss).

**Critical: the sub-agent MUST run in the foreground.** Do not set `run_in_background: true` on the Agent tool call. The dispatcher's job is to block until the specialist either completes the full state transition OR posts a recognized failure marker (e.g. `## Plan: Did Not Converge`, `## Plan: Triage Disagreement`, `## Do: Plan Wrong`, `## Do: Local Verification Failed`, `## Review: Cycle Cap Reached`, `## Review: No PR Linked`) — anything less leaves work orphaned mid-flight (commit pushed but no PR open, etc.).

Wait for the sub-agent to complete. Do not try to summarize or second-guess its work — the specialist's commit history, issue comments, and PR reviews are the audit trail. After the sub-agent returns, report a one-line summary of the state transition it accomplished and exit.

### Step 6 — Cleanup

The specialist is responsible for:

- Moving the issue to its next state (via `move-issue-status.sh`).
- Unassigning itself on completion (`gh issue edit --remove-assignee @me`).
- Posting any required artifacts (triage report, converged plan, PR, review).

`/project-tick` IS responsible for one cleanup: **deleting its sentinel-lock comment** (from Step 4). Always run this, regardless of the specialist's exit status:

```bash
gh api "repos/stellar-experimental/henyey/issues/comments/$SENTINEL_ID" --method DELETE 2>/dev/null || true
```

If the sub-agent fails (non-zero exit), leave the issue's state and assignee as-is — the next tick will see we're still assigned and skip it. The operator will see the stuck assignment in the daily summary / loop log. The sentinel still gets deleted so it doesn't pollute future race detection.

## Flags

- `--dry-run` — Print the pick and dispatch decision, exit without acquiring. For sanity-checking the priority ordering.
- `--state=<state>` — Restrict pick to one state only (e.g. `--state=in-review` to drain reviews first). Useful for targeted catch-up.
- `--issue=<num>` — Skip the picker and dispatch directly to that issue's specialist. Useful for manual recovery.

## Examples

```bash
# Normal tick.
/project-tick

# Show what would happen, don't act.
/project-tick --dry-run

# Just drain in-review queue.
/project-tick --state=in-review

# Force a specific issue.
/project-tick --issue=2698
```

## Operational notes

- **Concurrency:** N parallel ticks are fine. Each grabs a distinct issue via the assignee race. The state-priority ordering means parallel ticks naturally distribute across states (one takes `in-review`, the next takes `ready-for-doing`, etc.).
- **Idempotency:** if a tick is interrupted between assignment and dispatch, the issue stays assigned to us. The next tick picks a different issue (we filter on `assignees empty`). The stuck issue surfaces in the daily summary as "assigned for >N hours" — operator unassigns it.
- **No retry on specialist failure:** if `/plan` exits with the issue still in `ready-for-planning` and assigned to us, that's a bug in `/plan`, not for `/project-tick` to paper over. The operator deals with it.
- **No archival:** `archive-stale-done.sh` runs as a separate scheduled GH workflow (`.github/workflows/archive-done.yml`), not inside this tick.

## When NOT to use

- **Do not** call `/project-tick` from inside `/plan`, `/do`, `/triage`, or `/review-pr` — it dispatches *to* them, not the other way around.
- **Do not** use this for one-off recovery — if you want to re-trigger a specific issue's specialist, use `--issue=<num>` directly, or invoke the specialist slash command yourself.
