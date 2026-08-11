#!/usr/bin/env bash
# quickstart-hang-watchdog.sh — hang watchdog for the testnet quickstart
# "Run probes through wrapper" step (#3286 diagnostic + #3768 unwedge).
#
# Two phases, both on a timer inside the backgrounded sidecar the step launches:
#
#   1. DUMP (#3286): a process-tree + open-fd snapshot written to the
#      diagnostics dir, so the next hang leaves DATA behind.
#   2. UNWEDGE (#3768): after the dump, escalate — `docker kill` the quickstart
#      container the probe blocks on I/O against (so `go run` errors out and
#      returns) plus a belt-and-suspenders `pkill -9` of the lingering probe.
#
# Why the unwedge (#3768): the dump alone did not stop the ~55-min step hang
# from running to the 45-min JOB wall-clock, which is a *cancel* — and a cancel
# loses the log blob, discards this dump (the upload was gated on failure()),
# and disables the auto-retry. Killing the container from here unblocks the
# foreground probe so the step reaches a terminal conclusion (a neutral-green
# soft-skip on the testnet shard, #3272) WELL before the job cancel. A literal
# "separate step" cannot help — steps are sequential — so this backgrounded
# sidecar, already firing on a timer, is the only thing that runs concurrently
# with the wedged step.
#
# Two earlier wrapper-level fixes (#3273 `timeout -k`, #3287 `> file` redirect)
# both failed to bound the hang with the identical signature, so the hang is NOT
# in scripts/ci/run-quickstart-test.sh (left untouched). The leading hypothesis
# is an orphaned `go run` test-binary grandchild that survives the per-probe
# `timeout` and holds the step's stdout fd open; the dump targets that directly
# and the container kill breaks the underlying I/O wait.
#
# Usage:
#   quickstart-hang-watchdog.sh <out_dir> <step_pid> [container]
#     <out_dir>   directory to write dump.txt into (created if absent)
#     <step_pid>  PID of the probe step's shell, whose open fds we capture
#     [container] name of the quickstart docker container to kill on escalation
#                 (default: quickstart). Pass "" to skip the docker kill.
#
# Env:
#   WATCHDOG_ESCALATE_DELAY  seconds to wait after the dump before escalating
#                            (default 120). The escalation is DESTRUCTIVE, so it
#                            must fire only after the healthy budget — the caller
#                            reaps this sidecar on the healthy path before either
#                            timer fires, so on a passing run neither happens.
#   WATCHDOG_PROC_PATTERN    ERE matching the processes to snapshot and pkill
#                            (default: go run|quickstart/tests|stellar-core).
#
# Every capture/kill command is guarded with `|| true` (and the docker kill is
# `timeout 30`-bounded, mirroring run-quickstart-test.sh::capture_diagnostics)
# so a missing tool, a vanished PID, or a wedged docker daemon never aborts the
# watchdog under `set -e`.

set -euo pipefail

OUT_DIR="${1:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid> [container]}"
STEP_PID="${2:?usage: quickstart-hang-watchdog.sh <out_dir> <step_pid> [container]}"
CONTAINER="${3:-quickstart}"

mkdir -p "$OUT_DIR"
DUMP="$OUT_DIR/dump.txt"

# Pattern matching the processes we suspect of holding the step open. Used for
# listing lingering PIDs, for per-PID fd capture below, and for the escalation
# `pkill`. Overridable via WATCHDOG_PROC_PATTERN (a test seam so the pkill can
# be scoped to a unique marker).
PROC_PATTERN="${WATCHDOG_PROC_PATTERN:-go run|quickstart/tests|stellar-core}"

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

# ---- UNWEDGE phase (#3768) ----
# The dump is on disk; now break the hang so the step reaches a terminal
# conclusion before the 45-min job cancel. Wait WATCHDOG_ESCALATE_DELAY first so
# this destructive escalation never fires within the healthy budget (the caller
# reaps this sidecar on the healthy path before we get here). Then:
#   1. `docker kill` the quickstart container the probe blocks on I/O against,
#      so `go run` errors out and the foreground probe returns. `timeout 30`-
#      bounded so a wedged docker daemon cannot re-hang the watchdog.
#   2. `pkill -9` the lingering probe processes as a belt-and-suspenders, in
#      case killing the container is not enough to unblock a truly orphaned
#      grandchild.
# Each guarded `|| true` so a missing container, tool, or PID is a no-op.
sleep "${WATCHDOG_ESCALATE_DELAY:-120}"

if [[ -n "$CONTAINER" ]]; then
    echo "quickstart-hang-watchdog: escalating — docker kill $CONTAINER" >&2
    timeout 30 docker kill "$CONTAINER" 2>/dev/null || true
fi

echo "quickstart-hang-watchdog: escalating — pkill -9 -f '$PROC_PATTERN'" >&2
pkill -9 -f "$PROC_PATTERN" 2>/dev/null || true
