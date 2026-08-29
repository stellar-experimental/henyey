#!/usr/bin/env bash
# quickstart-hang-watchdog.sh — DIAGNOSTIC instrumentation for the testnet
# quickstart "Run probes through wrapper" step hang (#3286).
#
# This script does NOT fix the hang. Its deliverable is DATA from the next
# hang: process-tree + open-fd snapshots written to the diagnostics dir
# BEFORE the testnet step's tight `timeout-minutes` kill fires, so the
# subsequent "Upload diagnostics" step sweeps them into the uploaded artifact.
#
# Two wrapper-level fixes (#3273 `timeout -k`, #3287 `> file` redirect) both
# failed to bound the ~55-min step-8 hang with the identical signature, so the
# hang is NOT in scripts/ci/run-quickstart-test.sh (left untouched). The leading
# hypothesis is an orphaned `go run` test-binary grandchild that survives the
# per-probe `timeout` and holds the step's stdout fd open, keeping GitHub's
# step runner waiting. These dumps target that hypothesis directly by capturing,
# for every lingering go/quickstart-test/stellar-core process, the open fds it
# holds (/proc/<pid>/fd) — which is what would reveal the fd-holder.
#
# PERIODIC SAMPLING (#3768). The first version armed a SINGLE dump at a fixed
# T+1200s delay. On run 31745517379 that sample landed at 23:11:08 — exactly one
# second AFTER the 813-second silent window closed — so every process in it had
# `START 23:11` / `TIME 00:00:00`: a freshly-spawned HEALTHY probe. A one-shot
# wall-clock sample cannot characterise an INTERVAL hang; it samples one instant
# and that instant is very likely to be the wrong one. This script therefore now
# samples REPEATEDLY and BOUNDEDLY, producing a time series (dump-NN-<ts>.txt)
# across the whole step so several samples land INSIDE any multi-minute wedge.
#
# Usage:
#   quickstart-hang-watchdog.sh <out_dir> <step_pid>
#     <out_dir>   directory to write the dumps into (created if absent)
#     <step_pid>  PID of the probe step's shell, whose open fds we capture
#
# Env knobs (all optional):
#   WATCHDOG_DELAY        seconds before the FIRST sample (default: 300)
#   WATCHDOG_INTERVAL     seconds between subsequent samples (default: 60)
#   WATCHDOG_MAX_SAMPLES  hard cap on total samples (default: 24)
#   WATCHDOG_POLL         step-liveness poll granularity in seconds (default: 2)
#
# Default coverage: 300 + 23*60 = 1680s (28 min) > the testnet shard's 25-min
# step_timeout_minutes, so the whole step is covered. Cap x dump size is the
# artifact bound: 24 samples x ~30 KB ~= 720 KB, small enough for the uploaded
# artifact and incapable of filling a runner disk even in a pathological run.
# When the cap is reached the script SAYS SO (stderr + a cap-reached line in
# index.txt) and exits, rather than silently going quiet.
#
# Output layout in <out_dir>:
#   dump-NN-<UTC-timestamp>.txt   one file per sample (the time series)
#   dump.txt                      copy of the MOST RECENT sample (back-compat:
#                                 the original single-dump filename keeps
#                                 working for any consumer/harness that greps it)
#   index.txt                     one line per sample + the cap/exit reason
#
# Termination. The loop stops on ANY of:
#   * the step shell (<step_pid>) exiting — polled every WATCHDOG_POLL seconds,
#     so the watchdog self-reaps within ~POLL seconds of the step finishing even
#     if no signal reaches it. This matters: a backgrounded process that outlives
#     the step while holding the step's stdout is the very wedge under study, so
#     the watchdog must never become one itself.
#   * SIGTERM/SIGINT (the workflow's explicit `kill` + EXIT trap), honoured
#     IMMEDIATELY: each sleep runs as a background child that the script `wait`s
#     on, and `wait` is interruptible by a trapped signal (a foreground `sleep`
#     is not — bash would defer the trap until it finished, leaving the watchdog
#     alive for up to a full poll interval after the step ended).
#   * WATCHDOG_MAX_SAMPLES samples taken.
#
# Every capture command is guarded with `|| true` so a missing tool (e.g.
# pstree) or a vanished PID never aborts a dump under `set -e`.

set -euo pipefail

OUT_DIR="${1:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid>}"
STEP_PID="${2:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid>}"

WATCHDOG_DELAY="${WATCHDOG_DELAY:-300}"
WATCHDOG_INTERVAL="${WATCHDOG_INTERVAL:-60}"
WATCHDOG_MAX_SAMPLES="${WATCHDOG_MAX_SAMPLES:-24}"
WATCHDOG_POLL="${WATCHDOG_POLL:-2}"

mkdir -p "$OUT_DIR"
DUMP_LATEST="$OUT_DIR/dump.txt"
INDEX="$OUT_DIR/index.txt"

# Pattern matching the processes we suspect of holding the step open. Used both
# for listing lingering PIDs and for per-PID fd capture below.
PROC_PATTERN='go run|quickstart/tests|stellar-core'

# Set by the TERM/INT trap, which fires as soon as the interruptible `wait` in
# watchdog_wait returns — so a reap request is honoured essentially instantly
# rather than being deferred until a foreground `sleep` finishes.
STOP=false
SLEEP_PID=""
# shellcheck disable=SC2317  # invoked via trap, not a direct call
on_stop() {
    STOP=true
    # Do not leave the in-flight sleep behind as an orphan.
    [[ -n "$SLEEP_PID" ]] && kill "$SLEEP_PID" 2>/dev/null
    return 0
}
trap on_stop TERM INT

log() { echo "quickstart-hang-watchdog: $*" >&2; }

note() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" >> "$INDEX" 2>/dev/null || true; }

# Is the step shell we are instrumenting still alive? `kill -0` does not send a
# signal; it only probes existence/permission.
step_alive() { kill -0 "$STEP_PID" 2>/dev/null; }

# watchdog_wait <seconds>
# Sleep in WATCHDOG_POLL-second chunks, aborting early when the step exits or a
# stop signal arrives. Returns 0 if the full duration elapsed and sampling
# should continue, 1 if the loop should terminate.
watchdog_wait() {
    local remaining="$1"
    local chunk
    while [[ "$remaining" -gt 0 ]]; do
        if [[ "$STOP" == true ]]; then
            return 1
        fi
        if ! step_alive; then
            return 1
        fi
        chunk="$WATCHDOG_POLL"
        if [[ "$remaining" -lt "$chunk" ]]; then
            chunk="$remaining"
        fi
        # Backgrounded sleep + `wait`: `wait` IS interrupted by a trapped signal,
        # a foreground `sleep` is NOT. This is what makes SIGTERM reaping
        # immediate instead of deferred by up to WATCHDOG_POLL seconds.
        sleep "$chunk" &
        SLEEP_PID=$!
        wait "$SLEEP_PID" 2>/dev/null || true
        SLEEP_PID=""
        remaining=$((remaining - chunk))
    done
    if [[ "$STOP" == true ]]; then
        return 1
    fi
    if ! step_alive; then
        return 1
    fi
    return 0
}

# take_sample <sample_number> <dump_path>
# Write one process-tree + open-fd snapshot. Identical capture set to the
# original one-shot dump, plus a sample header so the time series is readable.
take_sample() {
    local sample="$1"
    local dump="$2"

    {
        echo "===== quickstart-hang-watchdog dump ====="
        echo "sample: $sample of max $WATCHDOG_MAX_SAMPLES"
        echo "captured (UTC): $(date -u 2>/dev/null || true)"
        echo "seconds since watchdog start: $(( $(date -u +%s) - START_EPOCH ))"
        echo "step shell pid: $STEP_PID"
        echo

        echo "===== ps -ejH (process tree, session/pgid columns) ====="
        ps -ejH 2>/dev/null || true
        echo

        echo "===== ps faux (full process listing) ====="
        ps faux 2>/dev/null || true
        echo

        echo "===== pstree -p (falls back to ps faux if absent) ====="
        if command -v pstree >/dev/null 2>&1; then
            pstree -p 2>/dev/null || true
        else
            echo "(pstree not installed — see 'ps faux' above)"
            ps faux 2>/dev/null || true
        fi
        echo

        echo "===== lingering go/quickstart-test/stellar-core processes ====="
        pgrep -af "$PROC_PATTERN" 2>/dev/null || true
        echo

        echo "===== open fds of the step shell (pid $STEP_PID) ====="
        echo "-- ls -l /proc/$STEP_PID/fd --"
        ls -l "/proc/$STEP_PID/fd" 2>/dev/null || true
        echo

        echo "===== open fds of each lingering process (the suspected fd-holder) ====="
        # For each lingering go/quickstart-test/stellar-core PID, dump its open
        # fds. This is what reveals the orphaned grandchild holding the step's
        # stdout pipe open (the leading hypothesis for the hang). Sampling this
        # REPEATEDLY is what distinguishes a survivor (same PID, growing TIME,
        # across consecutive samples) from a healthy freshly-spawned probe.
        for p in $(pgrep -f "$PROC_PATTERN" 2>/dev/null || true); do
            echo "== fd of $p =="
            ls -l "/proc/$p/fd" 2>/dev/null || true
            echo
        done
    } > "$dump" 2>&1 || true

    # Keep the original single-dump filename pointing at the most recent sample
    # so any existing consumer that reads dump.txt keeps working.
    cp -f "$dump" "$DUMP_LATEST" 2>/dev/null || true
}

START_EPOCH="$(date -u +%s)"

note "start: step_pid=$STEP_PID delay=${WATCHDOG_DELAY}s interval=${WATCHDOG_INTERVAL}s max_samples=$WATCHDOG_MAX_SAMPLES poll=${WATCHDOG_POLL}s"
log "armed (first sample in ${WATCHDOG_DELAY}s, then every ${WATCHDOG_INTERVAL}s, max $WATCHDOG_MAX_SAMPLES samples)"

SAMPLE=0
WAIT_FOR="$WATCHDOG_DELAY"
while watchdog_wait "$WAIT_FOR"; do
    SAMPLE=$((SAMPLE + 1))
    DUMP="$OUT_DIR/dump-$(printf '%02d' "$SAMPLE")-$(date -u +%Y%m%dT%H%M%SZ).txt"
    take_sample "$SAMPLE" "$DUMP"
    note "sample $SAMPLE -> $(basename "$DUMP")"
    log "wrote process-tree dump $SAMPLE/$WATCHDOG_MAX_SAMPLES to $DUMP"

    if [[ "$SAMPLE" -ge "$WATCHDOG_MAX_SAMPLES" ]]; then
        # Bounded by construction: say so loudly rather than going silently
        # quiet, so a reader of the artifact knows the series was TRUNCATED and
        # not that the hang ended.
        note "cap reached: $SAMPLE/$WATCHDOG_MAX_SAMPLES samples taken; sampling STOPPED (raise WATCHDOG_MAX_SAMPLES for more)"
        log "sample cap reached ($SAMPLE/$WATCHDOG_MAX_SAMPLES) — stopping; raise WATCHDOG_MAX_SAMPLES for a longer series"
        exit 0
    fi

    WAIT_FOR="$WATCHDOG_INTERVAL"
done

note "stopped after $SAMPLE sample(s): step pid $STEP_PID gone or watchdog signalled"
log "stopped after $SAMPLE sample(s) (step exited or watchdog reaped)"
exit 0
