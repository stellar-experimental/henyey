#!/usr/bin/env bash
#
# Smoke-test harness for monitor skill shell snippets.
#
# Validates the decision logic from .claude/skills/monitor-tick/SKILL.md and
# .claude/skills/monitor-loop/SKILL.md using mock filesystems. This is
# SEMANTIC testing of the logic's behavior, not literal shell equivalence —
# some source patterns are approximated by equivalent parameterized logic.
#
# Usage:
#   ./scripts/test-monitor-skill-snippets.sh              # run tests (warn on drift)
#   ./scripts/test-monitor-skill-snippets.sh --strict     # fail on source drift
#   ./scripts/test-monitor-skill-snippets.sh --update-checksums  # print new checksums
#
# Output: TAP (Test Anything Protocol) on stdout, diagnostics on stderr.
# Exit: 0 = all pass, 1 = any fail.
#
# Portability: GNU/Linux only (GNU stat -c, readlink, Bash 4+, symlinks).
#
# Drift detection: Checksums of fenced code blocks in cited skill sections.
# This is a tripwire — it detects textual changes that may invalidate tests,
# not semantic equivalence.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_ROOT="$REPO_ROOT/data/test-monitor-snippets"

# ── Arguments ────────────────────────────────────────────────────────────────
STRICT=false
UPDATE_CHECKSUMS=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict)           STRICT=true; shift ;;
    --update-checksums) UPDATE_CHECKSUMS=true; shift ;;
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
TAP_PLAN=16
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

# ── Drift Detection ─────────────────────────────────────────────────────────
# Expected checksums of fenced code blocks in skill files.
# Update with: ./scripts/test-monitor-skill-snippets.sh --update-checksums
TICK_SESSION_WIPE_CKSUM="9ecf2096243bd256b679ea19966dafb5b6e27881c1a0055031bf194f4ba2b891"
LOOP_ATTACH_CKSUM="c4d145a4cd8454034d9cbc900d16a8bb7b1dfc17da8493b07db3f6165a994504"
LOOP_CLEANUP_CKSUM="64b10e52552c7c4e95e1f49aa6d1edd61b146212f87dfb39accfe08022ac3be4"

extract_fenced_block() {
  # Extract the first fenced bash block from a line range of a file.
  # Handles indented fences (common in markdown lists).
  local file="$1" start="$2" end="$3"
  sed -n "${start},${end}p" "$file" | sed -n '/^ *```bash/,/^ *```$/p' | sed '1d;$d'
}

compute_checksums() {
  local tick_file="$REPO_ROOT/.claude/skills/monitor-tick/SKILL.md"
  local loop_file="$REPO_ROOT/.claude/skills/monitor-loop/SKILL.md"

  local tick_hash loop_attach_hash loop_cleanup_hash
  tick_hash=$(extract_fenced_block "$tick_file" 50 83 | sha256sum | cut -d' ' -f1)
  loop_attach_hash=$(extract_fenced_block "$loop_file" 466 486 | sha256sum | cut -d' ' -f1)
  loop_cleanup_hash=$(extract_fenced_block "$loop_file" 833 860 | sha256sum | cut -d' ' -f1)

  if [[ "$UPDATE_CHECKSUMS" == "true" ]]; then
    echo "# Updated checksums — paste into script:"
    echo "TICK_SESSION_WIPE_CKSUM=\"$tick_hash\""
    echo "LOOP_ATTACH_CKSUM=\"$loop_attach_hash\""
    echo "LOOP_CLEANUP_CKSUM=\"$loop_cleanup_hash\""
    exit 0
  fi

  local drift=false
  if [[ "$tick_hash" != "$TICK_SESSION_WIPE_CKSUM" ]]; then
    echo "WARNING: monitor-tick:50-83 code block has changed (drift detected)" >&2
    drift=true
  fi
  if [[ "$loop_attach_hash" != "$LOOP_ATTACH_CKSUM" ]]; then
    echo "WARNING: monitor-loop:466-486 code block has changed (drift detected)" >&2
    drift=true
  fi
  if [[ "$loop_cleanup_hash" != "$LOOP_CLEANUP_CKSUM" ]]; then
    echo "WARNING: monitor-loop:833-860 code block has changed (drift detected)" >&2
    drift=true
  fi

  if [[ "$drift" == "true" && "$STRICT" == "true" ]]; then
    echo "FATAL: Source drift detected in --strict mode. Run --update-checksums." >&2
    exit 1
  fi
}

# ── Decision Functions (parameterized mirrors of skill logic) ────────────────

# Mirrors monitor-tick/SKILL.md:50-83
# Sets: SESSION_WIPED, SESSION_WIPED_PROCESS_ALIVE
# Side effect: creates recovery dirs if session dir missing
check_session_wiped() {
  local data_root="$1" proc_root="$2" session_id="$3"
  SESSION_WIPED=no
  SESSION_WIPED_PROCESS_ALIVE=no

  if [[ ! -d "$data_root/$session_id" ]]; then
    local expected_binary="$data_root/$session_id/cargo-target/release/henyey"
    local our_pid=""

    for p in "$proc_root"/[0-9]*; do
      [[ -d "$p" ]] || continue
      local exe
      exe=$(readlink "$p/exe" 2>/dev/null || true)
      if [[ "$exe" == "$expected_binary" || "$exe" == "$expected_binary (deleted)" ]]; then
        our_pid=$(basename "$p")
        break
      fi
    done

    if [[ -n "$our_pid" ]]; then
      SESSION_WIPED=yes
      SESSION_WIPED_PROCESS_ALIVE=yes
    else
      SESSION_WIPED=yes
      SESSION_WIPED_PROCESS_ALIVE=no
    fi

    # Recreate minimal session structure (unconditional when dir missing).
    mkdir -p "$data_root/$session_id"/{logs,cache,cargo-target,metrics}
  fi
}

# Mirrors monitor-tick/SKILL.md:71-76
# Returns: 0 if fresh, 1 if stale (age > 7200s or file missing)
check_env_freshness() {
  local env_file="$1"
  local env_mtime env_age
  env_mtime=$(stat -c %Y "$env_file" 2>/dev/null || echo 0)
  env_age=$(( $(date +%s) - env_mtime ))
  if [[ "$env_age" -gt 7200 ]]; then
    echo "ERROR: env stale (${env_age}s > 2h)" >&2
    return 1
  fi
  return 0
}

# Mirrors monitor-loop/SKILL.md:466-485
# Prints recovered session-id. Creates dirs + touches .alive only for (deleted) paths.
# Returns 1 on malformed input.
recover_session_from_stdout() {
  local data_root="$1" proc_stdout="$2"

  # Check for (deleted) suffix
  if echo "$proc_stdout" | grep -q '(deleted)'; then
    local original_path
    original_path=$(echo "$proc_stdout" | sed 's/ (deleted)$//')
    local session_id
    session_id=$(echo "$original_path" | sed -n 's|.*/data/\([^/]*\)/.*|\1|p')
    if [[ -z "$session_id" ]]; then
      return 1
    fi
    mkdir -p "$data_root/$session_id"/{logs,cache,cargo-target,metrics}
    touch "$data_root/$session_id/.alive"
    echo "$session_id"
    return 0
  fi

  # Normal path — extract session-id from stdout path
  local session_id
  session_id=$(echo "$proc_stdout" | sed -n 's|.*/data/\([^/]*\)/.*|\1|p')
  if [[ -z "$session_id" ]]; then
    return 1
  fi
  echo "$session_id"
  return 0
}

# Mirrors monitor-loop/SKILL.md:833-860
# Prints "SKIP <reason>" or "PASS".
cleanup_guard() {
  local data_root="$1" proc_root="$2" candidate="$3" active_session="$4" alive_threshold="$5"

  # Layer 1: active session
  if [[ "$candidate" == "$active_session" ]]; then
    echo "SKIP active per monitor-loop.env"
    return 0
  fi

  # Layer 2: .alive freshness
  local alive_file="$data_root/$candidate/.alive"
  if [[ -f "$alive_file" ]]; then
    local alive_age
    alive_age=$(( $(date +%s) - $(stat -c %Y "$alive_file") ))
    if [[ "$alive_age" -lt "$alive_threshold" ]]; then
      echo "SKIP .alive touched ${alive_age}s ago (< ${alive_threshold}s)"
      return 0
    fi
  fi

  # Layer 3: running process references this session
  if find "$proc_root" -maxdepth 2 -name exe -exec readlink {} \; 2>/dev/null | grep -q "/data/$candidate/"; then
    echo "SKIP running process uses this session"
    return 0
  fi

  echo "PASS"
  return 0
}

# Mirrors monitor-tick/SKILL.md:119-127
check_mainnet_wiped() {
  local data_root="$1" session_wiped="$2"
  MAINNET_WIPED=no
  if [[ "$session_wiped" == "no" ]] && [[ ! -d "$data_root/mainnet" ]]; then
    MAINNET_WIPED=yes
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
  # Source: .claude/skills/monitor-tick/SKILL.md:50-83
  data="$TEST_ROOT/t1/data"
  proc="$TEST_ROOT/t1/proc"
  session_id="sess1111"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "1001" "$data/$session_id/cargo-target/release/henyey"

  check_session_wiped "$data" "$proc" "$session_id"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/logs" ]]; then
    tap_ok "session-wipe: process alive (exact binary)"
  else
    tap_not_ok "session-wipe: process alive (exact binary)" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 2: Session dir missing + process alive (deleted binary) ────────
  # Source: .claude/skills/monitor-tick/SKILL.md:57-58,81-82
  data="$TEST_ROOT/t2/data"
  proc="$TEST_ROOT/t2/proc"
  session_id="sess2222"
  mkdir -p "$data" "$proc"
  mock_proc_entry "$proc" "2001" "$data/$session_id/cargo-target/release/henyey (deleted)"

  check_session_wiped "$data" "$proc" "$session_id"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "yes" && -d "$data/$session_id/metrics" ]]; then
    tap_ok "session-wipe: process alive (deleted binary)"
  else
    tap_not_ok "session-wipe: process alive (deleted binary)" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 3: Different binary path (not our session) ─────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:54-62
  data="$TEST_ROOT/t3/data"
  proc="$TEST_ROOT/t3/proc"
  session_id="sess3333"
  mkdir -p "$data" "$proc"
  # Process running a DIFFERENT session's binary
  mock_proc_entry "$proc" "3001" "$data/other-session/cargo-target/release/henyey"
  # Create env file so freshness check passes
  mock_env_file "$data/monitor-loop.env" 100

  check_session_wiped "$data" "$proc" "$session_id"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" ]]; then
    # Verify it fell through to dead-process path (env check needed)
    if check_env_freshness "$data/monitor-loop.env"; then
      tap_ok "session-wipe: different binary not matched"
    else
      tap_not_ok "session-wipe: different binary not matched" "env check unexpectedly failed"
    fi
  else
    tap_not_ok "session-wipe: different binary not matched" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 4: Process dead + env fresh (100s) ────────────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:50-83
  data="$TEST_ROOT/t4/data"
  proc="$TEST_ROOT/t4/proc"
  session_id="sess4444"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 100

  check_session_wiped "$data" "$proc" "$session_id"
  if [[ "$SESSION_WIPED" == "yes" && "$SESSION_WIPED_PROCESS_ALIVE" == "no" && -d "$data/$session_id/cargo-target" ]]; then
    if check_env_freshness "$data/monitor-loop.env"; then
      tap_ok "session-wipe: dead process, env fresh"
    else
      tap_not_ok "session-wipe: dead process, env fresh" "env check failed"
    fi
  else
    tap_not_ok "session-wipe: dead process, env fresh" "WIPED=$SESSION_WIPED ALIVE=$SESSION_WIPED_PROCESS_ALIVE"
  fi

  # ── Test 5: Process dead + env stale (7201s) ───────────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:71-76
  data="$TEST_ROOT/t5/data"
  proc="$TEST_ROOT/t5/proc"
  session_id="sess5555"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7201

  check_session_wiped "$data" "$proc" "$session_id"
  local exit_code=0
  check_env_freshness "$data/monitor-loop.env" 2>/dev/null || exit_code=$?
  if [[ "$exit_code" -eq 1 ]]; then
    tap_ok "session-wipe: dead process, env stale (7201s)"
  else
    tap_not_ok "session-wipe: dead process, env stale (7201s)" "expected return 1, got $exit_code"
  fi

  # ── Test 6: Process dead + env at boundary (7200s) ─────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:73 (-gt 7200 means 7200 passes)
  data="$TEST_ROOT/t6/data"
  proc="$TEST_ROOT/t6/proc"
  session_id="sess6666"
  mkdir -p "$data" "$proc"
  mock_env_file "$data/monitor-loop.env" 7200

  check_session_wiped "$data" "$proc" "$session_id"
  if check_env_freshness "$data/monitor-loop.env"; then
    if [[ -d "$data/$session_id/logs" ]]; then
      tap_ok "session-wipe: env at boundary (7200s passes)"
    else
      tap_not_ok "session-wipe: env at boundary (7200s passes)" "recovery dirs not created"
    fi
  else
    tap_not_ok "session-wipe: env at boundary (7200s passes)" "7200 should not be stale (-gt, not -ge)"
  fi

  # ── Test 7: Process dead + env file missing ────────────────────────────
  # Source: .claude/skills/monitor-tick/SKILL.md:71 (stat fails → echo 0 → epoch age)
  data="$TEST_ROOT/t7/data"
  mkdir -p "$data"
  # No env file created
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
  # Source: .claude/skills/monitor-tick/SKILL.md:119-127
  data="$TEST_ROOT/t16/data"
  mkdir -p "$data"
  # No mainnet dir
  check_mainnet_wiped "$data" "no"
  if [[ "$MAINNET_WIPED" == "yes" ]]; then
    # Also verify it does NOT trigger when SESSION_WIPED=yes
    check_mainnet_wiped "$data" "yes"
    if [[ "$MAINNET_WIPED" == "no" ]]; then
      tap_ok "mainnet-wiped: detected when dir absent and session not wiped"
    else
      tap_not_ok "mainnet-wiped: detected when dir absent and session not wiped" "should not fire when SESSION_WIPED=yes"
    fi
  else
    tap_not_ok "mainnet-wiped: detected when dir absent and session not wiped" "MAINNET_WIPED=$MAINNET_WIPED"
  fi
}

# ── Main ─────────────────────────────────────────────────────────────────────
compute_checksums
run_tests

if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES test(s) failed" >&2
  exit 1
fi
exit 0
