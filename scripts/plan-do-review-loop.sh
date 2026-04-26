#!/usr/bin/env bash
# plan-do-review-loop.sh — endless worker that selects the next eligible GitHub
# issue, assigns it, then runs copilot `/plan-do-review <issue>`. If no
# eligible issues exist, it sleeps without invoking copilot.
#
# Usage:
#   ./scripts/plan-do-review-loop.sh
#   LOOP_MODEL=claude-opus-4.6 ./scripts/plan-do-review-loop.sh
#
# Env vars:
#   LOOP_MODEL         AI model passed to copilot (default: claude-opus-4.6)
#   LOOP_EMPTY_SLEEP   Seconds to sleep when no issues found (default: 60)
#   LOOP_BETWEEN_SLEEP Seconds to sleep between successful runs (default: 60)
#   LOOP_LOG_DIR       Directory for per-run logs (default: ~/data/plan-do-review-loop)

set -euo pipefail

LOOP_MODEL="${LOOP_MODEL:-claude-opus-4.6}"
LOOP_EMPTY_SLEEP="${LOOP_EMPTY_SLEEP:-60}"
LOOP_BETWEEN_SLEEP="${LOOP_BETWEEN_SLEEP:-60}"
LOOP_LOG_DIR="${LOOP_LOG_DIR:-$HOME/data/plan-do-review-loop}"

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
log "Empty sleep: ${LOOP_EMPTY_SLEEP}s"
log "Between sleep: ${LOOP_BETWEEN_SLEEP}s"
log "Log dir:   $LOOP_LOG_DIR"

# --- Issue selection (mirrors SKILL.md auto-selection) ---
# Prints "<number> <title>" on success, or nothing if no eligible issue exists.
# Returns 0 on success/empty, 1 on API error.
select_issue() {
  local json

  # Priority 0: resume — oldest open issue already assigned to me
  # (interrupted previous run). Excludes failed issues.
  if ! json="$(gh issue list \
    --state open \
    --assignee '@me' \
    --search 'sort:created-asc -label:plan-do-review-loop-failed -label:not-ready' \
    --json number,title \
    --limit 1)"; then
    return 1
  fi

  local number
  number="$(jq -r '.[0].number // empty' <<<"$json")"
  if [[ -n "$number" ]]; then
    local title
    title="$(jq -r '.[0].title // ""' <<<"$json")"
    echo "resume ${number} ${title}"
    return 0
  fi

  # Priority 1: oldest open, unassigned issue labeled "ready",
  # excluding "plan-do-review-loop-failed" and "not-ready".
  if ! json="$(gh issue list \
    --state open \
    --assignee '' \
    --search 'sort:created-asc -label:plan-do-review-loop-failed -label:not-ready label:ready' \
    --json number,title \
    --limit 1)"; then
    return 1
  fi

  number="$(jq -r '.[0].number // empty' <<<"$json")"
  if [[ -n "$number" ]]; then
    local title
    title="$(jq -r '.[0].title // ""' <<<"$json")"
    echo "new ${number} ${title}"
    return 0
  fi

  # Priority 2: any eligible issue (no "ready" requirement), oldest first.
  if ! json="$(gh issue list \
    --state open \
    --assignee '' \
    --search 'sort:created-asc -label:plan-do-review-loop-failed -label:not-ready' \
    --json number,title \
    --limit 1)"; then
    return 1
  fi

  number="$(jq -r '.[0].number // empty' <<<"$json")"
  if [[ -n "$number" ]]; then
    local title
    title="$(jq -r '.[0].title // ""' <<<"$json")"
    echo "new ${number} ${title}"
    return 0
  fi
}

# Label a failed auto-selected issue so it won't be picked again.
mark_failed() {
  local issue="$1"
  log "Marking issue #${issue} as failed"
  gh issue edit "$issue" --add-label "plan-do-review-loop-failed" 2>/dev/null \
    || log "WARNING: could not add failure label to issue #${issue}"
  local me
  me="$(gh api user -q .login 2>/dev/null)" || true
  if [[ -n "$me" ]]; then
    gh issue edit "$issue" --remove-assignee "$me" 2>/dev/null \
      || log "WARNING: could not unassign ${me} from issue #${issue}"
  fi
}

# --- Main loop ---
while true; do
  # Select an issue before invoking copilot
  set +e
  selected="$(select_issue)"
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

  # Parse "mode number title…"
  select_mode="${selected%% *}"
  rest="${selected#* }"
  issue_number="${rest%% *}"
  issue_title="${rest#* }"

  if [[ "$select_mode" == "resume" ]]; then
    log "Resuming previously assigned issue #${issue_number}: ${issue_title}"
  else
    log "Auto-selected new issue #${issue_number}: ${issue_title}"

    # Assign as concurrency lock
    if ! gh issue edit "$issue_number" --add-assignee "@me" 2>/dev/null; then
      log "Could not assign issue #${issue_number} — may have been claimed. Sleeping ${LOOP_EMPTY_SLEEP}s…"
      sleep "$LOOP_EMPTY_SLEEP"
      continue
    fi

    # Verify we are the sole assignee (assignment doesn't fail for multi-assignee)
    assignee_count="$(gh issue view "$issue_number" --json assignees --jq '.assignees | length' 2>/dev/null)" || assignee_count=""
    if [[ "$assignee_count" != "1" ]]; then
      log "Issue #${issue_number} has ${assignee_count:-unknown} assignees — another worker likely claimed it. Skipping."
      sleep "$LOOP_EMPTY_SLEEP"
      continue
    fi
    log "Assigned issue #${issue_number} to self (sole assignee)"
  fi

  # Run copilot with the pre-selected issue number
  ts="$(date +%Y%m%d-%H%M%S)"
  logfile="$LOOP_LOG_DIR/${ts}-issue-${issue_number}.log"
  log "Starting copilot /plan-do-review ${issue_number} → $logfile"

  set +e
  copilot \
    --model "$LOOP_MODEL" \
    --allow-all-tools \
    --allow-all-paths \
    --log-dir "$LOOP_LOG_DIR/copilot-logs" \
    -p "/plan-do-review ${issue_number}" \
    2>&1 | tee "$logfile"
  rc="${PIPESTATUS[0]}"
  set -e

  if [[ "$rc" -ne 0 ]]; then
    log "copilot exited $rc for issue #${issue_number}"
    mark_failed "$issue_number"
  else
    log "copilot exited 0 for issue #${issue_number}"
  fi

  log "Sleeping ${LOOP_BETWEEN_SLEEP}s before next run…"
  sleep "$LOOP_BETWEEN_SLEEP"
done
