#!/usr/bin/env bash
# plan-do-review-loop.sh — endless worker that selects up to N eligible GitHub
# issues, assigns them, then runs a single copilot session that processes all N
# issues in parallel via background agents. If no eligible issues exist, it
# sleeps without invoking copilot.
#
# Usage:
#   ./scripts/plan-do-review-loop.sh
#   LOOP_MODEL=claude-opus-4.6 LOOP_BATCH_SIZE=5 ./scripts/plan-do-review-loop.sh
#
# Env vars:
#   LOOP_MODEL              AI model passed to copilot (default: claude-opus-4.6)
#   LOOP_BATCH_SIZE         Max issues to process per batch (default: 3)
#   LOOP_EMPTY_SLEEP        Seconds to sleep when no issues found (default: 60)
#   LOOP_BETWEEN_SLEEP      Seconds to sleep between batch runs (default: 60)
#   LOOP_LOG_DIR            Directory for per-run logs (default: ~/data/plan-do-review-loop)
#   LOOP_MAX_STALE_RETRIES  Max consecutive no-progress attempts before marking
#                           an issue as failed (default: 5)
#   LOOP_ARCHIVE_DAYS       Archive closed project items older than this many
#                           days at the start of each tick (default: 2)

set -euo pipefail

LOOP_MODEL="${LOOP_MODEL:-claude-opus-4.6}"
LOOP_BATCH_SIZE="${LOOP_BATCH_SIZE:-3}"
LOOP_EMPTY_SLEEP="${LOOP_EMPTY_SLEEP:-60}"
LOOP_BETWEEN_SLEEP="${LOOP_BETWEEN_SLEEP:-60}"
LOOP_LOG_DIR="${LOOP_LOG_DIR:-$HOME/data/plan-do-review-loop}"
LOOP_MAX_STALE_RETRIES="${LOOP_MAX_STALE_RETRIES:-5}"

log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"; }

on_signal() {
  log "Received signal, shutting down."
  exit 0
}
trap on_signal INT TERM

# --- Preflight ---
command -v gh >/dev/null 2>&1 || { echo "ERROR: gh CLI not found" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "ERROR: jq not found" >&2; exit 1; }
command -v copilot >/dev/null 2>&1 || { echo "ERROR: copilot CLI not found" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "ERROR: gh not authenticated (run 'gh auth login')" >&2; exit 1; }

mkdir -p "$LOOP_LOG_DIR"

log "=== plan-do-review-loop ==="
log "Model:     $LOOP_MODEL"
log "Batch size: $LOOP_BATCH_SIZE"
log "Empty sleep: ${LOOP_EMPTY_SLEEP}s"
log "Between sleep: ${LOOP_BETWEEN_SLEEP}s"
log "Max stale retries: ${LOOP_MAX_STALE_RETRIES}"
log "Log dir:   $LOOP_LOG_DIR"

# --- Issue selection ---
# Project-board-native: eligible issues are those in the henyey project (#2)
# `Backlog` column, open, unassigned. Excludes everything currently in
# `Blocked`, `in plan`, `In progress`, `In review`, or `Done`.
#
# Priority: urgent → high → medium → low → rest. Within each tier, oldest
# first. Collects up to LOOP_BATCH_SIZE issues across tiers.
# Outputs one line per issue: "<number> <title>"
# Returns 0 on success/empty, 1 on API error.
PROJECT_OWNER="stellar-experimental"
PROJECT_NUMBER=2
BACKLOG_OPTION_ID="f75ad846"

select_issues() {
  local needed="$LOOP_BATCH_SIZE"

  # One paginated GraphQL query: all open Backlog items on project #2.
  # `--paginate` requires the cursor variable name to be exactly $endCursor.
  # `jq -s` is required because gh emits one JSON object per page.
  local backlog_json
  if ! backlog_json="$(gh api graphql --paginate -f query='
    query($org: String!, $proj: Int!, $endCursor: String) {
      organization(login: $org) {
        projectV2(number: $proj) {
          items(first: 100, after: $endCursor) {
            pageInfo { endCursor hasNextPage }
            nodes {
              status: fieldValueByName(name: "Status") {
                ... on ProjectV2ItemFieldSingleSelectValue { optionId }
              }
              content {
                ... on Issue {
                  number title createdAt state
                  assignees(first: 5) { nodes { login } }
                  labels(first: 20)    { nodes { name } }
                }
              }
            }
          }
        }
      }
    }' -f org="$PROJECT_OWNER" -F proj="$PROJECT_NUMBER" 2>/dev/null)"; then
    return 1
  fi

  # Walk priority tiers, dedupe, accumulate up to $needed.
  local found=0 seen=""
  local priority
  for priority in urgent high medium low ""; do
    [[ "$found" -ge "$needed" ]] && break
    local remaining=$((needed - found))

    # `$priority == ""` is the Priority-5 fallback: any remaining Backlog
    # open unassigned issue, oldest first (no priority-label requirement).
    local candidates
    candidates="$(jq -rs --arg p "$priority" \
                       --arg backlog "$BACKLOG_OPTION_ID" \
                       --argjson n "$remaining" '
      [ .[].data.organization.projectV2.items.nodes[]
        | select(.status.optionId == $backlog)
        | select(.content.state == "OPEN")
        | select((.content.assignees.nodes | length) == 0)
        | select($p == "" or (.content.labels.nodes | map(.name) | index($p)))
      ]
      | sort_by(.content.createdAt)
      | .[0:$n][]
      | "\(.content.number) \(.content.title)"
    ' <<<"$backlog_json")" || return 1

    local line num
    while IFS= read -r line; do
      [[ -z "$line" ]] && continue
      num="${line%% *}"
      [[ " $seen " == *" $num "* ]] && continue
      echo "$line"
      seen="$seen $num"
      found=$((found + 1))
      [[ "$found" -ge "$needed" ]] && break
    done <<<"$candidates"
  done
}

# Move a failed auto-selected issue to the `Blocked` column so it won't be
# picked again, and unassign. The board column replaces the legacy
# `plan-do-review-loop-failed` label.
mark_failed() {
  local issue="$1"
  log "Moving issue #${issue} to Blocked (failed)"
  bash "$(dirname "$0")/../.github/skills/plan-do-review/scripts/move-issue-status.sh" \
    "$issue" Blocked 2>/dev/null \
    || log "WARNING: could not move issue #${issue} to Blocked"
  local me
  me="$(gh api user -q .login 2>/dev/null)" || true
  if [[ -n "$me" ]]; then
    gh issue edit "$issue" --remove-assignee "$me" 2>/dev/null \
      || log "WARNING: could not unassign ${me} from issue #${issue}"
  fi
  rm -f "$LOOP_LOG_DIR/.progress-${issue}"
}

# Count issue comments matching plan-do-review progress markers.
# Returns a fingerprint (comment count) that changes when the skill makes
# forward progress (new proposal draft, critic response, converged proposal,
# review-fix report, implementation complete, Blocked move, redirect).
#
# "Moved to Blocked" matches the current dependency/readiness comments;
# "Marking as not-ready" is kept for backward compatibility with legacy
# comments posted before the project-board migration.
count_progress_markers() {
  local issue="$1"
  gh issue view "$issue" --json comments \
    --jq '[.comments[].body | select(
      test("Proposal Draft|Critic Response|Converged Proposal|Review-Fix Report|Implementation Complete|Moved to Blocked|Marking as not-ready|⏩ This issue is blocked")
    )] | length' 2>/dev/null || echo "0"
}

# Check whether the issue reached a terminal state (no further retries needed).
# Terminal states: closed, in the Blocked column, or unassigned (redirect).
is_terminal() {
  local issue="$1"
  local json
  json="$(gh issue view "$issue" --json state,assignees 2>/dev/null)" || return 1

  local state
  state="$(jq -r '.state' <<<"$json")"
  [[ "$state" == "CLOSED" ]] && return 0

  # Blocked column on the henyey project board means triage decided the
  # issue is not actionable right now (unmet deps, vague proposal, or skill
  # crash). Operator must re-triage by moving back to Backlog.
  local status_option
  status_option="$(gh api graphql -f query='
    query($owner: String!, $repo: String!, $num: Int!) {
      repository(owner: $owner, name: $repo) {
        issue(number: $num) {
          projectItems(first: 20) {
            nodes {
              project { id }
              fieldValueByName(name: "Status") {
                ... on ProjectV2ItemFieldSingleSelectValue { optionId }
              }
            }
          }
        }
      }
    }' -f owner="$PROJECT_OWNER" -f repo=henyey -F num="$issue" \
    --jq '.data.repository.issue.projectItems.nodes[]
          | select(.project.id == "PVT_kwDOD-vqsM4BWQnL")
          | .fieldValueByName.optionId' 2>/dev/null | head -n1)"
  # Blocked option id = 53ce269e
  [[ "$status_option" == "53ce269e" ]] && return 0

  # Unassigned means the skill handed it off (redirect / dependency triage).
  local assignee_count
  assignee_count="$(jq -r '.assignees | length' <<<"$json")"
  [[ "$assignee_count" == "0" ]] && return 0

  return 1
}

# Check stale-retry tracking. Returns 0 if the issue should be retried,
# 1 if it has exceeded LOOP_MAX_STALE_RETRIES without progress.
check_stale_retries() {
  local issue="$1"
  local progress_file="$LOOP_LOG_DIR/.progress-${issue}"

  local current_markers
  current_markers="$(count_progress_markers "$issue")"

  if [[ -f "$progress_file" ]]; then
    local prev_markers prev_stale_count
    prev_markers="$(sed -n '1p' "$progress_file")"
    prev_stale_count="$(sed -n '2p' "$progress_file")"

    if [[ "$current_markers" -gt "$prev_markers" ]]; then
      # Progress was made — reset stale counter
      log "Issue #${issue}: progress detected (markers: ${prev_markers} → ${current_markers}), resetting stale counter"
      printf '%s\n%s\n' "$current_markers" "0" > "$progress_file"
    else
      # No progress — increment stale counter
      local new_stale=$((prev_stale_count + 1))
      log "Issue #${issue}: no progress (markers still ${current_markers}), stale attempt ${new_stale}/${LOOP_MAX_STALE_RETRIES}"
      printf '%s\n%s\n' "$current_markers" "$new_stale" > "$progress_file"

      if [[ "$new_stale" -ge "$LOOP_MAX_STALE_RETRIES" ]]; then
        return 1  # exceeded max stale retries
      fi
    fi
  else
    # First attempt — initialize tracking
    printf '%s\n%s\n' "$current_markers" "0" > "$progress_file"
  fi
  return 0
}

# --- Main loop ---
LOOP_ARCHIVE_DAYS="${LOOP_ARCHIVE_DAYS:-2}"

while true; do
  # Best-effort: archive any closed project items older than LOOP_ARCHIVE_DAYS.
  # Idempotent and cheap (one paginated GraphQL query if nothing is stale).
  # Failure is logged but does not break the loop.
  archive_script="$(dirname "$0")/../.github/skills/plan-do-review/scripts/archive-stale-done.sh"
  if [[ -x "$archive_script" ]]; then
    set +e
    archive_out="$(bash "$archive_script" "$LOOP_ARCHIVE_DAYS" 2>&1)"
    archive_rc=$?
    set -e
    if [[ "$archive_rc" -eq 0 ]]; then
      # Only echo if something was actually archived (suppress the
      # routine "No items to archive" log line every minute).
      if [[ "$archive_out" != *"No items to archive"* ]]; then
        log "$archive_out"
      fi
    else
      log "WARNING: archive-stale-done exited ${archive_rc}: ${archive_out}"
    fi
  fi

  # Select up to LOOP_BATCH_SIZE issues
  set +e
  selected="$(select_issues)"
  select_rc=$?
  set -e

  if [[ "$select_rc" -ne 0 ]]; then
    log "GitHub API error during issue selection. Sleeping ${LOOP_EMPTY_SLEEP}s…"
    sleep "$LOOP_EMPTY_SLEEP"
    continue
  fi

  if [[ -z "$selected" ]]; then
    log "No eligible issues found. Sleeping ${LOOP_EMPTY_SLEEP}s…"
    sleep "$LOOP_EMPTY_SLEEP"
    continue
  fi

  # Collect issue numbers into an array, assigning each as concurrency lock
  declare -a batch_issues=()
  while IFS= read -r line; do
    issue_number="${line%% *}"
    issue_title="${line#* }"

    log "Auto-selected issue #${issue_number}: ${issue_title}"

    # Assign as concurrency lock
    if ! gh issue edit "$issue_number" --add-assignee "@me" 2>/dev/null; then
      log "Could not assign issue #${issue_number} — may have been claimed. Skipping."
      continue
    fi

    # Verify we are the sole assignee
    assignee_count="$(gh issue view "$issue_number" --json assignees --jq '.assignees | length' 2>/dev/null)" || assignee_count=""
    if [[ "$assignee_count" != "1" ]]; then
      log "Issue #${issue_number} has ${assignee_count:-unknown} assignees — another worker likely claimed it. Skipping."
      continue
    fi

    log "Assigned issue #${issue_number} to self (sole assignee)"
    batch_issues+=("$issue_number")
  done <<<"$selected"

  if [[ "${#batch_issues[@]}" -eq 0 ]]; then
    log "All selected issues were claimed by others. Sleeping ${LOOP_EMPTY_SLEEP}s…"
    sleep "$LOOP_EMPTY_SLEEP"
    continue
  fi

  # Build the combined prompt for a single copilot session.
  # Copilot will use background agents to process each issue in parallel.
  prompt="Process the following ${#batch_issues[@]} issues in parallel. For each issue, invoke /plan-do-review <number> using a separate background general-purpose agent so they run concurrently. Wait for all agents to complete before finishing.

Issues:
$(printf '  - #%s\n' "${batch_issues[@]}")"

  ts="$(date +%Y%m%d-%H%M%S)"
  logfile="$LOOP_LOG_DIR/${ts}-batch-$(IFS=-; echo "${batch_issues[*]}").log"
  log "Starting copilot batch [${batch_issues[*]}] → $logfile"

  set +e
  copilot \
    --model "$LOOP_MODEL" \
    --autopilot \
    --allow-all-tools \
    --allow-all-paths \
    --log-dir "$LOOP_LOG_DIR/copilot-logs" \
    -p "$prompt" \
    2>&1 | tee "$logfile"
  rc="${PIPESTATUS[0]}"
  set -e

  if [[ "$rc" -ne 0 ]]; then
    log "copilot exited $rc for batch [${batch_issues[*]}]"
  else
    log "copilot exited 0 for batch [${batch_issues[*]}]"
  fi

  # Brief pause for GitHub API consistency (auto-close may take a moment)
  sleep 3

  # Check terminal state for each issue in the batch
  for issue_number in "${batch_issues[@]}"; do
    if is_terminal "$issue_number"; then
      log "Issue #${issue_number} reached terminal state (closed/triaged/redirected)"
      rm -f "$LOOP_LOG_DIR/.progress-${issue_number}"
    else
      log "Issue #${issue_number} still open after copilot exit"
      if ! check_stale_retries "$issue_number"; then
        log "Issue #${issue_number} exceeded ${LOOP_MAX_STALE_RETRIES} consecutive stale retries — marking as failed"
        mark_failed "$issue_number"
      fi
    fi
  done

  unset batch_issues

  log "Sleeping ${LOOP_BETWEEN_SLEEP}s before next run…"
  sleep "$LOOP_BETWEEN_SLEEP"
done
