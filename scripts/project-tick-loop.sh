#!/usr/bin/env bash
# project-tick-loop.sh — endless worker that invokes /project-tick once per tick.
# Each tick picks ONE actionable issue from the henyey project board and
# dispatches the right specialist skill (/triage, /plan, /do, /review-pr).
# Multiple processes can run in parallel safely — concurrency via GitHub
# assignee race.
#
# Usage:
#   ./scripts/project-tick-loop.sh
#   LOOP_MODEL=claude-opus-4.7 ./scripts/project-tick-loop.sh
#
# Env vars:
#   LOOP_MODEL              AI model passed to copilot (default: claude-opus-4.7)
#   LOOP_EMPTY_SLEEP        Seconds to sleep when no actionable issue (default: 60)
#   LOOP_BETWEEN_SLEEP      Seconds to sleep between successful ticks (default: 30)
#   LOOP_FAILURE_SLEEP      Seconds to sleep after a tick exits non-zero (default: 120)
#   LOOP_LOG_DIR            Directory for per-run logs (default: ~/data/project-tick-loop)
#   LOOP_TICK_TIMEOUT       Max seconds per tick (default: 14400 = 4h)
#   LOOP_DRY_RUN            "1" to pass --dry-run to /project-tick

set -euo pipefail

LOOP_MODEL="${LOOP_MODEL:-claude-haiku-4.5}"
LOOP_EMPTY_SLEEP="${LOOP_EMPTY_SLEEP:-60}"
LOOP_BETWEEN_SLEEP="${LOOP_BETWEEN_SLEEP:-30}"
LOOP_FAILURE_SLEEP="${LOOP_FAILURE_SLEEP:-120}"
LOOP_LOG_DIR="${LOOP_LOG_DIR:-$HOME/data/project-tick-loop}"
LOOP_TICK_TIMEOUT="${LOOP_TICK_TIMEOUT:-14400}"
LOOP_DRY_RUN="${LOOP_DRY_RUN:-0}"

# Per-loop PID — exported so /project-tick's picker can scope its lost-race
# cooldown set to this loop (see issue #2822). Fresh on every loop start, so
# we also clean up any stale cooldown file from a previous run with the same
# PID (rare but possible after a long uptime).
export LOOP_PID=$$
rm -f "/tmp/project-tick-cooldown-${LOOP_PID}" 2>/dev/null || true

# Lock owner identity for /project-tick Step 4 (acquire-issue-lock.sh, #2917).
# Each tick is one long-lived `copilot` process (launched below); that process
# sources acquire-issue-lock.sh, which takes a host-local flock and holds the
# FD for the whole dispatch. The sentinel's `tick_pid=` field is keyed off
# `${TICK_PID:-$$}` inside the script, so with TICK_PID UNSET (#2948) it
# resolves to the tick's OWN `$$` — the actual flock holder — rather than this
# loop's pid. The loop therefore deliberately exports NO TICK_PID; the tick
# self-keys its sentinel identity. (A loop-supplied TICK_PID would be a stale
# liveness handle: the loop outlives any single tick, so a cross-host
# `kill -0 <loop_pid>` would stay "alive" after the real tick died.) Because the
# lock is FD-scoped the kernel releases it automatically when the copilot tick
# process exits or is killed — no manual reaping needed for prevention; the
# authoritative reaper identity is the self-recorded dispatch_pgid +
# dispatch_starttime (#2934/#2960), not this legacy field.

log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"; }

on_signal() {
  log "Received signal, shutting down."
  exit 0
}
trap on_signal INT TERM

# --- Preflight ---
command -v gh >/dev/null 2>&1 || { echo "ERROR: gh CLI not found" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "ERROR: jq not found" >&2; exit 1; }
command -v copilot >/dev/null 2>&1 || { echo "ERROR: copilot CLI not found" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "ERROR: gh not authenticated (run 'gh auth login')" >&2; exit 1; }

mkdir -p "$LOOP_LOG_DIR"

log "=== project-tick-loop ==="
log "Model:           $LOOP_MODEL"
log "Empty sleep:     ${LOOP_EMPTY_SLEEP}s"
log "Between sleep:   ${LOOP_BETWEEN_SLEEP}s"
log "Failure sleep:   ${LOOP_FAILURE_SLEEP}s"
log "Tick timeout:    ${LOOP_TICK_TIMEOUT}s"
log "Log dir:         $LOOP_LOG_DIR"
[[ "$LOOP_DRY_RUN" == "1" ]] && log "Dry run:         enabled"

# Tracks consecutive empty ticks for backoff-on-idle behavior.
consecutive_empty=0

while true; do
  ts="$(date +%Y%m%d-%H%M%S)"
  log_file="$LOOP_LOG_DIR/${ts}-tick.log"

  prompt="Run /project-tick"
  [[ "$LOOP_DRY_RUN" == "1" ]] && prompt="$prompt --dry-run"
  prompt="$prompt. Then exit."

  log "Tick start → $log_file"

  # Single-issue tick. copilot returns 0 on success (whether or not it picked
  # an issue); non-zero indicates a hard error (GH API failure, etc.).
  # Explicitly UNSET TICK_PID (#2948) so the dispatched copilot does NOT inherit
  # a loop-supplied value: acquire-issue-lock.sh's `${TICK_PID:-$$}` then falls
  # back to the tick's OWN `$$` (the actual flock holder) for the sentinel
  # `tick_pid=` field, instead of this loop's pid (a stale cross-host liveness
  # handle). The unset is explicit rather than a bare omission so any ambient
  # TICK_PID inherited into THIS loop's environment cannot leak into copilot and
  # defeat the fallback. The authoritative same-host guard is the FD-scoped
  # flock taken inside copilot; the reaper identity is the self-recorded
  # dispatch_pgid + dispatch_starttime (#2960), not this best-effort field.
  unset TICK_PID
  tick_exit=0

  # Launch the dispatch as its OWN process-group leader (#2934/#2957). reap-
  # stale-dispatch.sh reaps a dead prior dispatch by `kill -- -<pgid>`; that is
  # only safe if the recorded pgid covers JUST the copilot dispatch tree and not
  # this loop or unrelated siblings. `setsid` puts copilot in a fresh session /
  # process group, so acquire-issue-lock.sh (sourced INSIDE that copilot tree)
  # self-records `/proc/self`'s pgid as the dispatch group leader — no fragile
  # post-spawn env handoff to an already-exec'd child (#2956).
  #
  # Order matters: `setsid --wait timeout … copilot`, NOT
  # `timeout … setsid --wait copilot`. If `timeout` were the OUTER command it
  # would supervise the `setsid` wrapper, but `setsid` forks `copilot` into a
  # NEW session/process-group; on timeout the signal reaches the wrapper (or its
  # group) while the detached `copilot` tree keeps running — exactly the orphan
  # this PR exists to prevent (Copilot review #2960). With `timeout` INSIDE the
  # `setsid` session, `timeout` is the session/group leader and `copilot` is its
  # child in the SAME group, so `timeout` directly supervises and terminates the
  # copilot tree, and the self-recorded pgid (from `/proc/self` inside copilot)
  # still covers just this dispatch group. `setsid --wait` blocks until that
  # session leader (`timeout`) exits, so the propagated exit status is the real
  # dispatch's (124 on timeout — #2957), not a short-lived wrapper's. We
  # background it and `wait` so the trap-based signal handling stays responsive.
  setsid --wait timeout "$LOOP_TICK_TIMEOUT" copilot \
    --model "$LOOP_MODEL" \
    --autopilot \
    --allow-all-tools \
    --allow-all-paths \
    -p "$prompt" \
    >>"$log_file" 2>&1 &
  dispatch_pid=$!
  wait "$dispatch_pid" || tick_exit=$?

  if [[ $tick_exit -eq 124 ]]; then
    log "Tick TIMED OUT after ${LOOP_TICK_TIMEOUT}s. Sleeping ${LOOP_FAILURE_SLEEP}s."
    sleep "$LOOP_FAILURE_SLEEP"
    continue
  fi

  if [[ $tick_exit -ne 0 ]]; then
    log "Tick exited $tick_exit. Sleeping ${LOOP_FAILURE_SLEEP}s."
    sleep "$LOOP_FAILURE_SLEEP"
    continue
  fi

  # Detect "no actionable issues" — /project-tick emits that exact string when
  # the board is drained. Use it to back off on idle.
  if grep -q "no actionable issues" "$log_file" 2>/dev/null; then
    consecutive_empty=$((consecutive_empty + 1))
    sleep_for="$LOOP_EMPTY_SLEEP"
    # Backoff after sustained idleness: 1x, 1x, 2x, 4x, 8x, capped at 16x.
    case "$consecutive_empty" in
      1|2) ;;
      3)   sleep_for=$((LOOP_EMPTY_SLEEP * 2)) ;;
      4)   sleep_for=$((LOOP_EMPTY_SLEEP * 4)) ;;
      5)   sleep_for=$((LOOP_EMPTY_SLEEP * 8)) ;;
      *)   sleep_for=$((LOOP_EMPTY_SLEEP * 16)) ;;
    esac
    log "Board idle (consecutive=$consecutive_empty). Sleeping ${sleep_for}s."
    sleep "$sleep_for"
    continue
  fi

  consecutive_empty=0
  log "Tick OK. Sleeping ${LOOP_BETWEEN_SLEEP}s."
  sleep "$LOOP_BETWEEN_SLEEP"
done
