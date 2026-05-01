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
TAP_PLAN=19
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

  # monitor-loop must source the library and call its functions
  if ! grep -q 'source.*scripts/lib/monitor-decisions.sh' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not source scripts/lib/monitor-decisions.sh" >&2
    drift=true
  fi
  if ! grep -q 'recover_session_from_stdout' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not call recover_session_from_stdout" >&2
    drift=true
  fi
  if ! grep -q 'cleanup_guard' "$loop_file"; then
    echo "WARNING: monitor-loop/SKILL.md does not call cleanup_guard" >&2
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

  # ── Test 19: Session dir exists → not wiped (no-op fall-through) ───────
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
}

# ── Main ─────────────────────────────────────────────────────────────────────
check_skill_structure
run_tests

if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES test(s) failed" >&2
  exit 1
fi
exit 0
