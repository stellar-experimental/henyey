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
CONTRACT="$REPO_ROOT/scripts/ci/upstream-quickstart-contract.yml"

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

        # Check: pubnet shard DOES include stellar_rpc_up. Upstream
        # internal-test.yml runs test_stellar_rpc_up.go on every shard whose
        # `enable` contains 'rpc' (`if: contains(matrix.enable, 'rpc')`) with
        # no pubnet exclusion — only rpc_healthy is gated off on pubnet. This
        # positive assertion guards against silently dropping RPC-up coverage
        # from the pubnet/core,rpc,horizon shard (parity residual from #2916,
        # tracked in #2919).
        if echo "$pubnet_probes" | grep -q 'test_stellar_rpc_up'; then
            tap_ok "pubnet_includes_rpc_up"
        else
            tap_not_ok "pubnet_includes_rpc_up" "pubnet shard missing stellar_rpc_up (upstream runs it on every rpc-enabled shard)"
        fi
    else
        tap_not_ok "pubnet_excludes_rpc_healthy" "no pubnet shard found"
        tap_not_ok "pubnet_includes_rpc_up" "no pubnet shard found"
    fi

    # Check: local rpc shard includes test_friendbot.go (upstream runs friendbot
    # for any local shard whose enable contains rpc or horizon)
    local rpc_line
    rpc_line=$(grep -n 'enable: rpc$' "$WORKFLOW" | head -1 | cut -d: -f1)
    if [[ -n "$rpc_line" ]]; then
        local rpc_probes
        rpc_probes=$(sed -n "$((rpc_line)),+3p" "$WORKFLOW" | grep 'probes:')
        if echo "$rpc_probes" | grep -q 'test_friendbot.go'; then
            tap_ok "local_rpc_shard_includes_friendbot"
        else
            tap_not_ok "local_rpc_shard_includes_friendbot" \
                "local rpc shard missing test_friendbot.go (upstream runs it for enable:rpc)"
        fi
    else
        tap_not_ok "local_rpc_shard_includes_friendbot" "no local rpc shard found"
    fi

    # Check: artifact-layout validation is blocking (not a warning)
    # The validate-contract step must error (increment ERRORS) if docker save /
    # /tmp/image pattern is missing from internal-build.yml.
    if grep -q 'could not confirm /tmp/image.*non-blocking' "$WORKFLOW"; then
        tap_not_ok "artifact_layout_check_is_blocking" \
            "artifact-layout check is still non-blocking (⚠ warning instead of ✗ error)"
    else
        tap_ok "artifact_layout_check_is_blocking"
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

# ============================================================
# Test 7: test_upstream_contract_pinned_and_workflow_matches
#
# Validates that the workflow's assumptions about upstream stellar/quickstart
# match the pinned contract file. This catches drift: if upstream changes
# artifact naming, image tags, or probe layout, the contract file must be
# updated explicitly (forcing a human review of whether our workflow still
# works).
#
# Also validates that the validate-contract CI job references both build.yml
# AND internal-build.yml, ensuring the delegated artifact contract is checked.
# ============================================================
test_upstream_contract_validation() {
    if [[ ! -f "$CONTRACT" ]]; then
        tap_not_ok "upstream_contract_file_exists" "scripts/ci/upstream-quickstart-contract.yml not found"
        tap_not_ok "workflow_artifact_matches_contract" "skipped (no contract)"
        tap_not_ok "workflow_image_tag_matches_contract" "skipped (no contract)"
        tap_not_ok "workflow_build_inputs_match_contract" "skipped (no contract)"
        tap_not_ok "workflow_pubnet_exclusions_match_contract" "skipped (no contract)"
        tap_not_ok "workflow_probe_normalization_matches_contract" "skipped (no contract)"
        tap_not_ok "workflow_validates_internal_build" "skipped (no contract)"
        tap_not_ok "workflow_reads_contract_file" "skipped (no contract)"
        tap_not_ok "contract_documents_limitation" "skipped (no contract)"
        tap_not_ok "contract_documents_delegated_artifact" "skipped (no contract)"
        tap_not_ok "workflow_enforces_exact_artifact_pattern" "skipped (no contract)"
        tap_not_ok "workflow_enforces_exact_image_tag_pattern" "skipped (no contract)"
        return
    fi

    tap_ok "upstream_contract_file_exists"

    # Extract expected values from contract (simple grep — no YAML parser needed)
    local expected_artifact expected_tag artifact_pattern
    expected_artifact=$(grep '^expected_artifact_name:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')
    expected_tag=$(grep '^expected_image_tag:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')
    artifact_pattern=$(grep '^artifact_name_pattern:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')

    # Validate artifact name in workflow matches contract
    if grep -q "$expected_artifact" "$WORKFLOW"; then
        tap_ok "workflow_artifact_matches_contract"
    else
        tap_not_ok "workflow_artifact_matches_contract" "expected '$expected_artifact' in workflow"
    fi

    # Validate image tag in workflow matches contract
    if grep -q "$expected_tag" "$WORKFLOW"; then
        tap_ok "workflow_image_tag_matches_contract"
    else
        tap_not_ok "workflow_image_tag_matches_contract" "expected '$expected_tag' in workflow"
    fi

    # Validate workflow passes test: false (matches contract's build_inputs)
    if grep -q 'test: false' "$WORKFLOW"; then
        tap_ok "workflow_build_inputs_match_contract"
    else
        tap_not_ok "workflow_build_inputs_match_contract" "test: false not found"
    fi

    # Validate pubnet exclusions match contract
    # Contract says: horizon-core-up, horizon-ingesting, stellar-rpc-healthy excluded on pubnet
    local pubnet_probes_line
    pubnet_probes_line=$(awk '/network: pubnet/{found=1} found && /probes:/{print; exit}' "$WORKFLOW")
    local contract_ok=true
    for excluded in horizon_core_up horizon_ingesting stellar_rpc_healthy; do
        if echo "$pubnet_probes_line" | grep -q "$excluded"; then
            contract_ok=false
            break
        fi
    done
    if $contract_ok; then
        tap_ok "workflow_pubnet_exclusions_match_contract"
    else
        tap_not_ok "workflow_pubnet_exclusions_match_contract" "pubnet includes excluded probe"
    fi

    # Validate probe normalization exists (contract says: underscores to hyphens)
    if grep -q 'probe_name="${probe_name//_/-}"' "$WORKFLOW"; then
        tap_ok "workflow_probe_normalization_matches_contract"
    else
        tap_not_ok "workflow_probe_normalization_matches_contract" "underscore-to-hyphen normalization missing"
    fi

    # Validate that the workflow's validate-contract job fetches internal-build.yml
    if grep -q 'internal-build.yml' "$WORKFLOW"; then
        tap_ok "workflow_validates_internal_build"
    else
        tap_not_ok "workflow_validates_internal_build" "workflow does not fetch/validate internal-build.yml"
    fi

    # Validate that the workflow's validate-contract job reads the contract file
    if grep -q 'CONTRACT="scripts/ci/upstream-quickstart-contract.yml"' "$WORKFLOW" && \
       grep -q 'grep.*"$CONTRACT"' "$WORKFLOW"; then
        tap_ok "workflow_reads_contract_file"
    else
        tap_not_ok "workflow_reads_contract_file" "workflow does not read contract file in validation"
    fi

    # Validate contract documents the drift guard
    if grep -q 'Drift guard' "$CONTRACT" || grep -q 'drift guard' "$CONTRACT"; then
        tap_ok "contract_documents_drift_guard"
    else
        tap_not_ok "contract_documents_drift_guard" "contract should document the SHA == main drift guard"
    fi

    # Validate contract documents the delegated artifact contract
    if grep -q 'delegated_artifact_contract\|internal-build.yml' "$CONTRACT"; then
        tap_ok "contract_documents_delegated_artifact"
    else
        tap_not_ok "contract_documents_delegated_artifact" "contract should document internal-build.yml artifact expectations"
    fi

    # --- Validate the workflow enforces exact {tag}-{arch} artifact-name structure ---
    # The validate-contract job must use a regex that enforces the ordering:
    # image-quickstart-<tag_expr>-<arch_expr> (not just any two interpolations).
    if grep -q "ARTIFACT_REGEX" "$WORKFLOW" && \
       grep -qE 'tag.*arch' "$WORKFLOW" | head -1 && \
       grep -q 'grep -qE.*ARTIFACT_REGEX.*INTERNAL' "$WORKFLOW"; then
        tap_ok "workflow_enforces_exact_artifact_pattern"
    else
        tap_not_ok "workflow_enforces_exact_artifact_pattern" \
            "validate-contract must check internal-build.yml for exact {tag}-{arch} structure"
    fi

    # --- Validate the workflow enforces exact {tag}-{arch} image-tag structure ---
    # Same structural enforcement for image tags (quickstart:<tag>-<arch>).
    if grep -q "TAG_REGEX" "$WORKFLOW" && \
       grep -q 'grep -qE.*TAG_REGEX.*INTERNAL' "$WORKFLOW"; then
        tap_ok "workflow_enforces_exact_image_tag_pattern"
    else
        tap_not_ok "workflow_enforces_exact_image_tag_pattern" \
            "validate-contract must check internal-build.yml for exact image tag {tag}-{arch} structure"
    fi

    # --- Validate the workflow includes a drift guard (SHA == main check) ---
    if grep -q 'git ls-remote.*quickstart.*refs/heads/main' "$WORKFLOW" && \
       grep -q 'SHA.*MAIN_SHA\|MAIN_SHA.*SHA' "$WORKFLOW"; then
        tap_ok "workflow_has_drift_guard"
    else
        tap_not_ok "workflow_has_drift_guard" \
            "validate-contract must verify resolved SHA == main HEAD before build"
    fi
}

# --- Run all tests ---
tap_plan 34

test_timeout_retry_on_targeted_shard
test_non_timeout_failure_no_retry
test_workflow_shard_probe_contract
test_double_timeout_fails
test_non_targeted_timeout_no_retry
test_success_no_retry_artifacts
test_upstream_contract_validation

echo ""
echo "# Results: $PASS_COUNT/$TEST_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    exit 1
fi
exit 0
