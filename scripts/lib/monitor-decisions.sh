#!/usr/bin/env bash
#
# Shared decision logic for monitor-tick and monitor-loop skills.
#
# Requires: Bash 4+, GNU/Linux (stat -c, readlink, find, grep, sed, date).
# Portability: GNU/Linux only (not POSIX).
#
# Does NOT set shell options (set -e, -u, etc.) — callers control strictness.
# Idempotent: safe to source multiple times.
#

[[ -n "${_MONITOR_DECISIONS_LOADED:-}" ]] && return 0
_MONITOR_DECISIONS_LOADED=1

# ─────────────────────────────────────────────────────────────────────────────
# check_session_wiped DATA_ROOT PROC_ROOT SESSION_ID ENV_FILE
#
# Check whether the session directory was wiped out-of-band.
#
# Sets globals:
#   SESSION_WIPED              "yes" | "no"
#   SESSION_WIPED_PROCESS_ALIVE  "yes" | "no" (meaningful only when SESSION_WIPED=yes)
#
# Returns:
#   0 — not wiped, OR wiped-and-recoverable (dirs created)
#   1 — wiped, no process alive, env stale (dirs NOT created)
#
# Stderr on return 1:
#   "ERROR: session <SESSION_ID> absent, no process, env stale (<N>s > 2h). Run /monitor-loop."
#
# Call-site pattern in skills:
#   check_session_wiped "$HOME/data" "/proc" "$MONITOR_SESSION_ID" \
#     "$HOME/data/monitor-loop.env" || exit 1
# ─────────────────────────────────────────────────────────────────────────────
check_session_wiped() {
  local data_root="$1" proc_root="$2" session_id="$3" env_file="$4"
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
      # No matching process — check env freshness before recovery.
      local env_mtime env_age
      env_mtime=$(stat -c %Y "$env_file" 2>/dev/null || echo 0)
      env_age=$(( $(date +%s) - env_mtime ))
      if [[ "$env_age" -gt 7200 ]]; then
        echo "ERROR: session $session_id absent, no process, env stale (${env_age}s > 2h). Run /monitor-loop." >&2
        SESSION_WIPED=yes
        SESSION_WIPED_PROCESS_ALIVE=no
        return 1
      fi
      SESSION_WIPED=yes
      SESSION_WIPED_PROCESS_ALIVE=no
    fi

    # Recreate minimal session structure (only reached if recoverable).
    mkdir -p "$data_root/$session_id"/{logs,cache,cargo-target,metrics}
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# check_env_freshness ENV_FILE
#
# Standalone env freshness check.
#
# Returns: 0 (fresh, ≤7200s) or 1 (stale, >7200s or file missing → epoch age)
# Stderr on stale: "ERROR: env stale (<N>s > 2h)"
# ─────────────────────────────────────────────────────────────────────────────
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

# ─────────────────────────────────────────────────────────────────────────────
# recover_session_from_stdout DATA_ROOT PROC_STDOUT_PATH
#
# Recover session-id from a process's stdout fd symlink target.
#
# Accepted input:
#   Any path containing "/data/<session-id>/..." OR same with " (deleted)".
#   Session-id is extracted via the /data/<segment>/ pattern.
#
# Stdout: recovered session-id (one line)
# Stderr on (deleted):
#   "WARNING: henyey stdout target deleted (out-of-band wipe). Process still alive."
#
# Side effects:
#   - (deleted) paths: creates DATA_ROOT/<session-id>/{logs,cache,cargo-target,metrics}
#     and touches DATA_ROOT/<session-id>/.alive
#   - Normal paths: NO side effects
#
# Returns: 0 (success) or 1 (malformed — no extractable session-id)
# ─────────────────────────────────────────────────────────────────────────────
recover_session_from_stdout() {
  local data_root="$1" proc_stdout="$2"

  if echo "$proc_stdout" | grep -q '(deleted)'; then
    echo "WARNING: henyey stdout target deleted (out-of-band wipe). Process still alive." >&2
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

  # Normal path — extract session-id
  local session_id
  session_id=$(echo "$proc_stdout" | sed -n 's|.*/data/\([^/]*\)/.*|\1|p')
  if [[ -z "$session_id" ]]; then
    return 1
  fi
  echo "$session_id"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# cleanup_guard DATA_ROOT PROC_ROOT CANDIDATE ACTIVE_SESSION ALIVE_THRESHOLD
#
# Three-layer guard: determines if a session dir is safe to delete.
#
# Stdout (exactly one line):
#   "SKIP active per monitor-loop.env"
#   "SKIP .alive touched <N>s ago (< <T>s)"
#   "SKIP running process uses this session"
#   "PASS"
#
# Returns: always 0
# ─────────────────────────────────────────────────────────────────────────────
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
  if find "$proc_root" -maxdepth 2 -name exe -exec readlink {} \; 2>/dev/null | grep -q "$data_root/$candidate/"; then
    echo "SKIP running process uses this session"
    return 0
  fi

  echo "PASS"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# check_mainnet_wiped DATA_ROOT
#
# Sets global: MAINNET_WIPED "yes"|"no"
# Returns: always 0
# ─────────────────────────────────────────────────────────────────────────────
check_mainnet_wiped() {
  local data_root="$1"
  MAINNET_WIPED=no
  if [[ ! -d "$data_root/mainnet" ]]; then
    MAINNET_WIPED=yes
  fi
}
