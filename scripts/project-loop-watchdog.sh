#!/usr/bin/env bash
# Project-loop watchdog — crontab backstop for the in-session project-management
# board watcher (the /loop-driven "project-loop" heartbeat).
#
# The primary loop runs inside an interactive Claude Code session via
# ScheduleWakeup (~25-30 min cadence when idle). That loop can silently die
# (session compaction, consumed wakeups, session exit) with no signal to the
# operator — this exact failure mode caused a 5-day dark gap 2026-07-17 to
# 2026-07-22 during which three new issues went untriaged and a disk-usage
# escalation went unreported. This script runs from crontab every 15 min and
# launches a single headless board-tick ONLY when the loop looks dead.
#
# Modeled on scripts/monitor-watchdog.sh (same staleness/cooldown/lock
# mechanics), adapted for two differences from the monitor loop:
#   1. The project loop has no existing liveness-marker contract, so this
#      watchdog and the loop's own carried ScheduleWakeup prompt jointly
#      define one here (.alive touched at tick start, tick-history.jsonl
#      appended at tick end) under a fixed directory, not a per-session one
#      (the loop's underlying Claude session ID changes across restarts; the
#      logical loop's liveness state must not).
#   2. Rather than reusing a long-lived local checkout (which can be stale
#      relative to origin/main — see #3620), each firing clones a fresh
#      shallow checkout, runs the headless tick from it, and deletes it
#      afterward, so the headless dispatch always runs the current skill
#      content and never grows disk usage between firings.
#
# Canonical copy: scripts/project-loop-watchdog.sh in the henyey repo.
# Live copy: /home/tomer/data/project-loop-watchdog.sh (what crontab
# executes, decoupled from repo checkout state).
#
# Install: */15 * * * * /home/tomer/data/project-loop-watchdog.sh
set -u

# cron runs with a minimal PATH (/usr/bin:/bin) that omits ~/.local/bin, where
# the `claude` launcher lives (see monitor-watchdog.sh's 2026-07-22 incident).
export PATH="/home/tomer/.local/bin:$PATH"

# Paths/binaries are env-overridable (with the production values as defaults) so
# tests can point them at a ~/data fixture; production behavior is unchanged.
: "${CLAUDE_BIN:=/home/tomer/.local/bin/claude}"
: "${DATA:=/home/tomer/data}"
: "${SESS:=$DATA/project-loop}"
: "${HIST:=$SESS/tick-history.jsonl}"
: "${REMOTE:=https://github.com/stellar-experimental/henyey.git}"
ALIVE="$SESS/.alive"
LOCK="$DATA/project-loop-watchdog.lock"
MARKER="$DATA/project-loop-watchdog.lastfire"
LOG="$DATA/project-loop-watchdog.log"
FAILSTREAK="$DATA/project-loop-watchdog.failstreak"
CAP="$DATA/project-loop-watchdog.lastcapture"
SCRATCH="$DATA/project-loop-watchdog-scratch"

STALE_SECS=2700         # fire when last completed tick is older than 45 min
                        # (loop's own idle cadence widens to ~30 min; 1.5x that)
ALIVE_FRESH_SECS=1800   # skip if a tick STARTED within the last 30 min (in-flight;
                        # some specialist dispatches run 10+ min, so this is
                        # generous to avoid double-dispatching a live session)
REFIRE_COOLDOWN=1800    # never fire more than once per 30 min
TICK_TIMEOUT=1800       # cap a headless tick at 30 min
: "${ESCALATE_AFTER:=3}" # greppable escalation after N consecutive failed launches

# Shared post-condition detection (#3780) — same classifier as monitor-watchdog.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/watchdog-postcondition.sh
. "${WATCHDOG_LIB:-$SCRIPT_DIR/lib/watchdog-postcondition.sh}"

# Serialize watchdog instances.
exec 9>"$LOCK"
flock -n 9 || exit 0

now=$(date -u +%s)

# 1) Last COMPLETED tick (history line ts). Missing/unparseable => epoch 0 (fire).
last_epoch=0
if [ -f "$HIST" ]; then
  last_ts=$(tail -1 "$HIST" | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("ts",""))
except Exception: print("")' 2>/dev/null)
  [ -n "$last_ts" ] && last_epoch=$(date -u -d "$last_ts" +%s 2>/dev/null || echo 0)
fi
[ $(( now - last_epoch )) -le "$STALE_SECS" ] && exit 0

# 2) A tick touches .alive at START — if fresh, one is in flight; don't collide.
if [ -f "$ALIVE" ]; then
  alive_age=$(( now - $(stat -c %Y "$ALIVE") ))
  [ "$alive_age" -le "$ALIVE_FRESH_SECS" ] && exit 0
fi

# 3) Refire cooldown.
if [ -f "$MARKER" ]; then
  last_fire=$(cat "$MARKER" 2>/dev/null || echo 0)
  [ $(( now - last_fire )) -lt "$REFIRE_COOLDOWN" ] && exit 0
fi
# NOTE: the cooldown MARKER is intentionally NOT written here (#3780). It used
# to be written pre-tick, so a launch that never ran a real tick (spend limit,
# auth failure — both make `claude -p` exit 0) burned the full 30-min cooldown.
# It is now written only after the post-condition confirms a real tick ran.

# Keep the log bounded (~5 MB).
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG")" -gt 5242880 ]; then
  tail -c 1048576 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

echo "$(date -u +%FT%TZ) watchdog: loop stale ($((now - last_epoch))s since last tick) — launching headless tick" >> "$LOG"

# Fresh shallow clone each firing — never reuse a long-lived checkout that
# could be stale relative to origin/main (#3620), and never grow disk usage
# between firings (deleted at the end regardless of outcome).
rm -rf "$SCRATCH"
if ! git clone --quiet --depth 1 "$REMOTE" "$SCRATCH" >> "$LOG" 2>&1; then
  echo "$(date -u +%FT%TZ) watchdog: clone failed, aborting this firing" >> "$LOG"
  exit 1
fi

# Capture the pre-tick row count and the invocation output, then classify the
# outcome (#3780) — a new tick-history.jsonl row is the only true success signal.
pre=$(watchdog_hist_count "$HIST")
start=$(date -u +%s)
cd "$SCRATCH" || exit 1
timeout "$TICK_TIMEOUT" "$CLAUDE_BIN" --model claude-opus-4-8 --dangerously-skip-permissions \
  -p '/project-tick' > "$CAP" 2>&1
rc=$?
dur=$(( $(date -u +%s) - start ))
post=$(watchdog_hist_count "$HIST")
cat "$CAP" >> "$LOG"   # preserve full diagnosability in the watchdog log
outcome=$(watchdog_classify_outcome "$pre" "$post" "$dur" "$CAP")
rm -f "$CAP"
echo "$(date -u +%FT%TZ) watchdog: headless tick outcome=$outcome dur=${dur}s rows=$((post - pre)) exit=$rc" >> "$LOG"

if [ "$outcome" = "success" ]; then
  printf '%s\n' "$now" > "$MARKER"   # burn the cooldown only on a real tick
  printf '0\n' > "$FAILSTREAK"
else
  fs=$(cat "$FAILSTREAK" 2>/dev/null || echo 0)
  case "$fs" in ''|*[!0-9]*) fs=0 ;; esac
  fs=$((fs + 1))
  printf '%s\n' "$fs" > "$FAILSTREAK"
  if [ "$fs" -ge "$ESCALATE_AFTER" ]; then
    echo "$(date -u +%FT%TZ) watchdog: ESCALATION $fs consecutive failed launches (last outcome=$outcome) — project loop is dark and headless recovery is not working" >> "$LOG"
  fi
fi

cd "$DATA" || true
rm -rf "$SCRATCH"
