#!/usr/bin/env bash
# scripts/test-quickstart-harness.sh — TAP regression/coverage tests for the
# quickstart CI orchestration introduced in #2916.
#
# Tests verify:
#   1. Timeout-only retry logic in run-quickstart-test.sh
#   2. Non-timeout failures fail immediately (no retry)
#   3. Non-targeted shards never retry even on timeout
#   4. Workflow shard/probe wiring contract
#   5. Success path produces no retry artifacts
#   6. Second timeout still fails
#
# Run: bash scripts/test-quickstart-harness.sh
# Requires: bash 4+, timeout (coreutils)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WRAPPER="$REPO_ROOT/scripts/ci/run-quickstart-test.sh"
WORKFLOW="$REPO_ROOT/.github/workflows/quickstart.yml"

# TAP output
TEST_COUNT=0
PASS_COUNT=0
FAIL_COUNT=0

tap_ok() {
    TEST_COUNT=$((TEST_COUNT + 1))
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "ok $TEST_COUNT - $1"
}

tap_not_ok() {
    TEST_COUNT=$((TEST_COUNT + 1))
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "not ok $TEST_COUNT - $1"
    if [[ -n "${2:-}" ]]; then
        echo "  # $2"
    fi
}

tap_plan() {
    echo "1..$1"
}

# --- Setup ---
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

# Create a fake probe that exits with a given code after optional delay
make_probe() {
    local script="$TMPDIR_BASE/probe-$1.sh"
    local exit_code="${2:-0}"
    local delay="${3:-0}"
    cat > "$script" <<EOF
#!/bin/bash
sleep $delay
exit $exit_code
EOF
    chmod +x "$script"
    echo "$script"
}

# ============================================================
# Test 1: test_real_horizon_core_up_probe_times_out_under_current_policy_and_succeeds_via_wrapper
#
# Simulates the flaky scenario: probe times out on first attempt (exit 124),
# then succeeds on second attempt via the wrapper's retry logic.
# ============================================================
test_timeout_retry_on_targeted_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test1"
    mkdir -p "$diag_dir"

    # Create a probe that times out on first call, succeeds on second
    local state_file="$TMPDIR_BASE/state-test1"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test1.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    # Simulate a long-running process that will be killed by timeout
    sleep 999
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    # Run through wrapper with short timeout — first attempt should timeout (124),
    # wrapper should retry because this is the targeted shard, second should pass.
    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_real_horizon_core_up_probe_times_out_under_current_policy_and_succeeds_via_wrapper"
    else
        tap_not_ok "test_real_horizon_core_up_probe_times_out_under_current_policy_and_succeeds_via_wrapper" "exit=$exit_code"
    fi

    # Verify diagnostics from attempt 1 were captured
    if [[ -d "$diag_dir/attempt-1" ]]; then
        tap_ok "timeout_retry_preserves_attempt1_diagnostics"
    else
        tap_not_ok "timeout_retry_preserves_attempt1_diagnostics" "no attempt-1 dir"
    fi
}

# ============================================================
# Test 2: test_non_timeout_targeted_failure_does_not_retry
# ============================================================
test_non_timeout_failure_no_retry() {
    local diag_dir="$TMPDIR_BASE/diag-test2"
    mkdir -p "$diag_dir"

    # Probe that exits 1 immediately (non-timeout failure)
    local probe
    probe=$(make_probe "test2" 1 0)

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_non_timeout_targeted_failure_does_not_retry"
    else
        tap_not_ok "test_non_timeout_targeted_failure_does_not_retry" "expected failure, got success"
    fi

    # Verify only one attempt was made (no attempt-2 directory)
    if [[ ! -d "$diag_dir/attempt-2" ]]; then
        tap_ok "non_timeout_failure_single_attempt_only"
    else
        tap_not_ok "non_timeout_failure_single_attempt_only" "unexpected attempt-2 dir"
    fi
}

# ============================================================
# Test 3: test_workflow_preserves_shard_to_probe_contract
# ============================================================
test_workflow_shard_probe_contract() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "test_workflow_preserves_shard_to_probe_contract" "workflow file not found"
        return
    fi

    # Check: workflow disables upstream testing (test: false)
    if grep -q 'test:.*false' "$WORKFLOW" 2>/dev/null; then
        tap_ok "workflow_disables_upstream_testing"
    else
        tap_not_ok "workflow_disables_upstream_testing" "test: false not found in workflow"
    fi

    # Check: workflow has a local test job (match the YAML job key at top level)
    if grep -q '^  test:' "$WORKFLOW" 2>/dev/null; then
        tap_ok "workflow_has_local_test_job"
    else
        tap_not_ok "workflow_has_local_test_job" "no local test job found"
    fi

    # Check: workflow references run-quickstart-test.sh
    if grep -q 'run-quickstart-test.sh' "$WORKFLOW" 2>/dev/null; then
        tap_ok "workflow_uses_wrapper_script"
    else
        tap_not_ok "workflow_uses_wrapper_script" "wrapper not referenced in workflow"
    fi

    # Check: testnet/core,horizon shard exists
    if grep -q 'testnet' "$WORKFLOW" && grep -q 'core,horizon' "$WORKFLOW"; then
        tap_ok "workflow_has_testnet_core_horizon_shard"
    else
        tap_not_ok "workflow_has_testnet_core_horizon_shard" "testnet core,horizon shard not found"
    fi

    # Check: artifact name includes .tar suffix (upstream contract)
    if grep -q 'image-quickstart-testing-with-pr-amd64\.tar' "$WORKFLOW"; then
        tap_ok "workflow_artifact_name_matches_upstream"
    else
        tap_not_ok "workflow_artifact_name_matches_upstream" "artifact name missing .tar suffix"
    fi

    # Check: docker image tag matches upstream build output (quickstart:tag-arch)
    if grep -q 'quickstart:testing-with-pr-amd64' "$WORKFLOW"; then
        tap_ok "workflow_image_tag_matches_upstream"
    else
        tap_not_ok "workflow_image_tag_matches_upstream" "image tag does not match quickstart:testing-with-pr-amd64"
    fi

    # Check: probe names are normalized (underscores → hyphens)
    if grep -q 'probe_name.*_/-' "$WORKFLOW" || grep -q 'probe_name="${probe_name//_/-}"' "$WORKFLOW"; then
        tap_ok "workflow_normalizes_probe_names"
    else
        tap_not_ok "workflow_normalizes_probe_names" "probe name underscore-to-hyphen normalization missing"
    fi

    # Check: pubnet shard does NOT include stellar_rpc_healthy (excluded upstream)
    local pubnet_line
    pubnet_line=$(grep -n 'pubnet' "$WORKFLOW" | grep -v '#' | head -1 | cut -d: -f1)
    if [[ -n "$pubnet_line" ]]; then
        # Get the probes line for the pubnet entry (within ~5 lines after)
        local pubnet_probes
        pubnet_probes=$(sed -n "$((pubnet_line)),+5p" "$WORKFLOW" | grep 'probes:')
        if echo "$pubnet_probes" | grep -q 'test_stellar_rpc_healthy'; then
            tap_not_ok "pubnet_excludes_rpc_healthy" "pubnet shard includes stellar_rpc_healthy (excluded upstream)"
        else
            tap_ok "pubnet_excludes_rpc_healthy"
        fi
    else
        tap_not_ok "pubnet_excludes_rpc_healthy" "no pubnet shard found"
    fi
}

# ============================================================
# Test 4: test_targeted_timeout_still_fails_after_second_attempt
# ============================================================
test_double_timeout_fails() {
    local diag_dir="$TMPDIR_BASE/diag-test4"
    mkdir -p "$diag_dir"

    # Probe that always sleeps (always times out)
    local probe
    probe=$(make_probe "test4" 0 999)

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_targeted_timeout_still_fails_after_second_attempt"
    else
        tap_not_ok "test_targeted_timeout_still_fails_after_second_attempt" "expected failure"
    fi

    # Both attempts should have diagnostics
    if [[ -d "$diag_dir/attempt-1" && -d "$diag_dir/attempt-2" ]]; then
        tap_ok "double_timeout_preserves_both_diagnostics"
    else
        tap_not_ok "double_timeout_preserves_both_diagnostics" "missing attempt dirs"
    fi
}

# ============================================================
# Test 5: test_non_targeted_timeout_never_retries
# ============================================================
test_non_targeted_timeout_no_retry() {
    local diag_dir="$TMPDIR_BASE/diag-test5"
    mkdir -p "$diag_dir"

    # Probe that always sleeps (times out), but on a non-targeted shard
    local probe
    probe=$(make_probe "test5" 0 999)

    local exit_code=0
    "$WRAPPER" \
        --network local --enable "core" --probe core \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_non_targeted_timeout_never_retries"
    else
        tap_not_ok "test_non_targeted_timeout_never_retries" "expected failure"
    fi

    # Only one attempt (no retry on non-targeted shard)
    if [[ ! -d "$diag_dir/attempt-2" ]]; then
        tap_ok "non_targeted_timeout_single_attempt"
    else
        tap_not_ok "non_targeted_timeout_single_attempt" "unexpected retry on non-targeted shard"
    fi
}

# ============================================================
# Test 6: test_success_path_runs_once_without_retry_artifacts
# ============================================================
test_success_no_retry_artifacts() {
    local diag_dir="$TMPDIR_BASE/diag-test6"
    mkdir -p "$diag_dir"

    # Probe that succeeds immediately
    local probe
    probe=$(make_probe "test6" 0 0)

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_success_path_runs_once_without_retry_artifacts"
    else
        tap_not_ok "test_success_path_runs_once_without_retry_artifacts" "exit=$exit_code"
    fi

    # No diagnostics captured on success
    if [[ ! -d "$diag_dir/attempt-1" && ! -d "$diag_dir/attempt-2" ]]; then
        tap_ok "success_produces_no_diagnostics_dirs"
    else
        tap_not_ok "success_produces_no_diagnostics_dirs" "unexpected diagnostics on success"
    fi
}

# --- Run all tests ---
tap_plan 18

test_timeout_retry_on_targeted_shard
test_non_timeout_failure_no_retry
test_workflow_shard_probe_contract
test_double_timeout_fails
test_non_targeted_timeout_no_retry
test_success_no_retry_artifacts

echo ""
echo "# Results: $PASS_COUNT/$TEST_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    exit 1
fi
exit 0
