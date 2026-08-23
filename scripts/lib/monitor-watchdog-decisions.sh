#!/usr/bin/env bash
#
# Pure decision logic for the monitor-loop crontab watchdog
# (scripts/monitor-watchdog.sh).
#
# Mirrors the testability pattern of scripts/lib/monitor-decisions.sh: the
# watchdog wrapper owns all I/O (locks, launching the headless tick, writing
# the refire marker, logging); the two functions here are pure and fully unit-
# testable (scripts/test-monitor-watchdog.sh).
#
# Requires: Bash 3.2+, GNU/Linux (GNU `date -d`, `tail`, `grep -oE`, `sed -E`).
# Does NOT set shell options (set -e, -u) — callers control strictness.
# Idempotent: safe to source multiple times.
#

[[ -n "${_MONITOR_WATCHDOG_DECISIONS_LOADED:-}" ]] && return 0
_MONITOR_WATCHDOG_DECISIONS_LOADED=1

# ─────────────────────────────────────────────────────────────────────────────
# watchdog_last_epoch HIST_FILE
#
# Resolve the completion time of the last tick recorded in HIST_FILE (a JSONL
# tick history; the timestamp lives on the LAST line under key `ts` OR
# `timestamp`).
#
# Echoes EXACTLY one token on stdout:
#   MISSING      — HIST_FILE does not exist. A dark/bootstrapping loop; the
#                  caller SHOULD fire (this is the one fail-OPEN case).
#   PARSE_ERROR  — HIST_FILE exists but its last line has no parseable
#                  `ts`/`timestamp` value (not JSON, missing key, or a value
#                  GNU `date` cannot interpret). The caller MUST fail CLOSED:
#                  do NOT fire and do NOT write the refire marker (#3789). An
#                  unparseable tail is NOT evidence the loop is dead — treating
#                  it as epoch 0 (the old behavior) fired unconditionally and
#                  produced a 56,590-year "staleness" (#3757).
#   <integer>    — epoch seconds of the last completed tick.
#
# Dependency-light on purpose: uses only `tail`/`grep`/`sed`/`date` — never
# python3 or jq. A missing interpreter on cron's minimal PATH must not be able
# to silently blind the watchdog (dark-monitoring incident 2026-07-22).
#
# Accepts both `ts` (henyey monitor loop) and `timestamp` (alternate producers)
# keys; the first of either found on the last line wins.
#
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
watchdog_last_epoch() {
  local hist="$1"
  [ -f "$hist" ] || { printf 'MISSING\n'; return 0; }

  local line ts epoch
  line=$(tail -n 1 "$hist" 2>/dev/null)

  # Isolate the first "ts":"..." or "timestamp":"..." pair, then strip the
  # key/colon/quote scaffolding. Value may itself contain colons (ISO-8601
  # time), so peel the fixed prefix/suffix rather than splitting on ':'.
  ts=$(printf '%s' "$line" \
        | grep -oE '"(ts|timestamp)"[[:space:]]*:[[:space:]]*"[^"]*"' \
        | head -n 1 \
        | sed -E 's/^"[^"]*"[[:space:]]*:[[:space:]]*"//; s/"$//')

  if [ -z "$ts" ]; then
    printf 'PARSE_ERROR\n'
    return 0
  fi

  epoch=$(date -u -d "$ts" +%s 2>/dev/null)
  case "$epoch" in
    ''|*[!0-9]*) printf 'PARSE_ERROR\n'; return 0 ;;
  esac

  printf '%s\n' "$epoch"
  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# watchdog_should_fire NOW LAST_TOKEN STALE_SECS LAST_FIRE REFIRE_COOLDOWN
#
# Pure staleness + cooldown decision. Reads and writes NO files; all times are
# passed in as epoch seconds so the whole matrix is deterministically testable.
#
#   NOW              — current epoch seconds.
#   LAST_TOKEN       — output of watchdog_last_epoch (an epoch, MISSING, or
#                      PARSE_ERROR).
#   STALE_SECS       — fire only if the last completed tick is older than this.
#   LAST_FIRE        — epoch of the last watchdog firing (0 if never / unknown).
#   REFIRE_COOLDOWN  — never fire twice within this many seconds.
#
# Echoes EXACTLY one line "VERDICT REASON" on stdout:
#   fire no-history  — no history file: dark loop / bootstrap (fail open).
#   fire stale       — last tick older than STALE_SECS and cooldown elapsed.
#   skip fresh       — last tick within STALE_SECS.
#   skip cooldown    — stale, but a firing happened within REFIRE_COOLDOWN.
#   skip parse-error — history tail unparseable: FAIL CLOSED (do not fire,
#                      do not write the marker).
#
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
watchdog_should_fire() {
  local now="$1" last_token="$2" stale_secs="$3" last_fire="$4" cooldown="$5"

  # Fail closed on an unparseable tail — highest precedence.
  if [ "$last_token" = "PARSE_ERROR" ]; then
    printf 'skip parse-error\n'
    return 0
  fi

  # Staleness.
  local stale=no
  if [ "$last_token" = "MISSING" ]; then
    stale=yes
  elif [ $(( now - last_token )) -gt "$stale_secs" ]; then
    stale=yes
  fi

  if [ "$stale" = no ]; then
    printf 'skip fresh\n'
    return 0
  fi

  # Stale — apply the refire cooldown.
  case "$last_fire" in ''|*[!0-9]*) last_fire=0 ;; esac
  if [ "$last_fire" -gt 0 ] && [ $(( now - last_fire )) -lt "$cooldown" ]; then
    printf 'skip cooldown\n'
    return 0
  fi

  if [ "$last_token" = "MISSING" ]; then
    printf 'fire no-history\n'
  else
    printf 'fire stale\n'
  fi
  return 0
}
