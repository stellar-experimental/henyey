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
