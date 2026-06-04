---
name: project-tick
description: |
  Single-pick dispatcher primitive for the henyey project pipeline. One tick =
  pick one actionable issue from the project board and dispatch the right
  specialist sub-agent for its current state. Used for manual single picks and
  `--issue=` recovery; the continuous orchestrator is `/project-loop`, which owns
  concurrency centrally. Use when the user asks to "run a tick", "pick up an
  issue", or "process one board item".
argument-hint: "[--dry-run] [--state=<state>] [--issue=<num>]"
---

# /project-tick — single-pick pipeline dispatcher

You are a single-pick dispatcher for the henyey project management pipeline. Your job is **not** to plan, implement, or review anything — that is what the specialist skills do. Your job is to:

1. Read the project board.
2. Pick exactly one issue that is ready for work.
3. Self-assign it (for board visibility).
4. Dispatch the right specialist sub-agent based on its current state.
5. Stop.

`/project-tick` makes **one** central pick and dispatches **one** specialist. It is for manual single picks and `--issue=<num>` recovery. The continuous, parallel orchestrator is **`/project-loop`**, which reuses this skill's board query + filters + pick ordering, picks centrally, and tracks in-flight issues in an **in-memory** set. There is no GitHub locking anywhere in the pipeline — a single orchestrator never races itself, and standalone `/project-tick` runs are operator-initiated one-shots.

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
| `in-review` | `/review-pr` | Two parallel reviewers + refute pass + CI; auto-merges on triple-green; bounces back or blocks otherwise |
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

If the query fails, retry once after 5 seconds. If still failing, exit non-zero — the operator will see the failure.

### Step 2 — Filter to actionable items

An item is **actionable** if all of:

- `content.state == "OPEN"` (don't act on closed issues)
- `fieldValueByName.name ∈ { backlog, ready-for-planning, ready-for-doing, in-review }`
- `assignees.nodes` is empty (nobody is working on it)

Skip items where any check fails. Skip items whose status is `planning` (always assigned), `doing` (always assigned), `done`, or `blocked`.

#### Step 2b — Skip in-review items whose CI is still pending

`/review-pr`'s only useful work when CI is pending is to post "Waiting on CI" and unassign. Picking such items burns reviewer-agent spawns just to find CI hasn't finished — wasteful. Filter them at the dispatcher:

For each in-review candidate after the actionability filter above, look up the linked PR's CI summary and skip the item if CI is still running.

**Resolve the open PR number from the Step-1 GraphQL result — do NOT re-fetch via `gh issue view`.** Step 1's board query already fetches `closedByPullRequestsReferences(first: 5) { nodes { number state url } }` per issue, including the `state` subfield. Critically, `gh issue view --json closedByPullRequestsReferences` does **not** expose `.state` on that field (only the GraphQL endpoint does — see `do/SKILL.md` Step 0), so a `select(.state == "OPEN")` against `gh issue view` always yields empty and Step 2b silently becomes a no-op. Consume the per-issue `closedByPullRequestsReferences.nodes` already in hand from Step 1 instead:

```bash
for ISSUE in <in-review candidates>; do
  # Resolve the open PR number from the Step-1 GraphQL node for this issue
  # (content.closedByPullRequestsReferences.nodes — already fetched, with .state).
  PR_NUM=$(echo "$STEP1_NODE_FOR_ISSUE" | jq -r \
    '.content.closedByPullRequestsReferences.nodes
     | map(select(.state == "OPEN")) | .[0].number // empty')

  # No open PR linked = broken state; let /review-pr handle the recovery.
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

Wall-clock latency for the first review is unchanged in expectation because CI (10–30 min) dominates reviewer-agent time (2–3 min).

### Step 3 — Pick one issue

Order actionable items by:

1. **Close-WIP-first state priority** — descending: `in-review` > `ready-for-doing` > `ready-for-planning` > `backlog`. Reason: prevents PRs from rotting in review while fresh backlog items pile up. (`planning` and `doing` items are never picked — they are always assigned and filtered out.)
2. **Label priority** within state — descending: `urgent` > `high` > `medium` > `low` > (no priority label).
3. **Age** within priority tier — oldest `createdAt` first.

Pick the head of the sorted list. If the list is empty, print `no actionable issues` and exit 0.

### Step 4 — Self-assign

Self-assign the issue for board visibility:

```bash
gh issue edit "$ISSUE" --repo stellar-experimental/henyey --add-assignee @me
```

**No locking is needed.** The orchestrator (`/project-loop`, or a one-shot `/project-tick` run) picks centrally and tracks in-flight issues in an **in-memory** set — it never races itself, so there is no flock, no sentinel comment, and no cooldown machinery. The self-assign is purely for board visibility (so the daily summary and humans can see what is in flight); it is not a mutual-exclusion mechanism.

### Step 5 — Dispatch

Based on the issue's status, invoke the specialist skill **as a foreground sub-agent via the Agent/Task tool** so its work stays in its own context window AND the parent waits for it. Use `subagent_type: general-purpose`, `run_in_background: false`, and **set the `model` parameter explicitly for the stage** on the Agent call (do not let it inherit the orchestrator's model):

| Status | `model` | Sub-agent prompt |
|---|---|---|
| `backlog` | `haiku` | `Run /triage <ISSUE> and report the final board state transition.` |
| `ready-for-planning` | `opus` | `Run /plan <ISSUE> and report the final board state transition.` |
| `ready-for-doing` | `opus` | `Run /do <ISSUE> and report the final board state transition.` |
| `in-review` | `opus` | `Run /review-pr <ISSUE> and report the final board state transition.` |

**Rationale:** triage is a simple decision task (haiku is plenty). Planning, implementation, and review need strong code/plan reasoning (opus). The cross-model diversity that a second model family used to provide on plan-critics and PR-reviewers is now supplied by the **adversarial refute pass** built into `/plan` and `/review-pr` (an independent opus "skeptic" sub-agent tries to refute each blocking finding; a finding only stands if it survives refutation) — so an all-opus pipeline no longer risks the false-positive blind spot a single model family otherwise would.

**Critical: the sub-agent MUST run in the foreground.** Do not set `run_in_background: true`. The dispatcher's job is to block until the specialist either completes the full state transition OR posts a recognized failure marker (e.g. `## Plan: Triage Disagreement`, `## Plan: Scope Mismatch`, `## ⚠️ Plan: Force-Converged`, `## Do: Plan Wrong`, `## Do: Local Verification Failed`, `## Review: Cycle Cap Reached`, `## Review: No PR Linked`) — anything less leaves work orphaned mid-flight (commit pushed but no PR open, etc.).

Wait for the sub-agent to complete. Do not try to summarize or second-guess its work — the specialist's commit history, issue comments, and PR reviews are the audit trail. After the sub-agent returns, report a one-line summary of the state transition it accomplished and exit.

### Step 6 — Cleanup

The specialist owns its own state transition and cleanup:

- Moving the issue to its next state (via `move-issue-status.sh`).
- **Self-unassigning** on completion (`gh issue edit --remove-assignee @me`).
- Posting any required artifacts (triage report, converged plan, PR, review).

`/project-tick` has **nothing to release** — there is no lock and no sentinel comment in this pipeline. Once the specialist returns, the tick is done.

If the sub-agent fails (returns a failure marker), leave the issue's state and assignee as-is — the operator will see the stuck assignment in the daily summary. There is nothing to unwind.

## Flags

- `--dry-run` — Print the pick and dispatch decision, exit without self-assigning or dispatching. For sanity-checking the priority ordering.
- `--state=<state>` — Restrict pick to one state only (e.g. `--state=in-review` to drain reviews first). Useful for targeted catch-up.
- `--issue=<num>` — Skip the picker and dispatch directly to that issue's specialist. Useful for manual recovery.

## Examples

```bash
# Normal single pick.
/project-tick

# Show what would happen, don't act.
/project-tick --dry-run

# Just drain in-review queue (one pick).
/project-tick --state=in-review

# Force a specific issue (manual recovery).
/project-tick --issue=2698
```

## Operational notes

- **Single orchestrator, no locking by design.** `/project-tick` makes one pick per invocation. The continuous orchestrator `/project-loop` owns concurrency by picking centrally and tracking in-flight issues in an **in-memory** set — it never races itself, so there is no assignee race, no sentinel-comment lock, and no cooldown files anywhere. Do **not** run two orchestrators concurrently; there is no mutual exclusion to protect against double-dispatch.
- **Idempotency:** if a tick is interrupted between self-assign and dispatch, the issue stays assigned to us and surfaces in the daily summary as "assigned for >N hours" — the operator unassigns it.
- **No retry on specialist failure:** if `/plan` returns with the issue still in `ready-for-planning` and assigned to us, that's a bug in `/plan`, not for `/project-tick` to paper over. The operator deals with it.
- **No archival:** `archive-stale-done.sh` runs as a separate scheduled GH workflow (`.github/workflows/archive-done.yml`), not inside this tick.

## When NOT to use

- **Do not** call `/project-tick` from inside `/plan`, `/do`, `/triage`, or `/review-pr` — it dispatches *to* them, not the other way around.
- **Do not** use this for continuous processing — that is `/project-loop`'s job (central pick, parallel fan-out, CI pipelining, self-reflection). Use `/project-tick` only for a single manual pick or `--issue=<num>` recovery.
