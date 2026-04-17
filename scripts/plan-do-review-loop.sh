#!/usr/bin/env bash
# plan-do-review-loop.sh — endless worker that runs the copilot CLI with the
# `/plan-do-review` skill on one open GitHub issue at a time.
#
# Usage:
#   ./scripts/plan-do-review-loop.sh                  # process oldest open unassigned issue
#   LOOP_LABEL=proposal ./scripts/plan-do-review-loop.sh
#   LOOP_MODEL=claude-opus-4.6 ./scripts/plan-do-review-loop.sh
#
# Env vars:
#   LOOP_MODEL         AI model passed to copilot (default: claude-opus-4.6)
#   LOOP_LABEL         Optional extra label filter (default: empty)
#   LOOP_EMPTY_SLEEP   Seconds to sleep when no issues found (default: 60)
#   LOOP_LOG_DIR       Directory for per-run logs (default: ~/data/plan-do-review-loop)
#
# Selection rule: oldest open issue that is unassigned AND does not have the
# `plan-do-review-loop-failed` or `not-ready` labels. Assigning the issue to
# @me serves as the lock; failures are labeled and unassigned so the loop
# moves on.
#
# Crashed runs leave the issue assigned to you. The loop will NOT auto-pick it
# up again — unassign manually once you've reviewed what happened.

set -euo pipefail

LOOP_MODEL="${LOOP_MODEL:-claude-opus-4.6}"
LOOP_LABEL="${LOOP_LABEL:-}"
LOOP_EMPTY_SLEEP="${LOOP_EMPTY_SLEEP:-60}"
LOOP_LOG_DIR="${LOOP_LOG_DIR:-$HOME/data/plan-do-review-loop}"
FAIL_LABEL="plan-do-review-loop-failed"
NOT_READY_LABEL="not-ready"

log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"; }

on_signal() {
  log "Received signal, shutting down."
  exit 0
}
trap on_signal INT TERM

# --- Preflight ---
command -v gh >/dev/null 2>&1 || { echo "ERROR: gh CLI not found" >&2; exit 1; }
command -v copilot >/dev/null 2>&1 || { echo "ERROR: copilot CLI not found" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "ERROR: gh not authenticated (run 'gh auth login')" >&2; exit 1; }

ME="$(gh api user -q .login)"
[[ -n "$ME" ]] || { echo "ERROR: could not resolve gh login (gh api user)" >&2; exit 1; }

mkdir -p "$LOOP_LOG_DIR"

# Ensure labels exist (idempotent).
gh label create "$FAIL_LABEL" \
  --color D93F0B \
  --description "plan-do-review-loop attempted and failed" \
  >/dev/null 2>&1 || true
gh label create "$NOT_READY_LABEL" \
  --color FBCA04 \
  --description "Issue is not ready for plan-do-review" \
  >/dev/null 2>&1 || true

log "=== plan-do-review-loop ==="
log "User:      $ME"
log "Model:     $LOOP_MODEL"
log "Label:     ${LOOP_LABEL:-(none)}"
log "Sleep:     ${LOOP_EMPTY_SLEEP}s"
log "Log dir:   $LOOP_LOG_DIR"

# --- Main loop ---
while true; do
  search="sort:created-asc -label:$FAIL_LABEL -label:$NOT_READY_LABEL"
  if [[ -n "$LOOP_LABEL" ]]; then
    search="$search label:$LOOP_LABEL"
  fi

  issue_num="$(gh issue list \
    --state open \
    --assignee '' \
    --search "$search" \
    --json number \
    --limit 1 \
    --jq '.[0].number // empty')"

  if [[ -z "$issue_num" ]]; then
    log "No eligible issues. Sleeping ${LOOP_EMPTY_SLEEP}s…"
    sleep "$LOOP_EMPTY_SLEEP"
    continue
  fi

  log "Picked issue #$issue_num"

  # Assignment lock. If this fails (e.g., raced with another worker), skip.
  if ! gh issue edit "$issue_num" --add-assignee "@me" >/dev/null 2>&1; then
    log "Failed to assign #$issue_num to @me; skipping."
    sleep 5
    continue
  fi

  ts="$(date +%Y%m%d-%H%M%S)"
  logfile="$LOOP_LOG_DIR/${ts}-issue-${issue_num}.log"
  log "Running copilot on #$issue_num → $logfile"

  set +e
  copilot \
    --model "$LOOP_MODEL" \
    --allow-all-tools \
    --allow-all-paths \
    --log-dir "$LOOP_LOG_DIR/copilot-logs" \
    -p "/plan-do-review $issue_num" \
    2>&1 | tee "$logfile"
  rc="${PIPESTATUS[0]}"
  set -e

  if [[ "$rc" -ne 0 ]]; then
    log "copilot exited $rc on #$issue_num — labeling failed and unassigning."
    gh issue edit "$issue_num" \
      --add-label "$FAIL_LABEL" \
      --remove-assignee "$ME" \
      >/dev/null 2>&1 || log "WARN: failed to update #$issue_num after failure"
  else
    log "Completed #$issue_num (copilot exit 0)."
  fi
done
