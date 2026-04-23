#!/bin/bash
# Monitor Validator Script
# Launches /monitor-loop via Copilot CLI, then ticks /monitor-tick on a loop.
#
# Usage:
#   scripts/monitor-validator.sh [--watcher] [--interval MINUTES]
#
# Options:
#   --watcher     Run in watcher mode (read-only, no consensus)
#   --interval N  Minutes between ticks (default: 10)
#   --help        Show this help message

set -euo pipefail

# ── Configuration ────────────────────────────────────────────────────
MODE_FLAG=""
INTERVAL_MINUTES=10
MODEL="gpt-5.4"
LOG_DIR="$HOME/data/monitor"
LAUNCHER_LOG="$LOG_DIR/monitor-launcher.log"
TICK_TIMEOUT=1800  # 30 minutes per tick
MAX_TICK_LOGS=50
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Argument parsing ────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --watcher)
            MODE_FLAG="--watcher"
            shift
            ;;
        --interval)
            INTERVAL_MINUTES="${2:?--interval requires a number}"
            shift 2
            ;;
        --help|-h)
            sed -n '2,/^$/{ s/^# //; s/^#//; p }' "$0"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

# ── Setup ────────────────────────────────────────────────────────────
mkdir -p "$LOG_DIR"

log() {
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $1" | tee -a "$LAUNCHER_LOG"
}

prune_tick_logs() {
    local count
    count=$(find "$LOG_DIR" -name 'tick-*.log' -type f | wc -l)
    if (( count > MAX_TICK_LOGS )); then
        find "$LOG_DIR" -name 'tick-*.log' -type f -printf '%T@ %p\n' \
            | sort -n | head -n $(( count - MAX_TICK_LOGS )) \
            | cut -d' ' -f2- | xargs rm -f
    fi
}

cleanup() {
    log "Received signal — shutting down."
    exit 0
}
trap cleanup SIGINT SIGTERM

# ── Phase 1: monitor-loop (one-time setup) ───────────────────────────
log "═══ MONITOR LAUNCHER STARTED ═══"
log "Mode:     ${MODE_FLAG:-validator}"
log "Interval: ${INTERVAL_MINUTES}m"
log "Repo:     $REPO_ROOT"

LOOP_LOG="$LOG_DIR/loop-$(date -u '+%Y%m%dT%H%M%SZ').log"
log "Running /monitor-loop (setup + first tick)..."
log "  Log: $LOOP_LOG"

cd "$REPO_ROOT"
if ! copilot -p "/monitor-loop $MODE_FLAG" --model "$MODEL" --allow-all 2>&1 | tee "$LOOP_LOG"; then
    log "ERROR: /monitor-loop failed. See $LOOP_LOG"
    exit 1
fi
log "/monitor-loop completed successfully."

# Verify env file was written
if [[ ! -f "$HOME/data/monitor-loop.env" ]]; then
    log "ERROR: monitor-loop.env not found after /monitor-loop — setup incomplete."
    exit 1
fi

# ── Phase 2: tick loop ───────────────────────────────────────────────
log "Starting tick loop (every ${INTERVAL_MINUTES}m)..."

run_tick() {
    TICK_LOG="$LOG_DIR/tick-$(date -u '+%Y%m%dT%H%M%SZ').log"
    log "Running /monitor-tick..."

    cd "$REPO_ROOT"
    if timeout "$TICK_TIMEOUT" copilot -p "/monitor-tick" --model "$MODEL" --allow-all 2>&1 | tee "$TICK_LOG"; then
        log "/monitor-tick completed. Log: $TICK_LOG"
    else
        EXIT_CODE=$?
        if (( EXIT_CODE == 124 )); then
            log "WARNING: /monitor-tick timed out after ${TICK_TIMEOUT}s. Log: $TICK_LOG"
        else
            log "WARNING: /monitor-tick failed (exit $EXIT_CODE). Log: $TICK_LOG"
        fi
    fi

    prune_tick_logs
}

# First tick runs immediately
run_tick

while true; do
    log "Sleeping ${INTERVAL_MINUTES}m until next tick..."
    sleep $(( INTERVAL_MINUTES * 60 ))
    run_tick
done
