# Monitor Label Policy

> **Canonical reference.** This document is the single source of truth for
> issue-labeling and board-routing behavior across all monitor skills
> (`monitor-loop`, `monitor-tick`, `daily-summary`). Skill inlines are
> excerpts that reference this file. When they conflict, this document wins.
> Updates go here first; skill inlines are updated to match.

## Label Definitions

| Label | Criteria | Examples |
|-------|----------|----------|
| `urgent` | Blocks validator operation or consensus participation | Hash mismatch (any kind), wedged node (frozen event loop, watchdog auto-abort), failing CI on origin/main blocking deploy (post-#3351 that means a failing `Verify Execution (Mainnet)` run — the only workflow the deploy gate consumes; other workflow failures no longer gate deploy and file **unlabeled**), panic or crash from production code, SYNC FAILURE past active deadline, OOM-driven restart, deploy regression |
| `alarm-regression` | Alarm that was meaningfully active (≥5% of ticks) in the replay baseline has gone completely silent (0% firing) in the current replay window | An alarm tracking recovery-stalled ticks that stops firing after a code change, an alarm for high open_fds that disappears after a refactor |
| *(no label)* | Non-urgent: does not block operation | Calibration, threshold tuning, NONC alerts, cosmetic noise, follow-up improvements, metric drift without visible effect |
| `not-ready` | Needs operator decision before any code change | Tier-3 self-reflection issues, design decisions pending, ambiguous requirements needing human input |

## Label Lifecycle

- **Escalation:** Monitors MAY add `urgent` to an existing unlabeled issue
  if new evidence shows it is now operation-blocking. Use
  `gh issue edit <N> --add-label urgent`.
- **De-escalation:** Monitors do NOT remove labels. Only operators or
  downstream processes (e.g., `plan-do-review` upon completion) remove labels.
- **`not-ready` removal:** Operator-only action. Applied when the operator
  provides the needed decision or clarification.
- **Conflict (`not-ready` + new urgent evidence):** Monitor adds `urgent`
  AND comments with the blocking evidence. Both labels coexist temporarily —
  this signals "needs operator decision AND is now blocking." The operator
  resolves by removing `not-ready` when ready, or removing `urgent` if the
  evidence is dismissed.
- **`alarm-regression`:** Filed automatically by `scripts/dev/check-alarm-regression.sh`
  when weekly alarm replay detects a regression. Closed manually after
  investigation — the alarm was intentionally removed, its threshold was
  adjusted, or the underlying issue was fixed. May coexist with `urgent` if
  the missing alarm was operation-blocking.

## Board Routing

Project board routing replaces the retired `ready` label as the mechanism
for downstream automation (`plan-do-review` auto-selects from the Backlog
column on project #2).

### Routing by Severity

| Severity | Board Status | Rationale |
|----------|-------------|-----------|
| `urgent` or *(no label)* | Backlog | Eligible for auto-selection by `plan-do-review` |
| `alarm-regression` | Backlog | Eligible for auto-selection; supplemental to urgency labels |
| `not-ready` | Blocked | NOT eligible for auto-selection; operator must unblock |

### When to Route

- **After filing a new issue** (`gh issue create`): always call
  `move-issue-status.sh` with the appropriate status.
- **After commenting on an existing issue** (`gh issue comment`): check
  whether the issue is already on the project board. If NOT on the project,
  add it with the appropriate status. If already on the project, do NOT
  change its status (it may be `In progress`, `In review`, etc.).

### Board Routing Commands

```bash
# New issue — route to Backlog (urgent or no-label):
bash .github/skills/shared/scripts/move-issue-status.sh "$ISSUE_NUM" backlog

# New issue — route to Blocked (not-ready):
bash .github/skills/shared/scripts/move-issue-status.sh "$ISSUE_NUM" blocked

# Existing issue — only add if not already on project:
ITEM_ID=$(gh api graphql -f query='
  query($owner: String!, $repo: String!, $num: Int!) {
    repository(owner: $owner, name: $repo) {
      issue(number: $num) {
        projectItems(first: 20) {
          nodes { id project { id } }
        }
      }
    }
  }' -f owner=stellar-experimental -f repo=henyey -F num="$ISSUE_NUM" \
  --jq '.data.repository.issue.projectItems.nodes[]
        | select(.project.id == "PVT_kwDOD-vqsM4BWQnL") | .id' | head -n1)

if [ -z "$ITEM_ID" ]; then
  # Not on project — add with appropriate status
  bash .github/skills/shared/scripts/move-issue-status.sh "$ISSUE_NUM" backlog
fi
# else: already on project, preserve current status (no-op)
```

### Failure Handling

If `move-issue-status.sh` fails after a successful `gh issue create` or
`gh issue comment`:

1. **Log** the failure as an ACTION item in the tick status report, including
   the issue number.
2. **Continue operating** — the issue is filed in GitHub and will be found by
   manual triage.
3. **Retry on next tick**: the skill re-evaluates state each cycle. On the
   next tick, re-check issues filed/commented in recent ticks and retry
   board-add for any that are still off-project.
4. **Escalate after 3 failures**: If 3 consecutive ticks fail for the same
   issue, comment on the issue: `Board routing failed — manual triage needed`
   and tag the operator.

## Filing Flow

1. **Investigate** to root cause — read source code, check logs, trace paths.
2. **Search for existing issue:**
   `gh issue list --search "<symptom keywords>" --state open`.
   If a match exists and is OPEN:
   - `gh issue comment <N>` with new evidence.
   - Apply `urgent` only if new evidence meets urgent criteria.
   - Board-route per "existing issue" rules above.
   - STOP.
3. **File new issue** with `gh issue create`:
   - Append `--label urgent` ONLY when the symptom meets urgent criteria.
   - Append `--label not-ready` ONLY for tier-3 operator-decision issues.
   - Otherwise omit labels (non-urgent).
   - Board-route per "new issue" rules above.
4. **Do NOT spawn agents.** Downstream processes handle fixes.

## Deploy Regression Procedure

If the node fails after a deploy:

1. **Record the bad SHA** BEFORE any rollback rebuild:
   ```bash
   bad_sha=$(cat "$BUILD_SHA_FILE")
   ```

2. **Append to quarantine file** (idempotent, via shared helper):
   ```bash
   source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
   local rc=0
   quarantine_append "$HOME/data/deploy_quarantine.txt" "$bad_sha" "regression #<issue>" || rc=$?
   if [ $rc -ne 0 ]; then
     echo "WARNING: quarantine_append failed (rc=$rc) — deploy gate may not block next tick" >&2
   fi
   ```
   > `$bad_sha` is expected to be a valid 40-char hex SHA from step 1. The
   > nonzero-rc check covers both invalid-SHA (rc=1) and I/O errors (rc=2)
   > defensively.

3. **File or comment** on a GitHub issue (label `urgent` — validator
   operation is impacted) with regression details.

4. **Restart** the node on the last known-good binary while waiting for fix.

The quarantine gate blocks re-deployment. See monitor-tick for quarantine
clearance procedure.

## Recurrence Policy

- **Comment on existing issue** (default) when: same symptom at same site
  with additive data, additional instrumentation, reproduction on different
  commit of same bug.
- **File new issue** when: different subsystem, different phase/mark,
  different root-cause hypothesis, or different candidate site set. Include
  `Related to #<prior>` + one-line scope-diff.
- **Closed issue recurs**: file NEW issue with `Related to #<prior> (closed)`
  and note why prior fix didn't cover this case.

## Daily-Summary Rendering

Label precedence for issue rendering:
1. `urgent` → 🔴 (red dot) — takes precedence even if `not-ready` also present
2. `not-ready` → ⚫ (grey dot)
3. *(no label)* → ⚪ (white circle)

## Legacy Transition

- The GitHub label `ready` is NOT deleted from the repository (preserves
  history on closed issues).
- The label is simply no longer applied by any skill.
- **Rollout precondition**: before merging this change, verify
  `gh issue list --label ready --state open` returns zero results. If any
  open issues carry `ready`, manually relabel them per the severity model
  above.
