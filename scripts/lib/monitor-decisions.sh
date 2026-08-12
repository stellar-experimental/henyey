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
# _enumerate_henyey_processes DATA_ROOT PROC_ROOT
#
# Enumerate all live henyey `run` processes under DATA_ROOT.
# Validates both:
#   - exe symlink: DATA_ROOT/<session-id>/cargo-target/release/henyey[(deleted)]
#   - cmdline: contains `run` as a standalone argv element
#
# Each line of output: PID SESSION_ID
# Silently skips entries with unreadable/empty cmdline.
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
_enumerate_henyey_processes() {
  local data_root="$1" proc_root="$2"
  local suffix="/cargo-target/release/henyey"

  for p in "$proc_root"/[0-9]*; do
    [[ -d "$p" ]] || continue
    local exe
    exe=$(readlink "$p/exe" 2>/dev/null || true)
    [[ -z "$exe" ]] && continue
    # Strip " (deleted)" suffix
    local clean_exe="${exe%' (deleted)'}"
    # Prefix check: must be under data_root
    [[ "$clean_exe" == "$data_root"/* ]] || continue
    local after_root="${clean_exe#"$data_root"/}"
    # Suffix check: must end with /cargo-target/release/henyey
    [[ "$after_root" == *"$suffix" ]] || continue
    # Extract session_id (between data_root/ and /cargo-target/...)
    local session_id="${after_root%"$suffix"}"
    # session_id must be a single path segment (no slashes)
    [[ "$session_id" == */* ]] && continue
    [[ -z "$session_id" ]] && continue

    # Verify cmdline contains `run` subcommand (exact argv match).
    local has_run=false
    if [[ -r "$p/cmdline" ]]; then
      while IFS= read -r -d '' arg; do
        if [[ "$arg" == "run" ]]; then
          has_run=true
          break
        fi
      done < "$p/cmdline"
    fi
    if $has_run; then
      printf '%s %s\n' "$(basename "$p")" "$session_id"
    fi
  done
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# _find_session_process DATA_ROOT PROC_ROOT SESSION_ID
#
# Find a henyey `run` process for a specific session.
# Thin wrapper around _enumerate_henyey_processes filtered by session ID.
#
# Stdout: PID of first matching process, or empty string if none found.
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
_find_session_process() {
  local data_root="$1" proc_root="$2" session_id="$3"
  _enumerate_henyey_processes "$data_root" "$proc_root" \
    | awk -v sid="$session_id" '$2 == sid { print $1; exit }'
}

# ─────────────────────────────────────────────────────────────────────────────
# _parse_cmdline_config CMDLINE_FILE
#
# Extract config file path from a NUL-separated /proc/<pid>/cmdline.
# Supports -c, --config, --conf (alias), --config=value, --conf=value.
#
# Stdout: config path (one line) or empty.
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
_parse_cmdline_config() {
  local cmdline_file="$1"
  local prev=""
  while IFS= read -r -d '' arg; do
    if [[ "$prev" == "-c" || "$prev" == "--config" || "$prev" == "--conf" ]]; then
      echo "$arg"
      return 0
    fi
    # Handle --config=value and --conf=value forms
    case "$arg" in
      --config=*) echo "${arg#--config=}"; return 0 ;;
      --conf=*)   echo "${arg#--conf=}"; return 0 ;;
    esac
    prev="$arg"
  done < "$cmdline_file" 2>/dev/null
  return 0
}

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
    local our_pid
    our_pid=$(_find_session_process "$data_root" "$proc_root" "$session_id")

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
# check_long_stale_session DATA_ROOT PROC_ROOT SESSION_ID ENV_FILE
#
# Detect "session dir exists but is long-abandoned" state and refuse
# auto-relaunch. Sibling to check_session_wiped (which handles missing dirs).
#
# Primary signal: .alive mtime (touched every tick). Fallback: env file mtime.
# Process-alive check overrides staleness markers.
#
# Sets globals:
#   LONG_STALE_SESSION  "yes" | "no"
#
# Returns:
#   0 — session is not long-stale (process alive, or markers recent enough)
#   1 — session is long-stale; caller should exit without relaunching
#
# Stderr on return 1:
#   "ERROR: session <ID> long-stale (...). Refusing auto-relaunch ..."
#
# Call-site pattern:
#   check_long_stale_session "$HOME/data" "/proc" "$MONITOR_SESSION_ID" \
#     "$HOME/data/monitor-loop.env" || exit 1
# ─────────────────────────────────────────────────────────────────────────────
check_long_stale_session() {
  local data_root="$1" proc_root="$2" session_id="$3" env_file="$4"
  LONG_STALE_SESSION=no

  # Not our concern if session dir is missing (check_session_wiped handles that).
  if [[ ! -d "$data_root/$session_id" ]]; then
    return 0
  fi

  # Check .alive freshness (primary signal — touched every tick).
  local alive_file="$data_root/$session_id/.alive"
  local alive_age=""
  if [[ -f "$alive_file" ]]; then
    local alive_mtime
    alive_mtime=$(stat -c %Y "$alive_file" 2>/dev/null || echo 0)
    alive_age=$(( $(date +%s) - alive_mtime ))
    if [[ "$alive_age" -le 21600 ]]; then
      return 0  # Recent tick activity (≤ 6h).
    fi
  fi

  # .alive missing or stale — check env freshness as fallback.
  local env_mtime env_age
  env_mtime=$(stat -c %Y "$env_file" 2>/dev/null || echo 0)
  env_age=$(( $(date +%s) - env_mtime ))
  if [[ "$env_age" -le 86400 ]]; then
    return 0  # Env is recent enough (≤ 24h).
  fi

  # Both markers stale — check if process is still alive (overrides staleness).
  local our_pid
  our_pid=$(_find_session_process "$data_root" "$proc_root" "$session_id")
  if [[ -n "$our_pid" ]]; then
    return 0  # Process alive; session is active despite stale markers.
  fi

  # All conditions met: no process, .alive stale/missing, env stale.
  LONG_STALE_SESSION=yes
  local alive_msg
  if [[ -n "$alive_age" ]]; then
    alive_msg=".alive age ${alive_age}s > 6h"
  else
    alive_msg=".alive missing"
  fi
  echo "ERROR: session $session_id long-stale (no process, $alive_msg, env age ${env_age}s > 24h). Refusing auto-relaunch — run /monitor-loop to reset." >&2
  return 1
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

# ─────────────────────────────────────────────────────────────────────────────
# detect_crash_state LOGS_DIR [NOW_EPOCH]
#
# Analyzes crashed log files to determine crash state for the (3a) wipe trigger.
#
# Arguments:
#   LOGS_DIR   - Directory containing monitor.log.crashed-* files
#   NOW_EPOCH  - Optional: current epoch seconds (default: $(date +%s)).
#                Injecting this makes the 30-minute window deterministically
#                testable without real-time waits.
#
# Sets globals:
#   CRASH_RECENT_COUNT  - Number of crashed files modified within last 30 min
#   CRASH_LATEST_FILE   - Path to most recent crashed file (empty if none)
#   CRASH_HASH_MISMATCH - "yes" | "no" — latest crash indicates fatal state corruption
#
# Behavior:
#   1. Lists all monitor.log.crashed-* files in LOGS_DIR
#   2. For each: stat -c %Y for mtime epoch; skip files where stat fails
#      (race: file deleted between glob expansion and stat)
#   3. Filter to files with mtime > (NOW_EPOCH - 1800)  [strict >]
#   4. Sort: mtime descending, ties broken by path descending (lexicographic)
#   5. Grep newest for fatal wipe signature (text, JSON, and legacy prose):
#      - Text:   fatal_wipe_required=true  or  fatal_wipe_required: true
#      - JSON:   "fatal_wipe_required":true
#      - Prose:  "State wipe required before restart"
#      Contract: trigger_fatal_shutdown() in crates/app/src/app/lifecycle.rs
#
# Edge cases:
#   - Missing/empty LOGS_DIR: all outputs are 0/""/no (no error)
#   - All files older than 30 min: CRASH_RECENT_COUNT=0, CRASH_LATEST_FILE=""
#   - stat race (file vanishes): that file is silently skipped
#
# Returns: always 0
# ─────────────────────────────────────────────────────────────────────────────
detect_crash_state() {
  local logs_dir="$1"
  local now_epoch="${2:-$(date +%s)}"
  local boundary=$((now_epoch - 1800))

  CRASH_RECENT_COUNT=0
  CRASH_LATEST_FILE=""
  CRASH_HASH_MISMATCH="no"

  [[ -d "$logs_dir" ]] || return 0

  local files_with_mtime=""
  local f mtime
  for f in "$logs_dir"/monitor.log.crashed-*; do
    [[ -f "$f" ]] || continue
    mtime=$(stat -c %Y "$f" 2>/dev/null) || continue
    if [[ "$mtime" -gt "$boundary" ]]; then
      files_with_mtime+="$mtime $f"$'\n'
    fi
  done

  [[ -z "$files_with_mtime" ]] && return 0

  # Sort: mtime descending (numeric), ties broken by path descending
  local sorted
  sorted=$(printf '%s' "$files_with_mtime" | sort -t' ' -k1,1rn -k2,2r)

  CRASH_RECENT_COUNT=$(printf '%s\n' "$sorted" | grep -c .)
  CRASH_LATEST_FILE=$(printf '%s\n' "$sorted" | head -1 | cut -d' ' -f2-)

  if [[ -n "$CRASH_LATEST_FILE" ]] && \
     grep -qE 'fatal_wipe_required\s*[=:]\s*true|"fatal_wipe_required"\s*:\s*true|State wipe required before restart' \
       "$CRASH_LATEST_FILE" 2>/dev/null; then
    CRASH_HASH_MISMATCH="yes"
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# has_fatal_wipe_evidence LOGS_DIR LOG_FILE
#
# Checks for fatal_wipe_required=true in crashed rotations OR the active log.
# Unlike detect_crash_state() which is windowed to 30 min, this has no time
# limit — it answers "has this session EVER had a fatal corruption signal?"
#
# Arguments:
#   LOGS_DIR - Directory containing monitor.log.crashed-* files
#   LOG_FILE - Path to active monitor.log
#
# Sets globals:
#   FATAL_WIPE_EVIDENCE - "yes" | "no"
#   FATAL_WIPE_SOURCE   - "crashed:<filename>" | "active" | ""
#
# Detection pattern (same as detect_crash_state):
#   'fatal_wipe_required\s*[=:]\s*true|"fatal_wipe_required"\s*:\s*true|State wipe required before restart'
#
# Logic:
#   1. Check crashed files (bounded by check (5) retention: max 3 per category)
#   2. If no crashed match, check active log
#
# Returns: always 0
# ─────────────────────────────────────────────────────────────────────────────
has_fatal_wipe_evidence() {
  local logs_dir="$1"
  local log_file="$2"
  local pattern='fatal_wipe_required\s*[=:]\s*true|"fatal_wipe_required"\s*:\s*true|State wipe required before restart'

  FATAL_WIPE_EVIDENCE="no"
  FATAL_WIPE_SOURCE=""

  # Check crashed rotations
  if [[ -d "$logs_dir" ]]; then
    local f
    for f in "$logs_dir"/monitor.log.crashed-*; do
      [[ -f "$f" ]] || continue
      if grep -qE "$pattern" "$f" 2>/dev/null; then
        FATAL_WIPE_EVIDENCE="yes"
        FATAL_WIPE_SOURCE="crashed:$(basename "$f")"
        return 0
      fi
    done
  fi

  # Check active log (handles first-occurrence: current PID emitted the signal)
  if [[ -f "$log_file" ]] && grep -qE "$pattern" "$log_file" 2>/dev/null; then
    FATAL_WIPE_EVIDENCE="yes"
    FATAL_WIPE_SOURCE="active"
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# detect_soft_fail_blocked LOG_FILE PROC_START_EPOCH [NOW_EPOCH]
#
# Detects a running process stuck in the fatal-state-blocked loop.
#
# Arguments:
#   LOG_FILE         - Path to active monitor.log
#   PROC_START_EPOCH - Process start time (epoch seconds). Lines with timestamps
#                      before this are ignored (stale from prior run).
#   NOW_EPOCH        - Optional: current epoch (default: $(date +%s))
#
# Sets globals:
#   SOFT_FAIL_BLOCKED             - "yes" | "no"
#   SOFT_FAIL_BLOCKED_DURATION_SEC - Seconds between first and most-recent blocked
#                                    message within current PID lifetime (0 when no)
#
# Detection contract:
#   Matches ONLY the WARN-level "Recovery escalation blocked: previous fatal
#   state failure" message from consensus.rs:1174 (throttled every 30s).
#   Excludes DEBUG-level "(repeated)" variant at consensus.rs:1179-1180.
#
#   Pattern matches lines containing WARN level AND the blocked message:
#     Text: "2024-01-15T10:30:00.123456Z  WARN ... Recovery escalation blocked: previous fatal state failure"
#     JSON: {"timestamp":"...","level":"WARN",...,"message":"Recovery escalation blocked: previous fatal state failure..."}
#
# Logic:
#   1. tail -n 2000 LOG_FILE | grep (WARN + blocked pattern)
#   2. Extract ISO 8601 timestamps; skip unparseable
#   3. Convert to epoch; filter < PROC_START_EPOCH
#   4. Duration = max_epoch - min_epoch
#   5. yes when duration >= 300 AND max_epoch >= (NOW_EPOCH - 90)
#
# Edge cases:
#   - Missing/empty LOG_FILE: no, duration=0
#   - One matching line: no (duration=0 < 300)
#   - All timestamps < PROC_START_EPOCH: no
#   - Timestamp parse failure: skip silently
#   - Mixed text+JSON: both handled
#
# Returns: always 0
# ─────────────────────────────────────────────────────────────────────────────
detect_soft_fail_blocked() {
  local log_file="$1"
  local proc_start_epoch="$2"
  local now_epoch="${3:-$(date +%s)}"

  SOFT_FAIL_BLOCKED="no"
  SOFT_FAIL_BLOCKED_DURATION_SEC=0

  [[ -f "$log_file" ]] || return 0

  # Grep for WARN-level blocked messages (both text and JSON formats)
  local matched_lines
  matched_lines=$(tail -n 2000 "$log_file" 2>/dev/null \
    | grep -E '( WARN .+|"level"\s*:\s*"WARN".+)Recovery escalation blocked: previous fatal state failure' \
    2>/dev/null) || return 0

  [[ -z "$matched_lines" ]] && return 0

  # Extract and filter timestamps
  local min_epoch="" max_epoch=""
  local line ts epoch

  while IFS= read -r line; do
    # Try text format: first field is ISO timestamp (starts with "20")
    ts=$(printf '%s' "$line" | awk '{print $1}')
    if [[ "$ts" != 20* ]]; then
      # Try JSON format: extract "timestamp":"..." value
      ts=$(printf '%s' "$line" | grep -oP '"timestamp"\s*:\s*"\K[^"]+' 2>/dev/null)
    fi
    [[ -z "$ts" ]] && continue

    # Convert to epoch; skip on failure
    epoch=$(date -d "$ts" +%s 2>/dev/null) || continue
    [[ -z "$epoch" ]] && continue

    # Filter: discard timestamps before process start
    [[ "$epoch" -lt "$proc_start_epoch" ]] && continue

    # Track min and max
    if [[ -z "$min_epoch" ]] || [[ "$epoch" -lt "$min_epoch" ]]; then
      min_epoch="$epoch"
    fi
    if [[ -z "$max_epoch" ]] || [[ "$epoch" -gt "$max_epoch" ]]; then
      max_epoch="$epoch"
    fi
  done <<< "$matched_lines"

  # Need at least two distinct timestamps
  [[ -z "$min_epoch" || -z "$max_epoch" ]] && return 0

  local duration=$((max_epoch - min_epoch))
  SOFT_FAIL_BLOCKED_DURATION_SEC="$duration"

  # Fire when: sustained >= 5 min AND most recent within 90s
  local staleness=$((now_epoch - max_epoch))
  if [[ "$duration" -ge 300 ]] && [[ "$staleness" -le 90 ]]; then
    SOFT_FAIL_BLOCKED="yes"
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# eval_memory_guardrail RSS_MB AVAIL_MB HOST_RAM_GB HEAP_PREV_MB HEAP_CURR_MB HEAP_PREV2_MB
#
# Pure decision function for the monitor-tick HIGH-MEMORY guardrail (issue #3227).
# Host-RAM-relative thresholds (replaces the old absolute 12/16/8 GB literals) so
# the guardrail gives early warning on a 32 GB/no-swap box without false-firing
# on a 61 GB box. All arithmetic is integer-MB (no `bc`/floats):
#
#   report_mb       = HOST_RAM_GB * 1024 * 65 / 100   (0.65 * host RAM)
#   restart_rss_mb  = HOST_RAM_GB * 1024 * 75 / 100   (0.75 * host RAM)
#   restart_avail_mb= HOST_RAM_GB * 1024 * 12 / 100   (0.12 * host RAM)
#
# Verdict ladder (sets global MEMORY_GUARDRAIL_VERDICT):
#   restart          — RSS > restart_rss_mb AND AVAIL < restart_avail_mb AND
#                      both latest heap deltas grew > 500 MB:
#                        (HEAP_CURR - HEAP_PREV) > 500 AND (HEAP_PREV - HEAP_PREV2) > 500
#   report-high-mem  — RSS > report_mb (early-warning, report-only; no restart)
#   none             — otherwise
#
# The restart tier gates on system pressure AND evidence of a real heap leak, so
# a transient cold-catchup RSS spike (heap NOT growing) is not killed. Callers map
# the verdict to the restart ACTION / report line; this fn does NO I/O.
#
# Sets globals:
#   MEMORY_GUARDRAIL_VERDICT   "none" | "report-high-mem" | "restart"
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
eval_memory_guardrail() {
  local rss_mb="$1"
  local avail_mb="$2"
  local host_ram_gb="$3"
  local heap_prev_mb="$4"
  local heap_curr_mb="$5"
  local heap_prev2_mb="$6"

  MEMORY_GUARDRAIL_VERDICT="none"

  local report_mb=$(( host_ram_gb * 1024 * 65 / 100 ))
  local restart_rss_mb=$(( host_ram_gb * 1024 * 75 / 100 ))
  local restart_avail_mb=$(( host_ram_gb * 1024 * 12 / 100 ))

  # Restart only when system is genuinely under pressure AND the heap is leaking
  # (latest two heap_components_mb deltas both > 500 MB).
  if [[ "$rss_mb" -gt "$restart_rss_mb" ]] \
     && [[ "$avail_mb" -lt "$restart_avail_mb" ]] \
     && [[ $(( heap_curr_mb - heap_prev_mb )) -gt 500 ]] \
     && [[ $(( heap_prev_mb - heap_prev2_mb )) -gt 500 ]]; then
    MEMORY_GUARDRAIL_VERDICT="restart"
    return 0
  fi

  # Report-only early warning above 0.65 * host RAM.
  if [[ "$rss_mb" -gt "$report_mb" ]]; then
    MEMORY_GUARDRAIL_VERDICT="report-high-mem"
    return 0
  fi

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# grep_heartbeat_lines LOG_FILE [TAIL_COUNT]
#
# Prints heartbeat event lines from LOG_FILE.
# If TAIL_COUNT is provided, returns only the most recent N lines.
#
# Detection contract:
#   Text:  heartbeat=true  or  heartbeat: true
#   JSON:  "heartbeat":true
#
# Exit: preserves grep semantics (0=match, 1=no-match, 2=error).
# ─────────────────────────────────────────────────────────────────────────────
grep_heartbeat_lines() {
  local log_file="${1:?log file required}"
  local tail_count="${2:-}"
  local pattern='heartbeat\s*[=:]\s*true|"heartbeat"\s*:\s*true'
  if [[ -n "$tail_count" ]]; then
    local output rc
    output=$(grep -E "$pattern" "$log_file" 2>/dev/null)
    rc=$?
    [[ $rc -ne 0 ]] && return $rc
    printf '%s\n' "$output" | tail -n "$tail_count"
  else
    grep -E "$pattern" "$log_file" 2>/dev/null
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# classify_path_binary_relevance PATH  (issue #3215)
#
# Path-level classifier for the monitor-tick §10 deploy-gate binary-relevance
# check. Given a single repo-relative changed path, prints exactly ONE verdict:
#
#   no-impact         — allowlisted: documentation/CI/tooling/specs that never
#                       compiles into the release binary.
#   test-only         — crates/<crate>/tests/** integration tests (a separate
#                       compilation target, never linked into --release lib/bin).
#   needs-hunk-check  — crates/**/src/**.rs: MAY be test-only at the hunk level;
#                       the caller must run diff_is_test_only on this file's diff.
#   rebuild           — FAIL-SAFE default for EVERYTHING else (Cargo.toml,
#                       Cargo.lock, any build.rs, configs/, .rs outside src/tests,
#                       non-.rs under crates/, or any unrecognized path).
#
# The classifier is deliberately conservative: a false "rebuild" costs only a
# wasted build, while a false "no-impact"/"test-only" could skip a real deploy —
# so anything not provably non-binary-affecting falls through to rebuild.
#
# Prints the verdict to stdout. Returns 0 always.
# ─────────────────────────────────────────────────────────────────────────────
classify_path_binary_relevance() {
  # NOTE: must NOT be named `path` — under zsh `path` is a special array tied to
  # $PATH, so `local path=…` would clobber the command search path for the whole
  # function (#3581 zsh-safety sweep). Use a non-reserved name.
  local rel_path="$1"

  # Empty path → fail-safe.
  if [[ -z "$rel_path" ]]; then
    echo "rebuild"
    return 0
  fi

  # --- Allowlist: paths that never compile into the release binary. ---
  case "$rel_path" in
    .github/*|.claude/*|scripts/*|docs/*|metrics/*)
      echo "no-impact"; return 0 ;;
    .gitignore|.gitattributes|.gitmodules)
      echo "no-impact"; return 0 ;;
    stellar-specs|stellar-specs/*)
      echo "no-impact"; return 0 ;;
    # stellar-core is a parity-reference submodule (CLAUDE.md: "available as a
    # git submodule … for parity checks"); it is NEVER in the
    # `cargo build --release -p henyey` build graph, so a submodule-pointer bump
    # cannot change the compiled binary. Mirror the stellar-specs arm (#3530): a
    # `stellar-core`-pointer-only changeset becomes a skip *candidate*, while the
    # authoritative restart-skip is still independently gated by the §10-step-2a
    # release-binary byte-compare. So this arm can never on its own suppress a
    # real deploy — it only lets the byte-compare carve-out fire.
    stellar-core|stellar-core/*)
      echo "no-impact"; return 0 ;;
    # Container-image files (#3307): never read by `cargo build --release -p
    # henyey`, so they cannot change the compiled binary. Routing them through
    # no-impact makes a Dockerfile-only delta a skip *candidate* that the
    # §10-step-2a release-binary byte-compare then confirms, instead of forcing
    # a needless rebuild + validator restart (or re-tripping every tick).
    #
    # SAFE ONLY because the monitor runs the validator as the locally-compiled
    # binary (`release/henyey run ...`), NOT the Docker image — the Dockerfile
    # builds the SEPARATE Stellar Supercluster (SSC) integration image, which is
    # not the artifact this gate deploys. REVISIT this carve-out if the deploy
    # model ever becomes image-based (validator run from the Docker image): then
    # a Dockerfile change WOULD alter the deployed runtime and must rebuild.
    #
    # `*.dockerfile` matches slashed paths too (e.g. `ci/foo.dockerfile`); that
    # is fine — no `.dockerfile`-suffixed file is in the cargo build graph.
    # docker-compose*.yml is deliberately NOT allowlisted (stays on the rebuild
    # fail-safe); see the contrast assertion in test-monitor-skill-snippets.sh.
    Dockerfile|.dockerignore|*.dockerfile)
      echo "no-impact"; return 0 ;;
  esac
  # Root-level *.md (e.g. README.md, CLAUDE.md) — no slash, .md suffix.
  if [[ "$rel_path" != */* && "$rel_path" == *.md ]]; then
    echo "no-impact"; return 0
  fi

  # --- crates/<crate>/tests/** : integration tests, never in --release lib/bin.
  # Matches a `tests/` directory directly under a single crate dir.
  if [[ "$rel_path" == crates/*/tests/* ]]; then
    echo "test-only"; return 0
  fi

  # --- crates/**/src/**.rs : source that MAY be hunk-level test-only.
  # Must be a .rs file living under a `src/` directory inside crates/.
  if [[ "$rel_path" == crates/*/src/*.rs && "$rel_path" == *.rs ]]; then
    echo "needs-hunk-check"; return 0
  fi

  # --- Everything else: fail-safe rebuild. ---
  echo "rebuild"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# select_latest_green_deploy_target DEPLOYED_SHA ORIGIN_SHA   (issue #3351)
#
# Pure deploy-target selector for the monitor-tick §10 deploy gate. On a busy
# `main` the heavyweight `CI` workflow is cancel-per-head (structurally never
# green at HEAD) and `Verify Execution (Mainnet)` runs only on schedule /
# workflow_dispatch — so it is green only on the specific heads that were HEAD
# at a cron/dispatch run, never on the racing tip. The legacy "deploy origin
# HEAD iff its CI is green" gate therefore never fires. This selector instead
# picks the NEWEST commit whose `Verify Execution (Mainnet)` run completed
# `success` and that is safe to deploy (operator option 3: deploy latest-green).
#
# Reads Verify-Execution run records on stdin, one per line, newest-first
# (exactly the `gh run list … --json headSha,status,conclusion --jq` order):
#     <headSha>|<status>|<conclusion>
#
# Greenness is keyed on each sha's MOST-RECENT definitive verdict, not "any
# success ever". `Verify Execution (Mainnet)` is deterministic per-sha, so a
# newer `failure` on a sha whose older run `success`ed means a genuine
# regression (or a flake) surfaced by newer mainnet data — either way "latest
# success ever" is the wrong signal for "safe to ship now" (#3740). Because
# records arrive newest-first, the FIRST definitive (success/failure) row per
# sha wins; an older success can no longer resurrect a sha whose newest run
# failed. Non-definitive conclusions (cancelled/skipped/neutral/…) are
# cancel-per-head noise and are ignored (older rows still decide the sha).
#
# Prints the chosen deploy-target sha to STDOUT (empty if none), and a single
# reason token to STDERR. Reason tokens:
#     (sha printed)        — a valid newer green target was selected
#     no-green-run         — no definitive verdict yielded a usable green target
#     green-equals-deployed— newest valid green sha == deployed (up-to-date)
#     green-behind-deployed— only green sha(s) are ancestors of deployed (no
#                            backwards deploy)
#     green-not-on-main    — only green sha(s) are not ancestors of ORIGIN_SHA
#                            (off-main / force-pushed / unresolvable locally)
#     green-superseded-by-failure — a sha's newest VE verdict was a failure that
#                            superseded an older success for that same sha, and
#                            no other usable green remained (#3740)
#     walk-exceeded        — newest valid green sha is > MAX_DEPLOY_WALK commits
#                            ahead of deployed (defensive staleness bound)
#
# FAIL-CLOSED contract (DEPLOY-SAFETY):
#   - empty stdout  ⇒ caller MUST NOT deploy (no validated target). Never emits
#     a sha that is unvalidated, off-main, equal-to/behind deployed, or beyond
#     the walk bound.
#   - the ancestry oracle is injected as the overridable shell functions
#     `is_ancestor A B` (0 iff A is ancestor-or-equal of B) and
#     `commits_ahead DEP SHA` (count of DEP..SHA), defaulting to git. An oracle
#     ERROR or a sha unresolvable locally is treated as "skip THIS candidate"
#     (reason green-not-on-main) — it never aborts the tick and never deploys.
#   - records are consumed newest-first; the FIRST candidate satisfying every
#     guardrail wins. `green-equals-deployed` short-circuits as up-to-date the
#     moment the newest valid-on-main green sha is the deployed sha (so we never
#     redeploy the running binary, and never look past it to an older green).
#
# Returns 0 always (the reason token + empty/non-empty stdout carry the result).
# ─────────────────────────────────────────────────────────────────────────────

# Defensive staleness bound: refuse to jump more than this many commits in one
# deploy, even to a green sha (guards against a weeks-lapsed schedule yielding a
# wildly stale "green"). Single documented constant; env-overridable for tests.
: "${MAX_DEPLOY_WALK:=200}"

# Ancestry oracle (overridable for hermetic tests). Default: real git.
# is_ancestor A B → 0 iff A is an ancestor-or-equal of B (A reachable from B).
# NOTE: the existence guard MUST use the POSIX-portable `command -v`, NOT the
# bash-only `declare -F`. Under zsh `declare` aliases `typeset`, so
# `typeset -F is_ancestor` declares a FLOAT VARIABLE named is_ancestor and
# returns 0 — the `! declare -F …` guard is then false, the fallback function
# is NEVER defined, and the read loop hits `command not found: is_ancestor`,
# mis-resolving every green-on-main run to `green-not-on-main` (#3592). Same
# zsh-vs-bash class as the `status` reserved-var bug fixed in #3581.
if ! command -v is_ancestor >/dev/null 2>&1; then
  is_ancestor() { git merge-base --is-ancestor "$1" "$2" 2>/dev/null; }
fi
# commits_ahead DEP SHA → number of commits in DEP..SHA (reachable from SHA,
# not from DEP). Prints 0 on any error so the caller's numeric compare is safe.
# Guarded with `command -v` (portable) — see the is_ancestor note above (#3592).
if ! command -v commits_ahead >/dev/null 2>&1; then
  commits_ahead() { git rev-list --count "$1".."$2" 2>/dev/null || echo 0; }
fi

select_latest_green_deploy_target() {
  local deployed_sha="$1" origin_sha="$2"
  # NOTE: must NOT be named `status` — under zsh `status` is a read-only special
  # parameter (alias for $?), so `local … status …` aborts the function at the
  # declaration before the read loop runs, yielding empty stdout. Empty stdout
  # makes the deploy gate fail-closed to `defer-no-green`, silently deferring
  # every deploy (#3581). Use a non-reserved name.
  local line sha run_status conclusion
  local saw_green=0           # any completed/success record that was NOT superseded
  local saw_on_main=0         # any green record that is on main (ancestor of HEAD)
  local saw_superseded=0      # a sha's newest verdict was failure but an older run succeeded
  # Per-sha bookkeeping via whitespace-delimited accumulator strings (NOT a
  # bash-4 `local -A` associative array): the function is invoked under
  # `emulate -L zsh` in the deploy gate (see the #3581/#3592 zsh cases), where
  # associative-array semantics diverge. Hex shas contain no whitespace, so
  # space-delimited membership (`case " $set " in *" $sha "*)`) is exact.
  local seen_shas=''          # shas whose newest DEFINITIVE verdict is already recorded
  local failed_shas=''        # shas whose newest definitive verdict was failure

  while IFS='|' read -r sha run_status conclusion; do
    # Skip blank lines.
    [[ -z "$sha" ]] && continue
    # Only completed runs carry a verdict; in-progress/queued rows are not yet a
    # conclusion for this sha — skip without recording (an older completed row
    # for the same sha still decides it).
    [[ "$run_status" == "completed" ]] || continue
    # Only success/failure are DEFINITIVE verdicts. Any other conclusion
    # (cancelled/skipped/neutral/timed_out/action_required/…) is cancel-per-head
    # noise on a busy main — ignore the row and keep consulting older rows for
    # this sha (preserves the HEAD|cancelled → older-green behavior of #3351).
    [[ "$conclusion" == "success" || "$conclusion" == "failure" ]] || continue

    # Records arrive newest-first, so the FIRST definitive row per sha is that
    # sha's authoritative (most-recent) verdict. Once recorded, an older row for
    # the same sha cannot change it — an older success can no longer resurrect a
    # sha whose newest VE run failed. When a later (older) success row shows up
    # for a sha we already recorded as failed, note that a green was superseded
    # by a newer failure (the "note when skipped due to a newer failure" #3740
    # asks for) — then skip it.
    case " $seen_shas " in
      *" $sha "*)
        if [[ "$conclusion" == "success" ]]; then
          case " $failed_shas " in *" $sha "*) saw_superseded=1 ;; esac
        fi
        continue
        ;;
    esac
    seen_shas="$seen_shas $sha"

    if [[ "$conclusion" == "failure" ]]; then
      # Newest verdict for this sha is a failure → disqualify the sha entirely.
      # Record it so a later (older) success row for the same sha is reported as
      # green-superseded-by-failure rather than a bare no-green-run. saw_green is
      # intentionally NOT set for a disqualified sha.
      failed_shas="$failed_shas $sha"
      continue
    fi

    # conclusion == success and this is the sha's newest definitive verdict.
    saw_green=1

    # Guardrail 1 — on main: the green sha must be an ancestor-or-equal of the
    # origin HEAD. An oracle error / locally-unresolvable sha lands here too
    # (is_ancestor returns non-zero) → skip THIS candidate, never abort.
    if ! is_ancestor "$sha" "$origin_sha"; then
      continue
    fi
    saw_on_main=1

    # Up-to-date: the newest valid-on-main green sha IS the deployed sha. This is
    # a distinct, terminal outcome — report up-to-date and stop (never redeploy
    # the running binary, never fall through to an older green).
    if [[ "$sha" == "$deployed_sha" ]]; then
      echo "green-equals-deployed" >&2
      return 0
    fi

    # Guardrail 2 — not backwards: the green sha must be deployed-or-descendant.
    # If deployed_sha is NOT an ancestor of sha, this green is behind/diverged
    # from what's running → never deploy backwards. Skip this candidate.
    if ! is_ancestor "$deployed_sha" "$sha"; then
      continue
    fi

    # Guardrail 3 — walk bound: refuse a green sha more than MAX_DEPLOY_WALK
    # commits ahead of deployed (defensive against a long-lapsed schedule).
    local ahead
    ahead="$(commits_ahead "$deployed_sha" "$sha")"
    [[ "$ahead" =~ ^[0-9]+$ ]] || ahead=0
    if (( ahead > MAX_DEPLOY_WALK )); then
      echo "walk-exceeded" >&2
      return 0
    fi

    # All guardrails passed — this is the newest valid green target.
    echo "$sha"
    return 0
  done

  # No candidate selected — emit the most specific reason. Any green sha that
  # reached the walk-bound or up-to-date checks already returned inline above, so
  # reaching here means every green record failed an earlier guardrail.
  if (( saw_on_main == 1 )); then
    # Green sha(s) exist on main but every one is an ancestor of deployed_sha
    # (behind what's running) → no backwards deploy.
    echo "green-behind-deployed" >&2
  elif (( saw_green == 1 )); then
    # Green sha(s) exist but none are ancestors of origin HEAD (off-main /
    # force-pushed / unresolvable locally).
    echo "green-not-on-main" >&2
  elif (( saw_superseded == 1 )); then
    # No usable green remained, and the reason is that a sha's newest VE verdict
    # was a failure that superseded an older success for that same sha (#3740).
    echo "green-superseded-by-failure" >&2
  else
    # No definitive (success/failure) verdict yielded a usable green target.
    echo "no-green-run" >&2
  fi
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# diff_is_test_only  (reads ONE file's unified diff on stdin)  (issue #3215)
#
# Hunk-level classifier: returns 0 ONLY if EVERY changed (`+`/`-`) line in the
# diff lies strictly inside a `#[cfg(test)]` / `mod tests` region, tracked by
# brace-depth from the region opener to its matching close brace. ANY of the
# following → return 1 (rebuild), because the change may affect the binary:
#
#   - a changed line outside any confirmed cfg(test) region;
#   - a changed line ON the `#[cfg(test)]` attribute or the `mod tests {` opener
#     itself (the region boundary is being mutated — ambiguous);
#   - a hunk that cannot be anchored (zero-context hunk, whole-module deletion,
#     or any parse ambiguity);
#   - an empty / unreadable diff.
#
# Deliberately dumb + fail-safe: its safety comes from defaulting to rebuild on
# ANY ambiguity, NOT from parsing Rust accurately. It is NOT a Rust parser and
# must not grow into one. The authoritative skip is gated downstream by a
# release-binary byte-compare; this function only prunes the candidate set.
#
# The diff is re-walked using ONLY the new-file ('+'/' ') lines (the post-change
# file content the hunk headers describe). We reconstruct the post-change line
# stream, tag each reconstructed line as "changed" if it is an added line or sits
# adjacent to a removed line, and verify every changed line is inside a region.
#
# Returns: 0 = test-only (safe-to-consider-skip candidate); 1 = rebuild.
# ─────────────────────────────────────────────────────────────────────────────
diff_is_test_only() {
  awk '
    BEGIN {
      in_region = 0      # currently inside a confirmed #[cfg(test)] region
      depth = 0          # brace depth within the region
      pending_cfg = 0    # saw #[cfg(test)] attribute, awaiting its mod opener
      saw_hunk = 0       # at least one @@ hunk header seen
      saw_change = 0     # at least one +/- content line seen
      bad = 0            # a changed line landed outside a region / on a boundary
    }

    # Skip diff metadata lines that arent hunk content.
    /^diff --git / { next }
    /^index /      { next }
    /^--- /        { next }
    /^\+\+\+ /     { next }
    /^old mode /   { next }
    /^new mode /   { next }
    /^similarity / { next }
    /^rename /     { next }
    /^new file /   { next }
    /^deleted file / { next }
    /^Binary files / { bad = 1; next }

    # Hunk header. Reject zero-context / zero-line headers we cannot anchor:
    # a hunk whose new-file span is "+N,0" (pure deletion, no post lines) or
    # "+N" with no count and no context cannot be situated inside a region.
    /^@@ / {
      saw_hunk = 1
      # Parse the "+start,len" field. Forms: @@ -a,b +c,d @@  or  @@ -a +c @@
      hdr = $0
      # Extract the +c,d token.
      if (match(hdr, /\+[0-9]+(,[0-9]+)?/)) {
        plus = substr(hdr, RSTART, RLENGTH)
        sub(/^\+/, "", plus)
        if (index(plus, ",") > 0) {
          split(plus, pp, ",")
          newlen = pp[2] + 0
        } else {
          newlen = 1
        }
        # A hunk that adds no post-change lines (pure deletion) cannot be
        # anchored as "inside a test region" — fail-safe.
        if (newlen == 0) { bad = 1 }
      } else {
        bad = 1
      }
      next
    }

    # Content lines. Only meaningful after a hunk header.
    {
      if (saw_hunk == 0) { next }   # stray line before any hunk → ignore

      sign = substr($0, 1, 1)
      text = substr($0, 2)

      if (sign == "-") {
        # A removed line. If it is inside a region, fine; if it touches a region
        # boundary, flag. We treat removed lines conservatively: a removed line
        # that is NOT inside a confirmed region → bad. Removed lines do not
        # advance the post-change brace tracker (they are gone), but they DO
        # signal a change at the current region state.
        saw_change = 1
        # If the removed line is the region opener/attribute, the boundary is
        # being mutated → ambiguous.
        if (text ~ /#\[cfg\(test\)\]/ || text ~ /(^|[[:space:]])mod[[:space:]]+tests([[:space:]]|\{|$)/) {
          bad = 1
        } else if (in_region == 0) {
          bad = 1
        }
        next
      }

      # Context (space) or added (plus) line — part of the post-change file.
      changed = (sign == "+") ? 1 : 0
      if (sign == "+") saw_change = 1

      # --- Region tracking on the post-change line stream. ---
      # Detect a #[cfg(test)] attribute: arms the next mod opener.
      is_cfg_attr = (text ~ /#\[cfg\(test\)\]/)
      # Detect a `mod tests` opener (allow `pub mod tests`, trailing brace/space).
      is_mod_open = (text ~ /(^|[[:space:]])mod[[:space:]]+tests([[:space:]]*\{|[[:space:]]*$)/)

      if (changed && (is_cfg_attr || is_mod_open)) {
        # Editing the region boundary itself → ambiguous → fail-safe.
        bad = 1
      }

      if (is_cfg_attr) {
        pending_cfg = 1
      }

      # Count braces on this line to maintain depth when inside a region.
      opens = gsub(/\{/, "{", text)   # gsub returns count of substitutions
      closes = gsub(/\}/, "}", text)

      if (in_region == 0) {
        # Entering a region only when a #[cfg(test)]-armed `mod tests {` opens.
        if (is_mod_open && pending_cfg) {
          in_region = 1
          pending_cfg = 0
          # The opener line itself: a changed opener was already flagged above.
          # Initialize depth from the braces on the opener line.
          depth = opens - closes
          if (depth <= 0) {
            # `mod tests;` (no body) or one-liner — no region body to be inside.
            in_region = 0
            depth = 0
          }
        } else {
          # Not in a region. A changed line here is outside → bad.
          if (changed) bad = 1
          # A `mod tests {` without a preceding #[cfg(test)] is not a test region
          # we trust; pending_cfg only persists to the immediately-following mod.
          if (is_mod_open) pending_cfg = 0
        }
      } else {
        # Inside a region: this line is in-region BEFORE applying its own braces
        # for the closing case. A changed in-region line is allowed.
        depth += opens - closes
        if (depth <= 0) {
          # Region closed on/at this line. A changed line that closes the region
          # is at the boundary → ambiguous → fail-safe.
          if (changed) bad = 1
          in_region = 0
          depth = 0
        }
      }

      next
    }

    END {
      # Fail-safe on: any boundary/outside violation, an empty diff, a diff with
      # no hunks, or a diff with no actual changes.
      if (bad)         { exit 1 }
      if (saw_hunk == 0) { exit 1 }
      if (saw_change == 0) { exit 1 }
      exit 0
    }
  '
}

# ─────────────────────────────────────────────────────────────────────────────
# classify_stuck_alive_sync NODE_STATE CURRENT_LCL AGE_SEC RPC_STATUS RSS_MB \
#                           PROC_RESPONSIVE STATE_FILE [NOW_EPOCH] \
#                           [HEARTBEAT_AGE_SEC]
#
# Pure decision function for the monitor-tick remediation rung (3e):
# "stuck-but-alive SYNC FAILURE" auto-restart (issue #3219).
#
# Detects a node that is ALIVE and RESPONSIVE (admin /info answers) but is NOT
# making ledger progress: a frozen local `lcl`, climbing RPC `age`, RPC
# `unhealthy`, and RSS UNDER the OOM floor. This is the residual failure mode
# left over after (3c) soft-fail-wipe (owns the fatal-state-wipe case) and (3b)
# wedge (owns the UNRESPONSIVE / frozen-event-loop case). (3e) is the MIRROR of
# (3b): (3b) fires when the admin port is dead/timed-out; (3e) requires a live
# node. Liveness is proven by EITHER the admin port answering
# (PROC_RESPONSIVE=yes) OR — when /info is unresponsive — a `heartbeat=true` log
# line within HEARTBEAT_ALIVE_SEC (=120s), proving the event loop still ticks
# (#3579: the "/info-unresponsive-but-alive near-tip stall"). The two remain
# mutually exclusive: (3b) wedge requires a stale/absent heartbeat (truly
# frozen loop); a fresh heartbeat routes the node to (3e) instead. (3e) can
# never poach the genuinely-wedged path.
#
# Band-aid for root cause #3218 (overlay SCP broadcast backpressure). The
# max-restarts→escalate guard ensures a restart loop surfaces as `urgent`
# rather than silently masking the unfixed defect.
#
# Mirrors detect_soft_fail_blocked's injectable-time design: NOW_EPOCH is the
# optional last positional arg (defaults to wall clock) so all dwell/cooldown/
# window arithmetic is deterministically testable.
#
# SOLE READER/WRITER of STATE_FILE — the caller supplies only the live
# CURRENT_LCL value and the file path; this function owns all reads/writes of
# `frozen_lcl`, `frozen_since_epoch`, `last_restart_epoch`, and `restart_epochs`.
# On each call: if CURRENT_LCL != frozen_lcl, the lcl advanced (NOT stuck) → reset
# frozen_lcl=CURRENT_LCL and frozen_since_epoch=NOW; else frozen_since_epoch
# accumulates. frozen_lcl_seconds = NOW - frozen_since_epoch.
#
# Thresholds (named constants — operators may retune):
#   STUCK_AGE_SEC=600       RPC wall-clock `age` floor (check (2)'s 30s is a
#                           report-only flag; a restart must be far more
#                           conservative). AGE + frozen-lcl are an intentionally
#                           conservative AND (correlation only reduces false-fire).
#   STUCK_DWELL_SEC=600     frozen-lcl wall-clock dwell (cadence-independent;
#                           mirrors check (2)'s "<ledger>|<ts>" STUCK idiom).
#   OOM_FLOOR_MB=16384      RSS ceiling; the OOM gate (check (4)) owns over-floor.
#   STUCK_COOLDOWN_SEC=900  min seconds between stuck-restarts.
#   STUCK_WINDOW_SEC=7200   rolling window (2h) for the max-restarts guard.
#   MAX_STUCK_RESTARTS=3    max stuck-restarts within the window before escalate.
#
# State-set match: case-insensitive substring `valid` | `synced` | `track`.
# Verified against the real /info `state` enum
# (crates/app/src/compat_http/handlers/info.rs): healthy/validating →
# "Synced!" (matches `synced`), catch-up → "Catching up" (no match → never
# fires), boot → "Booting", stop → "Stopping". The extra `valid`/`track`
# substrings are defensive against future enum drift.
#
# Sets global STUCK_ALIVE_SYNC ∈ "yes" | "no" | "cooldown" | "escalate":
#   yes       — all six AND-gate conditions hold and neither guard tripped;
#               appends NOW to restart_epochs and sets last_restart_epoch=NOW.
#   cooldown  — conditions hold but NOW - last_restart_epoch < STUCK_COOLDOWN_SEC.
#   escalate  — conditions hold but >= MAX_STUCK_RESTARTS within STUCK_WINDOW_SEC.
#   no        — any AND-gate condition fails.
# Returns: 0 always. Does NO process I/O (no kill/relaunch) — the (3e) rung does that.
# ─────────────────────────────────────────────────────────────────────────────
classify_stuck_alive_sync() {
  local node_state="$1"
  local current_lcl="$2"
  local age_sec="$3"
  local rpc_status="$4"
  local rss_mb="$5"
  local proc_responsive="$6"
  local state_file="$7"
  local now_epoch="${8:-$(date +%s)}"
  # Optional arg 9 (#3579): seconds since the most recent `heartbeat=true` log
  # line. Empty / "-1" / non-numeric means "unknown / no recent heartbeat". Used
  # ONLY as a liveness fallback when /info is unresponsive (proc_responsive=no).
  local heartbeat_age_sec="${9:-}"

  # Named thresholds (operator-retunable).
  local STUCK_AGE_SEC=600
  local STUCK_DWELL_SEC=600
  local OOM_FLOOR_MB=16384
  local STUCK_COOLDOWN_SEC=900
  local STUCK_WINDOW_SEC=7200
  local MAX_STUCK_RESTARTS=3
  # Liveness-fallback window: a heartbeat this recent proves the event loop is
  # ticking even when /info is down. Mirrors the (3b) wedge 120s freshness gate,
  # so a node with a stale (>120s) heartbeat is "wedged" (3b owns it), while a
  # fresh-heartbeat node is "alive" and routes to (3e). (#3579)
  local HEARTBEAT_ALIVE_SEC=120

  STUCK_ALIVE_SYNC="no"

  # ── Read prior state (this function is the sole reader/writer). ─────────────
  local frozen_lcl="" frozen_since_epoch="" last_restart_epoch="0" restart_epochs=""
  if [[ -f "$state_file" ]]; then
    local line key val
    while IFS='=' read -r key val; do
      case "$key" in
        frozen_lcl)          frozen_lcl="$val" ;;
        frozen_since_epoch)  frozen_since_epoch="$val" ;;
        last_restart_epoch)  last_restart_epoch="$val" ;;
        restart_epochs)      restart_epochs="$val" ;;
      esac
    done < "$state_file"
  fi
  [[ -z "$last_restart_epoch" ]] && last_restart_epoch="0"

  # ── Frozen-lcl bookkeeping: reset on advance, else accumulate. ──────────────
  if [[ "$current_lcl" != "$frozen_lcl" ]]; then
    frozen_lcl="$current_lcl"
    frozen_since_epoch="$now_epoch"
  fi
  # Defensive: if the state file was missing/corrupt, anchor frozen_since to now.
  [[ -z "$frozen_since_epoch" ]] && frozen_since_epoch="$now_epoch"

  local frozen_lcl_seconds=$(( now_epoch - frozen_since_epoch ))
  [[ "$frozen_lcl_seconds" -lt 0 ]] && frozen_lcl_seconds=0

  # Prune restart_epochs to the rolling window (drop entries older than the window).
  local pruned="" e
  for e in $restart_epochs; do
    [[ "$e" =~ ^[0-9]+$ ]] || continue
    if [[ $(( now_epoch - e )) -lt "$STUCK_WINDOW_SEC" ]]; then
      pruned="${pruned:+$pruned }$e"
    fi
  done
  restart_epochs="$pruned"

  # ── Persist the (possibly reset/pruned) state before any early return. ──────
  _write_stuck_state() {
    printf 'frozen_lcl=%s\nfrozen_since_epoch=%s\nlast_restart_epoch=%s\nrestart_epochs=%s\n' \
      "$frozen_lcl" "$frozen_since_epoch" "$last_restart_epoch" "$restart_epochs" \
      > "$state_file" 2>/dev/null || true
  }

  # ── Six-way AND-gate. Any miss → "no" (state persisted, no restart). ────────
  # 1. Healthy/validating state (case-insensitive substring valid|synced|track).
  local lc_state
  lc_state=$(printf '%s' "$node_state" | tr '[:upper:]' '[:lower:]')
  case "$lc_state" in
    *valid*|*synced*|*track*) : ;;   # match — continue
    *) _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0 ;;
  esac
  # 2. Process alive. Primary signal: /info answered (proc_responsive=yes,
  #    mirror of (3b) wedge). Fallback (#3579): /info is unresponsive but a
  #    `heartbeat=true` log line appeared within HEARTBEAT_ALIVE_SEC, proving the
  #    event loop is still ticking — the "/info-unresponsive-but-alive" near-tip
  #    stall. (3b) wedge keeps the truly-wedged case (no recent heartbeat); the
  #    120s window matches (3b)'s freshness gate so the two stay mutually
  #    exclusive (alive-via-heartbeat → here; stale/no heartbeat → wedge).
  local proc_alive="no"
  if [[ "$proc_responsive" == "yes" ]]; then
    proc_alive="yes"
  elif [[ "$heartbeat_age_sec" =~ ^[0-9]+$ ]] \
       && [[ "$heartbeat_age_sec" -le "$HEARTBEAT_ALIVE_SEC" ]]; then
    proc_alive="yes"
  fi
  [[ "$proc_alive" == "yes" ]] || { _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0; }
  # 3. RPC unhealthy.
  [[ "$rpc_status" == "unhealthy" ]] || { _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0; }
  # 4. age over threshold.
  [[ "$age_sec" -gt "$STUCK_AGE_SEC" ]] || { _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0; }
  # 5. frozen-lcl dwell met.
  [[ "$frozen_lcl_seconds" -ge "$STUCK_DWELL_SEC" ]] || { _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0; }
  # 6. RSS under OOM floor.
  [[ "$rss_mb" -lt "$OOM_FLOOR_MB" ]] || { _write_stuck_state; STUCK_ALIVE_SYNC="no"; return 0; }

  # ── Guards (conditions 1–6 hold). ───────────────────────────────────────────
  # Cooldown: too soon after the last stuck-restart.
  if [[ "$last_restart_epoch" -gt 0 ]] \
     && [[ $(( now_epoch - last_restart_epoch )) -lt "$STUCK_COOLDOWN_SEC" ]]; then
    _write_stuck_state
    STUCK_ALIVE_SYNC="cooldown"
    return 0
  fi
  # Max-restarts: a restart loop signals #3218 is unfixed → escalate, no restart.
  local restart_count=0
  for e in $restart_epochs; do
    restart_count=$(( restart_count + 1 ))
  done
  if [[ "$restart_count" -ge "$MAX_STUCK_RESTARTS" ]]; then
    _write_stuck_state
    STUCK_ALIVE_SYNC="escalate"
    return 0
  fi

  # ── Fire: record this restart and return yes. ──────────────────────────────
  restart_epochs="${restart_epochs:+$restart_epochs }$now_epoch"
  last_restart_epoch="$now_epoch"
  _write_stuck_state
  STUCK_ALIVE_SYNC="yes"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# classify_event_loop_stall LOG_FILE STATE_FILE [NOW_EPOCH]
#
# Pure decision function for the monitor-tick alive-path sub-check (3f)
# "Recovered event-loop stall policy" (issue #3815).
#
# #3795 made the node emit exact loop-side accounting of event-loop inter-tick
# stalls. That signal comes in TWO shapes from crates/app/src/app/mod.rs:
#   - the SAMPLER's emit_error() carries `watchdog_freeze=true` — the loop is
#     wedged *right now*; monitor-tick's (3b) wedge owns it (restart is correct).
#   - the LOOP-SIDE emit_event_loop_stall() (mod.rs:4711) fires only *after* the
#     loop has demonstrably resumed. It deliberately carries NEITHER auto-restart
#     pattern (no `watchdog_freeze`, no "WATCHDOG: Event loop appears frozen"),
#     instead emitting message "Event loop stall (loop-side exact accounting)"
#     with `stall_recovered=true`, at WARN ([15,30)s) or ERROR (>=30s) tier.
#
# Restarting a node that has ALREADY recovered is the wrong action, so a lone
# recovered stall must NOT restart. But a *chronic* run of recovered >=30 s parks
# is real degradation. This function makes that policy explicit — replacing the
# pre-#3815 non-monotonicity where a 35 s recovered park (sampler ERROR-window
# blind) escaped restart while a 45 s one tripped it, purely by sampler geometry.
#
# It counts recovered ERROR-tier stall lines whose timestamps fall inside a
# rolling STALL_WINDOW_SEC and maps the count to an explicit verdict. The (3b)
# wedge `watchdog_freeze=true` line is never counted here: it carries neither the
# loop-side message nor `stall_recovered`, so the three-way AND-grep excludes it.
# The WARN-tier recovered stall is likewise excluded (only ERROR drives policy).
#
# SOLE READER/WRITER of STATE_FILE — the caller supplies only the log path and
# the state-file path. Only `last_restart_epoch` is persisted (for the restart
# cooldown guard); the in-window count is derived from the LOG each call, so the
# function is idempotent w.r.t. repeated ticks over the same log tail.
#
# Mirrors classify_stuck_alive_sync's injectable-time design: NOW_EPOCH is the
# optional last positional arg (defaults to wall clock) so all window/cooldown
# arithmetic is deterministically testable.
#
# Thresholds (named constants — operators may retune):
#   STALL_WINDOW_SEC=7200     rolling window (2h) for the in-window count.
#   MAX_RECOVERED_STALLS=3    recovered ERROR stalls in window before escalating
#                             from alert-only to restart.
#   STALL_COOLDOWN_SEC=900    min seconds between recovered-stall restarts; a
#                             restart within this window degrades to `alert`.
#
# Sets globals:
#   EVENT_LOOP_STALL_VERDICT ∈ "none" | "alert" | "restart":
#     none    — no recovered ERROR stall in window.
#     alert   — 1..(MAX_RECOVERED_STALLS-1) in window, OR a would-be restart
#               suppressed by the cooldown guard (alert-only, no restart).
#     restart — >= MAX_RECOVERED_STALLS in window and the cooldown is clear;
#               records last_restart_epoch=NOW.
#   EVENT_LOOP_STALL_COUNT — the in-window recovered ERROR stall count.
#
# Defensive anchoring: a missing/unreadable log, a missing/corrupt state file, or
# an unparseable timestamp all fail INERT toward none/alert — never `restart`.
# Returns: 0 always. Does NO process I/O (no kill/relaunch) — the (3f) rung does that.
# ─────────────────────────────────────────────────────────────────────────────
classify_event_loop_stall() {
  local log_file="$1"
  local state_file="$2"
  local now_epoch="${3:-$(date +%s)}"

  # Named thresholds (operator-retunable).
  local STALL_WINDOW_SEC=7200
  local MAX_RECOVERED_STALLS=3
  local STALL_COOLDOWN_SEC=900

  EVENT_LOOP_STALL_VERDICT="none"
  EVENT_LOOP_STALL_COUNT=0

  # ── Read prior state (sole reader/writer). Only last_restart_epoch persists. ─
  local last_restart_epoch="0"
  if [[ -f "$state_file" ]]; then
    local key val
    while IFS='=' read -r key val; do
      case "$key" in
        last_restart_epoch) last_restart_epoch="$val" ;;
      esac
    done < "$state_file"
  fi
  # Corrupt/non-numeric last_restart_epoch → treat as "never" (fail inert).
  [[ "$last_restart_epoch" =~ ^[0-9]+$ ]] || last_restart_epoch="0"

  _write_stall_state() {
    printf 'last_restart_epoch=%s\n' "$last_restart_epoch" > "$state_file" 2>/dev/null || true
  }

  # ── Count recovered ERROR-tier stall lines in the rolling window. ───────────
  # Three-way AND: ERROR level prefix, the loop-side message, and stall_recovered
  # true (matches BOTH Text `stall_recovered=true` and JSON `"stall_recovered":true`
  # — the optional `"?` absorbs the JSON closing quote, `[=:]` the `=`/`:`). The
  # (3b) wedge `watchdog_freeze=true` line matches none of the message/field
  # anchors, so it can never be counted here. WARN-tier lines fail the ERROR
  # anchor. A missing/unreadable log yields an empty stream → count 0 → none.
  local count=0 line ts epoch age
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    # Leading ISO8601 timestamp up to the trailing Z (same shape the wedge and
    # heartbeat-freshness checks parse). Unparseable → skip the line (inert).
    ts=$(printf '%s' "$line" | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z' | head -1)
    [[ -z "$ts" ]] && continue
    epoch=$(date -d "$ts" +%s 2>/dev/null)
    [[ "$epoch" =~ ^[0-9]+$ ]] || continue
    age=$(( now_epoch - epoch ))
    # In-window only: prune entries at/older than the window; ignore future ts.
    [[ "$age" -ge 0 && "$age" -lt "$STALL_WINDOW_SEC" ]] || continue
    count=$(( count + 1 ))
  done < <(grep -E '^[^ ]+Z[[:space:]]+ERROR[[:space:]]' "$log_file" 2>/dev/null \
             | grep -F 'Event loop stall (loop-side exact accounting)' \
             | grep -E 'stall_recovered"?[[:space:]]*[=:][[:space:]]*true')

  EVENT_LOOP_STALL_COUNT="$count"

  # ── Apply explicit policy. ──────────────────────────────────────────────────
  if [[ "$count" -le 0 ]]; then
    _write_stall_state
    EVENT_LOOP_STALL_VERDICT="none"
    return 0
  fi
  if [[ "$count" -lt "$MAX_RECOVERED_STALLS" ]]; then
    _write_stall_state
    EVENT_LOOP_STALL_VERDICT="alert"
    return 0
  fi
  # count >= MAX_RECOVERED_STALLS → restart, unless the cooldown guard trips.
  if [[ "$last_restart_epoch" -gt 0 ]] \
     && [[ $(( now_epoch - last_restart_epoch )) -lt "$STALL_COOLDOWN_SEC" ]]; then
    _write_stall_state
    EVENT_LOOP_STALL_VERDICT="alert"   # suppressed restart degrades to alert-only
    return 0
  fi
  last_restart_epoch="$now_epoch"
  _write_stall_state
  EVENT_LOOP_STALL_VERDICT="restart"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# classify_obsrvr_radar INDEX_HTTP_CODE NODE_HTTP_CODE HAS_REQUIRED_FIELDS
#
# Decision logic for monitor-tick check (9) "OBSRVR Radar" (issue #3753). Maps
# the result of the two Radar probes to exactly ONE of four stable literal
# tokens — the codified fix for the 37-distinct-spelling watch-token spread that
# made a 4+ day structural failure invisible. The emitted literal is what the
# tick appends to the `watch` array as `obsrvr=<literal>`, so daily-summary can
# aggregate a stable key.
#
# Arguments:
#   INDEX_HTTP_CODE     - HTTP status from `GET /api/v1/nodes` (the index
#                         endpoint). Empty string = timeout / no response.
#   NODE_HTTP_CODE      - HTTP status from `GET /api/v1/nodes/<PUBLIC_KEY>` (the
#                         per-node endpoint). Empty string = timeout / no response.
#   HAS_REQUIRED_FIELDS - literal `true`/`false`: whether a 2xx per-node body
#                         carried non-null `latestLedger` AND `updatedAt`.
#
# Prints exactly one literal on stdout (no trailing decoration):
#   ok             - per-node 2xx with required fields present.
#   api-incomplete - per-node 2xx but `latestLedger`/`updatedAt` missing/null
#                    (stale partial response; do NOT evaluate `lag`).
#   not-indexed    - per-node 404 AND index 200: the node is genuinely absent
#                    from an otherwise-healthy index (permanent until the node is
#                    registered with Radar — an out-of-band operator action).
#   api-error      - anything else: any non-2xx/timeout that is NOT the
#                    404-with-index-up signature, INCLUDING a per-node 404 while
#                    the index endpoint is also down (a whole-API Radar outage —
#                    gating `not-indexed` on index==200 is what keeps a genuine
#                    outage distinguishable from node-not-indexed, per issue §5.2).
#
# Returns: 0 always.
# Portability: POSIX-ish Bash/zsh; no external processes.
# ─────────────────────────────────────────────────────────────────────────────
classify_obsrvr_radar() {
  local index_code="$1"
  local node_code="$2"
  local has_fields="$3"

  # Per-node 2xx: the node IS indexed; distinguish complete vs partial response.
  if [[ "$node_code" =~ ^2[0-9][0-9]$ ]]; then
    if [[ "$has_fields" == "true" ]]; then
      printf 'ok'
    else
      printf 'api-incomplete'
    fi
    return 0
  fi

  # Per-node 404 while the index endpoint is up (200): node absent from index.
  # This is the ONLY path to `not-indexed`; a 404 alongside a down index falls
  # through to `api-error` below (whole-API outage, not a node-registration gap).
  if [[ "$node_code" == "404" && "$index_code" == "200" ]]; then
    printf 'not-indexed'
    return 0
  fi

  # Everything else — timeouts, 5xx, 404-with-index-down — is a transient error.
  printf 'api-error'
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# eval_obsrvr_not_indexed_streak CLASSIFICATION STATE_FILE [THRESHOLD]
#
# Escalation bookkeeping for a PERSISTENT `not-indexed` condition from
# classify_obsrvr_radar (issue #3753). A persistent streak measures *Radar's
# crawl coverage*, not our node's health (9 of 12 healthy stellar-core peers are
# likewise absent from the index — issue §5.2), so the escalation is surfaced as
# an external-observability notice and MUST NOT flip the node-health banner.
#
# This function is the sole reader/writer of STATE_FILE (same idiom as
# classify_stuck_alive_sync). It counts consecutive `not-indexed` classifications
# and fires a ONE-SHOT escalation the first tick the streak reaches THRESHOLD,
# then stays silent while the streak persists. Any non-`not-indexed`
# classification resets both the counter and the one-shot marker, so a future
# streak can escalate again.
#
# Arguments:
#   CLASSIFICATION - one of the classify_obsrvr_radar literals.
#   STATE_FILE     - per-session scratch file (key=value lines).
#   THRESHOLD      - consecutive-tick count that triggers escalation
#                    (default 12 ≈ 4h at the current tick cadence; operator-retunable).
#
# Sets globals:
#   OBSRVR_STREAK   - current consecutive-not-indexed count (0 if reset).
#   OBSRVR_ESCALATE - `yes` on the single tick the streak first reaches
#                     THRESHOLD, `no` otherwise.
#
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
eval_obsrvr_not_indexed_streak() {
  local classification="$1"
  local state_file="$2"
  local threshold="${3:-12}"

  OBSRVR_STREAK=0
  OBSRVR_ESCALATE="no"

  # ── Read prior state (sole reader/writer). ─────────────────────────────────
  local streak=0 escalated="false"
  if [[ -f "$state_file" ]]; then
    local key val
    while IFS='=' read -r key val; do
      case "$key" in
        streak)    streak="$val" ;;
        escalated) escalated="$val" ;;
      esac
    done < "$state_file"
  fi
  [[ "$streak" =~ ^[0-9]+$ ]] || streak=0

  if [[ "$classification" == "not-indexed" ]]; then
    streak=$(( streak + 1 ))
    # One-shot: fire only on the tick the streak first reaches THRESHOLD.
    if [[ "$streak" -ge "$threshold" && "$escalated" != "true" ]]; then
      OBSRVR_ESCALATE="yes"
      escalated="true"
    fi
  else
    # Any other classification breaks the streak and clears the one-shot marker.
    streak=0
    escalated="false"
  fi

  OBSRVR_STREAK="$streak"

  printf 'streak=%s\nescalated=%s\n' "$streak" "$escalated" \
    > "$state_file" 2>/dev/null || true
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# prune_rotated_logs LOGS_DIR [KEEP_PER_CATEGORY]
#
# Rotated-log retention for monitor-tick check (5). Keeps the newest
# KEEP_PER_CATEGORY (default 3) `monitor.log.<category>-*` archives per
# category and deletes the rest, where the *category* is the suffix word
# between `monitor.log.` and the first `-` (e.g. `crashed`, `preredeploy`,
# `predeploy`, `coldcatchup`, `stopped`, `prerestart`, `freshstart`).
#
# Two fixes over the previous hardcoded `for pat in preredeploy crashed stuck
# frozen` + `sort -r` (filename) loop (#3616):
#   1. ALL categories are covered — legacy/other suffixes (`predeploy`,
#      `coldcatchup`, `stopped`, `prerestart`, `freshstart`, …) are discovered
#      from the files on disk, not from a hardcoded list, so they no longer
#      accumulate unbounded.
#   2. Retention is by **mtime**, not by lexical filename. An infixed variant
#      such as `monitor.log.crashed-knit2lcl-20260615T192932Z` sorts its 'k'
#      ahead of a bare `crashed-20260623T…` under `sort -r`, which retained the
#      OLD logs and deleted the recent ones. mtime ordering keeps the newest.
#
# Arguments:
#   LOGS_DIR           - directory containing monitor.log.<category>-* archives
#   KEEP_PER_CATEGORY  - newest-N to keep per category (default 3)
#
# Sets globals:
#   PRUNED_LOG_COUNT - number of files removed (0 if none / dir missing)
#
# Returns: 0 always (no-op on a missing dir or empty category set).
# Portability: GNU find -printf, GNU sort, GNU sed -E, Bash 4+.
# ─────────────────────────────────────────────────────────────────────────────
prune_rotated_logs() {
  local logs_dir="$1"
  local keep="${2:-3}"

  PRUNED_LOG_COUNT=0

  [[ -d "$logs_dir" ]] || return 0
  [[ "$keep" =~ ^[0-9]+$ ]] || keep=3

  # Enumerate every rotated archive as: <mtime-epoch> <category> <path>
  # category = first '-'-delimited token after the literal `monitor.log.` prefix.
  # find -printf survives zsh NO_NOMATCH (no shell glob) and gives us mtime.
  local rows
  rows=$(find "$logs_dir" -maxdepth 1 -type f -name 'monitor.log.*-*' \
           -printf '%T@ %f\n' 2>/dev/null)
  [[ -z "$rows" ]] && return 0

  # Build category list and, per category, sort newest-first by mtime, then
  # delete everything past the keep-th entry.
  local categories
  categories=$(printf '%s\n' "$rows" \
    | sed -E 's/^[0-9.]+ monitor\.log\.([^-]+)-.*$/\1/' \
    | sort -u)

  local cat line fname removed=0
  while IFS= read -r cat; do
    [[ -n "$cat" ]] || continue
    # mtime descending; ties broken by filename descending for determinism.
    # tail -n +<keep+1> drops the newest `keep`, leaving the surplus to delete.
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      fname="${line#* }"
      rm -f "$logs_dir/$fname" 2>/dev/null && removed=$(( removed + 1 ))
    done < <(printf '%s\n' "$rows" \
               | grep -E " monitor\\.log\\.${cat}-" \
               | sort -t' ' -k1,1rn -k2,2r \
               | tail -n +$(( keep + 1 )))
  done <<< "$categories"

  PRUNED_LOG_COUNT="$removed"
  return 0
}
