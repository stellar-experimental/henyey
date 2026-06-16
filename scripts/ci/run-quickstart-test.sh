#!/usr/bin/env bash
# scripts/ci/run-quickstart-test.sh — Run one upstream quickstart probe under
# GNU timeout with diagnostics capture and timeout-only retry for the known
# flaky shard.
#
# Usage:
#   run-quickstart-test.sh --network <net> --enable <services> --probe <name> \
#       --timeout <seconds> (default: 600) --diagnostics-dir <dir> -- <probe_command...>
#
# Env:
#   KILL_GRACE — grace passed to GNU `timeout -k` before it escalates
#     SIGTERM → SIGKILL (default: 30s). Load-bearing (#3273): the upstream
#     `go run` probe ignores SIGTERM, so without a kill-after grace `timeout`
#     blocks in wait() forever and the CI step hangs instead of timing out.
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
#   is converted to a neutral `exit 0` with a grep-able
#   `SOFT-SKIP` marker; diagnostics are still captured so the degraded-testnet
#   event remains observable in uploaded artifacts. This extends the existing
#   pubnet/testnet-RPC-disabled "don't gate henyey-correctness merges on external
#   network liveness" precedent, scoped strictly to the TIMEOUT outcome.
#   When the flag is OFF (every other caller/shard), behavior is byte-identical.
#
#   What counts as a TIMEOUT disposition (#3273): exit 124 OR exit 137. GNU
#   `timeout` returns 124 when its SIGTERM kills the probe, but 137 (128+9,
#   SIGKILL) when the probe IGNORES SIGTERM and the `-k` grace escalates to
#   SIGKILL — which is exactly the upstream `go run` probe's signature (it does
#   not forward SIGTERM). Both are a deadline hit, so both are soft-skipped
#   under the flag. A genuine probe assertion FAILURE (any non-124, non-137
#   exit — `go` test failures exit 1) ALWAYS stays red, even under the flag, so
#   a real henyey-on-testnet break is never masked.
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
# Grace before GNU `timeout` escalates SIGTERM → SIGKILL (the `-k` argument).
# Load-bearing (#3273): the upstream `go run` probe ignores SIGTERM, so without
# a kill-after grace `timeout` blocks in wait() forever and the CI step hangs.
# Overridable via the KILL_GRACE env var (e.g. the harness passes a short grace).
KILL_GRACE="${KILL_GRACE:-30s}"
DIAGNOSTICS_DIR=""
# Opt-in (#3272): treat a TIMEOUT (exit 124 or 137 — TERM-timeout or -k SIGKILL)
# disposition as a neutral soft-skip instead of a red failure. Default OFF ⇒
# behavior byte-identical for all other shards/callers. See the header for the
# rationale and the strict 124-or-137 timeout-only scope.
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
# exit code is a TIMEOUT disposition: EXACTLY 124 OR 137. Both are a `timeout`
# deadline hit:
#   * 124 — `timeout` sent SIGTERM and the probe died from it (the clean case).
#   * 137 — the probe IGNORED SIGTERM, so the `-k` grace escalated to SIGKILL
#     (128+9). Because the probe runs in its own process group (no
#     --foreground), GNU `timeout` reports this group SIGKILL as 137, NOT 124
#     (verified on coreutils 8.32). This is the EXACT signature of the upstream
#     `go run` probe that hung PR #3273's testnet shard (run 27298458353): with
#     the new `-k` it is now force-killed and surfaces as 137. Treating 137 as a
#     timeout disposition is what makes the root-cause fix actually de-gate main
#     — without it the probe would be bounded but stay red.
# Every OTHER non-(124|137) exit is NOT soft-skipped — a genuine probe assertion
# failure (e.g. exit 1) must stay red so a real henyey-on-testnet break is never
# masked. Callers use this at each timeout sink (first-attempt non-retryable
# timeout, and the post-retry second timeout) to decide whether to `exit 0`
# instead of `exit 1`.
#
# Scope note: 137 is also the runner-reclamation SIGKILL signature handled by
# is_retryable_exit. That is fine here: a 137 is ALWAYS an externally-killed
# (timeout-grace or runner-reclamation) outcome, never a probe's own assertion
# result — `go` test failures exit 1, not 137 — so soft-skipping 137 under the
# opt-in testnet-only flag never masks a real break.
should_soft_skip_timeout() {
    [[ "$SOFT_ON_TIMEOUT" == true ]] && { [[ "$1" -eq 124 ]] || [[ "$1" -eq 137 ]]; }
}

# Emit the grep-able SOFT-SKIP marker for a soft-skipped TIMEOUT disposition.
# Takes the REAL exit code as $1 (124 = SIGTERM-killed, 137 = -k SIGKILL of a
# SIGTERM-ignoring probe — the actual `go run` signature) so the log reflects
# what actually happened, not a hardcoded 124. The harness greps only the
# stable `SOFT-SKIP` prefix, so interpolating the code does not break it.
emit_soft_skip() {
    local soft_skip_exit="$1"
    echo "=== SOFT-SKIP: testnet sync probe timed out (environmental, not a henyey failure) ===" >&2
    echo "=== SOFT-SKIP: $NETWORK/$ENABLE/$PROBE exit $soft_skip_exit treated as neutral (#3272); diagnostics preserved ===" >&2
}

# --- Helper: capture diagnostics ---
capture_diagnostics() {
    local attempt="$1"
    local attempt_dir="$DIAGNOSTICS_DIR/attempt-$attempt"
    mkdir -p "$attempt_dir"

    # Docker container state (best-effort).
    #
    # Each docker call is bounded with `timeout 30` (#3286). capture_diagnostics
    # runs INSIDE run_probe — after the probe's exit_code is captured but BEFORE
    # run_probe returns. On a resource-starved CI runner the docker daemon can go
    # into uninterruptible D-state, and an unbounded `docker ps -a` then wedges
    # forever. The `|| true` below does NOT bound such a hang — a process stuck
    # in a syscall never reaches `|| true` — so without `timeout` the call wedges,
    # run_probe never returns, and the 124/137 --soft-on-timeout soft-skip (which
    # lives in main, AFTER run_probe) is never reached: the step runs to its
    # budget and the job is cancelled (the #3286 hang signature, confirmed by the
    # #3289 instrumentation's captured process tree). `timeout 30` lets the
    # diagnostics phase fail-open quickly so run_probe returns and the soft-skip
    # fires. The `|| true` is KEPT so a timeout-induced 124 from a wedged docker
    # call is swallowed and never contaminates the probe's own exit_code (already
    # captured before capture_diagnostics runs) — the probe disposition stays
    # byte-identical to before this fix. This bounds only the diagnostics-capture
    # phase; it does NOT change any pass/fail/soft-skip logic.
    if command -v docker &>/dev/null; then
        timeout 30 docker ps -a > "$attempt_dir/docker-ps.txt" 2>&1 || true
        # Capture logs from any running quickstart containers. The enumeration is
        # bounded too — the same D-state daemon wedges `docker ps -aq` identically.
        for cid in $(timeout 30 docker ps -aq --filter "name=quickstart" 2>/dev/null || true); do
            timeout 30 docker logs "$cid" > "$attempt_dir/container-$cid.log" 2>&1 || true
            timeout 30 docker inspect "$cid" > "$attempt_dir/container-$cid-inspect.json" 2>&1 || true
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
    echo "Command: timeout -k ${KILL_GRACE} ${TIMEOUT}s ${PROBE_CMD[*]}" >&2

    # Use PIPESTATUS[0] to capture the timeout exit code, not the tee exit.
    #
    # The `-k ${KILL_GRACE}` is load-bearing (#3273 root-cause fix): GNU
    # `timeout` first sends SIGTERM at the deadline, then — only with -k — sends
    # SIGKILL after the grace if the child is still alive. The upstream probe is
    # `go run quickstart/tests/<probe>.go`, and `go run` does NOT forward
    # SIGTERM to the test binary it execs; that binary in turn spawns
    # `docker exec` children. WITHOUT -k, `timeout` sends SIGTERM (ignored) at
    # the deadline and then blocks in wait() FOREVER — it never force-kills,
    # never returns, and hangs the whole CI step until the job wall-clock
    # cancels it (observed: PR #3273's testnet shard, run 27298458353, hung ~54
    # min). With -k, the SIGKILL force-terminates the whole process group within
    # the grace window, so the wrapper is guaranteed to return promptly.
    #
    # Exit-code note: because the child runs in its own process group (we do NOT
    # pass --foreground — in a PIPELINE like this one `--foreground` would
    # re-introduce the hang, since timeout then can't bound the pipeline child),
    # a `-k`-forced SIGKILL of a SIGTERM-ignoring probe yields exit 137 (128+9),
    # NOT 124. A SIGTERM that DOES kill the probe still yields 124. The
    # soft-skip therefore treats BOTH 124 and 137 as the timeout disposition
    # (see should_soft_skip_timeout). A genuine probe assertion failure exits
    # with its own (non-124, non-137) code, propagated unchanged, and stays red.
    set +o pipefail
    timeout -k "$KILL_GRACE" "$TIMEOUT" "${PROBE_CMD[@]}" 2>&1 | tee "$DIAGNOSTICS_DIR/attempt-${attempt}-output.log"
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

    # Post-retry sink. Under --soft-on-timeout, a SECOND timeout (exit 124 or 137
    # — TERM-timeout or -k SIGKILL) is a neutral soft-skip (#3272); any other
    # exit (a genuine probe assertion failure) stays red.
    if should_soft_skip_timeout "$EXIT_CODE"; then
        emit_soft_skip "$EXIT_CODE"
        echo "=== Failed on retry (exit $EXIT_CODE) but soft-skipped (timeout) ===" >&2
        exit 0
    fi

    echo "=== Failed on retry (exit $EXIT_CODE) ===" >&2
    exit 1
fi

# Non-transient failure, or transient failure on a non-retryable shard.
# Under --soft-on-timeout, a TIMEOUT (exit 124 or 137 — TERM-timeout or -k
# SIGKILL) here is a neutral soft-skip (#3272); any other exit (a genuine probe
# assertion failure) stays red, so a real henyey-on-testnet break is never masked.
if should_soft_skip_timeout "$EXIT_CODE"; then
    emit_soft_skip "$EXIT_CODE"
    echo "=== Failed (exit $EXIT_CODE) but soft-skipped (timeout) ===" >&2
    exit 0
fi

# Fail immediately so genuine test failures stay loud.
echo "=== Failed (exit $EXIT_CODE), not retryable ===" >&2
exit 1
