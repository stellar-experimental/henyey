#!/usr/bin/env bash
# Monitor-loop watchdog — crontab backstop for the in-session /monitor-tick loop.
#
# The primary loop runs inside an interactive Claude Code session via
# ScheduleWakeup (~20 min cadence). That loop can silently die (session
# compaction, consumed wakeups, session exit). Audit 2026-07-16 found 571
# dead-hours across 64 days (33 gaps >1h). This script runs from crontab
# every 15 min and launches a headless tick ONLY when the loop looks dead.
#
# Post-condition (#3780): `claude -p` exit status is the CLI's, not the tick's.
# A refusal to run (org monthly spend limit, auth failure) exits 0 and used to
# be logged as a successful tick — on 2026-07-28/29 this produced 24h of dark
# monitoring logged as 49 "successful" ticks. The only true success signal is a
# NEW tick-history.jsonl row. We now capture the row count and duration around
# the launch, classify the outcome via scripts/lib/watchdog-postcondition.sh,
# write the refire-cooldown MARKER ONLY on a real success (so a no-op does not
# burn the cooldown), and emit a greppable ESCALATION line after N consecutive
# failed launches.
#
# Canonical copy: scripts/monitor-watchdog.sh in the henyey repo.
# Live copy: /home/tomer/data/monitor-watchdog.sh (what crontab executes,
# decoupled from repo checkout state). After changing this script, re-sync the
# live copy AND scripts/lib/watchdog-postcondition.sh alongside it.
#
# Install: */15 * * * * /home/tomer/data/monitor-watchdog.sh
set -u

# cron runs with a minimal PATH (/usr/bin:/bin) that omits ~/.local/bin, where
# the `claude` launcher lives. Without this the headless tick fails with
# "timeout: failed to run command 'claude': No such file or directory" and the
# watchdog silently no-ops every 15 min (dark-monitoring incident 2026-07-22).
export PATH="/home/tomer/.local/bin:$PATH"

# Paths/binaries are env-overridable (with the production values as defaults) so
# tests can point them at a ~/data fixture; production behavior is unchanged.
: "${CLAUDE_BIN:=/home/tomer/.local/bin/claude}"
: "${SESSION_ID:=74535976}"
: "${DATA:=/home/tomer/data}"
: "${SESS:=$DATA/$SESSION_ID}"
: "${HIST:=$SESS/tick-history.jsonl}"
: "${REPO:=/home/tomer/henyey-1}"
ALIVE="$SESS/.alive"
LOCK="$DATA/monitor-watchdog.lock"
MARKER="$DATA/monitor-watchdog.lastfire"
LOG="$DATA/monitor-watchdog.log"
FAILSTREAK="$DATA/monitor-watchdog.failstreak"
CAP="$DATA/monitor-watchdog.lastcapture"

STALE_SECS=1800        # fire when last completed tick is older than 30 min
ALIVE_FRESH_SECS=600   # skip if a tick STARTED within the last 10 min (in-flight)
REFIRE_COOLDOWN=1800   # never fire more than once per 30 min
TICK_TIMEOUT=1500      # cap a headless tick at 25 min
: "${ESCALATE_AFTER:=3}"  # greppable escalation after N consecutive failed launches

# Shared post-condition detection (row-count + duration + refusal classifier).
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
# to be written pre-tick, so a 1-second no-op burned the full 30-min cooldown.
# It is now written only after the post-condition confirms a real tick ran, so
# a failed launch can be retried at the next */15 slot.

# Keep the log bounded (~5 MB).
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG")" -gt 5242880 ]; then
  tail -c 1048576 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

echo "$(date -u +%FT%TZ) watchdog: loop stale ($((now - last_epoch))s since last tick) — launching headless tick" >> "$LOG"
cd "$REPO" || exit 1

# Capture the pre-tick row count and the invocation output, then classify.
pre=$(watchdog_hist_count "$HIST")
start=$(date -u +%s)
timeout "$TICK_TIMEOUT" "$CLAUDE_BIN" --model claude-opus-4-8 --dangerously-skip-permissions \
  -p '/monitor-tick' > "$CAP" 2>&1
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
    echo "$(date -u +%FT%TZ) watchdog: ESCALATION $fs consecutive failed launches (last outcome=$outcome) — monitor loop is dark and headless recovery is not working" >> "$LOG"
  fi
fi
