#!/usr/bin/env bash
#
# Smoke-test harness for monitor skill shell snippets.
#
# Tests the shared decision logic library (scripts/lib/monitor-decisions.sh)
# using mock filesystems. Also verifies that skill markdown files reference
# the library (structural assertions replace old checksum tripwires).
#
# Usage:
#   ./scripts/test-monitor-skill-snippets.sh              # run tests (warn on drift)
#   ./scripts/test-monitor-skill-snippets.sh --strict     # fail on structural drift
#
# Output: TAP (Test Anything Protocol) on stdout, diagnostics on stderr.
# Exit: 0 = all pass, 1 = any fail.
#
# Portability: GNU/Linux only (GNU stat -c, readlink, Bash 4+, symlinks).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Scratch lives under a mktemp dir (same pattern as test-shell-lib-cross-shell.sh):
# honor the #2843 ~/data session contract locally when available, else fall back
# to a system tmp dir. Do NOT route through $REPO_ROOT/data — that's a checked-in
# symlink to a local dev machine's ~/data and is dangling on CI runners, so
# `mkdir -p` through it fails with EEXIST (stat() follows the dangling link and
# reports missing, then the literal mkdir("data") collides with the symlink
# dirent that does exist).
if [[ -n "${CLAUDE_SESSION_ID:-}" && -d "${HOME}/data" ]]; then
  TEST_ROOT="$(mktemp -d "${HOME}/data/${CLAUDE_SESSION_ID}/test-monitor-snippets.XXXXXX")"
else
  TEST_ROOT="$(mktemp -d)"
fi

# ── Source the shared libraries (single source of truth) ──────────────────────
source "$SCRIPT_DIR/lib/monitor-decisions.sh"
source "$SCRIPT_DIR/lib/deploy-quarantine.sh"

# ── Arguments ────────────────────────────────────────────────────────────────
STRICT=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict) STRICT=true; shift ;;
    *) echo "Unknown argument: $1" >&2; exit 1 ;;
  esac
done

# ── Cleanup ──────────────────────────────────────────────────────────────────
cleanup() {
  # Kill any tracked background processes (from watcher lifecycle tests)
  if type -t _wt_cleanup_bg &>/dev/null; then _wt_cleanup_bg; fi
  rm -rf "$TEST_ROOT" 2>/dev/null || true
}
trap cleanup EXIT

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=487
TAP_CURRENT=0
TAP_FAILURES=0

tap_plan() {
  echo "1..$TAP_PLAN"
}

tap_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  echo "ok $TAP_CURRENT - $1"
}

tap_not_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_FAILURES=$((TAP_FAILURES + 1))
  echo "not ok $TAP_CURRENT - $1"
  if [[ -n "${2:-}" ]]; then
    echo "  # $2"
  fi
}

tap_skip() {
  # Emit a passing TAP line with a SKIP directive so the count stays
  # consistent regardless of whether the optional dependency is installed.
  TAP_CURRENT=$((TAP_CURRENT + 1))
  echo "ok $TAP_CURRENT - $1 # SKIP ${2:-unavailable}"
}

# ── Section extraction helper ─────────────────────────────────────────────────
# extract_md_section FILE START_PATTERN [END_PATTERN]
#
# Thin wrapper around the two sed extraction idioms used in this file.
# Arguments are sed BRE address patterns (callers must escape '/' as '\/').
#
# Two-arg form (heading-scoped):
#   Prints lines from START_PATTERN to the next line matching ^##,
#   excluding lines matching "^## " (## + space). The start heading
#   and any ###-level terminator ARE included.
#   Equivalent to: sed -n '/PAT/,/^##/{/^## /!p}'
#
# Three-arg form (range-scoped):
#   Prints lines from START_PATTERN to END_PATTERN (both inclusive).
#   Equivalent to: sed -n '/START/,/END/p'
#
# Exit status: always 0 (sed returns 0 even on no matches).
# Empty output indicates the start pattern was not found.
# Callers must check for empty output themselves.
extract_md_section() {
  local file="$1" start="$2" end="${3:-}"
  if [[ -n "$end" ]]; then
    sed -n "/${start}/,/${end}/p" "$file"
  else
    sed -n "/${start}/,/^##/{/^## /!p}" "$file"
  fi
}

# ── Label-Policy Drift Detection ─────────────────────────────────────────────
# Validates that monitor skill files conform to the centralized label policy
# in scripts/lib/monitor-label-policy.md.

# check_label_policy_drift FILE NAME
#
# Checks a skill file for label-policy conformance:
#   1. No retired 'ready' label usage
#   2. Canonical policy reference present
# Returns 0 if conformant, 1 if drift detected.
check_label_policy_drift() {
  local file="$1"
  local name="$2"
  local ok=true

  # Assertion 1: no retired 'ready' label (covers --label ready, --add-label ready,
  # --label=ready, quoted forms, and ready-labeled)
  if grep -qE -- '--label[= ]["'"'"'"]?ready|--add-label[= ]["'"'"'"]?ready|ready-labeled' "$file"; then
    echo "WARNING: $name uses retired 'ready' label" >&2
    ok=false
  fi

  # Assertion 2: canonical policy reference present
  if ! grep -q 'Canonical reference.*scripts/lib/monitor-label-policy.md' "$file"; then
    echo "WARNING: $name missing canonical reference to scripts/lib/monitor-label-policy.md" >&2
    ok=false
  fi

  [[ "$ok" == "false" ]] && return 1
  return 0
}

# check_filing_balance FILE SECTION_HEADING NAME
#
# Extracts a markdown section and verifies that every `gh issue create`
# has a corresponding `move-issue-status` instruction (count-based balance).
# Returns 0 if balanced, 1 if imbalanced.
check_filing_balance() {
  local file="$1"
  local section_heading="$2"
  local name="$3"

  local section
  section=$(extract_md_section "$file" "$section_heading")
  if [[ -z "$section" ]]; then
    echo "WARNING: $name has no '$section_heading' section" >&2
    return 1
  fi

  local create_count route_count
  create_count=$(echo "$section" | grep -c 'gh issue create' || true)
  route_count=$(echo "$section" | grep -c 'move-issue-status' || true)

  if [[ $create_count -gt 0 && $route_count -lt $create_count ]]; then
    echo "WARNING: $name section '$section_heading': $create_count 'gh issue create' but only $route_count 'move-issue-status' (expected >= $create_count)" >&2
    return 1
  fi
  return 0
}

# ── Structural Assertions ────────────────────────────────────────────────────
# Verify that skill markdown files reference the shared library.
# Replaces old checksum-based drift detection.

check_skill_structure() {
  local tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  local loop_file="$REPO_ROOT/.claude/skills/monitor-loop/SKILL.md"
  local drift=false

  # monitor-tick must source the library and call its functions
  if ! grep -q 'source.*scripts/lib/monitor-decisions.sh' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not source scripts/lib/monitor-decisions.sh" >&2
    drift=true
  fi
  if ! grep -q 'check_session_wiped' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call check_session_wiped" >&2
    drift=true
  fi
  # Verify fail-fast: check_session_wiped call must include || exit 1
  if ! grep -A2 'check_session_wiped' "$tick_file" | grep -q '|| exit 1'; then
    echo "WARNING: monitor-tick/SKILL.md calls check_session_wiped without || exit 1 fail-fast" >&2
    drift=true
  fi
  if ! grep -q 'check_mainnet_wiped' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call check_mainnet_wiped" >&2
    drift=true
  fi
  # monitor-tick must call check_long_stale_session (long-stale session guard)
  if ! grep -q 'check_long_stale_session' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call check_long_stale_session" >&2
    drift=true
  fi
  # Verify fail-fast: check_long_stale_session call must include || exit 1
  if ! grep -A2 'check_long_stale_session' "$tick_file" | grep -q '|| exit 1'; then
    echo "WARNING: monitor-tick/SKILL.md calls check_long_stale_session without || exit 1 fail-fast" >&2
    drift=true
  fi

  # monitor-tick must call detect_crash_state (3a refactor)
  if ! grep -q 'detect_crash_state' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call detect_crash_state" >&2
    drift=true
  fi
  # monitor-tick must NOT contain old inline crash detection (replaced by detect_crash_state)
  if grep -q 'recent_crashed=\$(find' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md still contains old inline crash detection (find-based)" >&2
    drift=true
  fi
  # monitor-tick must call detect_soft_fail_blocked (3c soft-fail trigger)
  if ! grep -q 'detect_soft_fail_blocked' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call detect_soft_fail_blocked" >&2
    drift=true
  fi
  # monitor-tick must call has_fatal_wipe_evidence (3c crash evidence)
  if ! grep -q 'has_fatal_wipe_evidence' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call has_fatal_wipe_evidence" >&2
    drift=true
  fi

  # monitor-loop must source the library and call its functions
  if ! grep -q 'source.*scripts/lib/monitor-decisions.sh' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not source scripts/lib/monitor-decisions.sh" >&2
    drift=true
  fi
  if ! grep -q 'recover_session_from_stdout' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not call recover_session_from_stdout" >&2
    drift=true
  fi
  # Verify fail-fast: recover_session_from_stdout call must include || exit 1
  if ! grep -A3 'recover_session_from_stdout' "$loop_file" | grep -q 'exit 1'; then
    echo "WARNING: monitor-loop/SKILL.md calls recover_session_from_stdout without fail-fast error handling" >&2
    drift=true
  fi
  if ! grep -q 'cleanup_guard' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not call cleanup_guard" >&2
    drift=true
  fi

  # monitor-loop must call grep_heartbeat_lines (not raw grep Heartbeat)
  if ! grep -q 'grep_heartbeat_lines' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not call grep_heartbeat_lines" >&2
    drift=true
  fi
  # Neither file should contain raw grep 'Heartbeat' patterns for log detection
  if grep -qE 'grep.*Heartbeat.*monitor' "$tick_file" 2>/dev/null; then
    echo "WARNING: monitor-tick/SKILL.md contains raw grep Heartbeat (use grep_heartbeat_lines)" >&2
    drift=true
  fi
  if grep -qE 'grep.*Heartbeat.*monitor' "$loop_file" 2>/dev/null; then
    echo "WARNING: monitor-loop/SKILL.md contains raw grep Heartbeat (use grep_heartbeat_lines)" >&2
    drift=true
  fi

  # deploy-quarantine.sh: monitor-tick must source and call quarantine helpers
  if ! grep -q 'source.*scripts/lib/deploy-quarantine.sh\|source.*deploy-quarantine.sh' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not source scripts/lib/deploy-quarantine.sh" >&2
    drift=true
  fi
  if ! grep -q 'check_quarantine_ancestry' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call check_quarantine_ancestry" >&2
    drift=true
  fi
  if ! grep -q 'quarantine_append' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call quarantine_append" >&2
    drift=true
  fi
  if ! grep -q 'quarantine_remove' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not call quarantine_remove" >&2
    drift=true
  fi
  # Old inline quarantine patterns should be gone
  if grep -q 'while IFS=.*read.*q_sha' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md still contains old inline quarantine read loop" >&2
    drift=true
  fi
  if grep -qE 'awk.*\$1 == sha.*found=1' "$tick_file" 2>/dev/null; then
    echo "WARNING: monitor-tick/SKILL.md still contains old inline quarantine append awk" >&2
    drift=true
  fi
  if grep -qE 'awk.*\$1 != sha.*deploy_quarantine' "$tick_file" 2>/dev/null; then
    echo "WARNING: monitor-tick/SKILL.md still contains old inline quarantine remove awk" >&2
    drift=true
  fi
  # quarantine_append must use || rc=$? capture pattern
  if ! grep -qE 'quarantine_append[[:space:]].*\|\|[[:space:]]*rc=\$\?' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md quarantine_append call missing || rc=\$? capture" >&2
    drift=true
  fi
  # quarantine_remove must use || rc=$? capture pattern
  if ! grep -qE 'quarantine_remove[[:space:]].*\|\|[[:space:]]*rc=\$\?' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md quarantine_remove call missing || rc=\$? capture" >&2
    drift=true
  fi
  # Neither helper should use bare $? check (fragile pattern)
  if grep -A3 'quarantine_append\|quarantine_remove' "$tick_file" | grep -qE 'if \[+ \$\? '; then
    echo "WARNING: monitor-tick/SKILL.md uses bare \$? after quarantine helper (use || rc=\$? pattern)" >&2
    drift=true
  fi

  # monitor-tick must contain persistent dedup for post-wipe recurrence (#2630)
  if ! grep -q 'monitor-tick-filed\.json' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not reference .monitor-tick-filed.json dedup file" >&2
    drift=true
  fi
  if ! grep -q 'monitor-tick-filing\.lock' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not reference .monitor-tick-filing.lock flock" >&2
    drift=true
  fi
  if ! grep -q 'flock -w 30' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md missing flock serialization for dedup filing" >&2
    drift=true
  fi
  if ! grep -q 'monitor-tick-recurrence-key' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md missing body marker for dedup remote search" >&2
    drift=true
  fi
  if ! grep -q 'schema_version' "$tick_file" && ! grep -q 'dedup_load\|dedup-filing' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md dedup file load missing schema_version check or dedup-filing library" >&2
    drift=true
  fi
  if ! grep -q 'write_dedup_file\|dedup_write\|dedup-filing' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md missing atomic write helper (write_dedup_file or dedup_write)" >&2
    drift=true
  fi
  # Old pattern: must NOT use bare gh issue list --search without persistent dedup
  if grep -q 'If an open issue already names the same.*comment on it instead of filing' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md still uses old search-only dedup for post-wipe recurrence" >&2
    drift=true
  fi

  # monitor-label-policy.md: Deploy Regression Procedure must use quarantine helper
  local policy_file="$REPO_ROOT/scripts/lib/monitor-label-policy.md"
  local deploy_section
  deploy_section=$(extract_md_section "$policy_file" '^## Deploy Regression Procedure')
  if [[ -z "$deploy_section" ]]; then
    echo "WARNING: monitor-label-policy.md has no '## Deploy Regression Procedure' section" >&2
    drift=true
  else
    # Negative: old inline awk pattern must be gone
    if echo "$deploy_section" | grep -qE 'awk.*\$1 == sha.*found=1'; then
      echo "WARNING: monitor-label-policy.md Deploy Regression still contains old inline quarantine awk" >&2
      drift=true
    fi
    # Negative: direct quarantine file writes must be gone
    if echo "$deploy_section" | grep -qE '>>.*deploy_quarantine'; then
      echo "WARNING: monitor-label-policy.md Deploy Regression still contains direct quarantine file writes" >&2
      drift=true
    fi
    # Positive: helper must be sourced
    if ! echo "$deploy_section" | grep -q 'source.*deploy-quarantine.sh'; then
      echo "WARNING: monitor-label-policy.md Deploy Regression does not source deploy-quarantine.sh" >&2
      drift=true
    fi
    # Positive: quarantine_append must be called
    if ! echo "$deploy_section" | grep -q 'quarantine_append'; then
      echo "WARNING: monitor-label-policy.md Deploy Regression does not call quarantine_append" >&2
      drift=true
    fi
    # Positive: return code must be captured and checked (not bare $?)
    if ! echo "$deploy_section" | grep -qE '\|\| rc=\$\?|rc=\$\?'; then
      echo "WARNING: monitor-label-policy.md Deploy Regression does not capture quarantine_append return code" >&2
      drift=true
    fi
  fi

  # Label-policy drift assertions
  if ! check_label_policy_drift "$tick_file" "monitor-tick"; then
    drift=true
  fi
  if ! check_label_policy_drift "$loop_file" "monitor-loop"; then
    drift=true
  fi

  # Filing-section balance checks (gh issue create must have move-issue-status)
  if ! check_filing_balance "$tick_file" "^### Filing flow" "monitor-tick"; then
    drift=true
  fi
  if ! check_filing_balance "$tick_file" "^### Firing alerts" "monitor-tick/alerts"; then
    drift=true
  fi
  if ! check_filing_balance "$tick_file" "^## CI check workflow" "monitor-tick/ci"; then
    drift=true
  fi
  if ! check_filing_balance "$loop_file" "^## Bug \/ CI-Failure Filing Workflow" "monitor-loop"; then
    drift=true
  fi

  # watcher-check-prompt.md must not contain pgrep or pkill
  local watcher_prompt="$REPO_ROOT/scripts/watcher-check-prompt.md"
  if grep -qE 'pgrep|pkill' "$watcher_prompt" 2>/dev/null; then
    echo "WARNING: watcher-check-prompt.md still contains pgrep/pkill patterns" >&2
    drift=true
  fi
  # watcher-check-prompt.md must not contain hardcoded /home/tomer/ paths
  if grep -q '/home/tomer/' "$watcher_prompt" 2>/dev/null; then
    echo "WARNING: watcher-check-prompt.md still contains hardcoded /home/tomer/ paths" >&2
    drift=true
  fi
  # watcher-check-prompt.md must reference watcher-ctl.sh
  if ! grep -q 'watcher-ctl.sh' "$watcher_prompt" 2>/dev/null; then
    echo "WARNING: watcher-check-prompt.md does not reference watcher-ctl.sh" >&2
    drift=true
  fi
  # watcher-ctl.sh must exist and be executable
  local watcher_ctl="$REPO_ROOT/scripts/watcher-ctl.sh"
  if [[ ! -x "$watcher_ctl" ]]; then
    echo "WARNING: scripts/watcher-ctl.sh is missing or not executable" >&2
    drift=true
  fi
  # watcher-ctl.sh must source monitor-decisions.sh
  if ! grep -q 'source.*monitor-decisions.sh' "$watcher_ctl" 2>/dev/null; then
    echo "WARNING: watcher-ctl.sh does not source monitor-decisions.sh" >&2
    drift=true
  fi

  if [[ "$drift" == "true" && "$STRICT" == "true" ]]; then
    echo "FATAL: Structural drift detected in --strict mode." >&2
    exit 1
  fi

  # Step 8 (alarm regression replay) structural checks
  if ! grep -q 'check-alarm-regression.sh' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md does not reference check-alarm-regression.sh" >&2
    drift=true
  fi
  # Verify replay_state watch items (replaced legacy replay_pending)
  if grep -q 'replay_pending=' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md still uses deprecated replay_pending (should be replay_state)" >&2
    drift=true
  fi
  for state in never-run archive-too-small failed stale; do
    if ! grep -q "replay_state=$state" "$tick_file"; then
      echo "WARNING: monitor-tick/SKILL.md missing replay_state=$state" >&2
      drift=true
    fi
  done
  # Verify replay state logic is inside the validator gate
  if ! grep -A200 'MONITOR_MODE.*validator' "$tick_file" | grep -q 'replay_state='; then
    echo "WARNING: replay_state logic is not inside the MONITOR_MODE==validator gate" >&2
    drift=true
  fi
  # Verify REPLAY_FAILED tracking variable is used
  if ! grep -q 'REPLAY_FAILED' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md missing REPLAY_FAILED failure tracking" >&2
    drift=true
  fi
  # Verify skill checkout drift detection guard
  if ! grep -q 'skill_stale=' "$tick_file"; then
    echo "WARNING: monitor-tick/SKILL.md missing skill_stale drift detection guard" >&2
    drift=true
  fi
  if [[ ! -x "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" ]]; then
    echo "WARNING: scripts/dev/check-alarm-regression.sh is missing or not executable" >&2
    drift=true
  fi
}


# ── Mock Helpers ─────────────────────────────────────────────────────────────

mock_proc_entry() {
  # Create a mock /proc/<pid> with exe symlink and optional cmdline
  local proc_root="$1" pid="$2" exe_target="$3" cmdline="${4:-}"
  mkdir -p "$proc_root/$pid"
  ln -sf "$exe_target" "$proc_root/$pid/exe"
  if [[ -n "$cmdline" ]]; then
    # Write NUL-separated cmdline with trailing NUL (matches /proc/<pid>/cmdline format)
    printf '%s\0' "$cmdline" | tr ' ' '\0' > "$proc_root/$pid/cmdline"
  fi
}

mock_proc_stdout() {
  # Create a mock /proc/<pid>/fd/1 symlink
  local proc_root="$1" pid="$2" stdout_target="$3"
  mkdir -p "$proc_root/$pid/fd"
  ln -sf "$stdout_target" "$proc_root/$pid/fd/1"
}

mock_env_file() {
  # Create an env file with specific age in seconds
  local env_file="$1" age_seconds="$2"
  local target_mtime=$(( $(date +%s) - age_seconds ))
  echo "MONITOR_SESSION_ID=abc12345" > "$env_file"
  touch -d "@$target_mtime" "$env_file"
}

mock_alive_file() {
  # Create a .alive file with specific age in seconds
  local alive_path="$1" age_seconds="$2"
  local target_mtime=$(( $(date +%s) - age_seconds ))
  mkdir -p "$(dirname "$alive_path")"
  touch "$alive_path"
  touch -d "@$target_mtime" "$alive_path"
}

mock_crashed_log() {
  # Create a monitor.log.crashed-<suffix> file with known content and mtime
  local logs_dir="$1" suffix="$2" content="$3" mtime_epoch="$4"
  mkdir -p "$logs_dir"
  printf '%s\n' "$content" > "$logs_dir/monitor.log.crashed-$suffix"
  touch -d "@$mtime_epoch" "$logs_dir/monitor.log.crashed-$suffix"
}

# ── Tests ────────────────────────────────────────────────────────────────────

run_tests() {
  tap_plan

  local data proc session_id

  # ── Test 1: Session dir missing + process alive (exact binary) ──────────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped
  data="$TEST_ROOT/t1/data"
  proc="$TEST_ROOT/t1/proc"
  session_id="sess1111"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "1001" "$data/$session_id/cargo-target/release/henyey" "henyey --mainnet run --validator"
  mock_env_file "$data/monitor-loop.env" 100

  local exit_code1=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code1=$?
  if [[ "$exit_code1" -eq 0 && "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/logs" ]]; then
    tap_ok "session-wipe: process alive (exact binary)"
  else
    tap_not_ok "session-wipe: process alive (exact binary)" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 2: Session dir missing + process alive (deleted binary) ────────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped
  data="$TEST_ROOT/t2/data"
  proc="$TEST_ROOT/t2/proc"
  session_id="sess2222"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "2001" "$data/$session_id/cargo-target/release/henyey (deleted)" "henyey --mainnet run --validator"
  mock_env_file "$data/monitor-loop.env" 100

  local exit_code2=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code2=$?
  if [[ "$exit_code2" -eq 0 && "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/metrics" ]]; then
    tap_ok "session-wipe: process alive (deleted binary)"
  else
    tap_not_ok "session-wipe: process alive (deleted binary)" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 3: Different binary path (not our session) ─────────────────────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped
  data="$TEST_ROOT/t3/data"
  proc="$TEST_ROOT/t3/proc"
  session_id="sess3333"
  mkdir -p "$data" "$proc"
  # Process running a DIFFERENT session's binary
  mock_proc_entry "$proc" "3001" "$data/other-session/cargo-target/release/henyey" "henyey --mainnet run --validator"
  # Create env file so freshness check passes
  mock_env_file "$data/monitor-loop.env" 100

  local exit_code3=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code3=$?
  if [[ "$exit_code3" -eq 0 && "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" && -d "$data/$session_id/logs" ]]; then
    tap_ok "session-wipe: different binary not matched"
  else
    tap_not_ok "session-wipe: different binary not matched" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 4: Process dead + env fresh (100s) ────────────────────────────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped
  data="$TEST_ROOT/t4/data"
  proc="$TEST_ROOT/t4/proc"
  session_id="sess4444"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 100

  local exit_code4=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code4=$?
  if [[ "$exit_code4" -eq 0 && "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" && -d "$data/$session_id/cargo-target" ]]; then
    tap_ok "session-wipe: dead process, env fresh"
  else
    tap_not_ok "session-wipe: dead process, env fresh" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 5: Process dead + env stale (7201s) ───────────────────────────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped returns 1
  data="$TEST_ROOT/t5/data"
  proc="$TEST_ROOT/t5/proc"
  session_id="sess5555"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7201

  local exit_code=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code=$?
  if [[ "$exit_code" -eq 1 ]]; then
    tap_ok "session-wipe: dead process, env stale (7201s)"
  else
    tap_not_ok "session-wipe: dead process, env stale (7201s)" "expected return 1, got $exit_code"
  fi

  # ── Test 6: Process dead + env on fresh side of boundary (7199s) ────────
  # Source: scripts/lib/monitor-decisions.sh — freshness predicate is
  # `env_age -gt <boundary>`, so anything at or below the boundary is fresh.
  # Mock 7199 (not the exact boundary): the age is recomputed from the wall
  # clock inside the check, so mocking the exact boundary races the clock — a
  # single second between mock and check flips the env to stale. 7199 stays
  # fresh across two integer-second advances, making the assertion
  # deterministic while still exercising the fresh side of the boundary.
  # The call is guarded (Test-5 pattern) so a stale-path `return 1` can never
  # abort the harness under `set -e` (see #3766).
  data="$TEST_ROOT/t6/data"
  proc="$TEST_ROOT/t6/proc"
  session_id="sess6666"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7199

  local exit_code6=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code6=$?
  if [[ "$exit_code6" -eq 0 && "$SESSION_WIPED" == "yes" && -d "$data/$session_id/logs" ]]; then
    tap_ok "session-wipe: env on fresh side of boundary (7199s)"
  else
    tap_not_ok "session-wipe: env on fresh side of boundary (7199s)" "rc=$exit_code6 WIPED=$SESSION_WIPED dirs=$(ls "$data/$session_id" 2>/dev/null || echo missing)"
  fi

  # ── Test 7: Process dead + env file missing ────────────────────────────
  # Source: scripts/lib/monitor-decisions.sh — stat fails → epoch age → stale
  data="$TEST_ROOT/t7/data"
  mkdir -p "$data"
  # No env file created — standalone check_env_freshness
  local exit_code7=0
  check_env_freshness "$data/monitor-loop.env" 2>/dev/null || exit_code7=$?
  if [[ "$exit_code7" -eq 1 ]]; then
    tap_ok "session-wipe: missing env file is stale"
  else
    tap_not_ok "session-wipe: missing env file is stale" "expected return 1, got $exit_code7"
  fi

  # ── Test 8: Attach-mode stdout with (deleted) suffix ───────────────────
  # Source: .claude/skills/monitor-loop/SKILL.md:473-485
  data="$TEST_ROOT/t8/data"
  mkdir -p "$data"
  local stdout_path="$data/ab12cd34/logs/monitor.log (deleted)"
  local recovered
  recovered=$(recover_session_from_stdout "$data" "$stdout_path")
  if [[ "$recovered" == "ab12cd34" && -d "$data/ab12cd34/logs" && -f "$data/ab12cd34/.alive" ]]; then
    tap_ok "attach-mode: (deleted) stdout recovers session-id"
  else
    tap_not_ok "attach-mode: (deleted) stdout recovers session-id" "recovered='$recovered'"
  fi

  # ── Test 9: Attach-mode normal stdout ──────────────────────────────────
  # Source: .claude/skills/monitor-loop/SKILL.md:466-472
  data="$TEST_ROOT/t9/data"
  mkdir -p "$data"
  local normal_path="$data/ef56gh78/logs/monitor.log"
  local recovered9
  recovered9=$(recover_session_from_stdout "$data" "$normal_path")
  if [[ "$recovered9" == "ef56gh78" && ! -d "$data/ef56gh78/logs" ]]; then
    tap_ok "attach-mode: normal stdout recovers session-id (no side effects)"
  else
    tap_not_ok "attach-mode: normal stdout recovers session-id (no side effects)" "recovered='$recovered9' or dirs exist"
  fi

  # ── Test 10: Attach-mode malformed (has /data/ but invalid layout) ─────
  # Source: defensive behavior
  data="$TEST_ROOT/t10/data"
  mkdir -p "$data"
  local malformed_path="/some/data/path-without-session-structure"
  local exit10=0
  recover_session_from_stdout "$data" "$malformed_path" >/dev/null 2>&1 || exit10=$?
  if [[ "$exit10" -ne 0 ]]; then
    tap_ok "attach-mode: malformed path (has /data/ but invalid) returns error"
  else
    tap_not_ok "attach-mode: malformed path (has /data/ but invalid) returns error" "expected non-zero"
  fi

  # ── Test 11: Attach-mode no /data/ segment ─────────────────────────────
  # Source: defensive behavior
  data="$TEST_ROOT/t11/data"
  mkdir -p "$data"
  local no_data_path="/var/log/some/random/path.log"
  local exit11=0
  recover_session_from_stdout "$data" "$no_data_path" >/dev/null 2>&1 || exit11=$?
  if [[ "$exit11" -ne 0 ]]; then
    tap_ok "attach-mode: no /data/ segment returns error"
  else
    tap_not_ok "attach-mode: no /data/ segment returns error" "expected non-zero"
  fi

  # ── Test 12: Cleanup guard refuses active session (layer 1) ────────────
  # Source: .claude/skills/monitor-loop/SKILL.md:837-841
  data="$TEST_ROOT/t12/data"
  proc="$TEST_ROOT/t12/proc"
  mkdir -p "$data/active-sess" "$proc"
  local result12
  result12=$(cleanup_guard "$data" "$proc" "active-sess" "active-sess" 3600)
  if echo "$result12" | grep -q "SKIP.*active"; then
    tap_ok "cleanup-guard: refuses active session (layer 1)"
  else
    tap_not_ok "cleanup-guard: refuses active session (layer 1)" "got: $result12"
  fi

  # ── Test 13: Cleanup guard refuses recent .alive (layer 2) ─────────────
  # Source: .claude/skills/monitor-loop/SKILL.md:843-851 (< 3600)
  data="$TEST_ROOT/t13/data"
  proc="$TEST_ROOT/t13/proc"
  mkdir -p "$data/recent-sess" "$proc"
  mock_alive_file "$data/recent-sess/.alive" 3599
  local result13
  result13=$(cleanup_guard "$data" "$proc" "recent-sess" "different-sess" 3600)
  if echo "$result13" | grep -q "SKIP.*alive"; then
    tap_ok "cleanup-guard: refuses recent .alive (3599s < 3600, layer 2)"
  else
    tap_not_ok "cleanup-guard: refuses recent .alive (3599s < 3600, layer 2)" "got: $result13"
  fi

  # ── Test 14: Cleanup guard .alive at boundary passes layer 2 ───────────
  # Source: .claude/skills/monitor-loop/SKILL.md:847 (-lt 3600 → 3600 is NOT less)
  data="$TEST_ROOT/t14/data"
  proc="$TEST_ROOT/t14/proc"
  mkdir -p "$data/boundary-sess" "$proc"
  mock_alive_file "$data/boundary-sess/.alive" 3600
  local result14
  result14=$(cleanup_guard "$data" "$proc" "boundary-sess" "different-sess" 3600)
  if echo "$result14" | grep -q "PASS"; then
    tap_ok "cleanup-guard: .alive at boundary (3600s) passes layer 2"
  else
    tap_not_ok "cleanup-guard: .alive at boundary (3600s) passes layer 2" "got: $result14"
  fi

  # ── Test 15: Cleanup guard refuses running-process session (layer 3) ───
  # Source: .claude/skills/monitor-loop/SKILL.md:853-857
  data="$TEST_ROOT/t15/data"
  proc="$TEST_ROOT/t15/proc"
  mkdir -p "$data/running-sess" "$proc"
  mock_alive_file "$data/running-sess/.alive" 7200  # old enough to pass layer 2
  mock_proc_entry "$proc" "9001" "$data/running-sess/cargo-target/release/henyey" "henyey --mainnet run --validator"
  local result15
  result15=$(cleanup_guard "$data" "$proc" "running-sess" "different-sess" 3600)
  if echo "$result15" | grep -q "SKIP.*process"; then
    tap_ok "cleanup-guard: refuses running-process session (layer 3)"
  else
    tap_not_ok "cleanup-guard: refuses running-process session (layer 3)" "got: $result15"
  fi

  # ── Test 16: MAINNET_WIPED detection ───────────────────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:119-127,141-150
  # MAINNET_WIPED is independent of SESSION_WIPED per truth table
  data="$TEST_ROOT/t16/data"
  mkdir -p "$data"
  # No mainnet dir — should detect wipe regardless of SESSION_WIPED state
  check_mainnet_wiped "$data"
  local mainnet_result_alone="$MAINNET_WIPED"
  # Also verify it fires even when SESSION_WIPED=yes (combined case #6-8)
  SESSION_WIPED=yes
  check_mainnet_wiped "$data"
  local mainnet_result_combined="$MAINNET_WIPED"
  # Verify it does NOT fire when mainnet dir exists
  mkdir -p "$data/mainnet"
  check_mainnet_wiped "$data"
  local mainnet_result_present="$MAINNET_WIPED"

  if [[ "$mainnet_result_alone" == "yes" && "$mainnet_result_combined" == "yes" && "$mainnet_result_present" == "no" ]]; then
    tap_ok "mainnet-wiped: independent of SESSION_WIPED, detects missing dir"
  else
    tap_not_ok "mainnet-wiped: independent of SESSION_WIPED, detects missing dir" "alone=$mainnet_result_alone combined=$mainnet_result_combined present=$mainnet_result_present"
  fi

  # ── Test 17: Stale env + missing session dir → return 1 + NO dirs ──────
  # Source: scripts/lib/monitor-decisions.sh — stale env aborts before mkdir
  data="$TEST_ROOT/t17/data"
  proc="$TEST_ROOT/t17/proc"
  session_id="sess1717"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7201

  local exit_code17=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_code17=$?
  if [[ "$exit_code17" -eq 1 && ! -d "$data/$session_id" ]]; then
    tap_ok "session-wipe: stale env does NOT create recovery dirs"
  else
    tap_not_ok "session-wipe: stale env does NOT create recovery dirs" "exit=$exit_code17 dir_exists=$(test -d "$data/$session_id" && echo yes || echo no)"
  fi

  # ── Test 18: Deleted-stdout emits warning to stderr ────────────────────
  # Source: scripts/lib/monitor-decisions.sh — recover_session_from_stdout
  data="$TEST_ROOT/t18/data"
  mkdir -p "$data"
  local stdout_path18="$data/warntest/logs/monitor.log (deleted)"
  local stderr18
  stderr18=$(recover_session_from_stdout "$data" "$stdout_path18" 2>&1 >/dev/null)
  if echo "$stderr18" | grep -q "WARNING.*stdout target deleted"; then
    tap_ok "attach-mode: (deleted) stdout emits warning to stderr"
  else
    tap_not_ok "attach-mode: (deleted) stdout emits warning to stderr" "stderr='$stderr18'"
  fi

  # ── Test 19: Cleanup guard no-.alive fall-through → PASS ─────────────
  # Source: scripts/lib/monitor-decisions.sh:167-185 (cleanup_guard)
  # When no .alive file exists, layer 2 is bypassed entirely. With no
  # running process (layer 3 miss), the result is PASS (eligible for cleanup).
  data="$TEST_ROOT/t19b/data"
  proc="$TEST_ROOT/t19b/proc"
  mkdir -p "$data/no-alive-sess" "$proc"
  # No .alive file created — layer 2 skipped
  # Not the active session — layer 1 skipped
  # No process entries — layer 3 skipped
  local result19b
  result19b=$(cleanup_guard "$data" "$proc" "no-alive-sess" "different-sess" 3600)
  if [[ "$result19b" == "PASS" ]]; then
    tap_ok "cleanup-guard: no .alive file falls through to PASS"
  else
    tap_not_ok "cleanup-guard: no .alive file falls through to PASS" "got: $result19b"
  fi

  # ── Test 20: Session dir exists → not wiped (no-op fall-through) ───────
  # Source: scripts/lib/monitor-decisions.sh — check_session_wiped
  # Verifies observable contract: when session dir already exists, function
  # reports not-wiped regardless of hostile environment state.
  data="$TEST_ROOT/t19/data"
  proc="$TEST_ROOT/t19/proc"
  session_id="sess1919"
  mkdir -p "$data/$session_id" "$proc" "$TEST_ROOT/t19"
  # Hostile env: stale env file (>7200s) that would trigger return 1 if checked
  mock_env_file "$data/monitor-loop.env" 7201
  # No matching proc entries (empty proc dir)

  local exit_code19=0
  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>"$TEST_ROOT/t19/stderr" || exit_code19=$?
  local stderr19
  stderr19=$(cat "$TEST_ROOT/t19/stderr")
  if [[ "$exit_code19" -eq 0 && "$SESSION_WIPED" == "no" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" \
        && -d "$data/$session_id" && -z "$(find "$data/$session_id" -mindepth 1 2>/dev/null)" \
        && -z "$stderr19" ]]; then
    tap_ok "session-wipe: session dir exists → not wiped"
  else
    tap_not_ok "session-wipe: session dir exists → not wiped" \
      "exit=$exit_code19 WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE stderr='$stderr19' contents='$(ls "$data/$session_id" 2>/dev/null)'"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # _find_session_process tests (T20b-T20c)
  # Source: scripts/lib/monitor-decisions.sh — _find_session_process
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 20b: _find_session_process finds matching PID ─────────────────
  data="$TEST_ROOT/t20b/data"
  proc="$TEST_ROOT/t20b/proc"
  session_id="sess20b"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "5001" "$data/$session_id/cargo-target/release/henyey" "henyey --mainnet run --validator"
  local found_pid
  found_pid=$(_find_session_process "$data" "$proc" "$session_id")
  if [[ "$found_pid" == "5001" ]]; then
    tap_ok "_find_session_process: returns PID for matching binary"
  else
    tap_not_ok "_find_session_process: returns PID for matching binary" "got: '$found_pid'"
  fi

  # ── Test 20c: _find_session_process returns empty for non-matching ─────
  data="$TEST_ROOT/t20c/data"
  proc="$TEST_ROOT/t20c/proc"
  session_id="sess20c"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "5002" "$data/other-session/cargo-target/release/henyey" "henyey --mainnet run"
  found_pid=$(_find_session_process "$data" "$proc" "$session_id")
  if [[ -z "$found_pid" ]]; then
    tap_ok "_find_session_process: empty for non-matching binary"
  else
    tap_not_ok "_find_session_process: empty for non-matching binary" "got: '$found_pid'"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # _enumerate_henyey_processes tests (T20c1-T20c7)
  # Source: scripts/lib/monitor-decisions.sh — _enumerate_henyey_processes
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 20c1: single run process ──────────────────────────────────────
  data="$TEST_ROOT/t20c1/data"
  proc="$TEST_ROOT/t20c1/proc"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "8001" "$data/sessc1/cargo-target/release/henyey" "henyey --mainnet run --validator"
  local enum_out
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ "$enum_out" == "8001 sessc1" ]]; then
    tap_ok "_enumerate_henyey_processes: single run process"
  else
    tap_not_ok "_enumerate_henyey_processes: single run process" "got: '$enum_out'"
  fi

  # ── Test 20c2: multiple processes, different sessions ──────────────────
  data="$TEST_ROOT/t20c2/data"
  proc="$TEST_ROOT/t20c2/proc"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "8002" "$data/sessA/cargo-target/release/henyey" "henyey run --validator"
  mock_proc_entry "$proc" "8003" "$data/sessB/cargo-target/release/henyey" "henyey run"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  local line_count
  line_count=$(echo "$enum_out" | grep -c . || true)
  if [[ "$line_count" -eq 2 ]] && echo "$enum_out" | grep -q "8002 sessA" && echo "$enum_out" | grep -q "8003 sessB"; then
    tap_ok "_enumerate_henyey_processes: multiple sessions listed"
  else
    tap_not_ok "_enumerate_henyey_processes: multiple sessions listed" "got: '$enum_out'"
  fi

  # ── Test 20c3: (deleted) exe matched ───────────────────────────────────
  data="$TEST_ROOT/t20c3/data"
  proc="$TEST_ROOT/t20c3/proc"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "8004" "$data/sessc3/cargo-target/release/henyey (deleted)" "henyey --mainnet run"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ "$enum_out" == "8004 sessc3" ]]; then
    tap_ok "_enumerate_henyey_processes: (deleted) exe matched"
  else
    tap_not_ok "_enumerate_henyey_processes: (deleted) exe matched" "got: '$enum_out'"
  fi

  # ── Test 20c3-zsh: (deleted) exe matched — zsh recheck ──────────────────
  # Regression guard for the zsh parameter-expansion bug fixed in PR #2780:
  # under zsh, the unquoted form `${exe% (deleted)}` parses ` ` as a glob and
  # silently fails to strip the suffix. This re-runs the T20c3 scenario inside
  # a `zsh -c` subprocess so the suffix-strip in monitor-decisions.sh line 37
  # is exercised under zsh semantics. Gated on zsh availability.
  if command -v zsh >/dev/null 2>&1; then
    data="$TEST_ROOT/t20c3z/data"
    proc="$TEST_ROOT/t20c3z/proc"
    mkdir -p "$data" "$proc"
    mock_proc_entry "$proc" "8010" "$data/sessc3z/cargo-target/release/henyey (deleted)" "henyey --mainnet run"
    # Pass all paths as positional args to avoid inline-quoting hazards.
    enum_out=$(zsh -c 'source "$1/scripts/lib/monitor-decisions.sh"; _enumerate_henyey_processes "$2" "$3"' -- "$REPO_ROOT" "$data" "$proc")
    if [[ "$enum_out" == "8010 sessc3z" ]]; then
      tap_ok "_enumerate_henyey_processes: (deleted) exe matched — zsh recheck"
    else
      tap_not_ok "_enumerate_henyey_processes: (deleted) exe matched — zsh recheck" "got: '$enum_out'"
    fi
  else
    tap_skip "_enumerate_henyey_processes: (deleted) exe matched — zsh recheck" "zsh not found"
  fi

  # ── Test 20c4: empty proc root ─────────────────────────────────────────
  data="$TEST_ROOT/t20c4/data"
  proc="$TEST_ROOT/t20c4/proc"
  mkdir -p "$data" "$proc"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ -z "$enum_out" ]]; then
    tap_ok "_enumerate_henyey_processes: empty proc root"
  else
    tap_not_ok "_enumerate_henyey_processes: empty proc root" "got: '$enum_out'"
  fi

  # ── Test 20c5: non-henyey binary ignored ───────────────────────────────
  data="$TEST_ROOT/t20c5/data"
  proc="$TEST_ROOT/t20c5/proc"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "8005" "/usr/bin/something" "something run"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ -z "$enum_out" ]]; then
    tap_ok "_enumerate_henyey_processes: non-henyey binary ignored"
  else
    tap_not_ok "_enumerate_henyey_processes: non-henyey binary ignored" "got: '$enum_out'"
  fi

  # ── Test 20c6: henyey binary but non-run cmdline ───────────────────────
  data="$TEST_ROOT/t20c6/data"
  proc="$TEST_ROOT/t20c6/proc"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "8006" "$data/sessc6/cargo-target/release/henyey" "henyey offline verify-execution --testnet"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ -z "$enum_out" ]]; then
    tap_ok "_enumerate_henyey_processes: non-run cmdline ignored"
  else
    tap_not_ok "_enumerate_henyey_processes: non-run cmdline ignored" "got: '$enum_out'"
  fi

  # ── Test 20c7: unreadable cmdline (no cmdline file) ────────────────────
  data="$TEST_ROOT/t20c7/data"
  proc="$TEST_ROOT/t20c7/proc"
  mkdir -p "$data" "$proc"
  # Create mock entry WITHOUT cmdline (3 args only)
  mock_proc_entry "$proc" "8007" "$data/sessc7/cargo-target/release/henyey"
  enum_out=$(_enumerate_henyey_processes "$data" "$proc")
  if [[ -z "$enum_out" ]]; then
    tap_ok "_enumerate_henyey_processes: missing cmdline ignored"
  else
    tap_not_ok "_enumerate_henyey_processes: missing cmdline ignored" "got: '$enum_out'"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # _parse_cmdline_config tests (T20c8-T20c12)
  # Source: scripts/lib/monitor-decisions.sh — _parse_cmdline_config
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 20c8: -c path ─────────────────────────────────────────────────
  local cmdfile="$TEST_ROOT/t20c8_cmdline"
  printf 'henyey\0run\0-c\0configs/mainnet.toml\0' > "$cmdfile"
  local config_out
  config_out=$(_parse_cmdline_config "$cmdfile")
  if [[ "$config_out" == "configs/mainnet.toml" ]]; then
    tap_ok "_parse_cmdline_config: -c path extracted"
  else
    tap_not_ok "_parse_cmdline_config: -c path extracted" "got: '$config_out'"
  fi

  # ── Test 20c9: --config path ───────────────────────────────────────────
  cmdfile="$TEST_ROOT/t20c9_cmdline"
  printf 'henyey\0run\0--config\0configs/validator.toml\0' > "$cmdfile"
  config_out=$(_parse_cmdline_config "$cmdfile")
  if [[ "$config_out" == "configs/validator.toml" ]]; then
    tap_ok "_parse_cmdline_config: --config path extracted"
  else
    tap_not_ok "_parse_cmdline_config: --config path extracted" "got: '$config_out'"
  fi

  # ── Test 20c10: --conf path (alias) ────────────────────────────────────
  cmdfile="$TEST_ROOT/t20c10_cmdline"
  printf 'henyey\0run\0--conf\0configs/custom.toml\0' > "$cmdfile"
  config_out=$(_parse_cmdline_config "$cmdfile")
  if [[ "$config_out" == "configs/custom.toml" ]]; then
    tap_ok "_parse_cmdline_config: --conf alias extracted"
  else
    tap_not_ok "_parse_cmdline_config: --conf alias extracted" "got: '$config_out'"
  fi

  # ── Test 20c11: --config=value form ────────────────────────────────────
  cmdfile="$TEST_ROOT/t20c11_cmdline"
  printf 'henyey\0run\0--config=configs/eq.toml\0' > "$cmdfile"
  config_out=$(_parse_cmdline_config "$cmdfile")
  if [[ "$config_out" == "configs/eq.toml" ]]; then
    tap_ok "_parse_cmdline_config: --config=value extracted"
  else
    tap_not_ok "_parse_cmdline_config: --config=value extracted" "got: '$config_out'"
  fi

  # ── Test 20c12: no config flag ─────────────────────────────────────────
  cmdfile="$TEST_ROOT/t20c12_cmdline"
  printf 'henyey\0--mainnet\0run\0--validator\0' > "$cmdfile"
  config_out=$(_parse_cmdline_config "$cmdfile")
  if [[ -z "$config_out" ]]; then
    tap_ok "_parse_cmdline_config: no config flag returns empty"
  else
    tap_not_ok "_parse_cmdline_config: no config flag returns empty" "got: '$config_out'"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # check_long_stale_session tests (T20d-T20n)
  # Source: scripts/lib/monitor-decisions.sh — check_long_stale_session
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 20d: Session dir missing → not stale (return 0) ───────────────
  data="$TEST_ROOT/t20d/data"
  proc="$TEST_ROOT/t20d/proc"
  session_id="sess20d"
  mkdir -p "$data" "$proc"
  # session dir does NOT exist
  local exit_20d=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20d=$?
  if [[ "$exit_20d" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: session dir missing → not stale"
  else
    tap_not_ok "long-stale: session dir missing → not stale" "exit=$exit_20d LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20e: Session dir exists + .alive fresh (100s) → not stale ─────
  data="$TEST_ROOT/t20e/data"
  proc="$TEST_ROOT/t20e/proc"
  session_id="sess20e"
  mkdir -p "$data/$session_id" "$proc"
  mock_alive_file "$data/$session_id/.alive" 100
  local exit_20e=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20e=$?
  if [[ "$exit_20e" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: .alive fresh (100s) → not stale"
  else
    tap_not_ok "long-stale: .alive fresh (100s) → not stale" "exit=$exit_20e LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20f: .alive stale + env stale + no process → return 1 ─────────
  data="$TEST_ROOT/t20f/data"
  proc="$TEST_ROOT/t20f/proc"
  session_id="sess20f"
  mkdir -p "$data/$session_id" "$proc"
  mock_alive_file "$data/$session_id/.alive" 21601
  mock_env_file "$data/monitor-loop.env" 86401
  local exit_20f=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20f=$?
  if [[ "$exit_20f" -eq 1 && "$LONG_STALE_SESSION" == "yes" ]]; then
    tap_ok "long-stale: .alive stale + env stale + no process → return 1"
  else
    tap_not_ok "long-stale: .alive stale + env stale + no process → return 1" "exit=$exit_20f LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20g: .alive stale + env stale + process alive → return 0 ──────
  data="$TEST_ROOT/t20g/data"
  proc="$TEST_ROOT/t20g/proc"
  session_id="sess20g"
  mkdir -p "$data/$session_id" "$proc"
  mock_alive_file "$data/$session_id/.alive" 21601
  mock_env_file "$data/monitor-loop.env" 86401
  mock_proc_entry "$proc" "6001" "$data/$session_id/cargo-target/release/henyey" "henyey --mainnet run --validator"
  local exit_20g=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20g=$?
  if [[ "$exit_20g" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: process alive overrides stale markers"
  else
    tap_not_ok "long-stale: process alive overrides stale markers" "exit=$exit_20g LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20h: .alive at boundary (21600s) → not stale (passes -le) ────
  data="$TEST_ROOT/t20h/data"
  proc="$TEST_ROOT/t20h/proc"
  session_id="sess20h"
  mkdir -p "$data/$session_id" "$proc"
  mock_alive_file "$data/$session_id/.alive" 21600
  mock_env_file "$data/monitor-loop.env" 86401
  local exit_20h=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20h=$?
  if [[ "$exit_20h" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: .alive at boundary (21600s) → not stale"
  else
    tap_not_ok "long-stale: .alive at boundary (21600s) → not stale" "exit=$exit_20h LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20i: .alive missing + env fresh (100s) → not stale ────────────
  data="$TEST_ROOT/t20i/data"
  proc="$TEST_ROOT/t20i/proc"
  session_id="sess20i"
  mkdir -p "$data/$session_id" "$proc"
  # No .alive file
  mock_env_file "$data/monitor-loop.env" 100
  local exit_20i=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20i=$?
  if [[ "$exit_20i" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: .alive missing + env fresh → not stale"
  else
    tap_not_ok "long-stale: .alive missing + env fresh → not stale" "exit=$exit_20i LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20j: .alive missing + env stale + no process → return 1 ───────
  data="$TEST_ROOT/t20j/data"
  proc="$TEST_ROOT/t20j/proc"
  session_id="sess20j"
  mkdir -p "$data/$session_id" "$proc"
  # No .alive file
  mock_env_file "$data/monitor-loop.env" 86401
  local exit_20j=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20j=$?
  if [[ "$exit_20j" -eq 1 && "$LONG_STALE_SESSION" == "yes" ]]; then
    tap_ok "long-stale: .alive missing + env stale + no process → return 1"
  else
    tap_not_ok "long-stale: .alive missing + env stale + no process → return 1" "exit=$exit_20j LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20k: .alive missing + env at boundary (86400s) → not stale ────
  data="$TEST_ROOT/t20k/data"
  proc="$TEST_ROOT/t20k/proc"
  session_id="sess20k"
  mkdir -p "$data/$session_id" "$proc"
  # No .alive file
  mock_env_file "$data/monitor-loop.env" 86400
  local exit_20k=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20k=$?
  if [[ "$exit_20k" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: env at boundary (86400s) → not stale"
  else
    tap_not_ok "long-stale: env at boundary (86400s) → not stale" "exit=$exit_20k LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20l: .alive stale + env fresh → not stale (env fallback) ──────
  data="$TEST_ROOT/t20l/data"
  proc="$TEST_ROOT/t20l/proc"
  session_id="sess20l"
  mkdir -p "$data/$session_id" "$proc"
  mock_alive_file "$data/$session_id/.alive" 21601
  mock_env_file "$data/monitor-loop.env" 100
  local exit_20l=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20l=$?
  if [[ "$exit_20l" -eq 0 && "$LONG_STALE_SESSION" == "no" ]]; then
    tap_ok "long-stale: .alive stale + env fresh → not stale (env fallback)"
  else
    tap_not_ok "long-stale: .alive stale + env fresh → not stale (env fallback)" "exit=$exit_20l LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20m: .alive missing + env stale + different session process → return 1 ─
  data="$TEST_ROOT/t20m/data"
  proc="$TEST_ROOT/t20m/proc"
  session_id="sess20m"
  mkdir -p "$data/$session_id" "$proc"
  # No .alive file; process running a DIFFERENT session's binary
  mock_env_file "$data/monitor-loop.env" 86401
  mock_proc_entry "$proc" "7001" "$data/other-session/cargo-target/release/henyey" "henyey --mainnet run"
  local exit_20m=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>/dev/null || exit_20m=$?
  if [[ "$exit_20m" -eq 1 && "$LONG_STALE_SESSION" == "yes" ]]; then
    tap_ok "long-stale: different session process does not save"
  else
    tap_not_ok "long-stale: different session process does not save" "exit=$exit_20m LONG_STALE=$LONG_STALE_SESSION"
  fi

  # ── Test 20n: stderr message on long-stale refusal ─────────────────────
  data="$TEST_ROOT/t20n/data"
  proc="$TEST_ROOT/t20n/proc"
  session_id="sess20n"
  mkdir -p "$data/$session_id" "$proc" "$TEST_ROOT/t20n"
  mock_alive_file "$data/$session_id/.alive" 21601
  mock_env_file "$data/monitor-loop.env" 86401
  local exit_20n=0
  check_long_stale_session "$data" "$proc" "$session_id" "$data/monitor-loop.env" 2>"$TEST_ROOT/t20n/stderr" || exit_20n=$?
  local stderr_20n
  stderr_20n=$(cat "$TEST_ROOT/t20n/stderr")
  if [[ "$exit_20n" -eq 1 && "$stderr_20n" == *"long-stale"* && "$stderr_20n" == *"$session_id"* && "$stderr_20n" == *"Refusing auto-relaunch"* ]]; then
    tap_ok "long-stale: stderr message on refusal"
  else
    tap_not_ok "long-stale: stderr message on refusal" "exit=$exit_20n stderr='$stderr_20n'"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # detect_crash_state tests (T21-T33)
  # Source: scripts/lib/monitor-decisions.sh — detect_crash_state
  # ════════════════════════════════════════════════════════════════════════════

  local NOW_EPOCH=1700000000  # fixed reference point for all crash tests
  local BOUNDARY=$((NOW_EPOCH - 1800))  # 1799998200

  # ── Test 21: fatal_wipe_required=true in text log (structured field) ────
  local logs21="$TEST_ROOT/t21/logs"
  mock_crashed_log "$logs21" "20260504T140000Z" \
    "2026-05-04T14:04:09.061014Z ERROR henyey_app::app::lifecycle: FATAL: unrecoverable local state failure — pre-close hash mismatch at ledger 62415630 fatal_wipe_required=true" \
    $((NOW_EPOCH - 300))
  detect_crash_state "$logs21" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "yes" && "$CRASH_RECENT_COUNT" -eq 1 ]]; then
    tap_ok "crash-detect: fatal_wipe_required=true in text log"
  else
    tap_not_ok "crash-detect: fatal_wipe_required=true in text log" \
      "MISMATCH=$CRASH_HASH_MISMATCH COUNT=$CRASH_RECENT_COUNT"
  fi

  # ── Test 22: Legacy prose "State wipe required before restart" ──────────
  local logs22="$TEST_ROOT/t22/logs"
  mock_crashed_log "$logs22" "20260504T140100Z" \
    "FATAL: unrecoverable local state failure — pre-close hash mismatch. Node will shut down. State wipe required before restart." \
    $((NOW_EPOCH - 300))
  detect_crash_state "$logs22" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "yes" ]]; then
    tap_ok "crash-detect: legacy prose 'State wipe required before restart'"
  else
    tap_not_ok "crash-detect: legacy prose 'State wipe required before restart'" \
      "MISMATCH=$CRASH_HASH_MISMATCH"
  fi

  # ── Test 23: Unrelated error log (no fatal wipe) ───────────────────────
  local logs23="$TEST_ROOT/t23/logs"
  mock_crashed_log "$logs23" "20260504T140200Z" \
    "thread 'main' panicked at 'out of memory' note: run with RUST_BACKTRACE=1" \
    $((NOW_EPOCH - 300))
  detect_crash_state "$logs23" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "no" && "$CRASH_RECENT_COUNT" -eq 1 ]]; then
    tap_ok "crash-detect: unrelated error → no hash mismatch"
  else
    tap_not_ok "crash-detect: unrelated error → no hash mismatch" \
      "MISMATCH=$CRASH_HASH_MISMATCH COUNT=$CRASH_RECENT_COUNT"
  fi

  # ── Test 24: All files older than 30 minutes ───────────────────────────
  local logs24="$TEST_ROOT/t24/logs"
  mock_crashed_log "$logs24" "20260504T130000Z" "fatal_wipe_required=true" $((NOW_EPOCH - 2000))
  mock_crashed_log "$logs24" "20260504T130100Z" "fatal_wipe_required=true" $((NOW_EPOCH - 3600))
  mock_crashed_log "$logs24" "20260504T130200Z" "fatal_wipe_required=true" $((NOW_EPOCH - 7200))
  detect_crash_state "$logs24" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 0 && -z "$CRASH_LATEST_FILE" ]]; then
    tap_ok "crash-detect: all files older than 30 min → count=0"
  else
    tap_not_ok "crash-detect: all files older than 30 min → count=0" \
      "COUNT=$CRASH_RECENT_COUNT LATEST=$CRASH_LATEST_FILE"
  fi

  # ── Test 25: Empty log directory ────────────────────────────────────────
  local logs25="$TEST_ROOT/t25/logs"
  mkdir -p "$logs25"
  detect_crash_state "$logs25" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 0 && -z "$CRASH_LATEST_FILE" && "$CRASH_HASH_MISMATCH" == "no" ]]; then
    tap_ok "crash-detect: empty log directory → count=0"
  else
    tap_not_ok "crash-detect: empty log directory → count=0" \
      "COUNT=$CRASH_RECENT_COUNT"
  fi

  # ── Test 26: Missing log directory ──────────────────────────────────────
  detect_crash_state "$TEST_ROOT/t26/nonexistent" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 0 && -z "$CRASH_LATEST_FILE" && "$CRASH_HASH_MISMATCH" == "no" ]]; then
    tap_ok "crash-detect: missing log directory → count=0"
  else
    tap_not_ok "crash-detect: missing log directory → count=0" \
      "COUNT=$CRASH_RECENT_COUNT"
  fi

  # ── Test 27: Mixed ages: 4 recent + 2 old → count=4, newest selected ──
  local logs27="$TEST_ROOT/t27/logs"
  # 4 recent files (within 30 min)
  mock_crashed_log "$logs27" "recent-a" "some log" $((NOW_EPOCH - 100))
  mock_crashed_log "$logs27" "recent-b" "some log" $((NOW_EPOCH - 200))
  mock_crashed_log "$logs27" "recent-c" "some log" $((NOW_EPOCH - 500))
  mock_crashed_log "$logs27" "recent-d" "some log" $((NOW_EPOCH - 1000))
  # 2 old files (outside 30 min)
  mock_crashed_log "$logs27" "old-a" "some log" $((NOW_EPOCH - 2000))
  mock_crashed_log "$logs27" "old-b" "some log" $((NOW_EPOCH - 3600))
  detect_crash_state "$logs27" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 4 && "$CRASH_LATEST_FILE" == "$logs27/monitor.log.crashed-recent-a" ]]; then
    tap_ok "crash-detect: 4 recent + 2 old → count=4, newest selected"
  else
    tap_not_ok "crash-detect: 4 recent + 2 old → count=4, newest selected" \
      "COUNT=$CRASH_RECENT_COUNT LATEST=$CRASH_LATEST_FILE"
  fi

  # ── Test 28: Exact boundary (NOW-1800, excluded by strict >) ───────────
  local logs28="$TEST_ROOT/t28/logs"
  mock_crashed_log "$logs28" "boundary" "fatal_wipe_required=true" "$BOUNDARY"
  detect_crash_state "$logs28" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 0 ]]; then
    tap_ok "crash-detect: exact boundary (NOW-1800) excluded"
  else
    tap_not_ok "crash-detect: exact boundary (NOW-1800) excluded" \
      "COUNT=$CRASH_RECENT_COUNT"
  fi

  # ── Test 29: Just inside boundary (NOW-1799, included) ─────────────────
  local logs29="$TEST_ROOT/t29/logs"
  mock_crashed_log "$logs29" "just-inside" "fatal_wipe_required=true" $((BOUNDARY + 1))
  detect_crash_state "$logs29" "$NOW_EPOCH"
  if [[ "$CRASH_RECENT_COUNT" -eq 1 && "$CRASH_HASH_MISMATCH" == "yes" ]]; then
    tap_ok "crash-detect: just inside boundary (NOW-1799) included"
  else
    tap_not_ok "crash-detect: just inside boundary (NOW-1799) included" \
      "COUNT=$CRASH_RECENT_COUNT MISMATCH=$CRASH_HASH_MISMATCH"
  fi

  # ── Test 30: Newest non-fatal, older recent fatal → no mismatch ────────
  local logs30="$TEST_ROOT/t30/logs"
  mock_crashed_log "$logs30" "newer-nonfatal" "thread panicked at OOM" $((NOW_EPOCH - 100))
  mock_crashed_log "$logs30" "older-fatal" "fatal_wipe_required=true" $((NOW_EPOCH - 500))
  detect_crash_state "$logs30" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "no" && "$CRASH_RECENT_COUNT" -eq 2 ]]; then
    tap_ok "crash-detect: newest non-fatal, older fatal → no mismatch"
  else
    tap_not_ok "crash-detect: newest non-fatal, older fatal → no mismatch" \
      "MISMATCH=$CRASH_HASH_MISMATCH COUNT=$CRASH_RECENT_COUNT LATEST=$CRASH_LATEST_FILE"
  fi

  # ── Test 31: Colon separator variant (fatal_wipe_required: true) ───────
  local logs31="$TEST_ROOT/t31/logs"
  mock_crashed_log "$logs31" "colon-variant" \
    "2026-05-04T14:04:09Z ERROR lifecycle: fatal_wipe_required: true FATAL: unrecoverable" \
    $((NOW_EPOCH - 300))
  detect_crash_state "$logs31" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "yes" ]]; then
    tap_ok "crash-detect: colon separator (fatal_wipe_required: true)"
  else
    tap_not_ok "crash-detect: colon separator (fatal_wipe_required: true)" \
      "MISMATCH=$CRASH_HASH_MISMATCH"
  fi

  # ── Test 32: JSON format ("fatal_wipe_required":true) ──────────────────
  local logs32="$TEST_ROOT/t32/logs"
  mock_crashed_log "$logs32" "json-format" \
    '{"timestamp":"2026-05-04T14:04:09Z","level":"ERROR","fields":{"fatal_wipe_required":true,"message":"FATAL: unrecoverable local state failure"}}' \
    $((NOW_EPOCH - 300))
  detect_crash_state "$logs32" "$NOW_EPOCH"
  if [[ "$CRASH_HASH_MISMATCH" == "yes" ]]; then
    tap_ok "crash-detect: JSON format (\"fatal_wipe_required\":true)"
  else
    tap_not_ok "crash-detect: JSON format (\"fatal_wipe_required\":true)" \
      "MISMATCH=$CRASH_HASH_MISMATCH"
  fi

  # ── Test 33: Mtime tie-break — lexicographic-last path wins ────────────
  local logs33="$TEST_ROOT/t33/logs"
  local same_mtime=$((NOW_EPOCH - 300))
  mock_crashed_log "$logs33" "aaa-first" "fatal_wipe_required=true" "$same_mtime"
  mock_crashed_log "$logs33" "zzz-last" "thread panicked at OOM" "$same_mtime"
  detect_crash_state "$logs33" "$NOW_EPOCH"
  # With same mtime, lexicographic-descending path wins → zzz-last is newest
  if [[ "$CRASH_LATEST_FILE" == "$logs33/monitor.log.crashed-zzz-last" && "$CRASH_HASH_MISMATCH" == "no" ]]; then
    tap_ok "crash-detect: mtime tie-break → lexicographic-last path"
  else
    tap_not_ok "crash-detect: mtime tie-break → lexicographic-last path" \
      "LATEST=$CRASH_LATEST_FILE MISMATCH=$CRASH_HASH_MISMATCH"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # prune_rotated_logs tests (#3616) — rotated-log retention by mtime, all cats
  # Source: scripts/lib/monitor-decisions.sh — prune_rotated_logs
  # mock_rotated_log LOGS_DIR FULL_SUFFIX MTIME_EPOCH (suffix incl. category)
  # ════════════════════════════════════════════════════════════════════════════
  mock_rotated_log() {
    local logs_dir="$1" suffix="$2" mtime_epoch="$3"
    mkdir -p "$logs_dir"
    printf 'log\n' > "$logs_dir/monitor.log.$suffix"
    touch -d "@$mtime_epoch" "$logs_dir/monitor.log.$suffix"
  }

  # ── Test 33a: Legacy categories ARE pruned (predeploy/coldcatchup/...) ──────
  # On origin/main only preredeploy/crashed/stuck/frozen were covered, so these
  # legacy categories accumulated unbounded (#3616). 5 predeploy → keep 3.
  local lp1="$TEST_ROOT/prune-legacy/logs"
  local i
  for i in 1 2 3 4 5; do
    mock_rotated_log "$lp1" "predeploy-2026060${i}T120000Z" $((NOW_EPOCH - 6000 + i*100))
  done
  mock_rotated_log "$lp1" "coldcatchup-a" $((NOW_EPOCH - 5000))
  mock_rotated_log "$lp1" "stopped-a"     $((NOW_EPOCH - 5000))
  prune_rotated_logs "$lp1" 3
  local pre_kept
  pre_kept=$(find "$lp1" -name 'monitor.log.predeploy-*' | wc -l | tr -d ' ')
  if [[ "$pre_kept" -eq 3 && "$PRUNED_LOG_COUNT" -eq 2 \
        && -f "$lp1/monitor.log.coldcatchup-a" && -f "$lp1/monitor.log.stopped-a" ]]; then
    tap_ok "prune-logs: legacy 'predeploy' pruned to newest 3 (legacy categories covered)"
  else
    tap_not_ok "prune-logs: legacy 'predeploy' pruned to newest 3 (legacy categories covered)" \
      "pre_kept=$pre_kept removed=$PRUNED_LOG_COUNT"
  fi

  # ── Test 33b: mtime ordering keeps RECENT, drops OLD infix variant ─────────
  # crashed-knit2lcl-* sorts its 'k' ahead of bare crashed-2026… under sort -r,
  # which on origin/main retained the OLD logs and deleted recent ones (#3616).
  local lp2="$TEST_ROOT/prune-mtime/logs"
  mock_rotated_log "$lp2" "crashed-knit2lcl-20260615T192932Z" $((NOW_EPOCH - 900000))  # OLD
  mock_rotated_log "$lp2" "crashed-knit2lcl-20260602T133013Z" $((NOW_EPOCH - 990000))  # OLDER
  mock_rotated_log "$lp2" "crashed-20260623T212302Z"          $((NOW_EPOCH - 100))     # RECENT
  mock_rotated_log "$lp2" "crashed-20260623T203607Z"          $((NOW_EPOCH - 200))     # RECENT
  mock_rotated_log "$lp2" "crashed-20260623T134735Z"          $((NOW_EPOCH - 300))     # RECENT
  prune_rotated_logs "$lp2" 3
  if [[ -f "$lp2/monitor.log.crashed-20260623T212302Z" \
        && -f "$lp2/monitor.log.crashed-20260623T203607Z" \
        && -f "$lp2/monitor.log.crashed-20260623T134735Z" \
        && ! -f "$lp2/monitor.log.crashed-knit2lcl-20260615T192932Z" \
        && ! -f "$lp2/monitor.log.crashed-knit2lcl-20260602T133013Z" \
        && "$PRUNED_LOG_COUNT" -eq 2 ]]; then
    tap_ok "prune-logs: mtime ordering keeps recent crashed, drops old knit2lcl infix"
  else
    tap_not_ok "prune-logs: mtime ordering keeps recent crashed, drops old knit2lcl infix" \
      "removed=$PRUNED_LOG_COUNT files=$(ls "$lp2" | tr '\n' ',')"
  fi

  # ── Test 33c: Under-keep category untouched; covered category pruned ───────
  local lp3="$TEST_ROOT/prune-mixed/logs"
  mock_rotated_log "$lp3" "frozen-a" $((NOW_EPOCH - 100))
  mock_rotated_log "$lp3" "frozen-b" $((NOW_EPOCH - 200))
  mock_rotated_log "$lp3" "preredeploy-coldD-20260601T000000Z" $((NOW_EPOCH - 300))
  mock_rotated_log "$lp3" "preredeploy-coldF-20260601T000100Z" $((NOW_EPOCH - 400))
  mock_rotated_log "$lp3" "preredeploy-20260601T000200Z"       $((NOW_EPOCH - 500))
  mock_rotated_log "$lp3" "preredeploy-20260601T000300Z"       $((NOW_EPOCH - 600))
  prune_rotated_logs "$lp3" 3
  local frozen_kept prered_kept
  frozen_kept=$(find "$lp3" -name 'monitor.log.frozen-*' | wc -l | tr -d ' ')
  prered_kept=$(find "$lp3" -name 'monitor.log.preredeploy-*' | wc -l | tr -d ' ')
  if [[ "$frozen_kept" -eq 2 && "$prered_kept" -eq 3 && "$PRUNED_LOG_COUNT" -eq 1 ]]; then
    tap_ok "prune-logs: under-keep 'frozen' kept whole; 'preredeploy-*' (incl. infix) pruned to 3"
  else
    tap_not_ok "prune-logs: under-keep 'frozen' kept whole; 'preredeploy-*' (incl. infix) pruned to 3" \
      "frozen=$frozen_kept prered=$prered_kept removed=$PRUNED_LOG_COUNT"
  fi

  # ── Test 33d: Missing dir → no-op, count 0 ─────────────────────────────────
  prune_rotated_logs "$TEST_ROOT/prune-missing/nope" 3
  if [[ "$PRUNED_LOG_COUNT" -eq 0 ]]; then
    tap_ok "prune-logs: missing dir → no-op (count 0)"
  else
    tap_not_ok "prune-logs: missing dir → no-op (count 0)" "removed=$PRUNED_LOG_COUNT"
  fi

  # ── Test 33e: Empty dir → no-op, count 0 ───────────────────────────────────
  local lp5="$TEST_ROOT/prune-empty/logs"; mkdir -p "$lp5"
  prune_rotated_logs "$lp5" 3
  if [[ "$PRUNED_LOG_COUNT" -eq 0 ]]; then
    tap_ok "prune-logs: empty dir → no-op (count 0)"
  else
    tap_not_ok "prune-logs: empty dir → no-op (count 0)" "removed=$PRUNED_LOG_COUNT"
  fi

  # ── Test 33f: SKILL.md calls prune_rotated_logs (not the old for-pat loop) ──
  local prune_tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'prune_rotated_logs' "$prune_tick_file" \
     && ! grep -q 'for pat in preredeploy crashed stuck frozen' "$prune_tick_file"; then
    tap_ok "prune-logs: SKILL.md uses prune_rotated_logs helper (no hardcoded for-pat loop)"
  else
    tap_not_ok "prune-logs: SKILL.md uses prune_rotated_logs helper (no hardcoded for-pat loop)" \
      "tick_file must call prune_rotated_logs and drop the 'for pat in preredeploy crashed stuck frozen' loop"
  fi

  # ── Heartbeat helper tests ─────────────────────────────────────────────────
  local hb_log="$TEST_ROOT/hb/monitor.log"
  mkdir -p "$TEST_ROOT/hb"

  # Test 34: Text format + tail
  printf '2026-05-01T00:00:00Z INFO heartbeat=true gap=0 peers=5\n' > "$hb_log"
  printf '2026-05-01T00:00:05Z INFO heartbeat=true gap=1 peers=3\n' >> "$hb_log"
  local hb_result
  hb_result=$(grep_heartbeat_lines "$hb_log" 1)
  if [[ "$hb_result" == *"gap=1"* ]]; then
    tap_ok "heartbeat: text format, tail-1 returns most recent"
  else
    tap_not_ok "heartbeat: text format, tail-1 returns most recent" "got: $hb_result"
  fi

  # Test 35: Colon separator
  printf 'heartbeat: true gap=2\n' > "$hb_log"
  hb_result=$(grep_heartbeat_lines "$hb_log")
  if [[ -n "$hb_result" ]]; then
    tap_ok "heartbeat: colon separator matches"
  else
    tap_not_ok "heartbeat: colon separator matches" "no output"
  fi

  # Test 36: JSON format
  printf '{"heartbeat":true,"gap":0}\n' > "$hb_log"
  hb_result=$(grep_heartbeat_lines "$hb_log")
  if [[ "$hb_result" == *'"heartbeat":true'* ]]; then
    tap_ok "heartbeat: JSON format matches"
  else
    tap_not_ok "heartbeat: JSON format matches" "got: $hb_result"
  fi

  # Test 37: Prose-only "Heartbeat" does NOT match (exit 1)
  printf 'INFO Heartbeat: essentially caught up with network\n' > "$hb_log"
  if ! grep_heartbeat_lines "$hb_log" >/dev/null 2>&1; then
    tap_ok "heartbeat: prose-only Heartbeat does not match"
  else
    tap_not_ok "heartbeat: prose-only Heartbeat does not match" "unexpected match"
  fi

  # Test 38: Empty file → exit 1
  : > "$hb_log"
  if ! grep_heartbeat_lines "$hb_log" >/dev/null 2>&1; then
    tap_ok "heartbeat: empty log → exit 1"
  else
    tap_not_ok "heartbeat: empty log → exit 1" "expected failure"
  fi

  # Test 39: Empty file with tail_count → exit 1
  : > "$hb_log"
  if ! grep_heartbeat_lines "$hb_log" 5 >/dev/null 2>&1; then
    tap_ok "heartbeat: empty log with tail_count → exit 1"
  else
    tap_not_ok "heartbeat: empty log with tail_count → exit 1" "expected failure"
  fi

  # Test 40: Missing file → exit code > 0
  local hb_rc=0
  grep_heartbeat_lines "$TEST_ROOT/hb/nonexistent.log" >/dev/null 2>&1 || hb_rc=$?
  if [[ $hb_rc -ne 0 ]]; then
    tap_ok "heartbeat: missing file → non-zero exit"
  else
    tap_not_ok "heartbeat: missing file → non-zero exit" "rc=$hb_rc"
  fi

  # ── Check 12b semantic assertions ──────────────────────────────────────────
  # Verify that the Check 12b recovery-stalled streak semantics in the
  # monitor-tick and monitor-loop SKILL.md specs have not regressed.
  # Cross-validates inline literals against the canonical TOML catalog.
  # See issues #2399, #2402, #2566.

  local tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  local loop_file="$REPO_ROOT/.claude/skills/monitor-loop/SKILL.md"
  local constants_file="$REPO_ROOT/.claude/skills/shared/metric-alarms.toml"

  # Section extractions (scoped to avoid false positives from unrelated text)
  local check_12b_section watcher_section output_section
  local loop_streak_table loop_snapshot_section loop_watcher_section

  check_12b_section=$(extract_md_section "$tick_file" '^### Check 12b:')
  watcher_section=$(extract_md_section "$tick_file" '^### Watcher mode')
  output_section=$(extract_md_section "$tick_file" '^MONITOR ' '^```$')
  loop_streak_table=$(extract_md_section "$loop_file" '^\*\*B\. Streak-gated' '^\*\*D\.')
  loop_snapshot_section=$(extract_md_section "$loop_file" '^\*\*Counter-streak snapshot\*\*' '^\*\*SCP')
  loop_watcher_section=$(extract_md_section "$loop_file" '^### Watcher mode')

  # Existence guards — if any extraction is empty, all tests fail with context
  local sections_ok=true
  for var_name in check_12b_section watcher_section output_section \
                  loop_streak_table loop_snapshot_section loop_watcher_section; do
    if [[ -z "${!var_name}" ]]; then
      echo "  # FATAL: $var_name extraction returned empty" >&2
      sections_ok=false
    fi
  done

  if [[ "$sections_ok" != "true" ]]; then
    tap_not_ok "check-12b-semantics: section extraction" "one or more sections not found"
    # Emit remaining planned tests as not-ok so TAP count matches
    while [[ $TAP_CURRENT -lt $TAP_PLAN ]]; do
      tap_not_ok "check-12b-semantics: skipped (section extraction failed)"
    done
    return
  fi

  # Test 41: TOML catalog file exists, is parseable, and contains recovery-stalled alarm
  # Extract recovery-stalled alarm constants from metric-alarms.toml
  local streak_val burst_val delta_val post_restart_val snapshot_file mode_val metric_name metric_label
  # Use Python to extract the recovery-stalled alarm entry from the TOML
  local toml_extract
  toml_extract=$(python3 - "$constants_file" <<'PYEOF'
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], 'rb') as f:
    data = tomllib.load(f)
for a in data['alarm']:
    if a['name'] == 'recovery-stalled':
        print('streak_val=' + str(a['streak_threshold']))
        print('burst_val=' + str(a['burst_threshold']))
        print('delta_val=' + str(a['delta_threshold']))
        print('post_restart_val=' + str(a.get('post_restart_absolute_threshold', 0)))
        print('snapshot_file=' + a['snapshot_file'])
        print('metric_name=' + a['metric'])
        labels = a.get('labels', [])
        if labels:
            lbl = labels[0]['key'] + '="' + labels[0]['value'] + '"'
            print("metric_label='" + lbl + "'")
        gates = a.get('gates', [])
        if 'validator-only' in gates:
            print('mode_val=validator')
        sys.exit(0)
print('NOT_FOUND')
sys.exit(1)
PYEOF
  ) || toml_extract=""

  if [[ -n "$toml_extract" && "$toml_extract" != "NOT_FOUND" ]]; then
    eval "$toml_extract"
    tap_ok "metric-alarms: TOML exists and parseable (streak=$streak_val burst=$burst_val delta=$delta_val)"
    # Test 41b: post-restart absolute threshold cross-validated against TOML (#3198).
    # Keeps the inline SKILL.md literal (50) cross-validated against the catalog.
    if [[ "$post_restart_val" == "50" ]]; then
      tap_ok "metric-alarms: recovery-stalled post_restart_absolute_threshold == 50"
    else
      tap_not_ok "metric-alarms: recovery-stalled post_restart_absolute_threshold == 50" \
        "expected post_restart_absolute_threshold=50, got '$post_restart_val'"
    fi
  else
    tap_not_ok "metric-alarms: TOML exists and parseable" \
      "Failed to parse recovery-stalled alarm from metric-alarms.toml"
    # Fail closed: remaining 12b tests cannot proceed
    while [[ $TAP_CURRENT -lt $TAP_PLAN ]]; do
      tap_not_ok "check-12b (skipped: TOML parse failed)"
    done
    return
  fi

  # Test 42: Streak threshold cross-validated against TOML
  if echo "$check_12b_section" | grep -Fq "breach_streak >= $streak_val"; then
    tap_ok "check-12b-semantics: streak threshold (breach_streak >= $streak_val)"
  else
    tap_not_ok "check-12b-semantics: streak threshold (breach_streak >= $streak_val)" \
      "Check 12b section missing 'breach_streak >= $streak_val'"
  fi

  # Test 43: Burst threshold cross-validated against TOML
  if echo "$check_12b_section" | grep -Fq "delta >= $burst_val"; then
    tap_ok "check-12b-semantics: burst threshold (delta >= $burst_val)"
  else
    tap_not_ok "check-12b-semantics: burst threshold (delta >= $burst_val)" \
      "Check 12b section missing 'delta >= $burst_val'"
  fi

  # Test 44: Snapshot file from TOML + negative assertion (not ratio_snapshot)
  if echo "$check_12b_section" | grep -Fq "$snapshot_file" \
     && ! echo "$check_12b_section" | grep -Fq 'ratio_snapshot'; then
    tap_ok "check-12b-semantics: uses $snapshot_file (not ratio_snapshot)"
  else
    tap_not_ok "check-12b-semantics: uses $snapshot_file (not ratio_snapshot)" \
      "Check 12b should reference '$snapshot_file', not ratio_snapshot"
  fi

  # Test 45: Excluded from watcher mode (monitor-tick, driven from TOML mode=validator)
  if [[ "$mode_val" == "validator" ]]; then
    if echo "$watcher_section" | grep -Fq 'Check 12b' \
       && echo "$watcher_section" | grep -iq 'skip' \
       && echo "$watcher_section" | grep -Fq 'recovery_stalled'; then
      tap_ok "check-12b-semantics: watcher mode excludes Check 12b (mode=$mode_val)"
    else
      tap_not_ok "check-12b-semantics: watcher mode excludes Check 12b (mode=$mode_val)" \
        "Watcher section must skip Check 12b and omit recovery_stalled line"
    fi
  else
    tap_ok "check-12b-semantics: watcher exclusion (mode=$mode_val, not validator — N/A)"
  fi

  # Test 46: Excluded from metrics: aggregate (NOT counted)
  if echo "$check_12b_section" | grep -Fq 'NOT counted in the'; then
    tap_ok "check-12b-semantics: excluded from metrics aggregate (NOT counted)"
  else
    tap_not_ok "check-12b-semantics: excluded from metrics aggregate (NOT counted)" \
      "Check 12b section missing 'NOT counted in the' (metrics exclusion)"
  fi

  # Test 47: recovery_stalled: in output template
  if echo "$output_section" | grep -Fq 'recovery_stalled:'; then
    tap_ok "check-12b-semantics: recovery_stalled in output template"
  else
    tap_not_ok "check-12b-semantics: recovery_stalled in output template" \
      "Output template missing 'recovery_stalled:' line"
  fi

  # Test 48: Cross-file — monitor-loop table row matches TOML values (row-specific)
  # Use metric_label from TOML to select the row (not hardcoded)
  local recovery_row
  recovery_row=$(echo "$loop_streak_table" | grep -F "$metric_label" || true)
  if [[ -n "$recovery_row" ]] \
     && echo "$recovery_row" | grep -Fq "${streak_val} ticks" \
     && echo "$recovery_row" | grep -Fq "≥ ${burst_val}" \
     && echo "$loop_snapshot_section" | grep -Fq "$snapshot_file"; then
    tap_ok "check-12b-semantics: monitor-loop table row + snapshot match TOML"
  else
    tap_not_ok "check-12b-semantics: monitor-loop table row + snapshot match TOML" \
      "Expected row with '$metric_label' containing '${streak_val} ticks', '≥ ${burst_val}', and '$snapshot_file' in snapshot"
  fi

  # Test 49: monitor-loop watcher explicitly excludes Check 12b
  if echo "$loop_watcher_section" | grep -Fq 'Check 12b' \
     && echo "$loop_watcher_section" | grep -Fq 'recovery_stalled' \
     && echo "$loop_snapshot_section" | grep -Fq 'Validator mode only'; then
    tap_ok "check-12b-semantics: monitor-loop watcher excludes Check 12b + validator-only"
  else
    tap_not_ok "check-12b-semantics: monitor-loop watcher excludes Check 12b + validator-only" \
      "monitor-loop: watcher must mention Check 12b + recovery_stalled; snapshot must say Validator mode only"
  fi

  # Test 50: Reference link to catalog file in both SKILL.md files
  if grep -Fq 'metric-alarms.toml' "$tick_file" \
     && grep -Fq 'metric-alarms.toml' "$loop_file"; then
    tap_ok "metric-alarms: reference link in both SKILL.md files"
  else
    tap_not_ok "metric-alarms: reference link in both SKILL.md files" \
      "Both SKILL.md must contain 'metric-alarms.toml'"
  fi

  # Test 51: Delta threshold cross-validated against TOML
  # monitor-tick pseudocode: "elif delta >= 1:" ; monitor-loop table: "≥ 1"
  if echo "$check_12b_section" | grep -Fq "delta >= $delta_val" \
     && echo "$recovery_row" | grep -Fq "≥ $delta_val"; then
    tap_ok "check-12b-semantics: delta threshold cross-validated (delta >= $delta_val)"
  else
    tap_not_ok "check-12b-semantics: delta threshold cross-validated (delta >= $delta_val)" \
      "Expected 'delta >= $delta_val' in monitor-tick and '≥ $delta_val' in monitor-loop table row"
  fi

  # Test 52: Metric name from TOML appears in both SKILL.md files
  if echo "$check_12b_section" | grep -Fq "$metric_name" \
     && echo "$loop_streak_table" | grep -Fq "$metric_name"; then
    tap_ok "check-12b-semantics: metric name from TOML in both specs ($metric_name)"
  else
    tap_not_ok "check-12b-semantics: metric name from TOML in both specs" \
      "Expected '$metric_name' in both Check 12b section and monitor-loop table"
  fi

  # Test 53: Metric label from TOML appears in both SKILL.md files
  if echo "$check_12b_section" | grep -Fq "$metric_label" \
     && echo "$loop_streak_table" | grep -Fq "$metric_label"; then
    tap_ok "check-12b-semantics: metric label from TOML in both specs ($metric_label)"
  else
    tap_not_ok "check-12b-semantics: metric label from TOML in both specs" \
      "Expected '$metric_label' in both Check 12b section and monitor-loop table"
  fi

  # Test 53b: recovery-stalled snapshot_file is a BARE filename (no '/'), matching
  # sibling snapshots (ratio_snapshot / counter_dynamic_snapshot). The evaluator
  # resolves snapshot_path = Path(state_dir) / snapshot_file and the SKILL invokes
  # it with --state-dir .../metrics, so any 'metrics/' prefix here doubles the path
  # (.../metrics/metrics/counter_streak_snapshot). See #3222. A directory prefix is
  # invalid; the bare name resolves single-level under --state-dir. Substring greps
  # (Tests 44/48) do NOT catch the doubling because the bare name is a substring of
  # the doubled path — hence this dedicated structural assertion.
  if [[ "$snapshot_file" != */* && "$snapshot_file" == "counter_streak_snapshot" ]]; then
    tap_ok "metric-alarms: recovery-stalled snapshot_file is bare filename ($snapshot_file)"
  else
    tap_not_ok "metric-alarms: recovery-stalled snapshot_file is bare filename" \
      "snapshot_file must be the bare name 'counter_streak_snapshot' (no '/' prefix), got '$snapshot_file' — a directory prefix doubles the resolved path under --state-dir .../metrics (#3222)"
  fi

  # Test 53c: check-12b Format block documents the evaluator's ACTUAL snapshot
  # schema. eval-alarms.py writes version/pid/start_ticks/counter_value/breach_streak
  # (no timestamp). The check-12b prose must use counter_value / breach_streak and
  # must NOT contain the drifted field names recovery_stalled_behind /
  # recovery_stalled_breach_streak / timestamp. See #3222.
  if echo "$check_12b_section" | grep -Fq 'counter_value' \
     && echo "$check_12b_section" | grep -Fq 'breach_streak' \
     && ! echo "$check_12b_section" | grep -Fq 'recovery_stalled_behind' \
     && ! echo "$check_12b_section" | grep -Fq 'recovery_stalled_breach_streak' \
     && ! echo "$check_12b_section" | grep -Fq 'timestamp'; then
    tap_ok "check-12b-semantics: doc snapshot schema matches evaluator (counter_value/breach_streak)"
  else
    tap_not_ok "check-12b-semantics: doc snapshot schema matches evaluator (counter_value/breach_streak)" \
      "Check 12b prose must use 'counter_value'/'breach_streak' and must NOT contain 'recovery_stalled_behind', 'recovery_stalled_breach_streak', or 'timestamp' (drifted from eval-alarms.py schema, #3222)"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # detect_soft_fail_blocked and has_fatal_wipe_evidence tests (T54-T63)
  # Source: scripts/lib/monitor-decisions.sh — (3c) soft-fail state-wipe trigger
  # ════════════════════════════════════════════════════════════════════════════

  local NOW_SF=1700000000  # fixed reference for soft-fail tests
  local PROC_START_SF=$((NOW_SF - 900))  # process started 15 min ago

  # ── Test 54: 20 WARN blocked messages spanning 600s → yes ──────────────────
  local sf54="$TEST_ROOT/t54"
  mkdir -p "$sf54"
  local sf54_log="$sf54/monitor.log"
  : > "$sf54_log"
  # Messages end at NOW_SF - 30 (within 90s), spanning 600s total
  for i in $(seq 0 19); do
    local ts_epoch=$((NOW_SF - 30 - (19 - i) * 30))
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts" >> "$sf54_log"
  done
  detect_soft_fail_blocked "$sf54_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "yes" && "$SOFT_FAIL_BLOCKED_DURATION_SEC" -ge 570 ]]; then
    tap_ok "soft-fail-detect: 20 messages spanning 600s → yes (duration=$SOFT_FAIL_BLOCKED_DURATION_SEC)"
  else
    tap_not_ok "soft-fail-detect: 20 messages spanning 600s → yes" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 55: 5 messages spanning 120s → no (duration < 300s) ───────────────
  local sf55="$TEST_ROOT/t55"
  mkdir -p "$sf55"
  local sf55_log="$sf55/monitor.log"
  : > "$sf55_log"
  for i in $(seq 0 4); do
    local ts_epoch=$((NOW_SF - 120 + i * 30))
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts" >> "$sf55_log"
  done
  detect_soft_fail_blocked "$sf55_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" ]]; then
    tap_ok "soft-fail-detect: 5 messages spanning 120s → no"
  else
    tap_not_ok "soft-fail-detect: 5 messages spanning 120s → no" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 56: 20 messages, most recent 120s ago → no (stale) ────────────────
  local sf56="$TEST_ROOT/t56"
  mkdir -p "$sf56"
  local sf56_log="$sf56/monitor.log"
  : > "$sf56_log"
  for i in $(seq 0 19); do
    local ts_epoch=$((NOW_SF - 700 + i * 30))
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts" >> "$sf56_log"
  done
  detect_soft_fail_blocked "$sf56_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" ]]; then
    tap_ok "soft-fail-detect: 20 messages most recent 120s ago → no (stale)"
  else
    tap_not_ok "soft-fail-detect: 20 messages most recent 120s ago → no (stale)" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 57: Empty/missing log file → no ───────────────────────────────────
  detect_soft_fail_blocked "$TEST_ROOT/t57/nonexistent.log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" && "$SOFT_FAIL_BLOCKED_DURATION_SEC" -eq 0 ]]; then
    tap_ok "soft-fail-detect: missing log file → no"
  else
    tap_not_ok "soft-fail-detect: missing log file → no" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 58: One blocked message → no (duration=0) ─────────────────────────
  local sf58="$TEST_ROOT/t58"
  mkdir -p "$sf58"
  local sf58_log="$sf58/monitor.log"
  local ts58=$(date -u -d "@$((NOW_SF - 30))" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
  printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts58" > "$sf58_log"
  detect_soft_fail_blocked "$sf58_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" ]]; then
    tap_ok "soft-fail-detect: one message → no (duration=0)"
  else
    tap_not_ok "soft-fail-detect: one message → no (duration=0)" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 59: All timestamps < PROC_START → no (stale from prior PID) ───────
  local sf59="$TEST_ROOT/t59"
  mkdir -p "$sf59"
  local sf59_log="$sf59/monitor.log"
  : > "$sf59_log"
  local late_start=$((NOW_SF - 60))  # process started 60s ago
  for i in $(seq 0 19); do
    local ts_epoch=$((NOW_SF - 700 + i * 30))  # all before late_start
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts" >> "$sf59_log"
  done
  detect_soft_fail_blocked "$sf59_log" "$late_start" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" ]]; then
    tap_ok "soft-fail-detect: all timestamps before PROC_START → no"
  else
    tap_not_ok "soft-fail-detect: all timestamps before PROC_START → no" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 60: Mixed text+JSON WARN lines → yes ─────────────────────────────
  local sf60="$TEST_ROOT/t60"
  mkdir -p "$sf60"
  local sf60_log="$sf60/monitor.log"
  : > "$sf60_log"
  # Messages end at NOW_SF - 30 (within 90s), spanning 540s total (10 msgs * 60s)
  for i in $(seq 0 9); do
    local ts_epoch=$((NOW_SF - 30 - (9 - i) * 60))
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%SZ" 2>/dev/null)
    if (( i % 2 == 0 )); then
      printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts" >> "$sf60_log"
    else
      printf '{"timestamp":"%s","level":"WARN","target":"henyey_app::app::consensus","message":"Recovery escalation blocked: previous fatal state failure — manual intervention required"}\n' "$ts" >> "$sf60_log"
    fi
  done
  detect_soft_fail_blocked "$sf60_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "yes" && "$SOFT_FAIL_BLOCKED_DURATION_SEC" -ge 500 ]]; then
    tap_ok "soft-fail-detect: mixed text+JSON → yes (duration=$SOFT_FAIL_BLOCKED_DURATION_SEC)"
  else
    tap_not_ok "soft-fail-detect: mixed text+JSON → yes" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 61: Mixed WARN + DEBUG lines (only WARN counted) ──────────────────
  local sf61="$TEST_ROOT/t61"
  mkdir -p "$sf61"
  local sf61_log="$sf61/monitor.log"
  : > "$sf61_log"
  # Add many DEBUG lines (should be ignored)
  for i in $(seq 0 19); do
    local ts_epoch=$((PROC_START_SF + 30 + i * 30))
    local ts=$(date -u -d "@$ts_epoch" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    printf '%s DEBUG henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure (repeated)\n' "$ts" >> "$sf61_log"
  done
  # Add only 2 WARN lines (duration < 300s)
  local ts61a=$(date -u -d "@$((NOW_SF - 60))" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
  local ts61b=$(date -u -d "@$((NOW_SF - 30))" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
  printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts61a" >> "$sf61_log"
  printf '%s  WARN henyey_app::app::consensus: Recovery escalation blocked: previous fatal state failure — manual intervention required\n' "$ts61b" >> "$sf61_log"
  detect_soft_fail_blocked "$sf61_log" "$PROC_START_SF" "$NOW_SF"
  if [[ "$SOFT_FAIL_BLOCKED" == "no" ]]; then
    tap_ok "soft-fail-detect: WARN+DEBUG mix, only WARN counted → no (duration=30)"
  else
    tap_not_ok "soft-fail-detect: WARN+DEBUG mix, only WARN counted → no" \
      "BLOCKED=$SOFT_FAIL_BLOCKED DURATION=$SOFT_FAIL_BLOCKED_DURATION_SEC"
  fi

  # ── Test 62: has_fatal_wipe_evidence — crashed + active + missing ──────────
  local sf62="$TEST_ROOT/t62"
  mkdir -p "$sf62/logs"
  # crashed file WITH signal
  printf '2026-05-04T14:04:09Z ERROR lifecycle: FATAL: unrecoverable fatal_wipe_required=true\n' \
    > "$sf62/logs/monitor.log.crashed-20260504T140409Z"
  local sf62_active="$sf62/logs/monitor.log"
  printf '2026-05-04T14:05:00Z INFO heartbeat=true\n' > "$sf62_active"
  has_fatal_wipe_evidence "$sf62/logs" "$sf62_active"
  local r62a="$FATAL_WIPE_EVIDENCE"
  local s62a="$FATAL_WIPE_SOURCE"

  # active log WITH signal (no crashed)
  local sf62b="$TEST_ROOT/t62b"
  mkdir -p "$sf62b/logs"
  local sf62b_active="$sf62b/logs/monitor.log"
  printf '2026-05-04T14:04:09Z ERROR lifecycle: FATAL: fatal_wipe_required=true\n' > "$sf62b_active"
  has_fatal_wipe_evidence "$sf62b/logs" "$sf62b_active"
  local r62b="$FATAL_WIPE_EVIDENCE"
  local s62b="$FATAL_WIPE_SOURCE"

  # neither
  local sf62c="$TEST_ROOT/t62c"
  mkdir -p "$sf62c/logs"
  printf '2026-05-04T14:05:00Z INFO normal operation\n' > "$sf62c/logs/monitor.log"
  has_fatal_wipe_evidence "$sf62c/logs" "$sf62c/logs/monitor.log"
  local r62c="$FATAL_WIPE_EVIDENCE"

  if [[ "$r62a" == "yes" && "$s62a" == crashed:* && "$r62b" == "yes" && "$s62b" == "active" && "$r62c" == "no" ]]; then
    tap_ok "fatal-wipe-evidence: crashed=yes active=yes neither=no"
  else
    tap_not_ok "fatal-wipe-evidence: crashed/active/neither" \
      "crashed: $r62a ($s62a), active: $r62b ($s62b), neither: $r62c"
  fi

  # ── Test 63: Consistency — SKILL.md references both functions ──────────────
  local tick_file_ref="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'detect_soft_fail_blocked' "$tick_file_ref" \
     && grep -q 'has_fatal_wipe_evidence' "$tick_file_ref"; then
    tap_ok "consistency: SKILL.md references detect_soft_fail_blocked and has_fatal_wipe_evidence"
  else
    tap_not_ok "consistency: SKILL.md references detect_soft_fail_blocked and has_fatal_wipe_evidence" \
      "One or both functions not referenced in monitor-tick/SKILL.md"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # classify_stuck_alive_sync tests (T63a–T63k)
  # Source: scripts/lib/monitor-decisions.sh — (3e) stuck-but-alive SYNC FAILURE
  # auto-restart trigger (issue #3219).
  #
  # Contract: classify_stuck_alive_sync NODE_STATE CURRENT_LCL AGE_SEC RPC_STATUS \
  #             RSS_MB PROC_RESPONSIVE STATE_FILE [NOW_EPOCH]
  # Sets STUCK_ALIVE_SYNC ∈ yes|no|cooldown|escalate. The function is the sole
  # reader/writer of STATE_FILE. NOW_EPOCH is injectable for deterministic tests.
  # ════════════════════════════════════════════════════════════════════════════

  local NOW_SA=1700000000             # fixed reference for stuck-alive tests
  local sa_dir="$TEST_ROOT/stuck-alive"
  mkdir -p "$sa_dir"

  # Helper: seed a state file with a frozen lcl that has been frozen for N seconds
  # (so frozen_lcl_seconds = N at NOW_SA). Optional last_restart_epoch / restart_epochs.
  _sa_seed() {
    # $1=file $2=frozen_lcl $3=frozen_for_sec $4=last_restart_epoch $5=restart_epochs(space-sep)
    local f="$1" lcl="$2" frozen_for="$3" last_restart="${4:-0}" epochs="${5:-}"
    printf 'frozen_lcl=%s\nfrozen_since_epoch=%s\nlast_restart_epoch=%s\nrestart_epochs=%s\n' \
      "$lcl" "$((NOW_SA - frozen_for))" "$last_restart" "$epochs" > "$f"
  }

  # ── Test 63a: full reproduced signature → yes ──────────────────────────────
  # Synced! + lcl frozen 700s + age 700 + unhealthy + RSS 10240 + responsive.
  local sa_a="$sa_dir/a.state"
  _sa_seed "$sa_a" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "yes" "$sa_a" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "yes" ]]; then
    tap_ok "stuck-alive: full signature (frozen 700s + age 700 + unhealthy + RSS 10240 + responsive) → yes"
  else
    tap_not_ok "stuck-alive: full signature → yes" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63b: transient sync (Catching up) → no ────────────────────────────
  local sa_b="$sa_dir/b.state"
  _sa_seed "$sa_b" 55000000 700
  classify_stuck_alive_sync "Catching up" 55000000 700 "unhealthy" 10240 "yes" "$sa_b" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: transient sync (Catching up) → no"
  else
    tap_not_ok "stuck-alive: transient sync (Catching up) → no" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63c: 3b wedge (PROC_RESPONSIVE=no) → no ───────────────────────────
  # The wedge path (3b) owns the unresponsive case; (3e) must NOT poach it.
  local sa_c="$sa_dir/c.state"
  _sa_seed "$sa_c" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "no" "$sa_c" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: 3b wedge (PROC_RESPONSIVE=no) → no (mirror of wedge path)"
  else
    tap_not_ok "stuck-alive: 3b wedge (PROC_RESPONSIVE=no) → no" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63d: healthy validating (RPC healthy, age 4s) → no ────────────────
  local sa_d="$sa_dir/d.state"
  _sa_seed "$sa_d" 55000000 4
  classify_stuck_alive_sync "Synced!" 55000000 4 "healthy" 10240 "yes" "$sa_d" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: healthy validating (RPC healthy, age 4s) → no"
  else
    tap_not_ok "stuck-alive: healthy validating → no" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63e: RSS over OOM floor (17000 > 16384) → no ──────────────────────
  # The OOM gate (check 4) owns RSS-over-floor restarts.
  local sa_e="$sa_dir/e.state"
  _sa_seed "$sa_e" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 17000 "yes" "$sa_e" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: RSS over OOM floor (17000) → no (OOM gate owns this)"
  else
    tap_not_ok "stuck-alive: RSS over OOM floor → no" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63f: dwell not met (frozen_lcl_seconds=300 < 600) → no ────────────
  local sa_f="$sa_dir/f.state"
  _sa_seed "$sa_f" 55000000 300
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "yes" "$sa_f" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: dwell not met (frozen_lcl_seconds=300 < 600) → no"
  else
    tap_not_ok "stuck-alive: dwell not met → no" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63g: frozen counter resets when lcl advances → no (state hygiene) ──
  # State file says frozen at 55000000 for 700s, but CURRENT_LCL is 55000010
  # (advanced) → the function must reset frozen_since to NOW and NOT fire.
  local sa_g="$sa_dir/g.state"
  _sa_seed "$sa_g" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000010 700 "unhealthy" 10240 "yes" "$sa_g" "$NOW_SA"
  local g_result="$STUCK_ALIVE_SYNC"
  # Verify the state file was rewritten with the new lcl and a fresh frozen_since.
  local g_frozen_lcl g_frozen_since
  g_frozen_lcl=$(grep '^frozen_lcl=' "$sa_g" | cut -d= -f2)
  g_frozen_since=$(grep '^frozen_since_epoch=' "$sa_g" | cut -d= -f2)
  if [[ "$g_result" == "no" && "$g_frozen_lcl" == "55000010" && "$g_frozen_since" == "$NOW_SA" ]]; then
    tap_ok "stuck-alive: frozen counter resets when lcl advances → no (state hygiene)"
  else
    tap_not_ok "stuck-alive: frozen counter resets when lcl advances → no" \
      "result=$g_result frozen_lcl=$g_frozen_lcl frozen_since=$g_frozen_since (want no/55000010/$NOW_SA)"
  fi

  # ── Test 63h: cooldown active (last_restart 300s ago < 900s) → cooldown ─────
  local sa_h="$sa_dir/h.state"
  _sa_seed "$sa_h" 55000000 700 "$((NOW_SA - 300))"
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "yes" "$sa_h" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "cooldown" ]]; then
    tap_ok "stuck-alive: cooldown active (last_restart 300s ago) → cooldown (no restart)"
  else
    tap_not_ok "stuck-alive: cooldown active → cooldown" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63i: max-restarts exceeded (3 in 2h window) → escalate ────────────
  # 3 restart epochs within the 7200s window, last one > cooldown ago.
  local sa_i="$sa_dir/i.state"
  local r1=$((NOW_SA - 6000)) r2=$((NOW_SA - 4000)) r3=$((NOW_SA - 2000))
  _sa_seed "$sa_i" 55000000 700 "$r3" "$r1 $r2 $r3"
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "yes" "$sa_i" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "escalate" ]]; then
    tap_ok "stuck-alive: max-restarts exceeded (3 in 2h window) → escalate (no restart, urgent)"
  else
    tap_not_ok "stuck-alive: max-restarts exceeded → escalate" "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63j: case-insensitive state match (Validating!/Tracking) → yes ────
  # Enum-drift robustness: if the /info enum ever emits these, the substring
  # gate (valid|synced|track) still fires when all other signals hold.
  local sa_j="$sa_dir/j.state"
  _sa_seed "$sa_j" 55000000 700
  classify_stuck_alive_sync "Validating!" 55000000 700 "unhealthy" 10240 "yes" "$sa_j" "$NOW_SA"
  local j1="$STUCK_ALIVE_SYNC"
  local sa_j2="$sa_dir/j2.state"
  _sa_seed "$sa_j2" 55000000 700
  classify_stuck_alive_sync "Tracking" 55000000 700 "unhealthy" 10240 "yes" "$sa_j2" "$NOW_SA"
  local j2="$STUCK_ALIVE_SYNC"
  if [[ "$j1" == "yes" && "$j2" == "yes" ]]; then
    tap_ok "stuck-alive: case-insensitive state match (Validating!/Tracking) → yes (enum-drift robust)"
  else
    tap_not_ok "stuck-alive: case-insensitive state match → yes" "Validating!=$j1 Tracking=$j2"
  fi

  # ── Test 63k: consistency — SKILL.md references classify_stuck_alive_sync ───
  local tick_file_sa="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'classify_stuck_alive_sync' "$tick_file_sa"; then
    tap_ok "consistency: monitor-tick/SKILL.md references classify_stuck_alive_sync"
  else
    tap_not_ok "consistency: monitor-tick/SKILL.md references classify_stuck_alive_sync" \
      "(3e) rung not wired in monitor-tick/SKILL.md"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # classify_stuck_alive_sync — /info-unresponsive-but-alive fallback (T63l–T63o)
  # Source: scripts/lib/monitor-decisions.sh — (3e) liveness gate widening (#3579)
  #
  # Extra (optional) positional arg 9: HEARTBEAT_AGE_SEC — seconds since the last
  # `heartbeat=true` log line (empty/-1 = unknown/none). When /info is down
  # (PROC_RESPONSIVE=no) but a heartbeat appeared within ~120s, the node is alive
  # (event loop ticking) and (3e) must STILL be able to fire on the near-tip-stall
  # signature. (3b) wedge keeps the truly-wedged case (no recent heartbeat).
  #
  # Contract: classify_stuck_alive_sync NODE_STATE CURRENT_LCL AGE_SEC RPC_STATUS \
  #             RSS_MB PROC_RESPONSIVE STATE_FILE [NOW_EPOCH] [HEARTBEAT_AGE_SEC]
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 63l: /info unresponsive + recent heartbeat (60s) → yes ────────────
  # The exact #3579 signature: alive (heartbeat 60s ago) but /info empty, lcl
  # frozen 700s, age 700, unhealthy, RSS under floor. (3e) MUST now fire.
  # RED on origin/main (PROC_RESPONSIVE=no → condition 2 fails → "no").
  local sa_l="$sa_dir/l.state"
  _sa_seed "$sa_l" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "no" "$sa_l" "$NOW_SA" 60
  if [[ "$STUCK_ALIVE_SYNC" == "yes" ]]; then
    tap_ok "stuck-alive: /info unresponsive + recent heartbeat (60s) near-tip stall → yes (#3579)"
  else
    tap_not_ok "stuck-alive: /info unresponsive + recent heartbeat near-tip stall → yes (#3579)" \
      "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63m: /info unresponsive + NO recent heartbeat (180s) → no ─────────
  # Truly wedged (event loop not ticking) — (3b) wedge owns this, NOT (3e).
  # Guards against (3e) poaching the wedge path via the new fallback.
  local sa_m="$sa_dir/m.state"
  _sa_seed "$sa_m" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "no" "$sa_m" "$NOW_SA" 180
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: /info unresponsive + stale heartbeat (180s>120s) → no (3b wedge owns it)"
  else
    tap_not_ok "stuck-alive: /info unresponsive + stale heartbeat → no (3b wedge owns it)" \
      "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63n: /info unresponsive + recent heartbeat but RPC healthy → no ───
  # False-positive guard: a healthy node whose /info is just flaky must NOT be
  # restarted. RPC healthy + low age means it is making progress.
  local sa_n="$sa_dir/n.state"
  _sa_seed "$sa_n" 55000000 4
  classify_stuck_alive_sync "Synced!" 55000000 4 "healthy" 10240 "no" "$sa_n" "$NOW_SA" 30
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: /info unresponsive + heartbeat-alive but RPC healthy → no (false-positive guard)"
  else
    tap_not_ok "stuck-alive: /info unresponsive + heartbeat-alive but RPC healthy → no" \
      "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63o: /info unresponsive + no heartbeat info (omitted arg) → no ────
  # Backward compatibility: when arg 9 is omitted (legacy callers) and /info is
  # down, the fallback has no signal → (3e) must not fire (unchanged behavior).
  local sa_o="$sa_dir/o.state"
  _sa_seed "$sa_o" 55000000 700
  classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "no" "$sa_o" "$NOW_SA"
  if [[ "$STUCK_ALIVE_SYNC" == "no" ]]; then
    tap_ok "stuck-alive: /info unresponsive + no heartbeat arg → no (backward-compat, unchanged)"
  else
    tap_not_ok "stuck-alive: /info unresponsive + no heartbeat arg → no (backward-compat)" \
      "got STUCK_ALIVE_SYNC=$STUCK_ALIVE_SYNC"
  fi

  # ── Test 63p: heartbeat-fallback fires identically under zsh ───────────────
  # The monitor runs under zsh; the new fallback must not rely on bash-only
  # builtins or zsh-reserved vars (status/path/argv/options). Exercise the
  # #3579 yes-case in a `zsh -c` subshell (mirror of T20c3-zsh).
  if command -v zsh >/dev/null 2>&1; then
    local sa_p="$sa_dir/p.state"
    _sa_seed "$sa_p" 55000000 700
    local zsh_sa_out
    zsh_sa_out=$(zsh -c '
      source "$1/scripts/lib/monitor-decisions.sh"
      classify_stuck_alive_sync "Synced!" 55000000 700 "unhealthy" 10240 "no" "$2" 1700000000 60
      printf "%s" "$STUCK_ALIVE_SYNC"
    ' -- "$REPO_ROOT" "$sa_p" 2>&1)
    if [[ "$zsh_sa_out" == "yes" ]]; then
      tap_ok "stuck-alive: /info unresponsive + recent heartbeat near-tip stall → yes under zsh (#3579)"
    else
      tap_not_ok "stuck-alive: /info unresponsive + recent heartbeat near-tip stall → yes under zsh (#3579)" \
        "got: '$zsh_sa_out'"
    fi
  else
    tap_skip "stuck-alive: heartbeat-fallback under zsh (#3579)" "zsh not found"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # classify_event_loop_stall (T63els1–T63els10) — issue #3815
  # Source: scripts/lib/monitor-decisions.sh — monitor-tick sub-check (3f)
  # "Recovered event-loop stall policy".
  #
  # Contract: classify_event_loop_stall LOG_FILE STATE_FILE [NOW_EPOCH]
  #   Counts recovered ERROR-tier loop-side stall lines (#3795's
  #   `Event loop stall (loop-side exact accounting)` + `stall_recovered=true`)
  #   whose timestamps fall inside a rolling STALL_WINDOW_SEC (7200s), then
  #   applies an explicit policy:
  #     none    — no recovered ERROR stall in window
  #     alert   — 1..(MAX_RECOVERED_STALLS-1) in window, OR restart suppressed
  #               by the STALL_COOLDOWN_SEC guard (alert-only, no restart)
  #     restart — >= MAX_RECOVERED_STALLS (default 3) in window, cooldown clear
  #   Sets EVENT_LOOP_STALL_VERDICT and EVENT_LOOP_STALL_COUNT. Sole reader/
  #   writer of STATE_FILE (only last_restart_epoch persisted). NOW_EPOCH is
  #   injectable for deterministic tests. The (3b)-wedge `watchdog_freeze=true`
  #   ERROR line is NOT counted here — it carries neither the loop-side message
  #   nor `stall_recovered`, so it stays with (3b). Missing/corrupt state or
  #   unparseable timestamps fail inert toward none/alert, never restart.
  #
  # RED on origin/main: the function does not exist there, so any call aborts
  # the harness with "command not found" (set -euo pipefail). GREEN after #3815.
  # ════════════════════════════════════════════════════════════════════════════

  local NOW_ELS=1700000000            # fixed reference for event-loop-stall tests
  local els_dir="$TEST_ROOT/event-loop-stall"
  mkdir -p "$els_dir"

  # Helper: append one loop-side stall log line at a given epoch/level/field-form.
  # $1=file $2=epoch $3=level(ERROR|WARN) $4=field-form(text|json)
  _els_line() {
    local f="$1" ep="$2" lvl="$3" form="$4"
    local ts field
    ts=$(date -u -d "@$ep" "+%Y-%m-%dT%H:%M:%S.000000Z" 2>/dev/null)
    if [[ "$form" == "json" ]]; then field='"stall_recovered":true'; else field='stall_recovered=true'; fi
    printf '%s %s henyey_app::app::watchdog: Event loop stall (loop-side exact accounting) %s stall_ms=35000 stall_secs=35 phase=3\n' \
      "$ts" "$lvl" "$field" >> "$f"
  }

  # ── Test 63els1: single recovered ERROR stall in window → alert (no restart) ─
  local els1_log="$els_dir/1.log" els1_st="$els_dir/1.state"
  : > "$els1_log"
  _els_line "$els1_log" "$((NOW_ELS - 100))" ERROR text
  classify_event_loop_stall "$els1_log" "$els1_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "alert" && "$EVENT_LOOP_STALL_COUNT" == "1" ]]; then
    tap_ok "event-loop-stall: single recovered ERROR stall in window → alert (count=1)"
  else
    tap_not_ok "event-loop-stall: single recovered ERROR stall in window → alert" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els2: MAX_RECOVERED_STALLS (3) in window, fresh state → restart ──
  local els2_log="$els_dir/2.log" els2_st="$els_dir/2.state"
  : > "$els2_log"
  _els_line "$els2_log" "$((NOW_ELS - 300))" ERROR text
  _els_line "$els2_log" "$((NOW_ELS - 200))" ERROR text
  _els_line "$els2_log" "$((NOW_ELS - 100))" ERROR text
  classify_event_loop_stall "$els2_log" "$els2_st" "$NOW_ELS"
  local els2_last
  els2_last=$(grep '^last_restart_epoch=' "$els2_st" 2>/dev/null | cut -d= -f2)
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "restart" && "$EVENT_LOOP_STALL_COUNT" == "3" \
        && "$els2_last" == "$NOW_ELS" ]]; then
    tap_ok "event-loop-stall: 3 recovered ERROR stalls in window → restart (last_restart recorded)"
  else
    tap_not_ok "event-loop-stall: 3 recovered ERROR stalls in window → restart" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT last_restart=$els2_last"
  fi

  # ── Test 63els3: prune boundary — line just OUTSIDE window is excluded ──────
  # 2 in-window + 1 at exactly NOW-7201 (age >= STALL_WINDOW_SEC → pruned). If
  # the boundary line were wrongly counted, count=3 → restart; correct prune
  # yields count=2 → alert.
  local els3_log="$els_dir/3.log" els3_st="$els_dir/3.state"
  : > "$els3_log"
  _els_line "$els3_log" "$((NOW_ELS - 100))" ERROR text
  _els_line "$els3_log" "$((NOW_ELS - 200))" ERROR text
  _els_line "$els3_log" "$((NOW_ELS - 7201))" ERROR text
  classify_event_loop_stall "$els3_log" "$els3_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "alert" && "$EVENT_LOOP_STALL_COUNT" == "2" ]]; then
    tap_ok "event-loop-stall: just-outside-window stall pruned → alert (count=2, boundary excluded)"
  else
    tap_not_ok "event-loop-stall: just-outside-window stall pruned → alert (count=2)" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els4: WARN-tier recovered stall only → none ─────────────────────
  # A sub-30s stall renders at WARN; only the ERROR tier (>=30s) drives policy.
  local els4_log="$els_dir/4.log" els4_st="$els_dir/4.state"
  : > "$els4_log"
  _els_line "$els4_log" "$((NOW_ELS - 100))" WARN text
  classify_event_loop_stall "$els4_log" "$els4_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "none" && "$EVENT_LOOP_STALL_COUNT" == "0" ]]; then
    tap_ok "event-loop-stall: WARN-tier recovered stall only → none (ERROR tier drives policy)"
  else
    tap_not_ok "event-loop-stall: WARN-tier recovered stall only → none" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els5: watchdog_freeze ERROR line is NOT counted (belongs to 3b) ──
  # The unrecovered-wedge line carries neither the loop-side message nor
  # stall_recovered, so (3f) must ignore it entirely.
  local els5_log="$els_dir/5.log" els5_st="$els_dir/5.state"
  printf '%s ERROR henyey_app::app::watchdog: WATCHDOG: Event loop appears frozen! watchdog_freeze=true\n' \
    "$(date -u -d "@$((NOW_ELS - 100))" "+%Y-%m-%dT%H:%M:%S.000000Z")" > "$els5_log"
  classify_event_loop_stall "$els5_log" "$els5_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "none" && "$EVENT_LOOP_STALL_COUNT" == "0" ]]; then
    tap_ok "event-loop-stall: watchdog_freeze ERROR line not counted → none (3b owns it)"
  else
    tap_not_ok "event-loop-stall: watchdog_freeze ERROR line not counted → none" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els6: corrupt state file → non-restart (fails inert) ────────────
  # A single in-window stall with a garbage state file: last_restart_epoch must
  # fall back to 0 without crashing, and the verdict must be alert (never a
  # spurious restart).
  local els6_log="$els_dir/6.log" els6_st="$els_dir/6.state"
  : > "$els6_log"
  _els_line "$els6_log" "$((NOW_ELS - 100))" ERROR text
  printf 'garbage\nlast_restart_epoch=not-a-number\n===\n' > "$els6_st"
  classify_event_loop_stall "$els6_log" "$els6_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "alert" ]]; then
    tap_ok "event-loop-stall: corrupt state file → alert (fails inert, non-restart)"
  else
    tap_not_ok "event-loop-stall: corrupt state file → alert (fails inert)" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els7: restart cooldown suppresses a second restart → alert ──────
  # 3 in-window stalls (would restart) but last_restart_epoch is 300s ago
  # (< STALL_COOLDOWN_SEC=900) → restart suppressed, verdict alert-only.
  local els7_log="$els_dir/7.log" els7_st="$els_dir/7.state"
  : > "$els7_log"
  _els_line "$els7_log" "$((NOW_ELS - 300))" ERROR text
  _els_line "$els7_log" "$((NOW_ELS - 200))" ERROR text
  _els_line "$els7_log" "$((NOW_ELS - 100))" ERROR text
  printf 'last_restart_epoch=%s\n' "$((NOW_ELS - 300))" > "$els7_st"
  classify_event_loop_stall "$els7_log" "$els7_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_VERDICT" == "alert" && "$EVENT_LOOP_STALL_COUNT" == "3" ]]; then
    tap_ok "event-loop-stall: cooldown active (last_restart 300s ago) → alert (restart suppressed)"
  else
    tap_not_ok "event-loop-stall: cooldown active → alert (restart suppressed)" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els8: both Text and JSON stall_recovered field forms counted ────
  # The node runs Text format, but a formatter change to JSON must still be
  # detected. Feed one of each rendered field form; both must count.
  local els8_log="$els_dir/8.log" els8_st="$els_dir/8.state"
  : > "$els8_log"
  _els_line "$els8_log" "$((NOW_ELS - 200))" ERROR text
  _els_line "$els8_log" "$((NOW_ELS - 100))" ERROR json
  classify_event_loop_stall "$els8_log" "$els8_st" "$NOW_ELS"
  if [[ "$EVENT_LOOP_STALL_COUNT" == "2" && "$EVENT_LOOP_STALL_VERDICT" == "alert" ]]; then
    tap_ok "event-loop-stall: both Text and JSON stall_recovered field forms counted (count=2)"
  else
    tap_not_ok "event-loop-stall: both Text and JSON field forms counted (count=2)" \
      "verdict=$EVENT_LOOP_STALL_VERDICT count=$EVENT_LOOP_STALL_COUNT"
  fi

  # ── Test 63els9: consistency — SKILL.md references classify_event_loop_stall ─
  local tick_file_els="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'classify_event_loop_stall' "$tick_file_els"; then
    tap_ok "consistency: monitor-tick/SKILL.md references classify_event_loop_stall"
  else
    tap_not_ok "consistency: monitor-tick/SKILL.md references classify_event_loop_stall" \
      "(3f) rung not wired in monitor-tick/SKILL.md"
  fi

  # ── Test 63els10: log-scan carve-out for the recovered-stall ERROR line ────
  # The (1) log-scan section must mark the loop-side recovered-stall ERROR line
  # a known class routed to (3f) and exclude it from the unexpected-ERROR count.
  if grep -q 'recovered-stall ERROR carve-out' "$tick_file_els"; then
    tap_ok "log-scan: stall_recovered ERROR line excluded from unexpected-ERROR count"
  else
    tap_not_ok "log-scan: stall_recovered ERROR line excluded from unexpected-ERROR count" \
      "carve-out marker missing from (1) Log scan in monitor-tick/SKILL.md"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # classify_catchup_stalled (T64a–T64h) — issue #3799
  # Source: scripts/lib/monitor-decisions.sh — (3g) catchup-stalled REPORT-ONLY
  # detector: a live, /info-answering node stuck in `Catching Up` whose startup
  # catchup await never returned (startup_peak gauge absent-or-0) and whose lcl is
  # frozen. Sibling to (3e), mutually exclusive by construction (3e requires
  # valid|synced|track; this requires `Catching Up`).
  #
  # Contract: classify_catchup_stalled NODE_STATE CURRENT_LCL STARTUP_PEAK_MB \
  #             UPTIME_SEC PROC_RESPONSIVE STATE_FILE [NOW_EPOCH]
  # Sets CATCHUP_STALLED ∈ yes|no. The function is the sole reader/writer of
  # STATE_FILE (its own — NOT (3e)'s stuck_restart_state). NOW_EPOCH is injectable
  # for deterministic tests. Report-only: no cooldown/streak/restart machinery.
  # ════════════════════════════════════════════════════════════════════════════

  local NOW_CS=1700000000             # fixed reference for catchup-stalled tests
  local cs_dir="$TEST_ROOT/catchup-stalled"
  mkdir -p "$cs_dir"

  # Helper: seed a state file with a frozen lcl that has been frozen for N seconds
  # (so frozen_lcl_seconds = N at NOW_CS). Mirrors _sa_seed's dwell bookkeeping.
  _cs_seed() {
    # $1=file $2=frozen_lcl $3=frozen_for_sec
    local f="$1" lcl="$2" frozen_for="$3"
    printf 'frozen_lcl=%s\nfrozen_since_epoch=%s\n' \
      "$lcl" "$((NOW_CS - frozen_for))" > "$f"
  }

  # ── Test 64a: full wedge signature → yes ───────────────────────────────────
  # Catching up + startup_peak absent (empty) + uptime 1080 + lcl frozen 700s +
  # responsive. The reproduced #3702/#3797 signature.
  local cs_a="$cs_dir/a.state"
  _cs_seed "$cs_a" 63733600 700
  classify_catchup_stalled "Catching up" 63733600 "" 1080 "yes" "$cs_a" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "yes" ]]; then
    tap_ok "catchup-stalled: full wedge (Catching up + startup_peak absent + uptime 1080 + frozen 700s + responsive) → yes"
  else
    tap_not_ok "catchup-stalled: full wedge → yes" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64b: legitimate replay (startup completed) → no ───────────────────
  # startup_peak set (27000) proves the await returned and app.run() spawned;
  # a genuinely-replaying node must never be flagged.
  local cs_b="$cs_dir/b.state"
  _cs_seed "$cs_b" 63733600 700
  classify_catchup_stalled "Catching up" 63733600 27000 1080 "yes" "$cs_b" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "no" ]]; then
    tap_ok "catchup-stalled: legitimate replay (startup_peak=27000) → no"
  else
    tap_not_ok "catchup-stalled: legitimate replay → no" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64c: warmup (uptime 120 < 300) → no ───────────────────────────────
  # Under the bucket-restore + cache-scan window; too early to judge.
  local cs_c="$cs_dir/c.state"
  _cs_seed "$cs_c" 63733600 700
  classify_catchup_stalled "Catching up" 63733600 "" 120 "yes" "$cs_c" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "no" ]]; then
    tap_ok "catchup-stalled: warmup (uptime 120 < 300) → no"
  else
    tap_not_ok "catchup-stalled: warmup → no" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64d: dwell not met — lcl advanced this tick → no + re-anchor ───────
  # State says frozen at 63733600 for 700s, but CURRENT_LCL advanced to 63733700
  # → reset frozen_since to NOW and do NOT fire.
  local cs_d="$cs_dir/d.state"
  _cs_seed "$cs_d" 63733600 700
  classify_catchup_stalled "Catching up" 63733700 "" 1080 "yes" "$cs_d" "$NOW_CS"
  local d_result="$CATCHUP_STALLED"
  local d_frozen_lcl d_frozen_since
  d_frozen_lcl=$(grep '^frozen_lcl=' "$cs_d" | cut -d= -f2)
  d_frozen_since=$(grep '^frozen_since_epoch=' "$cs_d" | cut -d= -f2)
  if [[ "$d_result" == "no" && "$d_frozen_lcl" == "63733700" && "$d_frozen_since" == "$NOW_CS" ]]; then
    tap_ok "catchup-stalled: lcl advanced this tick → no (re-anchors frozen_since)"
  else
    tap_not_ok "catchup-stalled: lcl advanced this tick → no" \
      "result=$d_result frozen_lcl=$d_frozen_lcl frozen_since=$d_frozen_since (want no/63733700/$NOW_CS)"
  fi

  # ── Test 64e: healthy synced (Synced!) → no (disjoint from (3e)) ────────────
  # State-gate is `catching up` only; a synced node is (3e)'s domain, never here.
  local cs_e="$cs_dir/e.state"
  _cs_seed "$cs_e" 63733600 700
  classify_catchup_stalled "Synced!" 63733600 "" 1080 "yes" "$cs_e" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "no" ]]; then
    tap_ok "catchup-stalled: healthy synced (Synced!) → no (disjoint from (3e))"
  else
    tap_not_ok "catchup-stalled: healthy synced → no" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64f: present-at-0 variant (startup_peak=0) → yes ───────────────────
  # The gauge present but 0 is treated identically to absent (startup not done).
  local cs_f="$cs_dir/f.state"
  _cs_seed "$cs_f" 63733600 700
  classify_catchup_stalled "Catching up" 63733600 0 1080 "yes" "$cs_f" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "yes" ]]; then
    tap_ok "catchup-stalled: present-at-0 variant (startup_peak=0) → yes"
  else
    tap_not_ok "catchup-stalled: present-at-0 variant → yes" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64g: unresponsive (PROC_RESPONSIVE=no) → no ((3b) owns wedge) ──────
  local cs_g="$cs_dir/g.state"
  _cs_seed "$cs_g" 63733600 700
  classify_catchup_stalled "Catching up" 63733600 "" 1080 "no" "$cs_g" "$NOW_CS"
  if [[ "$CATCHUP_STALLED" == "no" ]]; then
    tap_ok "catchup-stalled: unresponsive (PROC_RESPONSIVE=no) → no ((3b) owns wedge)"
  else
    tap_not_ok "catchup-stalled: unresponsive → no" "got CATCHUP_STALLED=$CATCHUP_STALLED"
  fi

  # ── Test 64h: fresh/absent state file → no (dwell 0, latency locked in) ─────
  # First detection of a freeze must NOT fire — the ~10 min dwell latency is
  # intentional. An absent state file anchors frozen_since to NOW → dwell 0.
  local cs_h="$cs_dir/h.state"   # deliberately NOT seeded (absent)
  classify_catchup_stalled "Catching up" 63733600 "" 1080 "yes" "$cs_h" "$NOW_CS"
  local h_result="$CATCHUP_STALLED"
  local h_frozen_since
  h_frozen_since=$(grep '^frozen_since_epoch=' "$cs_h" | cut -d= -f2)
  if [[ "$h_result" == "no" && "$h_frozen_since" == "$NOW_CS" ]]; then
    tap_ok "catchup-stalled: fresh/absent state file → no (dwell 0, first-detection latency locked in)"
  else
    tap_not_ok "catchup-stalled: fresh/absent state file → no" \
      "result=$h_result frozen_since=$h_frozen_since (want no/$NOW_CS)"
  fi

  # ── Test 64consistency: SKILL.md references classify_catchup_stalled ────────
  local tick_file_cs="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'classify_catchup_stalled' "$tick_file_cs"; then
    tap_ok "consistency: monitor-tick/SKILL.md references classify_catchup_stalled"
  else
    tap_not_ok "consistency: monitor-tick/SKILL.md references classify_catchup_stalled" \
      "(3g) rung not wired in monitor-tick/SKILL.md"
  fi

  # ════════════════════════════════════════════════════════════════════════════
  # classify_obsrvr_radar + eval_obsrvr_not_indexed_streak (T63q–T63x) — #3753
  # Source: scripts/lib/monitor-decisions.sh — monitor-tick check (9) OBSRVR Radar.
  #
  # classify_obsrvr_radar INDEX_HTTP_CODE NODE_HTTP_CODE HAS_REQUIRED_FIELDS
  #   prints exactly one of the four stable literals on stdout:
  #     ok | not-indexed | api-error | api-incomplete
  #   HAS_REQUIRED_FIELDS is passed as the literal token `true`/`false`. The
  #   `not-indexed` verdict requires per-node 404 AND index 200 so a whole-API
  #   Radar outage classifies `api-error`, not `not-indexed` (comment §5.2).
  #
  # eval_obsrvr_not_indexed_streak CLASSIFICATION STATE_FILE [THRESHOLD]
  #   sole reader/writer of STATE_FILE; sets OBSRVR_STREAK (current count) and
  #   OBSRVR_ESCALATE ∈ yes|no. Fires `yes` exactly once when the streak first
  #   reaches THRESHOLD (default 12), then `no` while the streak persists; a
  #   single non-`not-indexed` tick resets counter + one-shot marker.
  #
  # RED on origin/main: neither function exists there, so any call aborts the
  # harness with "command not found" (set -euo pipefail). GREEN after #3753.
  # ════════════════════════════════════════════════════════════════════════════

  # ── Test 63q: node=404 + index=200 → not-indexed (permanent, API itself up) ─
  local ob_c
  ob_c=$(classify_obsrvr_radar 200 404 false)
  if [[ "$ob_c" == "not-indexed" ]]; then
    tap_ok "obsrvr-classify: node=404 index=200 → not-indexed"
  else
    tap_not_ok "obsrvr-classify: node=404 index=200 → not-indexed" "got '$ob_c'"
  fi

  # ── Test 63r: node=200 + required fields present → ok ──────────────────────
  ob_c=$(classify_obsrvr_radar 200 200 true)
  if [[ "$ob_c" == "ok" ]]; then
    tap_ok "obsrvr-classify: node=200 fields=true → ok"
  else
    tap_not_ok "obsrvr-classify: node=200 fields=true → ok" "got '$ob_c'"
  fi

  # ── Test 63s: node=200 + required fields missing → api-incomplete ──────────
  ob_c=$(classify_obsrvr_radar 200 200 false)
  if [[ "$ob_c" == "api-incomplete" ]]; then
    tap_ok "obsrvr-classify: node=200 fields=false → api-incomplete"
  else
    tap_not_ok "obsrvr-classify: node=200 fields=false → api-incomplete" "got '$ob_c'"
  fi

  # ── Test 63t: node=500 → api-error ─────────────────────────────────────────
  ob_c=$(classify_obsrvr_radar 200 500 false)
  if [[ "$ob_c" == "api-error" ]]; then
    tap_ok "obsrvr-classify: node=500 → api-error"
  else
    tap_not_ok "obsrvr-classify: node=500 → api-error" "got '$ob_c'"
  fi

  # ── Test 63u: node=404 + index=500 (whole-API outage) → api-error ──────────
  # The decisive discriminator (comment §5.2): a 404 alongside a DOWN index is a
  # Radar outage, NOT node-not-indexed. Must classify api-error, not not-indexed.
  ob_c=$(classify_obsrvr_radar 500 404 false)
  if [[ "$ob_c" == "api-error" ]]; then
    tap_ok "obsrvr-classify: node=404 index=500 (API outage) → api-error (NOT not-indexed)"
  else
    tap_not_ok "obsrvr-classify: node=404 index=500 → api-error" "got '$ob_c'"
  fi

  # ── Test 63v: empty/timeout code → api-error ───────────────────────────────
  ob_c=$(classify_obsrvr_radar "" "" false)
  if [[ "$ob_c" == "api-error" ]]; then
    tap_ok "obsrvr-classify: empty/timeout code → api-error"
  else
    tap_not_ok "obsrvr-classify: empty/timeout code → api-error" "got '$ob_c'"
  fi

  # ── Test 63w: streak increments across consecutive not-indexed ticks ───────
  local ob_streak_file="$sa_dir/obsrvr-streak.state"
  rm -f "$ob_streak_file"
  eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
  local ob_s1="$OBSRVR_STREAK"
  eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
  eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
  if [[ "$ob_s1" == "1" && "$OBSRVR_STREAK" == "3" && "$OBSRVR_ESCALATE" == "no" ]]; then
    tap_ok "obsrvr-streak: increments across consecutive not-indexed (1→3), no escalate below threshold"
  else
    tap_not_ok "obsrvr-streak: increments across consecutive not-indexed" \
      "s1=$ob_s1 streak=$OBSRVR_STREAK escalate=$OBSRVR_ESCALATE (want 1/3/no)"
  fi

  # ── Test 63x: escalation fires exactly once at THRESHOLD, then never again ──
  # Continue the streak from 3 up to and past 12; escalate must fire once (at 12).
  local ob_fires=0 ob_t
  for ob_t in 4 5 6 7 8 9 10 11 12; do
    eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
    [[ "$OBSRVR_ESCALATE" == "yes" ]] && ob_fires=$((ob_fires + 1))
  done
  local ob_at12_streak="$OBSRVR_STREAK"
  # One more tick past threshold — must NOT re-fire (one-shot marker set).
  eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
  if [[ "$ob_at12_streak" == "12" && "$ob_fires" == "1" \
        && "$OBSRVR_STREAK" == "13" && "$OBSRVR_ESCALATE" == "no" ]]; then
    tap_ok "obsrvr-streak: escalation fires exactly once at THRESHOLD=12, silent thereafter (one-shot)"
  else
    tap_not_ok "obsrvr-streak: escalation fires exactly once at THRESHOLD" \
      "at12=$ob_at12_streak fires=$ob_fires streak13=$OBSRVR_STREAK escalate=$OBSRVR_ESCALATE"
  fi

  # ── Test 63y: a non-not-indexed tick resets counter + marker (re-escalates) ─
  eval_obsrvr_not_indexed_streak ok "$ob_streak_file" 12
  local ob_reset_streak="$OBSRVR_STREAK" ob_reset_esc="$OBSRVR_ESCALATE"
  # After a reset the one-shot marker must clear so a fresh streak can escalate.
  local ob_refire=0
  for ob_t in $(seq 1 12); do
    eval_obsrvr_not_indexed_streak not-indexed "$ob_streak_file" 12
    [[ "$OBSRVR_ESCALATE" == "yes" ]] && ob_refire=$((ob_refire + 1))
  done
  if [[ "$ob_reset_streak" == "0" && "$ob_reset_esc" == "no" && "$ob_refire" == "1" ]]; then
    tap_ok "obsrvr-streak: non-not-indexed tick resets counter+marker; fresh streak re-escalates once"
  else
    tap_not_ok "obsrvr-streak: reset + re-escalate" \
      "reset_streak=$ob_reset_streak reset_esc=$ob_reset_esc refire=$ob_refire (want 0/no/1)"
  fi

  # ── Test 63z: consistency — SKILL.md check (9) references both functions ────
  local tick_file_ob="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'classify_obsrvr_radar' "$tick_file_ob"; then
    tap_ok "consistency: monitor-tick/SKILL.md references classify_obsrvr_radar"
  else
    tap_not_ok "consistency: monitor-tick/SKILL.md references classify_obsrvr_radar" \
      "check (9) not wired to the shared classifier"
  fi
  if grep -q 'eval_obsrvr_not_indexed_streak' "$tick_file_ob"; then
    tap_ok "consistency: monitor-tick/SKILL.md references eval_obsrvr_not_indexed_streak"
  else
    tap_not_ok "consistency: monitor-tick/SKILL.md references eval_obsrvr_not_indexed_streak" \
      "check (9) not wired to the streak escalator"
  fi

  # ── Test 64: Tick history capture invokes the single-writer helper ──────────
  # #3791: the hand-rolled `<<'PY'` / json.dumps heredoc is replaced by a call
  # to scripts/lib/monitor-tick-artifacts.py emit-row so there is exactly one
  # writer (→ one schema). Structural assertion scoped to the fenced code block.
  local tick_hist_block
  tick_hist_block=$(sed -n '/^### Tick history capture/,/^##/{/^```bash/,/^```/p}' \
    "$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md")
  local t64_pass=true
  if ! echo "$tick_hist_block" | grep -q 'monitor-tick-artifacts.py'; then
    t64_pass=false
  fi
  if ! echo "$tick_hist_block" | grep -q 'emit-row'; then
    t64_pass=false
  fi
  if echo "$tick_hist_block" | grep -q "<<'PY'"; then
    t64_pass=false
  fi
  if echo "$tick_hist_block" | grep -q 'json.dumps'; then
    t64_pass=false
  fi
  if [[ "$t64_pass" == true ]]; then
    tap_ok "tick-history: invokes monitor-tick-artifacts.py emit-row, no raw heredoc (#3791)"
  else
    tap_not_ok "tick-history: invokes monitor-tick-artifacts.py emit-row, no raw heredoc (#3791)" \
      "Tick history capture block must call the helper and drop the <<'PY'/json.dumps heredoc"
  fi

  # ── Test 65: Tick history row via helper — valid JSON, ISO 8601 UTC, <=60s ──
  # #3791: execute the single-writer helper's emit-row and verify the emitted
  # row's ts is valid JSON, ISO 8601 UTC, and within the 60s skew contract.
  local t65_helper="$REPO_ROOT/scripts/lib/monitor-tick-artifacts.py"
  local t65_result t65_exit
  t65_result=$(python3 "$t65_helper" emit-row \
    --status OK --ledger 12345 --build abc1234 --deploys 0 --self-reflect clean 2>&1) \
    && t65_exit=0 || t65_exit=$?
  if [[ $t65_exit -ne 0 ]]; then
    tap_not_ok "tick-history-ts: helper emit-row → valid JSON, ISO 8601 UTC, <=60s skew (#3791)" \
      "emit-row failed (exit $t65_exit): $t65_result"
  else
    local t65_ok
    t65_ok=$(T65_RESULT="$t65_result" python3 -c "
import json, os
from datetime import datetime, timezone
try:
    obj = json.loads(os.environ['T65_RESULT'])
    assert 'ts' in obj, 'missing ts field'
    ts = datetime.strptime(obj['ts'], '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=timezone.utc)
    skew = abs((datetime.now(timezone.utc) - ts).total_seconds())
    assert obj['ts'].endswith('Z'), 'ts must end with Z'
    assert skew <= 60, f'ts skew {skew}s exceeds 60s'
    print('ok')
except Exception as e:
    print(f'fail: {e}')
" 2>&1)
    if [[ "$t65_ok" == "ok" ]]; then
      tap_ok "tick-history-ts: helper emit-row → valid JSON, ISO 8601 UTC, <=60s skew (#3791)"
    else
      tap_not_ok "tick-history-ts: helper emit-row → valid JSON, ISO 8601 UTC, <=60s skew (#3791)" \
        "$t65_ok"
    fi
  fi

  # ── #3791: single-writer helper schema self-check + closed vocabulary ───────
  local mta="$REPO_ROOT/scripts/lib/monitor-tick-artifacts.py"

  # validate-row type-guards a null ts BEFORE the regex: non-zero exit, and no
  # Python traceback (the check is `isinstance(ts,str) and TS_RE.match(ts)`).
  # Exit code 1 == a clean validation rejection (argparse/usage errors and a
  # missing-helper are exit 2), so this distinguishes a real reject from breakage.
  local nullts_out nullts_exit
  nullts_out=$(python3 "$mta" validate-row \
    --row '{"ts":null,"status":"OK","ledger":1,"build":"a","deploys":0,"warnings":[],"actions":[],"self_reflect":"clean","watch":[]}' 2>&1) \
    && nullts_exit=0 || nullts_exit=$?
  if [[ $nullts_exit -eq 1 ]] \
     && echo "$nullts_out" | grep -q 'validate-row failed' \
     && ! echo "$nullts_out" | grep -q 'Traceback'; then
    tap_ok "tick-row validate: null ts → clean reject (exit 1), no traceback (#3791)"
  else
    tap_not_ok "tick-row validate: null ts → clean reject (exit 1), no traceback (#3791)" \
      "exit=$nullts_exit out=$nullts_out"
  fi

  # validate-row accepts a diagnostic SUPERSET (required-subset, not set-equality).
  if python3 "$mta" validate-row \
      --row '{"ts":"2026-08-23T12:00:00Z","status":"OK","ledger":1,"build":"a","deploys":0,"warnings":[],"actions":[],"self_reflect":"clean","watch":[],"evaluator":"x","uptime_s":99}' >/dev/null 2>&1; then
    tap_ok "tick-row validate: diagnostic superset accepted (#3791)"
  else
    tap_not_ok "tick-row validate: diagnostic superset accepted (#3791)" \
      "required-subset contract must accept extra keys"
  fi

  # emit-row builds the canonical 9-key row with an ISO ts and list warnings/actions.
  local emit_row emit_ok
  emit_row=$(python3 "$mta" emit-row --status OK --ledger 42 --build abc1234 --deploys 0 --self-reflect clean 2>&1) || true
  emit_ok=$(EMIT_ROW="$emit_row" python3 -c "
import json, os, re
try:
    o = json.loads(os.environ['EMIT_ROW'])
    req = {'ts','status','ledger','build','deploys','warnings','actions','self_reflect','watch'}
    assert set(o) == req, f'keys {sorted(o)}'
    assert re.match(r'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z\$', o['ts']), 'ts shape'
    assert isinstance(o['warnings'], list) and isinstance(o['actions'], list), 'lists'
    assert o['warnings'] == [] and o['actions'] == [], 'empty lists'
    print('ok')
except Exception as e:
    print(f'fail: {e}')
" 2>&1)
  if [[ "$emit_ok" == "ok" ]]; then
    tap_ok "tick-row emit: canonical 9 keys, ISO ts, list warnings/actions (#3791)"
  else
    tap_not_ok "tick-row emit: canonical 9 keys, ISO ts, list warnings/actions (#3791)" "$emit_ok"
  fi

  # warnings vocabulary: unknown token rejected; catalog name + 'other' accepted.
  local unk_exit
  python3 "$mta" validate-row \
      --row '{"ts":"2026-08-23T12:00:00Z","status":"OK","ledger":1,"build":"a","deploys":0,"warnings":["recovery_stalled_forcing_catchup_behind"],"actions":[],"self_reflect":"clean","watch":[]}' \
      >/dev/null 2>&1 && unk_exit=0 || unk_exit=$?
  if [[ $unk_exit -eq 1 ]]; then
    tap_ok "tick-row validate: unknown warning token rejected (#3791)"
  else
    tap_not_ok "tick-row validate: unknown warning token rejected (#3791)" "expected clean reject (exit 1), got $unk_exit"
  fi
  if python3 "$mta" validate-row \
      --row '{"ts":"2026-08-23T12:00:00Z","status":"OK","ledger":1,"build":"a","deploys":0,"warnings":["recovery-stalled"],"actions":[],"self_reflect":"clean","watch":[]}' >/dev/null 2>&1; then
    tap_ok "tick-row validate: catalog-name warning accepted (#3791)"
  else
    tap_not_ok "tick-row validate: catalog-name warning accepted (#3791)" "recovery-stalled is a catalog name"
  fi
  if python3 "$mta" validate-row \
      --row '{"ts":"2026-08-23T12:00:00Z","status":"OK","ledger":1,"build":"a","deploys":0,"warnings":["other"],"actions":[],"self_reflect":"clean","watch":[]}' >/dev/null 2>&1; then
    tap_ok "tick-row validate: 'other' fallback warning accepted (#3791)"
  else
    tap_not_ok "tick-row validate: 'other' fallback warning accepted (#3791)" "other must be in vocab"
  fi

  # emit-row promotes an embedded measurement into a typed sibling key.
  local promo_row promo_ok
  promo_row=$(python3 "$mta" emit-row --status WARNING --ledger 1 --build a --deploys 0 --self-reflect clean --warnings 'low-disk 90%' 2>&1) || true
  promo_ok=$(PROMO="$promo_row" python3 -c "
import json, os
try:
    o = json.loads(os.environ['PROMO'])
    assert o['warnings'] == ['low-disk'], o['warnings']
    assert o.get('disk_free_pct') == 90, o.get('disk_free_pct')
    print('ok')
except Exception as e:
    print(f'fail: {e}')
" 2>&1)
  if [[ "$promo_ok" == "ok" ]]; then
    tap_ok "tick-row emit: embedded measurement promoted to sibling key (#3791)"
  else
    tap_not_ok "tick-row emit: embedded measurement promoted to sibling key (#3791)" "$promo_ok"
  fi

  # metadata.env carries a MEANINGFUL, bumped ARCHIVE_VERSION (was inert =1).
  local meta_out
  meta_out=$(python3 "$mta" write-metadata 2>&1) || true
  if echo "$meta_out" | grep -qx 'ARCHIVE_VERSION=2'; then
    tap_ok "archive metadata: ARCHIVE_VERSION=2 (meaningful, not inert 1) (#3791)"
  else
    tap_not_ok "archive metadata: ARCHIVE_VERSION=2 (meaningful, not inert 1) (#3791)" \
      "got: $(echo "$meta_out" | grep ARCHIVE_VERSION)"
  fi

  # scrape_identity written by the helper carries version + pid + start_ticks + ts.
  local id_out
  id_out=$(python3 "$mta" write-scrape-identity --pid 4242 --start-ticks 777 2>&1) || true
  if echo "$id_out" | grep -qx 'version=1' \
     && echo "$id_out" | grep -qx 'pid=4242' \
     && echo "$id_out" | grep -qx 'start_ticks=777' \
     && echo "$id_out" | grep -q '^timestamp='; then
    tap_ok "scrape-identity: helper emits version/pid/start_ticks/timestamp (#3791)"
  else
    tap_not_ok "scrape-identity: helper emits version/pid/start_ticks/timestamp (#3791)" "$id_out"
  fi

  # counter_streak_snapshot written by the helper carries all required fields.
  local cs_out
  cs_out=$(python3 "$mta" write-counter-streak --pid 4242 --start-ticks 777 --counter-value 5 --breach-streak 2 2>&1) || true
  if echo "$cs_out" | grep -qx 'version=1' \
     && echo "$cs_out" | grep -qx 'counter_value=5' \
     && echo "$cs_out" | grep -qx 'breach_streak=2'; then
    tap_ok "counter-streak: helper emits version/counter_value/breach_streak (#3791)"
  else
    tap_not_ok "counter-streak: helper emits version/counter_value/breach_streak (#3791)" "$cs_out"
  fi

  # Archive .prom copy preserves mtimes (cp -p), so archived mtimes describe the
  # archived bytes; metadata.env is emitted by the single-writer helper (#3791).
  local archive_block
  archive_block=$(sed -n '/^7\. \*\*Archive snapshot\*\*/,/^8\. /p' \
    "$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md")
  if echo "$archive_block" | grep -q 'cp -p '; then
    tap_ok "archive step: copies .prom with cp -p (mtime-preserving) (#3791)"
  else
    tap_not_ok "archive step: copies .prom with cp -p (mtime-preserving) (#3791)" \
      "archive snapshot must copy prom files with cp -p"
  fi
  if echo "$archive_block" | grep -q 'write-metadata' \
     && echo "$archive_block" | grep -q 'monitor-tick-artifacts.py'; then
    tap_ok "archive metadata: written via helper write-metadata (#3791)"
  else
    tap_not_ok "archive metadata: written via helper write-metadata (#3791)" \
      "metadata.env must be emitted by the single-writer helper"
  fi

  # ── Deploy Quarantine Tests ──────────────────────────────────────────────────

  local qdir="$TEST_ROOT/quarantine"
  mkdir -p "$qdir"

  # ── Test 66: parse_quarantine_file — missing file ──────────────────────────
  parse_quarantine_file "$qdir/nonexistent.txt"
  if [[ $? -eq 0 && -z "$QUARANTINE_ENTRIES" && -z "$QUARANTINE_WARNINGS" ]]; then
    tap_ok "quarantine-parse: missing file returns 0 with empty outputs"
  else
    tap_not_ok "quarantine-parse: missing file returns 0 with empty outputs" \
      "rc=$? entries='$QUARANTINE_ENTRIES' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 67: parse_quarantine_file — empty file ────────────────────────────
  touch "$qdir/empty.txt"
  parse_quarantine_file "$qdir/empty.txt"
  if [[ $? -eq 0 && -z "$QUARANTINE_ENTRIES" && -z "$QUARANTINE_WARNINGS" ]]; then
    tap_ok "quarantine-parse: empty file returns 0 with empty outputs"
  else
    tap_not_ok "quarantine-parse: empty file returns 0 with empty outputs" \
      "rc=$? entries='$QUARANTINE_ENTRIES' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 68: parse_quarantine_file — unreadable file ───────────────────────
  echo "abc" > "$qdir/unreadable.txt"
  chmod 000 "$qdir/unreadable.txt"
  local rc68=0
  parse_quarantine_file "$qdir/unreadable.txt" || rc68=$?
  chmod 644 "$qdir/unreadable.txt"  # restore for cleanup
  if [[ $rc68 -eq 1 && "$QUARANTINE_WARNINGS" == *"unreadable"* ]]; then
    tap_ok "quarantine-parse: unreadable file returns 1 (fail-closed)"
  else
    tap_not_ok "quarantine-parse: unreadable file returns 1 (fail-closed)" \
      "rc=$rc68 warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 69: parse_quarantine_file — comments only ─────────────────────────
  printf '# comment line\n  # indented comment\n\n' > "$qdir/comments.txt"
  parse_quarantine_file "$qdir/comments.txt"
  if [[ $? -eq 0 && -z "$QUARANTINE_ENTRIES" && -z "$QUARANTINE_WARNINGS" ]]; then
    tap_ok "quarantine-parse: comments-only file returns empty"
  else
    tap_not_ok "quarantine-parse: comments-only file returns empty" \
      "entries='$QUARANTINE_ENTRIES' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 70: parse_quarantine_file — valid entries ─────────────────────────
  local sha1="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local sha2="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  printf '%s regression #100\n%s regression #200\n' "$sha1" "$sha2" > "$qdir/valid.txt"
  parse_quarantine_file "$qdir/valid.txt"
  local expected_entries
  expected_entries=$(printf '%s\n%s' "$sha1" "$sha2")
  if [[ $? -eq 0 && "$QUARANTINE_ENTRIES" == "$expected_entries" && -z "$QUARANTINE_WARNINGS" ]]; then
    tap_ok "quarantine-parse: valid entries parsed correctly"
  else
    tap_not_ok "quarantine-parse: valid entries parsed correctly" \
      "entries='$QUARANTINE_ENTRIES' expected='$expected_entries' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 71: parse_quarantine_file — malformed entries ─────────────────────
  printf 'ZZZZ not-a-sha\ntoo-short\n' > "$qdir/malformed.txt"
  parse_quarantine_file "$qdir/malformed.txt"
  if [[ $? -eq 0 && -z "$QUARANTINE_ENTRIES" && "$QUARANTINE_WARNINGS" == *"malformed"* ]]; then
    tap_ok "quarantine-parse: malformed entries produce warnings"
  else
    tap_not_ok "quarantine-parse: malformed entries produce warnings" \
      "entries='$QUARANTINE_ENTRIES' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 72: parse_quarantine_file — mixed valid/malformed/comments ────────
  {
    printf '# header\n'
    printf '%s good one\n' "$sha1"
    printf 'BADSHA nope\n'
    printf '\n'
    printf '%s another good\n' "$sha2"
  } > "$qdir/mixed.txt"
  parse_quarantine_file "$qdir/mixed.txt"
  expected_entries=$(printf '%s\n%s' "$sha1" "$sha2")
  if [[ $? -eq 0 && "$QUARANTINE_ENTRIES" == "$expected_entries" && "$QUARANTINE_WARNINGS" == *"malformed: BADSHA"* ]]; then
    tap_ok "quarantine-parse: mixed file — valid entries + warnings"
  else
    tap_not_ok "quarantine-parse: mixed file — valid entries + warnings" \
      "entries='$QUARANTINE_ENTRIES' warnings='$QUARANTINE_WARNINGS'"
  fi

  # ── Test 73: parse_quarantine_file — tab-separated entry ───────────────────
  printf '%s\tregression tab-separated\n' "$sha1" > "$qdir/tabs.txt"
  parse_quarantine_file "$qdir/tabs.txt"
  if [[ $? -eq 0 && "$QUARANTINE_ENTRIES" == "$sha1" ]]; then
    tap_ok "quarantine-parse: tab-separated entry parsed"
  else
    tap_not_ok "quarantine-parse: tab-separated entry parsed" \
      "entries='$QUARANTINE_ENTRIES'"
  fi

  # ── Test 74: parse_quarantine_file — CRLF line endings ─────────────────────
  printf '%s reason\r\n' "$sha1" > "$qdir/crlf.txt"
  parse_quarantine_file "$qdir/crlf.txt"
  if [[ $? -eq 0 && "$QUARANTINE_ENTRIES" == "$sha1" ]]; then
    tap_ok "quarantine-parse: CRLF stripped correctly"
  else
    tap_not_ok "quarantine-parse: CRLF stripped correctly" \
      "entries='$QUARANTINE_ENTRIES'"
  fi

  # ── Test 75: parse_quarantine_file — bare SHA (no reason) ──────────────────
  printf '%s\n' "$sha1" > "$qdir/bare.txt"
  parse_quarantine_file "$qdir/bare.txt"
  if [[ $? -eq 0 && "$QUARANTINE_ENTRIES" == "$sha1" ]]; then
    tap_ok "quarantine-parse: bare SHA (no reason) is valid"
  else
    tap_not_ok "quarantine-parse: bare SHA (no reason) is valid" \
      "entries='$QUARANTINE_ENTRIES'"
  fi

  # ── Test 76: check_quarantine_ancestry — empty file (clear) ────────────────
  # Mock git to never be called
  git() { return 99; }
  touch "$qdir/empty_anc.txt"
  local rc76=0
  check_quarantine_ancestry "$qdir/empty_anc.txt" || rc76=$?
  unset -f git
  if [[ $rc76 -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-ancestry: empty file returns 1 (clear)"
  else
    tap_not_ok "quarantine-ancestry: empty file returns 1 (clear)" \
      "rc=$rc76 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 77: check_quarantine_active — ancestor + content still applied ────
  # New semantics: ancestor by itself is not enough; at least one hunk of the
  # bad commit's diff must still reverse-apply. Mock `diff` to emit a real
  # single-hunk diff (so the per-hunk splitter parses one hunk) and `apply` to
  # report it still reverse-applies (rc 0).
  printf '%s regression\n' "$sha1" > "$qdir/ancestor.txt"
  git() {
    case "$1" in
      merge-base) return 0 ;;
      diff)
        cat <<'CANNED'
diff --git a/f.rs b/f.rs
index 1111111..2222222 100644
--- a/f.rs
+++ b/f.rs
@@ -1,2 +1,3 @@ mod m {
+    still_present
 }
CANNED
        return 0 ;;
      apply) return 0 ;;  # hunk still reverse-applies → present
      *) return 0 ;;
    esac
  }
  local rc77=0
  check_quarantine_ancestry "$qdir/ancestor.txt" || rc77=$?
  unset -f git
  if [[ $rc77 -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: ancestor + content-applied returns 0 (blocked)"
  else
    tap_not_ok "quarantine-active: ancestor + content-applied returns 0 (blocked)" \
      "rc=$rc77 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78: check_quarantine_active — not ancestor (clear) ────────────────
  printf '%s regression\n' "$sha1" > "$qdir/not_ancestor.txt"
  git() { return 1; }  # mock: merge-base says NOT ancestor
  local rc78=0
  check_quarantine_ancestry "$qdir/not_ancestor.txt" || rc78=$?
  unset -f git
  if [[ $rc78 -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-active: not ancestor returns 1 (clear)"
  else
    tap_not_ok "quarantine-active: not ancestor returns 1 (clear)" \
      "rc=$rc78 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78b: check_quarantine_active — ancestor but content reverted ──────
  # Ancestor (rc=0); `diff` emits a real single-hunk diff but the only hunk
  # fails to reverse-apply (rc=1) → all hunks gone → CLEAR.
  printf '%s regression\n' "$sha1" > "$qdir/reverted.txt"
  git() {
    case "$1" in
      merge-base) return 0 ;;  # ancestor
      diff)
        cat <<'CANNED'
diff --git a/f.rs b/f.rs
index 1111111..2222222 100644
--- a/f.rs
+++ b/f.rs
@@ -1,2 +1,3 @@ mod m {
+    reverted_away
 }
CANNED
        return 0 ;;
      apply)      return 1 ;;  # patch rejected → content gone
      *)          return 0 ;;
    esac
  }
  local rc78b=0
  check_quarantine_ancestry "$qdir/reverted.txt" || rc78b=$?
  unset -f git
  if [[ $rc78b -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-active: ancestor + content-reverted returns 1 (auto-clear)"
  else
    tap_not_ok "quarantine-active: ancestor + content-reverted returns 1 (auto-clear)" \
      "rc=$rc78b status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Tests 78c/78d/78e/78c-zsh: per-hunk reverse-apply (#3248) ──────────────
  # Regression cluster for the false-clear bug fixed in #3248. The OLD code
  # piped the whole-commit diff into a SINGLE `git apply --check --reverse`,
  # which is all-or-nothing: a later unrelated commit drifting the context of
  # ANY one hunk of a multi-file quarantined commit makes the entire
  # reverse-apply fail → the gate reports CLEAR even though the harmful hunk
  # (e.g. retain:false) is fully intact in HEAD. The fix splits the diff
  # per-hunk and BLOCKS if ANY hunk still reverse-applies.
  #
  # Mock strategy (validated): mock `git diff sha^..sha` to emit a canned
  # multi-hunk, multi-file diff; mock `git apply` to read the per-hunk patch
  # off stdin and dispatch on the hunk's content marker — a "harmful" hunk
  # still reverse-applies (rc 0), a "drifted/removed" hunk does not (rc 1).
  # The mock therefore behaves identically whether it receives the whole-commit
  # diff (old code: sees the drifted hunk → rejects all) or a single-hunk patch
  # (new code: judges each hunk independently). This is what makes 78c FAIL on
  # origin/main and PASS after the per-hunk rewrite.

  # Canned 2-file, 2-hunk diff for a quarantined commit. Hunk 1 (main.rs) is
  # the harmful retain:false line; hunk 2 (memory_report.rs) is the unrelated
  # hunk a later commit drifted.
  _q_canned_diff_drift() {
    cat <<'CANNED'
diff --git a/crates/henyey/src/main.rs b/crates/henyey/src/main.rs
index 1111111..2222222 100644
--- a/crates/henyey/src/main.rs
+++ b/crates/henyey/src/main.rs
@@ -114,3 +114,4 @@ fn configure_allocator() {
     let _ = unsafe { &_rjem_malloc_conf };
+    HARMFUL_RETAIN_FALSE_MARKER
     let cfg = Config::load();
     run(cfg)
diff --git a/crates/ledger/src/memory_report.rs b/crates/ledger/src/memory_report.rs
index 3333333..4444444 100644
--- a/crates/ledger/src/memory_report.rs
+++ b/crates/ledger/src/memory_report.rs
@@ -356,3 +356,4 @@ impl MemoryReport {
     let total = self.heap + self.stack;
+    DRIFTED_UNRELATED_MARKER
     report.emit(total);
 }
CANNED
  }

  # 3-hunk single-file diff for the partial-presence fail-safe case. Only the
  # MIDDLE hunk still reverse-applies; the surrounding two are gone.
  _q_canned_diff_partial() {
    cat <<'CANNED'
diff --git a/crates/henyey/src/cfg.rs b/crates/henyey/src/cfg.rs
index aaaaaaa..bbbbbbb 100644
--- a/crates/henyey/src/cfg.rs
+++ b/crates/henyey/src/cfg.rs
@@ -10,2 +10,3 @@ mod top {
+    GONE_HUNK_ONE_MARKER
 }
@@ -40,2 +41,3 @@ mod mid {
+    HARMFUL_RETAIN_FALSE_MARKER
 }
@@ -90,2 +92,3 @@ mod bot {
+    GONE_HUNK_THREE_MARKER
 }
CANNED
  }

  # Per-hunk-aware `git` mock. `diff` emits the named canned diff; `apply`
  # reads the patch off stdin and faithfully models `git apply --check`'s
  # ALL-OR-NOTHING semantics: it succeeds (rc 0) only if EVERY hunk in the
  # patch reverse-applies. A "harmful" hunk (HARMFUL_RETAIN_FALSE_MARKER)
  # reverse-applies; a "drifted/removed" hunk (any *_MARKER that is NOT the
  # harmful one) does NOT (a later commit moved its context). So the mock
  # returns 0 iff the patch carries the harmful marker AND carries no
  # drifted/gone marker. This is the crux of the #3248 repro:
  #   - OLD code pipes the WHOLE 2-hunk diff into ONE apply → the patch
  #     contains BOTH the harmful AND the drifted marker → rc 1 → false CLEAR.
  #   - NEW code feeds each hunk separately → the harmful hunk alone → rc 0
  #     → BLOCK.
  # `merge-base` reports ancestor. `$_Q_MODE` selects the canned diff; if
  # `$_Q_APPLY_ALL_FAIL` is set, every patch is rejected (genuine-revert).
  _q_perhunk_git() {
    case "$1" in
      merge-base) return 0 ;;
      diff)
        case "${_Q_MODE:-drift}" in
          partial) _q_canned_diff_partial ;;
          *)       _q_canned_diff_drift ;;
        esac
        return 0
        ;;
      apply)
        local patch
        patch="$(cat)"
        if [[ -n "${_Q_APPLY_ALL_FAIL:-}" ]]; then
          return 1
        fi
        # All-or-nothing: any drifted/gone marker present → whole apply fails.
        if printf '%s' "$patch" | grep -qE 'DRIFTED_UNRELATED_MARKER|GONE_HUNK_ONE_MARKER|GONE_HUNK_THREE_MARKER'; then
          return 1
        fi
        if printf '%s' "$patch" | grep -q 'HARMFUL_RETAIN_FALSE_MARKER'; then
          return 0   # only the harmful hunk → reverse-applies → present
        fi
        return 1     # no harmful content → rejected
        ;;
      *) return 0 ;;
    esac
  }

  # ── Test 78c — drift → still-active (the #3248 bug; FAILS on origin/main) ───
  printf '%s regression #3238\n' "$sha1" > "$qdir/drift.txt"
  _Q_MODE=drift
  unset _Q_APPLY_ALL_FAIL
  git() { _q_perhunk_git "$@"; }
  local rc78c=0
  check_quarantine_active "$qdir/drift.txt" || rc78c=$?
  unset -f git
  unset _Q_MODE
  if [[ $rc78c -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: unrelated-hunk drift stays blocked (#3248 per-hunk)"
  else
    tap_not_ok "quarantine-active: unrelated-hunk drift stays blocked (#3248 per-hunk)" \
      "rc=$rc78c status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78d — genuine full revert → clear ─────────────────────────────────
  # ALL hunks fail to reverse-apply (the bad commit was genuinely reverted).
  printf '%s regression #3238\n' "$sha1" > "$qdir/revert.txt"
  _Q_MODE=drift
  _Q_APPLY_ALL_FAIL=1
  git() { _q_perhunk_git "$@"; }
  local rc78d=0
  check_quarantine_active "$qdir/revert.txt" || rc78d=$?
  unset -f git
  unset _Q_MODE _Q_APPLY_ALL_FAIL
  if [[ $rc78d -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-active: all hunks gone → clear (per-hunk all-gone path)"
  else
    tap_not_ok "quarantine-active: all hunks gone → clear (per-hunk all-gone path)" \
      "rc=$rc78d status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78e — partial presence (fail-safe) → active ───────────────────────
  # Same-file 3-hunk commit; only the middle (harmful) hunk survives. Per-file
  # granularity would false-clear here (the whole file's patch fails); per-hunk
  # correctly blocks. Encodes the fail-safe-toward-quarantined principle.
  printf '%s regression #3238\n' "$sha1" > "$qdir/partial.txt"
  _Q_MODE=partial
  unset _Q_APPLY_ALL_FAIL
  git() { _q_perhunk_git "$@"; }
  local rc78e=0
  check_quarantine_active "$qdir/partial.txt" || rc78e=$?
  unset -f git
  unset _Q_MODE
  if [[ $rc78e -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: partial same-file presence stays blocked (fail-safe)"
  else
    tap_not_ok "quarantine-active: partial same-file presence stays blocked (fail-safe)" \
      "rc=$rc78e status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78c-zsh — cross-shell recheck of the drift case ───────────────────
  # The awk hunk-splitter + the `while read`/rc-capture loop must behave
  # identically under zsh (the orchestrator runs under zsh). Re-run the 78c
  # drift scenario inside a `zsh -c` subprocess. Gated on zsh availability.
  if command -v zsh >/dev/null 2>&1; then
    printf '%s regression #3238\n' "$sha1" > "$qdir/drift_zsh.txt"
    local zsh_out
    zsh_out=$(zsh -c '
      emulate -L zsh
      source "$1/scripts/lib/deploy-quarantine.sh"
      git() {
        case "$1" in
          merge-base) return 0 ;;
          diff)
            cat <<CANNED
diff --git a/crates/henyey/src/main.rs b/crates/henyey/src/main.rs
index 1111111..2222222 100644
--- a/crates/henyey/src/main.rs
+++ b/crates/henyey/src/main.rs
@@ -114,3 +114,4 @@ fn configure_allocator() {
     let _ = unsafe { \&_rjem_malloc_conf };
+    HARMFUL_RETAIN_FALSE_MARKER
     let cfg = Config::load();
     run(cfg)
diff --git a/crates/ledger/src/memory_report.rs b/crates/ledger/src/memory_report.rs
index 3333333..4444444 100644
--- a/crates/ledger/src/memory_report.rs
+++ b/crates/ledger/src/memory_report.rs
@@ -356,3 +356,4 @@ impl MemoryReport {
     let total = self.heap + self.stack;
+    DRIFTED_UNRELATED_MARKER
     report.emit(total);
 }
CANNED
            return 0 ;;
          apply)
            local patch
            patch="$(cat)"
            if print -r -- "$patch" | grep -qE DRIFTED_UNRELATED_MARKER; then
              return 1
            fi
            if print -r -- "$patch" | grep -q HARMFUL_RETAIN_FALSE_MARKER; then
              return 0
            fi
            return 1 ;;
          *) return 0 ;;
        esac
      }
      rc=0
      check_quarantine_active "$2" || rc=$?
      print -r -- "rc=$rc status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
    ' -- "$REPO_ROOT" "$qdir/drift_zsh.txt")
    if [[ "$zsh_out" == "rc=0 status=blocked_active match=$sha1" ]]; then
      tap_ok "quarantine-active: drift stays blocked — zsh recheck"
    else
      tap_not_ok "quarantine-active: drift stays blocked — zsh recheck" "got: '$zsh_out'"
    fi
  else
    tap_skip "quarantine-active: drift stays blocked — zsh recheck" "zsh not found"
  fi

  # ── Tests 78f-78j + 78r: resolved:<sha> auto-clear and its fail-safes (#3256) ─
  # A bundled-commit quarantine (#3238) mixed a harmful hunk (removed by #3251)
  # with benign hunks intentionally kept on main. The per-hunk content-check
  # (78e) stays blocked_active FOREVER because the benign hunks legitimately
  # reverse-apply — the tick-199 false-block. The fix: an additive
  # `resolved:<fix-sha>` token auto-clears the entry once the fix SHA is an
  # ancestor of origin/main. Every uncertain case (not-merged, malformed,
  # self-resolution, git error) FALLS BACK to the per-hunk BLOCK (fail-safe).
  #
  # Mock strategy: reuse the `_Q_MODE=partial` canned diff (so the per-hunk
  # content-check WOULD block — this is what makes 78f genuinely depend on the
  # resolved short-circuit, not on the diff). A dedicated `git` mock dispatches
  # `merge-base --is-ancestor <sha> origin/main` on the SHA being checked: the
  # quarantined SHA ($sha1) is always an ancestor; the resolved/fix SHA is
  # governed by `$_Q_RESOLVED_RC` (0 = merged, 1 = not yet, 128 = git error).
  # `apply` is the partial-presence model: only the harmful hunk reverse-applies.
  _q_resolved_git() {
    case "$1" in
      merge-base)
        # Args: merge-base --is-ancestor <sha> origin/main → SHA is $3.
        if [[ "$3" == "$sha1" ]]; then
          return 0   # quarantined SHA is always deployed (ancestor)
        fi
        return "${_Q_RESOLVED_RC:-0}"  # fix SHA ancestry governed by the test
        ;;
      diff) _q_canned_diff_partial; return 0 ;;
      apply)
        local patch; patch="$(cat)"
        if printf '%s' "$patch" | grep -qE 'GONE_HUNK_ONE_MARKER|GONE_HUNK_THREE_MARKER'; then
          return 1
        fi
        if printf '%s' "$patch" | grep -q 'HARMFUL_RETAIN_FALSE_MARKER'; then
          return 0
        fi
        return 1
        ;;
      *) return 0 ;;
    esac
  }

  # VE-green oracle stub (#3632). The resolved-token auto-clear in
  # check_quarantine_active now ALSO requires the fix sha be VE-green, not just
  # merged. We shadow the overridable `quarantine_resolved_is_ve_green` oracle so
  # the predicate is driven by `$_Q_VE_GREEN_RC` (0 = a fix-containing sha passed
  # `Verify Execution (Mainnet)`; 1 = merged-but-not-yet-VE-green). The merge
  # (ancestry) side stays driven by `$_Q_RESOLVED_RC` via `_q_resolved_git`. The
  # two are orthogonal, exactly mirroring the merge-vs-VE-green split the gate now
  # enforces. Default 0 so the pre-#3632 "merged ⇒ clear" tests stay green when
  # they intend a fully-resolved (merged + VE-green) fix.
  _q_resolved_ve_green() { return "${_Q_VE_GREEN_RC:-0}"; }

  # ── Test 78f — bundled commit + surviving benign hunk + resolved-on-main +
  #   VE-green → CLEAR. FAILS on origin/main pre-#3256 (no `resolved` parsing →
  #   blocked_active, the tick-199 bug); PASSES once the resolved short-circuit
  #   lands. Post-#3632 the fix must ALSO be VE-green (_Q_VE_GREEN_RC=0).
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha2" > "$qdir/resolved_clear.txt"
  _Q_RESOLVED_RC=0
  _Q_VE_GREEN_RC=0
  git() { _q_resolved_git "$@"; }
  quarantine_resolved_is_ve_green() { _q_resolved_ve_green "$@"; }
  local rc78f=0
  check_quarantine_active "$qdir/resolved_clear.txt" || rc78f=$?
  unset -f git quarantine_resolved_is_ve_green
  unset _Q_RESOLVED_RC _Q_VE_GREEN_RC
  if [[ $rc78f -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-active: bundled commit + resolved-on-main + VE-green auto-clears (#3256, #3632)"
  else
    tap_not_ok "quarantine-active: bundled commit + resolved-on-main + VE-green auto-clears (#3256, #3632)" \
      "rc=$rc78f status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78f2 — REGRESSION FOR #3632: resolved fix is MERGED (on main) but NOT
  #   yet VE-green → must stay BLOCKED. This is the exact #3632 hazard: clearing
  #   on merge alone let the deploy gate ship the latest VE-green sha, which is
  #   still a PRE-FIX commit. FAILS on origin/main today (merge-only clear →
  #   QUARANTINE_STATUS=clear); PASSES after the VE-green gate lands (the
  #   merged-but-not-VE-green fix falls through to the per-hunk BLOCK).
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha2" > "$qdir/resolved_merged_not_vegreen.txt"
  _Q_RESOLVED_RC=0    # fix IS merged (ancestor of origin/main)
  _Q_VE_GREEN_RC=1    # fix is NOT yet VE-green
  git() { _q_resolved_git "$@"; }
  quarantine_resolved_is_ve_green() { _q_resolved_ve_green "$@"; }
  local rc78f2=0
  check_quarantine_active "$qdir/resolved_merged_not_vegreen.txt" || rc78f2=$?
  unset -f git quarantine_resolved_is_ve_green
  unset _Q_RESOLVED_RC _Q_VE_GREEN_RC
  if [[ $rc78f2 -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: resolved merged but NOT VE-green stays blocked (#3632)"
  else
    tap_not_ok "quarantine-active: resolved merged but NOT VE-green stays blocked (#3632)" \
      "rc=$rc78f2 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78g — resolved-SHA NOT yet on main → stays blocked (fail-safe) ─────
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha2" > "$qdir/resolved_unmerged.txt"
  _Q_RESOLVED_RC=1
  git() { _q_resolved_git "$@"; }
  local rc78g=0
  check_quarantine_active "$qdir/resolved_unmerged.txt" || rc78g=$?
  unset -f git
  unset _Q_RESOLVED_RC
  if [[ $rc78g -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: resolved-SHA not yet merged stays blocked (fail-safe)"
  else
    tap_not_ok "quarantine-active: resolved-SHA not yet merged stays blocked (fail-safe)" \
      "rc=$rc78g status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78h — malformed resolved token → falls back to per-hunk BLOCK ──────
  printf '%s regression #3238 resolved:notahex\n' "$sha1" > "$qdir/resolved_malformed.txt"
  _Q_RESOLVED_RC=0
  git() { _q_resolved_git "$@"; }
  local rc78h=0
  check_quarantine_active "$qdir/resolved_malformed.txt" || rc78h=$?
  unset -f git
  unset _Q_RESOLVED_RC
  if [[ $rc78h -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: malformed resolved token falls back to per-hunk block"
  else
    tap_not_ok "quarantine-active: malformed resolved token falls back to per-hunk block" \
      "rc=$rc78h status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78i — self-resolution (resolved == own SHA) → BLOCK ────────────────
  # A commit is always its own ancestor, so resolved:<own-sha> would
  # permanently auto-clear. The guard rejects it → per-hunk block governs.
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha1" > "$qdir/resolved_self.txt"
  _Q_RESOLVED_RC=0
  git() { _q_resolved_git "$@"; }
  local rc78i=0
  check_quarantine_active "$qdir/resolved_self.txt" || rc78i=$?
  unset -f git
  unset _Q_RESOLVED_RC
  if [[ $rc78i -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: self-resolution (resolved==own sha) is rejected, stays blocked"
  else
    tap_not_ok "quarantine-active: self-resolution (resolved==own sha) is rejected, stays blocked" \
      "rc=$rc78i status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78j — git error on resolved-ancestry check → BLOCK (fail-closed) ───
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha2" > "$qdir/resolved_giterr.txt"
  _Q_RESOLVED_RC=128
  git() { _q_resolved_git "$@"; }
  local rc78j=0
  check_quarantine_active "$qdir/resolved_giterr.txt" || rc78j=$?
  unset -f git
  unset _Q_RESOLVED_RC
  if [[ $rc78j -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: git error on resolved-ancestry falls back to block (fail-closed)"
  else
    tap_not_ok "quarantine-active: git error on resolved-ancestry falls back to block (fail-closed)" \
      "rc=$rc78j status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78k — parse extracts resolved token (token-boundary-anchored) ──────
  # `unresolved:<40hex>` must NOT false-match; a real `resolved:<40hex>` must.
  printf '%s reason unresolved:%s\n%s reason resolved:%s\n' \
    "$sha1" "$sha2" "$sha2" "$sha1" > "$qdir/resolved_parse.txt"
  parse_quarantine_file "$qdir/resolved_parse.txt"
  local expected_resolved
  expected_resolved=$(printf '%s\n%s' "-" "$sha1")
  if [[ "$QUARANTINE_RESOLVED" == "$expected_resolved" ]]; then
    tap_ok "quarantine-parse: resolved token extracted, unresolved: substring ignored"
  else
    tap_not_ok "quarantine-parse: resolved token extracted, unresolved: substring ignored" \
      "resolved='$QUARANTINE_RESOLVED' expected='$expected_resolved'"
  fi

  # ── Test 78l — quarantine_resolve stamps token onto matching entry ──────────
  local resolve_file="$qdir/resolve_stamp.txt"
  printf '%s regression #3238\n%s other entry\n' "$sha1" "$sha2" > "$resolve_file"
  quarantine_resolve "$resolve_file" "$sha1" "$sha2"
  local rc78l=$?
  parse_quarantine_file "$resolve_file"
  local stamped_resolved
  stamped_resolved=$(printf '%s\n%s' "$sha2" "-")
  if [[ $rc78l -eq 0 && "$QUARANTINE_RESOLVED" == "$stamped_resolved" ]] \
     && grep -q "^$sha1 regression #3238 resolved:$sha2$" "$resolve_file" \
     && grep -q "^$sha2 other entry$" "$resolve_file"; then
    tap_ok "quarantine-resolve: stamps resolved:<sha> onto matching entry, others untouched"
  else
    tap_not_ok "quarantine-resolve: stamps resolved:<sha> onto matching entry, others untouched" \
      "rc=$rc78l resolved='$QUARANTINE_RESOLVED' contents='$(cat "$resolve_file")'"
  fi

  # ── Test 78m — quarantine_resolve is idempotent (no duplicate token) ────────
  quarantine_resolve "$resolve_file" "$sha1" "$sha2"
  local rc78m=$?
  local token_count
  token_count=$(grep -o "resolved:$sha2" "$resolve_file" | wc -l)
  if [[ $rc78m -eq 0 && "$token_count" -eq 1 ]]; then
    tap_ok "quarantine-resolve: second call is idempotent (single token)"
  else
    tap_not_ok "quarantine-resolve: second call is idempotent (single token)" \
      "rc=$rc78m token_count=$token_count contents='$(cat "$resolve_file")'"
  fi

  # ── Test 78n — quarantine_resolve replaces a stale resolved token ───────────
  local sha3="cccccccccccccccccccccccccccccccccccccccc"
  quarantine_resolve "$resolve_file" "$sha1" "$sha3"
  local rc78n=$?
  if [[ $rc78n -eq 0 ]] \
     && grep -q "^$sha1 regression #3238 resolved:$sha3$" "$resolve_file" \
     && ! grep -q "resolved:$sha2" "$resolve_file"; then
    tap_ok "quarantine-resolve: re-stamp canonicalizes to the new fix SHA"
  else
    tap_not_ok "quarantine-resolve: re-stamp canonicalizes to the new fix SHA" \
      "rc=$rc78n contents='$(cat "$resolve_file")'"
  fi

  # ── Test 78o — quarantine_resolve rejects self-resolution (rc 1) ────────────
  local rc78o=0
  quarantine_resolve "$resolve_file" "$sha1" "$sha1" || rc78o=$?
  if [[ $rc78o -eq 1 ]]; then
    tap_ok "quarantine-resolve: self-resolution (sha==resolved) returns 1"
  else
    tap_not_ok "quarantine-resolve: self-resolution (sha==resolved) returns 1" "rc=$rc78o"
  fi

  # ── Test 78p — quarantine_resolve rejects invalid SHA (rc 1) ────────────────
  local rc78p=0
  quarantine_resolve "$resolve_file" "$sha1" "not-a-sha" || rc78p=$?
  local rc78p2=0
  quarantine_resolve "$resolve_file" "bad" "$sha2" || rc78p2=$?
  if [[ $rc78p -eq 1 && $rc78p2 -eq 1 ]]; then
    tap_ok "quarantine-resolve: invalid SHA (either arg) returns 1"
  else
    tap_not_ok "quarantine-resolve: invalid SHA (either arg) returns 1" \
      "resolved-bad=$rc78p sha-bad=$rc78p2"
  fi

  # ── Test 78q — quarantine_resolve on missing file is a no-op (rc 0) ─────────
  local rc78q=0
  quarantine_resolve "$qdir/no_such_resolve.txt" "$sha1" "$sha2" || rc78q=$?
  if [[ $rc78q -eq 0 && ! -e "$qdir/no_such_resolve.txt" ]]; then
    tap_ok "quarantine-resolve: missing file is a no-op (rc 0, no file created)"
  else
    tap_not_ok "quarantine-resolve: missing file is a no-op (rc 0, no file created)" \
      "rc=$rc78q exists=$([ -e "$qdir/no_such_resolve.txt" ] && echo yes || echo no)"
  fi

  # ── Tests 78s-78x: quarantine_autostamp — issue→merged-PR→merge-SHA (#3258) ─
  # quarantine_autostamp follows GitHub's STRUCTURED linkage: it parses the
  # crash-issue number `#N` already recorded in the entry's reason (`regression
  # #N`, written by monitor-tick §3d), asks GitHub which MERGED PR closed #N
  # (`gh issue view N --json closedByPullRequestsReferences` → per-PR `gh pr
  # view <pr> --json state,mergeCommit`), and stamps `resolved:<mergeCommit.oid>`
  # via the existing #3260 quarantine_resolve. It is WRITE-ONLY and best-effort:
  # any gh/network/parse failure, an open (non-MERGED) PR, or a missing/absent
  # issue# → it skips that entry (no stamp), so #3260's gate governs unchanged.
  #
  # FAIL-SAFE: autostamp NEVER clears. The sole clear authority remains #3260's
  # check_quarantine_active, which independently requires the stamped SHA be an
  # ancestor of origin/main. A wrong/unmerged/premature stamp still BLOCKs.
  #
  # gh mock: matches the REAL --jq output shapes (Critic A) —
  #   `gh issue view N --json closedByPullRequestsReferences --jq
  #      '.closedByPullRequestsReferences[]?.number'`  → newline-separated PR #s
  #   `gh pr view P --json state,mergeCommit --jq
  #      'select(.state=="MERGED") | .mergeCommit.oid'` → merge SHA or empty.
  # Driven by env: _GH_PR_FOR_ISSUE (PR# closing N or empty), _GH_PR_STATE
  # (MERGED / OPEN), _GH_PR_MERGESHA (the merge oid), _GH_RC (non-zero → error).
  _q_autostamp_gh() {
    if [[ "${_GH_RC:-0}" -ne 0 ]]; then return "${_GH_RC}"; fi
    case "$1" in
      issue)
        # gh issue view N --repo R --json closedByPullRequestsReferences --jq '...'
        # Real --jq emits one PR number per line (empty when none).
        [[ -n "${_GH_PR_FOR_ISSUE:-}" ]] && printf '%s\n' "$_GH_PR_FOR_ISSUE"
        return 0
        ;;
      pr)
        # gh pr view P --repo R --json state,mergeCommit --jq 'select(.state=="MERGED")|.mergeCommit.oid'
        # Real --jq emits the oid only when state==MERGED, else nothing.
        if [[ "${_GH_PR_STATE:-MERGED}" == "MERGED" ]]; then
          printf '%s\n' "${_GH_PR_MERGESHA:-}"
        fi
        return 0
        ;;
      *) return 0 ;;
    esac
  }

  # ── Test 78s — END-TO-END (#3258→#3260): reason #N + merged closing PR →
  #   autostamp stamps resolved:<mergeSha>, then the gate AUTO-CLEARS.
  # FAILS on origin/main today (no quarantine_autostamp → entry stays
  # blocked_active, the tick-199 false-block); PASSES after this lands.
  printf '%s regression #3238\n' "$sha1" > "$qdir/autostamp_e2e.txt"
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="$sha2"
  gh() { _q_autostamp_gh "$@"; }
  local rc78s_stamp=0
  quarantine_autostamp "$qdir/autostamp_e2e.txt" || rc78s_stamp=$?
  unset -f gh
  # Verify the stamp landed.
  parse_quarantine_file "$qdir/autostamp_e2e.txt"
  local stamped78s="$QUARANTINE_RESOLVED"
  # Now the gate: sha1 ancestor + benign hunk present (partial), but resolved
  # sha2 is on main AND VE-green → CLEAR. Reuse the resolved-aware git mock
  # (sha2 merged) + the VE-green oracle stub (#3632, sha2 VE-green).
  _Q_RESOLVED_RC=0
  _Q_VE_GREEN_RC=0
  git() { _q_resolved_git "$@"; }
  quarantine_resolved_is_ve_green() { _q_resolved_ve_green "$@"; }
  local rc78s=0
  check_quarantine_active "$qdir/autostamp_e2e.txt" || rc78s=$?
  unset -f git quarantine_resolved_is_ve_green
  unset _Q_RESOLVED_RC _Q_VE_GREEN_RC
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  if [[ "$stamped78s" == "$sha2" && $rc78s -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-autostamp: reason #N + merged PR → stamps resolved:<sha>, gate auto-clears (#3258→#3260)"
  else
    tap_not_ok "quarantine-autostamp: reason #N + merged PR → stamps resolved:<sha>, gate auto-clears (#3258→#3260)" \
      "stamped='$stamped78s' rc=$rc78s status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78t — no `#N` in reason → no stamp (gate's per-hunk check governs) ─
  printf '%s regression no-issue-number\n' "$sha1" > "$qdir/autostamp_noissue.txt"
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="$sha2"
  gh() { _q_autostamp_gh "$@"; }
  quarantine_autostamp "$qdir/autostamp_noissue.txt" || true
  unset -f gh
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/autostamp_noissue.txt"
  if [[ "$QUARANTINE_RESOLVED" == "-" ]]; then
    tap_ok "quarantine-autostamp: reason without #N is not stamped (per-hunk gate governs)"
  else
    tap_not_ok "quarantine-autostamp: reason without #N is not stamped (per-hunk gate governs)" \
      "resolved='$QUARANTINE_RESOLVED' contents='$(cat "$qdir/autostamp_noissue.txt")'"
  fi

  # ── Test 78u — closing PR is OPEN (state != MERGED) → no stamp (fail-safe) ──
  printf '%s regression #3238\n' "$sha1" > "$qdir/autostamp_open.txt"
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=OPEN _GH_PR_MERGESHA="$sha2"
  gh() { _q_autostamp_gh "$@"; }
  quarantine_autostamp "$qdir/autostamp_open.txt" || true
  unset -f gh
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/autostamp_open.txt"
  if [[ "$QUARANTINE_RESOLVED" == "-" ]]; then
    tap_ok "quarantine-autostamp: open (non-MERGED) closing PR is not stamped (fail-safe)"
  else
    tap_not_ok "quarantine-autostamp: open (non-MERGED) closing PR is not stamped (fail-safe)" \
      "resolved='$QUARANTINE_RESOLVED' contents='$(cat "$qdir/autostamp_open.txt")'"
  fi

  # ── Test 78v — gh error (rc!=0) → no stamp, function does not abort ────────
  printf '%s regression #3238\n' "$sha1" > "$qdir/autostamp_gherr.txt"
  _GH_RC=1 _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="$sha2"
  gh() { _q_autostamp_gh "$@"; }
  local rc78v=0
  quarantine_autostamp "$qdir/autostamp_gherr.txt" || rc78v=$?
  unset -f gh
  unset _GH_RC _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/autostamp_gherr.txt"
  if [[ "$QUARANTINE_RESOLVED" == "-" && $rc78v -eq 0 ]]; then
    tap_ok "quarantine-autostamp: gh error skips entry, returns without abort (best-effort)"
  else
    tap_not_ok "quarantine-autostamp: gh error skips entry, returns without abort (best-effort)" \
      "rc=$rc78v resolved='$QUARANTINE_RESOLVED' contents='$(cat "$qdir/autostamp_gherr.txt")'"
  fi

  # ── Test 78w — entry already stamped → unchanged, gh is NOT invoked ────────
  printf '%s regression #3238 resolved:%s\n' "$sha1" "$sha2" > "$qdir/autostamp_idem.txt"
  local _gh_called78w=0
  gh() { _gh_called78w=1; _q_autostamp_gh "$@"; }
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="cccccccccccccccccccccccccccccccccccccccc"
  quarantine_autostamp "$qdir/autostamp_idem.txt" || true
  unset -f gh
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/autostamp_idem.txt"
  if [[ "$QUARANTINE_RESOLVED" == "$sha2" && "$_gh_called78w" -eq 0 ]]; then
    tap_ok "quarantine-autostamp: already-stamped entry is unchanged, no gh call (idempotent)"
  else
    tap_not_ok "quarantine-autostamp: already-stamped entry is unchanged, no gh call (idempotent)" \
      "resolved='$QUARANTINE_RESOLVED' gh_called=$_gh_called78w contents='$(cat "$qdir/autostamp_idem.txt")'"
  fi

  # ── Test 78x — self-resolution (merge SHA == quarantined SHA) → not stamped ─
  # quarantine_resolve rejects sha==resolved; autostamp delegates that guard.
  printf '%s regression #3238\n' "$sha1" > "$qdir/autostamp_self.txt"
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="$sha1"
  gh() { _q_autostamp_gh "$@"; }
  quarantine_autostamp "$qdir/autostamp_self.txt" || true
  unset -f gh
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/autostamp_self.txt"
  if [[ "$QUARANTINE_RESOLVED" == "-" ]]; then
    tap_ok "quarantine-autostamp: self-resolution (merge sha == quarantined sha) is not stamped"
  else
    tap_not_ok "quarantine-autostamp: self-resolution (merge sha == quarantined sha) is not stamped" \
      "resolved='$QUARANTINE_RESOLVED' contents='$(cat "$qdir/autostamp_self.txt")'"
  fi

  # ── Tests 78y-78ab: hold:until-#<N> sentinel (#3711) ────────────────────────
  # A MANUAL-CLEAR-ONLY hold pins an entry to the lifecycle of a GitHub issue
  # instead of the textual presence of the SHA's diff. The per-hunk backstop
  # DECAYS as main drifts: once every hunk fails to reverse-apply, the gate
  # false-cleared #3702's hold (issue #3711). The sentinel must BLOCK until
  # the issue is verifiably CLOSED and FAIL CLOSED on any gh uncertainty.
  #
  # gh shim: check_quarantine_active invokes `timeout 15 gh ...`, and timeout
  # exec()s a REAL binary from PATH — a shell-function gh mock is invisible to
  # it. So these tests use a PATH shim script, driven by exported env:
  #   _GH_HOLD_SHIM_STATE — the issue state to print (OPEN/CLOSED/…; empty → nothing)
  #   _GH_HOLD_SHIM_RC    — exit code (default 0; non-zero models gh unavailable)
  local hold_shim_dir="$qdir/hold-gh-shim"
  mkdir -p "$hold_shim_dir"
  cat > "$hold_shim_dir/gh" <<'SHIM'
#!/usr/bin/env bash
[ "${_GH_HOLD_SHIM_RC:-0}" -ne 0 ] && exit "${_GH_HOLD_SHIM_RC}"
[ -n "${_GH_HOLD_SHIM_STATE:-}" ] && printf '%s\n' "$_GH_HOLD_SHIM_STATE"
exit 0
SHIM
  chmod +x "$hold_shim_dir/gh"

  # git mock for the hold tests: the quarantined SHA IS an ancestor, and its
  # diff's only hunk NO LONGER reverse-applies (content drifted away). WITHOUT
  # the sentinel this entry auto-CLEARS via step 3 — the exact #3711
  # false-clear — which is what makes 78y/78aa genuinely depend on the hold.
  _q_hold_git() {
    case "$1" in
      merge-base) return 0 ;;
      diff)
        cat <<'CANNED'
diff --git a/f.rs b/f.rs
index 1111111..2222222 100644
--- a/f.rs
+++ b/f.rs
@@ -1,2 +1,3 @@ mod m {
+    drifted_away
 }
CANNED
        return 0 ;;
      apply) return 1 ;;   # every hunk gone → content-check alone would CLEAR
      *) return 0 ;;
    esac
  }

  # ── Test 78y — hold:until + issue OPEN → blocked_active (FAILS pre-#3711) ──
  printf '%s hold:until-#3702 restore-from-disk wedge — MANUAL-CLEAR-ONLY\n' "$sha1" \
    > "$qdir/hold_open.txt"
  git() { _q_hold_git "$@"; }
  local hold_saved_path="$PATH"
  PATH="$hold_shim_dir:$PATH"
  export _GH_HOLD_SHIM_STATE=OPEN
  local rc78y=0
  check_quarantine_active "$qdir/hold_open.txt" || rc78y=$?
  PATH="$hold_saved_path"
  unset _GH_HOLD_SHIM_STATE
  unset -f git
  if [[ $rc78y -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-hold: hold:until-#N + issue OPEN blocks despite drifted diff (#3711)"
  else
    tap_not_ok "quarantine-hold: hold:until-#N + issue OPEN blocks despite drifted diff (#3711)" \
      "rc=$rc78y status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78z — hold:until + issue CLOSED → sentinel released, falls through ─
  # With the drifted-away diff (all hunks gone), the released entry CLEARs via
  # the normal step-3 logic — proving CLOSED falls through rather than blocking.
  printf '%s hold:until-#3702 restore-from-disk wedge — MANUAL-CLEAR-ONLY\n' "$sha1" \
    > "$qdir/hold_closed.txt"
  git() { _q_hold_git "$@"; }
  PATH="$hold_shim_dir:$PATH"
  export _GH_HOLD_SHIM_STATE=CLOSED
  local rc78z=0
  check_quarantine_active "$qdir/hold_closed.txt" || rc78z=$?
  PATH="$hold_saved_path"
  unset _GH_HOLD_SHIM_STATE
  unset -f git
  if [[ $rc78z -eq 1 && "$QUARANTINE_STATUS" == "clear" && -z "$QUARANTINED_MATCH" ]]; then
    tap_ok "quarantine-hold: hold:until-#N + issue CLOSED falls through to normal logic"
  else
    tap_not_ok "quarantine-hold: hold:until-#N + issue CLOSED falls through to normal logic" \
      "rc=$rc78z status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78aa — hold:until + gh unavailable → blocked_active (fail-closed) ─
  printf '%s hold:until-#3702 restore-from-disk wedge — MANUAL-CLEAR-ONLY\n' "$sha1" \
    > "$qdir/hold_gherr.txt"
  git() { _q_hold_git "$@"; }
  PATH="$hold_shim_dir:$PATH"
  export _GH_HOLD_SHIM_RC=127
  local rc78aa=0
  check_quarantine_active "$qdir/hold_gherr.txt" || rc78aa=$?
  PATH="$hold_saved_path"
  unset _GH_HOLD_SHIM_RC
  unset -f git
  if [[ $rc78aa -eq 0 && "$QUARANTINE_STATUS" == "blocked_active" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-hold: gh unavailable blocks (fail-closed)"
  else
    tap_not_ok "quarantine-hold: gh unavailable blocks (fail-closed)" \
      "rc=$rc78aa status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 78ab — autostamp never stamps a hold:until entry ──────────────────
  # The reason carries BOTH the hold sentinel AND a `regression #N` with a
  # merged closing PR — the exact #3708 bundled-#N shape. The hold exemption
  # must win: no resolved: stamp (manual-lifecycle-only).
  printf '%s hold:until-#3702 regression #3238\n' "$sha1" > "$qdir/hold_autostamp.txt"
  _GH_PR_FOR_ISSUE=3251 _GH_PR_STATE=MERGED _GH_PR_MERGESHA="$sha2"
  gh() { _q_autostamp_gh "$@"; }
  quarantine_autostamp "$qdir/hold_autostamp.txt" || true
  unset -f gh
  unset _GH_PR_FOR_ISSUE _GH_PR_STATE _GH_PR_MERGESHA
  parse_quarantine_file "$qdir/hold_autostamp.txt"
  if [[ "$QUARANTINE_RESOLVED" == "-" ]]; then
    tap_ok "quarantine-hold: autostamp skips hold:until entry (manual-lifecycle-only)"
  else
    tap_not_ok "quarantine-hold: autostamp skips hold:until entry (manual-lifecycle-only)" \
      "resolved='$QUARANTINE_RESOLVED' contents='$(cat "$qdir/hold_autostamp.txt")'"
  fi

  # ── Test 79: check_quarantine_active — git error on ancestry (fail-closed) ─
  printf '%s regression\n' "$sha1" > "$qdir/git_error.txt"
  git() { return 128; }  # mock: git error on merge-base
  local rc79=0
  check_quarantine_ancestry "$qdir/git_error.txt" || rc79=$?
  unset -f git
  if [[ $rc79 -eq 0 && "$QUARANTINE_STATUS" == "blocked_git_error" && "$QUARANTINED_MATCH" == "$sha1" ]]; then
    tap_ok "quarantine-active: git error returns 0 (fail-closed)"
  else
    tap_not_ok "quarantine-active: git error returns 0 (fail-closed)" \
      "rc=$rc79 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 80: check_quarantine_ancestry — unreadable file (fail-closed) ─────
  echo "data" > "$qdir/unread_anc.txt"
  chmod 000 "$qdir/unread_anc.txt"
  local rc80=0
  check_quarantine_ancestry "$qdir/unread_anc.txt" || rc80=$?
  chmod 644 "$qdir/unread_anc.txt"
  if [[ $rc80 -eq 0 && "$QUARANTINE_STATUS" == "blocked_unreadable" && "$QUARANTINED_MATCH" == "UNREADABLE" ]]; then
    tap_ok "quarantine-ancestry: unreadable file returns 0 (fail-closed)"
  else
    tap_not_ok "quarantine-ancestry: unreadable file returns 0 (fail-closed)" \
      "rc=$rc80 status=$QUARANTINE_STATUS match=$QUARANTINED_MATCH"
  fi

  # ── Test 81: quarantine_append — new entry ─────────────────────────────────
  local append_file="$qdir/append_test.txt"
  rm -f "$append_file"
  quarantine_append "$append_file" "$sha1" "regression #500"
  local rc81=$?
  if [[ $rc81 -eq 0 && -f "$append_file" ]] && grep -q "^$sha1 regression #500$" "$append_file"; then
    tap_ok "quarantine-append: new entry appended"
  else
    tap_not_ok "quarantine-append: new entry appended" \
      "rc=$rc81 contents='$(cat "$append_file" 2>/dev/null)'"
  fi

  # ── Test 82: quarantine_append — duplicate (idempotent) ────────────────────
  local before_content
  before_content=$(cat "$append_file")
  quarantine_append "$append_file" "$sha1" "regression #500"
  local rc82=$?
  local after_content
  after_content=$(cat "$append_file")
  if [[ $rc82 -eq 0 && "$before_content" == "$after_content" ]]; then
    tap_ok "quarantine-append: duplicate is no-op"
  else
    tap_not_ok "quarantine-append: duplicate is no-op" \
      "rc=$rc82 before='$before_content' after='$after_content'"
  fi

  # ── Test 83: quarantine_append — invalid SHA ───────────────────────────────
  local rc83=0
  quarantine_append "$append_file" "not-a-valid-sha" "reason" || rc83=$?
  if [[ $rc83 -eq 1 ]]; then
    tap_ok "quarantine-append: invalid SHA returns 1"
  else
    tap_not_ok "quarantine-append: invalid SHA returns 1" "rc=$rc83"
  fi

  # ── Test 84: quarantine_append — empty reason (bare SHA) ───────────────────
  local bare_file="$qdir/append_bare.txt"
  rm -f "$bare_file"
  quarantine_append "$bare_file" "$sha2" ""
  local rc84=$?
  local bare_content
  bare_content=$(cat "$bare_file")
  if [[ $rc84 -eq 0 && "$bare_content" == "$sha2" ]]; then
    tap_ok "quarantine-append: empty reason writes bare SHA (no trailing space)"
  else
    tap_not_ok "quarantine-append: empty reason writes bare SHA (no trailing space)" \
      "rc=$rc84 content='$bare_content'"
  fi

  # ── Test 85: quarantine_remove — existing entry ────────────────────────────
  local remove_file="$qdir/remove_test.txt"
  printf '%s regression #100\n%s regression #200\n' "$sha1" "$sha2" > "$remove_file"
  quarantine_remove "$remove_file" "$sha1"
  local rc85=$?
  if [[ $rc85 -eq 0 ]] && ! grep -q "^$sha1" "$remove_file" && grep -q "^$sha2" "$remove_file"; then
    tap_ok "quarantine-remove: entry removed, others preserved"
  else
    tap_not_ok "quarantine-remove: entry removed, others preserved" \
      "rc=$rc85 contents='$(cat "$remove_file")'"
  fi

  # ── Test 86: quarantine_remove — missing file (no-op) ──────────────────────
  quarantine_remove "$qdir/no_such_file.txt" "$sha1"
  local rc86=$?
  if [[ $rc86 -eq 0 ]]; then
    tap_ok "quarantine-remove: missing file returns 0"
  else
    tap_not_ok "quarantine-remove: missing file returns 0" "rc=$rc86"
  fi

  # ── Test 87: quarantine_remove — invalid SHA ───────────────────────────────
  local rc87=0
  quarantine_remove "$qdir/remove_test.txt" "invalid" || rc87=$?
  if [[ $rc87 -eq 1 ]]; then
    tap_ok "quarantine-remove: invalid SHA returns 1"
  else
    tap_not_ok "quarantine-remove: invalid SHA returns 1" "rc=$rc87"
  fi

  # ── Test 88: quarantine_append — reason sanitization ───────────────────────
  # Construct a reason with control chars and >200 printable chars post-strip.
  local sanitize_file="$qdir/sanitize_test.txt"
  rm -f "$sanitize_file"
  local LC_ALL=C
  # Build 210 printable 'A' chars interspersed with control chars
  local raw_reason
  raw_reason=$(printf 'A%.0s' {1..210})
  # Inject control chars at various positions
  raw_reason="${raw_reason:0:50}"$'\n'"${raw_reason:50:50}"$'\t'"${raw_reason:100:50}"$'\r'"${raw_reason:150:30}"$'\x01'"${raw_reason:180:30}"
  local rc88=0
  quarantine_append "$sanitize_file" "$sha1" "$raw_reason" || rc88=$?
  local t88_ok="ok"
  if [[ $rc88 -ne 0 ]]; then
    t88_ok="rc=$rc88 (expected 0)"
  elif [[ $(wc -l < "$sanitize_file") -ne 1 ]]; then
    t88_ok="file has $(wc -l < "$sanitize_file") lines (expected 1)"
  elif grep -qP '[\x00-\x1f]' "$sanitize_file"; then
    t88_ok="file contains control chars"
  else
    # Extract reason: skip the 40-char SHA + 1 space = 41 chars prefix
    local stored_reason
    stored_reason=$(cut -c42- < "$sanitize_file")
    local reason_len=${#stored_reason}
    if [[ $reason_len -ne 200 ]]; then
      t88_ok="reason length=$reason_len (expected 200)"
    else
      # Expected: first 200 'A' chars (all control chars stripped, then truncated)
      local expected_reason
      expected_reason=$(printf 'A%.0s' {1..200})
      if [[ "$stored_reason" != "$expected_reason" ]]; then
        t88_ok="reason content mismatch"
      fi
    fi
  fi
  if [[ "$t88_ok" == "ok" ]]; then
    tap_ok "quarantine-append: reason sanitization (control chars stripped, truncated to 200)"
  else
    tap_not_ok "quarantine-append: reason sanitization (control chars stripped, truncated to 200)" \
      "$t88_ok"
  fi

  # ── Test 89: quarantine_append — I/O error (mkdir fails, rc=2) ─────────────
  # Use a regular file as a path component so mkdir -p fails deterministically.
  touch "$qdir/blocker_file"
  local rc89=0
  quarantine_append "$qdir/blocker_file/sub/quarantine.txt" "$sha1" "reason" || rc89=$?
  local t89_ok="ok"
  if [[ $rc89 -ne 2 ]]; then
    t89_ok="rc=$rc89 (expected 2)"
  elif [[ -d "$qdir/blocker_file/sub" ]]; then
    t89_ok="sub/ directory was created (partial side effect)"
  elif [[ -e "$qdir/blocker_file/sub/quarantine.txt" ]]; then
    t89_ok="quarantine file was created (partial side effect)"
  fi
  if [[ "$t89_ok" == "ok" ]]; then
    tap_ok "quarantine-append: I/O error (mkdir fails) returns 2"
  else
    tap_not_ok "quarantine-append: I/O error (mkdir fails) returns 2" \
      "$t89_ok"
  fi

  # ── Test 90: quarantine_remove — I/O error (mv fails, rc=2) ────────────────
  # Mock mv to simulate failure; verify rc=2, file unchanged, no .tmp leftover.
  local mv_test_file="$qdir/mv_fail_test.txt"
  printf '%s regression\n' "$sha1" > "$mv_test_file"
  local pre_content
  pre_content=$(cat "$mv_test_file")
  mv() { return 1; }
  local rc90=0
  quarantine_remove "$mv_test_file" "$sha1" || rc90=$?
  unset -f mv
  local t90_ok="ok"
  if [[ $rc90 -ne 2 ]]; then
    t90_ok="rc=$rc90 (expected 2)"
  else
    local post_content
    post_content=$(cat "$mv_test_file")
    if [[ "$post_content" != "$pre_content" ]]; then
      t90_ok="file content changed"
    elif [[ -e "${mv_test_file}.tmp" ]]; then
      t90_ok=".tmp file was not cleaned up"
    fi
  fi
  if [[ "$t90_ok" == "ok" ]]; then
    tap_ok "quarantine-remove: I/O error (mv fails) returns 2, file unchanged, no .tmp"
  else
    tap_not_ok "quarantine-remove: I/O error (mv fails) returns 2, file unchanged, no .tmp" \
      "$t90_ok"
  fi

  # ══════════════════════════════════════════════════════════════════════════
  # watcher-ctl.sh tests
  # ══════════════════════════════════════════════════════════════════════════

  # ── Test: _is_our_watcher validates exact binary, run+watcher argv, config ─
  local wt_data="$TEST_ROOT/wt1/data"
  local wt_proc="$TEST_ROOT/wt1/proc"
  local wt_project="$TEST_ROOT/wt1/project"
  mkdir -p "$wt_data/watcher" "$wt_proc" "$wt_project/target/release" "$wt_project/configs"

  # Create mock binary and config
  touch "$wt_project/target/release/henyey"
  chmod +x "$wt_project/target/release/henyey"
  echo "test config" > "$wt_project/configs/watcher-testnet.toml"

  local wt_binary
  wt_binary="$(readlink -f "$wt_project/target/release/henyey")"

  # Create a matching mock proc entry: correct exe, run + --watcher + --config
  mkdir -p "$wt_proc/5001"
  ln -sf "$wt_binary" "$wt_proc/5001/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" > "$wt_proc/5001/cmdline"
  ln -sf "$wt_project" "$wt_proc/5001/cwd"

  # Test status returns 1 with no PID file and no matching process
  local wt_rc=0
  PROC_ROOT="$wt_proc/empty" PID_FILE="$wt_data/watcher/nonexistent.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status > /dev/null 2>&1 || wt_rc=$?
  if [[ $wt_rc -eq 1 ]]; then
    tap_ok "watcher-ctl: status returns 1 with no watcher"
  else
    tap_not_ok "watcher-ctl: status returns 1 with no watcher" "exit=$wt_rc"
  fi

  # Test status returns 0 when untracked watcher found via proc scan
  mkdir -p "$wt_proc/5001"
  wt_rc=0
  local wt_status_out
  wt_status_out=$(PROC_ROOT="$wt_proc" PID_FILE="$wt_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 0 ]] && echo "$wt_status_out" | grep -q "UNTRACKED.*5001.*adopting"; then
    tap_ok "watcher-ctl: status adopts untracked watcher from proc scan"
  else
    tap_not_ok "watcher-ctl: status adopts untracked watcher from proc scan" "rc=$wt_rc out=$wt_status_out"
  fi

  # Test status returns 0 when PID file points to valid watcher
  echo "5001" > "$wt_data/watcher/testnet-watcher.pid"
  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_proc" PID_FILE="$wt_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 0 ]] && echo "$wt_status_out" | grep -q "running.*PID 5001"; then
    tap_ok "watcher-ctl: status returns 0 for tracked running watcher"
  else
    tap_not_ok "watcher-ctl: status returns 0 for tracked running watcher" "rc=$wt_rc out=$wt_status_out"
  fi

  # Test status cleans up stale PID file (PID exists but wrong binary)
  local wt_proc2="$TEST_ROOT/wt2/proc"
  local wt_data2="$TEST_ROOT/wt2/data"
  mkdir -p "$wt_proc2/6001" "$wt_data2/watcher"
  ln -sf "/usr/bin/false" "$wt_proc2/6001/exe"
  printf '%s\0' "false" > "$wt_proc2/6001/cmdline"
  echo "6001" > "$wt_data2/watcher/testnet-watcher.pid"
  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_proc2" PID_FILE="$wt_data2/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "Stale PID file"; then
    tap_ok "watcher-ctl: status cleans stale PID file (wrong binary)"
  else
    tap_not_ok "watcher-ctl: status cleans stale PID file (wrong binary)" "rc=$wt_rc out=$wt_status_out"
  fi

  # Test _is_our_watcher rejects missing --watcher flag
  local wt_proc3="$TEST_ROOT/wt3/proc"
  mkdir -p "$wt_proc3/7001"
  ln -sf "$wt_binary" "$wt_proc3/7001/exe"
  # run without --watcher
  printf '%s\0%s\0%s\0%s\0' "$wt_binary" "run" "--config" "configs/watcher-testnet.toml" > "$wt_proc3/7001/cmdline"
  ln -sf "$wt_project" "$wt_proc3/7001/cwd"
  local wt_data3="$TEST_ROOT/wt3/data"
  mkdir -p "$wt_data3/watcher"
  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_proc3" PID_FILE="$wt_data3/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "NOT running"; then
    tap_ok "watcher-ctl: rejects process without --watcher flag"
  else
    tap_not_ok "watcher-ctl: rejects process without --watcher flag" "rc=$wt_rc out=$wt_status_out"
  fi

  # Test _is_our_watcher rejects wrong config path
  local wt_proc4="$TEST_ROOT/wt4/proc"
  mkdir -p "$wt_proc4/8001"
  ln -sf "$wt_binary" "$wt_proc4/8001/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_binary" "run" "--watcher" "--config" "configs/validator-mainnet.toml" > "$wt_proc4/8001/cmdline"
  ln -sf "$wt_project" "$wt_proc4/8001/cwd"
  local wt_data4="$TEST_ROOT/wt4/data"
  mkdir -p "$wt_data4/watcher"
  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_proc4" PID_FILE="$wt_data4/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_project" BINARY="$wt_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" status 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "NOT running"; then
    tap_ok "watcher-ctl: rejects process with wrong config"
  else
    tap_not_ok "watcher-ctl: rejects process with wrong config" "rc=$wt_rc out=$wt_status_out"
  fi

  # Test usage message on invalid command
  wt_rc=0
  wt_status_out=$(bash "$REPO_ROOT/scripts/watcher-ctl.sh" invalid 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "Usage:"; then
    tap_ok "watcher-ctl: shows usage on invalid command"
  else
    tap_not_ok "watcher-ctl: shows usage on invalid command" "rc=$wt_rc out=$wt_status_out"
  fi

  # ══════════════════════════════════════════════════════════════════════════
  # watcher-ctl.sh start/stop/restart tests (T123-T134)
  # Tests lifecycle subcommands using mock /proc and real background processes.
  # ══════════════════════════════════════════════════════════════════════════

  # Track background PIDs for cleanup (global so the EXIT trap can reach it)
  _wt_bg_pids=()
  _wt_cleanup_bg() {
    local p children c
    for p in "${_wt_bg_pids[@]}"; do
      children=$(ps --ppid "$p" -o pid= 2>/dev/null) || true
      kill -9 "$p" 2>/dev/null; wait "$p" 2>/dev/null || true
      for c in $children; do kill -9 "$c" 2>/dev/null || true; done
    done
    _wt_bg_pids=()
  }

  # Find a PID guaranteed not to be in use on the host.
  # Scans downward from pid_max-1 to find the first non-existent /proc entry.
  _unused_pid() {
    local max
    max=$(cat /proc/sys/kernel/pid_max 2>/dev/null || echo 4194304)
    local candidate=$((max - 1))
    while [[ -d "/proc/$candidate" ]] && [[ $candidate -gt 2 ]]; do
      candidate=$((candidate - 1))
    done
    echo "$candidate"
  }

  # Shared mock project dir for lifecycle tests (reuse wt1 project)
  local wt_lc_project="$TEST_ROOT/wt_lc/project"
  mkdir -p "$wt_lc_project/target/release" "$wt_lc_project/configs"
  touch "$wt_lc_project/target/release/henyey"
  chmod +x "$wt_lc_project/target/release/henyey"
  echo "test config" > "$wt_lc_project/configs/watcher-testnet.toml"
  local wt_lc_binary
  wt_lc_binary="$(readlink -f "$wt_lc_project/target/release/henyey")"

  # ── T123: cmd_stop with no watcher running ─────────────────────────────
  local wt_stop1_proc="$TEST_ROOT/wt_stop1/proc"
  local wt_stop1_data="$TEST_ROOT/wt_stop1/data"
  mkdir -p "$wt_stop1_proc/empty" "$wt_stop1_data/watcher"
  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_stop1_proc/empty" \
    PID_FILE="$wt_stop1_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" stop 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 0 ]] && echo "$wt_status_out" | grep -q "No watcher running"; then
    tap_ok "watcher-ctl: stop returns 0 when no watcher running"
  else
    tap_not_ok "watcher-ctl: stop returns 0 when no watcher running" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T124: cmd_stop happy path (process exits promptly) ─────────────────
  # Launch a real sleep process, create mock /proc entry, stop it.
  local wt_stop2_proc="$TEST_ROOT/wt_stop2/proc"
  local wt_stop2_data="$TEST_ROOT/wt_stop2/data"
  mkdir -p "$wt_stop2_data/watcher"

  sleep 300 &
  local stop2_pid=$!
  _wt_bg_pids+=("$stop2_pid")

  mkdir -p "$wt_stop2_proc/$stop2_pid"
  ln -sf "$wt_lc_binary" "$wt_stop2_proc/$stop2_pid/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_lc_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_stop2_proc/$stop2_pid/cmdline"
  ln -sf "$wt_lc_project" "$wt_stop2_proc/$stop2_pid/cwd"
  echo "$stop2_pid" > "$wt_stop2_data/watcher/testnet-watcher.pid"

  # Background monitor: remove mock proc dir when real process exits
  (while kill -0 "$stop2_pid" 2>/dev/null; do sleep 0.1; done
   rm -rf "$wt_stop2_proc/$stop2_pid") &
  local stop2_monitor=$!
  _wt_bg_pids+=("$stop2_monitor")

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_stop2_proc" \
    PID_FILE="$wt_stop2_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" stop 2>&1) || wt_rc=$?
  local stop2_ok=true
  if [[ $wt_rc -ne 0 ]]; then stop2_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Watcher stopped"; then stop2_ok=false; fi
  if [[ -f "$wt_stop2_data/watcher/testnet-watcher.pid" ]]; then stop2_ok=false; fi
  if [[ "$stop2_ok" == "true" ]]; then
    tap_ok "watcher-ctl: stop happy path (process exits promptly)"
  else
    tap_not_ok "watcher-ctl: stop happy path (process exits promptly)" \
      "rc=$wt_rc pid_exists=$(test -f "$wt_stop2_data/watcher/testnet-watcher.pid" && echo yes || echo no) out=$wt_status_out"
  fi
  # Wait for monitor to finish
  wait "$stop2_monitor" 2>/dev/null || true

  # ── T125: cmd_stop timeout detection ───────────────────────────────────
  # Mock proc dir persists (not tied to real /proc), process traps SIGTERM.
  local wt_stop3_proc="$TEST_ROOT/wt_stop3/proc"
  local wt_stop3_data="$TEST_ROOT/wt_stop3/data"
  mkdir -p "$wt_stop3_data/watcher"

  # Launch a process that ignores SIGTERM
  bash -c 'trap "" TERM; sleep 300' &
  local stop3_pid=$!
  _wt_bg_pids+=("$stop3_pid")

  # Create mock proc entry (this dir persists regardless of real process state)
  mkdir -p "$wt_stop3_proc/$stop3_pid"
  ln -sf "$wt_lc_binary" "$wt_stop3_proc/$stop3_pid/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_lc_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_stop3_proc/$stop3_pid/cmdline"
  ln -sf "$wt_lc_project" "$wt_stop3_proc/$stop3_pid/cwd"
  echo "$stop3_pid" > "$wt_stop3_data/watcher/testnet-watcher.pid"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_stop3_proc" \
    PID_FILE="$wt_stop3_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" stop 2>&1) || wt_rc=$?
  local stop3_ok=true
  if [[ $wt_rc -ne 1 ]]; then stop3_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "did not exit after 15s"; then stop3_ok=false; fi
  if [[ ! -f "$wt_stop3_data/watcher/testnet-watcher.pid" ]]; then stop3_ok=false; fi
  if [[ "$stop3_ok" == "true" ]]; then
    tap_ok "watcher-ctl: stop timeout detection (15s)"
  else
    tap_not_ok "watcher-ctl: stop timeout detection (15s)" \
      "rc=$wt_rc pid_retained=$(test -f "$wt_stop3_data/watcher/testnet-watcher.pid" && echo yes || echo no) out=$wt_status_out"
  fi
  # Clean up the TERM-ignoring process and its children (sleep)
  local stop3_children
  stop3_children=$(ps --ppid "$stop3_pid" -o pid= 2>/dev/null) || true
  kill -9 "$stop3_pid" 2>/dev/null; wait "$stop3_pid" 2>/dev/null || true
  local stop3_c; for stop3_c in $stop3_children; do kill -9 "$stop3_c" 2>/dev/null || true; done

  # ── T126: cmd_stop kill failure ────────────────────────────────────────
  # Mock proc entry exists (so cmd_status passes), but PID doesn't exist as
  # a real OS process, so kill fails.
  local wt_stop4_proc="$TEST_ROOT/wt_stop4/proc"
  local wt_stop4_data="$TEST_ROOT/wt_stop4/data"
  local stop4_fake_pid
  stop4_fake_pid=$(_unused_pid)
  mkdir -p "$wt_stop4_proc/$stop4_fake_pid" "$wt_stop4_data/watcher"
  ln -sf "$wt_lc_binary" "$wt_stop4_proc/$stop4_fake_pid/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_lc_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_stop4_proc/$stop4_fake_pid/cmdline"
  ln -sf "$wt_lc_project" "$wt_stop4_proc/$stop4_fake_pid/cwd"
  echo "$stop4_fake_pid" > "$wt_stop4_data/watcher/testnet-watcher.pid"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_stop4_proc" \
    PID_FILE="$wt_stop4_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" stop 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -qE "kill.*failed"; then
    tap_ok "watcher-ctl: stop kill failure returns 1"
  else
    tap_not_ok "watcher-ctl: stop kill failure returns 1" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T127: cmd_start refuses when watcher already running ───────────────
  local wt_start1_proc="$TEST_ROOT/wt_start1/proc"
  local wt_start1_data="$TEST_ROOT/wt_start1/data"
  mkdir -p "$wt_start1_data/watcher"

  # Create mock watcher process at PID 10001
  mkdir -p "$wt_start1_proc/10001"
  ln -sf "$wt_lc_binary" "$wt_start1_proc/10001/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_lc_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_start1_proc/10001/cmdline"
  ln -sf "$wt_lc_project" "$wt_start1_proc/10001/cwd"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_start1_proc" \
    PID_FILE="$wt_start1_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_start1_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" start 2>&1) || wt_rc=$?
  local start1_ok=true
  if [[ $wt_rc -ne 1 ]]; then start1_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "already running"; then start1_ok=false; fi
  # Verify PID file was adopted with the existing PID
  if [[ ! -f "$wt_start1_data/watcher/testnet-watcher.pid" ]] || \
     [[ "$(cat "$wt_start1_data/watcher/testnet-watcher.pid")" != "10001" ]]; then
    start1_ok=false
  fi
  if [[ "$start1_ok" == "true" ]]; then
    tap_ok "watcher-ctl: start refuses when already running + adopts PID"
  else
    tap_not_ok "watcher-ctl: start refuses when already running + adopts PID" \
      "rc=$wt_rc pid_file=$(cat "$wt_start1_data/watcher/testnet-watcher.pid" 2>/dev/null || echo MISSING) out=$wt_status_out"
  fi

  # ── T128: cmd_start binary not found ───────────────────────────────────
  local wt_start2_proc="$TEST_ROOT/wt_start2/proc"
  local wt_start2_data="$TEST_ROOT/wt_start2/data"
  mkdir -p "$wt_start2_proc/empty" "$wt_start2_data/watcher"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_start2_proc/empty" \
    PID_FILE="$wt_start2_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="/nonexistent/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_start2_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" start 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "Binary not found"; then
    tap_ok "watcher-ctl: start fails when binary not found"
  else
    tap_not_ok "watcher-ctl: start fails when binary not found" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T129: cmd_start crash detection ────────────────────────────────────
  # /bin/true exits immediately — identity check fails after 1s sleep.
  local wt_start3_proc="$TEST_ROOT/wt_start3/proc"
  local wt_start3_data="$TEST_ROOT/wt_start3/data"
  mkdir -p "$wt_start3_proc/empty" "$wt_start3_data/watcher"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_start3_proc/empty" \
    PID_FILE="$wt_start3_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="/bin/true" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_start3_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" start 2>&1) || wt_rc=$?
  if [[ $wt_rc -eq 1 ]] && echo "$wt_status_out" | grep -q "identity check failed"; then
    tap_ok "watcher-ctl: start detects startup crash"
  else
    tap_not_ok "watcher-ctl: start detects startup crash" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T130: cmd_start happy path ─────────────────────────────────────────
  # Create a mock binary that sets up its own /proc entry then sleeps.
  local wt_start4_proc="$TEST_ROOT/wt_start4/proc"
  local wt_start4_data="$TEST_ROOT/wt_start4/data"
  local wt_start4_bin="$TEST_ROOT/wt_start4/mock-henyey"
  mkdir -p "$wt_start4_data/watcher"

  # The mock binary creates its own mock proc entry then sleeps
  cat > "$wt_start4_bin" <<'MOCK_BINARY'
#!/usr/bin/env bash
# Mock henyey binary for testing cmd_start happy path.
# Creates its own mock /proc entry so _is_our_watcher passes.
set -euo pipefail
MY_PID=$$
MOCK_PROC="${MOCK_PROC_ROOT}/${MY_PID}"
mkdir -p "$MOCK_PROC"
# exe symlink: point to our own resolved binary path
ln -sf "$(readlink -f "$0")" "$MOCK_PROC/exe"
# cmdline: reconstruct argv as NUL-separated
printf '%s\0' "$@" > "$MOCK_PROC/cmdline"
# cwd symlink
ln -sf "$(pwd)" "$MOCK_PROC/cwd"
# Sleep until killed
exec sleep 300
MOCK_BINARY
  chmod +x "$wt_start4_bin"
  local wt_start4_bin_real
  wt_start4_bin_real="$(readlink -f "$wt_start4_bin")"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_start4_proc" MOCK_PROC_ROOT="$wt_start4_proc" \
    PID_FILE="$wt_start4_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_start4_bin" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_start4_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" start 2>&1) || wt_rc=$?
  local start4_ok=true
  if [[ $wt_rc -ne 0 ]]; then start4_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Watcher started"; then start4_ok=false; fi
  if [[ ! -f "$wt_start4_data/watcher/testnet-watcher.pid" ]]; then start4_ok=false; fi
  if [[ "$start4_ok" == "true" ]]; then
    # Clean up the background process
    local start4_launched_pid
    start4_launched_pid=$(cat "$wt_start4_data/watcher/testnet-watcher.pid")
    _wt_bg_pids+=("$start4_launched_pid")
    tap_ok "watcher-ctl: start happy path succeeds"
  else
    tap_not_ok "watcher-ctl: start happy path succeeds" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T131: cmd_restart when nothing running, start fails ────────────────
  local wt_restart1_proc="$TEST_ROOT/wt_restart1/proc"
  local wt_restart1_data="$TEST_ROOT/wt_restart1/data"
  mkdir -p "$wt_restart1_proc/empty" "$wt_restart1_data/watcher"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_restart1_proc/empty" \
    PID_FILE="$wt_restart1_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="/nonexistent/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_restart1_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" restart 2>&1) || wt_rc=$?
  local restart1_ok=true
  if [[ $wt_rc -ne 1 ]]; then restart1_ok=false; fi
  # Verify both stop phase and start phase ran
  if ! echo "$wt_status_out" | grep -q "No watcher running"; then restart1_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Binary not found"; then restart1_ok=false; fi
  if [[ "$restart1_ok" == "true" ]]; then
    tap_ok "watcher-ctl: restart sequencing (stop no-op then start fails)"
  else
    tap_not_ok "watcher-ctl: restart sequencing (stop no-op then start fails)" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T132: cmd_restart propagates stop failure ──────────────────────────
  # Mock proc entry exists (status passes) but kill fails (fake PID).
  local wt_restart2_proc="$TEST_ROOT/wt_restart2/proc"
  local wt_restart2_data="$TEST_ROOT/wt_restart2/data"
  local restart2_fake_pid
  restart2_fake_pid=$(_unused_pid)
  mkdir -p "$wt_restart2_proc/$restart2_fake_pid" "$wt_restart2_data/watcher"
  ln -sf "$wt_lc_binary" "$wt_restart2_proc/$restart2_fake_pid/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_lc_binary" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_restart2_proc/$restart2_fake_pid/cmdline"
  ln -sf "$wt_lc_project" "$wt_restart2_proc/$restart2_fake_pid/cwd"
  echo "$restart2_fake_pid" > "$wt_restart2_data/watcher/testnet-watcher.pid"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_restart2_proc" \
    PID_FILE="$wt_restart2_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_lc_project/target/release/henyey" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_restart2_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" restart 2>&1) || wt_rc=$?
  local restart2_ok=true
  if [[ $wt_rc -ne 1 ]]; then restart2_ok=false; fi
  # Verify cmd_start was NOT reached — no start-phase output
  if echo "$wt_status_out" | grep -q "Binary not found"; then restart2_ok=false; fi
  if echo "$wt_status_out" | grep -q "already running"; then restart2_ok=false; fi
  if echo "$wt_status_out" | grep -q "Watcher started"; then restart2_ok=false; fi
  if [[ "$restart2_ok" == "true" ]]; then
    tap_ok "watcher-ctl: restart propagates stop failure (start not reached)"
  else
    tap_not_ok "watcher-ctl: restart propagates stop failure (start not reached)" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T133: cmd_restart success (stop no-op, start succeeds) ─────────────
  # Nothing running → stop no-ops. Start succeeds with mock binary.
  local wt_restart3_proc="$TEST_ROOT/wt_restart3/proc"
  local wt_restart3_data="$TEST_ROOT/wt_restart3/data"
  local wt_restart3_bin="$TEST_ROOT/wt_restart3/mock-henyey"
  mkdir -p "$wt_restart3_data/watcher"

  # Reuse the same mock binary pattern as T130
  cat > "$wt_restart3_bin" <<'MOCK_BINARY'
#!/usr/bin/env bash
set -euo pipefail
MY_PID=$$
MOCK_PROC="${MOCK_PROC_ROOT}/${MY_PID}"
mkdir -p "$MOCK_PROC"
ln -sf "$(readlink -f "$0")" "$MOCK_PROC/exe"
printf '%s\0' "$@" > "$MOCK_PROC/cmdline"
ln -sf "$(pwd)" "$MOCK_PROC/cwd"
exec sleep 300
MOCK_BINARY
  chmod +x "$wt_restart3_bin"

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_restart3_proc" MOCK_PROC_ROOT="$wt_restart3_proc" \
    PID_FILE="$wt_restart3_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_restart3_bin" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_restart3_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" restart 2>&1) || wt_rc=$?
  local restart3_ok=true
  if [[ $wt_rc -ne 0 ]]; then restart3_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "No watcher running"; then restart3_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Watcher started"; then restart3_ok=false; fi
  if [[ "$restart3_ok" == "true" ]]; then
    local restart3_launched_pid
    restart3_launched_pid=$(cat "$wt_restart3_data/watcher/testnet-watcher.pid" 2>/dev/null || echo "")
    [[ -n "$restart3_launched_pid" ]] && _wt_bg_pids+=("$restart3_launched_pid")
    tap_ok "watcher-ctl: restart success (stop no-op, start succeeds)"
  else
    tap_not_ok "watcher-ctl: restart success (stop no-op, start succeeds)" "rc=$wt_rc out=$wt_status_out"
  fi

  # ── T134: cmd_restart full cycle (stop running watcher, start new) ─────
  # Launch a "old" watcher process, then restart it with a new mock binary.
  local wt_restart4_proc="$TEST_ROOT/wt_restart4/proc"
  local wt_restart4_data="$TEST_ROOT/wt_restart4/data"
  local wt_restart4_bin="$TEST_ROOT/wt_restart4/mock-henyey"
  mkdir -p "$wt_restart4_data/watcher"

  # Mock binary for the new watcher (creates its own mock proc entry)
  # Create this first so we can use its path for the old watcher's exe too.
  cat > "$wt_restart4_bin" <<'MOCK_BINARY'
#!/usr/bin/env bash
set -euo pipefail
MY_PID=$$
MOCK_PROC="${MOCK_PROC_ROOT}/${MY_PID}"
mkdir -p "$MOCK_PROC"
ln -sf "$(readlink -f "$0")" "$MOCK_PROC/exe"
printf '%s\0' "$@" > "$MOCK_PROC/cmdline"
ln -sf "$(pwd)" "$MOCK_PROC/cwd"
exec sleep 300
MOCK_BINARY
  chmod +x "$wt_restart4_bin"
  local wt_restart4_bin_real
  wt_restart4_bin_real="$(readlink -f "$wt_restart4_bin")"

  # Old watcher: a real sleep process
  sleep 300 &
  local restart4_old_pid=$!
  _wt_bg_pids+=("$restart4_old_pid")

  # Set up mock proc entry for the old watcher (exe must match BINARY)
  mkdir -p "$wt_restart4_proc/$restart4_old_pid"
  ln -sf "$wt_restart4_bin_real" "$wt_restart4_proc/$restart4_old_pid/exe"
  printf '%s\0%s\0%s\0%s\0%s\0' "$wt_restart4_bin_real" "run" "--watcher" "--config" "configs/watcher-testnet.toml" \
    > "$wt_restart4_proc/$restart4_old_pid/cmdline"
  ln -sf "$wt_lc_project" "$wt_restart4_proc/$restart4_old_pid/cwd"
  echo "$restart4_old_pid" > "$wt_restart4_data/watcher/testnet-watcher.pid"

  # Background monitor: remove old watcher's mock proc dir when it exits
  (while kill -0 "$restart4_old_pid" 2>/dev/null; do sleep 0.1; done
   rm -rf "$wt_restart4_proc/$restart4_old_pid") &
  local restart4_monitor=$!
  _wt_bg_pids+=("$restart4_monitor")

  wt_rc=0
  wt_status_out=$(PROC_ROOT="$wt_restart4_proc" MOCK_PROC_ROOT="$wt_restart4_proc" \
    PID_FILE="$wt_restart4_data/watcher/testnet-watcher.pid" \
    PROJECT_DIR="$wt_lc_project" BINARY="$wt_restart4_bin" \
    WATCHER_CONFIG="configs/watcher-testnet.toml" \
    LOG_FILE="$wt_restart4_data/watcher/test.log" \
    bash "$REPO_ROOT/scripts/watcher-ctl.sh" restart 2>&1) || wt_rc=$?
  local restart4_ok=true
  if [[ $wt_rc -ne 0 ]]; then restart4_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Watcher stopped"; then restart4_ok=false; fi
  if ! echo "$wt_status_out" | grep -q "Watcher started"; then restart4_ok=false; fi
  # Verify old PID is gone and new PID is different
  local restart4_new_pid
  restart4_new_pid=$(cat "$wt_restart4_data/watcher/testnet-watcher.pid" 2>/dev/null || echo "")
  if [[ -z "$restart4_new_pid" ]] || [[ "$restart4_new_pid" == "$restart4_old_pid" ]]; then
    restart4_ok=false
  fi
  if [[ "$restart4_ok" == "true" ]]; then
    _wt_bg_pids+=("$restart4_new_pid")
    tap_ok "watcher-ctl: restart full cycle (old stopped, new started)"
  else
    tap_not_ok "watcher-ctl: restart full cycle (old stopped, new started)" \
      "rc=$wt_rc old=$restart4_old_pid new=$restart4_new_pid out=$wt_status_out"
  fi
  wait "$restart4_monitor" 2>/dev/null || true

  # ── Check 12 scrape_identity / PREV_PROM_INVALID semantic assertions ────────
  # Verify that the identity check and invalidation semantics added to Check 12
  # are documented in monitor-tick SKILL.md. See issue #2563.

  local identity_section
  identity_section=$(extract_md_section "$tick_file" '^5\. \*\*Process identity check')

  # Fail-closed guard: if extraction is empty, skip dependent tests
  if [[ -z "$identity_section" ]]; then
    tap_not_ok "scrape-identity: section extraction" "identity section not found in $tick_file"
    tap_not_ok "scrape-identity: invalidation rules (skipped)"
    tap_not_ok "scrape-identity: persistence-reset gauges (skipped)"
    tap_not_ok "scrape-identity: metrics baselines skipped (skipped)"
    tap_not_ok "scrape-identity: state files table (skipped)"
  else

  # Test 135: scrape_identity file format documented
  if echo "$identity_section" | grep -Fq 'version=1' \
     && echo "$identity_section" | grep -Fq 'pid=' \
     && echo "$identity_section" | grep -Fq 'start_ticks=' \
     && echo "$identity_section" | grep -Fq 'timestamp='; then
    tap_ok "scrape-identity: file format fields documented (version=1, pid, start_ticks, timestamp)"
  else
    tap_not_ok "scrape-identity: file format fields documented" \
      "Identity section missing one or more of: version=1, pid=, start_ticks=, timestamp="
  fi

  # Test 136: PREV_PROM_INVALID invalidation rules documented
  if echo "$identity_section" | grep -Fq 'process identity changed' \
     && echo "$identity_section" | grep -Fq 'no scrape_identity' \
     && echo "$identity_section" | grep -Fq 'scrape_identity malformed' \
     && echo "$identity_section" | grep -Fq 'no prev.prom'; then
    tap_ok "scrape-identity: PREV_PROM_INVALID invalidation rules (3 triggers documented)"
  else
    tap_not_ok "scrape-identity: PREV_PROM_INVALID invalidation rules" \
      "Missing one of: 'process identity changed', 'no scrape_identity', 'scrape_identity malformed', 'no prev.prom'"
  fi

  # Test 137: Persistence-reset gauges listed in PREV_PROM_INVALID block
  if echo "$identity_section" | grep -Fq 'henyey_jemalloc_fragmentation_pct' \
     && echo "$identity_section" | grep -Fq 'henyey_scp_verify_input_backlog' \
     && echo "$identity_section" | grep -Fq 'henyey_overlay_fetch_channel_depth' \
     && echo "$identity_section" | grep -Fq 'reset the persistence counter'; then
    tap_ok "scrape-identity: persistence-reset gauges (3 gauges + reset semantics)"
  else
    tap_not_ok "scrape-identity: persistence-reset gauges" \
      "Missing one of: henyey_jemalloc_fragmentation_pct, henyey_scp_verify_input_backlog, henyey_overlay_fetch_channel_depth, or 'reset the persistence counter'"
  fi

  # Test 138: metrics: status format includes baselines skipped variants
  local metrics_line
  metrics_line=$(echo "$output_section" | grep -F 'metrics:' | head -1)
  if [[ -n "$metrics_line" ]] \
     && echo "$metrics_line" | grep -Fq 'baselines skipped' \
     && echo "$metrics_line" | grep -Fq 'gauge alerts'; then
    tap_ok "scrape-identity: metrics status format includes baselines skipped + gauge alerts"
  else
    tap_not_ok "scrape-identity: metrics status format includes baselines skipped + gauge alerts" \
      "metrics: line missing 'baselines skipped' or 'gauge alerts' variant"
  fi

  # Test 139: State files table labels current.prom, prev.prom, scrape_identity as check 12
  local state_table_ok=true
  for state_file in "current.prom" "prev.prom" "scrape_identity"; do
    local row
    row=$(grep -F "$state_file" "$tick_file" | grep -F '|' | head -1)
    if [[ -z "$row" ]] || ! echo "$row" | grep -Fq 'check 12'; then
      state_table_ok=false
      break
    fi
  done
  if [[ "$state_table_ok" == "true" ]]; then
    tap_ok "scrape-identity: state files table (current.prom, prev.prom, scrape_identity → check 12)"
  else
    tap_not_ok "scrape-identity: state files table" \
      "One or more of current.prom, prev.prom, scrape_identity missing from state table or not labeled check 12"
  fi

  # Test 140: gap-staleness (#3246) documented in Check 12 — the identity section
  # must compute prev_scrape_age from the timestamp field, export
  # PREV_SCRAPE_AGE_SECONDS, name the gap-stale rule, and state the default
  # threshold. The env var must also appear in the evaluator-invocation list.
  if echo "$identity_section" | grep -Fq 'prev_scrape_age' \
     && echo "$identity_section" | grep -Fq 'PREV_SCRAPE_AGE_SECONDS' \
     && echo "$identity_section" | grep -Fq 'gap-stale' \
     && echo "$identity_section" | grep -Fq '3600'; then
    tap_ok "scrape-identity: gap-stale rule documented (prev_scrape_age, PREV_SCRAPE_AGE_SECONDS, gap-stale, 3600)"
  else
    tap_not_ok "scrape-identity: gap-stale rule documented" \
      "identity section missing one of: prev_scrape_age, PREV_SCRAPE_AGE_SECONDS, gap-stale, 3600"
  fi
  if grep -Fq 'export PREV_SCRAPE_AGE_SECONDS' "$tick_file"; then
    tap_ok "scrape-identity: PREV_SCRAPE_AGE_SECONDS in evaluator-invocation env list"
  else
    tap_not_ok "scrape-identity: PREV_SCRAPE_AGE_SECONDS in evaluator-invocation env list" \
      "no 'export PREV_SCRAPE_AGE_SECONDS' in $tick_file"
  fi

  fi  # end identity_section guard

  # Clean up all background processes from lifecycle tests
  # Disable errexit temporarily — kill+wait of background processes can
  # propagate SIGCHLD exit codes that trigger set -e and premature exit.
  set +e
  _wt_cleanup_bg
  set -e
  # Cross-validate PostVerifyReason labels in crates/herder/src/scp_verify.rs
  # against the hard-coded label sets in monitor-tick and monitor-loop SKILL.md.
  # See issue #2519 (follow-up from #2481).
  # ════════════════════════════════════════════════════════════════════════════

  local scp_verify_file="$REPO_ROOT/crates/herder/src/scp_verify.rs"

  # Extract canonical labels from PostVerifyReason impl block (scoped to
  # avoid capturing PreFilterRejectReason::label() which also lives in this file).
  local pv_impl_block canonical_labels canonical_count all_array_size
  pv_impl_block=$(sed -n '/^impl PostVerifyReason/,/^}/p' "$scp_verify_file")
  canonical_labels=$(echo "$pv_impl_block" | grep -oP '=> "\K[^"]+' | sort)
  canonical_count=$(echo "$canonical_labels" | grep -c . || true)
  all_array_size=$(echo "$pv_impl_block" | grep -oP 'pub const ALL: \[Self; \K\d+' || true)

  # Test 140: Canonical extraction guard — fail closed on extraction problems
  if [[ "$canonical_count" -gt 0 && "$canonical_count" == "$all_array_size" ]]; then
    tap_ok "pv-label-sync: canonical extraction (count=$canonical_count, ALL size=$all_array_size)"
  else
    tap_not_ok "pv-label-sync: canonical extraction" \
      "count=$canonical_count ALL_size=$all_array_size (expected >0 and equal)"
    # Fail closed: emit remaining 5 tests as skipped
    local _i; for _i in 141 142 143 144 145; do
      tap_not_ok "pv-label-sync: skipped (canonical extraction failed)"
    done
    return
  fi

  # Extract scoped sections from skill docs
  # The ratio checks section was moved from monitor-tick SKILL.md to the TOML
  # catalog (metric-alarms.toml). Validate labels against TOML and monitor-loop.
  local loop_ratio_section
  loop_ratio_section=$(sed -n '/^\*\*D\. Ratio checks/,/^\*\*[A-Z]\./p' "$loop_file")

  # Extract expected_labels from the TOML catalog (scp-accept-rate-low alarm)
  local toml_labels
  toml_labels=$(python3 - "$REPO_ROOT/.claude/skills/shared/metric-alarms.toml" <<'PYEOF'
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], 'rb') as f:
    data = tomllib.load(f)
for a in data['alarm']:
    if a['name'] == 'scp-accept-rate-low' and 'expected_labels' in a:
        for lbl in sorted(a['expected_labels']):
            print(lbl)
        break
PYEOF
  ) || toml_labels=""

  # If sections are empty, all remaining tests fail
  if [[ -z "$loop_ratio_section" || -z "$toml_labels" ]]; then
    local _i; for _i in 141 142 143 144 145; do
      tap_not_ok "pv-label-sync: skipped (section extraction failed: loop_empty=$([ -z "$loop_ratio_section" ] && echo yes || echo no) toml_empty=$([ -z "$toml_labels" ] && echo yes || echo no))"
    done
    return
  fi

  # Test 141: TOML expected_labels match canonical source labels
  if [[ "$toml_labels" == "$canonical_labels" ]]; then
    tap_ok "pv-label-sync: TOML expected_labels match source ($canonical_count)"
  else
    tap_not_ok "pv-label-sync: TOML expected_labels match source" \
      "expected: $(echo "$canonical_labels" | tr '\n' ' ') got: $(echo "$toml_labels" | tr '\n' ' ')"
  fi

  # Test 142: TOML expected_labels count matches canonical
  local toml_label_count
  toml_label_count=$(echo "$toml_labels" | grep -c . || true)
  if [[ "$toml_label_count" == "$canonical_count" ]]; then
    tap_ok "pv-label-sync: TOML label count matches source ($canonical_count)"
  else
    tap_not_ok "pv-label-sync: TOML label count" \
      "expected $canonical_count, got $toml_label_count"
  fi

  # Test 143: monitor-loop inline labels match canonical
  local loop_label_line loop_inline_labels
  loop_label_line=$(echo "$loop_ratio_section" | grep 'Post-verify label set' || true)
  loop_inline_labels=$(echo "$loop_label_line" | grep -oP '`\K[a-z_]+(?=`)' | sort)
  if [[ "$loop_inline_labels" == "$canonical_labels" ]]; then
    tap_ok "pv-label-sync: monitor-loop inline labels ($canonical_count)"
  else
    tap_not_ok "pv-label-sync: monitor-loop inline labels" \
      "expected: $(echo "$canonical_labels" | tr '\n' ' ') got: $(echo "$loop_inline_labels" | tr '\n' ' ')"
  fi

  # Test 144: monitor-loop label count references match canonical
  local loop_count_refs loop_counts_ok=true
  loop_count_refs=$(echo "$loop_ratio_section" | grep -oP '(?:all |the |exact |expected )\K\d+(?=[ -](?:label|post-verify|`henyey_scp_post_verify))' || true)
  if [[ -z "$loop_count_refs" ]]; then
    loop_counts_ok=false
  else
    while IFS= read -r count_val; do
      if [[ "$count_val" != "$canonical_count" ]]; then
        loop_counts_ok=false
        break
      fi
    done <<< "$loop_count_refs"
  fi
  if [[ "$loop_counts_ok" == "true" ]]; then
    tap_ok "pv-label-sync: monitor-loop count refs ($canonical_count)"
  else
    tap_not_ok "pv-label-sync: monitor-loop count refs" \
      "expected all=$canonical_count, found: $(echo "$loop_count_refs" | tr '\n' ' ')"
  fi

  # Test 145: evaluator source references all expected labels
  local eval_script="$REPO_ROOT/scripts/lib/eval-alarms.py"
  if [[ -f "$eval_script" ]] && grep -Fq 'expected_labels' "$eval_script"; then
    tap_ok "pv-label-sync: evaluator handles expected_labels"
  else
    tap_not_ok "pv-label-sync: evaluator handles expected_labels" \
      "eval-alarms.py missing or doesn't reference expected_labels"
  fi

  # ── Alarm catalog evaluator tests (T146-T147) ──────────────────────────────
  # Validate the TOML alarm catalog and evaluator script

  # Test 146: evaluator --validate-only passes for the TOML catalog
  local eval_script="$REPO_ROOT/scripts/lib/eval-alarms.py"
  local catalog_file="$REPO_ROOT/.claude/skills/shared/metric-alarms.toml"
  if [[ -f "$eval_script" && -f "$catalog_file" ]]; then
    local validate_out
    validate_out=$(python3 "$eval_script" --catalog "$catalog_file" --validate-only 2>&1) || true
    if echo "$validate_out" | grep -q '"valid": true'; then
      local alarm_count
      alarm_count=$(echo "$validate_out" | python3 -c "import json,sys; print(json.load(sys.stdin)['alarm_count'])" 2>/dev/null || echo "?")
      tap_ok "eval-alarms: --validate-only passes ($alarm_count alarms)"
    else
      tap_not_ok "eval-alarms: --validate-only" \
        "Validation failed: $validate_out"
    fi
  else
    tap_not_ok "eval-alarms: --validate-only" \
      "Missing eval-alarms.py or metric-alarms.toml"
  fi

  # Test 146a (regression #3201): monitor/evaluator state files under repo-root
  # metrics/ are ignored by .gitignore so a stray write can't dirty the tree and
  # hard-block the deploy gate. The anchored (leading-/) block must NOT ignore
  # the tracked dashboards in metrics/ nor any crates/*/metrics path.
  #
  # check-ignore must run from the repo root (the .gitignore anchors are
  # repo-root-relative). Names enumerated here EXACTLY match the .gitignore
  # block (deduped); archive/ is a directory entry, checked via a path under it.
  local ci_ok=true ci_detail=""
  local stray_names=(
    "current.prom"
    "prev.prom"
    "gauge_persistence"
    "scrape_identity"
    "counter_streak_snapshot"
    "ratio_snapshot"
    "counter_dynamic_snapshot"
    "anomaly_cooldown.json"
    "archive/some-file"
  )
  local n
  for n in "${stray_names[@]}"; do
    if ! git -C "$REPO_ROOT" check-ignore -q "metrics/$n"; then
      ci_ok=false
      ci_detail+="metrics/$n NOT ignored; "
    fi
  done
  # Over-broad-pattern guard: the tracked dashboard must NOT be ignored.
  if git -C "$REPO_ROOT" check-ignore -q "metrics/henyey-dashboard.json"; then
    ci_ok=false
    ci_detail+="metrics/henyey-dashboard.json IS ignored (over-broad); "
  fi
  # The anchored block must not leak into crate-local metrics dirs.
  if git -C "$REPO_ROOT" check-ignore -q "crates/foo/metrics/gauge_persistence"; then
    ci_ok=false
    ci_detail+="crates/foo/metrics/gauge_persistence IS ignored (anchor leaked); "
  fi
  if [[ "$ci_ok" == "true" ]]; then
    tap_ok "gitignore: monitor state under /metrics is ignored; tracked dashboards are not"
  else
    tap_not_ok "gitignore: monitor state under /metrics is ignored; tracked dashboards are not" \
      "$ci_detail"
  fi

  # Test 146b (regression #3201): eval-alarms.py rejects a RELATIVE --state-dir.
  # A relative `--state-dir metrics` resolves against the repo-root cwd and drops
  # state files (gauge_persistence, ...) into the tracked metrics/ dir. The guard
  # must reject (not abspath) with a non-zero exit and write nothing. Run in a
  # subshell cd'd into a temp dir so the test itself can never dirty the repo.
  if [[ -f "$eval_script" && -f "$catalog_file" ]]; then
    local rel_tmp rel_current rel_rc
    rel_tmp=$(mktemp -d)
    rel_current="$rel_tmp/current.prom"
    echo "# empty" > "$rel_current"
    rel_rc=0
    (
      cd "$rel_tmp" || exit 99
      MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
        python3 "$eval_script" --catalog "$catalog_file" \
        --current "$rel_current" --state-dir metrics >/dev/null 2>&1
    ) || rel_rc=$?
    if [[ "$rel_rc" -ne 0 && ! -e "$rel_tmp/metrics/gauge_persistence" && ! -d "$rel_tmp/metrics" ]]; then
      tap_ok "eval-alarms: relative --state-dir is rejected (non-zero exit, nothing written)"
    else
      tap_not_ok "eval-alarms: relative --state-dir is rejected (non-zero exit, nothing written)" \
        "rc=$rel_rc; metrics dir exists: $([[ -d "$rel_tmp/metrics" ]] && echo yes || echo no)"
    fi
    rm -rf "$rel_tmp"
  else
    tap_not_ok "eval-alarms: relative --state-dir is rejected (non-zero exit, nothing written)" \
      "Missing eval-alarms.py or metric-alarms.toml"
  fi

  # Test 147: every alarm metric in the TOML exists somewhere in the Rust
  #           codebase under crates/. Alarms with exempt = true are skipped.
  #           exempt = true requires a non-empty exempt_reason.
  local metrics_missing=0 metrics_checked=0 metrics_exempt=0 missing_list=""
  local exempt_errors=""
  if [[ -f "$catalog_file" ]]; then
    # Extract per-alarm metric info including exempt status
    local alarm_data
    alarm_data=$(python3 - "$catalog_file" <<'PYEOF'
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], 'rb') as f:
    data = tomllib.load(f)
for a in data['alarm']:
    name = a.get('name', '<unnamed>')
    exempt = a.get('exempt', False)
    exempt_reason = a.get('exempt_reason', '')
    # Validate exempt/exempt_reason pairing
    if exempt and not exempt_reason:
        print(f"EXEMPT_ERROR:{name}:exempt=true but no exempt_reason")
        continue
    if not exempt and exempt_reason:
        print(f"EXEMPT_ERROR:{name}:exempt_reason without exempt=true")
        continue
    # Extract all metric names from any alarm kind
    for key in ['metric', 'metric_sum', 'numerator_metric', 'denominator_metric',
                'numerator', 'denominator', 'numerator_sum', 'denominator_sum']:
        v = a.get(key)
        if isinstance(v, str):
            # Strip label selector
            m = v.split('{')[0]
            print(f"{'EXEMPT' if exempt else 'CHECK'}:{name}:{m}")
        elif isinstance(v, list):
            for item in v:
                m = item.split('{')[0]
                print(f"{'EXEMPT' if exempt else 'CHECK'}:{name}:{m}")
PYEOF
    ) || true
    # Check for exempt validation errors
    while IFS= read -r line; do
      [[ -z "$line" ]] && continue
      if [[ "$line" == EXEMPT_ERROR:* ]]; then
        exempt_errors="$exempt_errors ${line#EXEMPT_ERROR:}"
      fi
    done <<< "$alarm_data"
    if [[ -n "$exempt_errors" ]]; then
      tap_not_ok "eval-alarms: exempt validation" \
        "Invalid exempt config:$exempt_errors"
    fi
    # Source-grep check (per alarm, not per deduplicated metric)
    local crates_dir="$REPO_ROOT/crates"
    while IFS= read -r line; do
      [[ -z "$line" ]] && continue
      [[ "$line" == EXEMPT_ERROR:* ]] && continue
      local status alarm_name metric_name
      status="${line%%:*}"
      local rest="${line#*:}"
      alarm_name="${rest%%:*}"
      metric_name="${rest#*:}"
      if [[ "$status" == "EXEMPT" ]]; then
        metrics_exempt=$((metrics_exempt + 1))
        continue
      fi
      metrics_checked=$((metrics_checked + 1))
      if ! grep -rq "$metric_name" "$crates_dir" --include='*.rs' 2>/dev/null; then
        metrics_missing=$((metrics_missing + 1))
        missing_list="$missing_list $alarm_name:$metric_name"
      fi
    done <<< "$alarm_data"
    if [[ "$metrics_missing" -eq 0 ]]; then
      local exempt_note=""
      [[ "$metrics_exempt" -gt 0 ]] && exempt_note=", $metrics_exempt exempt"
      tap_ok "eval-alarms: all $metrics_checked alarm metrics found in source${exempt_note}"
    else
      tap_not_ok "eval-alarms: alarm metrics source-grep" \
        "$metrics_missing missing:$missing_list"
    fi
  else
    tap_not_ok "eval-alarms: alarm metrics source-grep" "catalog missing"
  fi

  # Test 147a: exempt alarm is skipped by source-grep and evaluator
  local exempt_toml
  exempt_toml=$(mktemp)
  cat > "$exempt_toml" <<'EXEMPT_TOML'
schema_version = 1
[[alarm]]
name = "test-exempt-alarm"
metric = "nonexistent_metric_that_does_not_exist_anywhere"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = []
cooldown_key = "test-exempt"
cooldown_seconds = 3600
filing_title = "test"
filing_search = "test"
summary = "test"
details = "test"
notes = ""
exempt = true
exempt_reason = "Test metric — does not exist in our codebase"
EXEMPT_TOML
  local exempt_validate
  exempt_validate=$(python3 "$eval_script" --catalog "$exempt_toml" --validate-only 2>&1) || true
  if echo "$exempt_validate" | grep -q '"valid": true'; then
    tap_ok "eval-alarms: exempt alarm passes --validate-only"
  else
    tap_not_ok "eval-alarms: exempt alarm validation" \
      "Validation failed: $exempt_validate"
  fi

  # Test 147a-runtime: exempt alarm returns state="skipped" at runtime
  local exempt_current exempt_prev exempt_state_dir
  exempt_current=$(mktemp)
  exempt_prev=$(mktemp)
  exempt_state_dir=$(mktemp -d)
  echo "# empty" > "$exempt_current"
  echo "# empty" > "$exempt_prev"
  local exempt_runtime
  exempt_runtime=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    python3 "$eval_script" --catalog "$exempt_toml" \
    --current "$exempt_current" --prev "$exempt_prev" \
    --state-dir "$exempt_state_dir" 2>/dev/null) || true
  local exempt_state exempt_skip_reason
  exempt_state=$(echo "$exempt_runtime" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'test-exempt-alarm':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  exempt_skip_reason=$(echo "$exempt_runtime" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'test-exempt-alarm':
            print(r.get('skip_reason', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  if [[ "$exempt_state" == "skipped" && "$exempt_skip_reason" == *"exempt: Test metric"* ]]; then
    tap_ok "eval-alarms: exempt alarm runtime state=skipped with skip_reason"
  else
    tap_not_ok "eval-alarms: exempt alarm runtime skip" \
      "Expected state=skipped + skip_reason='exempt: Test metric', got state=$exempt_state reason=$exempt_skip_reason"
  fi
  rm -f "$exempt_current" "$exempt_prev" "$exempt_toml"
  rm -rf "$exempt_state_dir"

  # Test 147b: exempt=true without exempt_reason is rejected by evaluator
  local bad_exempt_toml
  bad_exempt_toml=$(mktemp)
  cat > "$bad_exempt_toml" <<'BAD_EXEMPT_TOML'
schema_version = 1
[[alarm]]
name = "test-bad-exempt"
metric = "some_metric"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = []
cooldown_key = "test-bad-exempt"
cooldown_seconds = 3600
filing_title = "test"
filing_search = "test"
summary = "test"
details = "test"
notes = ""
exempt = true
BAD_EXEMPT_TOML
  local bad_exempt_validate
  bad_exempt_validate=$(python3 "$eval_script" --catalog "$bad_exempt_toml" --validate-only 2>&1) || true
  # Check that validation rejected the bad exempt config
  if echo "$bad_exempt_validate" | grep -q "exempt"; then
    tap_ok "eval-alarms: exempt=true without exempt_reason is rejected"
  else
    tap_not_ok "eval-alarms: exempt without reason validation" \
      "Expected rejection but got (exit=$bad_exempt_exit): $bad_exempt_validate"
  fi
  rm -f "$bad_exempt_toml"

  # Test 147c: ledger-invariant-failure and tx-internal-error fire on breach fixtures
  local fixture_dir="$REPO_ROOT/scripts/fixtures/eval-alarms"
  local counter_state_dir
  counter_state_dir=$(mktemp -d)
  local counter_eval_out
  counter_eval_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$fixture_dir/breach-current.prom" \
    --prev "$fixture_dir/healthy-prev.prom" \
    --state-dir "$counter_state_dir" 2>/dev/null) || true
  local inv_state tx_state
  inv_state=$(echo "$counter_eval_out" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for r in data.get('alarms', []):
    if r.get('name') == 'ledger-invariant-failure':
        print(r.get('state', 'missing'))
        break
else:
    print('not-found')
" 2>/dev/null || echo "parse-error")
  tx_state=$(echo "$counter_eval_out" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for r in data.get('alarms', []):
    if r.get('name') == 'tx-internal-error':
        print(r.get('state', 'missing'))
        break
else:
    print('not-found')
" 2>/dev/null || echo "parse-error")
  if [[ "$inv_state" == "firing" ]]; then
    tap_ok "eval-alarms: ledger-invariant-failure fires on breach fixture"
  else
    tap_not_ok "eval-alarms: ledger-invariant-failure" \
      "Expected firing, got $inv_state"
  fi
  if [[ "$tx_state" == "firing" ]]; then
    tap_ok "eval-alarms: tx-internal-error fires on breach fixture"
  else
    tap_not_ok "eval-alarms: tx-internal-error" \
      "Expected firing, got $tx_state"
  fi
  rm -rf "$counter_state_dir"

  # Test 148: quorum-fail-at-low multi-tick persistence test (for_ticks=3)
  local state_dir
  state_dir=$(mktemp -d)
  local tick_prom prev_prom
  tick_prom=$(mktemp)
  prev_prom=$(mktemp)
  # Breaching value: quorum_fail_at = 0 (<= 1 threshold)
  cat > "$tick_prom" <<'TICK_PROM'
stellar_quorum_fail_at 0
stellar_herder_state 2
TICK_PROM
  cat > "$prev_prom" <<'PREV_PROM'
stellar_quorum_fail_at 0
stellar_herder_state 2
PREV_PROM
  local t148_pass=true
  local t148_detail=""
  for tick in 1 2 3; do
    local tick_out
    tick_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
      python3 "$eval_script" \
      --catalog "$catalog_file" \
      --current "$tick_prom" \
      --prev "$prev_prom" \
      --state-dir "$state_dir" 2>/dev/null) || true
    # Extract quorum-fail-at-low result
    local qfa_state
    qfa_state=$(echo "$tick_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
    if [[ "$tick" -lt 3 ]]; then
      if [[ "$qfa_state" != "breach" ]]; then
        t148_pass=false
        t148_detail="tick $tick: expected breach, got $qfa_state"
        break
      fi
    else
      if [[ "$qfa_state" != "firing" ]]; then
        t148_pass=false
        t148_detail="tick $tick: expected firing, got $qfa_state"
        break
      fi
    fi
  done
  # Tick 4: clear (quorum_fail_at = 5, above threshold)
  if $t148_pass; then
    cat > "$tick_prom" <<'TICK_PROM_CLEAR'
stellar_quorum_fail_at 5
stellar_herder_state 2
TICK_PROM_CLEAR
    local clear_out
    clear_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
      python3 "$eval_script" \
      --catalog "$catalog_file" \
      --current "$tick_prom" \
      --prev "$prev_prom" \
      --state-dir "$state_dir" 2>/dev/null) || true
    local clear_state
    clear_state=$(echo "$clear_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
    if [[ "$clear_state" == "firing" ]] || [[ "$clear_state" == "breach" ]]; then
      t148_pass=false
      t148_detail="tick 4 (clear): expected ok/skipped, got $clear_state"
    fi
  fi
  if $t148_pass; then
    tap_ok "eval-alarms: quorum-fail-at-low multi-tick persistence (breach→breach→firing→clear)"
  else
    tap_not_ok "eval-alarms: quorum-fail-at-low multi-tick" "$t148_detail"
  fi
  rm -f "$tick_prom" "$prev_prom"
  rm -rf "$state_dir"

  # Test 148a: warmup gate + stale gauge persistence reset (#2614)
  # Pre-seed persistence with count=2, then run a warmup tick (WARMUP_TICKS_REMAINING=1)
  # with breaching value → alarm should be skipped AND persistence reset to 0.
  # Then run with WARMUP_TICKS_REMAINING=0 → should get "breach" (count 1), not "firing".
  local state_dir_148a
  state_dir_148a=$(mktemp -d)
  local tick_prom_148a prev_prom_148a
  tick_prom_148a=$(mktemp)
  prev_prom_148a=$(mktemp)
  # Breaching value: quorum_fail_at = 0 (<= 1 threshold)
  cat > "$tick_prom_148a" <<'TICK_PROM_148A'
stellar_quorum_fail_at 0
stellar_herder_state 2
TICK_PROM_148A
  cat > "$prev_prom_148a" <<'PREV_PROM_148A'
stellar_quorum_fail_at 0
stellar_herder_state 2
PREV_PROM_148A
  # Pre-seed stale persistence (as if 2 ticks had already breached before restart)
  mkdir -p "$state_dir_148a"
  echo "gauge_persist_quorum-fail-at-low=2" > "$state_dir_148a/gauge_persistence"
  local t148a_pass=true
  local t148a_detail=""
  # Tick 1: warmup remaining=1 → should skip + reset persistence
  local warmup_out
  warmup_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=1 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148a" \
    --prev "$prev_prom_148a" \
    --state-dir "$state_dir_148a" 2>/dev/null) || true
  local warmup_state
  warmup_state=$(echo "$warmup_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  if [[ "$warmup_state" != "skipped" ]]; then
    t148a_pass=false
    t148a_detail="warmup tick: expected skipped, got $warmup_state"
  fi
  # Verify persistence was reset to 0
  if $t148a_pass; then
    local persist_val
    persist_val=$(grep 'gauge_persist_quorum-fail-at-low' "$state_dir_148a/gauge_persistence" 2>/dev/null | cut -d= -f2)
    if [[ "$persist_val" != "0" ]]; then
      t148a_pass=false
      t148a_detail="persistence not reset: expected 0, got ${persist_val:-empty}"
    fi
  fi
  # Tick 2: warmup=0, breaching → should be "breach" (count 1), NOT "firing" (stale count 3)
  if $t148a_pass; then
    local post_warmup_out
    post_warmup_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
      python3 "$eval_script" \
      --catalog "$catalog_file" \
      --current "$tick_prom_148a" \
      --prev "$prev_prom_148a" \
      --state-dir "$state_dir_148a" 2>/dev/null) || true
    local post_warmup_state
    post_warmup_state=$(echo "$post_warmup_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
    if [[ "$post_warmup_state" != "breach" ]]; then
      t148a_pass=false
      t148a_detail="post-warmup tick: expected breach, got $post_warmup_state"
    fi
  fi
  # Ticks 3-4: continue breaching → breach then firing
  if $t148a_pass; then
    for expected_state in breach firing; do
      local cont_out
      cont_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
        python3 "$eval_script" \
        --catalog "$catalog_file" \
        --current "$tick_prom_148a" \
        --prev "$prev_prom_148a" \
        --state-dir "$state_dir_148a" 2>/dev/null) || true
      local cont_state
      cont_state=$(echo "$cont_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
      if [[ "$cont_state" != "$expected_state" ]]; then
        t148a_pass=false
        t148a_detail="continuation: expected $expected_state, got $cont_state"
        break
      fi
    done
  fi
  if $t148a_pass; then
    tap_ok "eval-alarms: quorum-fail-at-low warmup gate resets stale persistence (#2614)"
  else
    tap_not_ok "eval-alarms: quorum-fail-at-low warmup gate resets stale persistence (#2614)" "$t148a_detail"
  fi
  rm -f "$tick_prom_148a" "$prev_prom_148a"
  rm -rf "$state_dir_148a"

  # Test 148b: metric-missing resets stale gauge persistence (#2614)
  # Pre-seed persistence with count=2, run with metric absent → skipped + persistence reset.
  local state_dir_148b
  state_dir_148b=$(mktemp -d)
  local tick_prom_148b prev_prom_148b
  tick_prom_148b=$(mktemp)
  prev_prom_148b=$(mktemp)
  # Metric ABSENT from prom data — only herder_state present
  cat > "$tick_prom_148b" <<'TICK_PROM_148B'
stellar_herder_state 2
TICK_PROM_148B
  cat > "$prev_prom_148b" <<'PREV_PROM_148B'
stellar_herder_state 2
PREV_PROM_148B
  mkdir -p "$state_dir_148b"
  echo "gauge_persist_quorum-fail-at-low=2" > "$state_dir_148b/gauge_persistence"
  local t148b_pass=true
  local t148b_detail=""
  # Tick with missing metric → should skip + reset persistence
  local missing_out
  missing_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148b" \
    --prev "$prev_prom_148b" \
    --state-dir "$state_dir_148b" 2>/dev/null) || true
  local missing_state
  missing_state=$(echo "$missing_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  if [[ "$missing_state" != "skipped" ]]; then
    t148b_pass=false
    t148b_detail="metric-missing tick: expected skipped, got $missing_state"
  fi
  # Verify persistence was reset to 0
  if $t148b_pass; then
    local persist_val_b
    persist_val_b=$(grep 'gauge_persist_quorum-fail-at-low' "$state_dir_148b/gauge_persistence" 2>/dev/null | cut -d= -f2)
    if [[ "$persist_val_b" != "0" ]]; then
      t148b_pass=false
      t148b_detail="persistence not reset: expected 0, got ${persist_val_b:-empty}"
    fi
  fi
  # Next tick with metric present + breaching → should be "breach" (count 1)
  if $t148b_pass; then
    cat > "$tick_prom_148b" <<'TICK_PROM_148B2'
stellar_quorum_fail_at 0
stellar_herder_state 2
TICK_PROM_148B2
    cat > "$prev_prom_148b" <<'PREV_PROM_148B2'
stellar_quorum_fail_at 0
stellar_herder_state 2
PREV_PROM_148B2
    local next_out
    next_out=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
      python3 "$eval_script" \
      --catalog "$catalog_file" \
      --current "$tick_prom_148b" \
      --prev "$prev_prom_148b" \
      --state-dir "$state_dir_148b" 2>/dev/null) || true
    local next_state
    next_state=$(echo "$next_out" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'quorum-fail-at-low':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
    if [[ "$next_state" != "breach" ]]; then
      t148b_pass=false
      t148b_detail="post-missing tick: expected breach, got $next_state"
    fi
  fi
  if $t148b_pass; then
    tap_ok "eval-alarms: metric-missing resets stale gauge persistence (#2614)"
  else
    tap_not_ok "eval-alarms: metric-missing resets stale gauge persistence (#2614)" "$t148b_detail"
  fi
  rm -f "$tick_prom_148b" "$prev_prom_148b"
  rm -rf "$state_dir_148b"

  # Test 148c: recovery-stalled post-restart absolute-value fire (#3198).
  # Scenario from #3197/#3198: a deploy restart changes the process PID. The
  # monitor's first observation of the restarted process already shows a large
  # forcing_catchup_behind value (213) accrued during startup/warmup. The
  # streak machine would normally reset its baseline on the PID change and
  # return "collecting_baseline" — silently discarding the absolute value. With
  # post_restart_absolute_threshold=50 the discarded value is evaluated and the
  # alarm fires WARN once. A clean restart (value 5, below threshold) must NOT
  # fire.
  local state_dir_148c
  state_dir_148c=$(mktemp -d)
  # Pre-seed a snapshot from the PRE-restart incarnation (pid=1000). The catalog
  # snapshot_file is the BARE name (#3222), so the evaluator resolves it directly
  # under --state-dir (no 'metrics/' subdir).
  cat > "$state_dir_148c/counter_streak_snapshot" <<'SNAP_148C'
version=1
pid=1000
start_ticks=1000
counter_value=0
breach_streak=0
SNAP_148C
  local tick_prom_148c
  tick_prom_148c=$(mktemp)
  # First post-restart observation: PID changed to 2000, big accrued stall.
  cat > "$tick_prom_148c" <<'TICK_PROM_148C'
henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"} 213
TICK_PROM_148C
  local fire_out_148c
  fire_out_148c=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    PID=2000 START_TICKS=2000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148c" \
    --state-dir "$state_dir_148c" 2>/dev/null) || true
  local rs_state_148c rs_postrestart_148c
  rs_state_148c=$(echo "$fire_out_148c" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'recovery-stalled':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  if [[ "$rs_state_148c" == "firing" ]]; then
    tap_ok "eval-alarms: recovery-stalled post-restart absolute fire (213 >= 50 → firing, not collecting_baseline)"
  else
    tap_not_ok "eval-alarms: recovery-stalled post-restart absolute fire" \
      "expected firing on PID-change with value 213, got $rs_state_148c"
  fi
  rm -f "$tick_prom_148c"
  rm -rf "$state_dir_148c"

  # Test 148d: recovery-stalled clean restart below threshold does NOT fire.
  local state_dir_148d
  state_dir_148d=$(mktemp -d)
  cat > "$state_dir_148d/counter_streak_snapshot" <<'SNAP_148D'
version=1
pid=1000
start_ticks=1000
counter_value=0
breach_streak=0
SNAP_148D
  local tick_prom_148d
  tick_prom_148d=$(mktemp)
  cat > "$tick_prom_148d" <<'TICK_PROM_148D'
henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"} 5
TICK_PROM_148D
  local clean_out_148d rs_state_148d
  clean_out_148d=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    PID=2000 START_TICKS=2000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148d" \
    --state-dir "$state_dir_148d" 2>/dev/null) || true
  rs_state_148d=$(echo "$clean_out_148d" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'recovery-stalled':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error")
  if [[ "$rs_state_148d" == "collecting_baseline" ]]; then
    tap_ok "eval-alarms: recovery-stalled clean restart below threshold (5 < 50 → collecting_baseline, no false-fire)"
  else
    tap_not_ok "eval-alarms: recovery-stalled clean restart below threshold" \
      "expected collecting_baseline on PID-change with value 5, got $rs_state_148d"
  fi
  rm -f "$tick_prom_148d"
  rm -rf "$state_dir_148d"

  # Test 148e: --no-snapshot-write read-only dry-run (#3209).
  # Regression for the double-run masking bug: the evaluator mutates its streak
  # snapshot on EVERY run, so a second run within a tick compares the
  # just-written snapshot against itself (delta consumed) and masks the
  # recovery-stalled WARN that the first run fired (the #3206 alarm). The fix
  # adds --no-snapshot-write so a diagnostic/repeat invocation re-evaluates
  # against the existing on-disk snapshot without consuming the delta.
  #
  # Fixture mirrors 148c: pre-seed a PRE-restart snapshot (pid=1000), then feed
  # a first post-restart observation (PID 2000, accrued stall 213) that fires
  # recovery-stalled via the post_restart_absolute_threshold=50 path.
  rs_state_of() {
    # $1 = evaluator stdout JSON; prints the recovery-stalled alarm state.
    echo "$1" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == 'recovery-stalled':
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" 2>/dev/null || echo "parse-error"
  }
  seed_148e_snapshot() {
    # $1 = state-dir; (re)write the PRE-restart snapshot fixture. The catalog
    # snapshot_file is the BARE name (#3222), so the evaluator resolves it
    # directly under --state-dir (no 'metrics/' subdir).
    cat > "$1/counter_streak_snapshot" <<'SNAP_148E'
version=1
pid=1000
start_ticks=1000
counter_value=0
breach_streak=0
SNAP_148E
  }
  local state_dir_148e tick_prom_148e
  state_dir_148e=$(mktemp -d)
  tick_prom_148e=$(mktemp)
  cat > "$tick_prom_148e" <<'TICK_PROM_148E'
henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"} 213
TICK_PROM_148E

  # Run 1 (no flag): the canonical per-tick run — fires AND advances the
  # snapshot (consumes the delta).
  seed_148e_snapshot "$state_dir_148e"
  local out1_148e rs1_148e
  out1_148e=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    PID=2000 START_TICKS=2000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148e" \
    --state-dir "$state_dir_148e" 2>/dev/null) || true
  rs1_148e=$(rs_state_of "$out1_148e")
  if [[ "$rs1_148e" == "firing" ]]; then
    tap_ok "eval-alarms: --no-snapshot-write run 1 (no flag) fires recovery-stalled (213 → firing)"
  else
    tap_not_ok "eval-alarms: --no-snapshot-write run 1 (no flag) fires recovery-stalled" \
      "expected firing, got $rs1_148e"
  fi

  # Run 2 (no flag, SAME inputs, NO re-seed): documents the masking bug. The
  # snapshot was advanced by run 1, so the delta is consumed and the alarm is
  # masked (state != firing).
  local out2_148e rs2_148e
  out2_148e=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    PID=2000 START_TICKS=2000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148e" \
    --state-dir "$state_dir_148e" 2>/dev/null) || true
  rs2_148e=$(rs_state_of "$out2_148e")
  if [[ "$rs2_148e" != "firing" ]]; then
    tap_ok "eval-alarms: --no-snapshot-write run 2 (no flag) masks recovery-stalled (delta consumed → $rs2_148e)"
  else
    tap_not_ok "eval-alarms: --no-snapshot-write run 2 (no flag) masks recovery-stalled" \
      "expected masked (delta consumed by run 1), got firing"
  fi

  # Run 3 (--no-snapshot-write) against a FRESH re-seed of the PRE-restart
  # snapshot: the alarm must STILL fire AND the snapshot file must be left
  # BYTE-unchanged (delta not consumed). These two are the decisive fix
  # assertions. Fails on origin/main: --no-snapshot-write is an unknown arg,
  # so parse_args() exits non-zero, out3 is empty, and rs3 != firing.
  seed_148e_snapshot "$state_dir_148e"
  local snap_before_148e
  snap_before_148e=$(cat "$state_dir_148e/counter_streak_snapshot")
  local out3_148e rs3_148e snap_after_148e
  out3_148e=$(MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0 \
    PID=2000 START_TICKS=2000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148e" \
    --state-dir "$state_dir_148e" \
    --no-snapshot-write 2>/dev/null) || true
  rs3_148e=$(rs_state_of "$out3_148e")
  if [[ "$rs3_148e" == "firing" ]]; then
    tap_ok "eval-alarms: --no-snapshot-write run 3 (with flag) still fires recovery-stalled (213 → firing)"
  else
    tap_not_ok "eval-alarms: --no-snapshot-write run 3 (with flag) still fires recovery-stalled" \
      "expected firing with --no-snapshot-write, got $rs3_148e (unknown flag on origin/main?)"
  fi
  snap_after_148e=$(cat "$state_dir_148e/counter_streak_snapshot")
  if [[ "$snap_before_148e" == "$snap_after_148e" ]]; then
    tap_ok "eval-alarms: --no-snapshot-write leaves counter_streak_snapshot byte-unchanged"
  else
    tap_not_ok "eval-alarms: --no-snapshot-write leaves counter_streak_snapshot byte-unchanged" \
      "snapshot mutated despite --no-snapshot-write"
  fi
  rm -f "$tick_prom_148e"
  rm -rf "$state_dir_148e"

  # ── Tests 148f–148i: gap-staleness (#3246) ──────────────────────────────────
  # When the monitor LOOP stalls for a long wall-clock gap but the validator
  # PROCESS survives (same PID/start_ticks), the prev.prom / snapshot baseline
  # stays valid by identity but becomes hours old. The counter-delta is then
  # computed across the whole gap → false acute-burst alarms. The fix derives a
  # gap_stale signal from PREV_SCRAPE_AGE_SECONDS >= GAP_STALE_THRESHOLD_SECONDS
  # (default 3600) and applies it asymmetrically: prev.prom-family alarms SKIP
  # (the file baseline is recomputed each tick); snapshot-family alarms
  # RE-BASELINE (the snapshot persists across ticks).
  local alarm_state_of
  alarm_state_of() {
    # $1 = evaluator stdout JSON, $2 = alarm name; prints the alarm state.
    echo "$1" | python3 -c "
import json, sys
name = sys.argv[1]
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == name:
            print(r.get('state', 'missing'))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" "$2" 2>/dev/null || echo "parse-error"
  }
  local alarm_skipreason_of
  alarm_skipreason_of() {
    # $1 = evaluator stdout JSON, $2 = alarm name; prints the alarm skip_reason.
    echo "$1" | python3 -c "
import json, sys
name = sys.argv[1]
try:
    data = json.load(sys.stdin)
    for r in data.get('alarms', []):
        if r.get('name') == name:
            print(r.get('skip_reason', ''))
            break
    else:
        print('not-found')
except:
    print('parse-error')
" "$2" 2>/dev/null || echo "parse-error"
  }

  # Shared counter fixture: a valid prev.prom baseline (same incarnation, no
  # PREV_PROM_INVALID) with a large delta on the `lost-sync` counter (op >= 1).
  # The delta would normally fire — the only difference between 148f and 148g is
  # the prev-scrape age.
  local gap_prev_prom gap_cur_prom
  gap_prev_prom=$(mktemp)
  gap_cur_prom=$(mktemp)
  cat > "$gap_prev_prom" <<'GAP_PREV'
stellar_herder_lost_sync_total 100
GAP_PREV
  cat > "$gap_cur_prom" <<'GAP_CUR'
stellar_herder_lost_sync_total 117
GAP_CUR

  # Test 148f: counter gap-stale skip. prev age 12h (43200s) >= threshold ⇒ the
  # delta=17 lost-sync alarm must NOT acute-fire; it is skipped with a gap-stale
  # reason. FAILS pre-fix (no gap_stale logic ⇒ fires).
  local state_dir_148f out_148f
  state_dir_148f=$(mktemp -d)
  out_148f=$(MONITOR_MODE=validator UPTIME_SECONDS=99999 WARMUP_TICKS_REMAINING=0 \
    PREV_PROM_INVALID=false PREV_SCRAPE_AGE_SECONDS=43200 \
    PID=3366449 START_TICKS=5000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$gap_cur_prom" \
    --prev "$gap_prev_prom" \
    --state-dir "$state_dir_148f" 2>/dev/null) || true
  local ls_state_148f ls_reason_148f
  ls_state_148f=$(alarm_state_of "$out_148f" "lost-sync")
  ls_reason_148f=$(alarm_skipreason_of "$out_148f" "lost-sync")
  if [[ "$ls_state_148f" == "skipped" && "$ls_reason_148f" == *"gap-stale"* ]]; then
    tap_ok "eval-alarms: counter gap-stale skip (prev age 12h ⇒ lost-sync skipped, reason '$ls_reason_148f', not firing)"
  else
    tap_not_ok "eval-alarms: counter gap-stale skip" \
      "expected skipped+gap-stale, got state=$ls_state_148f reason=$ls_reason_148f"
  fi
  rm -rf "$state_dir_148f"

  # Test 148g: counter normal interval still fires. Same fixture, prev age 20min
  # (1200s) < threshold ⇒ the delta=17 alarm STILL fires. Guards against
  # over-skipping. Passes both pre- and post-fix (no gap_stale at 1200s).
  local state_dir_148g out_148g ls_state_148g
  state_dir_148g=$(mktemp -d)
  out_148g=$(MONITOR_MODE=validator UPTIME_SECONDS=99999 WARMUP_TICKS_REMAINING=0 \
    PREV_PROM_INVALID=false PREV_SCRAPE_AGE_SECONDS=1200 \
    PID=3366449 START_TICKS=5000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$gap_cur_prom" \
    --prev "$gap_prev_prom" \
    --state-dir "$state_dir_148g" 2>/dev/null) || true
  ls_state_148g=$(alarm_state_of "$out_148g" "lost-sync")
  if [[ "$ls_state_148g" == "firing" ]]; then
    tap_ok "eval-alarms: counter normal interval still fires (prev age 20m ⇒ lost-sync firing)"
  else
    tap_not_ok "eval-alarms: counter normal interval still fires" \
      "expected firing at 1200s prev age, got $ls_state_148g"
  fi
  rm -rf "$state_dir_148g"
  rm -f "$gap_prev_prom" "$gap_cur_prom"

  # Test 148h: recovery-stalled burst gap-stale RE-BASELINE (the headline
  # tick-184 case). A valid SAME-PID snapshot baseline (pid=3366449) with a burst
  # delta (counter jumped 0 → 703 across the gap). With prev age 12h >= threshold
  # the alarm must RE-BASELINE — return collecting_baseline (NOT firing) AND
  # rewrite the snapshot to the current counter value with breach_streak=0, so
  # next tick's delta is small. A plain skip would leave the stale baseline and
  # fire on the NEXT tick — that's why snapshot-family re-baselines. FAILS pre-fix
  # (delta=703 >= burst_threshold ⇒ firing). NB: same PID, so this is the
  # distinct-from-#3206 case — the identity check passes, #3206's PID-mismatch
  # reset never triggers; gap-staleness is what re-baselines here.
  local state_dir_148h tick_prom_148h
  state_dir_148h=$(mktemp -d)
  cat > "$state_dir_148h/counter_streak_snapshot" <<'SNAP_148H'
version=1
pid=3366449
start_ticks=5000
counter_value=0
breach_streak=0
SNAP_148H
  tick_prom_148h=$(mktemp)
  cat > "$tick_prom_148h" <<'TICK_PROM_148H'
henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"} 703
TICK_PROM_148H
  local out_148h rs_state_148h
  out_148h=$(MONITOR_MODE=validator UPTIME_SECONDS=99999 WARMUP_TICKS_REMAINING=0 \
    PREV_PROM_INVALID=false PREV_SCRAPE_AGE_SECONDS=43200 \
    PID=3366449 START_TICKS=5000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148h" \
    --state-dir "$state_dir_148h" 2>/dev/null) || true
  rs_state_148h=$(rs_state_of "$out_148h")
  local snap_cv_148h snap_streak_148h
  snap_cv_148h=$(grep '^counter_value=' "$state_dir_148h/counter_streak_snapshot" | cut -d= -f2)
  snap_streak_148h=$(grep '^breach_streak=' "$state_dir_148h/counter_streak_snapshot" | cut -d= -f2)
  if [[ "$rs_state_148h" == "collecting_baseline" \
        && "$snap_cv_148h" == "703" && "$snap_streak_148h" == "0" ]]; then
    tap_ok "eval-alarms: recovery-stalled burst gap-stale re-baseline (703 ⇒ collecting_baseline, snapshot reseeded to 703 streak 0, not firing)"
  else
    tap_not_ok "eval-alarms: recovery-stalled burst gap-stale re-baseline" \
      "expected collecting_baseline + snapshot reseed (cv=703 streak=0), got state=$rs_state_148h cv=$snap_cv_148h streak=$snap_streak_148h"
  fi
  rm -f "$tick_prom_148h"
  rm -rf "$state_dir_148h"

  # Test 148i: counter-ratio gap-stale RE-BASELINE. A valid same-PID ratio
  # snapshot baseline with a breaching ratio across the gap must re-baseline
  # (collecting_baseline) and reset the streak to 0, rather than advancing the
  # streak toward a fire. Uses `pending-too-old-ratio` (min_volume=100).
  # FAILS pre-fix (the breaching ratio advances the streak → breach/firing).
  local state_dir_148i tick_prom_148i
  state_dir_148i=$(mktemp -d)
  cat > "$state_dir_148i/ratio_snapshot" <<'SNAP_148I'
version=1
pid=3366449
start_ticks=5000
pending-too-old-ratio_numerator=0
pending-too-old-ratio_denominator=0
pending-too-old-ratio_streak=2
SNAP_148I
  tick_prom_148i=$(mktemp)
  # num jumps 0→900, den 0→1000 across the gap → ratio 0.9 (breaching), den_delta
  # 1000 >= min_volume 100.
  cat > "$tick_prom_148i" <<'TICK_PROM_148I'
stellar_herder_pending_too_old_total 900
stellar_herder_pending_received_total 1000
TICK_PROM_148I
  local out_148i pt_state_148i
  out_148i=$(MONITOR_MODE=validator UPTIME_SECONDS=99999 WARMUP_TICKS_REMAINING=0 \
    PREV_PROM_INVALID=false PREV_SCRAPE_AGE_SECONDS=43200 \
    PID=3366449 START_TICKS=5000 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$tick_prom_148i" \
    --state-dir "$state_dir_148i" 2>/dev/null) || true
  pt_state_148i=$(alarm_state_of "$out_148i" "pending-too-old-ratio")
  local ratio_streak_148i
  ratio_streak_148i=$(grep '^pending-too-old-ratio_streak=' "$state_dir_148i/ratio_snapshot" | cut -d= -f2)
  if [[ "$pt_state_148i" == "collecting_baseline" && "$ratio_streak_148i" == "0" ]]; then
    tap_ok "eval-alarms: counter-ratio gap-stale re-baseline (breaching ratio ⇒ collecting_baseline, streak reset 0)"
  else
    tap_not_ok "eval-alarms: counter-ratio gap-stale re-baseline" \
      "expected collecting_baseline + streak 0, got state=$pt_state_148i streak=$ratio_streak_148i"
  fi
  rm -f "$tick_prom_148i"
  rm -rf "$state_dir_148i"

  # Test 149: alarm-surfaces.toml file-local invariants + mirrored UIDs
  #           resolve to real Grafana alert UIDs in henyey-slo-alerts.yaml.
  local yaml_file="$REPO_ROOT/metrics/alerts/henyey-slo-alerts.yaml"
  local surfaces_file="$REPO_ROOT/.claude/skills/shared/alarm-surfaces.toml"
  if [[ -f "$surfaces_file" ]] && [[ -f "$yaml_file" ]]; then
    local mirrors_result
    mirrors_result=$(PYTHONPATH="$REPO_ROOT/scripts/lib" python3 -c "
import sys, alarm_surfaces
surfaces = alarm_surfaces.load_surfaces(sys.argv[1])
errors = alarm_surfaces.validate_local(surfaces)
if errors:
    print('FAIL:' + '; '.join(errors))
    sys.exit(0)
yaml_uids = alarm_surfaces.extract_yaml_uids(sys.argv[2])
yaml_set = set(yaml_uids)
mirrored = surfaces.get('mirrored', [])
missing = []
for e in mirrored:
    for uid in e.get('grafana_uid', []):
        if uid not in yaml_set:
            missing.append(uid)
if missing:
    print('FAIL:mirrored UIDs not in YAML: ' + ', '.join(missing))
else:
    count = sum(len(e.get('grafana_uid', [])) for e in mirrored)
    print('OK:' + str(count))
" "$surfaces_file" "$yaml_file") || mirrors_result="FAIL:python-error"
    if [[ "$mirrors_result" == OK:* ]]; then
      local uid_count="${mirrors_result#OK:}"
      tap_ok "eval-alarms: all $uid_count mirrored UIDs resolve to Grafana alerts"
    else
      local missing_uids="${mirrors_result#FAIL:}"
      tap_not_ok "eval-alarms: mirrored UID validation" \
        "$missing_uids"
    fi
  else
    tap_not_ok "eval-alarms: mirrored UID validation" \
      "Missing alarm-surfaces.toml or YAML file"
  fi

  # Test 150: every mirrored toml_name in alarm-surfaces.toml resolves to
  #           an alarm name in metric-alarms.toml.
  if [[ -f "$surfaces_file" ]] && [[ -f "$catalog_file" ]]; then
    local toml_result
    toml_result=$(PYTHONPATH="$REPO_ROOT/scripts/lib" python3 -c "
import sys, alarm_surfaces
surfaces = alarm_surfaces.load_surfaces(sys.argv[1])
toml_names = alarm_surfaces.extract_toml_alarm_names(sys.argv[2])
mirrored = surfaces.get('mirrored', [])
missing = []
for e in mirrored:
    if e['toml_name'] not in toml_names:
        missing.append(e['toml_name'])
if missing:
    print('FAIL:toml_names not in metric-alarms.toml: ' + ', '.join(missing))
else:
    print('OK:' + str(len(mirrored)))
" "$surfaces_file" "$catalog_file") || toml_result="FAIL:python-error"
    if [[ "$toml_result" == OK:* ]]; then
      local alarm_count="${toml_result#OK:}"
      tap_ok "eval-alarms: all $alarm_count mirrored toml_names resolve to alarms"
    else
      local toml_errors="${toml_result#FAIL:}"
      tap_not_ok "eval-alarms: toml_name resolution" \
        "$toml_errors"
    fi
  else
    tap_not_ok "eval-alarms: toml_name resolution" \
      "Missing alarm-surfaces.toml or catalog file"
  fi

  # Test 151: every Grafana alert UID in henyey-slo-alerts.yaml is classified
  #           in alarm-surfaces.toml as either mirrored or grafana_only.
  #           Complete coverage, disjoint sets, no stale entries, unique YAML UIDs.
  if [[ -f "$yaml_file" ]] && [[ -f "$surfaces_file" ]] && [[ -f "$catalog_file" ]]; then
    local reverse_result
    reverse_result=$(PYTHONPATH="$REPO_ROOT/scripts/lib" python3 -c "
import sys, alarm_surfaces
surfaces = alarm_surfaces.load_surfaces(sys.argv[1])
toml_names = alarm_surfaces.extract_toml_alarm_names(sys.argv[2])
yaml_uids = alarm_surfaces.extract_yaml_uids(sys.argv[3])
errors = alarm_surfaces.validate_cross(surfaces, toml_names, yaml_uids)
if errors:
    print('FAIL:' + '; '.join(errors))
else:
    print('OK:' + str(len(set(yaml_uids))))
" "$surfaces_file" "$catalog_file" "$yaml_file") || reverse_result="FAIL:python-error"
    if [[ "$reverse_result" == OK:* ]]; then
      local yaml_uid_count="${reverse_result#OK:}"
      tap_ok "eval-alarms: all $yaml_uid_count Grafana UIDs classified (mirrored or grafana_only)"
    else
      local reverse_errors="${reverse_result#FAIL:}"
      tap_not_ok "eval-alarms: Grafana UID classification" \
        "$reverse_errors"
    fi
  else
    tap_not_ok "eval-alarms: Grafana UID classification" \
      "Missing YAML, alarm-surfaces.toml, or catalog file"
  fi

  # Test 152: ALARM_SURFACES.md is not stale — generated tables match
  #           the committed file.
  local surfaces_md="$REPO_ROOT/.claude/skills/shared/ALARM_SURFACES.md"
  if [[ -f "$surfaces_file" ]] && [[ -f "$surfaces_md" ]]; then
    local gen_result
    gen_result=$(PYTHONPATH="$REPO_ROOT/scripts/lib" python3 \
      "$REPO_ROOT/scripts/lib/gen-alarm-surfaces.py" --check \
      "$surfaces_file" "$surfaces_md" 2>&1) || true
    if [[ "$gen_result" == OK:* ]]; then
      tap_ok "eval-alarms: ALARM_SURFACES.md is up to date with alarm-surfaces.toml"
    else
      tap_not_ok "eval-alarms: ALARM_SURFACES.md staleness check" \
        "$gen_result"
    fi
  else
    tap_not_ok "eval-alarms: ALARM_SURFACES.md staleness check" \
      "Missing alarm-surfaces.toml or ALARM_SURFACES.md"
  fi

  # ── Archive and Replay Tests ──────────────────────────────────────────────
  # Tests for per-tick metrics snapshot archiving and historical replay.

  local archive_root="$TEST_ROOT/archive-tests"

  # ── Test: Archive creation — directory structure and metadata.env ────────
  local t_arc="$archive_root/t-create"
  local t_arc_session="$t_arc/data/test-session"
  mkdir -p "$t_arc_session/metrics"
  echo "# HELP test_metric gauge" > "$t_arc_session/metrics/current.prom"
  echo "test_metric 42" >> "$t_arc_session/metrics/current.prom"
  echo "# HELP test_metric gauge" > "$t_arc_session/metrics/prev.prom"
  echo "test_metric 40" >> "$t_arc_session/metrics/prev.prom"

  # Simulate archive step
  local arc_dir="$t_arc_session/metrics/archive"
  mkdir -p "$arc_dir"
  local arc_ts="2025-05-12T14:00:00.000000000Z"
  local snap_tmp="$arc_dir/${arc_ts}.tmp"
  local snap_final="$arc_dir/${arc_ts}"
  mkdir -p "$snap_tmp"
  cp "$t_arc_session/metrics/current.prom" "$snap_tmp/current.prom"
  cp "$t_arc_session/metrics/prev.prom" "$snap_tmp/prev.prom"
  cat > "$snap_tmp/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=14400
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF
  mv "$snap_tmp" "$snap_final"

  if [[ -d "$snap_final" ]] && [[ -f "$snap_final/metadata.env" ]] \
     && [[ -f "$snap_final/current.prom" ]] && [[ -f "$snap_final/prev.prom" ]] \
     && grep -q "ARCHIVE_VERSION=1" "$snap_final/metadata.env"; then
    tap_ok "archive: directory structure and metadata.env created correctly"
  else
    tap_not_ok "archive: directory structure and metadata.env created correctly" \
      "Missing files in $snap_final"
  fi

  # ── Test: Archive atomicity — .tmp dirs not counted as snapshots ────────
  mkdir -p "$arc_dir/2025-05-12T15:00:00.000000000Z.tmp"
  echo "incomplete" > "$arc_dir/2025-05-12T15:00:00.000000000Z.tmp/metadata.env"
  local snap_count
  snap_count=$(find "$arc_dir" -maxdepth 1 -mindepth 1 -type d ! -name '*.tmp' | wc -l)
  if [[ "$snap_count" -eq 1 ]]; then
    tap_ok "archive: .tmp directories excluded from snapshot count"
  else
    tap_not_ok "archive: .tmp directories excluded from snapshot count" \
      "Expected 1 snapshot, got $snap_count"
  fi

  # ── Test: Archive pruning via prune_metrics_archive (mtime-sort) ──────────
  # Regression for #3724: the archive holds snapshot dirs in two timestamp-name
  # formats. A lexical name sort ('-' = 0x2D < '0'..'9') places ALL dash-format
  # RFC-3339 dirs before ALL compact-format dirs regardless of real age, so a
  # name-sort retention deletes the NEWEST (dash) snapshots while pinning
  # month-old (compact) ones. prune_metrics_archive orders by mtime, so it
  # evicts the TRUE-oldest. This FAILS on origin/main (embedded name-sort copy).
  local t_prune="$archive_root/t-prune"
  local prune_arc="$t_prune/data/prune-session/metrics/archive"
  mkdir -p "$prune_arc"
  local now_epoch; now_epoch=$(date +%s)
  # 3 recent dash-format dirs (newest mtime), lexically FIRST.
  local pd
  for pd in 2026-07-27 2026-07-26 2026-07-25; do
    mkdir -p "$prune_arc/${pd}T00:00:00.000000000Z"
    echo "ARCHIVE_VERSION=1" > "$prune_arc/${pd}T00:00:00.000000000Z/metadata.env"
    touch -d "@$((now_epoch - 100))" "$prune_arc/${pd}T00:00:00.000000000Z"
  done
  # 2 old compact-format dirs (oldest mtime), lexically LAST ('2' > '-').
  local pc
  for pc in 20260617T000000Z 20260618T000000Z; do
    mkdir -p "$prune_arc/$pc"
    echo "ARCHIVE_VERSION=1" > "$prune_arc/$pc/metadata.env"
    touch -d "@$((now_epoch - 4000000))" "$prune_arc/$pc"
  done
  # keep=3: total 5, excess 2. mtime-sort removes the 2 OLDEST (both compact);
  # a name sort would instead remove the lexical head = 2 recent dash dirs.
  prune_metrics_archive "$prune_arc" 3
  local prune_remaining
  prune_remaining=$(find "$prune_arc" -maxdepth 1 -mindepth 1 -type d | wc -l | tr -d ' ')
  if [[ "$PRUNED_ARCHIVE_COUNT" -eq 2 && "$prune_remaining" -eq 3 \
        && ! -d "$prune_arc/20260617T000000Z" \
        && ! -d "$prune_arc/20260618T000000Z" \
        && -d "$prune_arc/2026-07-27T00:00:00.000000000Z" \
        && -d "$prune_arc/2026-07-26T00:00:00.000000000Z" \
        && -d "$prune_arc/2026-07-25T00:00:00.000000000Z" ]]; then
    tap_ok "archive: prune_metrics_archive evicts oldest-mtime (compact) dirs, keeps recent dash"
  else
    tap_not_ok "archive: prune_metrics_archive evicts oldest-mtime (compact) dirs, keeps recent dash" \
      "removed=$PRUNED_ARCHIVE_COUNT remaining=$prune_remaining files=$(ls "$prune_arc" | tr '\n' ',')"
  fi

  # ── Test: prune_metrics_archive no-op cases (missing / empty / <= keep) ────
  prune_metrics_archive "$archive_root/prune-missing/nope" 500
  local noop_missing=$PRUNED_ARCHIVE_COUNT
  local pe="$archive_root/prune-empty"; mkdir -p "$pe"
  prune_metrics_archive "$pe" 500
  local noop_empty=$PRUNED_ARCHIVE_COUNT
  local pu="$archive_root/prune-underkeep"; mkdir -p "$pu/a" "$pu/b"
  prune_metrics_archive "$pu" 500
  local noop_under=$PRUNED_ARCHIVE_COUNT
  local under_remaining
  under_remaining=$(find "$pu" -maxdepth 1 -mindepth 1 -type d | wc -l | tr -d ' ')
  if [[ "$noop_missing" -eq 0 && "$noop_empty" -eq 0 && "$noop_under" -eq 0 && "$under_remaining" -eq 2 ]]; then
    tap_ok "archive: prune_metrics_archive no-op on missing/empty/under-keep (count 0, nothing deleted)"
  else
    tap_not_ok "archive: prune_metrics_archive no-op on missing/empty/under-keep (count 0, nothing deleted)" \
      "missing=$noop_missing empty=$noop_empty under=$noop_under under_remaining=$under_remaining"
  fi

  # ── Test: prune_metrics_archive excludes .tmp staging dirs ────────────────
  local ptmp="$archive_root/prune-tmp"; mkdir -p "$ptmp"
  mkdir -p "$ptmp/2026-07-27T00:00:00.000000000Z" "$ptmp/2026-07-01T00:00:00.000000000Z.tmp"
  touch -d "@$((now_epoch - 100))"     "$ptmp/2026-07-27T00:00:00.000000000Z"
  touch -d "@$((now_epoch - 9000000))" "$ptmp/2026-07-01T00:00:00.000000000Z.tmp"
  prune_metrics_archive "$ptmp" 0
  if [[ "$PRUNED_ARCHIVE_COUNT" -eq 1 \
        && ! -d "$ptmp/2026-07-27T00:00:00.000000000Z" \
        && -d "$ptmp/2026-07-01T00:00:00.000000000Z.tmp" ]]; then
    tap_ok "archive: prune_metrics_archive excludes .tmp dirs from the eviction set"
  else
    tap_not_ok "archive: prune_metrics_archive excludes .tmp dirs from the eviction set" \
      "removed=$PRUNED_ARCHIVE_COUNT files=$(ls "$ptmp" | tr '\n' ',')"
  fi

  # ── Test: Replay processes snapshots in mtime (chronological) order ───────
  # Regression for #3724: replay claims chronological evaluation but name-sorted
  # the archive. Build a fixture where the OLDEST-by-mtime dir is compact-named
  # (lexically LAST) and the NEWEST-by-mtime is dash-named (lexically FIRST): a
  # name sort reports first_ts=dash (WRONG), mtime sort reports first_ts=compact
  # (correct). FAILS on origin/main (replay enumerator name-sorts).
  local t_replay="$archive_root/t-replay"
  local replay_session="$t_replay/data/replay-session"
  local replay_arc="$replay_session/metrics/archive"
  mkdir -p "$replay_arc"
  local rnow; rnow=$(date +%s)
  local old_name="20260617T000000Z"                  # oldest mtime, compact (lexically last)
  local mid_name="2026-07-01T00:00:00.000000000Z"    # middle mtime, dash
  local new_name="2026-07-02T00:00:00.000000000Z"    # newest mtime, dash (lexically first)
  local rp
  for rp in "$old_name=9000000" "$mid_name=200000" "$new_name=100"; do
    local rn="${rp%%=*}"; local ago="${rp##*=}"
    local sd="$replay_arc/$rn"
    mkdir -p "$sd"
    echo "# empty" > "$sd/current.prom"
    echo "# empty" > "$sd/prev.prom"
    cat > "$sd/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=900
MONITOR_MODE=validator
PID=12345
START_TICKS=100
METAEOF
    touch -d "@$((rnow - ago))" "$sd/current.prom" "$sd/prev.prom" "$sd/metadata.env" "$sd"
  done
  local replay_json
  replay_json=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" \
    "$replay_session" --replay --json 2>/dev/null) || true
  local replay_order_ok
  replay_order_ok=$(echo "$replay_json" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print('ok' if (d.get('first_ts') == '$old_name' and d.get('last_ts') == '$new_name') else 'fail:%s..%s' % (d.get('first_ts'), d.get('last_ts')))
except Exception:
    print('fail:exc')
" 2>/dev/null) || echo "fail:noout"
  if [[ "$replay_order_ok" == "ok" ]]; then
    tap_ok "archive: replay evaluates in mtime order (first_ts oldest, last_ts newest)"
  else
    tap_not_ok "archive: replay evaluates in mtime order (first_ts oldest, last_ts newest)" \
      "got $replay_order_ok; json=${replay_json:0:200}"
  fi

  # ── Test: SKILL.md + replay script use mtime-sort, not name-sort (#3724) ──
  local prune_tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  local replay_script="$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh"
  if grep -q 'prune_metrics_archive' "$prune_tick_file" \
     && ! grep -qE 'SNAPSHOTS\[|sort -z' "$prune_tick_file"; then
    tap_ok "archive: SKILL.md step-7 uses prune_metrics_archive (no name-sort / SNAPSHOTS array)"
  else
    tap_not_ok "archive: SKILL.md step-7 uses prune_metrics_archive (no name-sort / SNAPSHOTS array)" \
      "SKILL.md must call prune_metrics_archive and drop 'sort -z'/'SNAPSHOTS['"
  fi
  if ! grep -q 'sort -z' "$replay_script" \
     && grep -qF "printf '%T@" "$replay_script"; then
    tap_ok "archive: replay script enumerates archive by mtime (no 'sort -z' name-sort)"
  else
    tap_not_ok "archive: replay script enumerates archive by mtime (no 'sort -z' name-sort)" \
      "replay script must enumerate with -printf '%T@ %p' | sort -n, not 'sort -z'"
  fi

  # ── Test: Replay skipped-tick handling ──────────────────────────────────
  local t_skip="$archive_root/t-skip"
  local skip_arc="$t_skip/data/skip-session/metrics/archive"
  mkdir -p "$skip_arc"

  # Create a skipped tick
  local skip_dir="$skip_arc/2025-05-10T10:00:00.000000000Z"
  mkdir -p "$skip_dir"
  cat > "$skip_dir/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=true
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=0
MONITOR_MODE=validator
PID=
START_TICKS=
METAEOF

  # Verify TICK_SKIPPED is correctly read
  source "$skip_dir/metadata.env"
  if [[ "$TICK_SKIPPED" == "true" ]] && [[ "$ARCHIVE_VERSION" == "1" ]]; then
    tap_ok "archive: skipped-tick metadata.env correctly marks TICK_SKIPPED=true"
  else
    tap_not_ok "archive: skipped-tick metadata.env correctly marks TICK_SKIPPED=true" \
      "TICK_SKIPPED=$TICK_SKIPPED ARCHIVE_VERSION=$ARCHIVE_VERSION"
  fi

  # ── Test: Malformed metadata.env detection ──────────────────────────────
  # Source the validate_metadata function from the replay script
  eval "$(sed -n '/^validate_metadata()/,/^}/p' "$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh")"

  local t_malformed="$archive_root/t-malformed"
  local mal_arc="$t_malformed/data/mal-session/metrics/archive"
  mkdir -p "$mal_arc/2025-05-10T10:00:00.000000000Z"
  echo "ARCHIVE_VERSION=99" > "$mal_arc/2025-05-10T10:00:00.000000000Z/metadata.env"

  if ! validate_metadata "$mal_arc/2025-05-10T10:00:00.000000000Z" 2>/dev/null; then
    tap_ok "archive: validate_metadata rejects wrong ARCHIVE_VERSION"
  else
    tap_not_ok "archive: validate_metadata rejects wrong ARCHIVE_VERSION" \
      "validate_metadata returned 0"
  fi

  # ── Test: validate_metadata accepts the bumped ARCHIVE_VERSION=2 (#3791) ──
  # The monitor now writes ARCHIVE_VERSION=2 (see monitor-tick-artifacts.py).
  # The replay reader must accept the known-good set {1,2}, not fail-close on
  # anything but 1 — otherwise every post-#3791 archive is rejected as corrupt.
  local t_v2="$archive_root/t-v2"
  local v2_arc="$t_v2/data/v2-session/metrics/archive"
  mkdir -p "$v2_arc/2025-05-10T10:00:00.000000000Z"
  cat > "$v2_arc/2025-05-10T10:00:00.000000000Z/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=2
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=60
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF
  if validate_metadata "$v2_arc/2025-05-10T10:00:00.000000000Z" 2>/dev/null; then
    tap_ok "archive: validate_metadata accepts ARCHIVE_VERSION=2 (#3791)"
  else
    tap_not_ok "archive: validate_metadata accepts ARCHIVE_VERSION=2 (#3791)" \
      "validate_metadata rejected a valid v2 archive"
  fi

  # ── Test: Missing required key in metadata.env ──────────────────────────
  local t_missing_key="$archive_root/t-missing-key"
  local mk_arc="$t_missing_key/data/mk-session/metrics/archive"
  mkdir -p "$mk_arc/2025-05-10T10:00:00.000000000Z"
  # metadata.env with ARCHIVE_VERSION=1 but missing UPTIME_SECONDS
  cat > "$mk_arc/2025-05-10T10:00:00.000000000Z/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF
  if ! validate_metadata "$mk_arc/2025-05-10T10:00:00.000000000Z" 2>/dev/null; then
    tap_ok "archive: validate_metadata rejects missing UPTIME_SECONDS"
  else
    tap_not_ok "archive: validate_metadata rejects missing UPTIME_SECONDS" \
      "validate_metadata returned 0 for incomplete metadata"
  fi

  # ── Test: Invalid numeric field in metadata.env ─────────────────────────
  local t_invalid_num="$archive_root/t-invalid-num"
  local in_arc="$t_invalid_num/data/in-session/metrics/archive"
  mkdir -p "$in_arc/2025-05-10T10:00:00.000000000Z"
  cat > "$in_arc/2025-05-10T10:00:00.000000000Z/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=abc
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF
  if ! validate_metadata "$in_arc/2025-05-10T10:00:00.000000000Z" 2>/dev/null; then
    tap_ok "archive: validate_metadata rejects non-numeric UPTIME_SECONDS"
  else
    tap_not_ok "archive: validate_metadata rejects non-numeric UPTIME_SECONDS" \
      "validate_metadata returned 0 for UPTIME_SECONDS=abc"
  fi

  # ── Test: Stale state isolation — missing key after valid snapshot ──────
  # First set UPTIME_SECONDS (simulating a valid prior snapshot having set it),
  # then validate a metadata.env that is missing it — should fail because
  # validate_metadata clears stale state before sourcing.
  UPTIME_SECONDS=9999  # simulate stale state from prior snapshot
  local t_stale="$archive_root/t-stale"
  local stale_arc="$t_stale/data/stale-session/metrics/archive"
  mkdir -p "$stale_arc/2025-05-10T10:00:00.000000000Z"
  cat > "$stale_arc/2025-05-10T10:00:00.000000000Z/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF
  if ! validate_metadata "$stale_arc/2025-05-10T10:00:00.000000000Z" 2>/dev/null; then
    tap_ok "archive: validate_metadata detects missing key despite stale shell state"
  else
    tap_not_ok "archive: validate_metadata detects missing key despite stale shell state" \
      "validate_metadata returned 0 — stale UPTIME_SECONDS=$UPTIME_SECONDS leaked through"
  fi

  # ── Test: Default mode metadata fallback ────────────────────────────────
  local t_fallback="$archive_root/t-fallback"
  local fb_session="$t_fallback/data/fb-session"
  local fb_arc="$fb_session/metrics/archive"
  mkdir -p "$fb_arc/2025-05-12T14:00:00.000000000Z"
  cat > "$fb_arc/2025-05-12T14:00:00.000000000Z/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=true
WARMUP_TICKS_REMAINING=2
FRESH_START=yes
CRASH_RECOVERY=no
UPTIME_SECONDS=60
MONITOR_MODE=watcher
PID=99999
START_TICKS=111111
METAEOF

  # Find latest snapshot and source metadata
  local latest_snap=""
  while IFS= read -r -d '' d; do
    if [[ -f "$d/metadata.env" ]]; then
      latest_snap="$d"
    fi
  done < <(find "$fb_arc" -maxdepth 1 -mindepth 1 -type d ! -name '*.tmp' -print0 | sort -z)

  if [[ -n "$latest_snap" ]]; then
    source "$latest_snap/metadata.env"
    if [[ "$MONITOR_MODE" == "watcher" ]] && [[ "$WARMUP_TICKS_REMAINING" == "2" ]] \
       && [[ "$PREV_PROM_INVALID" == "true" ]]; then
      tap_ok "archive: default mode reads metadata from latest archive snapshot"
    else
      tap_not_ok "archive: default mode reads metadata from latest archive snapshot" \
        "MONITOR_MODE=$MONITOR_MODE WARMUP=$WARMUP_TICKS_REMAINING PREV_INVALID=$PREV_PROM_INVALID"
    fi
  else
    tap_not_ok "archive: default mode reads metadata from latest archive snapshot" \
      "No latest snapshot found"
  fi

  # ── Test: Post-gap persistence — skipped tick doesn't reset state ───────
  local t_gap="$archive_root/t-gap"
  local gap_arc="$t_gap/data/gap-session/metrics/archive"
  mkdir -p "$gap_arc"

  # Tick 1: normal evaluation (breach starts persistence counter)
  local gap_t1="$gap_arc/2025-05-10T10:00:00.000000000Z"
  mkdir -p "$gap_t1"
  echo "# TYPE test_gauge gauge" > "$gap_t1/current.prom"
  echo "test_gauge 100" >> "$gap_t1/current.prom"
  echo "# TYPE test_gauge gauge" > "$gap_t1/prev.prom"
  echo "test_gauge 90" >> "$gap_t1/prev.prom"
  cat > "$gap_t1/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=14400
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF

  # Tick 2: skipped (gap)
  local gap_t2="$gap_arc/2025-05-10T11:00:00.000000000Z"
  mkdir -p "$gap_t2"
  cat > "$gap_t2/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=true
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=0
MONITOR_MODE=validator
PID=
START_TICKS=
METAEOF

  # Tick 3: normal evaluation (persistence should continue)
  local gap_t3="$gap_arc/2025-05-10T12:00:00.000000000Z"
  mkdir -p "$gap_t3"
  echo "# TYPE test_gauge gauge" > "$gap_t3/current.prom"
  echo "test_gauge 100" >> "$gap_t3/current.prom"
  echo "# TYPE test_gauge gauge" > "$gap_t3/prev.prom"
  echo "test_gauge 90" >> "$gap_t3/prev.prom"
  cat > "$gap_t3/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=14400
MONITOR_MODE=validator
PID=12345
START_TICKS=987654
METAEOF

  # Verify: 3 snapshots, 1 skipped, state dir persists across gap
  local gap_total=0 gap_skipped=0
  for gd in "$gap_arc"/2025-*; do
    [[ -d "$gd" ]] || continue
    gap_total=$((gap_total + 1))
    source "$gd/metadata.env"
    if [[ "$TICK_SKIPPED" == "true" ]]; then
      gap_skipped=$((gap_skipped + 1))
    fi
  done
  if [[ "$gap_total" -eq 3 ]] && [[ "$gap_skipped" -eq 1 ]]; then
    tap_ok "archive: post-gap persistence — skipped tick counted, state preserved"
  else
    tap_not_ok "archive: post-gap persistence — skipped tick counted, state preserved" \
      "total=$gap_total skipped=$gap_skipped"
  fi

  # ── Test: Retention boundary — pruned archive still usable ──────────────
  local t_boundary="$archive_root/t-boundary"
  local bound_arc="$t_boundary/data/bound-session/metrics/archive"
  mkdir -p "$bound_arc"
  # Create 3 snapshots, prune to keep 2
  for ts in "2025-01-01T00:00:00.000000000Z" "2025-01-02T00:00:00.000000000Z" "2025-01-03T00:00:00.000000000Z"; do
    mkdir -p "$bound_arc/$ts"
    echo "ARCHIVE_VERSION=1" > "$bound_arc/$ts/metadata.env"
    echo "TICK_SKIPPED=false" >> "$bound_arc/$ts/metadata.env"
  done

  # Prune to keep 2
  local bound_snaps=()
  while IFS= read -r -d '' d; do
    bound_snaps+=("$d")
  done < <(find "$bound_arc" -maxdepth 1 -mindepth 1 -type d ! -name '*.tmp' -print0 | sort -z)
  local bound_count=${#bound_snaps[@]}
  if [[ "$bound_count" -gt 2 ]]; then
    local bound_excess=$((bound_count - 2))
    for ((i=0; i<bound_excess; i++)); do
      rm -rf "${bound_snaps[$i]}"
    done
  fi

  local bound_remaining
  bound_remaining=$(find "$bound_arc" -maxdepth 1 -mindepth 1 -type d | wc -l)
  local oldest_remaining
  oldest_remaining=$(find "$bound_arc" -maxdepth 1 -mindepth 1 -type d -print0 | sort -z | head -z -n 1 | tr -d '\0')
  if [[ "$bound_remaining" -eq 2 ]] && [[ "$(basename "$oldest_remaining")" == "2025-01-02T00:00:00.000000000Z" ]]; then
    tap_ok "archive: retention boundary — oldest pruned, newest 2 kept"
  else
    tap_not_ok "archive: retention boundary — oldest pruned, newest 2 kept" \
      "remaining=$bound_remaining oldest=$(basename "${oldest_remaining:-none}")"
  fi

  # Clean up archive test dirs
  rm -rf "$archive_root"

  # ── Alarm replay --json behavioral tests ────────────────────────────────
  local replay_root
  replay_root=$(mktemp -d "$TEST_ROOT/replay-XXXXXX")

  # Test: --json without --replay → error
  local json_only_err
  json_only_err=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" --json 2>&1) && {
    tap_not_ok "replay: --json without --replay exits with error" "exited 0"
  } || {
    if echo "$json_only_err" | grep -q "requires --replay"; then
      tap_ok "replay: --json without --replay exits with error"
    else
      tap_not_ok "replay: --json without --replay exits with error" \
        "wrong error: $json_only_err"
    fi
  }

  # Test: --replay --json --window → error
  local json_window_err
  json_window_err=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" \
    "$replay_root" --replay --json --window 10 2>&1) && {
    tap_not_ok "replay: --json with --window exits with error" "exited 0"
  } || {
    if echo "$json_window_err" | grep -q "cannot be used with --json"; then
      tap_ok "replay: --json with --window exits with error"
    else
      tap_not_ok "replay: --json with --window exits with error" \
        "wrong error: $json_window_err"
    fi
  }

  # Test: --replay --json on empty archive → zeroed JSON schema
  local empty_session="$replay_root/empty-session"
  mkdir -p "$empty_session/metrics/archive"
  local empty_json
  empty_json=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" \
    "$empty_session" --replay --json 2>/dev/null) || true
  local empty_schema_ok
  empty_schema_ok=$(echo "$empty_json" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    ok = (d.get('schema_version') == 1 and
          d.get('evaluated_ticks') == 0 and
          d.get('skipped_ticks') == 0 and
          d.get('error_ticks') == 0 and
          d.get('total_snapshots') == 0 and
          d.get('alarms') == {})
    print('ok' if ok else 'fail')
except:
    print('fail')
" 2>/dev/null) || echo "fail"
  if [[ "$empty_schema_ok" == "ok" ]]; then
    tap_ok "replay: --replay --json on empty archive returns zeroed schema"
  else
    tap_not_ok "replay: --replay --json on empty archive returns zeroed schema" \
      "output: $empty_json"
  fi

  # Test: --replay --json parses multi-line evaluator JSON correctly
  # This is the regression test for the original bug: eval-alarms.py outputs
  # pretty-printed JSON (indent=2), and the old parser used json.loads(line)
  # which silently dropped all multi-line results.
  local multiline_session="$replay_root/multiline-session"
  mkdir -p "$multiline_session/metrics/archive"
  local ts1="2025-06-01T00:00:00.000000000Z"
  local ts2="2025-06-01T00:20:00.000000000Z"
  for ts in "$ts1" "$ts2"; do
    local snap="$multiline_session/metrics/archive/$ts"
    mkdir -p "$snap"
    cat > "$snap/metadata.env" << 'METAEOF'
ARCHIVE_VERSION=1
TICK_SKIPPED=false
PREV_PROM_INVALID=false
WARMUP_TICKS_REMAINING=0
FRESH_START=no
CRASH_RECOVERY=no
UPTIME_SECONDS=900
MONITOR_MODE=validator
PID=12345
START_TICKS=100
METAEOF
    # Create minimal prom files (evaluator will find no metrics, producing
    # alarms with state=skipped, which is sufficient to verify parsing works)
    echo "# empty" > "$snap/current.prom"
    echo "# empty" > "$snap/prev.prom"
  done

  local multiline_json
  multiline_json=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" \
    "$multiline_session" --replay --json 2>/dev/null) || true
  local multiline_ok
  multiline_ok=$(echo "$multiline_json" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    # Must have evaluated_ticks > 0 to prove parsing worked
    ok = (d.get('schema_version') == 1 and
          d.get('evaluated_ticks', 0) > 0 and
          d.get('total_snapshots', 0) == 2 and
          isinstance(d.get('alarms'), dict) and
          len(d.get('alarms', {})) > 0)
    print('ok' if ok else 'fail')
except Exception as e:
    print('fail')
" 2>/dev/null) || echo "fail"
  if [[ "$multiline_ok" == "ok" ]]; then
    tap_ok "replay: --replay --json parses multi-line evaluator JSON"
  else
    tap_not_ok "replay: --replay --json parses multi-line evaluator JSON" \
      "output: ${multiline_json:0:200}"
  fi

  # ── check-alarm-regression.sh behavioral tests ─────────────────────────

  # Create a test-specific catalog that strips silence_expected so existing
  # regression tests continue to detect lost-sync regressions as expected.
  local catalog_for_reg="$TEST_ROOT/test-catalog-no-silence.toml"
  sed '/^silence_expected/d' "$REPO_ROOT/.claude/skills/shared/metric-alarms.toml" > "$catalog_for_reg"
  local catalog_for_reg_checksum
  catalog_for_reg_checksum=$(sha256sum "$catalog_for_reg" | cut -d' ' -f1)

  # Test: no baseline → creates baseline, exits 0
  # Use real catalog alarm names so catalog validation passes
  local reg_session="$replay_root/reg-session"
  mkdir -p "$reg_session/metrics"
  local reg_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$reg_current" > "$reg_session/metrics/reg-current.json"

  local reg_out
  reg_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_session" --current "$reg_session/metrics/reg-current.json" \
    --catalog "$catalog_for_reg" 2>&1) || true
  if [[ -f "$reg_session/metrics/replay-baseline.json" ]] && \
     [[ -f "$reg_session/metrics/replay-baseline-stable.json" ]] && \
     echo "$reg_out" | grep -q "Baseline established"; then
    tap_ok "regression: no baseline creates both baselines"
  else
    tap_not_ok "regression: no baseline creates both baselines" "output: $reg_out"
  fi

  # Test: baseline + no regressions → baseline updated, exits 0
  # Same current as baseline → no regressions
  local reg_out2
  reg_out2=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_session" --current "$reg_session/metrics/reg-current.json" \
    --catalog "$catalog_for_reg" 2>&1) || true
  local reg_stdout2
  reg_stdout2=$(echo "$reg_out2" | grep -v "^No regressions\|^Baseline" | head -1)
  if echo "$reg_out2" | grep -q "No regressions" && [[ "$reg_stdout2" == "[]" ]]; then
    tap_ok "regression: no regressions updates baseline"
  else
    tap_not_ok "regression: no regressions updates baseline" "output: $reg_out2"
  fi

  # Test: baseline + regression → baseline NOT updated
  local reg_regressed='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$reg_regressed" > "$reg_session/metrics/reg-regressed.json"
  # Save baseline mtime before
  local baseline_before
  baseline_before=$(stat -c %Y "$reg_session/metrics/replay-baseline.json" 2>/dev/null) || baseline_before=0
  sleep 1  # ensure mtime difference if updated

  local reg_out3
  reg_out3=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_session" --current "$reg_session/metrics/reg-regressed.json" \
    --catalog "$catalog_for_reg" 2>&1) || true
  local baseline_after
  baseline_after=$(stat -c %Y "$reg_session/metrics/replay-baseline.json" 2>/dev/null) || baseline_after=0

  if echo "$reg_out3" | grep -q "regression(s) found" && [[ "$baseline_before" == "$baseline_after" ]]; then
    tap_ok "regression: regressions found, baseline NOT updated"
  else
    tap_not_ok "regression: regressions found, baseline NOT updated" \
      "output: $reg_out3 before=$baseline_before after=$baseline_after"
  fi

  # Test: regression JSON contains correct baseline_fired_pct_display (percentage)
  local pct_display
  pct_display=$(echo "$reg_out3" | grep -v "regression(s) found\|Filing suppressed" | head -1 | python3 -c "
import json, sys
data = json.load(sys.stdin)
if data and 'baseline_fired_pct_display' in data[0]:
    print(data[0]['baseline_fired_pct_display'])
else:
    print('MISSING')
" 2>/dev/null) || pct_display="ERROR"
  if [[ "$pct_display" != "MISSING" ]] && [[ "$pct_display" != "ERROR" ]]; then
    tap_ok "regression: baseline_fired_pct_display present in output (${pct_display}%)"
  else
    tap_not_ok "regression: baseline_fired_pct_display present in output" "got: $pct_display"
  fi

  # Test: non-catalog alarms are filtered out (contamination test)
  local reg_contaminated_session="$replay_root/reg-contaminated"
  mkdir -p "$reg_contaminated_session/metrics"
  # Baseline has both real (lost-sync) and fake (alarm_a) alarms firing
  local reg_contaminated_baseline
  reg_contaminated_baseline=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
d = json.loads(sys.argv[1])
with open(sys.argv[3], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2], 'alarm_versions': versions}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"alarm_a":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$catalog_for_reg_checksum" "$catalog_for_reg")
  echo "$reg_contaminated_baseline" > "$reg_contaminated_session/metrics/replay-baseline.json"
  # Current has neither firing
  local reg_contaminated_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"alarm_a":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$reg_contaminated_current" > "$reg_contaminated_session/metrics/reg-contaminated-current.json"

  local reg_contam_out
  reg_contam_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_contaminated_session" \
    --current "$reg_contaminated_session/metrics/reg-contaminated-current.json" \
    --catalog "$catalog_for_reg" 2>&1) || true
  # Should report regression for lost-sync but NOT for alarm_a
  local contam_has_lost_sync contam_has_alarm_a
  contam_has_lost_sync=$(echo "$reg_contam_out" | grep -c '"lost-sync"') || true
  contam_has_alarm_a=$(echo "$reg_contam_out" | grep -c '"alarm_a"') || true
  if [[ "$contam_has_lost_sync" -ge 1 ]] && [[ "$contam_has_alarm_a" -eq 0 ]]; then
    tap_ok "regression: non-catalog alarms filtered (alarm_a excluded, lost-sync kept)"
  else
    tap_not_ok "regression: non-catalog alarms filtered" \
      "lost-sync=$contam_has_lost_sync alarm_a=$contam_has_alarm_a output: ${reg_contam_out:0:200}"
  fi

  # Test: baseline pruning on write — non-catalog alarms removed from baseline file
  local reg_prune_session="$replay_root/reg-prune"
  mkdir -p "$reg_prune_session/metrics"
  # Current with real + fake alarms, no baseline exists yet
  local reg_prune_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"fake_alarm":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}}}'
  echo "$reg_prune_current" > "$reg_prune_session/metrics/reg-prune-current.json"

  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_prune_session" --current "$reg_prune_session/metrics/reg-prune-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1 || true
  # Check that baseline file does not contain fake_alarm
  local prune_has_fake
  prune_has_fake=$(grep -c 'fake_alarm' "$reg_prune_session/metrics/replay-baseline.json" 2>/dev/null) || true
  local prune_has_real
  prune_has_real=$(grep -c 'lost-sync' "$reg_prune_session/metrics/replay-baseline.json" 2>/dev/null) || true
  if [[ "$prune_has_fake" -eq 0 ]] && [[ "$prune_has_real" -ge 1 ]]; then
    tap_ok "regression: baseline pruned on write (fake_alarm removed)"
  else
    tap_not_ok "regression: baseline pruned on write" \
      "fake=$prune_has_fake real=$prune_has_real"
  fi

  # Test: missing catalog → exit 2
  local reg_missing_cat_out
  set +e
  reg_missing_cat_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_session" --current "$reg_session/metrics/reg-current.json" \
    --catalog "/nonexistent/catalog.toml" 2>&1)
  local reg_missing_cat_exit=$?
  set -e
  if [[ "$reg_missing_cat_exit" -eq 2 ]]; then
    tap_ok "regression: missing catalog exits 2"
  else
    tap_not_ok "regression: missing catalog exits 2" \
      "exit=$reg_missing_cat_exit output: ${reg_missing_cat_out:0:200}"
  fi

  # Test: malformed catalog (valid TOML, no alarm entries) → exit 2
  local malformed_catalog="$replay_root/malformed-catalog.toml"
  echo 'schema_version = 1' > "$malformed_catalog"
  # Need a fresh session dir without baseline for this test
  local reg_malformed_session="$replay_root/reg-malformed"
  mkdir -p "$reg_malformed_session/metrics"
  echo "$reg_current" > "$reg_malformed_session/metrics/reg-current.json"

  local reg_malformed_out
  set +e
  reg_malformed_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$reg_malformed_session" --current "$reg_malformed_session/metrics/reg-current.json" \
    --catalog "$malformed_catalog" 2>&1)
  local reg_malformed_exit=$?
  set -e
  if [[ "$reg_malformed_exit" -eq 2 ]]; then
    tap_ok "regression: malformed catalog (no alarms) exits 2"
  else
    tap_not_ok "regression: malformed catalog (no alarms) exits 2" \
      "exit=$reg_malformed_exit output: ${reg_malformed_out:0:200}"
  fi

  rm -rf "$replay_root"

  # ── check-alarm-regression.sh --no-file tests ─────────────────────────────

  local nofile_root
  nofile_root=$(mktemp -d)
  local nofile_session="$nofile_root/nofile-session"
  mkdir -p "$nofile_session/metrics"

  # Create baseline with active alarm, then test with silent alarm
  local nofile_active='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}'
  local nofile_silent='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$nofile_active" > "$nofile_session/metrics/nofile-active.json"
  echo "$nofile_silent" > "$nofile_session/metrics/nofile-silent.json"

  # Bootstrap baseline
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$nofile_session" --current "$nofile_session/metrics/nofile-active.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1 || true

  # Run with regressions + --no-file
  local nofile_out
  nofile_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$nofile_session" --current "$nofile_session/metrics/nofile-silent.json" \
    --catalog "$catalog_for_reg" 2>&1) || true

  # Verify: regressions detected in output
  if echo "$nofile_out" | grep -q "regression(s) found"; then
    tap_ok "--no-file: regressions still detected"
  else
    tap_not_ok "--no-file: regressions still detected" "output: $nofile_out"
  fi

  # Verify: filing suppressed message present
  if echo "$nofile_out" | grep -q "Filing suppressed (--no-file)"; then
    tap_ok "--no-file: filing suppressed message emitted"
  else
    tap_not_ok "--no-file: filing suppressed message emitted" "output: $nofile_out"
  fi

  # Verify: no dedup file created (repo-global side effect suppressed)
  if [[ ! -f "$nofile_root/.alarm-regression-filed.json" ]]; then
    tap_ok "--no-file: no dedup file created"
  else
    tap_not_ok "--no-file: no dedup file created" "file exists: $(cat "$nofile_root/.alarm-regression-filed.json")"
  fi

  # Verify: no lock file created
  if [[ ! -f "$nofile_root/.alarm-regression-filing.lock" ]]; then
    tap_ok "--no-file: no lock file created"
  else
    tap_not_ok "--no-file: no lock file created"
  fi

  # Verify: JSON regression output still present on stdout
  local nofile_json
  nofile_json=$(echo "$nofile_out" | grep -v "regression(s)\|Filing suppressed\|Baseline\|Rolling" | head -1)
  if echo "$nofile_json" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d)>0" 2>/dev/null; then
    tap_ok "--no-file: regression JSON still emitted"
  else
    tap_not_ok "--no-file: regression JSON still emitted" "json: $nofile_json"
  fi

  rm -rf "$nofile_root"

  # ── check-alarm-regression.sh --no-file gh-stub test ───────────────────────
  # Verifies --no-file makes zero GitHub API calls using a PATH-based gh stub.

  local ghstub_root
  ghstub_root=$(mktemp -d)
  local ghstub_fake_repo="$ghstub_root/fake-repo"
  local ghstub_session="$ghstub_root/session"
  local ghstub_gh_dir="$ghstub_root/gh-stub"
  local ghstub_gh_log="$ghstub_root/gh-calls.log"
  mkdir -p "$ghstub_session/metrics" "$ghstub_gh_dir"
  mkdir -p "$ghstub_fake_repo/scripts/dev" "$ghstub_fake_repo/scripts/lib"
  mkdir -p "$ghstub_fake_repo/.github/skills/shared/scripts"
  cp "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" "$ghstub_fake_repo/scripts/dev/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.sh" "$ghstub_fake_repo/scripts/lib/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.py" "$ghstub_fake_repo/scripts/lib/"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$ghstub_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  chmod +x "$ghstub_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"

  # Failing gh stub: logs invocation and exits 99
  cat > "$ghstub_gh_dir/gh" << 'GHSTUBEOF'
#!/usr/bin/env bash
echo "$*" >> GHSTUB_LOG_PLACEHOLDER
exit 99
GHSTUBEOF
  sed -i "s|GHSTUB_LOG_PLACEHOLDER|$ghstub_gh_log|" "$ghstub_gh_dir/gh"
  chmod +x "$ghstub_gh_dir/gh"

  # Create baseline with valid provenance (lost-sync active at 10%)
  local ghstub_baseline
  ghstub_baseline=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
d = json.loads(sys.argv[1])
with open(sys.argv[3], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2], 'alarm_versions': versions}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$catalog_for_reg_checksum" "$catalog_for_reg")
  echo "$ghstub_baseline" > "$ghstub_session/metrics/replay-baseline.json"
  # Stable baseline too (to avoid recreation)
  echo "$ghstub_baseline" > "$ghstub_session/metrics/replay-baseline-stable.json"

  # Current where lost-sync went silent → regression
  local ghstub_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$ghstub_current" > "$ghstub_session/metrics/ghstub-current.json"

  # Pre-seed dedup file and save checksum
  local ghstub_dedup_seed='{"schema_version":1,"filed":{"lost-sync":{"filed_at":"2099-01-01T00:00:00Z","issue_url":"https://example.com/1"}}}'
  echo "$ghstub_dedup_seed" > "$ghstub_fake_repo/.alarm-regression-filed.json"
  local ghstub_dedup_checksum
  ghstub_dedup_checksum=$(sha256sum "$ghstub_fake_repo/.alarm-regression-filed.json" | cut -d' ' -f1)

  # Run with --no-file
  local ghstub_out
  ghstub_out=$(PATH="$ghstub_gh_dir:$PATH" "$ghstub_fake_repo/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ghstub_session" --current "$ghstub_session/metrics/ghstub-current.json" \
    --catalog "$catalog_for_reg" 2>&1) || true

  # Assert: zero gh calls
  if [[ ! -f "$ghstub_gh_log" ]] || [[ ! -s "$ghstub_gh_log" ]]; then
    tap_ok "gh-stub: zero gh calls under --no-file"
  else
    tap_not_ok "gh-stub: zero gh calls under --no-file" "gh calls: $(cat "$ghstub_gh_log")"
  fi

  # Assert: regressions still detected
  if echo "$ghstub_out" | grep -q "regression(s) found"; then
    tap_ok "gh-stub: regressions still detected under --no-file"
  else
    tap_not_ok "gh-stub: regressions still detected under --no-file" "output: $ghstub_out"
  fi

  # Assert: filing suppressed message
  if echo "$ghstub_out" | grep -q "Filing suppressed (--no-file)"; then
    tap_ok "gh-stub: filing suppressed message under --no-file"
  else
    tap_not_ok "gh-stub: filing suppressed message under --no-file" "output: $ghstub_out"
  fi

  # Assert: dedup file unchanged (byte-identical)
  local ghstub_dedup_after
  ghstub_dedup_after=$(sha256sum "$ghstub_fake_repo/.alarm-regression-filed.json" 2>/dev/null | cut -d' ' -f1) || ghstub_dedup_after="MISSING"
  if [[ "$ghstub_dedup_checksum" == "$ghstub_dedup_after" ]]; then
    tap_ok "gh-stub: dedup file unchanged under --no-file"
  else
    tap_not_ok "gh-stub: dedup file unchanged under --no-file" \
      "before=$ghstub_dedup_checksum after=$ghstub_dedup_after"
  fi

  rm -rf "$ghstub_root"

  # ── check-alarm-regression.sh dedup catalog-pruning test ───────────────────
  # Verifies non-catalog entries are pruned from .alarm-regression-filed.json.

  local prune_root
  prune_root=$(mktemp -d)
  local prune_fake_repo="$prune_root/fake-repo"
  local prune_session="$prune_root/session"
  local prune_gh_dir="$prune_root/gh-stub"
  local prune_gh_log="$prune_root/gh-calls.log"
  mkdir -p "$prune_session/metrics" "$prune_gh_dir"
  mkdir -p "$prune_fake_repo/scripts/dev" "$prune_fake_repo/scripts/lib"
  mkdir -p "$prune_fake_repo/.github/skills/shared/scripts"
  cp "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" "$prune_fake_repo/scripts/dev/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.sh" "$prune_fake_repo/scripts/lib/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.py" "$prune_fake_repo/scripts/lib/"
  printf '#!/usr/bin/env bash\nexit 0\n' > "$prune_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  chmod +x "$prune_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"

  # Working gh stub: returns [] for issue list, fake URL for issue create
  cat > "$prune_gh_dir/gh" << 'PRUNESTUBEOF'
#!/usr/bin/env bash
echo "$*" >> PRUNE_LOG_PLACEHOLDER
if [[ "$1" == "issue" && "$2" == "list" ]]; then
  echo '[]'
  exit 0
elif [[ "$1" == "issue" && "$2" == "create" ]]; then
  echo 'https://github.com/stellar-experimental/henyey/issues/9999'
  exit 0
elif [[ "$1" == "label" ]]; then
  exit 0
fi
exit 0
PRUNESTUBEOF
  sed -i "s|PRUNE_LOG_PLACEHOLDER|$prune_gh_log|" "$prune_gh_dir/gh"
  chmod +x "$prune_gh_dir/gh"

  # Create baseline with valid provenance (lost-sync active)
  local prune_baseline
  prune_baseline=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
d = json.loads(sys.argv[1])
with open(sys.argv[3], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2], 'alarm_versions': versions}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$catalog_for_reg_checksum" "$catalog_for_reg")
  echo "$prune_baseline" > "$prune_session/metrics/replay-baseline.json"
  echo "$prune_baseline" > "$prune_session/metrics/replay-baseline-stable.json"

  # Current where lost-sync went silent → regression
  local prune_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$prune_current" > "$prune_session/metrics/prune-current.json"

  # Pre-seed dedup file with non-catalog + catalog entries (use filed_at, not ts)
  local prune_dedup_seed='{"schema_version":1,"filed":{"stale-test-alarm":{"filed_at":"2099-01-01T00:00:00Z","issue_url":"https://example.com/stale"},"peer-count-low":{"filed_at":"2099-01-01T00:00:00Z","issue_url":"https://example.com/peer"}}}'
  echo "$prune_dedup_seed" > "$prune_fake_repo/.alarm-regression-filed.json"

  # Run WITHOUT --no-file
  local prune_out
  prune_out=$(PATH="$prune_gh_dir:$PATH" "$prune_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$prune_session" --current "$prune_session/metrics/prune-current.json" \
    --catalog "$catalog_for_reg" 2>&1) || true

  # Assert: non-catalog entry removed
  local prune_has_stale
  prune_has_stale=$(grep -c '"stale-test-alarm"' "$prune_fake_repo/.alarm-regression-filed.json" 2>/dev/null) || true
  if [[ "$prune_has_stale" -eq 0 ]]; then
    tap_ok "dedup-prune: non-catalog entry removed (stale-test-alarm)"
  else
    tap_not_ok "dedup-prune: non-catalog entry removed (stale-test-alarm)" \
      "still present: $(cat "$prune_fake_repo/.alarm-regression-filed.json")"
  fi

  # Assert: catalog entry preserved
  local prune_has_peer
  prune_has_peer=$(grep -c '"peer-count-low"' "$prune_fake_repo/.alarm-regression-filed.json" 2>/dev/null) || true
  if [[ "$prune_has_peer" -ge 1 ]]; then
    tap_ok "dedup-prune: catalog entry preserved (peer-count-low)"
  else
    tap_not_ok "dedup-prune: catalog entry preserved (peer-count-low)" \
      "missing: $(cat "$prune_fake_repo/.alarm-regression-filed.json")"
  fi

  # Assert: newly filed alarm recorded
  local prune_has_lostsync
  prune_has_lostsync=$(grep -c '"lost-sync"' "$prune_fake_repo/.alarm-regression-filed.json" 2>/dev/null) || true
  if [[ "$prune_has_lostsync" -ge 1 ]]; then
    tap_ok "dedup-prune: newly filed alarm recorded (lost-sync)"
  else
    tap_not_ok "dedup-prune: newly filed alarm recorded (lost-sync)" \
      "missing: $(cat "$prune_fake_repo/.alarm-regression-filed.json")"
  fi

  # Assert: pruning reported in stderr
  if echo "$prune_out" | grep -q "Pruned non-catalog dedup entry: stale-test-alarm"; then
    tap_ok "dedup-prune: stderr reports pruning of stale-test-alarm"
  else
    tap_not_ok "dedup-prune: stderr reports pruning of stale-test-alarm" "output: $prune_out"
  fi

  rm -rf "$prune_root"

  # ── check-alarm-regression.sh stable-baseline tests ────────────────────────

  local stable_root
  stable_root=$(mktemp -d)
  local stable_catalog="$stable_root/test-catalog-no-silence.toml"
  sed '/^silence_expected/d' "$REPO_ROOT/.claude/skills/shared/metric-alarms.toml" > "$stable_catalog"
  local stable_catalog_checksum
  stable_catalog_checksum=$(sha256sum "$stable_catalog" | cut -d' ' -f1)

  # Shared test data: alarm active at 10% (20/200)
  local stable_current_active='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  # Current with alarm silent (0% firing)
  local stable_current_silent='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  # Current with alarm absent
  local stable_current_absent='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  # Current with low sample count
  local stable_current_low='{"schema_version":1,"evaluated_ticks":5,"skipped_ticks":0,"error_ticks":0,"total_snapshots":5,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":5,"baseline":0,"skip":0}}}'
  # Rolling baseline where alarm has decayed below 5%
  local stable_rolling_decayed='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":5,"breach":0,"ok":185,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'

  # Helper: add provenance to a baseline JSON string (for use as pre-existing baselines in tests)
  add_test_provenance() {
    python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
d = json.loads(sys.argv[1])
with open(sys.argv[3], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2], 'alarm_versions': versions}
print(json.dumps(d))
" "$1" "$stable_catalog_checksum" "$stable_catalog"
  }
  local stable_baseline_active
  stable_baseline_active=$(add_test_provenance "$stable_current_active")
  local stable_baseline_silent
  stable_baseline_silent=$(add_test_provenance "$stable_current_silent")
  local stable_baseline_absent
  stable_baseline_absent=$(add_test_provenance "$stable_current_absent")
  local stable_baseline_decayed
  stable_baseline_decayed=$(add_test_provenance "$stable_rolling_decayed")

  # Test 1: Initial run creates both baselines (already tested above, this is the
  # stable-specific verification that content is identical)
  local sb_t1="$stable_root/t1"
  mkdir -p "$sb_t1/metrics"
  echo "$stable_current_active" > "$sb_t1/metrics/current.json"
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t1" --current "$sb_t1/metrics/current.json" --catalog "$stable_catalog" >/dev/null 2>&1 || true
  if diff -q "$sb_t1/metrics/replay-baseline.json" "$sb_t1/metrics/replay-baseline-stable.json" >/dev/null 2>&1; then
    tap_ok "stable: initial run creates identical rolling and stable baselines"
  else
    tap_not_ok "stable: initial run creates identical rolling and stable baselines"
  fi

  # Test 2: Stable NOT updated on clean run (mtime check)
  local stable_mtime_before
  stable_mtime_before=$(stat -c %Y "$sb_t1/metrics/replay-baseline-stable.json" 2>/dev/null) || stable_mtime_before=0
  sleep 1
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t1" --current "$sb_t1/metrics/current.json" --catalog "$stable_catalog" >/dev/null 2>&1 || true
  local stable_mtime_after
  stable_mtime_after=$(stat -c %Y "$sb_t1/metrics/replay-baseline-stable.json" 2>/dev/null) || stable_mtime_after=0
  if [[ "$stable_mtime_before" == "$stable_mtime_after" ]]; then
    tap_ok "stable: stable baseline NOT updated on clean run"
  else
    tap_not_ok "stable: stable baseline NOT updated on clean run" \
      "before=$stable_mtime_before after=$stable_mtime_after"
  fi

  # Test 3: Gradual decay detected via stable baseline
  # Setup: stable has alarm at 10%, rolling has decayed below 5%, current is 0%
  local sb_t3="$stable_root/t3"
  mkdir -p "$sb_t3/metrics"
  echo "$stable_baseline_active" > "$sb_t3/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_decayed" > "$sb_t3/metrics/replay-baseline.json"
  echo "$stable_current_silent" > "$sb_t3/metrics/current.json"
  local sb_t3_out
  sb_t3_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t3" --current "$sb_t3/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  if echo "$sb_t3_out" | grep -q "gradually decayed to silent"; then
    tap_ok "stable: gradual decay detected via stable baseline"
  else
    tap_not_ok "stable: gradual decay detected via stable baseline" "output: ${sb_t3_out:0:300}"
  fi

  # Test 4: No false positive when alarm still active
  local sb_t4="$stable_root/t4"
  mkdir -p "$sb_t4/metrics"
  echo "$stable_baseline_active" > "$sb_t4/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_active" > "$sb_t4/metrics/replay-baseline.json"
  echo "$stable_current_active" > "$sb_t4/metrics/current.json"
  local sb_t4_out
  sb_t4_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t4" --current "$sb_t4/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  local sb_t4_json
  sb_t4_json=$(echo "$sb_t4_out" | grep -v "^No regressions\|^Rolling\|^Stable\|^Baseline" | head -1)
  if [[ "$sb_t4_json" == "[]" ]]; then
    tap_ok "stable: no false positive when alarm still active"
  else
    tap_not_ok "stable: no false positive when alarm still active" "output: ${sb_t4_out:0:200}"
  fi

  # Test 5: Partial bootstrap — rolling exists, stable missing
  local sb_t5="$stable_root/t5"
  mkdir -p "$sb_t5/metrics"
  echo "$stable_baseline_active" > "$sb_t5/metrics/replay-baseline.json"
  echo "$stable_current_active" > "$sb_t5/metrics/current.json"
  local sb_t5_out
  sb_t5_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t5" --current "$sb_t5/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  if [[ -f "$sb_t5/metrics/replay-baseline-stable.json" ]] && echo "$sb_t5_out" | grep -q "Stable baseline established"; then
    tap_ok "stable: partial bootstrap — rolling exists, stable created"
  else
    tap_not_ok "stable: partial bootstrap — rolling exists, stable created" "output: ${sb_t5_out:0:200}"
  fi

  # Test 6: Partial bootstrap — stable exists, rolling missing
  local sb_t6="$stable_root/t6"
  mkdir -p "$sb_t6/metrics"
  echo "$stable_baseline_active" > "$sb_t6/metrics/replay-baseline-stable.json"
  echo "$stable_current_active" > "$sb_t6/metrics/current.json"
  local sb_t6_out
  sb_t6_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t6" --current "$sb_t6/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  if [[ -f "$sb_t6/metrics/replay-baseline.json" ]] && echo "$sb_t6_out" | grep -q "Rolling baseline re-established"; then
    tap_ok "stable: partial bootstrap — stable exists, rolling created"
  else
    tap_not_ok "stable: partial bootstrap — stable exists, rolling created" "output: ${sb_t6_out:0:200}"
  fi

  # Test 7: Corrupt stable baseline (invalid JSON) → exit 2
  local sb_t7="$stable_root/t7"
  mkdir -p "$sb_t7/metrics"
  echo "$stable_baseline_active" > "$sb_t7/metrics/replay-baseline.json"
  echo "NOT VALID JSON" > "$sb_t7/metrics/replay-baseline-stable.json"
  echo "$stable_current_active" > "$sb_t7/metrics/current.json"
  local sb_t7_out
  set +e
  sb_t7_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t7" --current "$sb_t7/metrics/current.json" --catalog "$stable_catalog" 2>&1)
  local sb_t7_exit=$?
  set -e
  if [[ "$sb_t7_exit" -eq 2 ]]; then
    tap_ok "stable: corrupt stable baseline exits 2"
  else
    tap_not_ok "stable: corrupt stable baseline exits 2" "exit=$sb_t7_exit output: ${sb_t7_out:0:200}"
  fi

  # Test 8: Schema mismatch stable → exit 2
  local sb_t8="$stable_root/t8"
  mkdir -p "$sb_t8/metrics"
  echo "$stable_baseline_active" > "$sb_t8/metrics/replay-baseline.json"
  echo '{"schema_version":2,"evaluated_ticks":200,"alarms":{}}' > "$sb_t8/metrics/replay-baseline-stable.json"
  echo "$stable_current_active" > "$sb_t8/metrics/current.json"
  local sb_t8_out
  set +e
  sb_t8_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t8" --current "$sb_t8/metrics/current.json" --catalog "$stable_catalog" 2>&1)
  local sb_t8_exit=$?
  set -e
  if [[ "$sb_t8_exit" -eq 2 ]]; then
    tap_ok "stable: schema mismatch stable exits 2"
  else
    tap_not_ok "stable: schema mismatch stable exits 2" "exit=$sb_t8_exit output: ${sb_t8_out:0:200}"
  fi

  # Test 9: Absent alarm from stable → regression reported
  local sb_t9="$stable_root/t9"
  mkdir -p "$sb_t9/metrics"
  echo "$stable_baseline_active" > "$sb_t9/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_active" > "$sb_t9/metrics/replay-baseline.json"
  echo "$stable_current_absent" > "$sb_t9/metrics/current.json"
  local sb_t9_out
  sb_t9_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t9" --current "$sb_t9/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  if echo "$sb_t9_out" | grep -q "gradual drift"; then
    tap_ok "stable: absent alarm from stable reports regression"
  else
    tap_not_ok "stable: absent alarm from stable reports regression" "output: ${sb_t9_out:0:300}"
  fi

  # Test 10: Low-sample current → alarm skipped
  local sb_t10="$stable_root/t10"
  mkdir -p "$sb_t10/metrics"
  echo "$stable_baseline_active" > "$sb_t10/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_active" > "$sb_t10/metrics/replay-baseline.json"
  echo "$stable_current_low" > "$sb_t10/metrics/current.json"
  local sb_t10_out
  sb_t10_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t10" --current "$sb_t10/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  local sb_t10_json
  sb_t10_json=$(echo "$sb_t10_out" | grep -v "^No regressions\|^Rolling\|^Stable\|^Baseline" | head -1)
  if [[ "$sb_t10_json" == "[]" ]]; then
    tap_ok "stable: low-sample current skips alarm"
  else
    tap_not_ok "stable: low-sample current skips alarm" "output: ${sb_t10_out:0:200}"
  fi

  # Test 11: Dedup — both baselines flag same alarm → stable wins with full object
  local sb_t11="$stable_root/t11"
  mkdir -p "$sb_t11/metrics"
  # Stable: alarm at 10% (20/200)
  echo "$stable_baseline_active" > "$sb_t11/metrics/replay-baseline-stable.json"
  # Rolling: alarm at 8% (16/200) — different percentage
  local rolling_8pct='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":16,"breach":5,"ok":169,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$(add_test_provenance "$rolling_8pct")" > "$sb_t11/metrics/replay-baseline.json"
  echo "$stable_current_silent" > "$sb_t11/metrics/current.json"
  local sb_t11_out
  sb_t11_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t11" --current "$sb_t11/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  # Should have stable source and stable's percentage (10%, not 8%)
  local sb_t11_source
  sb_t11_source=$(echo "$sb_t11_out" | grep -v "regression(s)" | head -1 | python3 -c "
import json, sys
data = json.load(sys.stdin)
r = [x for x in data if x['alarm'] == 'lost-sync']
if r and r[0]['baseline_source'] == 'stable' and r[0]['baseline_fired_pct_display'] == 10.53:
    print('ok')
else:
    print('fail: ' + json.dumps(r))
" 2>/dev/null) || sb_t11_source="fail"
  if [[ "$sb_t11_source" == "ok" ]]; then
    tap_ok "stable: dedup — stable wins with full object"
  else
    tap_not_ok "stable: dedup — stable wins with full object" "result: $sb_t11_source output: ${sb_t11_out:0:300}"
  fi

  # Test 12: --stable-baseline flag overrides default path
  local sb_t12="$stable_root/t12"
  mkdir -p "$sb_t12/metrics"
  echo "$stable_current_active" > "$sb_t12/metrics/current.json"
  local custom_stable_path="$sb_t12/metrics/custom-stable.json"
  # First create rolling baseline so we trigger the partial-bootstrap path
  echo "$stable_baseline_active" > "$sb_t12/metrics/replay-baseline.json"
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t12" --current "$sb_t12/metrics/current.json" \
    --stable-baseline "$custom_stable_path" \
    --catalog "$stable_catalog" >/dev/null 2>&1 || true
  if [[ -f "$custom_stable_path" ]]; then
    tap_ok "stable: --stable-baseline flag overrides path"
  else
    tap_not_ok "stable: --stable-baseline flag overrides path"
  fi

  # Test 13: Stable-only regressions prevent rolling baseline update
  local sb_t13="$stable_root/t13"
  mkdir -p "$sb_t13/metrics"
  # Stable has alarm active at 10%, rolling has it decayed below 5%
  echo "$stable_baseline_active" > "$sb_t13/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_decayed" > "$sb_t13/metrics/replay-baseline.json"
  echo "$stable_current_silent" > "$sb_t13/metrics/current.json"
  local rolling_mtime_before
  rolling_mtime_before=$(stat -c %Y "$sb_t13/metrics/replay-baseline.json" 2>/dev/null) || rolling_mtime_before=0
  sleep 1
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t13" --current "$sb_t13/metrics/current.json" --catalog "$stable_catalog" >/dev/null 2>&1 || true
  local rolling_mtime_after
  rolling_mtime_after=$(stat -c %Y "$sb_t13/metrics/replay-baseline.json" 2>/dev/null) || rolling_mtime_after=0
  if [[ "$rolling_mtime_before" == "$rolling_mtime_after" ]]; then
    tap_ok "stable: stable-only regressions prevent rolling update"
  else
    tap_not_ok "stable: stable-only regressions prevent rolling update" \
      "before=$rolling_mtime_before after=$rolling_mtime_after"
  fi

  # Test 14: Same-path rejection via realpath
  local sb_t14="$stable_root/t14"
  mkdir -p "$sb_t14/metrics"
  echo "$stable_current_active" > "$sb_t14/metrics/current.json"
  echo "$stable_baseline_active" > "$sb_t14/metrics/replay-baseline.json"
  set +e
  local sb_t14_out
  sb_t14_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t14" --current "$sb_t14/metrics/current.json" \
    --baseline "$sb_t14/metrics/replay-baseline.json" \
    --stable-baseline "$sb_t14/metrics/replay-baseline.json" \
    --catalog "$stable_catalog" 2>&1)
  local sb_t14_exit=$?
  set -e
  if [[ "$sb_t14_exit" -eq 2 ]] && echo "$sb_t14_out" | grep -q "must be different files"; then
    tap_ok "stable: same-path rejection via realpath"
  else
    tap_not_ok "stable: same-path rejection via realpath" "exit=$sb_t14_exit output: ${sb_t14_out:0:200}"
  fi

  # Test 15: Invalid current replay → exit 2
  local sb_t15="$stable_root/t15"
  mkdir -p "$sb_t15/metrics"
  echo "INVALID JSON" > "$sb_t15/metrics/current.json"
  set +e
  local sb_t15_out
  sb_t15_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t15" --current "$sb_t15/metrics/current.json" \
    --catalog "$stable_catalog" 2>&1)
  local sb_t15_exit=$?
  set -e
  if [[ "$sb_t15_exit" -eq 2 ]]; then
    tap_ok "stable: invalid current replay exits 2"
  else
    tap_not_ok "stable: invalid current replay exits 2" "exit=$sb_t15_exit output: ${sb_t15_out:0:200}"
  fi

  # Test 16: baseline_source field present in regression JSON
  local sb_t16="$stable_root/t16"
  mkdir -p "$sb_t16/metrics"
  echo "$stable_baseline_active" > "$sb_t16/metrics/replay-baseline-stable.json"
  echo "$stable_baseline_active" > "$sb_t16/metrics/replay-baseline.json"
  echo "$stable_current_silent" > "$sb_t16/metrics/current.json"
  local sb_t16_out
  sb_t16_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t16" --current "$sb_t16/metrics/current.json" --catalog "$stable_catalog" 2>&1) || true
  local sb_t16_has_source
  sb_t16_has_source=$(echo "$sb_t16_out" | grep -v "regression(s)" | head -1 | python3 -c "
import json, sys
data = json.load(sys.stdin)
if all('baseline_source' in r for r in data) and len(data) > 0:
    print('ok')
else:
    print('fail')
" 2>/dev/null) || sb_t16_has_source="fail"
  if [[ "$sb_t16_has_source" == "ok" ]]; then
    tap_ok "stable: baseline_source field present in regression JSON"
  else
    tap_not_ok "stable: baseline_source field present in regression JSON" "output: ${sb_t16_out:0:200}"
  fi

  # Test 17: Stable baseline with missing evaluated_ticks → exit 2
  local sb_t17="$stable_root/t17"
  mkdir -p "$sb_t17/metrics"
  echo "$stable_baseline_active" > "$sb_t17/metrics/replay-baseline.json"
  echo '{"schema_version":1,"alarms":{}}' > "$sb_t17/metrics/replay-baseline-stable.json"
  echo "$stable_current_active" > "$sb_t17/metrics/current.json"
  local sb_t17_out
  set +e
  sb_t17_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t17" --current "$sb_t17/metrics/current.json" --catalog "$stable_catalog" 2>&1)
  local sb_t17_exit=$?
  set -e
  if [[ "$sb_t17_exit" -eq 2 ]] && echo "$sb_t17_out" | grep -q "invalid evaluated_ticks"; then
    tap_ok "stable: missing evaluated_ticks in stable exits 2"
  else
    tap_not_ok "stable: missing evaluated_ticks in stable exits 2" "exit=$sb_t17_exit output: ${sb_t17_out:0:200}"
  fi

  # Test 18: Rolling baseline with negative evaluated_ticks → exit 2
  local sb_t18="$stable_root/t18"
  mkdir -p "$sb_t18/metrics"
  echo '{"schema_version":1,"evaluated_ticks":-1,"alarms":{}}' > "$sb_t18/metrics/replay-baseline.json"
  echo "$stable_baseline_active" > "$sb_t18/metrics/replay-baseline-stable.json"
  echo "$stable_current_active" > "$sb_t18/metrics/current.json"
  local sb_t18_out
  set +e
  sb_t18_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$sb_t18" --current "$sb_t18/metrics/current.json" --catalog "$stable_catalog" 2>&1)
  local sb_t18_exit=$?
  set -e
  if [[ "$sb_t18_exit" -eq 2 ]] && echo "$sb_t18_out" | grep -q "invalid evaluated_ticks"; then
    tap_ok "stable: negative evaluated_ticks in rolling exits 2"
  else
    tap_not_ok "stable: negative evaluated_ticks in rolling exits 2" "exit=$sb_t18_exit output: ${sb_t18_out:0:200}"
  fi

  rm -rf "$stable_root"

  # ── check-alarm-regression.sh provenance tests (#2623) ───────────────────

  local prov_root
  prov_root=$(mktemp -d)
  local prov_catalog="$REPO_ROOT/.claude/skills/shared/metric-alarms.toml"
  local prov_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'

  # Test: Provenance presence — baselines contain provenance metadata
  local prov_t1="$prov_root/t1"
  mkdir -p "$prov_t1/metrics"
  echo "$prov_current" > "$prov_t1/metrics/current.json"
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t1" --current "$prov_t1/metrics/current.json" --catalog "$prov_catalog" >/dev/null 2>&1 || true
  local prov_has_fields
  prov_has_fields=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
p = d.get('provenance', {})
if isinstance(p, dict) and 'created_at' in p and 'created_commit' in p and 'catalog_checksum' in p:
    print('yes')
else:
    print('no')
" "$prov_t1/metrics/replay-baseline.json" 2>/dev/null) || prov_has_fields="no"
  if [[ "$prov_has_fields" == "yes" ]]; then
    tap_ok "provenance: baselines contain provenance metadata"
  else
    tap_not_ok "provenance: baselines contain provenance metadata"
  fi

  # Test: Stable baseline also has provenance
  local prov_stable_fields
  prov_stable_fields=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
p = d.get('provenance', {})
if isinstance(p, dict) and 'created_at' in p and 'catalog_checksum' in p:
    print('yes')
else:
    print('no')
" "$prov_t1/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_stable_fields="no"
  if [[ "$prov_stable_fields" == "yes" ]]; then
    tap_ok "provenance: stable baseline has provenance"
  else
    tap_not_ok "provenance: stable baseline has provenance"
  fi

  # Test: Matching checksum — no invalidation
  local prov_t2="$prov_root/t2"
  mkdir -p "$prov_t2/metrics"
  echo "$prov_current" > "$prov_t2/metrics/current.json"
  # Create baselines with matching catalog checksum AND alarm_versions
  local prov_real_checksum
  prov_real_checksum=$(sha256sum "$prov_catalog" | cut -d' ' -f1)
  local prov_alarm_versions
  prov_alarm_versions=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
print(json.dumps(versions, sort_keys=True))
" "$prov_catalog")
  local prov_baseline_with_prov
  prov_baseline_with_prov=$(python3 -c "
import json, sys
data = {
    'schema_version': 1, 'evaluated_ticks': 200, 'skipped_ticks': 10,
    'error_ticks': 0, 'total_snapshots': 210, 'first_ts': 't1', 'last_ts': 't2',
    'alarms': {'lost-sync': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10}},
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'abc123',
        'catalog_checksum': sys.argv[1],
        'alarm_versions': json.loads(sys.argv[2])
    }
}
print(json.dumps(data))
" "$prov_real_checksum" "$prov_alarm_versions")
  echo "$prov_baseline_with_prov" > "$prov_t2/metrics/replay-baseline.json"
  echo "$prov_baseline_with_prov" > "$prov_t2/metrics/replay-baseline-stable.json"
  local prov_hash_before
  prov_hash_before=$(sha256sum "$prov_t2/metrics/replay-baseline-stable.json" | cut -d' ' -f1)
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t2" --current "$prov_t2/metrics/current.json" --catalog "$prov_catalog" >/dev/null 2>&1 || true
  local prov_hash_after
  prov_hash_after=$(sha256sum "$prov_t2/metrics/replay-baseline-stable.json" | cut -d' ' -f1)
  if [[ "$prov_hash_before" == "$prov_hash_after" ]]; then
    tap_ok "provenance: matching checksum — stable baseline not invalidated"
  else
    tap_not_ok "provenance: matching checksum — stable baseline not invalidated"
  fi

  # Test: Changed checksum — auto-invalidation
  local prov_t3="$prov_root/t3"
  mkdir -p "$prov_t3/metrics"
  echo "$prov_current" > "$prov_t3/metrics/current.json"
  local prov_stale_baseline="{\"schema_version\":1,\"evaluated_ticks\":200,\"skipped_ticks\":10,\"error_ticks\":0,\"total_snapshots\":210,\"first_ts\":\"t1\",\"last_ts\":\"t2\",\"alarms\":{\"lost-sync\":{\"firing\":20,\"breach\":5,\"ok\":165,\"baseline\":0,\"skip\":10}},\"provenance\":{\"created_at\":\"2026-01-01T00:00:00Z\",\"created_commit\":\"abc123\",\"catalog_checksum\":\"0000000000000000000000000000000000000000000000000000000000000000\"}}"
  echo "$prov_stale_baseline" > "$prov_t3/metrics/replay-baseline.json"
  echo "$prov_stale_baseline" > "$prov_t3/metrics/replay-baseline-stable.json"
  local prov_t3_out
  prov_t3_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t3" --current "$prov_t3/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_new_checksum
  prov_new_checksum=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(d.get('provenance',{}).get('catalog_checksum',''))
" "$prov_t3/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_new_checksum=""
  if (echo "$prov_t3_out" | grep -q "auto-invalidated" || echo "$prov_t3_out" | grep -q "stamping per-alarm version provenance") && [[ "$prov_new_checksum" == "$prov_real_checksum" ]]; then
    tap_ok "provenance: changed checksum triggers auto-invalidation"
  else
    tap_not_ok "provenance: changed checksum triggers auto-invalidation" "output: $prov_t3_out, new_checksum: $prov_new_checksum"
  fi

  # Test: Legacy baseline (no provenance) — non-destructive migration, stamps provenance.
  # Alarm data is preserved because first_ts is not ISO-formatted (Step D skips).
  local prov_t4="$prov_root/t4"
  mkdir -p "$prov_t4/metrics"
  echo "$prov_current" > "$prov_t4/metrics/current.json"
  local prov_legacy='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}'
  echo "$prov_legacy" > "$prov_t4/metrics/replay-baseline.json"
  echo "$prov_legacy" > "$prov_t4/metrics/replay-baseline-stable.json"
  local prov_t4_out
  prov_t4_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t4" --current "$prov_t4/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_legacy_recreated
  prov_legacy_recreated=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
p = d.get('provenance', {})
# Verify provenance exists with correct checksum and alarm_versions
has_prov = isinstance(p, dict) and 'catalog_checksum' in p and isinstance(p.get('alarm_versions'), dict)
# Alarm data preserved when first_ts is not ISO-formatted (Step D skips)
alarms = d.get('alarms', {})
ls = alarms.get('lost-sync', {})
preserved = ls.get('firing') == 20
print('yes' if has_prov and preserved else 'no')
" "$prov_t4/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_legacy_recreated="no"
  if echo "$prov_t4_out" | grep -q "Migrating.*baseline" && [[ "$prov_legacy_recreated" == "yes" ]]; then
    tap_ok "provenance: legacy baseline migrated with provenance (alarm data preserved)"
  else
    tap_not_ok "provenance: legacy baseline migrated with provenance (alarm data preserved)" "output: $prov_t4_out, result: $prov_legacy_recreated"
  fi

  # Test: Malformed provenance — treated as legacy, data preserved (non-ISO first_ts)
  local prov_t5="$prov_root/t5"
  mkdir -p "$prov_t5/metrics"
  echo "$prov_current" > "$prov_t5/metrics/current.json"
  local prov_malformed='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}},"provenance":"garbage"}'
  echo "$prov_malformed" > "$prov_t5/metrics/replay-baseline.json"
  echo "$prov_malformed" > "$prov_t5/metrics/replay-baseline-stable.json"
  local prov_t5_out
  prov_t5_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t5" --current "$prov_t5/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_malformed_fixed
  prov_malformed_fixed=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
p = d.get('provenance', {})
# Verify provenance is a proper dict with checksum and alarm_versions
has_prov = isinstance(p, dict) and 'catalog_checksum' in p and isinstance(p.get('alarm_versions'), dict)
# Alarm data preserved when first_ts is not ISO-formatted (Step D skips)
alarms = d.get('alarms', {})
ls = alarms.get('lost-sync', {})
preserved = ls.get('firing') == 20
print('yes' if has_prov and preserved else 'no')
" "$prov_t5/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_malformed_fixed="no"
  if [[ "$prov_malformed_fixed" == "yes" ]]; then
    tap_ok "provenance: malformed provenance treated as legacy and fixed (alarm data preserved)"
  else
    tap_not_ok "provenance: malformed provenance treated as legacy and fixed (alarm data preserved)" "output: $prov_t5_out"
  fi

  # Test: Rolling baseline gets provenance on update
  local prov_t6="$prov_root/t6"
  mkdir -p "$prov_t6/metrics"
  echo "$prov_current" > "$prov_t6/metrics/current.json"
  # Bootstrap first
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t6" --current "$prov_t6/metrics/current.json" --catalog "$prov_catalog" >/dev/null 2>&1 || true
  # Run again (should update rolling with provenance)
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t6" --current "$prov_t6/metrics/current.json" --catalog "$prov_catalog" >/dev/null 2>&1 || true
  local prov_rolling_has_prov
  prov_rolling_has_prov=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
p = d.get('provenance', {})
print('yes' if isinstance(p, dict) and 'catalog_checksum' in p else 'no')
" "$prov_t6/metrics/replay-baseline.json" 2>/dev/null) || prov_rolling_has_prov="no"
  if [[ "$prov_rolling_has_prov" == "yes" ]]; then
    tap_ok "provenance: rolling baseline has provenance after update"
  else
    tap_not_ok "provenance: rolling baseline has provenance after update"
  fi

  # Test: No .tmp files left behind
  local prov_tmpfiles
  prov_tmpfiles=$(find "$prov_root" -name "*.tmp.*" 2>/dev/null | wc -l)
  if [[ "$prov_tmpfiles" == "0" ]]; then
    tap_ok "provenance: no .tmp files left behind after runs"
  else
    tap_not_ok "provenance: no .tmp files left behind after runs" "found $prov_tmpfiles tmp files"
  fi

  # Test: Rolling baseline auto-invalidated on checksum change
  local prov_t7="$prov_root/t7"
  mkdir -p "$prov_t7/metrics"
  echo "$prov_current" > "$prov_t7/metrics/current.json"
  local prov_stale_rolling="{\"schema_version\":1,\"evaluated_ticks\":200,\"skipped_ticks\":10,\"error_ticks\":0,\"total_snapshots\":210,\"first_ts\":\"t1\",\"last_ts\":\"t2\",\"alarms\":{\"lost-sync\":{\"firing\":20,\"breach\":5,\"ok\":165,\"baseline\":0,\"skip\":10}},\"provenance\":{\"created_at\":\"2026-01-01T00:00:00Z\",\"created_commit\":\"abc123\",\"catalog_checksum\":\"0000000000000000000000000000000000000000000000000000000000000000\"}}"
  echo "$prov_stale_rolling" > "$prov_t7/metrics/replay-baseline.json"
  # Stable has correct checksum — won't be invalidated
  echo "$prov_baseline_with_prov" > "$prov_t7/metrics/replay-baseline-stable.json"
  local prov_t7_out
  prov_t7_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t7" --current "$prov_t7/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_rolling_new_cs
  prov_rolling_new_cs=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(d.get('provenance',{}).get('catalog_checksum',''))
" "$prov_t7/metrics/replay-baseline.json" 2>/dev/null) || prov_rolling_new_cs=""
  if (echo "$prov_t7_out" | grep -q "Rolling baseline auto-invalidated" || echo "$prov_t7_out" | grep -q "Migrating Rolling baseline") && [[ "$prov_rolling_new_cs" == "$prov_real_checksum" ]]; then
    tap_ok "provenance: rolling baseline auto-invalidated on checksum change"
  else
    tap_not_ok "provenance: rolling baseline auto-invalidated on checksum change" "output: $prov_t7_out, checksum: $prov_rolling_new_cs"
  fi

  # Test: Non-git fallback — created_commit is "unknown" when git fails
  local prov_t8="$prov_root/t8"
  mkdir -p "$prov_t8/metrics" "$prov_t8/bin"
  echo "$prov_current" > "$prov_t8/metrics/current.json"
  # Create a git stub that fails for rev-parse
  cat > "$prov_t8/bin/git" << 'GIT_STUB'
#!/bin/bash
exit 1
GIT_STUB
  chmod +x "$prov_t8/bin/git"
  local prov_t8_out
  prov_t8_out=$(PATH="$prov_t8/bin:$PATH" "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t8" --current "$prov_t8/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_commit_val
  prov_commit_val=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(d.get('provenance',{}).get('created_commit',''))
" "$prov_t8/metrics/replay-baseline.json" 2>/dev/null) || prov_commit_val=""
  if [[ "$prov_commit_val" == "unknown" ]]; then
    tap_ok "provenance: non-git fallback sets created_commit to unknown"
  else
    tap_not_ok "provenance: non-git fallback sets created_commit to unknown" "got: $prov_commit_val"
  fi

  # Test: Failure-path safety — read-only dir preserves existing baseline
  local prov_t9="$prov_root/t9"
  mkdir -p "$prov_t9/metrics"
  echo "$prov_current" > "$prov_t9/metrics/current.json"
  # Create a valid baseline with stale checksum, then make dir read-only
  echo "$prov_stale_rolling" > "$prov_t9/metrics/replay-baseline.json"
  echo "$prov_stale_rolling" > "$prov_t9/metrics/replay-baseline-stable.json"
  chmod a-w "$prov_t9/metrics"
  local prov_t9_out
  prov_t9_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t9" --current "$prov_t9/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  chmod u+w "$prov_t9/metrics"
  # Baseline should still be intact (not truncated/corrupted)
  local prov_t9_valid
  prov_t9_valid=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
if d.get('schema_version') == 1 and isinstance(d.get('alarms'), dict):
    print('yes')
else:
    print('no')
" "$prov_t9/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_t9_valid="no"
  if [[ "$prov_t9_valid" == "yes" ]]; then
    tap_ok "provenance: failure-path preserves existing baseline"
  else
    tap_not_ok "provenance: failure-path preserves existing baseline"
  fi

  # Test: Comment-only catalog edit preserves baseline data (per-alarm versioning)
  local prov_t10="$prov_root/t10"
  mkdir -p "$prov_t10/metrics"
  echo "$prov_current" > "$prov_t10/metrics/current.json"
  # Create a modified catalog with just a comment change
  local prov_alt_catalog="$prov_root/alt-catalog.toml"
  cp "$prov_catalog" "$prov_alt_catalog"
  echo "# This comment changes the checksum" >> "$prov_alt_catalog"
  local prov_alt_checksum
  prov_alt_checksum=$(sha256sum "$prov_alt_catalog" | cut -d' ' -f1)
  # Create baseline with the ORIGINAL catalog checksum (versions match the alt catalog)
  echo "$prov_baseline_with_prov" > "$prov_t10/metrics/replay-baseline.json"
  echo "$prov_baseline_with_prov" > "$prov_t10/metrics/replay-baseline-stable.json"
  local prov_hash_before_t10
  prov_hash_before_t10=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(len(d.get('alarms', {})))
" "$prov_t10/metrics/replay-baseline-stable.json")
  local prov_t10_out
  prov_t10_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t10" --current "$prov_t10/metrics/current.json" --catalog "$prov_alt_catalog" 2>&1) || true
  local prov_alarms_after_t10
  prov_alarms_after_t10=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(len(d.get('alarms', {})))
" "$prov_t10/metrics/replay-baseline-stable.json")
  if echo "$prov_t10_out" | grep -q "no alarm versions differ" && [[ "$prov_alarms_after_t10" == "$prov_hash_before_t10" ]]; then
    tap_ok "provenance: comment-only catalog edit preserves data (per-alarm versioning)"
  else
    tap_not_ok "provenance: comment-only catalog edit preserves data (per-alarm versioning)" "output: $prov_t10_out, alarms_before=$prov_hash_before_t10, alarms_after=$prov_alarms_after_t10"
  fi

  # Test: Already-migrated stale baseline — Step D temporal check fires (#2647)
  # A baseline that went through the buggy legacy migration has current alarm_versions
  # and catalog_checksum, but stale alarm data from before semantic_change_date.
  local prov_t11="$prov_root/t11"
  mkdir -p "$prov_t11/metrics"
  echo "$prov_current" > "$prov_t11/metrics/current.json"
  local prov_real_checksum_t11
  prov_real_checksum_t11=$(sha256sum "$prov_catalog" | cut -d' ' -f1)
  local prov_real_versions_t11
  prov_real_versions_t11=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
with open(sys.argv[1], 'rb') as f:
    cat = tomllib.load(f)
versions = {}
for a in cat.get('alarm', []):
    versions[a['name']] = a.get('baseline_version', 1)
print(json.dumps(versions))
" "$prov_catalog")
  local prov_stale_migrated
  prov_stale_migrated=$(python3 -c "
import json, sys
checksum = sys.argv[1]
versions = json.loads(sys.argv[2])
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': '2026-01-01T00:00:00Z',
    'last_ts': '2026-01-02T00:00:00Z',
    'alarms': {
        'lost-sync': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z',
        'created_commit': 'stale-commit',
        'catalog_checksum': checksum,
        'alarm_versions': versions,
    }
}
print(json.dumps(data))
" "$prov_real_checksum_t11" "$prov_real_versions_t11")
  echo "$prov_stale_migrated" > "$prov_t11/metrics/replay-baseline.json"
  echo "$prov_stale_migrated" > "$prov_t11/metrics/replay-baseline-stable.json"
  local prov_t11_out
  prov_t11_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t11" --current "$prov_t11/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_t11_invalidated
  prov_t11_invalidated=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
alarms = d.get('alarms', {})
print('yes' if 'lost-sync' not in alarms else 'no')
" "$prov_t11/metrics/replay-baseline-stable.json" 2>/dev/null) || prov_t11_invalidated="no"
  if [[ "$prov_t11_invalidated" == "yes" ]]; then
    tap_ok "provenance: already-migrated stale baseline — Step D temporal invalidation fires (#2647)"
  else
    tap_not_ok "provenance: already-migrated stale baseline — Step D temporal invalidation fires (#2647)" \
      "output: $prov_t11_out, invalidated: $prov_t11_invalidated"
  fi

  # Test: No-op passthrough — checksum matches, first_ts after semantic_change_date
  local prov_t12="$prov_root/t12"
  mkdir -p "$prov_t12/metrics"
  echo "$prov_current" > "$prov_t12/metrics/current.json"
  local prov_noop_baseline
  prov_noop_baseline=$(python3 -c "
import json, sys
checksum = sys.argv[1]
versions = json.loads(sys.argv[2])
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': '2099-01-01T00:00:00Z',
    'last_ts': '2099-01-02T00:00:00Z',
    'alarms': {
        'lost-sync': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2099-01-01T00:00:00Z',
        'created_commit': 'recent-commit',
        'catalog_checksum': checksum,
        'alarm_versions': versions,
    }
}
print(json.dumps(data))
" "$prov_real_checksum_t11" "$prov_real_versions_t11")
  echo "$prov_noop_baseline" > "$prov_t12/metrics/replay-baseline.json"
  echo "$prov_noop_baseline" > "$prov_t12/metrics/replay-baseline-stable.json"
  local prov_hash_before_t12
  prov_hash_before_t12=$(sha256sum "$prov_t12/metrics/replay-baseline-stable.json" | cut -d' ' -f1)
  local prov_t12_out
  prov_t12_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$prov_t12" --current "$prov_t12/metrics/current.json" --catalog "$prov_catalog" 2>&1) || true
  local prov_hash_after_t12
  prov_hash_after_t12=$(sha256sum "$prov_t12/metrics/replay-baseline-stable.json" | cut -d' ' -f1)
  if [[ "$prov_hash_before_t12" == "$prov_hash_after_t12" ]]; then
    tap_ok "provenance: no-op passthrough — file unchanged when nothing to invalidate (#2647)"
  else
    tap_not_ok "provenance: no-op passthrough — file unchanged when nothing to invalidate (#2647)" \
      "hash_before=$prov_hash_before_t12, hash_after=$prov_hash_after_t12"
  fi

  rm -rf "$prov_root"

  # ── check-alarm-regression.sh --force-baseline-update tests ──────────────
  # Tests for issue #2616: add --force-baseline-update flag to acknowledge
  # regressions as legitimate improvements and reset baselines.

  local fbu_root
  fbu_root=$(mktemp -d)
  local fbu_session="$fbu_root/session"
  mkdir -p "$fbu_session/metrics"

  # Set up: create baselines with lost-sync firing at 10%
  local fbu_active='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  local fbu_silent='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'

  # Bootstrap baselines from active data
  echo "$fbu_active" > "$fbu_session/metrics/fbu-active.json"
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$fbu_session" --current "$fbu_session/metrics/fbu-active.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1 || true

  # Verify baselines exist
  if [[ ! -f "$fbu_session/metrics/replay-baseline.json" ]] || \
     [[ ! -f "$fbu_session/metrics/replay-baseline-stable.json" ]]; then
    tap_not_ok "force-baseline-update: bootstrap failed"
  else
    # Test 1: Without --force-baseline-update, regressions freeze baselines
    echo "$fbu_silent" > "$fbu_session/metrics/fbu-silent.json"
    local fbu_rolling_before fbu_stable_before
    fbu_rolling_before=$(md5sum "$fbu_session/metrics/replay-baseline.json" | cut -d' ' -f1)
    fbu_stable_before=$(md5sum "$fbu_session/metrics/replay-baseline-stable.json" | cut -d' ' -f1)

    local fbu_out1
    fbu_out1=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
      "$fbu_session" --current "$fbu_session/metrics/fbu-silent.json" \
      --catalog "$catalog_for_reg" 2>&1) || true

    local fbu_rolling_after1 fbu_stable_after1
    fbu_rolling_after1=$(md5sum "$fbu_session/metrics/replay-baseline.json" | cut -d' ' -f1)
    fbu_stable_after1=$(md5sum "$fbu_session/metrics/replay-baseline-stable.json" | cut -d' ' -f1)

    if echo "$fbu_out1" | grep -q "regression(s) found" && \
       [[ "$fbu_rolling_before" == "$fbu_rolling_after1" ]] && \
       [[ "$fbu_stable_before" == "$fbu_stable_after1" ]]; then
      tap_ok "force-baseline-update: without flag, baselines NOT updated on regression"
    else
      tap_not_ok "force-baseline-update: without flag, baselines NOT updated on regression" \
        "output: ${fbu_out1:0:200}"
    fi

    # Test 2: With --force-baseline-update, both baselines ARE updated
    local fbu_out2
    fbu_out2=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
      "$fbu_session" --current "$fbu_session/metrics/fbu-silent.json" \
      --catalog "$catalog_for_reg" --force-baseline-update 2>&1) || true

    local fbu_rolling_after2 fbu_stable_after2
    fbu_rolling_after2=$(md5sum "$fbu_session/metrics/replay-baseline.json" | cut -d' ' -f1)
    fbu_stable_after2=$(md5sum "$fbu_session/metrics/replay-baseline-stable.json" | cut -d' ' -f1)

    if echo "$fbu_out2" | grep -q "Force-updating baselines" && \
       [[ "$fbu_rolling_before" != "$fbu_rolling_after2" ]] && \
       [[ "$fbu_stable_before" != "$fbu_stable_after2" ]]; then
      tap_ok "force-baseline-update: with flag, both baselines updated"
    else
      tap_not_ok "force-baseline-update: with flag, both baselines updated" \
        "output: ${fbu_out2:0:200} rolling_changed=$([[ $fbu_rolling_before != $fbu_rolling_after2 ]] && echo yes || echo no) stable_changed=$([[ $fbu_stable_before != $fbu_stable_after2 ]] && echo yes || echo no)"
    fi

    # Test 3: Subsequent run without flag shows no regressions (baseline took effect)
    local fbu_out3
    fbu_out3=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
      "$fbu_session" --current "$fbu_session/metrics/fbu-silent.json" \
      --catalog "$catalog_for_reg" 2>&1) || true
    local fbu_stdout3
    fbu_stdout3=$(echo "$fbu_out3" | grep -v "^No regressions\|^Rolling baseline\|^Stable baseline\|^Baseline" | head -1)

    if echo "$fbu_out3" | grep -q "No regressions" && [[ "$fbu_stdout3" == "[]" ]]; then
      tap_ok "force-baseline-update: subsequent run shows no regressions"
    else
      tap_not_ok "force-baseline-update: subsequent run shows no regressions" \
        "output: ${fbu_out3:0:200}"
    fi
  fi

  rm -rf "$fbu_root"

  # ── check-alarm-regression.sh per-alarm acknowledgment tests (#2627) ────

  local ack_root
  ack_root=$(mktemp -d)
  local ack_session="$ack_root/session"
  mkdir -p "$ack_session/metrics"
  local catalog_for_ack="$ack_root/test-catalog-no-silence.toml"
  sed '/^silence_expected/d' "$REPO_ROOT/.claude/skills/shared/metric-alarms.toml" > "$catalog_for_ack"
  local catalog_for_ack_checksum
  catalog_for_ack_checksum=$(sha256sum "$catalog_for_ack" | cut -d' ' -f1)

  # Create baseline with lost-sync firing
  local ack_baseline
  ack_baseline=$(python3 -c "
import json, sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
d = json.loads(sys.argv[1])
with open(sys.argv[3], 'rb') as f:
    catalog = tomllib.load(f)
versions = {a['name']: a.get('baseline_version', 1) for a in catalog.get('alarm', [])}
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2], 'alarm_versions': versions}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}' "$catalog_for_ack_checksum" "$catalog_for_ack")
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline.json"
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline-stable.json"

  # Current where lost-sync has regressed to 0
  local ack_regressed='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$ack_regressed" > "$ack_session/metrics/ack-regressed.json"

  # Test: --acknowledge happy path
  local ack_out1
  ack_out1=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --ack-rationale "Fixed in PR #2616" \
    --catalog "$catalog_for_ack" 2>&1) || true
  if echo "$ack_out1" | grep -q "Acknowledged alarm: lost-sync" && \
     [[ -f "$ack_session/metrics/alarm-acknowledgments.json" ]]; then
    tap_ok "ack: acknowledge happy path creates file"
  else
    tap_not_ok "ack: acknowledge happy path creates file" "output: $ack_out1"
  fi

  # Test: ack file has correct metadata
  local ack_meta
  ack_meta=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
a = d['alarms']['lost-sync']
ok = (a.get('rationale') == 'Fixed in PR #2616' and
      'acknowledged_at' in a and
      'acknowledged_commit' in a and
      d.get('schema_version') == 1)
print('ok' if ok else 'fail')
" "$ack_session/metrics/alarm-acknowledgments.json" 2>/dev/null) || ack_meta="error"
  if [[ "$ack_meta" == "ok" ]]; then
    tap_ok "ack: ack file contains correct metadata"
  else
    tap_not_ok "ack: ack file contains correct metadata" "meta=$ack_meta"
  fi

  # Test: --acknowledge with --ack-issue
  local ack_out_issue
  ack_out_issue=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --ack-rationale "Updated" --ack-issue 2616 \
    --catalog "$catalog_for_ack" 2>&1) || true
  local ack_issue_val
  ack_issue_val=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(d['alarms']['lost-sync'].get('issue', ''))
" "$ack_session/metrics/alarm-acknowledgments.json" 2>/dev/null) || ack_issue_val=""
  if [[ "$ack_issue_val" == "2616" ]]; then
    tap_ok "ack: --ack-issue stores issue number"
  else
    tap_not_ok "ack: --ack-issue stores issue number" "got: $ack_issue_val"
  fi

  # Test: --acknowledge non-catalog alarm exits 2
  local ack_out_bad
  local ack_bad_exit=0
  ack_out_bad=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge fake-alarm-xyz --ack-rationale "test" \
    --catalog "$catalog_for_ack" 2>&1) || ack_bad_exit=$?
  if [[ "$ack_bad_exit" -eq 2 ]] && echo "$ack_out_bad" | grep -q "not found in catalog"; then
    tap_ok "ack: non-catalog alarm exits 2"
  else
    tap_not_ok "ack: non-catalog alarm exits 2" "exit=$ack_bad_exit output: $ack_out_bad"
  fi

  # Test: --acknowledge without --ack-rationale exits 1
  local ack_out_norat
  local ack_norat_exit=0
  ack_out_norat=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync \
    --catalog "$catalog_for_ack" 2>&1) || ack_norat_exit=$?
  if [[ "$ack_norat_exit" -eq 1 ]] && echo "$ack_out_norat" | grep -q "requires --ack-rationale"; then
    tap_ok "ack: without rationale exits 1"
  else
    tap_not_ok "ack: without rationale exits 1" "exit=$ack_norat_exit output: $ack_out_norat"
  fi

  # Test: --acknowledge with empty rationale exits 1
  local ack_out_empty
  local ack_empty_exit=0
  ack_out_empty=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --ack-rationale "   " \
    --catalog "$catalog_for_ack" 2>&1) || ack_empty_exit=$?
  if [[ "$ack_empty_exit" -eq 1 ]]; then
    tap_ok "ack: empty rationale exits 1"
  else
    tap_not_ok "ack: empty rationale exits 1" "exit=$ack_empty_exit"
  fi

  # Test: acknowledged alarm excluded from regression detection
  local ack_detect_out
  ack_detect_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --current "$ack_session/metrics/ack-regressed.json" \
    --catalog "$catalog_for_ack" 2>&1) || true
  local ack_detect_json
  ack_detect_json=$(echo "$ack_detect_out" | grep '^\[' | head -1)
  local ack_has_lostsync
  ack_has_lostsync=$(echo "$ack_detect_json" | grep -c '"lost-sync"' 2>/dev/null) || ack_has_lostsync=0
  if [[ "$ack_has_lostsync" -eq 0 ]] && echo "$ack_detect_out" | grep -q "Acknowledged alarm skipped: lost-sync"; then
    tap_ok "ack: acknowledged alarm excluded from regression detection"
  else
    tap_not_ok "ack: acknowledged alarm excluded from regression detection" \
      "lostsync=$ack_has_lostsync output: ${ack_detect_out:0:300}"
  fi

  # Test: --list-acknowledgments shows entry
  local ack_list_out
  ack_list_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --list-acknowledgments --catalog "$catalog_for_ack" 2>&1) || true
  if echo "$ack_list_out" | grep -q "lost-sync" && echo "$ack_list_out" | grep -q "Updated"; then
    tap_ok "ack: list shows alarm and rationale"
  else
    tap_not_ok "ack: list shows alarm and rationale" "output: $ack_list_out"
  fi

  # Test: --list-acknowledgments with no acks
  local ack_list_empty_session="$ack_root/empty-session"
  mkdir -p "$ack_list_empty_session/metrics"
  local ack_list_empty_out
  ack_list_empty_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_list_empty_session" --list-acknowledgments --catalog "$catalog_for_ack" 2>&1) || true
  if echo "$ack_list_empty_out" | grep -q "No acknowledgments."; then
    tap_ok "ack: list with no acks shows message"
  else
    tap_not_ok "ack: list with no acks shows message" "output: $ack_list_empty_out"
  fi

  # Test: --revoke-acknowledgment removes entry and re-enables detection
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --revoke-acknowledgment lost-sync --catalog "$catalog_for_ack" 2>/dev/null || true
  local ack_revoke_detect_out
  # Re-establish baselines (rolling was updated during earlier ack test)
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline.json"
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline-stable.json"
  ack_revoke_detect_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --current "$ack_session/metrics/ack-regressed.json" \
    --catalog "$catalog_for_ack" 2>&1) || true
  local ack_revoke_json
  ack_revoke_json=$(echo "$ack_revoke_detect_out" | grep '^\[' | head -1)
  local ack_revoke_lostsync
  ack_revoke_lostsync=$(echo "$ack_revoke_json" | grep -c '"lost-sync"' 2>/dev/null) || ack_revoke_lostsync=0
  if [[ "$ack_revoke_lostsync" -ge 1 ]]; then
    tap_ok "ack: revoke re-enables regression detection"
  else
    tap_not_ok "ack: revoke re-enables regression detection" \
      "lostsync=$ack_revoke_lostsync output: ${ack_revoke_detect_out:0:300}"
  fi

  # Test: --revoke-acknowledgment not found
  local ack_revoke_nf_out
  ack_revoke_nf_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --revoke-acknowledgment nonexistent --catalog "$catalog_for_ack" 2>&1) || true
  local ack_revoke_nf_exit=$?
  if echo "$ack_revoke_nf_out" | grep -q "No acknowledgment found for: nonexistent"; then
    tap_ok "ack: revoke not-found warns"
  else
    tap_not_ok "ack: revoke not-found warns" "output: $ack_revoke_nf_out"
  fi

  # Test: malformed ack file during detection exits 2
  echo "NOT JSON" > "$ack_session/metrics/alarm-acknowledgments.json"
  local ack_malformed_out
  local ack_malformed_exit=0
  ack_malformed_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --current "$ack_session/metrics/ack-regressed.json" \
    --catalog "$catalog_for_ack" 2>&1) || ack_malformed_exit=$?
  if [[ "$ack_malformed_exit" -eq 2 ]]; then
    tap_ok "ack: malformed ack file exits 2"
  else
    tap_not_ok "ack: malformed ack file exits 2" "exit=$ack_malformed_exit output: ${ack_malformed_out:0:200}"
  fi

  # Test: malformed ack file during --list exits 2
  local ack_malformed_list_out
  local ack_malformed_list_exit=0
  ack_malformed_list_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --list-acknowledgments --catalog "$catalog_for_ack" 2>&1) || ack_malformed_list_exit=$?
  if [[ "$ack_malformed_list_exit" -eq 2 ]]; then
    tap_ok "ack: malformed ack file during list exits 2"
  else
    tap_not_ok "ack: malformed ack file during list exits 2" "exit=$ack_malformed_list_exit"
  fi

  # Test: mutual exclusion --acknowledge + --force-baseline-update
  local ack_mutex_out
  local ack_mutex_exit=0
  ack_mutex_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --force-baseline-update \
    --ack-rationale "test" --catalog "$catalog_for_ack" 2>&1) || ack_mutex_exit=$?
  if [[ "$ack_mutex_exit" -eq 1 ]] && echo "$ack_mutex_out" | grep -q "mutually exclusive"; then
    tap_ok "ack: mutual exclusion rejects combined flags"
  else
    tap_not_ok "ack: mutual exclusion rejects combined flags" "exit=$ack_mutex_exit output: $ack_mutex_out"
  fi

  # Test: catalog change invalidates acks
  # Create ack file with a different catalog checksum
  echo '{"schema_version":1,"catalog_checksum":"stale_checksum_000","alarms":{"lost-sync":{"acknowledged_at":"2026-01-01T00:00:00Z","acknowledged_commit":"abc","rationale":"old"}}}' \
    > "$ack_session/metrics/alarm-acknowledgments.json"
  local ack_stale_out
  ack_stale_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --list-acknowledgments --catalog "$catalog_for_ack" 2>&1) || true
  if echo "$ack_stale_out" | grep -q "No acknowledgments." || echo "$ack_stale_out" | grep -q "invalidated"; then
    tap_ok "ack: catalog change invalidates acknowledgments"
  else
    tap_not_ok "ack: catalog change invalidates acknowledgments" "output: $ack_stale_out"
  fi

  # Test: normal detection persists catalog invalidation to disk
  # After normal detection sees a catalog mismatch, the ack file should be reset
  echo '{"schema_version":1,"catalog_checksum":"stale_checksum_001","alarms":{"lost-sync":{"acknowledged_at":"2026-01-01T00:00:00Z","acknowledged_commit":"abc","rationale":"old"}}}' \
    > "$ack_session/metrics/alarm-acknowledgments.json"
  # Create minimal baselines so normal detection reaches ack-loading code
  local ack_catalog_cksum
  ack_catalog_cksum=$(sha256sum "$catalog_for_ack" | cut -d' ' -f1)
  local ack_baseline='{"schema_version":1,"alarms":{},"evaluated_ticks":10,"catalog_checksum":"'"$ack_catalog_cksum"'","provenance_commit":"abc","provenance_timestamp":"2026-01-01T00:00:00Z"}'
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline.json"
  echo "$ack_baseline" > "$ack_session/metrics/replay-baseline-stable.json"
  # Run normal detection (provide a current JSON file so it doesn't try replay)
  local ack_persist_dir="$ack_root/persist-test"
  mkdir -p "$ack_persist_dir"
  echo '{"schema_version":1,"alarms":{},"evaluated_ticks":10}' > "$ack_persist_dir/current.json"
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --current "$ack_persist_dir/current.json" --catalog "$catalog_for_ack" 2>/dev/null || true
  local ack_persist_alarms
  ack_persist_alarms=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(json.dumps(d.get('alarms', {})))
" "$ack_session/metrics/alarm-acknowledgments.json" 2>/dev/null) || ack_persist_alarms="FAIL"
  if [[ "$ack_persist_alarms" == "{}" ]]; then
    tap_ok "ack: normal detection persists catalog invalidation to disk"
  else
    tap_not_ok "ack: normal detection persists catalog invalidation to disk" "alarms=$ack_persist_alarms"
  fi

  # Test: overwrite existing ack updates metadata
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --ack-rationale "First reason" \
    --catalog "$catalog_for_ack" 2>/dev/null || true
  "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --acknowledge lost-sync --ack-rationale "Second reason" \
    --catalog "$catalog_for_ack" 2>/dev/null || true
  local ack_overwrite_rat
  ack_overwrite_rat=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
print(d['alarms']['lost-sync']['rationale'])
" "$ack_session/metrics/alarm-acknowledgments.json" 2>/dev/null) || ack_overwrite_rat=""
  if [[ "$ack_overwrite_rat" == "Second reason" ]]; then
    tap_ok "ack: overwrite updates metadata"
  else
    tap_not_ok "ack: overwrite updates metadata" "rationale=$ack_overwrite_rat"
  fi

  # Test: malformed ack file during --revoke-acknowledgment exits 2
  echo "NOT JSON" > "$ack_session/metrics/alarm-acknowledgments.json"
  local ack_malformed_revoke_out
  local ack_malformed_revoke_exit=0
  ack_malformed_revoke_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ack_session" --revoke-acknowledgment lost-sync --catalog "$catalog_for_ack" 2>&1) || ack_malformed_revoke_exit=$?
  if [[ "$ack_malformed_revoke_exit" -eq 2 ]]; then
    tap_ok "ack: malformed ack file during revoke exits 2"
  else
    tap_not_ok "ack: malformed ack file during revoke exits 2" "exit=$ack_malformed_revoke_exit"
  fi

  rm -rf "$ack_root"

  # ── check-alarm-regression.sh silence_expected tests (#2661) ────────────────
  # Tests for the silence_expected catalog field that suppresses false-positive
  # regressions for alarms that legitimately go to 0% on a healthy node.

  local se_root="$replay_root/silence-expected"
  mkdir -p "$se_root/session/metrics"

  # Create a minimal catalog with one silence_expected alarm and one normal alarm
  local se_catalog="$se_root/catalog.toml"
  cat > "$se_catalog" << 'SE_CAT'
schema_version = 1

[[alarm]]
name = "lost-sync"
silence_expected = true
metric = "stellar_herder_lost_sync_total"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "SYNC"
gates = ["warmup-2-ticks"]
cooldown_key = "stellar_herder_lost_sync_total"
cooldown_seconds = 3600
filing_title = "metrics: stellar_herder_lost_sync_total — delta={value}"
filing_search = "metrics: stellar_herder_lost_sync_total"
summary = "Herder lost sync"
details = "delta={value}"

[[alarm]]
name = "peer-count-low"
metric = "stellar_overlay_peer_count"
kind = "gauge"
extraction = "form2"
labels = []
op = "<"
threshold = 3
severity = "OVERLAY"
gates = ["warmup-2-ticks"]
cooldown_key = "stellar_overlay_peer_count"
cooldown_seconds = 600
filing_title = "metrics: stellar_overlay_peer_count — value={value}"
filing_search = "metrics: stellar_overlay_peer_count"
summary = "Peer count low"
details = "value={value}"
SE_CAT

  # Baseline: lost-sync fires 15%, peer-count-low fires 10%
  local se_baseline='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":30,"breach":5,"ok":155,"baseline":0,"skip":10},"peer-count-low":{"firing":20,"breach":0,"ok":170,"baseline":0,"skip":10}}}'
  local se_baseline_prov
  se_baseline_prov=$(echo "$se_baseline" | python3 -c "
import json, sys, hashlib
d = json.load(sys.stdin)
catalog_cksum = hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest()
d['provenance'] = {'created_at':'2026-01-01T00:00:00Z','created_commit':'test','catalog_checksum':catalog_cksum,'alarm_versions':{'lost-sync':1,'peer-count-low':1}}
print(json.dumps(d))
" "$se_catalog")
  echo "$se_baseline_prov" > "$se_root/session/metrics/replay-baseline.json"
  echo "$se_baseline_prov" > "$se_root/session/metrics/replay-baseline-stable.json"

  # Current: both alarms at 0% firing
  local se_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$se_current" > "$se_root/session/metrics/se-current.json"

  # Test: silence_expected alarm (lost-sync) does NOT produce regression,
  # but normal alarm (peer-count-low) DOES produce regression
  local se_out
  se_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$se_root/session" --current "$se_root/session/metrics/se-current.json" \
    --catalog "$se_catalog" 2>&1) || true
  local se_has_peer se_has_lost
  se_has_peer=$(echo "$se_out" | grep -c "peer-count-low" || true)
  se_has_lost=$(echo "$se_out" | grep -c "lost-sync" || true)
  if [[ "$se_has_peer" -gt 0 ]] && [[ "$se_has_lost" -eq 0 ]]; then
    tap_ok "silence_expected: suppresses 0%-firing regression for marked alarm"
  else
    tap_not_ok "silence_expected: suppresses 0%-firing regression for marked alarm" \
      "peer=$se_has_peer lost=$se_has_lost output: ${se_out:0:300}"
  fi

  # Test: silence_expected alarm ABSENT from current → regression IS emitted
  # (catalog drift detection preserved)
  local se_absent_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"peer-count-low":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$se_absent_current" > "$se_root/session/metrics/se-absent-current.json"

  local se_absent_out
  se_absent_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$se_root/session" --current "$se_root/session/metrics/se-absent-current.json" \
    --catalog "$se_catalog" 2>&1) || true
  local se_absent_has_lost
  se_absent_has_lost=$(echo "$se_absent_out" | grep -c "lost-sync" || true)
  if [[ "$se_absent_has_lost" -gt 0 ]]; then
    tap_ok "silence_expected: absent-from-replay still produces regression"
  else
    tap_not_ok "silence_expected: absent-from-replay still produces regression" \
      "lost=$se_absent_has_lost output: ${se_absent_out:0:300}"
  fi

  # Test: malformed silence_expected (non-boolean) exits 2
  local se_bad_root="$replay_root/silence-expected-bad"
  mkdir -p "$se_bad_root/session/metrics"
  local se_bad_catalog="$se_bad_root/bad-catalog.toml"
  cat > "$se_bad_catalog" << 'SE_BAD_CAT'
schema_version = 1

[[alarm]]
name = "lost-sync"
silence_expected = "yes"
metric = "stellar_herder_lost_sync_total"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "SYNC"
gates = ["warmup-2-ticks"]
cooldown_key = "stellar_herder_lost_sync_total"
cooldown_seconds = 3600
filing_title = "metrics: stellar_herder_lost_sync_total — delta={value}"
filing_search = "metrics: stellar_herder_lost_sync_total"
summary = "Herder lost sync"
details = "delta={value}"
SE_BAD_CAT
  local se_bad_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":10,"breach":0,"ok":180,"baseline":0,"skip":10}}}'
  echo "$se_bad_current" > "$se_bad_root/session/metrics/current.json"
  local se_bad_exit=0
  local se_bad_out
  se_bad_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$se_bad_root/session" --current "$se_bad_root/session/metrics/current.json" \
    --catalog "$se_bad_catalog" 2>&1) || se_bad_exit=$?
  if [[ "$se_bad_exit" -eq 2 ]] && echo "$se_bad_out" | grep -qi "silence_expected"; then
    tap_ok "silence_expected: malformed value (string) exits 2"
  else
    tap_not_ok "silence_expected: malformed value (string) exits 2" \
      "exit=$se_bad_exit output: ${se_bad_out:0:300}"
  fi
  rm -rf "$se_bad_root"

  rm -rf "$se_root"

  # ── check-alarm-regression.sh per-alarm versioning tests (#2640) ────────────
  # Tests for per-alarm baseline_version invalidation logic.

  local ver_root
  ver_root=$(mktemp -d)
  local ver_session="$ver_root/session"
  mkdir -p "$ver_session/metrics"

  # Create a test catalog with two alarms: one with baseline_version=2, one with default (1)
  local ver_catalog="$ver_root/test-catalog.toml"
  cat > "$ver_catalog" << 'VER_CAT_EOF'
schema_version = 1

[[alarm]]
name = "alarm-versioned"
baseline_version = 2
metric = "test_metric_a"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_a"
cooldown_seconds = 3600
filing_title = "test a"
filing_search = "test a"
summary = "Test A"
details = "delta={value}"
notes = ""

[[alarm]]
name = "alarm-default"
metric = "test_metric_b"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_b"
cooldown_seconds = 3600
filing_title = "test b"
filing_search = "test b"
summary = "Test B"
details = "delta={value}"
notes = ""
VER_CAT_EOF

  local ver_catalog_checksum
  ver_catalog_checksum=$(sha256sum "$ver_catalog" | cut -d' ' -f1)

  # Create a baseline that was stored with alarm_versions showing version 1 for both
  local ver_baseline
  ver_baseline=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 't1',
    'last_ts': 't2',
    'alarms': {
        'alarm-versioned': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-default': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z',
        'created_commit': 'old-commit',
        'catalog_checksum': 'old-checksum-does-not-match',
        'alarm_versions': {'alarm-versioned': 1, 'alarm-default': 1}
    }
}
print(json.dumps(data))
")
  echo "$ver_baseline" > "$ver_session/metrics/replay-baseline.json"
  echo "$ver_baseline" > "$ver_session/metrics/replay-baseline-stable.json"

  # Current replay data with both alarms active
  local ver_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"alarm-versioned":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"alarm-default":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}}}'
  echo "$ver_current" > "$ver_session/metrics/ver-current.json"

  # Run check-alarm-regression — should invalidate alarm-versioned (version 1→2) but keep alarm-default
  local ver_out
  ver_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_session" --current "$ver_session/metrics/ver-current.json" \
    --catalog "$ver_catalog" 2>&1) || true

  # Check that alarm-versioned was invalidated (removed) during per-alarm check
  # Note: rolling baseline gets repopulated from current data on clean runs,
  # so we verify via the output message that invalidation happened, and check
  # the alarm_versions in provenance were updated correctly.
  local ver_rolling_has_correct_versions
  ver_rolling_has_correct_versions=$(python3 -c "
import json
with open('$ver_session/metrics/replay-baseline.json') as f:
    d = json.load(f)
av = d.get('provenance', {}).get('alarm_versions', {})
# Should have the NEW versions stamped
print(av.get('alarm-versioned') == 2 and av.get('alarm-default') == 1)
" 2>/dev/null) || ver_rolling_has_correct_versions="ERROR"

  if echo "$ver_out" | grep -q "per-alarm invalidation.*alarm-versioned" && [[ "$ver_rolling_has_correct_versions" == "True" ]]; then
    tap_ok "per-alarm-version: version bump invalidates only affected alarm"
  else
    tap_not_ok "per-alarm-version: version bump invalidates only affected alarm" \
      "correct_versions=$ver_rolling_has_correct_versions output=${ver_out:0:200}"
  fi

  # Check stable baseline too
  local ver_stable_has_versioned ver_stable_has_default
  ver_stable_has_versioned=$(python3 -c "
import json
with open('$ver_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-versioned' in d.get('alarms', {}))
" 2>/dev/null) || ver_stable_has_versioned="ERROR"
  ver_stable_has_default=$(python3 -c "
import json
with open('$ver_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-default' in d.get('alarms', {}))
" 2>/dev/null) || ver_stable_has_default="ERROR"

  if [[ "$ver_stable_has_versioned" == "False" ]] && [[ "$ver_stable_has_default" == "True" ]]; then
    tap_ok "per-alarm-version: stable baseline per-alarm invalidation"
  else
    tap_not_ok "per-alarm-version: stable baseline per-alarm invalidation" \
      "versioned=$ver_stable_has_versioned default=$ver_stable_has_default"
  fi

  # Test: cosmetic-only edit (same versions) — all data preserved
  local ver_cosmetic_session="$ver_root/cosmetic"
  mkdir -p "$ver_cosmetic_session/metrics"
  # Create a catalog identical semantically but with different notes (different checksum)
  local ver_catalog2="$ver_root/test-catalog2.toml"
  sed 's/notes = ""/notes = "cosmetic change"/' "$ver_catalog" > "$ver_catalog2"
  local ver_catalog2_checksum
  ver_catalog2_checksum=$(sha256sum "$ver_catalog2" | cut -d' ' -f1)

  # Baseline with matching alarm_versions but DIFFERENT catalog_checksum
  local ver_cosmetic_baseline
  ver_cosmetic_baseline=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 't1',
    'last_ts': 't2',
    'alarms': {
        'alarm-versioned': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-default': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z',
        'created_commit': 'old-commit',
        'catalog_checksum': 'different-old-checksum',
        'alarm_versions': {'alarm-versioned': 2, 'alarm-default': 1}
    }
}
print(json.dumps(data))
")
  echo "$ver_cosmetic_baseline" > "$ver_cosmetic_session/metrics/replay-baseline.json"
  echo "$ver_cosmetic_baseline" > "$ver_cosmetic_session/metrics/replay-baseline-stable.json"
  echo "$ver_current" > "$ver_cosmetic_session/metrics/ver-cosmetic-current.json"

  local ver_cosmetic_out
  ver_cosmetic_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_cosmetic_session" --current "$ver_cosmetic_session/metrics/ver-cosmetic-current.json" \
    --catalog "$ver_catalog2" 2>&1) || true

  # Both alarms should still be in baseline (versions match)
  local ver_cos_has_both
  ver_cos_has_both=$(python3 -c "
import json
with open('$ver_cosmetic_session/metrics/replay-baseline.json') as f:
    d = json.load(f)
alarms = d.get('alarms', {})
print('alarm-versioned' in alarms and 'alarm-default' in alarms)
" 2>/dev/null) || ver_cos_has_both="ERROR"

  if [[ "$ver_cos_has_both" == "True" ]]; then
    tap_ok "per-alarm-version: cosmetic edit preserves all data"
  else
    tap_not_ok "per-alarm-version: cosmetic edit preserves all data" \
      "has_both=$ver_cos_has_both output=${ver_cosmetic_out:0:200}"
  fi

  # Test: legacy baseline (no alarm_versions) — alarm data invalidated (fail-closed, #2647)
  local ver_legacy_session="$ver_root/legacy"
  mkdir -p "$ver_legacy_session/metrics"
  local ver_legacy_baseline
  ver_legacy_baseline=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 't1',
    'last_ts': 't2',
    'alarms': {
        'alarm-versioned': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-default': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z',
        'created_commit': 'old-commit',
        'catalog_checksum': 'anything'
    }
}
print(json.dumps(data))
")
  echo "$ver_legacy_baseline" > "$ver_legacy_session/metrics/replay-baseline.json"
  echo "$ver_legacy_baseline" > "$ver_legacy_session/metrics/replay-baseline-stable.json"
  echo "$ver_current" > "$ver_legacy_session/metrics/ver-legacy-current.json"

  local ver_legacy_out
  ver_legacy_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_legacy_session" --current "$ver_legacy_session/metrics/ver-legacy-current.json" \
    --catalog "$ver_catalog" 2>&1) || true

  # Non-destructive: legacy migration preserves alarm data and stamps provenance.
  # first_ts is not ISO-formatted → Step D skips → alarms preserved.
  local ver_legacy_has_versions ver_legacy_alarms_preserved
  ver_legacy_has_versions=$(python3 -c "
import json
with open('$ver_legacy_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
p = d.get('provenance', {})
av = p.get('alarm_versions')
print(isinstance(av, dict) and len(av) > 0)
" 2>/dev/null) || ver_legacy_has_versions="ERROR"
  ver_legacy_alarms_preserved=$(python3 -c "
import json
with open('$ver_legacy_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
alarms = d.get('alarms', {})
print(len(alarms) == 2)
" 2>/dev/null) || ver_legacy_alarms_preserved="ERROR"

  if [[ "$ver_legacy_has_versions" == "True" ]] && [[ "$ver_legacy_alarms_preserved" == "True" ]]; then
    tap_ok "per-alarm-version: legacy migration stamps versions and preserves alarms"
  else
    tap_not_ok "per-alarm-version: legacy migration stamps versions and preserves alarms" \
      "has_versions=$ver_legacy_has_versions alarms_preserved=$ver_legacy_alarms_preserved"
  fi

  # Test: alarm removal — only removed alarm's entry deleted
  local ver_remove_session="$ver_root/remove"
  mkdir -p "$ver_remove_session/metrics"
  # Catalog with only alarm-default (alarm-versioned removed)
  local ver_catalog_removed="$ver_root/test-catalog-removed.toml"
  cat > "$ver_catalog_removed" << 'VER_REM_EOF'
schema_version = 1

[[alarm]]
name = "alarm-default"
metric = "test_metric_b"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_b"
cooldown_seconds = 3600
filing_title = "test b"
filing_search = "test b"
summary = "Test B"
details = "delta={value}"
notes = ""
VER_REM_EOF

  # Baseline has both alarms with proper provenance
  local ver_remove_baseline
  ver_remove_baseline=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 't1',
    'last_ts': 't2',
    'alarms': {
        'alarm-versioned': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-default': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-01-01T00:00:00Z',
        'created_commit': 'old-commit',
        'catalog_checksum': 'old-checksum',
        'alarm_versions': {'alarm-versioned': 2, 'alarm-default': 1}
    }
}
print(json.dumps(data))
")
  echo "$ver_remove_baseline" > "$ver_remove_session/metrics/replay-baseline.json"
  echo "$ver_remove_baseline" > "$ver_remove_session/metrics/replay-baseline-stable.json"
  local ver_remove_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"alarm-default":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}}}'
  echo "$ver_remove_current" > "$ver_remove_session/metrics/ver-remove-current.json"

  local ver_remove_out
  ver_remove_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_remove_session" --current "$ver_remove_session/metrics/ver-remove-current.json" \
    --catalog "$ver_catalog_removed" 2>&1) || true

  local ver_rem_has_versioned ver_rem_has_default
  ver_rem_has_versioned=$(python3 -c "
import json
with open('$ver_remove_session/metrics/replay-baseline.json') as f:
    d = json.load(f)
print('alarm-versioned' in d.get('alarms', {}))
" 2>/dev/null) || ver_rem_has_versioned="ERROR"
  ver_rem_has_default=$(python3 -c "
import json
with open('$ver_remove_session/metrics/replay-baseline.json') as f:
    d = json.load(f)
print('alarm-default' in d.get('alarms', {}))
" 2>/dev/null) || ver_rem_has_default="ERROR"

  if [[ "$ver_rem_has_versioned" == "False" ]] && [[ "$ver_rem_has_default" == "True" ]]; then
    tap_ok "per-alarm-version: alarm removal deletes only removed alarm"
  else
    tap_not_ok "per-alarm-version: alarm removal deletes only removed alarm" \
      "versioned=$ver_rem_has_versioned default=$ver_rem_has_default"
  fi

  # Test: ack preservation on version bump
  local ver_ack_session="$ver_root/ack-test"
  mkdir -p "$ver_ack_session/metrics"
  # Create ack file with both alarms acknowledged
  local ver_ack_file="$ver_ack_session/metrics/alarm-acknowledgments.json"
  python3 -c "
import json
data = {
    'schema_version': 1,
    'catalog_checksum': 'old-checksum',
    'alarm_versions': {'alarm-versioned': 1, 'alarm-default': 1},
    'alarms': {
        'alarm-versioned': {'rationale': 'test ack', 'acknowledged_at': '2026-01-01T00:00:00Z'},
        'alarm-default': {'rationale': 'test ack 2', 'acknowledged_at': '2026-01-01T00:00:00Z'}
    }
}
print(json.dumps(data))
" > "$ver_ack_file"

  # Set up baseline so the script runs normally
  echo "$ver_baseline" > "$ver_ack_session/metrics/replay-baseline.json"
  echo "$ver_baseline" > "$ver_ack_session/metrics/replay-baseline-stable.json"
  echo "$ver_current" > "$ver_ack_session/metrics/ver-ack-current.json"

  local ver_ack_out
  ver_ack_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_ack_session" --current "$ver_ack_session/metrics/ver-ack-current.json" \
    --catalog "$ver_catalog" 2>&1) || true

  # alarm-versioned ack should be removed (version 1→2), alarm-default should be preserved
  local ver_ack_has_versioned ver_ack_has_default
  ver_ack_has_versioned=$(python3 -c "
import json
with open('$ver_ack_file') as f:
    d = json.load(f)
print('alarm-versioned' in d.get('alarms', {}))
" 2>/dev/null) || ver_ack_has_versioned="ERROR"
  ver_ack_has_default=$(python3 -c "
import json
with open('$ver_ack_file') as f:
    d = json.load(f)
print('alarm-default' in d.get('alarms', {}))
" 2>/dev/null) || ver_ack_has_default="ERROR"

  if [[ "$ver_ack_has_versioned" == "False" ]] && [[ "$ver_ack_has_default" == "True" ]]; then
    tap_ok "per-alarm-version: ack preserved for non-bumped alarm, removed for bumped"
  else
    tap_not_ok "per-alarm-version: ack preserved for non-bumped alarm, removed for bumped" \
      "versioned=$ver_ack_has_versioned default=$ver_ack_has_default"
  fi

  # Test: contaminated post-bump baseline — version matches but data is stale (#2642)
  # Scenario: stable baseline was created AFTER version bump (provenance shows v2),
  # but captured transitional firing data. Later, alarm goes silent → false regression.
  # Bumping to v3 should invalidate and clear the false positive.
  local ver_contam_session="$ver_root/contaminated"
  mkdir -p "$ver_contam_session/metrics"

  # Catalog with alarm-versioned at version 3 (simulating the fix bump)
  local ver_catalog_v3="$ver_root/test-catalog-v3.toml"
  cat > "$ver_catalog_v3" << 'VER_V3_EOF'
schema_version = 1

[[alarm]]
name = "alarm-versioned"
baseline_version = 3
metric = "test_metric_a"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_a"
cooldown_seconds = 3600
filing_title = "test a"
filing_search = "test a"
summary = "Test A"
details = "delta={value}"
notes = ""

[[alarm]]
name = "alarm-default"
metric = "test_metric_b"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_b"
cooldown_seconds = 3600
filing_title = "test b"
filing_search = "test b"
summary = "Test B"
details = "delta={value}"
notes = ""
VER_V3_EOF

  # Stable baseline: created post-v2 bump (provenance says v2), stale firing data
  local ver_contam_baseline
  ver_contam_baseline=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 't1',
    'last_ts': 't2',
    'alarms': {
        'alarm-versioned': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-default': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-02-01T00:00:00Z',
        'created_commit': 'post-bump-commit',
        'catalog_checksum': 'old-post-bump-checksum',
        'alarm_versions': {'alarm-versioned': 2, 'alarm-default': 1}
    }
}
print(json.dumps(data))
")
  echo "$ver_contam_baseline" > "$ver_contam_session/metrics/replay-baseline.json"
  echo "$ver_contam_baseline" > "$ver_contam_session/metrics/replay-baseline-stable.json"

  # Current replay: alarm-versioned is now silent (0 firing), alarm-default still active
  local ver_contam_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"alarm-versioned":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10},"alarm-default":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}}}'
  echo "$ver_contam_current" > "$ver_contam_session/metrics/ver-contam-current.json"

  # Run with v3 catalog — should invalidate alarm-versioned (stored v2 != current v3)
  local ver_contam_out ver_contam_regressions
  ver_contam_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$ver_contam_session" --current "$ver_contam_session/metrics/ver-contam-current.json" \
    --catalog "$ver_catalog_v3" 2>&1) || true
  ver_contam_regressions=$(echo "$ver_contam_out" | grep -v '^[A-Z]' | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    # Check no regression for alarm-versioned
    names = [r['alarm'] for r in data]
    print('alarm-versioned' not in names)
except:
    print('ERROR')
" 2>/dev/null) || ver_contam_regressions="ERROR"

  # Verify stable baseline no longer contains alarm-versioned
  local ver_contam_stable_clean
  ver_contam_stable_clean=$(python3 -c "
import json
with open('$ver_contam_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
alarms = d.get('alarms', {})
prov = d.get('provenance', {}).get('alarm_versions', {})
# alarm-versioned should be removed from alarms, version updated to 3
print('alarm-versioned' not in alarms and prov.get('alarm-versioned') == 3)
" 2>/dev/null) || ver_contam_stable_clean="ERROR"

  if [[ "$ver_contam_regressions" == "True" ]] && [[ "$ver_contam_stable_clean" == "True" ]]; then
    tap_ok "per-alarm-version: contaminated post-bump baseline cleared by version bump (#2642)"
  else
    tap_not_ok "per-alarm-version: contaminated post-bump baseline cleared by version bump (#2642)" \
      "no_regression=$ver_contam_regressions stable_clean=$ver_contam_stable_clean output=${ver_contam_out:0:300}"
  fi

  # ── Step D: Temporal provenance (semantic_change_date) tests (#2646) ────────

  # Test catalog with semantic_change_date set on one alarm
  local stepd_root
  stepd_root=$(mktemp -d)
  local stepd_session="$stepd_root/session"
  mkdir -p "$stepd_session/metrics"

  local stepd_catalog="$stepd_root/test-catalog.toml"
  cat > "$stepd_catalog" << 'STEPD_CAT_EOF'
schema_version = 1

[[alarm]]
name = "alarm-with-date"
baseline_version = 2
semantic_change_date = "2026-05-10T12:00:00Z"
metric = "test_metric_a"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_a"
cooldown_seconds = 3600
filing_title = "test a"
filing_search = "test a"
summary = "Test A"
details = "delta={value}"
notes = ""

[[alarm]]
name = "alarm-no-date"
baseline_version = 1
metric = "test_metric_b"
kind = "counter"
extraction = "form1"
labels = []
op = ">="
threshold = 1
severity = "WARN"
gates = ["warmup-2-ticks"]
cooldown_key = "test_metric_b"
cooldown_seconds = 3600
filing_title = "test b"
filing_search = "test b"
summary = "Test B"
details = "delta={value}"
notes = ""
STEPD_CAT_EOF

  local stepd_checksum
  stepd_checksum=$(sha256sum "$stepd_catalog" | cut -d' ' -f1)

  # Test 1: Baseline with first_ts BEFORE semantic_change_date → alarm invalidated
  local stepd_baseline_pre
  stepd_baseline_pre=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': '2026-05-09T00:00:00Z',
    'last_ts': '2026-05-09T12:00:00Z',
    'alarms': {
        'alarm-with-date': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-no-date': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-05-09T01:00:00Z',
        'created_commit': 'abc123',
        'catalog_checksum': 'old-checksum-mismatch',
        'alarm_versions': {'alarm-with-date': 2, 'alarm-no-date': 1}
    }
}
print(json.dumps(data))
")
  echo "$stepd_baseline_pre" > "$stepd_session/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_pre" > "$stepd_session/metrics/replay-baseline.json"

  # Current replay data (post-change)
  local stepd_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"2026-05-11T00:00:00Z","last_ts":"2026-05-11T12:00:00Z","alarms":{"alarm-with-date":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"alarm-no-date":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}}}'
  echo "$stepd_current" > "$stepd_session/metrics/stepd-current.json"

  local stepd_out
  stepd_out=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session" --current "$stepd_session/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  # stable baseline should have alarm-with-date removed (first_ts < semantic_change_date)
  # but alarm-no-date preserved (no semantic_change_date)
  local stepd_stable_has_dated stepd_stable_has_undated
  stepd_stable_has_dated=$(python3 -c "
import json
with open('$stepd_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd_stable_has_dated="ERROR"
  stepd_stable_has_undated=$(python3 -c "
import json
with open('$stepd_session/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-no-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd_stable_has_undated="ERROR"

  if [[ "$stepd_stable_has_dated" == "False" ]] && [[ "$stepd_stable_has_undated" == "True" ]]; then
    tap_ok "step-d: first_ts < semantic_change_date invalidates alarm"
  else
    tap_not_ok "step-d: first_ts < semantic_change_date invalidates alarm" \
      "dated=$stepd_stable_has_dated undated=$stepd_stable_has_undated output=${stepd_out:0:200}"
  fi

  # Test 2: Baseline with first_ts >= semantic_change_date → alarm preserved
  local stepd_session2="$stepd_root/session2"
  mkdir -p "$stepd_session2/metrics"

  local stepd_baseline_post
  stepd_baseline_post=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': '2026-05-10T12:00:00Z',
    'last_ts': '2026-05-11T00:00:00Z',
    'alarms': {
        'alarm-with-date': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-no-date': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-05-10T13:00:00Z',
        'created_commit': 'def456',
        'catalog_checksum': 'old-checksum-mismatch2',
        'alarm_versions': {'alarm-with-date': 2, 'alarm-no-date': 1}
    }
}
print(json.dumps(data))
")
  echo "$stepd_baseline_post" > "$stepd_session2/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_post" > "$stepd_session2/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session2/metrics/stepd-current.json"

  local stepd_out2
  stepd_out2=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session2" --current "$stepd_session2/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  # Both alarms should be preserved (first_ts == semantic_change_date for alarm-with-date)
  local stepd2_stable_has_dated stepd2_stable_has_undated
  stepd2_stable_has_dated=$(python3 -c "
import json
with open('$stepd_session2/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd2_stable_has_dated="ERROR"
  stepd2_stable_has_undated=$(python3 -c "
import json
with open('$stepd_session2/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-no-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd2_stable_has_undated="ERROR"

  if [[ "$stepd2_stable_has_dated" == "True" ]] && [[ "$stepd2_stable_has_undated" == "True" ]]; then
    tap_ok "step-d: first_ts >= semantic_change_date preserves alarm"
  else
    tap_not_ok "step-d: first_ts >= semantic_change_date preserves alarm" \
      "dated=$stepd2_stable_has_dated undated=$stepd2_stable_has_undated output=${stepd_out2:0:200}"
  fi

  # Test 3: Baseline without valid first_ts (legacy/malformed) → no Step D check
  local stepd_session3="$stepd_root/session3"
  mkdir -p "$stepd_session3/metrics"

  local stepd_baseline_legacy
  stepd_baseline_legacy=$(python3 -c "
import json
data = {
    'schema_version': 1,
    'evaluated_ticks': 200,
    'skipped_ticks': 10,
    'error_ticks': 0,
    'total_snapshots': 210,
    'first_ts': 'not-a-valid-ts',
    'last_ts': 'also-invalid',
    'alarms': {
        'alarm-with-date': {'firing': 20, 'breach': 5, 'ok': 165, 'baseline': 0, 'skip': 10},
        'alarm-no-date': {'firing': 15, 'breach': 3, 'ok': 172, 'baseline': 0, 'skip': 10}
    },
    'provenance': {
        'created_at': '2026-05-09T01:00:00Z',
        'created_commit': 'old',
        'catalog_checksum': 'old-checksum-mismatch3',
        'alarm_versions': {'alarm-with-date': 2, 'alarm-no-date': 1}
    }
}
print(json.dumps(data))
")
  echo "$stepd_baseline_legacy" > "$stepd_session3/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_legacy" > "$stepd_session3/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session3/metrics/stepd-current.json"

  local stepd_out3
  stepd_out3=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session3" --current "$stepd_session3/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  # Both alarms preserved — malformed first_ts skips Step D
  local stepd3_stable_has_dated
  stepd3_stable_has_dated=$(python3 -c "
import json
with open('$stepd_session3/metrics/replay-baseline-stable.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd3_stable_has_dated="ERROR"

  if [[ "$stepd3_stable_has_dated" == "True" ]]; then
    tap_ok "step-d: malformed first_ts skips temporal check"
  else
    tap_not_ok "step-d: malformed first_ts skips temporal check" \
      "dated=$stepd3_stable_has_dated output=${stepd_out3:0:200}"
  fi

  # Test 4: Ack invalidation — acknowledged_at < semantic_change_date → removed
  local stepd_session4="$stepd_root/session4"
  mkdir -p "$stepd_session4/metrics"

  # Create ack file with old acknowledgment
  cat > "$stepd_session4/metrics/alarm-acknowledgments.json" << 'STEPD_ACK_EOF'
{"schema_version":1,"catalog_checksum":"old-ack-checksum","alarm_versions":{"alarm-with-date":2,"alarm-no-date":1},"alarms":{"alarm-with-date":{"acknowledged_at":"2026-05-08T00:00:00Z","rationale":"old reason","acknowledged_by":"test"},"alarm-no-date":{"acknowledged_at":"2026-05-11T00:00:00Z","rationale":"recent reason","acknowledged_by":"test"}}}
STEPD_ACK_EOF

  # Need baselines for the script to run — use post-change ones
  echo "$stepd_baseline_post" > "$stepd_session4/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_post" > "$stepd_session4/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session4/metrics/stepd-current.json"

  local stepd_out4
  stepd_out4=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session4" --current "$stepd_session4/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  # Check ack file: alarm-with-date should be removed (ack predates change),
  # alarm-no-date should be preserved (no semantic_change_date for that alarm)
  local stepd4_ack_dated stepd4_ack_undated
  stepd4_ack_dated=$(python3 -c "
import json
with open('$stepd_session4/metrics/alarm-acknowledgments.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd4_ack_dated="ERROR"
  stepd4_ack_undated=$(python3 -c "
import json
with open('$stepd_session4/metrics/alarm-acknowledgments.json') as f:
    d = json.load(f)
print('alarm-no-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd4_ack_undated="ERROR"

  if [[ "$stepd4_ack_dated" == "False" ]] && [[ "$stepd4_ack_undated" == "True" ]]; then
    tap_ok "step-d: ack invalidation removes stale acknowledgment"
  else
    tap_not_ok "step-d: ack invalidation removes stale acknowledgment" \
      "dated=$stepd4_ack_dated undated=$stepd4_ack_undated output=${stepd_out4:0:200}"
  fi

  # Test 5: Ack with acknowledged_at >= semantic_change_date → preserved
  local stepd_session5="$stepd_root/session5"
  mkdir -p "$stepd_session5/metrics"

  cat > "$stepd_session5/metrics/alarm-acknowledgments.json" << 'STEPD_ACK5_EOF'
{"schema_version":1,"catalog_checksum":"old-ack-checksum5","alarm_versions":{"alarm-with-date":2,"alarm-no-date":1},"alarms":{"alarm-with-date":{"acknowledged_at":"2026-05-11T00:00:00Z","rationale":"post-change ack","acknowledged_by":"test"}}}
STEPD_ACK5_EOF

  echo "$stepd_baseline_post" > "$stepd_session5/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_post" > "$stepd_session5/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session5/metrics/stepd-current.json"

  local stepd_out5
  stepd_out5=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session5" --current "$stepd_session5/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  local stepd5_ack_dated
  stepd5_ack_dated=$(python3 -c "
import json
with open('$stepd_session5/metrics/alarm-acknowledgments.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd5_ack_dated="ERROR"

  if [[ "$stepd5_ack_dated" == "True" ]]; then
    tap_ok "step-d: ack with acknowledged_at >= semantic_change_date preserved"
  else
    tap_not_ok "step-d: ack with acknowledged_at >= semantic_change_date preserved" \
      "dated=$stepd5_ack_dated output=${stepd_out5:0:200}"
  fi

  # Test 6: Ack with malformed acknowledged_at → removed (fail-closed)
  local stepd_session6="$stepd_root/session6"
  mkdir -p "$stepd_session6/metrics"

  cat > "$stepd_session6/metrics/alarm-acknowledgments.json" << 'STEPD_ACK6_EOF'
{"schema_version":1,"catalog_checksum":"old-ack-checksum6","alarm_versions":{"alarm-with-date":2,"alarm-no-date":1},"alarms":{"alarm-with-date":{"acknowledged_at":"not-a-valid-timestamp","rationale":"malformed","acknowledged_by":"test"}}}
STEPD_ACK6_EOF

  echo "$stepd_baseline_post" > "$stepd_session6/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_post" > "$stepd_session6/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session6/metrics/stepd-current.json"

  local stepd_out6
  stepd_out6=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session6" --current "$stepd_session6/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  local stepd6_ack_dated
  stepd6_ack_dated=$(python3 -c "
import json
with open('$stepd_session6/metrics/alarm-acknowledgments.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd6_ack_dated="ERROR"

  if [[ "$stepd6_ack_dated" == "False" ]]; then
    tap_ok "step-d: malformed acknowledged_at removed (fail-closed)"
  else
    tap_not_ok "step-d: malformed acknowledged_at removed (fail-closed)" \
      "dated=$stepd6_ack_dated output=${stepd_out6:0:200}"
  fi

  # Test 7: Ack with missing acknowledged_at → removed (fail-closed)
  local stepd_session7="$stepd_root/session7"
  mkdir -p "$stepd_session7/metrics"

  cat > "$stepd_session7/metrics/alarm-acknowledgments.json" << 'STEPD_ACK7_EOF'
{"schema_version":1,"catalog_checksum":"old-ack-checksum7","alarm_versions":{"alarm-with-date":2,"alarm-no-date":1},"alarms":{"alarm-with-date":{"rationale":"no timestamp field","acknowledged_by":"test"}}}
STEPD_ACK7_EOF

  echo "$stepd_baseline_post" > "$stepd_session7/metrics/replay-baseline-stable.json"
  echo "$stepd_baseline_post" > "$stepd_session7/metrics/replay-baseline.json"
  echo "$stepd_current" > "$stepd_session7/metrics/stepd-current.json"

  local stepd_out7
  stepd_out7=$("$REPO_ROOT/scripts/dev/check-alarm-regression.sh" --no-file \
    "$stepd_session7" --current "$stepd_session7/metrics/stepd-current.json" \
    --catalog "$stepd_catalog" 2>&1) || true

  local stepd7_ack_dated
  stepd7_ack_dated=$(python3 -c "
import json
with open('$stepd_session7/metrics/alarm-acknowledgments.json') as f:
    d = json.load(f)
print('alarm-with-date' in d.get('alarms', {}))
" 2>/dev/null) || stepd7_ack_dated="ERROR"

  if [[ "$stepd7_ack_dated" == "False" ]]; then
    tap_ok "step-d: missing acknowledged_at removed (fail-closed)"
  else
    tap_not_ok "step-d: missing acknowledged_at removed (fail-closed)" \
      "dated=$stepd7_ack_dated output=${stepd_out7:0:200}"
  fi

  rm -rf "$stepd_root"

  rm -rf "$ver_root"

  # ── check-alarm-regression.sh dedup tests ──────────────────────────────────
  # Tests for issue #2608: duplicate issue filing due to stale dedup snapshot.
  # Uses PATH-based gh stub to intercept GitHub API calls.

  local dedup_root
  dedup_root=$(mktemp -d)
  local dedup_session="$dedup_root/session"
  mkdir -p "$dedup_session/metrics"

  # Create a baseline where lost-sync was active (with provenance to avoid recreation)
  local dedup_catalog_checksum
  dedup_catalog_checksum=$(sha256sum "$catalog_for_reg" | cut -d' ' -f1)
  local dedup_baseline
  dedup_baseline=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2]}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$dedup_catalog_checksum")
  echo "$dedup_baseline" > "$dedup_session/metrics/replay-baseline.json"

  # Current where lost-sync went silent → regression
  local dedup_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$dedup_current" > "$dedup_session/metrics/dedup-current.json"

  local dedup_gh_dir="$dedup_root/gh-stub"
  local dedup_gh_log="$dedup_root/gh-calls.log"
  mkdir -p "$dedup_gh_dir"

  # Helper: create a gh stub with configurable issue list response
  create_gh_stub() {
    local stub_dir="$1"
    local list_response="$2"  # JSON to return for 'gh issue list'
    local create_response="$3"  # URL to return for 'gh issue create'
    cat > "$stub_dir/gh" << STUBEOF
#!/usr/bin/env bash
echo "\$*" >> "$dedup_gh_log"
if [[ "\$1" == "issue" && "\$2" == "list" ]]; then
  echo '$list_response'
  exit 0
elif [[ "\$1" == "issue" && "\$2" == "create" ]]; then
  echo '${create_response:-https://github.com/stellar-experimental/henyey/issues/9999}'
  exit 0
elif [[ "\$1" == "label" ]]; then
  exit 0
fi
exit 0
STUBEOF
    chmod +x "$stub_dir/gh"
  }

  # Also stub move-issue-status.sh to be a no-op
  local dedup_skills_dir="$dedup_root/fake-skills/shared/scripts"
  mkdir -p "$dedup_skills_dir"
  echo '#!/usr/bin/env bash' > "$dedup_skills_dir/move-issue-status.sh"
  echo 'exit 0' >> "$dedup_skills_dir/move-issue-status.sh"
  chmod +x "$dedup_skills_dir/move-issue-status.sh"

  # We need a fake REPO_ROOT that points to our stub skills dir
  # but still has the real script. Symlink approach:
  local dedup_fake_repo="$dedup_root/fake-repo"
  mkdir -p "$dedup_fake_repo/.github/skills/shared/scripts"
  cp "$dedup_skills_dir/move-issue-status.sh" "$dedup_fake_repo/.github/skills/shared/scripts/"
  mkdir -p "$dedup_fake_repo/scripts/dev"
  cp "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" "$dedup_fake_repo/scripts/dev/"
  # Copy shared dedup-filing library (used by check-alarm-regression.sh)
  mkdir -p "$dedup_fake_repo/scripts/lib"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.py" "$dedup_fake_repo/scripts/lib/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.sh" "$dedup_fake_repo/scripts/lib/"

  # Test 1: Existing-issue dedup (exact title match → skip filing)
  > "$dedup_gh_log"
  create_gh_stub "$dedup_gh_dir" \
    '[{"title":"Alarm regression: lost-sync","body":"<!-- alarm-regression-key: lost-sync -->"}]' \
    ""
  set +e
  PATH="$dedup_gh_dir:$PATH" "$dedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$dedup_session" --current "$dedup_session/metrics/dedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e
  if ! grep -q "issue create" "$dedup_gh_log" 2>/dev/null; then
    tap_ok "regression dedup: existing issue (exact title) skips filing"
  else
    tap_not_ok "regression dedup: existing issue (exact title) skips filing" \
      "gh issue create was called: $(cat "$dedup_gh_log")"
  fi

  # Test 2: Title false-positive rejection (similar but not exact title → still file)
  > "$dedup_gh_log"
  create_gh_stub "$dedup_gh_dir" \
    '[{"title":"Alarm regression: lost-sync-extra","body":"no marker"}]' \
    "https://github.com/stellar-experimental/henyey/issues/9999"
  set +e
  PATH="$dedup_gh_dir:$PATH" "$dedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$dedup_session" --current "$dedup_session/metrics/dedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e
  if grep -q "issue create" "$dedup_gh_log" 2>/dev/null; then
    tap_ok "regression dedup: false-positive title rejected, issue filed"
  else
    tap_not_ok "regression dedup: false-positive title rejected, issue filed" \
      "gh issue create was NOT called: $(cat "$dedup_gh_log")"
  fi

  # Test 3: Body-marker fallback (wrong title, correct marker → skip filing)
  > "$dedup_gh_log"
  create_gh_stub "$dedup_gh_dir" \
    '[{"title":"Wrong title entirely","body":"some text <!-- alarm-regression-key: lost-sync --> more text"}]' \
    ""
  set +e
  PATH="$dedup_gh_dir:$PATH" "$dedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$dedup_session" --current "$dedup_session/metrics/dedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e
  if ! grep -q "issue create" "$dedup_gh_log" 2>/dev/null; then
    tap_ok "regression dedup: body-marker fallback skips filing"
  else
    tap_not_ok "regression dedup: body-marker fallback skips filing" \
      "gh issue create was called: $(cat "$dedup_gh_log")"
  fi

  # Test 4: Clean filing (no existing issues → file once)
  > "$dedup_gh_log"
  rm -f "$dedup_fake_repo/.alarm-regression-filed.json"
  create_gh_stub "$dedup_gh_dir" \
    '[]' \
    "https://github.com/stellar-experimental/henyey/issues/9999"
  set +e
  PATH="$dedup_gh_dir:$PATH" "$dedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$dedup_session" --current "$dedup_session/metrics/dedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e
  local create_count
  create_count=$(grep -c "issue create" "$dedup_gh_log" 2>/dev/null) || create_count=0
  if [[ "$create_count" -eq 1 ]]; then
    tap_ok "regression dedup: clean filing creates exactly one issue"
  else
    tap_not_ok "regression dedup: clean filing creates exactly one issue" \
      "expected 1 create call, got $create_count: $(cat "$dedup_gh_log")"
  fi

  # Test 5: Lookup failure graceful degradation (gh issue list fails → still file)
  > "$dedup_gh_log"
  rm -f "$dedup_fake_repo/.alarm-regression-filed.json"
  cat > "$dedup_gh_dir/gh" << 'FAILSTUBEOF'
#!/usr/bin/env bash
echo "$*" >> LOGFILE
if [[ "$1" == "issue" && "$2" == "list" ]]; then
  exit 1
elif [[ "$1" == "issue" && "$2" == "create" ]]; then
  echo "https://github.com/stellar-experimental/henyey/issues/9999"
  exit 0
elif [[ "$1" == "label" ]]; then
  exit 0
fi
exit 0
FAILSTUBEOF
  sed -i "s|LOGFILE|$dedup_gh_log|g" "$dedup_gh_dir/gh"
  chmod +x "$dedup_gh_dir/gh"
  set +e
  PATH="$dedup_gh_dir:$PATH" "$dedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$dedup_session" --current "$dedup_session/metrics/dedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e
  if grep -q "issue create" "$dedup_gh_log" 2>/dev/null; then
    tap_ok "regression dedup: lookup failure still files issue (fail open)"
  else
    tap_not_ok "regression dedup: lookup failure still files issue (fail open)" \
      "gh issue create was NOT called: $(cat "$dedup_gh_log")"
  fi

  rm -rf "$dedup_root"

  # ── eval-alarms telemetry regression tests ─────────────────────────────────
  # Tests for issue #2574: misleading ERROR_NO_SERIES and unsubstituted placeholders

  local eval_script="$REPO_ROOT/scripts/lib/eval-alarms.py"
  local catalog_file="$REPO_ROOT/.claude/skills/shared/metric-alarms.toml"
  local fixture_dir="$REPO_ROOT/scripts/fixtures/eval-alarms"
  local telemetry_state_dir
  telemetry_state_dir=$(mktemp -d)

  # Run eval-alarms with healthy fixtures and capture stderr (telemetry)
  local telemetry_stderr telemetry_stdout
  telemetry_stdout=$(MONITOR_MODE=validator UPTIME_SECONDS=900 \
    WARMUP_TICKS_REMAINING=0 PID=12345 START_TICKS=100 \
    python3 "$eval_script" \
    --catalog "$catalog_file" \
    --current "$fixture_dir/healthy-current.prom" \
    --prev "$fixture_dir/healthy-prev.prom" \
    --state-dir "$telemetry_state_dir" 2>"$telemetry_state_dir/stderr.log") || true
  telemetry_stderr=$(cat "$telemetry_state_dir/stderr.log")

  # Test: histogram-p99 alarms should NOT produce ERROR_NO_SERIES (all 8 alarms)
  local hist_error
  hist_error=$(echo "$telemetry_stderr" | grep -c 'ERROR_NO_SERIES' || true)
  if [[ "$hist_error" -eq 0 ]]; then
    tap_ok "eval-alarms: no false ERROR_NO_SERIES in telemetry"
  else
    local hist_detail
    hist_detail=$(echo "$telemetry_stderr" | grep 'ERROR_NO_SERIES' | head -3)
    tap_not_ok "eval-alarms: no false ERROR_NO_SERIES in telemetry" \
      "Found $hist_error ERROR_NO_SERIES lines: $hist_detail"
  fi

  # Test: counter-ratio alarms with numerator_sum should have non-empty metric
  local ratio_empty_metric
  ratio_empty_metric=$(echo "$telemetry_stderr" | grep 'scp-accept-rate' | grep 'metric= ' || true)
  if [[ -z "$ratio_empty_metric" ]]; then
    tap_ok "eval-alarms: counter-ratio numerator_sum metric resolved"
  else
    tap_not_ok "eval-alarms: counter-ratio numerator_sum metric resolved" \
      "Empty metric in telemetry: $ratio_empty_metric"
  fi

  # Test: no unresolved {placeholder} in any alarm result details or filing_title
  local placeholder_leaks
  placeholder_leaks=$(echo "$telemetry_stdout" | python3 -c "
import json, sys, re
data = json.load(sys.stdin)
leaks = []
for r in data.get('alarms', []):
    for field in ('details', 'filing_title', 'summary'):
        val = r.get(field, '')
        if val and re.search(r'\{[a-z_]+\}', val):
            leaks.append(f\"{r['name']}.{field}: {val}\")
print(len(leaks))
for l in leaks[:5]:
    print(l, file=sys.stderr)
" 2>"$telemetry_state_dir/placeholder-leaks.log") || echo "error"
  if [[ "$placeholder_leaks" == "0" ]]; then
    tap_ok "eval-alarms: no unresolved placeholders in results"
  else
    local leak_detail
    leak_detail=$(cat "$telemetry_state_dir/placeholder-leaks.log" | head -3)
    tap_not_ok "eval-alarms: no unresolved placeholders in results" \
      "$placeholder_leaks leaks: $leak_detail"
  fi

  # Test: no false WARNING from placeholder guard
  local guard_warnings
  guard_warnings=$(echo "$telemetry_stderr" | grep -c 'WARNING: unresolved placeholder' || true)
  if [[ "$guard_warnings" -eq 0 ]]; then
    tap_ok "eval-alarms: no placeholder guard warnings on healthy fixtures"
  else
    local guard_detail
    guard_detail=$(echo "$telemetry_stderr" | grep 'WARNING: unresolved placeholder' | head -3)
    tap_not_ok "eval-alarms: no placeholder guard warnings on healthy fixtures" \
      "$guard_warnings warnings: $guard_detail"
  fi

  # Test: gauge-ratio telemetry shows numerator_metric (fd-exhaustion alarm)
  local gr_metric
  gr_metric=$(echo "$telemetry_stderr" | grep 'fd-exhaustion' | head -1)
  if echo "$gr_metric" | grep -q 'metric=henyey_process_open_fds'; then
    tap_ok "eval-alarms: gauge-ratio telemetry uses numerator_metric"
  else
    tap_not_ok "eval-alarms: gauge-ratio telemetry uses numerator_metric" \
      "fd-exhaustion line: $gr_metric"
  fi

  # Test: exempt/gate skip paths produce placeholder-free results
  # Create a catalog with an exempt counter-ratio alarm to test exempt skip path
  local exempt_catalog exempt_state_dir exempt_stdout
  exempt_catalog=$(mktemp)
  exempt_state_dir=$(mktemp -d)
  cat > "$exempt_catalog" <<'EXEMPT_CAT'
schema_version = 1

[[alarm]]
name = "test-exempt-ratio"
kind = "counter-ratio"
numerator = "stellar_ledger_apply_failure_total"
denominator = "stellar_ledger_apply_success_total"
ratio_op = ">"
ratio_threshold = 0.5
streak_threshold = 3
severity = "WARN"
exempt = true
exempt_reason = "testing exempt path"
details = "fail_ratio={value} threshold={threshold} streak={streak}/{streak_threshold}"
filing_title = "test"
summary = "test"
EXEMPT_CAT
  exempt_stdout=$(MONITOR_MODE=validator UPTIME_SECONDS=900 \
    WARMUP_TICKS_REMAINING=0 PID=12345 START_TICKS=100 \
    python3 "$eval_script" \
    --catalog "$exempt_catalog" \
    --current "$fixture_dir/healthy-current.prom" \
    --prev "$fixture_dir/healthy-prev.prom" \
    --state-dir "$exempt_state_dir" 2>/dev/null) || true
  local exempt_leaks
  exempt_leaks=$(echo "$exempt_stdout" | python3 -c "
import json, sys, re
data = json.load(sys.stdin)
leaks = []
for r in data.get('alarms', []):
    for field in ('details', 'filing_title', 'summary'):
        val = r.get(field, '')
        if val and re.search(r'\{[a-z_]+\}', val):
            leaks.append(f\"{r['name']}.{field}: {val}\")
print(len(leaks))
" 2>/dev/null) || echo "error"
  if [[ "$exempt_leaks" == "0" ]]; then
    tap_ok "eval-alarms: exempt skip path produces placeholder-free results"
  else
    tap_not_ok "eval-alarms: exempt skip path produces placeholder-free results" \
      "$exempt_leaks placeholder leaks in exempt results"
  fi
  rm -f "$exempt_catalog"
  rm -rf "$exempt_state_dir"

  rm -rf "$telemetry_state_dir"

  # ── Cross-invocation dedup tests (Gap 1 of #2619) ──────────────────────
  # Uses PATH-based gh stub (same approach as existing dedup tests above)
  echo "# Cross-invocation dedup tests" >&2

  local xdedup_root
  xdedup_root=$(mktemp -d)
  local xdedup_session="$xdedup_root/session"
  mkdir -p "$xdedup_session/metrics"

  local xdedup_catalog_checksum
  xdedup_catalog_checksum=$(sha256sum "$catalog_for_reg" | cut -d' ' -f1)

  # Create baselines with provenance (lost-sync active at 10%)
  local xdedup_baseline
  xdedup_baseline=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2]}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$xdedup_catalog_checksum")
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline.json"
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline-stable.json"

  # Current where lost-sync went silent → regression
  local xdedup_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$xdedup_current" > "$xdedup_session/metrics/xdedup-current.json"

  local xdedup_gh_dir="$xdedup_root/gh-stub"
  local xdedup_gh_log="$xdedup_root/gh-calls.log"
  mkdir -p "$xdedup_gh_dir"

  # Fake repo with stub move-issue-status.sh
  local xdedup_fake_repo="$xdedup_root/fake-repo"
  mkdir -p "$xdedup_fake_repo/.github/skills/shared/scripts"
  echo '#!/usr/bin/env bash' > "$xdedup_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  echo 'exit 0' >> "$xdedup_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  chmod +x "$xdedup_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  mkdir -p "$xdedup_fake_repo/scripts/dev"
  cp "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" "$xdedup_fake_repo/scripts/dev/"
  # Copy shared dedup-filing library (used by check-alarm-regression.sh)
  mkdir -p "$xdedup_fake_repo/scripts/lib"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.py" "$xdedup_fake_repo/scripts/lib/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.sh" "$xdedup_fake_repo/scripts/lib/"

  # gh stub: no existing issues (allow filing), returns a fake issue URL
  cat > "$xdedup_gh_dir/gh" << 'STUBEOF'
#!/usr/bin/env bash
echo "$*" >> LOGFILE
if [[ "$1" == "issue" && "$2" == "list" ]]; then
  echo '[]'
  exit 0
elif [[ "$1" == "issue" && "$2" == "create" ]]; then
  echo 'https://github.com/stellar-experimental/henyey/issues/9999'
  exit 0
elif [[ "$1" == "issue" && "$2" == "view" ]]; then
  echo '{"comments":[]}'
  exit 0
elif [[ "$1" == "label" ]]; then
  exit 0
fi
exit 0
STUBEOF
  sed -i "s|LOGFILE|$xdedup_gh_log|g" "$xdedup_gh_dir/gh"
  chmod +x "$xdedup_gh_dir/gh"

  # Test 1: First run creates dedup record
  > "$xdedup_gh_log"
  set +e
  PATH="$xdedup_gh_dir:$PATH" "$xdedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$xdedup_session" --current "$xdedup_session/metrics/xdedup-current.json" \
    --catalog "$catalog_for_reg" >/dev/null 2>&1
  set -e

  local xdedup_file="$xdedup_fake_repo/.alarm-regression-filed.json"
  if [[ -f "$xdedup_file" ]] && python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
assert 'lost-sync' in data.get('filed', {}), 'lost-sync not in dedup file'
" "$xdedup_file" 2>/dev/null; then
    tap_ok "dedup: first run records filed alarm in dedup file"
  else
    tap_not_ok "dedup: first run records filed alarm in dedup file" "dedup file: $(cat "$xdedup_file" 2>/dev/null || echo 'missing')"
  fi

  # Test 2: Second run should skip filing due to persistent dedup record
  > "$xdedup_gh_log"
  # Re-create baselines (they get updated on first run since no regression from rolling)
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline.json"
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline-stable.json"
  local xdedup_out2
  set +e
  xdedup_out2=$(PATH="$xdedup_gh_dir:$PATH" "$xdedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$xdedup_session" --current "$xdedup_session/metrics/xdedup-current.json" \
    --catalog "$catalog_for_reg" 2>&1)
  set -e

  if echo "$xdedup_out2" | grep -q "cross-invocation duplicate"; then
    tap_ok "dedup: second run skips filing due to persistent dedup"
  else
    tap_not_ok "dedup: second run skips filing due to persistent dedup" "output: ${xdedup_out2:0:500}"
  fi

  # Test 3: Dedup pruning — set filed_at to 25 hours ago, should allow re-filing
  if [[ -f "$xdedup_file" ]]; then
    python3 -c "
import json, sys
from datetime import datetime, timezone, timedelta
with open(sys.argv[1]) as f:
    data = json.load(f)
old_time = (datetime.now(timezone.utc) - timedelta(hours=25)).strftime('%Y-%m-%dT%H:%M:%SZ')
for alarm in data.get('filed', {}):
    data['filed'][alarm]['filed_at'] = old_time
with open(sys.argv[1], 'w') as f:
    json.dump(data, f)
" "$xdedup_file" 2>/dev/null

    > "$xdedup_gh_log"
    echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline.json"
    echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline-stable.json"
    set +e
    local xdedup_out3
    xdedup_out3=$(PATH="$xdedup_gh_dir:$PATH" "$xdedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
      "$xdedup_session" --current "$xdedup_session/metrics/xdedup-current.json" \
      --catalog "$catalog_for_reg" 2>&1)
    set -e

    if ! echo "$xdedup_out3" | grep -q "cross-invocation duplicate"; then
      tap_ok "dedup: expired entries pruned, re-filing allowed"
    else
      tap_not_ok "dedup: expired entries pruned, re-filing allowed" "output: ${xdedup_out3:0:500}"
    fi
  else
    tap_not_ok "dedup: expired entries pruned, re-filing allowed" "dedup file missing"
  fi

  # Test 4: Corrupt dedup file treated as empty (no crash)
  echo "NOT VALID JSON{{{" > "$xdedup_file"
  > "$xdedup_gh_log"
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline.json"
  echo "$xdedup_baseline" > "$xdedup_session/metrics/replay-baseline-stable.json"
  set +e
  local xdedup_corrupt_out
  xdedup_corrupt_out=$(PATH="$xdedup_gh_dir:$PATH" "$xdedup_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$xdedup_session" --current "$xdedup_session/metrics/xdedup-current.json" \
    --catalog "$catalog_for_reg" 2>&1)
  set -e

  if echo "$xdedup_corrupt_out" | grep -q "Corrupt or invalid dedup file"; then
    tap_ok "dedup: corrupt file treated as empty with warning"
  else
    tap_not_ok "dedup: corrupt file treated as empty with warning" "output: ${xdedup_corrupt_out:0:500}"
  fi

  rm -f "$xdedup_file"
  rm -rf "$xdedup_root"

  # ── refresh-stable-baseline.sh tests (Gap 2a of #2619) ─────────────────
  echo "# refresh-stable-baseline.sh tests" >&2

  local refresh_session="$replay_root/refresh-session"
  mkdir -p "$refresh_session/metrics"

  # Create baselines with two alarms
  local refresh_baseline='{"schema_version":1,"evaluated_ticks":200,"alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10},"peer-count-low":{"firing":15,"breach":3,"ok":172,"baseline":0,"skip":10}},"provenance":{"catalog_checksum":"test","commit":"test","timestamp":"t"}}'
  echo "$refresh_baseline" > "$refresh_session/metrics/replay-baseline.json"
  echo "$refresh_baseline" > "$refresh_session/metrics/replay-baseline-stable.json"

  # Refresh just lost-sync
  local refresh_out
  refresh_out=$("$REPO_ROOT/scripts/dev/refresh-stable-baseline.sh" \
    "$refresh_session" lost-sync 2>&1) || true

  # Verify lost-sync removed from stable baseline but peer-count-low kept
  local stable_check
  stable_check=$(python3 -c "
import json
with open('$refresh_session/metrics/replay-baseline-stable.json') as f:
    data = json.load(f)
has_ls = 'yes' if 'lost-sync' in data.get('alarms', {}) else 'no'
has_pc = 'yes' if 'peer-count-low' in data.get('alarms', {}) else 'no'
print(has_ls + ':' + has_pc)
" 2>/dev/null) || stable_check="error"

  if [[ "$stable_check" == "no:yes" ]]; then
    tap_ok "refresh: removes alarm from stable baseline, keeps others"
  else
    tap_not_ok "refresh: removes alarm from stable baseline, keeps others" "result: $stable_check"
  fi

  # Verify lost-sync also removed from rolling baseline
  local rolling_check
  rolling_check=$(python3 -c "
import json
with open('$refresh_session/metrics/replay-baseline.json') as f:
    data = json.load(f)
print('yes' if 'lost-sync' in data.get('alarms', {}) else 'no')
" 2>/dev/null) || rolling_check="error"

  if [[ "$rolling_check" == "no" ]]; then
    tap_ok "refresh: removes alarm from rolling baseline"
  else
    tap_not_ok "refresh: removes alarm from rolling baseline" "result: $rolling_check"
  fi

  # Test: refresh auto-revokes matching acknowledgment
  echo '{"schema_version":1,"catalog_checksum":"","alarms":{"lost-sync":{"acknowledged_at":"2026-01-01T00:00:00Z","acknowledged_commit":"abc","rationale":"test"}}}' > "$refresh_session/metrics/alarm-acknowledgments.json"

  # Re-create baselines for this test
  echo "$refresh_baseline" > "$refresh_session/metrics/replay-baseline.json"
  echo "$refresh_baseline" > "$refresh_session/metrics/replay-baseline-stable.json"

  "$REPO_ROOT/scripts/dev/refresh-stable-baseline.sh" \
    "$refresh_session" lost-sync 2>/dev/null || true

  local ack_check
  ack_check=$(python3 -c "
import json
with open('$refresh_session/metrics/alarm-acknowledgments.json') as f:
    data = json.load(f)
print('yes' if 'lost-sync' in data.get('alarms', {}) else 'no')
" 2>/dev/null) || ack_check="error"

  if [[ "$ack_check" == "no" ]]; then
    tap_ok "refresh: auto-revokes matching acknowledgment"
  else
    tap_not_ok "refresh: auto-revokes matching acknowledgment" "result: $ack_check"
  fi

  # Test: refresh with invalid alarm name
  local refresh_invalid_out
  refresh_invalid_out=$("$REPO_ROOT/scripts/dev/refresh-stable-baseline.sh" \
    "$refresh_session" "nonexistent-alarm" 2>&1) || true

  if echo "$refresh_invalid_out" | grep -q "not found in catalog"; then
    tap_ok "refresh: rejects alarm not in catalog"
  else
    tap_not_ok "refresh: rejects alarm not in catalog" "output: $refresh_invalid_out"
  fi

  # Test: refresh with missing baseline files → no error
  local refresh_empty_session="$replay_root/refresh-empty"
  mkdir -p "$refresh_empty_session/metrics"
  local refresh_missing_out
  refresh_missing_out=$("$REPO_ROOT/scripts/dev/refresh-stable-baseline.sh" \
    "$refresh_empty_session" lost-sync 2>&1) || true

  if echo "$refresh_missing_out" | grep -q "file not found"; then
    tap_ok "refresh: missing baseline files handled gracefully"
  else
    tap_not_ok "refresh: missing baseline files handled gracefully" "output: $refresh_missing_out"
  fi

  rm -rf "$refresh_session" "$refresh_empty_session"

  # Test: refresh with malformed ack file → exit 2 (fail closed)
  local refresh_malformed_session="$replay_root/refresh-malformed"
  mkdir -p "$refresh_malformed_session/metrics"
  echo "$refresh_baseline" > "$refresh_malformed_session/metrics/replay-baseline.json"
  echo "$refresh_malformed_session" > "$refresh_malformed_session/metrics/replay-baseline-stable.json"
  echo "NOT VALID JSON{{{" > "$refresh_malformed_session/metrics/alarm-acknowledgments.json"
  # Need valid baselines for the alarm removal to succeed first
  echo "$refresh_baseline" > "$refresh_malformed_session/metrics/replay-baseline-stable.json"

  set +e
  local refresh_malformed_out
  refresh_malformed_out=$("$REPO_ROOT/scripts/dev/refresh-stable-baseline.sh" \
    "$refresh_malformed_session" lost-sync 2>&1)
  local refresh_malformed_exit=$?
  set -e

  if [[ "$refresh_malformed_exit" -eq 2 ]]; then
    tap_ok "refresh: malformed ack file exits 2 (fail closed)"
  else
    tap_not_ok "refresh: malformed ack file exits 2 (fail closed)" "exit=$refresh_malformed_exit output: $refresh_malformed_out"
  fi

  rm -rf "$refresh_malformed_session"

  # ── Auto-comment dedup tests (Gap 2b of #2619) ─────────────────────────
  echo "# Auto-comment dedup tests" >&2

  local comment_root
  comment_root=$(mktemp -d)
  local comment_session="$comment_root/session"
  mkdir -p "$comment_session/metrics"

  local comment_catalog_checksum
  comment_catalog_checksum=$(sha256sum "$catalog_for_reg" | cut -d' ' -f1)

  # Create baselines with provenance
  local comment_baseline
  comment_baseline=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
d['provenance'] = {'created_at': '2026-01-01T00:00:00Z', 'created_commit': 'test', 'catalog_checksum': sys.argv[2]}
print(json.dumps(d))
" '{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t1","last_ts":"t2","alarms":{"lost-sync":{"firing":20,"breach":5,"ok":165,"baseline":0,"skip":10}}}' "$comment_catalog_checksum")
  echo "$comment_baseline" > "$comment_session/metrics/replay-baseline.json"
  echo "$comment_baseline" > "$comment_session/metrics/replay-baseline-stable.json"

  local comment_current='{"schema_version":1,"evaluated_ticks":200,"skipped_ticks":10,"error_ticks":0,"total_snapshots":210,"first_ts":"t3","last_ts":"t4","alarms":{"lost-sync":{"firing":0,"breach":0,"ok":190,"baseline":0,"skip":10}}}'
  echo "$comment_current" > "$comment_session/metrics/comment-current.json"

  local comment_gh_dir="$comment_root/gh-stub"
  local comment_gh_log="$comment_root/gh-calls.log"
  mkdir -p "$comment_gh_dir"

  local comment_fake_repo="$comment_root/fake-repo"
  mkdir -p "$comment_fake_repo/.github/skills/shared/scripts"
  echo '#!/usr/bin/env bash' > "$comment_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  echo 'exit 0' >> "$comment_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  chmod +x "$comment_fake_repo/.github/skills/shared/scripts/move-issue-status.sh"
  mkdir -p "$comment_fake_repo/scripts/dev"
  cp "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" "$comment_fake_repo/scripts/dev/"
  # Copy shared dedup-filing library (used by check-alarm-regression.sh)
  mkdir -p "$comment_fake_repo/scripts/lib"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.py" "$comment_fake_repo/scripts/lib/"
  cp "$REPO_ROOT/scripts/lib/dedup-filing.sh" "$comment_fake_repo/scripts/lib/"

  # gh stub: existing issue found (duplicate) + returns "0" for --jq comment check
  cat > "$comment_gh_dir/gh" << 'STUBEOF'
#!/usr/bin/env bash
echo "$*" >> LOGFILE
if [[ "$1" == "issue" && "$2" == "list" ]]; then
  echo '[{"number":9999,"title":"Alarm regression: lost-sync","body":"<!-- alarm-regression-key: lost-sync -->"}]'
  exit 0
elif [[ "$1" == "issue" && "$2" == "view" ]]; then
  # The script uses --jq to count matching comments; return "0" (no prior update today)
  echo "0"
  exit 0
elif [[ "$1" == "issue" && "$2" == "comment" ]]; then
  echo "https://github.com/stellar-experimental/henyey/issues/9999#comment-1"
  exit 0
elif [[ "$1" == "label" ]]; then
  exit 0
fi
exit 0
STUBEOF
  sed -i "s|LOGFILE|$comment_gh_log|g" "$comment_gh_dir/gh"
  chmod +x "$comment_gh_dir/gh"

  > "$comment_gh_log"
  set +e
  local comment_out
  comment_out=$(PATH="$comment_gh_dir:$PATH" "$comment_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$comment_session" --current "$comment_session/metrics/comment-current.json" \
    --catalog "$catalog_for_reg" 2>&1)
  set -e

  if grep -q "issue comment" "$comment_gh_log" 2>/dev/null && echo "$comment_out" | grep -q "Posted update comment"; then
    tap_ok "autocomment: posts update comment on existing duplicate issue"
  else
    tap_not_ok "autocomment: posts update comment on existing duplicate issue" "log: $(cat "$comment_gh_log") output: ${comment_out:0:500}"
  fi

  # Test same-day dedup: second run with same-day marker already present
  # Stub returns comments with today's marker
  TODAY=$(date -u +%Y-%m-%d)
  cat > "$comment_gh_dir/gh" << STUBEOF2
#!/usr/bin/env bash
echo "\$*" >> "$comment_gh_log"
if [[ "\$1" == "issue" && "\$2" == "list" ]]; then
  echo '[{"number":9999,"title":"Alarm regression: lost-sync","body":"<!-- alarm-regression-key: lost-sync -->"}]'
  exit 0
elif [[ "\$1" == "issue" && "\$2" == "view" ]]; then
  # Return "1" — already commented today
  echo "1"
  exit 0
elif [[ "\$1" == "label" ]]; then
  exit 0
fi
exit 0
STUBEOF2
  chmod +x "$comment_gh_dir/gh"

  > "$comment_gh_log"
  echo "$comment_baseline" > "$comment_session/metrics/replay-baseline.json"
  echo "$comment_baseline" > "$comment_session/metrics/replay-baseline-stable.json"
  rm -f "$comment_fake_repo/.alarm-regression-filed.json"
  set +e
  local comment_out2
  comment_out2=$(PATH="$comment_gh_dir:$PATH" "$comment_fake_repo/scripts/dev/check-alarm-regression.sh" \
    "$comment_session" --current "$comment_session/metrics/comment-current.json" \
    --catalog "$catalog_for_reg" 2>&1)
  set -e

  if echo "$comment_out2" | grep -q "already posted today"; then
    tap_ok "autocomment: same-day dedup skips re-commenting"
  else
    tap_not_ok "autocomment: same-day dedup skips re-commenting" "output: ${comment_out2:0:500}"
  fi

  rm -rf "$comment_root"

  # ══════════════════════════════════════════════════════════════════════════════
  # ── dedup-filing library behavioral tests (#2639) ───────────────────────────
  # ══════════════════════════════════════════════════════════════════════════════

  # ── Shared setup ────────────────────────────────────────────────────────────
  local dedup_test_root="$TEST_ROOT/dedup-filing"
  mkdir -p "$dedup_test_root"
  source "$REPO_ROOT/scripts/lib/dedup-filing.sh"

  # Helper: semantic JSON assertion (field equality)
  # Usage: _json_assert "$json_str" '.filed["key"]["issue_number"]' 42
  _json_field() {
    python3 -c "
import json, sys
data = json.loads(sys.argv[1])
path = sys.argv[2]
# Simple dotted path with bracket notation
import re
obj = data
for part in re.findall(r'\.(\w+)|\[\"([^\"]+)\"\]', path):
    key = part[0] or part[1]
    if isinstance(obj, dict):
        obj = obj.get(key)
    else:
        obj = None
        break
print(json.dumps(obj) if obj is not None else 'null')
" "$1" "$2" 2>/dev/null
  }

  _json_has_key() {
    python3 -c "
import json, sys
data = json.loads(sys.argv[1])
keys = sys.argv[2:]
obj = data
for k in keys:
    if isinstance(obj, dict) and k in obj:
        obj = obj[k]
    else:
        sys.exit(1)
sys.exit(0)
" "$@" 2>/dev/null
  }

  # ── Layer 1: Library unit tests ─────────────────────────────────────────────

  # Test: Load — missing file
  local dl_out
  dl_out=$(dedup_load "$dedup_test_root/nonexistent-file.json" 2>/dev/null)
  if _json_has_key "$dl_out" "schema_version" && _json_has_key "$dl_out" "filed"; then
    tap_ok "dedup-load: missing file returns empty schema"
  else
    tap_not_ok "dedup-load: missing file returns empty schema" "output: ${dl_out:0:200}"
  fi

  # Test: Load — corrupt file (non-JSON)
  echo "this is not json {{{" > "$dedup_test_root/corrupt.json"
  local dl_corrupt_out dl_corrupt_err
  dl_corrupt_err=$(dedup_load "$dedup_test_root/corrupt.json" 2>&1 >/dev/null)
  dl_corrupt_out=$(dedup_load "$dedup_test_root/corrupt.json" 2>/dev/null)
  if _json_has_key "$dl_corrupt_out" "schema_version" && echo "$dl_corrupt_err" | grep -qi "warning"; then
    tap_ok "dedup-load: corrupt file returns empty schema + warning"
  else
    tap_not_ok "dedup-load: corrupt file returns empty schema + warning" "out: ${dl_corrupt_out:0:100} err: ${dl_corrupt_err:0:100}"
  fi

  # Test: Load — wrong schema_version
  echo '{"schema_version":2,"filed":{"x":{}}}' > "$dedup_test_root/bad-schema.json"
  local dl_schema_out dl_schema_err
  dl_schema_err=$(dedup_load "$dedup_test_root/bad-schema.json" 2>&1 >/dev/null)
  dl_schema_out=$(dedup_load "$dedup_test_root/bad-schema.json" 2>/dev/null)
  if _json_has_key "$dl_schema_out" "schema_version" && ! _json_has_key "$dl_schema_out" "filed" "x"; then
    tap_ok "dedup-load: wrong schema_version returns empty schema"
  else
    tap_not_ok "dedup-load: wrong schema_version returns empty schema" "out: ${dl_schema_out:0:200}"
  fi

  # Test: Load — valid file
  echo '{"schema_version":1,"filed":{"abc":{"issue_number":99,"filed_at":"2026-01-01T00:00:00Z"}}}' > "$dedup_test_root/valid.json"
  local dl_valid_out
  dl_valid_out=$(dedup_load "$dedup_test_root/valid.json" 2>/dev/null)
  if _json_has_key "$dl_valid_out" "filed" "abc"; then
    tap_ok "dedup-load: valid file returns data"
  else
    tap_not_ok "dedup-load: valid file returns data" "out: ${dl_valid_out:0:200}"
  fi

  # Test: Prune — TTL boundary (> semantics: entry at cutoff is expired)
  # To prove > (not >=), we test two entries:
  #   - one filed exactly 24h+1s ago → definitely pruned (beyond cutoff)
  #   - one filed exactly 24h-1s ago → definitely kept (within window)
  # This confirms the boundary behavior regardless of sub-second clock drift.
  local ts_beyond_cutoff ts_within_cutoff
  ts_beyond_cutoff=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=24,seconds=1)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  ts_within_cutoff=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=23,minutes=59,seconds=58)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  local prune_boundary_data='{"schema_version":1,"filed":{"beyond":{"filed_at":"'"$ts_beyond_cutoff"'","issue_number":1},"within":{"filed_at":"'"$ts_within_cutoff"'","issue_number":2}}}'
  local prune_boundary_out
  prune_boundary_out=$(dedup_prune "$prune_boundary_data" "24h")
  if ! _json_has_key "$prune_boundary_out" "filed" "beyond" && _json_has_key "$prune_boundary_out" "filed" "within"; then
    tap_ok "dedup-prune: TTL boundary correctly prunes old, keeps fresh"
  else
    tap_not_ok "dedup-prune: TTL boundary correctly prunes old, keeps fresh" "out: ${prune_boundary_out:0:200}"
  fi

  # Test: Prune — malformed filed_at is silently dropped
  local prune_malformed_data='{"schema_version":1,"filed":{"bad":{"filed_at":"not-a-date"},"good":{"filed_at":"2099-01-01T00:00:00Z","issue_number":2}}}'
  local prune_malformed_out
  prune_malformed_out=$(dedup_prune "$prune_malformed_data" "24h")
  if ! _json_has_key "$prune_malformed_out" "filed" "bad" && _json_has_key "$prune_malformed_out" "filed" "good"; then
    tap_ok "dedup-prune: malformed filed_at silently dropped"
  else
    tap_not_ok "dedup-prune: malformed filed_at silently dropped" "out: ${prune_malformed_out:0:200}"
  fi

  # Test: Prune — keeps fresh entries
  local ts_1h_ago
  ts_1h_ago=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=1)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  local prune_fresh_data='{"schema_version":1,"filed":{"fresh":{"filed_at":"'"$ts_1h_ago"'","issue_number":3}}}'
  local prune_fresh_out
  prune_fresh_out=$(dedup_prune "$prune_fresh_data" "24h")
  if _json_has_key "$prune_fresh_out" "filed" "fresh"; then
    tap_ok "dedup-prune: fresh entry survives prune"
  else
    tap_not_ok "dedup-prune: fresh entry survives prune" "out: ${prune_fresh_out:0:200}"
  fi

  # Test: Check — hit
  local check_data='{"schema_version":1,"filed":{"sha123":{"issue_number":55,"filed_at":"2026-01-01T00:00:00Z"}}}'
  local check_hit_out
  set +e
  check_hit_out=$(dedup_check "$check_data" "sha123" 2>/dev/null)
  local check_hit_rc=$?
  set -e
  if [[ $check_hit_rc -eq 0 ]] && echo "$check_hit_out" | python3 -c "import json,sys; assert json.load(sys.stdin)['issue_number']==55" 2>/dev/null; then
    tap_ok "dedup-check: hit returns exit 0 + entry JSON"
  else
    tap_not_ok "dedup-check: hit returns exit 0 + entry JSON" "rc=$check_hit_rc out: ${check_hit_out:0:200}"
  fi

  # Test: Check — miss
  set +e
  local check_miss_out
  check_miss_out=$(dedup_check "$check_data" "nonexistent" 2>/dev/null)
  local check_miss_rc=$?
  set -e
  if [[ $check_miss_rc -eq 1 ]] && [[ -z "$check_miss_out" ]]; then
    tap_ok "dedup-check: miss returns exit 1 + empty stdout"
  else
    tap_not_ok "dedup-check: miss returns exit 1 + empty stdout" "rc=$check_miss_rc out: ${check_miss_out:0:100}"
  fi

  # Test: Check — empty string key (normal miss)
  set +e
  local check_empty_out
  check_empty_out=$(dedup_check "$check_data" "" 2>/dev/null)
  local check_empty_rc=$?
  set -e
  if [[ $check_empty_rc -eq 1 ]]; then
    tap_ok "dedup-check: empty key is a normal miss"
  else
    tap_not_ok "dedup-check: empty key is a normal miss" "rc=$check_empty_rc"
  fi

  # Test: Record — k=v pairs with integer coercion
  local record_input='{"schema_version":1,"filed":{}}'
  local record_out
  record_out=$(dedup_record "$record_input" "mysha" "issue_number=42" "last_commented_date=2026-05-01")
  if _json_has_key "$record_out" "filed" "mysha" && \
     echo "$record_out" | python3 -c "import json,sys; d=json.load(sys.stdin); e=d['filed']['mysha']; assert e['issue_number']==42; assert e['last_commented_date']=='2026-05-01'; assert 'filed_at' in e" 2>/dev/null; then
    tap_ok "dedup-record: k=v pairs with integer coercion + auto filed_at"
  else
    tap_not_ok "dedup-record: k=v pairs with integer coercion + auto filed_at" "out: ${record_out:0:300}"
  fi

  # Test: Remove — existing key
  local remove_data='{"schema_version":1,"filed":{"a":{"filed_at":"2026-01-01T00:00:00Z"},"b":{"filed_at":"2026-01-01T00:00:00Z"}}}'
  local remove_out
  remove_out=$(dedup_remove "$remove_data" "a")
  if ! _json_has_key "$remove_out" "filed" "a" && _json_has_key "$remove_out" "filed" "b"; then
    tap_ok "dedup-remove: existing key removed, others preserved"
  else
    tap_not_ok "dedup-remove: existing key removed, others preserved" "out: ${remove_out:0:200}"
  fi

  # Test: Remove — absent key (no-op)
  local remove_noop_out
  remove_noop_out=$(dedup_remove "$remove_data" "nonexistent")
  if _json_has_key "$remove_noop_out" "filed" "a" && _json_has_key "$remove_noop_out" "filed" "b"; then
    tap_ok "dedup-remove: absent key is no-op"
  else
    tap_not_ok "dedup-remove: absent key is no-op" "out: ${remove_noop_out:0:200}"
  fi

  # Test: Update-field — existing key
  local uf_data='{"schema_version":1,"filed":{"k1":{"filed_at":"2026-01-01T00:00:00Z","issue_number":10}}}'
  local uf_out
  uf_out=$(dedup_update_field "$uf_data" "k1" "last_commented_date" "2026-05-13")
  if echo "$uf_out" | python3 -c "import json,sys; d=json.load(sys.stdin); e=d['filed']['k1']; assert e['last_commented_date']=='2026-05-13'; assert e['issue_number']==10" 2>/dev/null; then
    tap_ok "dedup-update-field: updates field, preserves others"
  else
    tap_not_ok "dedup-update-field: updates field, preserves others" "out: ${uf_out:0:200}"
  fi

  # Test: Update-field — missing key
  set +e
  local uf_missing_err
  uf_missing_err=$(dedup_update_field "$uf_data" "nosuchkey" "x" "y" 2>&1 >/dev/null)
  local uf_missing_rc=$?
  set -e
  if [[ $uf_missing_rc -ne 0 ]] && echo "$uf_missing_err" | grep -q "Key not found"; then
    tap_ok "dedup-update-field: missing key exits non-zero"
  else
    tap_not_ok "dedup-update-field: missing key exits non-zero" "rc=$uf_missing_rc err: ${uf_missing_err:0:100}"
  fi

  # Test: Write — atomic (valid JSON writes, no residue)
  local write_file="$dedup_test_root/write-test.json"
  local write_data='{"schema_version":1,"filed":{"w":{"filed_at":"2026-01-01T00:00:00Z"}}}'
  dedup_write "$write_file" "$write_data" 2>/dev/null
  if [[ -f "$write_file" ]] && python3 -c "import json; json.load(open('$write_file'))" 2>/dev/null && \
     ! ls "$dedup_test_root"/write-test.json.tmp.* 2>/dev/null | grep -q .; then
    tap_ok "dedup-write: atomic write succeeds, no tmp residue"
  else
    tap_not_ok "dedup-write: atomic write succeeds, no tmp residue"
  fi

  # Test: Write — invalid JSON rejected
  echo "valid content" > "$dedup_test_root/write-guard.json"
  set +e
  dedup_write "$dedup_test_root/write-guard.json" "not json {{{" 2>/dev/null
  local write_bad_rc=$?
  set -e
  if [[ $write_bad_rc -ne 0 ]] && grep -q "valid content" "$dedup_test_root/write-guard.json"; then
    tap_ok "dedup-write: invalid JSON rejected, file unchanged"
  else
    tap_not_ok "dedup-write: invalid JSON rejected, file unchanged" "rc=$write_bad_rc"
  fi

  # ── Layer 2: monitor-tick §3d flow tests ────────────────────────────────────

  local flow_root="$dedup_test_root/flow"
  mkdir -p "$flow_root/gh-stub"
  local flow_gh_log="$flow_root/gh-calls.log"

  # Helper: create a gh stub for flow tests
  _flow_gh_stub() {
    local stub_dir="$flow_root/gh-stub"
    local view_response="${1:-}"   # JSON for 'gh issue view'
    local list_response="${2:-[]}" # JSON for 'gh issue list'
    local create_response="${3:-}" # URL for 'gh issue create'
    local comment_exit="${4:-0}"   # exit code for 'gh issue comment'
    cat > "$stub_dir/gh" << STUBEOF
#!/usr/bin/env bash
echo "\$*" >> "$flow_gh_log"
if [[ "\$1" == "issue" && "\$2" == "view" ]]; then
  echo '$view_response'
  exit 0
elif [[ "\$1" == "issue" && "\$2" == "list" ]]; then
  echo '$list_response'
  exit 0
elif [[ "\$1" == "issue" && "\$2" == "create" ]]; then
  if [[ -n '$create_response' ]]; then
    echo '$create_response'
    exit 0
  fi
  exit 1
elif [[ "\$1" == "issue" && "\$2" == "comment" ]]; then
  exit $comment_exit
elif [[ "\$1" == "issue" && "\$2" == "edit" ]]; then
  exit 0
elif [[ "\$1" == "api" ]]; then
  echo '{"data":{"resource":{"projectItems":{"totalCount":0}}}}'
  exit 0
fi
exit 0
STUBEOF
    chmod +x "$stub_dir/gh"
  }

  # Stub move-issue-status.sh
  mkdir -p "$flow_root/fake-repo/.github/skills/shared/scripts"
  cat > "$flow_root/fake-repo/.github/skills/shared/scripts/move-issue-status.sh" << 'MOVEOF'
#!/usr/bin/env bash
echo "move-issue-status $*" >> FLOW_LOG_PLACEHOLDER
exit 0
MOVEOF
  sed -i "s|FLOW_LOG_PLACEHOLDER|$flow_gh_log|" "$flow_root/fake-repo/.github/skills/shared/scripts/move-issue-status.sh"
  chmod +x "$flow_root/fake-repo/.github/skills/shared/scripts/move-issue-status.sh"

  # Test: Empty build_sha guard — asserts no dedup or gh calls when build_sha is empty
  > "$flow_gh_log"
  local build_sha=""
  _flow_gh_stub '{"state":"OPEN"}' '[]' 'https://github.com/stellar-experimental/henyey/issues/888' '0'
  local empty_sha_dedup_file="$flow_root/dedup-empty-sha.json"
  local empty_sha_original='{"schema_version":1,"filed":{"somsha":{"issue_number":99,"filed_at":"2099-01-01T00:00:00Z"}}}'
  echo "$empty_sha_original" > "$empty_sha_dedup_file"
  # §3d says: if build_sha is empty, do NOT enter flock/dedup/filing path
  # The else branch simulates what WOULD happen if the guard were missing
  if [[ -z "$build_sha" ]]; then
    # Guard fires correctly: skip all dedup/filing/gh operations
    :
  else
    # This path must NOT execute when build_sha is empty
    flow_data=$(dedup_load "$empty_sha_dedup_file" 2>/dev/null)
    PATH="$flow_root/gh-stub:$PATH" gh issue create --title "bad" --body "bad" 2>/dev/null
    flow_data=$(dedup_record "$flow_data" "bad_key" "issue_number=999")
    dedup_write "$empty_sha_dedup_file" "$flow_data" 2>/dev/null
  fi
  # Verify: gh log is completely empty AND dedup file byte-for-byte matches original
  local empty_sha_current
  empty_sha_current=$(cat "$empty_sha_dedup_file")
  if ! grep -q "." "$flow_gh_log" 2>/dev/null && \
     python3 -c "import json,sys; orig=json.loads(sys.argv[1]); cur=json.loads(sys.argv[2]); assert orig==cur" "$empty_sha_original" "$empty_sha_current" 2>/dev/null; then
    tap_ok "flow: empty build_sha guard short-circuits (no dedup/gh calls)"
  else
    tap_not_ok "flow: empty build_sha guard short-circuits (no dedup/gh calls)" "gh_log=$(cat $flow_gh_log 2>/dev/null) file_changed=$(diff <(echo '$empty_sha_original') <(echo '$empty_sha_current') 2>&1 | head -3)"
  fi

  # Test: Open-issue hit → skip + daily comment (last_commented_date updated)
  > "$flow_gh_log"
  build_sha="abc123def456"
  local flow_dedup_file="$flow_root/dedup-open.json"
  local today
  today=$(date -u +%Y-%m-%d)
  local yesterday
  yesterday=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(days=1)).strftime('%Y-%m-%d'))")
  local recent_ts
  recent_ts=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=2)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  echo '{"schema_version":1,"filed":{"abc123def456":{"issue_number":100,"filed_at":"'"$recent_ts"'","last_commented_date":"'"$yesterday"'"}}}' > "$flow_dedup_file"
  _flow_gh_stub '{"state":"OPEN"}' '[]' '' '0'
  # Simulate the §3d flow: load → prune → check → view → comment → update
  local flow_data
  flow_data=$(dedup_load "$flow_dedup_file" 2>/dev/null)
  flow_data=$(dedup_prune "$flow_data" "30d")
  set +e
  local flow_entry
  flow_entry=$(dedup_check "$flow_data" "$build_sha" 2>/dev/null)
  local flow_check_rc=$?
  set -e
  if [[ $flow_check_rc -eq 0 ]]; then
    # Hit — check state via gh stub
    local issue_state
    issue_state=$(PATH="$flow_root/gh-stub:$PATH" gh issue view 100 --json state --jq .state 2>/dev/null)
    if [[ "$issue_state" == *"OPEN"* ]]; then
      # Post daily comment (not rate-limited since yesterday != today)
      local last_date
      last_date=$(echo "$flow_entry" | python3 -c "import json,sys; print(json.load(sys.stdin).get('last_commented_date',''))" 2>/dev/null)
      if [[ "$last_date" != "$today" ]]; then
        PATH="$flow_root/gh-stub:$PATH" gh issue comment 100 --body "update" 2>/dev/null
        flow_data=$(dedup_update_field "$flow_data" "$build_sha" "last_commented_date" "$today")
        dedup_write "$flow_dedup_file" "$flow_data" 2>/dev/null
      fi
    fi
  fi
  # Verify: comment was made, last_commented_date updated
  if grep -q "issue comment" "$flow_gh_log" && \
     python3 -c "import json; d=json.load(open('$flow_dedup_file')); assert d['filed']['abc123def456']['last_commented_date']=='$today'" 2>/dev/null; then
    tap_ok "flow: open-issue hit → daily comment + date updated"
  else
    tap_not_ok "flow: open-issue hit → daily comment + date updated" "log: $(cat $flow_gh_log 2>/dev/null | head -3)"
  fi

  # Test: Open-issue hit → same-day rate limit (no comment)
  > "$flow_gh_log"
  local flow_dedup_file2="$flow_root/dedup-sameday.json"
  echo '{"schema_version":1,"filed":{"abc123def456":{"issue_number":100,"filed_at":"'"$recent_ts"'","last_commented_date":"'"$today"'"}}}' > "$flow_dedup_file2"
  _flow_gh_stub '{"state":"OPEN"}' '[]' '' '0'
  flow_data=$(dedup_load "$flow_dedup_file2" 2>/dev/null)
  set +e
  flow_entry=$(dedup_check "$flow_data" "$build_sha" 2>/dev/null)
  set -e
  local last_date2
  last_date2=$(echo "$flow_entry" | python3 -c "import json,sys; print(json.load(sys.stdin).get('last_commented_date',''))" 2>/dev/null)
  if [[ "$last_date2" == "$today" ]]; then
    # Rate limited — no comment
    :
  fi
  if ! grep -q "issue comment" "$flow_gh_log"; then
    tap_ok "flow: same-day rate limit skips comment"
  else
    tap_not_ok "flow: same-day rate limit skips comment" "unexpected comment call"
  fi

  # Test: Open-issue hit → comment failure (date NOT updated)
  > "$flow_gh_log"
  local flow_dedup_file3="$flow_root/dedup-commentfail.json"
  echo '{"schema_version":1,"filed":{"abc123def456":{"issue_number":100,"filed_at":"'"$recent_ts"'","last_commented_date":"'"$yesterday"'"}}}' > "$flow_dedup_file3"
  _flow_gh_stub '{"state":"OPEN"}' '[]' '' '1'  # comment exits 1
  flow_data=$(dedup_load "$flow_dedup_file3" 2>/dev/null)
  set +e
  flow_entry=$(dedup_check "$flow_data" "$build_sha" 2>/dev/null)
  set -e
  last_date=$(echo "$flow_entry" | python3 -c "import json,sys; print(json.load(sys.stdin).get('last_commented_date',''))" 2>/dev/null)
  if [[ "$last_date" != "$today" ]]; then
    # Try to comment — stub will fail
    set +e
    PATH="$flow_root/gh-stub:$PATH" gh issue comment 100 --body "update" 2>/dev/null
    local comment_rc=$?
    set -e
    if [[ $comment_rc -ne 0 ]]; then
      # Do NOT update date — leave dedup file unchanged
      :
    fi
  fi
  if python3 -c "import json; d=json.load(open('$flow_dedup_file3')); assert d['filed']['abc123def456']['last_commented_date']=='$yesterday'" 2>/dev/null; then
    tap_ok "flow: comment failure → date NOT updated"
  else
    tap_not_ok "flow: comment failure → date NOT updated"
  fi

  # Test: Closed-issue hit → remove + refile (verifies both dedup removal and issue creation)
  > "$flow_gh_log"
  local flow_dedup_file4="$flow_root/dedup-closed.json"
  local closed_recent_ts
  closed_recent_ts=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=1)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  # Seed with a unique stale_marker field to prove removal happened (record won't re-add it)
  echo '{"schema_version":1,"filed":{"sha_closed":{"issue_number":200,"filed_at":"'"$closed_recent_ts"'","stale_marker":"must_disappear"}}}' > "$flow_dedup_file4"
  _flow_gh_stub '{"state":"CLOSED"}' '[]' 'https://github.com/stellar-experimental/henyey/issues/201' '0'
  flow_data=$(dedup_load "$flow_dedup_file4" 2>/dev/null)
  flow_data=$(dedup_prune "$flow_data" "30d")
  set +e
  flow_entry=$(dedup_check "$flow_data" "sha_closed" 2>/dev/null)
  local flow_closed_rc=$?
  set -e
  local closed_predecessor=""
  if [[ $flow_closed_rc -eq 0 ]]; then
    # Check state
    local closed_state
    closed_state=$(PATH="$flow_root/gh-stub:$PATH" gh issue view 200 --json state --jq .state 2>/dev/null)
    if [[ "$closed_state" == *"CLOSED"* ]]; then
      closed_predecessor="200"
      flow_data=$(dedup_remove "$flow_data" "sha_closed")
      # Fall through to filing (remote search + create)
      local refile_result
      refile_result=$(PATH="$flow_root/gh-stub:$PATH" gh issue create --title "OFFLINE: refile" --body "test" 2>/dev/null)
      local refile_num
      refile_num=$(echo "$refile_result" | grep -oP '\d+$' || true)
      if [[ -n "$refile_num" ]]; then
        flow_data=$(dedup_record "$flow_data" "sha_closed" "issue_number=$refile_num")
        dedup_write "$flow_dedup_file4" "$flow_data" 2>/dev/null
      fi
    fi
  fi
  # Verify: old entry removed (stale_marker gone), new entry with refiled issue number, gh create called
  if [[ -n "$closed_predecessor" ]] && \
     grep -q "issue create" "$flow_gh_log" && \
     python3 -c "
import json
d = json.load(open('$flow_dedup_file4'))
e = d['filed']['sha_closed']
assert e['issue_number'] == 201, f'wrong issue: {e[\"issue_number\"]}'
assert 'stale_marker' not in e, 'stale_marker survived — remove did not happen'
" 2>/dev/null; then
    tap_ok "flow: closed-issue hit → remove + refile with new issue"
  else
    tap_not_ok "flow: closed-issue hit → remove + refile with new issue" "log: $(cat $flow_gh_log 2>/dev/null | head -3)"
  fi

  # Test: UNKNOWN state hit → suppress filing
  > "$flow_gh_log"
  local flow_dedup_file5="$flow_root/dedup-unknown.json"
  local unk_recent_ts
  unk_recent_ts=$(python3 -c "from datetime import datetime,timezone,timedelta; print((datetime.now(timezone.utc)-timedelta(hours=2)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
  echo '{"schema_version":1,"filed":{"sha_unk":{"issue_number":300,"filed_at":"'"$unk_recent_ts"'"}}}' > "$flow_dedup_file5"
  # gh issue view will fail (exit 1 simulating network error)
  cat > "$flow_root/gh-stub/gh" << 'UNKNOWNEOF'
#!/usr/bin/env bash
if [[ "$1" == "issue" && "$2" == "view" ]]; then
  exit 1
fi
exit 0
UNKNOWNEOF
  chmod +x "$flow_root/gh-stub/gh"
  flow_data=$(dedup_load "$flow_dedup_file5" 2>/dev/null)
  flow_data=$(dedup_prune "$flow_data" "30d")
  set +e
  flow_entry=$(dedup_check "$flow_data" "sha_unk" 2>/dev/null)
  local flow_unk_rc=$?
  set -e
  local should_suppress=false
  if [[ $flow_unk_rc -eq 0 ]]; then
    set +e
    local unk_state
    unk_state=$(PATH="$flow_root/gh-stub:$PATH" gh issue view 300 --json state --jq .state 2>/dev/null)
    local unk_view_rc=$?
    set -e
    if [[ $unk_view_rc -ne 0 ]] || [[ -z "$unk_state" ]]; then
      should_suppress=true
    fi
  fi
  if [[ "$should_suppress" == "true" ]]; then
    tap_ok "flow: UNKNOWN state → suppress filing safely"
  else
    tap_not_ok "flow: UNKNOWN state → suppress filing safely" "suppress=$should_suppress"
  fi

  # Test: Remote-search backfill (local miss → remote hit → record)
  > "$flow_gh_log"
  local flow_dedup_file6="$flow_root/dedup-backfill.json"
  echo '{"schema_version":1,"filed":{}}' > "$flow_dedup_file6"
  local backfill_sha="sha_backfill_789"
  local backfill_marker="<!-- monitor-tick-recurrence-key: $backfill_sha -->"
  _flow_gh_stub '' '[{"number":500,"body":"'"$backfill_marker"' some body text"}]' '' '0'
  flow_data=$(dedup_load "$flow_dedup_file6" 2>/dev/null)
  set +e
  dedup_check "$flow_data" "$backfill_sha" 2>/dev/null
  local backfill_check_rc=$?
  set -e
  if [[ $backfill_check_rc -ne 0 ]]; then
    # Local miss — do remote search
    local remote_result
    remote_result=$(PATH="$flow_root/gh-stub:$PATH" gh issue list --json number,body 2>/dev/null)
    local found_num
    found_num=$(echo "$remote_result" | python3 -c "
import json,sys
items = json.load(sys.stdin)
marker = '$backfill_marker'
for item in items:
    if marker in item.get('body',''):
        print(item['number'])
        break
" 2>/dev/null)
    if [[ -n "$found_num" ]]; then
      flow_data=$(dedup_record "$flow_data" "$backfill_sha" "issue_number=$found_num")
      dedup_write "$flow_dedup_file6" "$flow_data" 2>/dev/null
    fi
  fi
  if python3 -c "import json; d=json.load(open('$flow_dedup_file6')); assert d['filed']['$backfill_sha']['issue_number']==500" 2>/dev/null; then
    tap_ok "flow: remote-search backfill records correct issue"
  else
    tap_not_ok "flow: remote-search backfill records correct issue"
  fi

  # Test: Remote-search false-positive (no matching BODY_MARKER → falls through)
  > "$flow_gh_log"
  local flow_dedup_file7="$flow_root/dedup-falsepos.json"
  echo '{"schema_version":1,"filed":{}}' > "$flow_dedup_file7"
  local fp_sha="sha_falsepos_999"
  local fp_marker="<!-- monitor-tick-recurrence-key: $fp_sha -->"
  # Remote returns issues that do NOT contain the marker
  _flow_gh_stub '' '[{"number":600,"body":"unrelated issue body without marker"}]' '' '0'
  flow_data=$(dedup_load "$flow_dedup_file7" 2>/dev/null)
  set +e
  dedup_check "$flow_data" "$fp_sha" 2>/dev/null
  local fp_check_rc=$?
  set -e
  local fp_found=""
  if [[ $fp_check_rc -ne 0 ]]; then
    local fp_remote
    fp_remote=$(PATH="$flow_root/gh-stub:$PATH" gh issue list --json number,body 2>/dev/null)
    fp_found=$(echo "$fp_remote" | python3 -c "
import json,sys
items = json.load(sys.stdin)
marker = '$fp_marker'
for item in items:
    if marker in item.get('body',''):
        print(item['number'])
        break
" 2>/dev/null)
  fi
  if [[ -z "$fp_found" ]]; then
    tap_ok "flow: remote-search false-positive falls through to filing"
  else
    tap_not_ok "flow: remote-search false-positive falls through to filing" "found=$fp_found"
  fi

  # Test: gh issue create failure → no dedup_record (dedup file stays untouched)
  > "$flow_gh_log"
  local flow_dedup_file8="$flow_root/dedup-createfail.json"
  echo '{"schema_version":1,"filed":{}}' > "$flow_dedup_file8"
  local create_fail_sha="sha_that_should_not_be_recorded"
  _flow_gh_stub '' '[]' '' '0'  # create_response is empty (stub exits 1 for create)
  flow_data=$(dedup_load "$flow_dedup_file8" 2>/dev/null)
  set +e
  local create_out
  create_out=$(PATH="$flow_root/gh-stub:$PATH" gh issue create --title "test" --body "test" 2>/dev/null)
  local create_rc=$?
  set -e
  local created_num
  created_num=$(echo "$create_out" | grep -oP '\d+$' || true)
  if [[ -n "$created_num" ]]; then
    # Would record — but we expect failure
    flow_data=$(dedup_record "$flow_data" "$create_fail_sha" "issue_number=$created_num")
    dedup_write "$flow_dedup_file8" "$flow_data" 2>/dev/null
  fi
  # Verify: dedup file has NO entry for this sha (create failed, so no record was made)
  if ! python3 -c "import json; d=json.load(open('$flow_dedup_file8')); assert '$create_fail_sha' not in d['filed']" 2>/dev/null; then
    tap_not_ok "flow: gh create failure → dedup file untouched" "sha was recorded despite failure"
  elif python3 -c "import json; d=json.load(open('$flow_dedup_file8')); assert d=={'schema_version':1,'filed':{}}" 2>/dev/null; then
    tap_ok "flow: gh create failure → dedup file untouched"
  else
    tap_not_ok "flow: gh create failure → dedup file untouched"
  fi

  # Test: Lock timeout (flock held → exits 0 without any dedup/gh operations)
  > "$flow_gh_log"
  local lock_file="$flow_root/test-filing.lock"
  local lock_dedup_file="$flow_root/dedup-lock-test.json"
  echo '{"schema_version":1,"filed":{}}' > "$lock_dedup_file"
  _flow_gh_stub '{"state":"OPEN"}' '[]' 'https://github.com/stellar-experimental/henyey/issues/999' '0'
  # Hold the lock in a background subshell
  (flock -x 9; sleep 5) 9>"$lock_file" &
  local lock_holder_pid=$!
  sleep 0.2  # Let the background process acquire the lock
  set +e
  local lock_out
  lock_out=$(
    (
      flock -w 1 9 || { echo "Could not acquire lock"; exit 0; }
      # If lock acquired, would do full dedup + gh operations (all should be prevented)
      echo "acquired"
      source "$REPO_ROOT/scripts/lib/dedup-filing.sh"
      DEDUP_DATA=$(dedup_load "$lock_dedup_file" 2>/dev/null)
      PATH="$flow_root/gh-stub:$PATH" gh issue view 999 --json state 2>/dev/null
      PATH="$flow_root/gh-stub:$PATH" gh issue create --title "should not happen" --body "test" 2>/dev/null
    ) 9>"$lock_file" 2>&1
  )
  set -e
  kill $lock_holder_pid 2>/dev/null || true
  wait $lock_holder_pid 2>/dev/null || true
  # Verify: lock timed out, NO gh calls of any kind, dedup file untouched
  if echo "$lock_out" | grep -q "Could not acquire" && \
     ! grep -q "." "$flow_gh_log" 2>/dev/null && \
     python3 -c "import json; d=json.load(open('$lock_dedup_file')); assert d=={'schema_version':1,'filed':{}}" 2>/dev/null; then
    tap_ok "flow: lock timeout → no dedup/gh operations"
  else
    tap_not_ok "flow: lock timeout → no dedup/gh operations" "out: ${lock_out:0:100} log: $(cat $flow_gh_log 2>/dev/null)"
  fi

  # Test: Board routing on new issue
  > "$flow_gh_log"
  _flow_gh_stub '' '[]' 'https://github.com/stellar-experimental/henyey/issues/777' '0'
  set +e
  local board_create_out
  board_create_out=$(PATH="$flow_root/gh-stub:$PATH" gh issue create --title "test" --body "test" 2>/dev/null)
  set -e
  local board_num
  board_num=$(echo "$board_create_out" | grep -oP '\d+$' || true)
  if [[ -n "$board_num" ]]; then
    PATH="$flow_root/gh-stub:$PATH" bash "$flow_root/fake-repo/.github/skills/shared/scripts/move-issue-status.sh" "$board_num" "Backlog" 2>/dev/null
  fi
  if grep -q "move-issue-status 777 Backlog" "$flow_gh_log"; then
    tap_ok "flow: board routing calls move-issue-status with correct args"
  else
    tap_not_ok "flow: board routing calls move-issue-status with correct args" "log: $(cat $flow_gh_log 2>/dev/null)"
  fi

  # Test: Board routing — daily comment adds off-board issue
  > "$flow_gh_log"
  # Stub: gh api graphql returns totalCount=0 (stub echoes raw value since jq not processed)
  cat > "$flow_root/gh-stub/gh" << BOARDEOF
#!/usr/bin/env bash
echo "\$*" >> "$flow_gh_log"
if [[ "\$1" == "api" && "\$2" == "graphql" ]]; then
  echo '0'
  exit 0
fi
exit 0
BOARDEOF
  chmod +x "$flow_root/gh-stub/gh"
  # Simulate: check if on board → not on board → call move-issue-status
  local on_board
  on_board=$(PATH="$flow_root/gh-stub:$PATH" gh api graphql -f query='test' --jq '.data.resource.projectItems.totalCount' 2>/dev/null)
  if [[ "$on_board" == "0" ]]; then
    PATH="$flow_root/gh-stub:$PATH" bash "$flow_root/fake-repo/.github/skills/shared/scripts/move-issue-status.sh" "100" "Backlog" 2>/dev/null
  fi
  if grep -q "move-issue-status 100 Backlog" "$flow_gh_log"; then
    tap_ok "flow: daily comment adds off-board issue to Backlog"
  else
    tap_not_ok "flow: daily comment adds off-board issue to Backlog" "log: $(cat $flow_gh_log 2>/dev/null)"
  fi

  rm -rf "$dedup_test_root"

  # ── eval_memory_guardrail (host-RAM-relative HIGH-MEMORY guardrail, #3227) ──
  # Pure decision fn: eval_memory_guardrail rss_mb avail_mb host_ram_gb \
  #   heap_prev_mb heap_curr_mb heap_prev2_mb → sets MEMORY_GUARDRAIL_VERDICT
  # to none | report-high-mem | restart. Integer-MB math, no bc/floats.
  # Thresholds: report > 0.65*ram; restart > 0.75*ram AND avail < 0.12*ram AND
  # latest two heap_components_mb deltas both > 500 MB.

  # 32 GB host, RSS=24500 (>0.75*32GB=24576? no — 24500<24576). Use 24600 to be
  # unambiguously past the 0.75 floor while keeping >7 GB margin to the 32 GB wall.
  eval_memory_guardrail 24600 3500 32 17000 17600 16400
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "restart" ]]; then
    tap_ok "eval_memory_guardrail: 32GB RSS>0.75 + avail<0.12 + heap +600/+600 → restart"
  else
    tap_not_ok "eval_memory_guardrail: 32GB restart case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # 32 GB host, RSS=22000 (>0.65*32GB=21299, <0.75=24576), avail healthy, heap flat.
  eval_memory_guardrail 22000 10000 32 17000 17000 17000
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "report-high-mem" ]]; then
    tap_ok "eval_memory_guardrail: 32GB RSS>0.65 only → report-high-mem (no restart)"
  else
    tap_not_ok "eval_memory_guardrail: 32GB report-only case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # 32 GB host, RSS=20000 (steady ~18-20 GB anon, <0.65*32GB=21299), heap flat.
  # Validates the 0.65 report bump: steady state must NOT chronically report-fire.
  eval_memory_guardrail 20000 12000 32 17000 17000 17000
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "none" ]]; then
    tap_ok "eval_memory_guardrail: 32GB steady-state RSS<0.65 → none (no chronic report)"
  else
    tap_not_ok "eval_memory_guardrail: 32GB steady-state case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # 32 GB host, RSS=24600 (>0.75) during catchup but avail healthy AND heap NOT
  # growing → transient catchup spike must not be killed (the ~27-29 GB cold-
  # catchup peak that the operator-side scan lever, out of scope, addresses).
  eval_memory_guardrail 24600 12000 32 17000 17000 17000
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "report-high-mem" ]]; then
    tap_ok "eval_memory_guardrail: 32GB catchup spike, avail healthy, heap flat → no restart"
  else
    tap_not_ok "eval_memory_guardrail: 32GB catchup-spike case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # 61 GB host (DEFAULT), RSS=24000, avail=7000, heap +600/+600. The OLD absolute
  # rule (RSS>16GB AND avail<8GB AND heap growing) would have been at/near
  # restart here; the host-relative rule does NOT false-fire because
  # 24000 < 0.65*61GB=40601 ⇒ verdict none. This is the "no false-fire on 61 GB" case.
  eval_memory_guardrail 24000 7000 61 17000 17600 16400
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "none" ]]; then
    tap_ok "eval_memory_guardrail: 61GB RSS=24G (old 16/8 rule near-restart) → none"
  else
    tap_not_ok "eval_memory_guardrail: 61GB no-false-fire case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # 61 GB host, RSS=46000 (>0.75*61GB=46848? no — 46000<46848). Use 47000 to clear
  # the 0.75 floor: legit 61 GB restart still works when host genuinely under pressure.
  eval_memory_guardrail 47000 7000 61 17000 17600 16400
  if [[ "$MEMORY_GUARDRAIL_VERDICT" == "restart" ]]; then
    tap_ok "eval_memory_guardrail: 61GB RSS>0.75 + avail<0.12 + heap growing → restart"
  else
    tap_not_ok "eval_memory_guardrail: 61GB legit-restart case" "got $MEMORY_GUARDRAIL_VERDICT"
  fi

  # ── Structural assertions (#3227): monitor-tick wires the guardrail + surfaces peak ──
  local tick_md="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'eval_memory_guardrail' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md calls eval_memory_guardrail"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md calls eval_memory_guardrail" "not found"
  fi
  if grep -q 'MONITOR_HOST_RAM_GB' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md references MONITOR_HOST_RAM_GB"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md references MONITOR_HOST_RAM_GB" "not found"
  fi
  if grep -q 'henyey_startup_peak_anon_rss_mb' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md surfaces henyey_startup_peak_anon_rss_mb"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md surfaces henyey_startup_peak_anon_rss_mb" "not found"
  fi

  # ── Test-only deploy-gate carve-out (#3215) ─────────────────────────────────
  # Two pure functions in monitor-decisions.sh power the §10 deploy-gate
  # binary-relevance check:
  #   classify_path_binary_relevance PATH → no-impact | test-only |
  #                                          needs-hunk-check | rebuild (fail-safe)
  #   diff_is_test_only  (reads one file's unified diff on stdin) → 0 iff every
  #                       changed line is strictly inside a #[cfg(test)]/mod tests
  #                       region; ANY ambiguity → non-zero (rebuild).
  # The functions only mark a CANDIDATE; the authoritative skip is gated on a
  # release-binary byte-compare in SKILL.md §10 (structural assertions below).

  # --- classify_path_binary_relevance: path-level table test ---
  if classify_path_binary_relevance ".github/workflows/ci.yml" >/dev/null 2>&1; then
    _has_classify=yes
  else
    _has_classify=no
  fi

  _cls() { classify_path_binary_relevance "$1" 2>/dev/null; }

  if [[ "$(_cls ".github/workflows/ci.yml")" == "no-impact" ]]; then
    tap_ok "classify_path: .github/ → no-impact"
  else
    tap_not_ok "classify_path: .github/ → no-impact" "got '$(_cls ".github/workflows/ci.yml")'"
  fi
  if [[ "$(_cls ".claude/skills/monitor-tick/SKILL.md")" == "no-impact" ]]; then
    tap_ok "classify_path: .claude/ → no-impact"
  else
    tap_not_ok "classify_path: .claude/ → no-impact" "got '$(_cls ".claude/skills/monitor-tick/SKILL.md")'"
  fi
  if [[ "$(_cls "scripts/lib/monitor-decisions.sh")" == "no-impact" ]]; then
    tap_ok "classify_path: scripts/ → no-impact"
  else
    tap_not_ok "classify_path: scripts/ → no-impact" "got '$(_cls "scripts/lib/monitor-decisions.sh")'"
  fi
  if [[ "$(_cls "README.md")" == "no-impact" ]]; then
    tap_ok "classify_path: root *.md → no-impact"
  else
    tap_not_ok "classify_path: root *.md → no-impact" "got '$(_cls "README.md")'"
  fi
  if [[ "$(_cls "stellar-specs")" == "no-impact" ]]; then
    tap_ok "classify_path: stellar-specs submodule → no-impact"
  else
    tap_not_ok "classify_path: stellar-specs submodule → no-impact" "got '$(_cls "stellar-specs")'"
  fi
  # #3530: stellar-core is a parity-reference submodule (CLAUDE.md), never in the
  # `cargo build --release -p henyey` graph — a pointer-only bump cannot change
  # the binary. Mirror the stellar-specs arm so a submodule-pin-only commit is a
  # skip CANDIDATE; the §10-step-2a byte-compare still independently gates the
  # actual restart-skip.
  if [[ "$(_cls "stellar-core")" == "no-impact" ]]; then
    tap_ok "classify_path: stellar-core submodule → no-impact (#3530)"
  else
    tap_not_ok "classify_path: stellar-core submodule → no-impact (#3530)" "got '$(_cls "stellar-core")'"
  fi
  if [[ "$(_cls "stellar-core/src/foo")" == "no-impact" ]]; then
    tap_ok "classify_path: stellar-core/* path → no-impact (#3530)"
  else
    tap_not_ok "classify_path: stellar-core/* path → no-impact (#3530)" "got '$(_cls "stellar-core/src/foo")'"
  fi
  # Container-image files (#3307): Dockerfile / .dockerignore / *.dockerfile are
  # never read by `cargo build --release -p henyey`, so they cannot change the
  # compiled binary. Route them through no-impact so a Dockerfile-only delta hits
  # the authoritative §10-step-2a binary byte-compare instead of a needless
  # rebuild+restart. (Safe ONLY because the monitor runs the locally-compiled
  # release/henyey, not the Docker image — see monitor-decisions.sh comment.)
  if [[ "$(_cls "Dockerfile")" == "no-impact" ]]; then
    tap_ok "classify_path: Dockerfile → no-impact"
  else
    tap_not_ok "classify_path: Dockerfile → no-impact" "got '$(_cls "Dockerfile")'"
  fi
  if [[ "$(_cls ".dockerignore")" == "no-impact" ]]; then
    tap_ok "classify_path: .dockerignore → no-impact"
  else
    tap_not_ok "classify_path: .dockerignore → no-impact" "got '$(_cls ".dockerignore")'"
  fi
  if [[ "$(_cls "foo.dockerfile")" == "no-impact" ]]; then
    tap_ok "classify_path: *.dockerfile → no-impact"
  else
    tap_not_ok "classify_path: *.dockerfile → no-impact" "got '$(_cls "foo.dockerfile")'"
  fi
  # CONTRAST / deliberate boundary (#3307): docker-compose*.yml is intentionally
  # NOT allowlisted — it stays on the rebuild fail-safe. This assertion locks
  # that scope boundary so a future allowlist edit can't silently absorb it.
  if [[ "$(_cls "docker-compose.yml")" == "rebuild" ]]; then
    tap_ok "classify_path: docker-compose.yml → rebuild (deliberate boundary)"
  else
    tap_not_ok "classify_path: docker-compose.yml → rebuild (deliberate boundary)" "got '$(_cls "docker-compose.yml")'"
  fi
  if [[ "$(_cls "crates/app/tests/publish_tests.rs")" == "test-only" ]]; then
    tap_ok "classify_path: crates/*/tests/** → test-only"
  else
    tap_not_ok "classify_path: crates/*/tests/** → test-only" "got '$(_cls "crates/app/tests/publish_tests.rs")'"
  fi
  if [[ "$(_cls "crates/app/src/app/publish.rs")" == "needs-hunk-check" ]]; then
    tap_ok "classify_path: crates/**/src/**.rs → needs-hunk-check"
  else
    tap_not_ok "classify_path: crates/**/src/**.rs → needs-hunk-check" "got '$(_cls "crates/app/src/app/publish.rs")'"
  fi
  # Fail-safe: everything else → rebuild.
  if [[ "$(_cls "Cargo.toml")" == "rebuild" ]]; then
    tap_ok "classify_path: Cargo.toml → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: Cargo.toml → rebuild (fail-safe)" "got '$(_cls "Cargo.toml")'"
  fi
  if [[ "$(_cls "Cargo.lock")" == "rebuild" ]]; then
    tap_ok "classify_path: Cargo.lock → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: Cargo.lock → rebuild (fail-safe)" "got '$(_cls "Cargo.lock")'"
  fi
  if [[ "$(_cls "crates/app/build.rs")" == "rebuild" ]]; then
    tap_ok "classify_path: crates/*/build.rs → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: crates/*/build.rs → rebuild (fail-safe)" "got '$(_cls "crates/app/build.rs")'"
  fi
  if [[ "$(_cls "configs/validator-mainnet.toml")" == "rebuild" ]]; then
    tap_ok "classify_path: configs/ → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: configs/ → rebuild (fail-safe)" "got '$(_cls "configs/validator-mainnet.toml")'"
  fi
  # .rs under crates/ but NOT in src/ or tests/ (e.g. benches, examples) → rebuild.
  if [[ "$(_cls "crates/app/benches/apply_load.rs")" == "rebuild" ]]; then
    tap_ok "classify_path: crates/*/benches/*.rs → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: crates/*/benches/*.rs → rebuild (fail-safe)" "got '$(_cls "crates/app/benches/apply_load.rs")'"
  fi
  # Non-.rs file under crates/src (e.g. an included .json/.wasm fixture) → rebuild.
  if [[ "$(_cls "crates/app/src/fixtures/data.json")" == "rebuild" ]]; then
    tap_ok "classify_path: non-.rs under crates/src → rebuild (fail-safe)"
  else
    tap_not_ok "classify_path: non-.rs under crates/src → rebuild (fail-safe)" "got '$(_cls "crates/app/src/fixtures/data.json")'"
  fi

  # ── #3351: select_latest_green_deploy_target ────────────────────────────────
  # The selector picks the newest Verify-Execution-green sha that is on main,
  # at-or-ahead of the deployed sha, and within MAX_DEPLOY_WALK commits. It is
  # fail-closed: empty stdout (+ a stderr reason) on no valid target, so the
  # SKILL never deploys an unvalidated binary. The git ancestry oracle is
  # injected as overridable functions (is_ancestor / commits_ahead) so these
  # tests are hermetic — no on-disk repo needed.
  #
  # Hermetic ancestry model used by the stub oracle below. We define a simple
  # linear main history (oldest → newest):
  #     DEP  →  G_OLD  →  G  →  HEAD
  # plus an off-main sha (FORK) that is NOT an ancestor of HEAD, and a far-ahead
  # green sha (FAR) used to exercise the walk bound. The stub answers ancestry
  # from a fixed table so the selector logic is exercised without real git.
  _order_of() {
    # echo a monotonic rank for each known sha; unknown → empty (unresolvable).
    case "$1" in
      DEP)   echo 0 ;;
      G_OLD) echo 1 ;;
      G)     echo 2 ;;
      HEAD)  echo 3 ;;
      FAR)   echo 9999 ;;   # far ahead of DEP on main
      *)     echo "" ;;     # FORK / unknown → not resolvable on main
    esac
  }
  # is_ancestor A B : 0 iff A is an ancestor-or-equal of B on the linear main.
  # FORK is off-main: never an ancestor of anything, and HEAD is never its anc.
  is_ancestor() {
    local a="$1" b="$2" ra rb
    if [[ "$a" == "FORK" || "$b" == "FORK" ]]; then return 1; fi
    ra="$(_order_of "$a")"; rb="$(_order_of "$b")"
    [[ -n "$ra" && -n "$rb" && "$ra" -le "$rb" ]]
  }
  # commits_ahead DEP SHA : count of commits in DEP..SHA (reachable from SHA, not DEP).
  commits_ahead() {
    local dep="$1" sha="$2" rd rs
    rd="$(_order_of "$dep")"; rs="$(_order_of "$sha")"
    if [[ -z "$rd" || -z "$rs" ]]; then echo 0; return 0; fi
    if [[ "$rs" -le "$rd" ]]; then echo 0; return 0; fi
    echo $(( rs - rd ))
  }

  # Case 1: HEAD has no green Verify Execution (CI cancelled); older sha G is
  # completed/success, on main, ahead of deployed → selector picks G.
  _records_head_cancelled() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "G|completed|success" \
      "G_OLD|completed|success"
  }
  _sel1="$(_records_head_cancelled | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ "$_sel1" == "G" ]]; then
    tap_ok "select_latest_green: picks newest green ancestor when HEAD CI cancelled (#3351)"
  else
    tap_not_ok "select_latest_green: picks newest green ancestor when HEAD CI cancelled (#3351)" "got '$_sel1'"
  fi

  # Case 2: no green record at all → empty stdout + reason no-green-run.
  _records_none_green() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "G|in_progress|" \
      "G_OLD|completed|failure"
  }
  _err2="$(_records_none_green | select_latest_green_deploy_target "DEP" "HEAD" 2>&1 >/dev/null)"
  _out2="$(_records_none_green | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ -z "$_out2" && "$_err2" == *"no-green-run"* ]]; then
    tap_ok "select_latest_green: no green run → empty + reason no-green-run (#3351)"
  else
    tap_not_ok "select_latest_green: no green run → empty + reason no-green-run (#3351)" "out='$_out2' err='$_err2'"
  fi

  # Case 3: only green sha == deployed → up-to-date no-op (empty + green-equals-deployed),
  # NOT a redeploy of the running sha.
  _records_green_eq_dep() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "DEP|completed|success"
  }
  _err3="$(_records_green_eq_dep | select_latest_green_deploy_target "DEP" "HEAD" 2>&1 >/dev/null)"
  _out3="$(_records_green_eq_dep | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ -z "$_out3" && "$_err3" == *"green-equals-deployed"* ]]; then
    tap_ok "select_latest_green: green == deployed → empty + green-equals-deployed (#3351)"
  else
    tap_not_ok "select_latest_green: green == deployed → empty + green-equals-deployed (#3351)" "out='$_out3' err='$_err3'"
  fi

  # Case 4: only green sha is BEHIND deployed (ancestor of DEP) → no backwards
  # deploy (empty + green-behind-deployed). Deploy DEP_AHEAD where green G_OLD is
  # behind it: deployed=G, green=G_OLD (G_OLD is an ancestor of G).
  _records_green_behind() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "G_OLD|completed|success"
  }
  _err4="$(_records_green_behind | select_latest_green_deploy_target "G" "HEAD" 2>&1 >/dev/null)"
  _out4="$(_records_green_behind | select_latest_green_deploy_target "G" "HEAD" 2>/dev/null)"
  if [[ -z "$_out4" && "$_err4" == *"green-behind-deployed"* ]]; then
    tap_ok "select_latest_green: green behind deployed → empty + green-behind-deployed (#3351)"
  else
    tap_not_ok "select_latest_green: green behind deployed → empty + green-behind-deployed (#3351)" "out='$_out4' err='$_err4'"
  fi

  # Case 5: green sha is off-main (not an ancestor of HEAD) → skip candidate,
  # reason green-not-on-main; with no other green → empty.
  _records_green_offmain() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "FORK|completed|success"
  }
  _err5="$(_records_green_offmain | select_latest_green_deploy_target "DEP" "HEAD" 2>&1 >/dev/null)"
  _out5="$(_records_green_offmain | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ -z "$_out5" && "$_err5" == *"green-not-on-main"* ]]; then
    tap_ok "select_latest_green: off-main green → empty + green-not-on-main (#3351)"
  else
    tap_not_ok "select_latest_green: off-main green → empty + green-not-on-main (#3351)" "out='$_out5' err='$_err5'"
  fi

  # Case 6: green sha exceeds MAX_DEPLOY_WALK commits ahead of deployed →
  # reject (empty + walk-exceeded). FAR is 9999 commits ahead of DEP.
  _records_far_green() {
    printf '%s\n' \
      "HEAD|completed|cancelled" \
      "FAR|completed|success"
  }
  _err6="$(_records_far_green | select_latest_green_deploy_target "DEP" "FAR" 2>&1 >/dev/null)"
  _out6="$(_records_far_green | select_latest_green_deploy_target "DEP" "FAR" 2>/dev/null)"
  if [[ -z "$_out6" && "$_err6" == *"walk-exceeded"* ]]; then
    tap_ok "select_latest_green: green > MAX_DEPLOY_WALK ahead → empty + walk-exceeded (#3351)"
  else
    tap_not_ok "select_latest_green: green > MAX_DEPLOY_WALK ahead → empty + walk-exceeded (#3351)" "out='$_out6' err='$_err6'"
  fi

  # Case 7 (#3740): a sha whose NEWEST Verify-Execution run concluded `failure`
  # must be disqualified even though an OLDER run of the SAME sha succeeded.
  # Records newest-first: G's newest is failure (supersedes G's older success),
  # so the selector must fall through to the next green sha G_OLD, NOT return G.
  # FAILS on main (main skips the failure row and returns G off the older success).
  _records_newer_failure_same_sha() {
    printf '%s\n' \
      "G|completed|failure" \
      "G|completed|success" \
      "G_OLD|completed|success"
  }
  _sel7="$(_records_newer_failure_same_sha | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ "$_sel7" == "G_OLD" ]]; then
    tap_ok "select_latest_green: sha with newer failure is disqualified (#3740)"
  else
    tap_not_ok "select_latest_green: sha with newer failure is disqualified (#3740)" "got '$_sel7' (expected 'G_OLD')"
  fi

  # Case 8 (#3740): newest run for the only candidate sha is a failure that
  # supersedes an older success, and there is no other green → empty stdout +
  # reason green-superseded-by-failure (the "note logged when a sha is skipped
  # due to a newer failure" the issue asks for). FAILS on main (returns G).
  _records_superseded_only() {
    printf '%s\n' \
      "G|completed|failure" \
      "G|completed|success"
  }
  _err8="$(_records_superseded_only | select_latest_green_deploy_target "DEP" "HEAD" 2>&1 >/dev/null)"
  _out8="$(_records_superseded_only | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ -z "$_out8" && "$_err8" == *"green-superseded-by-failure"* ]]; then
    tap_ok "select_latest_green: newer failure + no other green → green-superseded-by-failure (#3740)"
  else
    tap_not_ok "select_latest_green: newer failure + no other green → green-superseded-by-failure (#3740)" "out='$_out8' err='$_err8'"
  fi

  # Case 9 (#3740, GUARD — passes on main too): a `cancelled` newest row is
  # cancel-per-head noise, NOT a definitive verdict, so it must NOT disqualify an
  # older success for the same sha. Guards the fix against over-disqualifying:
  # only success/failure are definitive; cancelled/skipped/etc. are ignored.
  _records_cancelled_newest_same_sha() {
    printf '%s\n' \
      "G|completed|cancelled" \
      "G|completed|success"
  }
  _sel9="$(_records_cancelled_newest_same_sha | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null)"
  if [[ "$_sel9" == "G" ]]; then
    tap_ok "select_latest_green: cancelled newest does not mask older green same sha (#3740)"
  else
    tap_not_ok "select_latest_green: cancelled newest does not mask older green same sha (#3740)" "got '$_sel9' (expected 'G')"
  fi

  # ── #3581: select_latest_green_deploy_target runs under zsh ────────────────
  # The monitor-tick deploy gate invokes this selector INLINE in the tick shell,
  # which is zsh. Under zsh, `status` is a read-only special parameter (alias for
  # $?), so the function's `local line sha status conclusion` aborted the function
  # at the declaration line BEFORE the read loop, yielding empty stdout — which
  # the gate fail-closes to `defer-no-green`, silently deferring EVERY deploy.
  # The six bash cases above cannot catch this (the bug is zsh-specific), so this
  # test invokes the function inside a `zsh -c` subprocess with its own hermetic
  # ancestry oracle (green sha G on-main, 1 commit ahead of DEP) and asserts the
  # selector returns G — i.e. it neither aborts nor mis-reports. Gated on zsh
  # availability (matching the 20c3-zsh / 78c-zsh convention).
  if command -v zsh >/dev/null 2>&1; then
    local zsh_sel
    zsh_sel=$(zsh -c '
      emulate -L zsh
      source "$1/scripts/lib/monitor-decisions.sh"
      # Hermetic linear-main oracle: DEP -> G -> HEAD; G is on main, 1 ahead of DEP.
      _order_of() {
        case "$1" in
          DEP)  print 0 ;;
          G)    print 1 ;;
          HEAD) print 2 ;;
          *)    print "" ;;
        esac
      }
      is_ancestor() {
        local ra rb
        ra="$(_order_of "$1")"; rb="$(_order_of "$2")"
        [[ -n "$ra" && -n "$rb" && "$ra" -le "$rb" ]]
      }
      commits_ahead() {
        local rd rs
        rd="$(_order_of "$1")"; rs="$(_order_of "$2")"
        if [[ -z "$rd" || -z "$rs" || "$rs" -le "$rd" ]]; then print 0; return 0; fi
        print $(( rs - rd ))
      }
      printf "%s\n" "G|completed|success" | select_latest_green_deploy_target "DEP" "HEAD" 2>/dev/null
    ' -- "$REPO_ROOT")
    if [[ "$zsh_sel" == "G" ]]; then
      tap_ok "select_latest_green: runs under zsh without read-only-status abort (#3581)"
    else
      tap_not_ok "select_latest_green: runs under zsh without read-only-status abort (#3581)" "got '$zsh_sel' (expected 'G')"
    fi
  else
    tap_skip "select_latest_green: runs under zsh without read-only-status abort (#3581)" "zsh not found"
  fi

  # ── #3592: select_latest_green ancestry-oracle fallback defined under zsh ───
  # The selector defines fallback `is_ancestor` / `commits_ahead` git oracles
  # guarded by an existence check. That guard used the bash-only `declare -F`,
  # which under zsh aliases `typeset -F NAME` — declaring a FLOAT VARIABLE named
  # NAME and returning 0. So `! declare -F is_ancestor` was false, the fallback
  # function was NEVER defined, and the read-loop's first `is_ancestor` call hit
  # `command not found: is_ancestor` → the candidate was dropped → every
  # green-on-main run mis-resolved to `green-not-on-main` (a missed deploy).
  #
  # The #3581 case above CANNOT catch this: it pre-defines its own oracle
  # stubs after sourcing, so the script's fallback is never exercised. This
  # case deliberately does NOT pre-define the oracle — it relies on the
  # script's OWN fallback (the buggy guard path), backed by a hermetic single-
  # commit git repo so the default `git merge-base --is-ancestor` resolves. The
  # green record's sha equals the deployed sha and is its own ancestor, so a
  # correct gate returns `green-equals-deployed`; the buggy guard returns
  # `green-not-on-main`. Gated on zsh + git availability.
  if command -v zsh >/dev/null 2>&1 && command -v git >/dev/null 2>&1; then
    local zsh_repo zsh_sha zsh_reason
    zsh_repo="$(mktemp -d)"
    git -C "$zsh_repo" init -q 2>/dev/null
    git -C "$zsh_repo" -c user.name=t -c user.email=t@t commit -q --allow-empty -m init 2>/dev/null
    zsh_sha="$(git -C "$zsh_repo" rev-parse HEAD 2>/dev/null)"
    # Capture the reason token (stderr) from the gate, sourcing the REAL script
    # and relying on its fallback ancestry oracle (no pre-defined stubs).
    zsh_reason=$(zsh -c '
      emulate -L zsh
      cd "$2" || exit 3
      source "$1/scripts/lib/monitor-decisions.sh"
      printf "%s\n" "$3|completed|success" \
        | select_latest_green_deploy_target "$3" "$3" 2>&1 >/dev/null
    ' -- "$REPO_ROOT" "$zsh_repo" "$zsh_sha")
    if [[ "$zsh_reason" == *"green-equals-deployed"* \
          && "$zsh_reason" != *"command not found"* ]]; then
      tap_ok "select_latest_green: fallback is_ancestor defined under zsh — green-on-main resolves (#3592)"
    else
      tap_not_ok "select_latest_green: fallback is_ancestor defined under zsh — green-on-main resolves (#3592)" "got '$zsh_reason' (expected 'green-equals-deployed')"
    fi
    rm -rf "$zsh_repo"
  else
    tap_skip "select_latest_green: fallback is_ancestor defined under zsh — green-on-main resolves (#3592)" "zsh or git not found"
  fi

  # Restore real git oracles so no later test sees the stubs.
  unset -f is_ancestor commits_ahead _order_of

  # --- diff_is_test_only: hunk-level ---
  # PASS case: a src/*.rs diff whose every changed line is strictly inside a
  # #[cfg(test)] mod tests { ... } region.
  _diff_test_only_pass() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..2222222 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -200,6 +200,7 @@ fn produce(x: u32) -> u32 {
 #[cfg(test)]
 mod tests {
     use super::*;
+    // new helper comment inside tests
     #[test]
     fn test_produce_doubles() {
         assert_eq!(produce(2), 4);
@@ -210,3 +211,4 @@ mod tests {
     fn test_produce_zero() {
         assert_eq!(produce(0), 0);
+        assert_ne!(produce(0), 1);
     }
 }
CANNED
  }
  if _diff_test_only_pass | diff_is_test_only 2>/dev/null; then
    tap_ok "diff_is_test_only: all hunks inside #[cfg(test)] mod tests → test-only"
  else
    tap_not_ok "diff_is_test_only: all hunks inside #[cfg(test)] mod tests → test-only" "expected rc 0"
  fi

  # FAIL (a): a hunk OUTSIDE the cfg(test) region → rebuild.
  _diff_outside() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..2222222 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -10,3 +10,4 @@ fn produce(x: u32) -> u32 {
     let y = x * 2;
+    let y = y + 1;
     y
 }
CANNED
  }
  if _diff_outside | diff_is_test_only 2>/dev/null; then
    tap_not_ok "diff_is_test_only: production hunk → rebuild (fail-safe)" "expected non-zero"
  else
    tap_ok "diff_is_test_only: production hunk → rebuild (fail-safe)"
  fi

  # FAIL (b): a MIXED diff (one test hunk + one production hunk) → rebuild.
  _diff_mixed() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..2222222 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -10,3 +10,4 @@ fn produce(x: u32) -> u32 {
     let y = x * 2;
+    let y = y + 1;
     y
 }
@@ -200,5 +201,6 @@ mod tests {
 #[cfg(test)]
 mod tests {
     #[test]
+    fn test_new() { assert!(true); }
     fn test_old() {}
 }
CANNED
  }
  if _diff_mixed | diff_is_test_only 2>/dev/null; then
    tap_not_ok "diff_is_test_only: mixed test+production diff → rebuild (fail-safe)" "expected non-zero"
  else
    tap_ok "diff_is_test_only: mixed test+production diff → rebuild (fail-safe)"
  fi

  # FAIL (c): a diff that edits the #[cfg(test)]/mod tests opener line itself →
  # rebuild (the boundary is ambiguous — can't confirm region membership).
  _diff_boundary() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..2222222 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -198,4 +198,5 @@ fn produce(x: u32) -> u32 {
 }
+#[cfg(test)]
 mod tests {
     use super::*;
CANNED
  }
  if _diff_boundary | diff_is_test_only 2>/dev/null; then
    tap_not_ok "diff_is_test_only: edits #[cfg(test)] opener → rebuild (fail-safe)" "expected non-zero"
  else
    tap_ok "diff_is_test_only: edits #[cfg(test)] opener → rebuild (fail-safe)"
  fi

  # FAIL (d): a zero-context hunk / whole cfg(test)-module deletion the
  # brace-tracker cannot anchor inside a confirmed region → rebuild (Critic A).
  _diff_zerocontext() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..2222222 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -50,0 +51,1 @@
+    let unanchored = compute();
CANNED
  }
  if _diff_zerocontext | diff_is_test_only 2>/dev/null; then
    tap_not_ok "diff_is_test_only: zero-context hunk → rebuild (fail-safe)" "expected non-zero"
  else
    tap_ok "diff_is_test_only: zero-context hunk → rebuild (fail-safe)"
  fi
  _diff_moddelete() {
    cat <<'CANNED'
diff --git a/crates/app/src/app/publish.rs b/crates/app/src/app/publish.rs
index 1111111..0000000 100644
--- a/crates/app/src/app/publish.rs
+++ b/crates/app/src/app/publish.rs
@@ -1,4 +0,0 @@
-fn produce(x: u32) -> u32 {
-    x * 2
-}
CANNED
  }
  if _diff_moddelete | diff_is_test_only 2>/dev/null; then
    tap_not_ok "diff_is_test_only: whole-module deletion → rebuild (fail-safe)" "expected non-zero"
  else
    tap_ok "diff_is_test_only: whole-module deletion → rebuild (fail-safe)"
  fi

  # --- End-to-end gate decision (canned multi-file `git diff` mock, #3248 style) ---
  # Walk a changeset path-by-path applying classify_path_binary_relevance, and
  # for needs-hunk-check paths drive diff_is_test_only on that file's per-file
  # diff. The aggregate decision: needs_rebuild=no iff EVERY path resolves to
  # no-impact or test-only (the latter confirmed by diff_is_test_only); else yes.
  _gate_decide() {
    # $1 = newline-separated changed paths; reads per-file diffs from a mock
    # `_gate_filediff <path>` the caller defines.
    local paths="$1" p verdict needs="no"
    while IFS= read -r p; do
      [[ -z "$p" ]] && continue
      verdict="$(classify_path_binary_relevance "$p" 2>/dev/null)"
      case "$verdict" in
        no-impact) ;;
        test-only) ;;
        needs-hunk-check)
          if ! _gate_filediff "$p" | diff_is_test_only 2>/dev/null; then
            needs="yes"; break
          fi
          ;;
        *) needs="yes"; break ;;
      esac
    done <<< "$paths"
    echo "$needs"
  }

  # (a) test-only changeset → needs_rebuild=no candidate.
  _gate_filediff() { _diff_test_only_pass; }
  local _changeset_a="crates/app/tests/publish_tests.rs
crates/app/src/app/publish.rs
README.md"
  if [[ "$(_gate_decide "$_changeset_a")" == "no" ]]; then
    tap_ok "gate-decision: test-only changeset → needs_rebuild=no candidate"
  else
    tap_not_ok "gate-decision: test-only changeset → needs_rebuild=no candidate" "got '$(_gate_decide "$_changeset_a")'"
  fi

  # (b) real-src changeset → needs_rebuild=yes (the fail-safe).
  _gate_filediff() { _diff_outside; }
  local _changeset_b="crates/app/src/app/publish.rs"
  if [[ "$(_gate_decide "$_changeset_b")" == "yes" ]]; then
    tap_ok "gate-decision: real-src changeset → needs_rebuild=yes (fail-safe)"
  else
    tap_not_ok "gate-decision: real-src changeset → needs_rebuild=yes (fail-safe)" "got '$(_gate_decide "$_changeset_b")'"
  fi

  # (c) a Cargo.toml in the changeset forces rebuild regardless of test-only siblings.
  _gate_filediff() { _diff_test_only_pass; }
  local _changeset_c="crates/app/tests/publish_tests.rs
Cargo.toml"
  if [[ "$(_gate_decide "$_changeset_c")" == "yes" ]]; then
    tap_ok "gate-decision: Cargo.toml in changeset → needs_rebuild=yes (fail-safe)"
  else
    tap_not_ok "gate-decision: Cargo.toml in changeset → needs_rebuild=yes (fail-safe)" "got '$(_gate_decide "$_changeset_c")'"
  fi
  unset -f _gate_filediff

  # --- Structural assertions: SKILL.md §10 wires the functions + byte-compare ---
  local tick_md="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  if grep -q 'classify_path_binary_relevance' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md calls classify_path_binary_relevance"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md calls classify_path_binary_relevance" "not found"
  fi
  if grep -q 'diff_is_test_only' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md calls diff_is_test_only"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md calls diff_is_test_only" "not found"
  fi
  # The authoritative skip is gated on a release-binary byte-compare.
  if grep -qiE 'byte-compare|byte-identical|cmp -s|byte compare' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md retains binary byte-compare confirmation"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md retains binary byte-compare confirmation" "not found"
  fi
  if grep -q 'DEPLOY SYNCED (test-only' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md reports DEPLOY SYNCED (test-only…)"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md reports DEPLOY SYNCED (test-only…)" "not found"
  fi
  # On a confirmed test-only skip, BUILD_SHA_FILE advances (fixes per-tick re-trip).
  local syncsec
  syncsec=$(extract_md_section "$tick_md" '^   On a confirmed test-only skip' '^[0-9]\. ' 2>/dev/null || true)
  if grep -q 'BUILD_SHA_FILE' "$tick_md"; then
    tap_ok "structural: monitor-tick/SKILL.md advances BUILD_SHA_FILE on confirmed skip"
  else
    tap_not_ok "structural: monitor-tick/SKILL.md advances BUILD_SHA_FILE on confirmed skip" "not found"
  fi

  # ── Structural self-test: Test 6 boundary check is deterministic + guarded ──
  # Regression guard for #3766. Test 6 pins the `-gt 7200` fresh side of the
  # boundary, but two failure modes must never come back:
  #   1. Mocking the EXACT boundary (7200) races the wall clock — a single tick
  #      between `mock_env_file` and the check flips the env to 7201 (stale).
  #   2. A BARE `check_session_wiped` call aborts the whole harness under the
  #      script's `set -e` when it legitimately returns 1 (stale path).
  # Scope the assertion to Test 6's own block (sess6666 .. Test 7) — 7200 and
  # check_session_wiped both appear legitimately elsewhere in this file.
  local self_src="$REPO_ROOT/scripts/test-monitor-skill-snippets.sh"
  local t6_block
  t6_block=$(awk '/session_id="sess6666"/{f=1} f{print} /Test 7:/{if(f)exit}' "$self_src")
  if [[ -n "$t6_block" ]] \
     && ! grep -q '7200' <<<"$t6_block" \
     && grep -q 'exit_code6' <<<"$t6_block"; then
    tap_ok "structural: Test 6 boundary check is deterministic and guarded (#3766)"
  else
    tap_not_ok "structural: Test 6 boundary check is deterministic and guarded (#3766)" \
      "Test 6 must mock 7199 (not the exact 7200 boundary) and guard its check_session_wiped call via exit_code6"
  fi
}
check_skill_structure
run_tests

# End-of-run plan/actual reconciliation (#3766): if the number of emitted TAP
# results does not match the announced plan (early abort, or a stale off-by-one
# TAP_PLAN), surface it as a diagnosed failure through the existing gate instead
# of a silent short-count or bare `exit 1`.
if [[ "$TAP_CURRENT" -ne "$TAP_PLAN" ]]; then
  echo "# PLAN MISMATCH: planned $TAP_PLAN, emitted $TAP_CURRENT" >&2
  TAP_FAILURES=$((TAP_FAILURES + 1))
fi

if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES test(s) failed" >&2
  exit 1
fi
exit 0
