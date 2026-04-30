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
# Priority: urgent → high → medium → low → rest (all unassigned).
# Within each tier, oldest first. Excludes not-ready and failed issues.
# Skips issues already assigned to anyone (uses `no:assignee` search qualifier).
# Collects up to LOOP_BATCH_SIZE issues across priority tiers.
# Outputs one line per issue: "<number> <title>"
# Returns 0 on success/empty, 1 on API error.
select_issues() {
  local needed="$LOOP_BATCH_SIZE"
  local found=0
  local seen_numbers=""
  local json number title

  # Priority 1–4: unassigned issues by priority label (urgent → high → medium → low),
  # oldest first within each tier.
  local priority
  for priority in urgent high medium low; do
    [[ "$found" -ge "$needed" ]] && break

    local remaining=$((needed - found))
    if ! json="$(gh issue list \
      --state open \
      --label "$priority" \
      --search 'sort:created-asc no:assignee -label:plan-do-review-loop-failed -label:not-ready' \
      --json number,title \
      --limit "$remaining")"; then
      return 1
    fi

    local count
    count="$(jq 'length' <<<"$json")"
    local i
    for ((i = 0; i < count && found < needed; i++)); do
      number="$(jq -r ".[$i].number" <<<"$json")"
      # Skip duplicates (an issue may match multiple priority labels)
      if [[ " $seen_numbers " == *" $number "* ]]; then
        continue
      fi
      title="$(jq -r ".[$i].title // \"\"" <<<"$json")"
      echo "${number} ${title}"
      seen_numbers="$seen_numbers $number"
      found=$((found + 1))
    done
  done

  # Priority 5: any remaining eligible issue (no priority label requirement), oldest first.
  if [[ "$found" -lt "$needed" ]]; then
    local remaining=$((needed - found))
    if ! json="$(gh issue list \
      --state open \
      --search 'sort:created-asc no:assignee -label:plan-do-review-loop-failed -label:not-ready' \
      --json number,title \
      --limit "$remaining")"; then
      return 1
    fi

    local count
    count="$(jq 'length' <<<"$json")"
    local i
    for ((i = 0; i < count && found < needed; i++)); do
      number="$(jq -r ".[$i].number" <<<"$json")"
      if [[ " $seen_numbers " == *" $number "* ]]; then
        continue
      fi
      title="$(jq -r ".[$i].title // \"\"" <<<"$json")"
      echo "${number} ${title}"
      seen_numbers="$seen_numbers $number"
      found=$((found + 1))
    done
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
  rm -f "$LOOP_LOG_DIR/.progress-${issue}"
}

# Count issue comments matching plan-do-review progress markers.
# Returns a fingerprint (comment count) that changes when the skill makes
# forward progress (new proposal draft, critic response, converged proposal,
# review-fix report, implementation complete, not-ready triage, redirect).
count_progress_markers() {
  local issue="$1"
  gh issue view "$issue" --json comments \
    --jq '[.comments[].body | select(
      test("Proposal Draft|Critic Response|Converged Proposal|Review-Fix Report|Implementation Complete|Marking as not-ready|⏩ This issue is blocked")
    )] | length' 2>/dev/null || echo "0"
}

# Check whether the issue reached a terminal state (no further retries needed).
# Terminal states: closed, labeled not-ready, unassigned (redirect/triage).
is_terminal() {
  local issue="$1"
  local json
  json="$(gh issue view "$issue" --json state,labels,assignees 2>/dev/null)" || return 1

  local state
  state="$(jq -r '.state' <<<"$json")"
  [[ "$state" == "CLOSED" ]] && return 0

  # not-ready label means triage decided it's not actionable
  local has_not_ready
  has_not_ready="$(jq -r '[.labels[].name] | any(. == "not-ready")' <<<"$json")"
  [[ "$has_not_ready" == "true" ]] && return 0

  # Unassigned means the skill handed it off (redirect or triage)
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
while true; do
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
