#!/usr/bin/env bash
# scripts/ci/run-quickstart-test.sh — Run one upstream quickstart probe under
# GNU timeout with diagnostics capture and timeout-only retry for the known
# flaky shard.
#
# Usage:
#   run-quickstart-test.sh --network <net> --enable <services> --probe <name> \
#       --timeout <seconds> (default: 600) --diagnostics-dir <dir> -- <probe_command...>
#
# Exit codes:
#   0 — probe passed (possibly after one retry)
#   1 — probe failed (non-timeout, or second timeout on retryable shard)
#   2 — usage error
#
# Timeout budget: the caller (.github/workflows/quickstart.yml) is responsible
# for supplying the per-probe budget via --timeout <seconds>. That budget
# mirrors upstream stellar/quickstart/.github/workflows/internal-test.yml:
# `github.run_attempt * timeout_multiplier (4) * 60` seconds (4 min on attempt
# 1, escalating on manual re-runs). This wrapper does not compute the budget;
# it applies the given --timeout via GNU `timeout` and layers the targeted
# single retry on top of it (see below).
#
# The ONLY retryable case: network=testnet, enable=core,horizon,
# probe=horizon-core-up, and exit was a transient-infra-classified code:
#   * exit 124 — GNU `timeout` killed a slow start (the original #2916 flake).
#   * exit 143 — the probe (or the runner) received SIGTERM (128+15=143),
#     i.e. "the runner has received a shutdown signal" spot-runner reclamation
#     (#3131). When the runner survives the SIGTERM long enough for the wrapper
#     to observe the probe's 143, re-running once self-heals without a manual
#     orchestrator re-run; if the runner is fully reclaimed the wrapper dies
#     too and the workflow's normal re-run path applies.
# The retry re-runs the probe under the SAME --timeout budget — it is an
# additional attempt at the same per-attempt budget, not a change to the
# budget (#2920). It stays scoped to the targeted shard so a genuine probe
# failure (exit 1, etc.) or any failure on another shard still fails loudly.

set -euo pipefail

# --- Argument parsing ---
NETWORK=""
ENABLE=""
PROBE=""
TIMEOUT=600
DIAGNOSTICS_DIR=""
PROBE_CMD=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --network) NETWORK="$2"; shift 2 ;;
        --enable) ENABLE="$2"; shift 2 ;;
        --probe) PROBE="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --diagnostics-dir) DIAGNOSTICS_DIR="$2"; shift 2 ;;
        --) shift; PROBE_CMD=("$@"); break ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$NETWORK" || -z "$ENABLE" || -z "$PROBE" || ${#PROBE_CMD[@]} -eq 0 ]]; then
    echo "Usage: run-quickstart-test.sh --network <net> --enable <services> --probe <name> --timeout <seconds> [--diagnostics-dir <dir>] -- <cmd...>" >&2
    exit 2
fi

DIAGNOSTICS_DIR="${DIAGNOSTICS_DIR:-/tmp/quickstart-diagnostics/$NETWORK-$ENABLE-$PROBE}"
mkdir -p "$DIAGNOSTICS_DIR"

# --- Helper: is this the retryable shard? ---
is_retryable_shard() {
    [[ "$NETWORK" == "testnet" && "$ENABLE" == "core,horizon" && "$PROBE" == "horizon-core-up" ]]
}

# --- Helper: is this exit code a retryable transient-infra failure? ---
# 124: GNU `timeout` killed a slow start (#2916). 143: SIGTERM (128+15) —
# runner-shutdown / spot-runner reclamation (#3131). Both are infra-transient,
# not test-logic failures, so they are retried once on the targeted shard only.
is_retryable_exit() {
    [[ "$1" -eq 124 || "$1" -eq 143 ]]
}

# --- Helper: capture diagnostics ---
capture_diagnostics() {
    local attempt="$1"
    local attempt_dir="$DIAGNOSTICS_DIR/attempt-$attempt"
    mkdir -p "$attempt_dir"

    # Docker container state (best-effort)
    if command -v docker &>/dev/null; then
        docker ps -a > "$attempt_dir/docker-ps.txt" 2>&1 || true
        # Capture logs from any running quickstart containers
        for cid in $(docker ps -aq --filter "name=quickstart" 2>/dev/null || true); do
            docker logs "$cid" > "$attempt_dir/container-$cid.log" 2>&1 || true
            docker inspect "$cid" > "$attempt_dir/container-$cid-inspect.json" 2>&1 || true
        done
    fi

    # Capture HTTP state from Horizon if reachable
    curl -sf http://localhost:8000/ > "$attempt_dir/horizon-root.json" 2>/dev/null || true
    curl -sf http://localhost:11626/info > "$attempt_dir/core-info.json" 2>/dev/null || true

    echo "Diagnostics saved to $attempt_dir" >&2
}

# --- Run probe under timeout ---
run_probe() {
    local attempt="$1"
    local exit_code=0

    echo "=== Attempt $attempt: $PROBE ($NETWORK/$ENABLE) ===" >&2
    echo "Command: timeout ${TIMEOUT}s ${PROBE_CMD[*]}" >&2

    # Use PIPESTATUS[0] to capture the timeout exit code, not the tee exit.
    set +o pipefail
    timeout "$TIMEOUT" "${PROBE_CMD[@]}" 2>&1 | tee "$DIAGNOSTICS_DIR/attempt-${attempt}-output.log"
    exit_code=${PIPESTATUS[0]}
    set -o pipefail

    if [[ $exit_code -ne 0 ]]; then
        capture_diagnostics "$attempt"
    fi

    return $exit_code
}

# --- Main execution ---
EXIT_CODE=0
run_probe 1 || EXIT_CODE=$?

if [[ $EXIT_CODE -eq 0 ]]; then
    # Success on first attempt
    exit 0
fi

if is_retryable_exit "$EXIT_CODE" && is_retryable_shard; then
    # Transient-infra failure (timeout 124 / runner-shutdown 143) on the
    # known-flaky shard — retry once under the same per-attempt budget.
    echo "=== Transient-infra failure (exit $EXIT_CODE) on retryable shard ($NETWORK/$ENABLE/$PROBE), retrying ===" >&2
    EXIT_CODE=0
    run_probe 2 || EXIT_CODE=$?

    if [[ $EXIT_CODE -eq 0 ]]; then
        echo "=== Passed on retry ===" >&2
        exit 0
    fi

    echo "=== Failed on retry (exit $EXIT_CODE) ===" >&2
    exit 1
fi

# Non-transient failure, or transient failure on a non-retryable shard —
# fail immediately so genuine test failures stay loud.
echo "=== Failed (exit $EXIT_CODE), not retryable ===" >&2
exit 1
