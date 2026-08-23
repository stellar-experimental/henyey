#!/usr/bin/env bash
# Monitor-loop watchdog — crontab backstop for the in-session /monitor-tick loop.
#
# The primary loop runs inside an interactive Claude Code session via
# ScheduleWakeup (~20 min cadence). That loop can silently die (session
# compaction, consumed wakeups, session exit). Audit 2026-07-16 found 571
# dead-hours across 64 days (33 gaps >1h). This script runs from crontab
# every 15 min and launches a headless tick ONLY when the loop looks dead.
#
# Canonical copy: scripts/monitor-watchdog.sh in the henyey repo (this file).
# Live copy: /home/tomer/data/monitor-watchdog.sh (what crontab executes,
# decoupled from repo checkout state — deploy by copying this file there).
#
# Install: */15 * * * * /home/tomer/data/monitor-watchdog.sh
#
# Decision logic (staleness, cooldown, tail parsing) lives in the pure,
# unit-tested lib scripts/lib/monitor-watchdog-decisions.sh (mirrors the
# monitor-decisions.sh testability pattern). This wrapper owns all I/O: the
# self-serialization lock, the shared whole-tick lock, the refire marker, the
# log, and the headless launch. Covered by scripts/test-monitor-watchdog.sh.
#
# Staleness rule (corrected — #3789): the watchdog measures COMPLETION-to-now,
# so it fires whenever `wakeup + tick_duration > STALE_SECS`. STALE_SECS must
# therefore satisfy `expected_wakeup <= STALE_SECS - expected_tick_duration`,
# NOT merely "keep wakeups under STALE_SECS". With the loop's observed ~30-min
# idle cadence plus multi-minute ticks, the old 1800s bar fired on healthy but
# slow cycles (#3757: the watchdog fired a duplicate headless tick on nearly
# every cron cycle). STALE_SECS=4200 (70 min) gives ~40 min of headroom.
set -u

# cron runs with a minimal PATH (/usr/bin:/bin) that omits ~/.local/bin, where
# the `claude` launcher lives. Without this the headless tick fails with
# "timeout: failed to run command 'claude': No such file or directory" and the
# watchdog silently no-ops every 15 min (dark-monitoring incident 2026-07-22).
export PATH="/home/tomer/.local/bin:$PATH"

# Source the pure decision lib relative to this script (works regardless of the
# cwd cron invokes us from).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/monitor-watchdog-decisions.sh
source "$SCRIPT_DIR/lib/monitor-watchdog-decisions.sh"

# ── Config — production defaults, all env-overridable (the test harness and any
# alternate deployment retarget these without editing the script). ────────────
CLAUDE_BIN="${CLAUDE_BIN:-/home/tomer/.local/bin/claude}"
CLAUDE_MODEL="${WATCHDOG_CLAUDE_MODEL:-claude-opus-4-8}"
DATA="${WATCHDOG_DATA:-/home/tomer/data}"
ENV_FILE="${WATCHDOG_ENV_FILE:-$DATA/monitor-loop.env}"
REPO="${WATCHDOG_REPO:-/home/tomer/henyey-1}"

# SESSION_ID: prefer an explicit MONITOR_SESSION_ID, else read it from
# monitor-loop.env, else the historical production default. Keeps the watchdog
# and the loop pointed at the same session dir even across loop restarts.
SESSION_ID="${MONITOR_SESSION_ID:-}"
if [ -z "$SESSION_ID" ] && [ -f "$ENV_FILE" ]; then
  SESSION_ID=$(grep -E '^MONITOR_SESSION_ID=' "$ENV_FILE" 2>/dev/null | tail -n 1 | cut -d= -f2-)
fi
SESSION_ID="${SESSION_ID:-74535976}"

SESS="$DATA/$SESSION_ID"
HIST="${WATCHDOG_HIST:-$SESS/tick-history.jsonl}"
LOCK="${WATCHDOG_LOCK:-$DATA/monitor-watchdog.lock}"        # self-serialization
TICK_LOCK="${WATCHDOG_TICK_LOCK:-$DATA/monitor-tick.lock}"  # shared whole-tick
MARKER="${WATCHDOG_MARKER:-$DATA/monitor-watchdog.lastfire}"
LOG="${WATCHDOG_LOG:-$DATA/monitor-watchdog.log}"

STALE_SECS="${WATCHDOG_STALE_SECS:-4200}"        # fire when last COMPLETED tick
                                                 # is older than 70 min
REFIRE_COOLDOWN="${WATCHDOG_REFIRE_COOLDOWN:-1800}"  # >= 1 firing / 30 min
TICK_TIMEOUT="${WATCHDOG_TICK_TIMEOUT:-1500}"    # cap a headless tick at 25 min

mkdir -p "$DATA" 2>/dev/null || true

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$1" >> "$LOG"; }

# ── Self-serialize watchdog instances (fd 9). ────────────────────────────────
exec 9>"$LOCK"
flock -n 9 || exit 0

now=$(date -u +%s)

# ── Decide (pure). ───────────────────────────────────────────────────────────
last_token=$(watchdog_last_epoch "$HIST")

last_fire=0
if [ -f "$MARKER" ]; then
  last_fire=$(cat "$MARKER" 2>/dev/null || echo 0)
  case "$last_fire" in ''|*[!0-9]*) last_fire=0 ;; esac
fi

decision=$(watchdog_should_fire "$now" "$last_token" "$STALE_SECS" "$last_fire" "$REFIRE_COOLDOWN")
verdict="${decision%% *}"
reason="${decision#* }"

if [ "$verdict" != "fire" ]; then
  # Fail CLOSED on an unparseable tail: log LOUDLY (an unparseable history is a
  # real anomaly an operator should see) and exit WITHOUT writing the marker
  # (#3789). fresh/cooldown skips are the quiet common case.
  if [ "$reason" = "parse-error" ]; then
    log "WARNING: history tail unparseable ($HIST) — failing CLOSED, NOT firing. Inspect the loop's liveness manually."
  fi
  exit 0
fi

# ── In-flight guard (fd 8): take the shared whole-tick lock. If an in-session
# tick (which takes the same lock around its metrics scrape/archive critical
# section) or a prior headless firing holds it, a tick is in flight — skip.
# Held across the headless launch below so the child inherits it and the
# in-session check-12 serializes behind us. This REPLACES the old .alive-mtime
# heuristic, closing the interleaving race that corrupted archive metadata.env
# (#3757 / #3789). ─────────────────────────────────────────────────────────────
exec 8>"$TICK_LOCK"
if ! flock -n 8; then
  log "skipped: tick in flight"
  exit 0
fi

# Committed to firing — record the marker (only reached past the fail-closed
# and in-flight guards, so a PARSE_ERROR skip never writes it).
printf '%s\n' "$now" > "$MARKER"

# Keep the log bounded (~5 MB).
if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -gt 5242880 ]; then
  tail -c 1048576 "$LOG" > "$LOG.tmp" && mv "$LOG.tmp" "$LOG"
fi

log "watchdog: loop stale (reason=$reason, last=$last_token, now=$now) — launching headless tick"
cd "$REPO" || { log "watchdog: repo $REPO missing — aborting firing"; exit 1; }
timeout "$TICK_TIMEOUT" "$CLAUDE_BIN" --model "$CLAUDE_MODEL" --dangerously-skip-permissions \
  -p '/monitor-tick' >> "$LOG" 2>&1
rc=$?
log "watchdog: headless tick exit=$rc"
