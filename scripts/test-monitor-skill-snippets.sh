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
TEST_ROOT="$REPO_ROOT/data/test-monitor-snippets"

# ── Source the shared library (single source of truth) ────────────────────────
source "$SCRIPT_DIR/lib/monitor-decisions.sh"

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
  rm -rf "$TEST_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
cleanup  # ensure fresh state
mkdir -p "$TEST_ROOT"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=53
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

  if [[ "$drift" == "true" && "$STRICT" == "true" ]]; then
    echo "FATAL: Structural drift detected in --strict mode." >&2
    exit 1
  fi
}


# ── Mock Helpers ─────────────────────────────────────────────────────────────

mock_proc_entry() {
  # Create a mock /proc/<pid> with exe symlink
  local proc_root="$1" pid="$2" exe_target="$3"
  mkdir -p "$proc_root/$pid"
  ln -sf "$exe_target" "$proc_root/$pid/exe"
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
  mock_proc_entry "$proc" "1001" "$data/$session_id/cargo-target/release/henyey"
  mock_env_file "$data/monitor-loop.env" 100

  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/logs" ]]; then
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
  mock_proc_entry "$proc" "2001" "$data/$session_id/cargo-target/release/henyey (deleted)"
  mock_env_file "$data/monitor-loop.env" 100

  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/metrics" ]]; then
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
  mock_proc_entry "$proc" "3001" "$data/other-session/cargo-target/release/henyey"
  # Create env file so freshness check passes
  mock_env_file "$data/monitor-loop.env" 100

  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" && -d "$data/$session_id/logs" ]]; then
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

  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" && -d "$data/$session_id/cargo-target" ]]; then
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

  # ── Test 6: Process dead + env at boundary (7200s) ─────────────────────
  # Source: scripts/lib/monitor-decisions.sh — -gt 7200 means 7200 passes
  data="$TEST_ROOT/t6/data"
  proc="$TEST_ROOT/t6/proc"
  session_id="sess6666"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7200

  check_session_wiped "$data" "$proc" "$session_id" "$data/monitor-loop.env"
  if [[ "$SESSION_WIPED" == "yes" && -d "$data/$session_id/logs" ]]; then
    tap_ok "session-wipe: env at boundary (7200s passes)"
  else
    tap_not_ok "session-wipe: env at boundary (7200s passes)" "WIPED=$SESSION_WIPED dirs=$(ls "$data/$session_id" 2>/dev/null || echo missing)"
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
  mock_proc_entry "$proc" "9001" "$data/running-sess/cargo-target/release/henyey"
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
  # Cross-validates inline literals against the canonical TOML source.
  # See issues #2399, #2402.

  local tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  local loop_file="$REPO_ROOT/.claude/skills/monitor-loop/SKILL.md"
  local constants_file="$REPO_ROOT/.claude/skills/shared/check-12b-constants.toml"

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

  # Test 41: TOML constants file exists and is parseable (fail closed)
  local streak_val burst_val delta_val snapshot_file mode_val metric_name metric_label
  streak_val=$(grep -oP '^streak\s*=\s*\K\d+' "$constants_file" 2>/dev/null) || streak_val=""
  burst_val=$(grep -oP '^burst\s*=\s*\K\d+' "$constants_file" 2>/dev/null) || burst_val=""
  delta_val=$(grep -oP '^delta\s*=\s*\K\d+' "$constants_file" 2>/dev/null) || delta_val=""
  snapshot_file=$(grep -oP '^file\s*=\s*"\K[^"]+' "$constants_file" 2>/dev/null) || snapshot_file=""
  mode_val=$(grep -oP '^mode\s*=\s*"\K[^"]+' "$constants_file" 2>/dev/null) || mode_val=""
  metric_name=$(grep -oP '^name\s*=\s*"\K[^"]+' "$constants_file" 2>/dev/null) || metric_name=""
  metric_label=$(grep -oP "^label\s*=\s*'\K[^']+" "$constants_file" 2>/dev/null) || metric_label=""

  if [[ -n "$streak_val" && -n "$burst_val" && -n "$delta_val" \
        && -n "$snapshot_file" && -n "$mode_val" \
        && -n "$metric_name" && -n "$metric_label" ]]; then
    tap_ok "check-12b-constants: TOML exists and parseable (streak=$streak_val burst=$burst_val delta=$delta_val)"
  else
    tap_not_ok "check-12b-constants: TOML exists and parseable" \
      "Missing or unparseable: streak=$streak_val burst=$burst_val delta=$delta_val file=$snapshot_file mode=$mode_val metric=$metric_name label=$metric_label"
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

  # Test 50: Reference link to constants file in both SKILL.md files
  if grep -Fq 'check-12b-constants.toml' "$tick_file" \
     && grep -Fq 'check-12b-constants.toml' "$loop_file"; then
    tap_ok "check-12b-constants: reference link in both SKILL.md files"
  else
    tap_not_ok "check-12b-constants: reference link in both SKILL.md files" \
      "Both SKILL.md must contain 'check-12b-constants.toml'"
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
}

# ── Main ─────────────────────────────────────────────────────────────────────
check_skill_structure
run_tests

if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES test(s) failed" >&2
  exit 1
fi
exit 0
