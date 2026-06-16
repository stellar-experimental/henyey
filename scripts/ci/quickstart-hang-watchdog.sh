#!/usr/bin/env bash
# quickstart-hang-watchdog.sh — DIAGNOSTIC instrumentation for the testnet
# quickstart "Run probes through wrapper" step hang (#3286).
#
# This script does NOT fix the hang. Its deliverable is DATA from the next
# hang: a process-tree + open-fd snapshot written to the diagnostics dir
# BEFORE the testnet step's tight `timeout-minutes` kill fires, so the
# subsequent "Upload diagnostics" step sweeps it into the uploaded artifact.
#
# Two wrapper-level fixes (#3273 `timeout -k`, #3287 `> file` redirect) both
# failed to bound the ~55-min step-8 hang with the identical signature, so the
# hang is NOT in scripts/ci/run-quickstart-test.sh (left untouched). The leading
# hypothesis is an orphaned `go run` test-binary grandchild that survives the
# per-probe `timeout` and holds the step's stdout fd open, keeping GitHub's
# step runner waiting. This dump targets that hypothesis directly by capturing,
# for every lingering go/quickstart-test/stellar-core process, the open fds it
# holds (/proc/<pid>/fd) — which is what would reveal the fd-holder.
#
# Usage:
#   quickstart-hang-watchdog.sh <out_dir> <step_pid>
#     <out_dir>   directory to write dump.txt into (created if absent)
#     <step_pid>  PID of the probe step's shell, whose open fds we capture
#
# Every capture command is guarded with `|| true` so a missing tool (e.g.
# pstree) or a vanished PID never aborts the dump under `set -e`.

set -euo pipefail

OUT_DIR="${1:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid>}"
STEP_PID="${2:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid>}"

mkdir -p "$OUT_DIR"
DUMP="$OUT_DIR/dump.txt"

# Pattern matching the processes we suspect of holding the step open. Used both
# for listing lingering PIDs and for per-PID fd capture below.
PROC_PATTERN='go run|quickstart/tests|stellar-core'

{
    echo "===== quickstart-hang-watchdog dump ====="
    echo "captured (UTC): $(date -u 2>/dev/null || true)"
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
    # For each lingering go/quickstart-test/stellar-core PID, dump its open fds.
    # This is what reveals the orphaned grandchild holding the step's stdout
    # pipe open (the leading hypothesis for the hang).
    for p in $(pgrep -f "$PROC_PATTERN" 2>/dev/null || true); do
        echo "== fd of $p =="
        ls -l "/proc/$p/fd" 2>/dev/null || true
        echo
    done
} > "$DUMP" 2>&1 || true

echo "quickstart-hang-watchdog: wrote process-tree dump to $DUMP" >&2
