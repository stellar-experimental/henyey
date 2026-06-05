#!/usr/bin/env bash
# pipeline-anomaly-log.sh — per-session anomaly log for the project-loop
# self-reflection pass.
#
# The orchestrator appends observable "the pipeline itself is buggy" signals
# during each pass (merge-helper fallback, repeated flaky re-run, refuted
# reviewer finding, bounce-cap hit, escaped workspace, ...). The idle-transition
# reflection pass dumps the log, files `self-improvement` issues, then clears it.
#
# Source-safe: define functions, never `exit` when sourced. The log path is
# `$PIPELINE_ANOMALY_LOG` if set (used by tests), else
# `<real-home>/data/<session>/pipeline-anomaly.log`, where <real-home> is the
# passwd-derived home from agent-worktree-contract.sh (immune to $HOME poisoning).

# Resolve the per-session anomaly log path. Honors a PIPELINE_ANOMALY_LOG
# override (tests); otherwise derives <real-home>/data/<session>/pipeline-anomaly.log.
anomaly_log_path() {
  if [[ -n "${PIPELINE_ANOMALY_LOG:-}" ]]; then
    printf '%s\n' "$PIPELINE_ANOMALY_LOG"
    return 0
  fi
  local real_home
  if command -v _contract_real_home >/dev/null 2>&1; then
    real_home="$(_contract_real_home)"
  else
    real_home="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
    [[ -z "$real_home" ]] && real_home="$(eval echo "~$(id -un)")"
  fi
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-default}}"
  printf '%s\n' "$real_home/data/$session_id/pipeline-anomaly.log"
}

# anomaly_log_append "<signal>" "<evidence>" — append a timestamped TSV line.
anomaly_log_append() {
  local signal="$1" evidence="$2"
  local log_file
  log_file="$(anomaly_log_path)" || return 1
  mkdir -p "$(dirname "$log_file")" || return 1
  # Strip embedded tabs/newlines so each anomaly stays on one TSV line.
  signal="${signal//$'\t'/ }"; signal="${signal//$'\n'/ }"
  evidence="${evidence//$'\t'/ }"; evidence="${evidence//$'\n'/ }"
  # Fail loud on append error (e.g. read-only dir, ENOSPC) so a lost anomaly
  # signal surfaces instead of being silently dropped.
  printf '%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$signal" "$evidence" \
    >> "$log_file" || return 1
}

# anomaly_log_dump — print the current session's log (nothing if absent).
anomaly_log_dump() {
  local log_file
  log_file="$(anomaly_log_path)" || return 1
  [[ -f "$log_file" ]] && cat "$log_file"
  return 0
}

# anomaly_log_clear — truncate the log (called after a reflection pass).
anomaly_log_clear() {
  local log_file
  log_file="$(anomaly_log_path)" || return 1
  [[ -f "$log_file" ]] && : > "$log_file"
  return 0
}

# Self-test: only runs when executed directly, never when sourced. The guard is
# quoted with a :- default so it can never abort under a future `setopt nounset`
# / `set -u` caller (zsh has no BASH_SOURCE → the default yields ""; "" never
# equals "$0", so the self-test stays bash-execute-only — behavior unchanged).
if [[ "${BASH_SOURCE[0]:-}" == "${0}" ]]; then
  set -euo pipefail
  tmp_log="$(mktemp)"
  trap 'rm -f "$tmp_log"' EXIT
  export PIPELINE_ANOMALY_LOG="$tmp_log"

  anomaly_log_clear
  [[ -z "$(anomaly_log_dump)" ]] || { echo "FAIL: expected empty after clear" >&2; exit 1; }

  anomaly_log_append "merge-helper-fallback" "PR #123 fell back to gh pr merge"
  anomaly_log_append "flaky-rerun" "quickstart re-run 2x"
  lines="$(anomaly_log_dump | wc -l | tr -d ' ')"
  [[ "$lines" == "2" ]] || { echo "FAIL: expected 2 lines, got $lines" >&2; exit 1; }
  anomaly_log_dump | grep -q "merge-helper-fallback" || { echo "FAIL: missing append" >&2; exit 1; }

  anomaly_log_clear
  [[ -z "$(anomaly_log_dump)" ]] || { echo "FAIL: expected empty after second clear" >&2; exit 1; }

  echo "pipeline-anomaly-log.sh self-test: OK"
fi
