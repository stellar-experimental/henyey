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
#   STEP_BUDGET_SECONDS / STEP_START_EPOCH — the GitHub STEP's total
#     `timeout-minutes` budget (in seconds) and the epoch second at which the
#     step began. Set by .github/workflows/quickstart.yml. When BOTH are present
#     AND --soft-on-timeout is on, the wrapper schedules attempts against the
#     REMAINING step budget (#3768) instead of blindly starting an attempt the
#     step cap will kill mid-flight. Unset ⇒ mechanism inert, behaviour
#     byte-identical. See the "Budget-aware scheduling" block below.
#   BUDGET_MARGIN_SECONDS (default 60) / BUDGET_MIN_ATTEMPT_SECONDS (default 15)
#     — tuning knobs for the above.
#
# Exit codes:
#   0 — probe passed (possibly after one retry), OR a timeout (exit 124) was
#       soft-skipped under --soft-on-timeout (see below), OR the step budget was
#       exhausted before an attempt could be started (BUDGET-EXHAUSTED, #3768)
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

# --- Step-budget awareness (#3768) ---
# Set by .github/workflows/quickstart.yml from the step's own `timeout-minutes`
# and the step's start time. When BOTH are set AND --soft-on-timeout is on, the
# wrapper knows how much of the GitHub STEP budget is left and will not start an
# attempt that provably cannot finish inside it. See budget_plan_attempt() for
# the full rationale. When either is unset/non-numeric the whole mechanism is
# inert and behaviour is byte-identical to before (every non-testnet shard, and
# every direct/manual invocation).
STEP_BUDGET_SECONDS="${STEP_BUDGET_SECONDS:-}"
STEP_START_EPOCH="${STEP_START_EPOCH:-}"
# Seconds reserved at the end of the step budget for the post-probe work the
# wrapper still has to do (capture_diagnostics + the workflow's remaining loop
# iterations). Calibrated from run 31745517379, where diagnostics capture after
# a 124 took 15s (23:15:07 timeout → 23:15:22 diagnostics written); 60s is 4x
# that, and is the margin BELOW which no new attempt may be started.
BUDGET_MARGIN_SECONDS="${BUDGET_MARGIN_SECONDS:-60}"
# Floor: with less usable budget than this we do not start an attempt at all.
# Above it we still run the probe (at a capped timeout) so a genuine assertion
# failure — which exits in well under a second — is still OBSERVED and stays RED.
BUDGET_MIN_ATTEMPT_SECONDS="${BUDGET_MIN_ATTEMPT_SECONDS:-15}"
# Per-attempt budget actually handed to GNU `timeout`. Equal to $TIMEOUT unless
# the remaining step budget forces a cap (see budget_plan_attempt).
ATTEMPT_TIMEOUT=""
# Usable seconds remaining at the moment a budget skip was decided (for the log).
BUDGET_REMAINING_AT_SKIP=0

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

ATTEMPT_TIMEOUT="$TIMEOUT"

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

# --- Budget-aware scheduling (#3768) -------------------------------------
#
# WHY. The soft-skip above can only classify an outcome the wrapper actually
# OBSERVES. A GitHub *step-level* `timeout-minutes` kill is invisible to it: the
# runner kills the step and marks the job RED with no exit code for the wrapper
# to soft-skip. Run 31745517379 died exactly this way — every probe disposition
# in it was `exit 124` on a `--soft-on-timeout` shard (i.e. the exact
# environmental class #3272 exists to neutralise), but `horizon-core-up` burned
# 19m33s of the 25-minute step budget, leaving `horizon-ingesting` attempt 2
# about 45 seconds. The step cap fired mid-attempt and converted a run that
# should have been neutral-green into a hard RED.
#
# WHAT. Before starting an attempt, compare the REMAINING step budget with the
# per-probe timeout:
#   * usable = step_start + step_budget - now - BUDGET_MARGIN_SECONDS
#   * usable < BUDGET_MIN_ATTEMPT_SECONDS  → do not start an attempt at all;
#     the caller emits the BUDGET-EXHAUSTED soft-skip and exits 0 (neutral).
#   * BUDGET_MIN_ATTEMPT_SECONDS <= usable < TIMEOUT → still RUN the attempt,
#     but CAP its `timeout` at `usable` so it cannot overrun the step cap.
#   * usable >= TIMEOUT → run at the full per-probe budget (unchanged).
#
# The middle branch is deliberate and is what keeps the safety contract intact:
# a genuine probe assertion failure exits in well under a second, so it is still
# OBSERVED and still propagates as RED even when the budget is tight. Only when
# there is effectively no budget left at all do we skip without running — and in
# that situation the alternative is not "we see the failure", it is "the step
# cap kills us and we see nothing at all".
#
# SCOPE. Everything here is gated on SOFT_ON_TIMEOUT (the testnet shard only)
# AND on both budget env vars being present and numeric. Every other shard /
# caller never enters any of these branches, so their behaviour — including a
# genuine `go` exit 1 staying RED — is byte-identical to before.
#
# NOT a timer loosening: no timeout is raised anywhere. #3273 (`-k`) and #3287
# (redirect) already showed that loosening/redirecting timers does not fix this.
budget_enabled() {
    [[ "$SOFT_ON_TIMEOUT" == true ]] || return 1
    [[ "$STEP_BUDGET_SECONDS" =~ ^[0-9]+$ ]] || return 1
    [[ "$STEP_START_EPOCH" =~ ^[0-9]+$ ]] || return 1
    return 0
}

# Usable seconds left in the step budget after reserving BUDGET_MARGIN_SECONDS
# for the wrapper's own post-probe work (diagnostics capture, remaining loop).
# May be negative when the budget is already blown.
budget_usable_seconds() {
    local now
    now="$(date -u +%s)"
    echo $(( STEP_START_EPOCH + STEP_BUDGET_SECONDS - now - BUDGET_MARGIN_SECONDS ))
}

# budget_plan_attempt
# Sets ATTEMPT_TIMEOUT for the next attempt.
# Returns 0 → start the attempt with ATTEMPT_TIMEOUT.
# Returns 1 → budget exhausted, do NOT start an attempt (BUDGET_REMAINING_AT_SKIP
#             holds the usable seconds for the log).
budget_plan_attempt() {
    ATTEMPT_TIMEOUT="$TIMEOUT"
    budget_enabled || return 0

    local usable
    usable="$(budget_usable_seconds)"

    if [[ "$usable" -lt "$BUDGET_MIN_ATTEMPT_SECONDS" ]]; then
        BUDGET_REMAINING_AT_SKIP="$usable"
        return 1
    fi

    if [[ "$usable" -lt "$TIMEOUT" ]]; then
        ATTEMPT_TIMEOUT="$usable"
        echo "=== Step budget short: capping this attempt at ${ATTEMPT_TIMEOUT}s (per-probe budget ${TIMEOUT}s) so it cannot overrun the step cap (#3768) ===" >&2
    fi
    return 0
}

# Emit the grep-able SOFT-SKIP marker for a BUDGET-EXHAUSTED skip. Keeps the
# stable `SOFT-SKIP` prefix that downstream log-scraping greps for, and adds the
# distinct `BUDGET-EXHAUSTED` token so triage can never confuse it with a
# genuine probe timeout (exit 124/137), which carries `exit <code>` instead.
emit_budget_skip() {
    local usable="$1"
    local attempt="$2"
    echo "=== SOFT-SKIP: BUDGET-EXHAUSTED — not enough step budget left to run attempt $attempt (environmental, not a henyey failure) ===" >&2
    echo "=== SOFT-SKIP: BUDGET-EXHAUSTED $NETWORK/$ENABLE/$PROBE — ${usable}s usable of the ${STEP_BUDGET_SECONDS}s step budget (per-probe timeout ${TIMEOUT}s, reserve ${BUDGET_MARGIN_SECONDS}s, floor ${BUDGET_MIN_ATTEMPT_SECONDS}s); NO probe attempt started, treated as neutral (#3768) ===" >&2
    # Leave a breadcrumb in the uploaded diagnostics artifact so the skip is
    # observable outside the job log.
    {
        echo "budget-exhausted soft-skip (#3768)"
        echo "when (UTC):            $(date -u 2>/dev/null || true)"
        echo "shard/probe:           $NETWORK/$ENABLE/$PROBE"
        echo "attempt not started:   $attempt"
        echo "usable seconds left:   $usable"
        echo "step budget seconds:   $STEP_BUDGET_SECONDS"
        echo "step start epoch:      $STEP_START_EPOCH"
        echo "per-probe timeout:     $TIMEOUT"
        echo "reserve margin:        $BUDGET_MARGIN_SECONDS"
        echo "min attempt floor:     $BUDGET_MIN_ATTEMPT_SECONDS"
    } > "$DIAGNOSTICS_DIR/budget-skip.txt" 2>/dev/null || true
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
    # ATTEMPT_TIMEOUT is $TIMEOUT unless budget_plan_attempt capped it because
    # the remaining STEP budget is shorter than the per-probe budget (#3768).
    # With the budget mechanism inert (every non-testnet shard / any caller that
    # does not set the budget env) it is always exactly $TIMEOUT.
    echo "Command: timeout -k ${KILL_GRACE} ${ATTEMPT_TIMEOUT}s ${PROBE_CMD[*]}" >&2

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
    timeout -k "$KILL_GRACE" "$ATTEMPT_TIMEOUT" "${PROBE_CMD[@]}" 2>&1 | tee "$DIAGNOSTICS_DIR/attempt-${attempt}-output.log"
    exit_code=${PIPESTATUS[0]}
    set -o pipefail

    if [[ $exit_code -ne 0 ]]; then
        capture_diagnostics "$attempt"
    fi

    return $exit_code
}

# --- Main execution ---
# Budget gate BEFORE attempt 1 (#3768). If the previous probes in this step have
# already eaten the step budget, starting an attempt here provably cannot
# complete — the GitHub step cap would kill it mid-flight and turn a shard whose
# timeouts are explicitly non-gating into a hard RED. Emit the neutral
# BUDGET-EXHAUSTED soft-skip instead. Inert unless --soft-on-timeout AND the
# budget env vars are set (see budget_plan_attempt).
if ! budget_plan_attempt; then
    emit_budget_skip "$BUDGET_REMAINING_AT_SKIP" 1
    exit 0
fi

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

    # Budget gate BEFORE the retry (#3768). Same reasoning as the pre-attempt-1
    # gate, with one extra guard: we only convert to a neutral skip when the
    # disposition we ALREADY observed is itself soft-skippable (124/137). For
    # any other retryable-but-not-soft-skippable exit (143 — runner SIGTERM,
    # which today stays RED after a double failure) we deliberately do NOT
    # widen the soft-skip: we fall through and retry at the full budget exactly
    # as before, so that contract is untouched.
    if ! budget_plan_attempt; then
        if should_soft_skip_timeout "$EXIT_CODE"; then
            emit_budget_skip "$BUDGET_REMAINING_AT_SKIP" 2
            exit 0
        fi
        ATTEMPT_TIMEOUT="$TIMEOUT"
    fi

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
