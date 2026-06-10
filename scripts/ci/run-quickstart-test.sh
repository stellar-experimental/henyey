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
#   0 — probe passed (possibly after one retry), OR a timeout (exit 124) was
#       soft-skipped under --soft-on-timeout (see below)
#   1 — probe failed (non-timeout, or second timeout on retryable shard)
#   2 — usage error
#
# --soft-on-timeout (opt-in, default OFF — #3272):
#   The chronic external-testnet flake (issue #3272) red-rolls `main` whenever a
#   stuck testnet sync probe times out (GNU `timeout` exit 124) on both the first
#   attempt and the single retry. The testnet shard depends on external network
#   liveness (checkpoint cadence, archive availability) and is NOT a henyey
#   correctness signal — so a TIMEOUT there should not gate the merge. When
#   --soft-on-timeout is passed (the testnet shard only), a *timeout* disposition
#   (exit code 124 ONLY) is converted to a neutral `exit 0` with a grep-able
#   `SOFT-SKIP` marker; diagnostics are still captured so the degraded-testnet
#   event remains observable in uploaded artifacts. This extends the existing
#   pubnet/testnet-RPC-disabled "don't gate henyey-correctness merges on external
#   network liveness" precedent, scoped strictly to the TIMEOUT outcome.
#   When the flag is OFF (every other caller/shard), behavior is byte-identical.
#   A genuine probe assertion FAILURE (any non-124 exit) ALWAYS stays red, even
#   under the flag, so a real henyey-on-testnet break is never masked. Only 124
#   is soft-skipped; 125/126/127/137 (and any other non-124) stay red.
#
# Timeout budget: the caller (.github/workflows/quickstart.yml) is responsible
# for supplying the per-probe budget via --timeout <seconds>. That budget
# mirrors upstream stellar/quickstart/.github/workflows/internal-test.yml:
# `github.run_attempt * timeout_multiplier (4) * 60` seconds (4 min on attempt
# 1, escalating on manual re-runs). This wrapper does not compute the budget;
# it applies the given --timeout via GNU `timeout` and layers the targeted
# single retry on top of it (see below).
#
# The retryable case: network=testnet, enable=core,horizon (ANY probe on that
# shard), and exit was a transient-infra-classified code:
#   * exit 124 — GNU `timeout` killed a slow start (the original #2916 flake).
#   * exit 143 — the probe (or the runner) received SIGTERM (128+15=143),
#     i.e. "the runner has received a shutdown signal" spot-runner reclamation
#     (#3131). When the runner survives the SIGTERM long enough for the wrapper
#     to observe the probe's 143, re-running once self-heals without a manual
#     orchestrator re-run; if the runner is fully reclaimed the wrapper dies
#     too and the workflow's normal re-run path applies.
#   * exit 137 — SIGKILL (128+9=137), the harsher reclamation signature observed
#     on PR #3187's run 27037187429 ("exit code 137"). Same transient class as
#     143; retried identically (#3193). Whole-runner reclamation kills this
#     wrapper too, so the in-script retry only covers the case where the runner
#     survives; the out-of-run quickstart-retry.yml workflow recovers the rest.
#
# Scope widened from probe=horizon-core-up to the whole testnet/core,horizon
# shard (#3185). Testnet stellar-core is slow to catch up; the GREEN baseline
# (f449a5a9) routinely shows horizon-core-up timing out (124) on attempt 1 and
# only passing on the wrapper retry. But the SAME slow-startup propagates to the
# NEXT startup-dependent probe in the shard — horizon-ingesting timed out (124)
# right after horizon-core-up finally came up (069ebfcc, run 27019344504) and,
# because the retry was scoped to one probe name, hard-failed as "not retryable".
# Any probe on this shard can be caught mid-startup by a slow core or a
# spot-runner SIGTERM, so the transient-infra retry now covers the shard, not a
# single probe. Non-transient exits (e.g. exit 1) still fail loudly, and other
# networks/enable-sets (local/pubnet) are still never retried.
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
# Opt-in (#3272): treat a TIMEOUT (exit 124) disposition as a neutral soft-skip
# instead of a red failure. Default OFF ⇒ behavior byte-identical for all other
# shards/callers. See the header for the rationale and the strict 124-only scope.
SOFT_ON_TIMEOUT=false
PROBE_CMD=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --network) NETWORK="$2"; shift 2 ;;
        --enable) ENABLE="$2"; shift 2 ;;
        --probe) PROBE="$2"; shift 2 ;;
        --timeout) TIMEOUT="$2"; shift 2 ;;
        --diagnostics-dir) DIAGNOSTICS_DIR="$2"; shift 2 ;;
        --soft-on-timeout) SOFT_ON_TIMEOUT=true; shift ;;
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
# The testnet/core,horizon shard is the known slow-startup shard (#2916/#3185):
# testnet stellar-core takes minutes to catch up, so any startup-dependent probe
# on this shard can hit a transient-infra exit (124 timeout / 143 SIGTERM). The
# retry is scoped to this shard (not a single probe name) so whichever probe is
# running when core is still catching up — horizon-core-up, horizon-ingesting,
# etc. — gets the same single self-healing retry. local/pubnet shards never retry.
is_retryable_shard() {
    [[ "$NETWORK" == "testnet" && "$ENABLE" == "core,horizon" ]]
}

# --- Helper: is this exit code a retryable transient-infra failure? ---
# 124: GNU `timeout` killed a slow start (#2916). 143: SIGTERM (128+15) —
# runner-shutdown / spot-runner reclamation (#3131). 137: SIGKILL (128+9) —
# the harsher runner-reclamation signature observed on PR #3187's run
# 27037187429 ("The runner has received a shutdown signal" + "exit code 137",
# #3193). All three are infra-transient, not test-logic failures, so they are
# retried once on the targeted shard only. (Note: whole-runner reclamation also
# kills this wrapper, so the in-script retry only helps when the runner survives
# long enough to observe the probe's exit; the out-of-run quickstart-retry.yml
# workflow recovers the fully-reclaimed case.)
is_retryable_exit() {
    [[ "$1" -eq 124 || "$1" -eq 143 || "$1" -eq 137 ]]
}

# --- Helper: soft-skip a TIMEOUT disposition under --soft-on-timeout (#3272) ---
# Returns 0 (and emits the grep-able SOFT-SKIP marker) iff the flag is on AND the
# exit code is EXACTLY 124 (a GNU `timeout` TIMEOUT). Strictly 124 only:
# 125/126/127/137 and any other non-124 exit are NOT soft-skipped — a genuine
# probe assertion failure must stay red so a real henyey-on-testnet break is
# never masked. Callers use this at each of the two 124 sinks (first-attempt
# non-retryable timeout, and the post-retry second timeout) to decide whether to
# `exit 0` instead of `exit 1`.
should_soft_skip_timeout() {
    [[ "$SOFT_ON_TIMEOUT" == true && "$1" -eq 124 ]]
}

emit_soft_skip() {
    echo "=== SOFT-SKIP: testnet sync probe timed out (environmental, not a henyey failure) ===" >&2
    echo "=== SOFT-SKIP: $NETWORK/$ENABLE/$PROBE exit 124 treated as neutral (#3272); diagnostics preserved ===" >&2
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

    # Post-retry sink. Under --soft-on-timeout, a SECOND timeout (exit 124 ONLY)
    # is a neutral soft-skip (#3272); any other non-124 exit (a genuine probe
    # assertion failure) stays red.
    if should_soft_skip_timeout "$EXIT_CODE"; then
        emit_soft_skip
        echo "=== Failed on retry (exit $EXIT_CODE) but soft-skipped (timeout) ===" >&2
        exit 0
    fi

    echo "=== Failed on retry (exit $EXIT_CODE) ===" >&2
    exit 1
fi

# Non-transient failure, or transient failure on a non-retryable shard.
# Under --soft-on-timeout, a TIMEOUT (exit 124 ONLY) here is a neutral soft-skip
# (#3272); any other non-124 exit (a genuine probe assertion failure) stays red,
# so a real henyey-on-testnet break is never masked.
if should_soft_skip_timeout "$EXIT_CODE"; then
    emit_soft_skip
    echo "=== Failed (exit $EXIT_CODE) but soft-skipped (timeout) ===" >&2
    exit 0
fi

# Fail immediately so genuine test failures stay loud.
echo "=== Failed (exit $EXIT_CODE), not retryable ===" >&2
exit 1
