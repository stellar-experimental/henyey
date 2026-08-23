#!/usr/bin/env bash
# Shared post-condition detection for the crontab watchdogs (#3780).
#
# Both monitor-watchdog.sh and project-loop-watchdog.sh launch a headless
# `claude -p` tick when their in-session loop looks dead. The defect this lib
# fixes: `exit=$?` is the CLI's exit status, not the tick's. A refusal to run
# (org spend limit, auth failure, "not logged in") exits 0 and is
# indistinguishable from a completed tick — on 2026-07-28/29 this produced 24h
# of dark monitoring logged as 49 "successful" ticks. The only true success
# signal is a NEW row in tick-history.jsonl.
#
# This file is SOURCE-SAFE: it defines functions and sets a few overridable
# defaults, with no other top-level side effects (no `set -e`, no output), so
# it can be sourced into scripts that manage their own shell options.
#
# Canonical copy: scripts/lib/watchdog-postcondition.sh in the henyey repo.
# The live crontab copies of the watchdogs must be re-synced from the fixed
# canonical scripts (and this lib copied alongside) after merge.

# Any invocation completing faster than this floor is treated as not-a-real-tick
# even if a row appears to have advanced (the incident's spend-limit no-ops ran
# 1–3 s; real ticks ran p50 372 s). Overridable so tests can lower it.
: "${WATCHDOG_DUR_FLOOR:=30}"

# Signatures that make `claude -p` exit 0 without running a tick. Case-insensitive
# ERE. Overridable so operators can extend it without editing this file.
: "${WATCHDOG_REFUSAL_RE:=spend limit|usage-credits|Invalid API key|authentication|not logged in|please run .*login|rate limit|quota}"

# watchdog_hist_count HIST_FILE
# Echo the number of rows (lines) in HIST_FILE, or 0 if it is missing/empty.
watchdog_hist_count() {
  local f="${1:-}"
  if [ -n "$f" ] && [ -f "$f" ]; then
    # `wc -l < file` avoids printing the filename; trim any padding.
    wc -l < "$f" 2>/dev/null | tr -d '[:space:]'
  else
    echo 0
  fi
}

# watchdog_classify_outcome PRE POST DURATION_SECS [OUTPUT_FILE]
# Pure classifier. Echoes exactly one of:
#   success       — a new tick-history row appeared AND duration >= floor
#   suspect-fast  — a new row appeared but under the duration floor (do NOT
#                   treat as success; the STALE/ALIVE gates prevent
#                   double-dispatch, so leaving the cooldown unburned is safe)
#   noop-refusal  — no new row AND the captured output matches a known refusal
#   noop-empty    — no new row and no recognized reason
watchdog_classify_outcome() {
  local pre="${1:-0}" post="${2:-0}" dur="${3:-0}" out="${4:-}"
  local floor="${WATCHDOG_DUR_FLOOR:-30}"

  # Guard against non-numeric input so the comparisons below never error.
  case "$pre$post$dur$floor" in *[!0-9]*) ;; esac

  if [ "${post:-0}" -gt "${pre:-0}" ] 2>/dev/null; then
    if [ "${dur:-0}" -ge "${floor:-30}" ] 2>/dev/null; then
      echo success
    else
      echo suspect-fast
    fi
    return 0
  fi

  if [ -n "$out" ] && [ -f "$out" ] && grep -Eiq -- "$WATCHDOG_REFUSAL_RE" "$out" 2>/dev/null; then
    echo noop-refusal
  else
    echo noop-empty
  fi
}
