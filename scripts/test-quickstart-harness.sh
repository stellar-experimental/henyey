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
#   7. Transient-infra exits 124/143/137 are retryable on the targeted shard
#      only (#3193 adds 137 — SIGKILL runner reclamation)
#   8. The broken in-run rerun-on-transient job is removed from quickstart.yml
#      and recovery lives in a separate workflow_run-triggered workflow
#      (quickstart-retry.yml) that runs AFTER the run completes (#3193)
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
# Recorded by tap_plan so the end-of-run self-check can report a plan/emitted
# mismatch explicitly instead of leaving a silently wrong `1..N` header.
TAP_PLANNED=0

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
    TAP_PLANNED="$1"
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

# extract_pubnet_matrix_block <workflow-file>
# Emits the non-comment lines of the pubnet matrix block: from the
# `network: pubnet` marker up to (but excluding) that shard's `steps:` line.
extract_pubnet_matrix_block() {
    # SIGPIPE-safe (#3835): the early `exit` lives in the awk that reads the file
    # directly, so there is no upstream pipe writer to be killed when the reader
    # stops early. The only downstream reader (grep) drains to EOF. Byte-identical
    # to the original two-awk shape on the committed workflow.
    awk '/network: pubnet/{f=1} f&&/^    steps:/{exit} f{print}' "$1" \
        | grep -vE '^[[:space:]]*#'
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

    # Check: local/galexie shard is soft-degated (#3563). The pinned galexie
    # image (galexie-v26.1.0) never exports a partition against henyey
    # Protocol 27, so test_galexie.go hangs forever and the shard times out
    # (exit 124). The shard must mirror the testnet shard's de-gate so a
    # TIMEOUT (exit 124/137) becomes a neutral SOFT-SKIP (exit 0) while a
    # genuine assertion failure (exit 1) stays RED — and fast-fails in
    # minutes (probe_timeout: 240 + step_timeout_minutes: 25) so the PR's own
    # CI can never reproduce the ~55-min hang. This text assertion guards
    # against a future edit silently dropping any of the three keys (which
    # would re-red-roll `main` on the same incompat — tracked in #3565).
    # Locate the `enable: galexie` matrix entry, then assert the three keys
    # appear within the entry's block (the next ~6 lines, before the next
    # matrix `- network:` entry).
    local galexie_line
    galexie_line=$(grep -nm1 'enable: galexie$' "$WORKFLOW" | cut -d: -f1)
    if [[ -n "$galexie_line" ]]; then
        local galexie_block
        galexie_block=$(sed -n "$((galexie_line)),+6p" "$WORKFLOW")
        if echo "$galexie_block" | grep -q 'soft_on_timeout:[[:space:]]*true'; then
            tap_ok "local_galexie_shard_soft_on_timeout"
        else
            tap_not_ok "local_galexie_shard_soft_on_timeout" \
                "local/galexie matrix entry missing soft_on_timeout: true (#3563 de-gate)"
        fi
        if echo "$galexie_block" | grep -q 'probe_timeout:[[:space:]]*240'; then
            tap_ok "local_galexie_shard_probe_timeout_240"
        else
            tap_not_ok "local_galexie_shard_probe_timeout_240" \
                "local/galexie matrix entry missing probe_timeout: 240 (fast-fail in minutes)"
        fi
        if echo "$galexie_block" | grep -q 'step_timeout_minutes:[[:space:]]*25'; then
            tap_ok "local_galexie_shard_step_timeout_minutes_25"
        else
            tap_not_ok "local_galexie_shard_step_timeout_minutes_25" \
                "local/galexie matrix entry missing step_timeout_minutes: 25 (step-level fast-fail)"
        fi
    else
        tap_not_ok "local_galexie_shard_soft_on_timeout" "no local/galexie shard found"
        tap_not_ok "local_galexie_shard_probe_timeout_240" "no local/galexie shard found"
        tap_not_ok "local_galexie_shard_step_timeout_minutes_25" "no local/galexie shard found"
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
    local pubnet_line pubnet_matches
    # Capture the file-streaming stages fully (both drain to EOF), then run the
    # early-exit `head -1` on the bounded variable — no file-reading writer is
    # left holding a closed pipe (#3835).
    pubnet_matches=$(grep -n 'pubnet' "$WORKFLOW" | grep -v '#')
    pubnet_line=$(printf '%s\n' "$pubnet_matches" | head -1 | cut -d: -f1)
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
    rpc_line=$(grep -nm1 'enable: rpc$' "$WORKFLOW" | cut -d: -f1)
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
# Test 5b: test_local_galexie_soft_skip_is_timeout_only (#3563)
#
# The local/galexie shard is soft-degated (soft_on_timeout: true) because the
# pinned galexie image never exports a partition against henyey Protocol 27,
# so test_galexie.go hangs and the shard times out (exit 124/137). Assert the
# wrapper, when invoked with --soft-on-timeout on the local/galexie shard:
#   (a) converts a TIMEOUT (exit 124) into a neutral soft-skip (exit 0) with a
#       grep-able SOFT-SKIP marker, AND
#   (b) keeps a GENUINE probe assertion failure (exit 1) RED (exit 1) — so a
#       real henyey-emits-bad-meta break (galexie crash / probe assertion) is
#       never masked by the de-gate.
# This is the deterministic counterpart to the live Quickstart run: it proves
# the soft-skip is TIMEOUT-ONLY for the exact local/galexie invocation.
# (local/galexie is NOT a retryable shard, so the soft-skip fires on the
# first-attempt non-retryable timeout sink — single attempt, no retry.)
# ============================================================
test_local_galexie_soft_skip_timeout_only() {
    local diag_dir="$TMPDIR_BASE/diag-test5b"
    mkdir -p "$diag_dir"

    # (a) A probe that always times out, soft-skipped to exit 0 under the flag.
    local timeout_probe
    timeout_probe=$(make_probe "test5b-timeout" 0 999)
    local soft_marker="$diag_dir/soft.log"
    local exit_code=0
    "$WRAPPER" --soft-on-timeout \
        --network local --enable galexie --probe galexie \
        --timeout 2 --diagnostics-dir "$diag_dir/timeout" \
        -- "$timeout_probe" >"$soft_marker" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "local_galexie_timeout_soft_skips_to_zero"
    else
        tap_not_ok "local_galexie_timeout_soft_skips_to_zero" \
            "expected soft-skip exit 0 on timeout, got $exit_code"
    fi
    if grep -q 'SOFT-SKIP' "$soft_marker"; then
        tap_ok "local_galexie_timeout_emits_soft_skip_marker"
    else
        tap_not_ok "local_galexie_timeout_emits_soft_skip_marker" "no SOFT-SKIP marker emitted"
    fi

    # (b) A genuine assertion failure (exit 1) STAYS RED even under the flag.
    local fail_probe
    fail_probe=$(make_probe "test5b-fail" 1 0)
    local fail_exit=0
    "$WRAPPER" --soft-on-timeout \
        --network local --enable galexie --probe galexie \
        --timeout 10 --diagnostics-dir "$diag_dir/fail" \
        -- "$fail_probe" >/dev/null 2>&1 || fail_exit=$?

    if [[ $fail_exit -eq 1 ]]; then
        tap_ok "local_galexie_assertion_failure_stays_red"
    else
        tap_not_ok "local_galexie_assertion_failure_stays_red" \
            "expected exit 1 to stay red under --soft-on-timeout, got $fail_exit"
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
        tap_not_ok "contract_defines_anchored_regexes" "skipped (no contract)"
        tap_not_ok "workflow_artifact_regex_accepts_exact_tar" "skipped (no contract)"
        tap_not_ok "workflow_image_tag_regex_accepts_exact_tag" "skipped (no contract)"
        tap_not_ok "workflow_artifact_regex_rejects_zip" "skipped (no contract)"
        tap_not_ok "workflow_artifact_regex_rejects_double_ext" "skipped (no contract)"
        tap_not_ok "workflow_image_tag_regex_rejects_debug_suffix" "skipped (no contract)"
        tap_not_ok "workflow_uses_anchored_grep" "skipped (no contract)"
        return
    fi

    tap_ok "upstream_contract_file_exists"

    # Extract expected values from contract (simple grep — no YAML parser needed)
    local expected_artifact expected_tag artifact_pattern
    local artifact_regex image_tag_regex
    expected_artifact=$(grep '^expected_artifact_name:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')
    expected_tag=$(grep '^expected_image_tag:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')
    artifact_pattern=$(grep '^artifact_name_pattern:' "$CONTRACT" | sed 's/.*: *"\(.*\)"/\1/')
    # Anchored ERE patterns — the single source of truth shared with the
    # validate-contract workflow job. These enforce the EXACT consumer-facing
    # shape (image-quickstart-{tag}-{arch}.tar / quickstart:{tag}-{arch}) so a
    # suffix drift like .zip / .tar.gz / -debug is rejected, not silently
    # accepted by a substring match.
    # `|| true` so a missing field yields an empty string (reported as a failed
    # assertion below) rather than aborting under `set -euo pipefail`.
    artifact_regex=$(grep '^artifact_name_regex:' "$CONTRACT" | sed 's/^artifact_name_regex: *"\(.*\)"$/\1/' || true)
    image_tag_regex=$(grep '^image_tag_regex:' "$CONTRACT" | sed 's/^image_tag_regex: *"\(.*\)"$/\1/' || true)

    # Validate artifact name in workflow matches contract (anchored to the exact
    # image-quickstart-{tag}-{arch}.tar shape, not a substring).
    if [[ -n "$artifact_regex" ]] && grep -Eq "$artifact_regex" "$WORKFLOW"; then
        tap_ok "workflow_artifact_matches_contract"
    else
        tap_not_ok "workflow_artifact_matches_contract" "expected exact '$expected_artifact' (regex: $artifact_regex) in workflow"
    fi

    # Validate image tag in workflow matches contract (anchored to the exact
    # quickstart:{tag}-{arch} shape, not a substring).
    if [[ -n "$image_tag_regex" ]] && grep -Eq "$image_tag_regex" "$WORKFLOW"; then
        tap_ok "workflow_image_tag_matches_contract"
    else
        tap_not_ok "workflow_image_tag_matches_contract" "expected exact '$expected_tag' (regex: $image_tag_regex) in workflow"
    fi

    # --- Contract defines the anchored ERE regexes (single source of truth) ---
    if [[ -n "$artifact_regex" && -n "$image_tag_regex" ]]; then
        tap_ok "contract_defines_anchored_regexes"
    else
        tap_not_ok "contract_defines_anchored_regexes" \
            "contract must define artifact_name_regex: and image_tag_regex: anchored ERE strings"
    fi

    # --- Positive controls: anchored regexes accept the exact expected strings ---
    if [[ -n "$artifact_regex" ]] && echo "$expected_artifact" | grep -Eq "$artifact_regex"; then
        tap_ok "workflow_artifact_regex_accepts_exact_tar"
    else
        tap_not_ok "workflow_artifact_regex_accepts_exact_tar" \
            "anchored artifact regex must accept '$expected_artifact'"
    fi
    if [[ -n "$image_tag_regex" ]] && echo "$expected_tag" | grep -Eq "$image_tag_regex"; then
        tap_ok "workflow_image_tag_regex_accepts_exact_tag"
    else
        tap_not_ok "workflow_image_tag_regex_accepts_exact_tag" \
            "anchored image-tag regex must accept '$expected_tag'"
    fi

    # --- Negative cases: incompatible suffixes must be REJECTED ---
    # A future relaxation of the anchored regexes to a substring match would
    # let these through and turn these assertions red — that is the point.
    # Derive the negative fixtures from the contract's expected values so they
    # track the contract as it evolves: if expected_artifact_name /
    # expected_image_tag ever change, these still exercise genuine suffix
    # rejection (rather than passing because a stale hard-coded tag/arch no
    # longer matches the regex for an unrelated reason).
    #   - .zip      : swap the trailing .tar for .zip
    #   - .tar.gz   : append .gz to the full .tar artifact name
    #   - -debug    : append -debug to the full image tag
    local negative_artifact_zip negative_artifact_double_ext negative_tag_debug
    negative_artifact_zip="${expected_artifact%.tar}.zip"
    negative_artifact_double_ext="${expected_artifact}.gz"
    negative_tag_debug="${expected_tag}-debug"
    if [[ -n "$artifact_regex" ]] && ! echo "$negative_artifact_zip" | grep -Eq "$artifact_regex"; then
        tap_ok "workflow_artifact_regex_rejects_zip"
    else
        tap_not_ok "workflow_artifact_regex_rejects_zip" \
            "anchored artifact regex must reject the .zip suffix ($negative_artifact_zip)"
    fi
    if [[ -n "$artifact_regex" ]] && ! echo "$negative_artifact_double_ext" | grep -Eq "$artifact_regex"; then
        tap_ok "workflow_artifact_regex_rejects_double_ext"
    else
        tap_not_ok "workflow_artifact_regex_rejects_double_ext" \
            "anchored artifact regex must reject the .tar.gz double extension ($negative_artifact_double_ext)"
    fi
    if [[ -n "$image_tag_regex" ]] && ! echo "$negative_tag_debug" | grep -Eq "$image_tag_regex"; then
        tap_ok "workflow_image_tag_regex_rejects_debug_suffix"
    else
        tap_not_ok "workflow_image_tag_regex_rejects_debug_suffix" \
            "anchored image-tag regex must reject the -debug suffix ($negative_tag_debug)"
    fi

    # --- Workflow consumer-side checks use anchored grep -Eq (not bare prefix) ---
    if grep -q 'grep -Eq "\$ARTIFACT_NAME_REGEX"' "$WORKFLOW" && \
       grep -q 'grep -Eq "\$IMAGE_TAG_REGEX"' "$WORKFLOW"; then
        tap_ok "workflow_uses_anchored_grep"
    else
        tap_not_ok "workflow_uses_anchored_grep" \
            "validate-contract must read the anchored regexes and check the consumer lines with grep -Eq"
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

# ============================================================
# Test 8: test_workflow_uses_upstream_run_attempt_timeout_budget
#
# Regression for #2920. The #2916 rewrite hardcoded PROBE_TIMEOUT=600 (a flat
# 10-min per-probe budget), silently diverging from upstream
# stellar/quickstart/.github/workflows/internal-test.yml, which computes the
# per-probe timeout as `github.run_attempt * timeout_multiplier` minutes with
# timeout_multiplier=4 (4 min on attempt 1, escalating on manual re-runs).
#
# This test locks the restored upstream formula: the workflow must (a) define
# a workflow-level `timeout_multiplier: 4`, (b) compute PROBE_TIMEOUT from
# github.run_attempt, timeout_multiplier, and a *60 minutes→seconds
# conversion, and (c) NOT hardcode the divergent PROBE_TIMEOUT=600.
# ============================================================
test_workflow_uses_upstream_run_attempt_timeout_budget() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "workflow_defines_timeout_multiplier_4" "workflow file not found"
        tap_not_ok "workflow_computes_probe_timeout_from_run_attempt" "workflow file not found"
        tap_not_ok "workflow_does_not_hardcode_probe_timeout_600" "workflow file not found"
        return
    fi

    # (a) workflow-level env defines timeout_multiplier: 4 (mirrors upstream).
    if grep -qE '^[[:space:]]*timeout_multiplier:[[:space:]]*4[[:space:]]*$' "$WORKFLOW"; then
        tap_ok "workflow_defines_timeout_multiplier_4"
    else
        tap_not_ok "workflow_defines_timeout_multiplier_4" \
            "workflow must define 'timeout_multiplier: 4' (upstream internal-test.yml env)"
    fi

    # (b) PROBE_TIMEOUT is computed from run_attempt * timeout_multiplier * 60.
    # `|| true` so a no-match doesn't abort under `set -e`.
    local probe_timeout_expr
    probe_timeout_expr=$( (grep -m1 -E 'PROBE_TIMEOUT=' "$WORKFLOW" || true) )
    if echo "$probe_timeout_expr" | grep -q 'github.run_attempt' && \
       echo "$probe_timeout_expr" | grep -q 'timeout_multiplier' && \
       echo "$probe_timeout_expr" | grep -q '\* 60'; then
        tap_ok "workflow_computes_probe_timeout_from_run_attempt"
    else
        tap_not_ok "workflow_computes_probe_timeout_from_run_attempt" \
            "PROBE_TIMEOUT must be github.run_attempt * timeout_multiplier * 60; got: $probe_timeout_expr"
    fi

    # (c) the divergent hardcoded 600 literal must be gone.
    if grep -qE 'PROBE_TIMEOUT=600([^0-9]|$)' "$WORKFLOW"; then
        tap_not_ok "workflow_does_not_hardcode_probe_timeout_600" \
            "workflow still hardcodes PROBE_TIMEOUT=600 (divergent flat 10-min budget)"
    else
        tap_ok "workflow_does_not_hardcode_probe_timeout_600"
    fi
}

# ============================================================
# Test 9: test_timeout_budget_matches_upstream_formula
#
# Text/structure assertion (the harness runs locally and cannot evaluate the
# GitHub `github.run_attempt` expression — per Critic A). Asserts the
# workflow's PROBE_TIMEOUT expression text contains all three upstream-formula
# terms (run_attempt, the timeout_multiplier env, and the *60 conversion) and
# that the multiplier value the workflow uses equals upstream's 4.
# ============================================================
test_timeout_budget_matches_upstream_formula() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "workflow_timeout_expression_has_all_upstream_terms" "workflow file not found"
        tap_not_ok "workflow_multiplier_equals_upstream_4" "workflow file not found"
        return
    fi

    # All three formula terms present in the single PROBE_TIMEOUT expression.
    # `|| true` so a no-match doesn't abort under `set -e`.
    local probe_timeout_expr
    probe_timeout_expr=$( (grep -m1 -E 'PROBE_TIMEOUT=' "$WORKFLOW" || true) )
    if echo "$probe_timeout_expr" | grep -q 'github.run_attempt' && \
       echo "$probe_timeout_expr" | grep -q 'timeout_multiplier' && \
       echo "$probe_timeout_expr" | grep -q '\* 60'; then
        tap_ok "workflow_timeout_expression_has_all_upstream_terms"
    else
        tap_not_ok "workflow_timeout_expression_has_all_upstream_terms" \
            "expression must contain github.run_attempt, timeout_multiplier, and * 60; got: $probe_timeout_expr"
    fi

    # The multiplier value equals upstream's 4.
    # `|| true` so a no-match doesn't abort under `set -e`.
    local workflow_multiplier
    workflow_multiplier=$( (grep -m1 -E '^[[:space:]]*timeout_multiplier:[[:space:]]*[0-9]+' "$WORKFLOW" || true) \
        | sed -E 's/.*timeout_multiplier:[[:space:]]*([0-9]+).*/\1/')
    if [[ "$workflow_multiplier" == "4" ]]; then
        tap_ok "workflow_multiplier_equals_upstream_4"
    else
        tap_not_ok "workflow_multiplier_equals_upstream_4" \
            "workflow timeout_multiplier='$workflow_multiplier', upstream is 4"
    fi
}

# ============================================================
# Test 10: test_contract_pins_timeout_multiplier
#
# The pinned upstream contract must record timeout_multiplier: 4 so that
# multiplier drift in upstream internal-test.yml is caught by validate-contract
# and by the harness rather than silently diverging again (the #2920 goal).
# ============================================================
test_contract_pins_timeout_multiplier() {
    if [[ ! -f "$CONTRACT" ]]; then
        tap_not_ok "contract_pins_timeout_multiplier_4" "contract file not found"
        return
    fi

    if grep -qE '^[[:space:]]*timeout_multiplier:[[:space:]]*4[[:space:]]*$' "$CONTRACT"; then
        tap_ok "contract_pins_timeout_multiplier_4"
    else
        tap_not_ok "contract_pins_timeout_multiplier_4" \
            "contract must pin 'timeout_multiplier: 4'"
    fi
}

# ============================================================
# Test 11: test_retry_is_layered_on_top_of_budget
#
# Triage test obligation #3: the single targeted retry must be layered ON TOP
# OF the upstream per-attempt budget, not a replacement of it. We drive the
# wrapper with a probe that times out once (exit 124) then succeeds on the
# targeted shard, capturing the `timeout <seconds>` command line the wrapper
# logs for each attempt. Both attempt 1 and attempt 2 must use the SAME
# --timeout budget that was passed in (here 3s), proving the retry adds an
# extra attempt at the same per-attempt budget rather than shrinking/growing it.
# ============================================================
test_retry_is_layered_on_top_of_budget() {
    local diag_dir="$TMPDIR_BASE/diag-test11"
    mkdir -p "$diag_dir"

    # Probe: times out (sleep 999) on attempt 1, succeeds on attempt 2.
    local state_file="$TMPDIR_BASE/state-test11"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test11.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    sleep 999
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test11.txt"
    local budget=3
    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout "$budget" --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || exit_code=$?

    # The wrapper logs "Command: timeout -k <grace> <budget>s ..." for each
    # attempt (the `-k <grace>` is the #3273 kill-after force-kill). The
    # per-attempt budget is the duration that immediately follows the `-k
    # <grace>` token. Both attempts must use the same budget that was passed in.
    # We extract the budget position (the SECOND duration on the timeout line,
    # after the `-k <grace>` first duration) so the assertion tracks the budget,
    # not the fixed grace.
    # `|| true` so a no-match (grep exit 1) doesn't abort under `set -e`.
    local attempt_budgets
    attempt_budgets=$( (grep -oE "timeout -k [0-9]+s ${budget}s" "$wrapper_log" || true) | wc -l | tr -d ' ')
    local other_budgets
    other_budgets=$( (grep -oE 'timeout -k [0-9]+s [0-9]+s' "$wrapper_log" || true) \
        | (grep -vE "timeout -k [0-9]+s ${budget}s" || true) | wc -l | tr -d ' ')

    if [[ $exit_code -eq 0 && "$attempt_budgets" == "2" && "$other_budgets" == "0" ]]; then
        tap_ok "retry_uses_same_budget_as_first_attempt"
    else
        tap_not_ok "retry_uses_same_budget_as_first_attempt" \
            "exit=$exit_code attempts_at_${budget}s=$attempt_budgets other_budgets=$other_budgets"
    fi
}

# ============================================================
# Test 12: test_runner_shutdown_exit143_retries_on_targeted_shard
#
# Regression for #3131. The testnet/core,horizon/horizon-core-up probe flakes
# with exit 143 ("the runner has received a shutdown signal" — spot-runner
# reclamation / the probe subprocess receiving SIGTERM, 128+15=143) in addition
# to the timeout (exit 124) class already handled. The wrapper must treat
# exit 143 as a retryable transient on the targeted shard, exactly like exit
# 124: re-run once, and pass if the retry succeeds.
# ============================================================
test_runner_shutdown_exit143_retries_on_targeted_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test12"
    mkdir -p "$diag_dir"

    # Probe: exits 143 (runner-shutdown signature) on attempt 1, succeeds on 2.
    local state_file="$TMPDIR_BASE/state-test12"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test12.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    exit 143
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_runner_shutdown_exit143_retries_on_targeted_shard"
    else
        tap_not_ok "test_runner_shutdown_exit143_retries_on_targeted_shard" "exit=$exit_code (expected 0 via retry)"
    fi

    # The first (exit-143) attempt must have captured diagnostics, proving the
    # probe failed on attempt 1 and the overall exit 0 came from the retry path.
    # (A successful retry exits 0 and so leaves no attempt-2 diagnostics dir —
    # diagnostics are only captured on a non-zero attempt; this mirrors the
    # exit-124 retry test's attempt-1 assertion.)
    if [[ -d "$diag_dir/attempt-1" ]]; then
        tap_ok "exit143_retry_captured_attempt1_diagnostics"
    else
        tap_not_ok "exit143_retry_captured_attempt1_diagnostics" "no attempt-1 dir (first failure not captured)"
    fi
}

# ============================================================
# Test 13: test_runner_shutdown_exit143_does_not_retry_on_non_targeted_shard
#
# The exit-143 retry must be scoped to the same targeted shard as the exit-124
# retry — a non-targeted shard exiting 143 must fail immediately with no retry,
# so a genuine henyey failure elsewhere stays loud.
# ============================================================
test_runner_shutdown_exit143_no_retry_on_non_targeted_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test13"
    mkdir -p "$diag_dir"

    # Probe exits 143 immediately on a non-targeted shard.
    local probe
    probe=$(make_probe "test13" 143 0)

    local exit_code=0
    "$WRAPPER" \
        --network local --enable "core" --probe core \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_runner_shutdown_exit143_does_not_retry_on_non_targeted_shard"
    else
        tap_not_ok "test_runner_shutdown_exit143_does_not_retry_on_non_targeted_shard" "expected failure"
    fi

    if [[ ! -d "$diag_dir/attempt-2" ]]; then
        tap_ok "non_targeted_exit143_single_attempt"
    else
        tap_not_ok "non_targeted_exit143_single_attempt" "unexpected retry on non-targeted shard"
    fi
}

# ============================================================
# Test 14: test_runner_shutdown_exit143_still_fails_after_second_attempt
#
# If exit 143 recurs on the retry, the wrapper must still fail (the retry is a
# single additional attempt, not an unbounded loop). This proves the exit-143
# retry mirrors the exit-124 double-failure semantics.
# ============================================================
test_runner_shutdown_exit143_double_failure_fails() {
    local diag_dir="$TMPDIR_BASE/diag-test14"
    mkdir -p "$diag_dir"

    # Probe always exits 143.
    local probe
    probe=$(make_probe "test14" 143 0)

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_runner_shutdown_exit143_still_fails_after_second_attempt"
    else
        tap_not_ok "test_runner_shutdown_exit143_still_fails_after_second_attempt" "expected failure"
    fi

    if [[ -d "$diag_dir/attempt-1" && -d "$diag_dir/attempt-2" ]]; then
        tap_ok "exit143_double_failure_preserves_both_diagnostics"
    else
        tap_not_ok "exit143_double_failure_preserves_both_diagnostics" "missing attempt dirs"
    fi
}

# ============================================================
# Test 15: test_transient_retry_covers_whole_testnet_shard_not_just_horizon_core_up
#
# Regression for #3185. The retry was originally scoped to the single probe
# horizon-core-up. But testnet stellar-core's slow catchup propagates to the
# NEXT startup-dependent probe: in run 27019344504 (069ebfcc), horizon-ingesting
# timed out (exit 124) right after horizon-core-up finally came up, and because
# the retry only matched horizon-core-up it hard-failed as "not retryable".
# The retry is now scoped to the whole testnet/core,horizon shard, so a
# transient-infra exit on ANY probe of that shard (here: horizon-ingesting)
# self-heals via the single retry.
# ============================================================
test_transient_retry_covers_whole_testnet_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test15"
    mkdir -p "$diag_dir"

    # Probe times out (124) on first attempt, succeeds on second — same shape
    # as the green-baseline horizon-core-up flake, but on horizon-ingesting.
    local state_file="$TMPDIR_BASE/state-test15"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test15.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    sleep 999
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-ingesting \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_transient_retry_covers_whole_testnet_shard_not_just_horizon_core_up"
    else
        tap_not_ok "test_transient_retry_covers_whole_testnet_shard_not_just_horizon_core_up" "exit=$exit_code (horizon-ingesting on testnet/core,horizon should retry)"
    fi

    # Attempt 1 failed (timeout 124) so its diagnostics were captured; the
    # overall exit 0 above proves attempt 2 ran and passed. A successful retry
    # captures no diagnostics (capture is failure-only), so we assert on the
    # attempt-1 dir, not attempt-2.
    if [[ -d "$diag_dir/attempt-1" ]]; then
        tap_ok "shard_scope_retry_captured_failed_first_attempt"
    else
        tap_not_ok "shard_scope_retry_captured_failed_first_attempt" "no attempt-1 dir (first attempt not captured)"
    fi
}

# ============================================================
# Test 16: test_exit143_retries_on_non_horizon_core_up_testnet_probe
#
# Regression for #3185. The exit-143 (SIGTERM) retry must also cover the whole
# testnet/core,horizon shard, not just horizon-core-up — a runner SIGTERM can
# land on any probe. Here a horizon-ingesting probe exits 143 once then passes.
# ============================================================
test_exit143_retries_on_non_horizon_core_up_testnet_probe() {
    local diag_dir="$TMPDIR_BASE/diag-test16"
    mkdir -p "$diag_dir"

    local state_file="$TMPDIR_BASE/state-test16"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test16.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    exit 143
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-ingesting \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_exit143_retries_on_non_horizon_core_up_testnet_probe"
    else
        tap_not_ok "test_exit143_retries_on_non_horizon_core_up_testnet_probe" "exit=$exit_code"
    fi
}

# ============================================================
# Test 17: test_exit137_retries_on_targeted_shard
#
# Regression for #3193. Whole-runner reclamation on PR #3187's run
# 27037187429 produced exit 137 (SIGKILL, 128+9) — "The runner has received a
# shutdown signal" followed by "exit code 137" — NOT the 143 (SIGTERM) the
# wrapper already covered. When the runner survives the SIGKILL of the probe
# subprocess long enough for the wrapper to observe the 137, re-running once
# self-heals. The wrapper must treat exit 137 as a retryable transient on the
# targeted shard, exactly like exit 124/143.
# ============================================================
test_exit137_retries_on_targeted_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test17"
    mkdir -p "$diag_dir"

    # Probe: exits 137 (SIGKILL signature) on attempt 1, succeeds on 2.
    local state_file="$TMPDIR_BASE/state-test17"
    echo "0" > "$state_file"
    local probe="$TMPDIR_BASE/probe-test17.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
STATE_FILE="__STATE__"
COUNT=$(cat "$STATE_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$STATE_FILE"
if [[ $COUNT -eq 1 ]]; then
    exit 137
fi
exit 0
EOF
    sed -i "s|__STATE__|$state_file|g" "$probe"
    chmod +x "$probe"

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_exit137_retries_on_targeted_shard"
    else
        tap_not_ok "test_exit137_retries_on_targeted_shard" "exit=$exit_code (expected 0 via retry)"
    fi

    if [[ -d "$diag_dir/attempt-1" ]]; then
        tap_ok "exit137_retry_captured_attempt1_diagnostics"
    else
        tap_not_ok "exit137_retry_captured_attempt1_diagnostics" "no attempt-1 dir (first failure not captured)"
    fi
}

# ============================================================
# Test 18: test_exit137_does_not_retry_on_non_targeted_shard
#
# The exit-137 retry must be scoped to the same targeted shard as 124/143 — a
# non-targeted shard exiting 137 must fail immediately with no retry, so a
# genuine henyey failure elsewhere stays loud.
# ============================================================
test_exit137_no_retry_on_non_targeted_shard() {
    local diag_dir="$TMPDIR_BASE/diag-test18"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test18" 137 0)

    local exit_code=0
    "$WRAPPER" \
        --network local --enable "core" --probe core \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_exit137_does_not_retry_on_non_targeted_shard"
    else
        tap_not_ok "test_exit137_does_not_retry_on_non_targeted_shard" "expected failure"
    fi

    if [[ ! -d "$diag_dir/attempt-2" ]]; then
        tap_ok "non_targeted_exit137_single_attempt"
    else
        tap_not_ok "non_targeted_exit137_single_attempt" "unexpected retry on non-targeted shard"
    fi
}

# ============================================================
# Test 19: test_exit137_still_fails_after_second_attempt
#
# If exit 137 recurs on the retry, the wrapper must still fail (single extra
# attempt, not an unbounded loop) — mirrors the 124/143 double-failure case.
# ============================================================
test_exit137_double_failure_fails() {
    local diag_dir="$TMPDIR_BASE/diag-test19"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test19" 137 0)

    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_exit137_still_fails_after_second_attempt"
    else
        tap_not_ok "test_exit137_still_fails_after_second_attempt" "expected failure"
    fi

    if [[ -d "$diag_dir/attempt-1" && -d "$diag_dir/attempt-2" ]]; then
        tap_ok "exit137_double_failure_preserves_both_diagnostics"
    else
        tap_not_ok "exit137_double_failure_preserves_both_diagnostics" "missing attempt dirs"
    fi
}

# ============================================================
# Test 20: test_in_run_rerun_job_removed_from_quickstart_workflow
#
# Regression for #3193. The in-run `rerun-on-transient` job in quickstart.yml
# tried to `gh run rerun <id> --failed` for ITS OWN still-running run, which
# fails with "This workflow is already running" (exit 1) — the auto-retry never
# fired, it only added a spurious FAILURE check. The broken in-run job must be
# REMOVED from quickstart.yml entirely; recovery moves to a separate
# workflow_run-triggered workflow (Test 21).
# ============================================================
test_in_run_rerun_job_removed() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "test_in_run_rerun_job_removed_from_quickstart_workflow" "workflow file not found"
        return
    fi

    # The broken in-run job key must be gone.
    if ! grep -q '^  rerun-on-transient:' "$WORKFLOW"; then
        tap_ok "in_run_rerun_job_key_removed"
    else
        tap_not_ok "in_run_rerun_job_key_removed" \
            "rerun-on-transient job still present in quickstart.yml (re-dispatches its own running run -> 'already running')"
    fi

    # No `gh run rerun` left inside the main workflow at all — the same-run
    # rerun is the exact bug; recovery belongs in the separate workflow.
    if ! grep -q 'gh run rerun' "$WORKFLOW"; then
        tap_ok "no_in_run_gh_run_rerun_in_quickstart"
    else
        tap_not_ok "no_in_run_gh_run_rerun_in_quickstart" \
            "quickstart.yml still calls gh run rerun in-run (same-run rerun fails 'already running')"
    fi
}

# ============================================================
# Test 21: test_separate_workflow_run_retry_workflow_recovers_transient
#
# Regression for #3193. Recovery from a whole-runner reclamation must live in a
# SEPARATE workflow triggered by `workflow_run` (types: completed) on the
# Quickstart workflow, so it runs AFTER the Quickstart run finishes and
# `gh run rerun <id> --failed` works (no "already running"). Assert the new
# workflow file exists and:
#   * triggers on workflow_run / completed
#   * names the EXACT Quickstart workflow (must match quickstart.yml's `name:`)
#   * is bounded: guards on conclusion == failure AND run_attempt == 1
#   * grants actions: write
#   * re-dispatches via `gh run rerun <workflow_run.id> --failed`
# ============================================================
test_separate_workflow_run_retry_workflow() {
    local retry_wf="$REPO_ROOT/.github/workflows/quickstart-retry.yml"

    if [[ ! -f "$retry_wf" ]]; then
        tap_not_ok "retry_workflow_file_exists" "quickstart-retry.yml not found"
        tap_not_ok "retry_workflow_triggers_on_workflow_run_completed" "no file"
        tap_not_ok "retry_workflow_name_matches_quickstart_exactly" "no file"
        tap_not_ok "retry_workflow_guards_on_failure_conclusion" "no file"
        tap_not_ok "retry_workflow_bounded_to_first_attempt" "no file"
        tap_not_ok "retry_workflow_has_actions_write_permission" "no file"
        tap_not_ok "retry_workflow_reruns_the_triggering_run" "no file"
        return
    fi
    tap_ok "retry_workflow_file_exists"

    # Triggers on workflow_run / completed.
    if grep -q 'workflow_run:' "$retry_wf" && grep -qE 'types:.*completed|-\s*completed' "$retry_wf"; then
        tap_ok "retry_workflow_triggers_on_workflow_run_completed"
    else
        tap_not_ok "retry_workflow_triggers_on_workflow_run_completed" \
            "must trigger on: workflow_run: { types: [completed] }"
    fi

    # The workflows: list must name the EXACT Quickstart workflow name string
    # as declared by quickstart.yml's top-level `name:` field. A mismatch means
    # the trigger never fires.
    local quickstart_name
    quickstart_name=$(grep -m1 -E '^name:' "$WORKFLOW" | sed -E 's/^name:[[:space:]]*//; s/^"(.*)"$/\1/; s/^'"'"'(.*)'"'"'$/\1/')
    if [[ -n "$quickstart_name" ]] && grep -qF "$quickstart_name" "$retry_wf"; then
        tap_ok "retry_workflow_name_matches_quickstart_exactly"
    else
        tap_not_ok "retry_workflow_name_matches_quickstart_exactly" \
            "workflow_run.workflows must list the exact name '$quickstart_name' from quickstart.yml"
    fi

    # Guard: only act on a failure conclusion.
    if grep -q "workflow_run.conclusion == 'failure'" "$retry_wf"; then
        tap_ok "retry_workflow_guards_on_failure_conclusion"
    else
        tap_not_ok "retry_workflow_guards_on_failure_conclusion" \
            "must guard on github.event.workflow_run.conclusion == 'failure'"
    fi

    # Bounded one-shot: only re-run when the triggering run was its first attempt
    # (run_attempt == 1). Attempt 2 carries run_attempt == 2, so the guard fails
    # and no further rerun is dispatched — cannot loop.
    if grep -q 'workflow_run.run_attempt == 1' "$retry_wf"; then
        tap_ok "retry_workflow_bounded_to_first_attempt"
    else
        tap_not_ok "retry_workflow_bounded_to_first_attempt" \
            "must guard on github.event.workflow_run.run_attempt == 1 (bounded one-shot, no loop)"
    fi

    # Needs actions: write to re-dispatch.
    if grep -q 'actions: write' "$retry_wf"; then
        tap_ok "retry_workflow_has_actions_write_permission"
    else
        tap_not_ok "retry_workflow_has_actions_write_permission" "missing actions: write permission"
    fi

    # Re-dispatches the TRIGGERING run (workflow_run.id), not its own run, with --failed.
    if grep -q 'gh run rerun' "$retry_wf" \
        && grep -q -- '--failed' "$retry_wf" \
        && grep -q 'workflow_run.id' "$retry_wf"; then
        tap_ok "retry_workflow_reruns_the_triggering_run"
    else
        tap_not_ok "retry_workflow_reruns_the_triggering_run" \
            "must run 'gh run rerun \${{ github.event.workflow_run.id }} --failed'"
    fi
}

# ============================================================
# Test 22: test_soft_on_timeout_testnet_timeout_is_neutral_skip
#
# Regression for #3272. The chronic external-testnet flake red-rolls main when a
# stuck testnet sync probe times out (exit 124) on both the first attempt and
# the single retry. With the opt-in --soft-on-timeout flag (used only by the
# testnet shard), a double-timeout (exit 124) must be converted to a neutral
# exit 0 with a grep-able SOFT-SKIP marker, so an environmental testnet hang no
# longer blocks the merge gate — while diagnostics are still captured.
# FAILS on origin/main: the --soft-on-timeout flag does not exist there, so a
# double-timeout on the testnet shard exits non-zero (see
# test_targeted_timeout_still_fails_after_second_attempt).
# ============================================================
test_soft_on_timeout_testnet_timeout_is_neutral_skip() {
    local diag_dir="$TMPDIR_BASE/diag-test22"
    mkdir -p "$diag_dir"

    # Probe that always sleeps (always times out → exit 124 on both attempts).
    local probe
    probe=$(make_probe "test22" 0 999)

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test22.txt"
    local exit_code=0
    "$WRAPPER" \
        --soft-on-timeout \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || exit_code=$?

    # Under --soft-on-timeout, a double-timeout (124) is a neutral soft-skip.
    if [[ $exit_code -eq 0 ]]; then
        tap_ok "test_soft_on_timeout_testnet_timeout_is_neutral_skip"
    else
        tap_not_ok "test_soft_on_timeout_testnet_timeout_is_neutral_skip" \
            "exit=$exit_code (expected 0 soft-skip on a testnet timeout)"
    fi

    # A grep-able SOFT-SKIP marker must be emitted so the soft-skip is observable.
    if grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "soft_on_timeout_emits_soft_skip_marker"
    else
        tap_not_ok "soft_on_timeout_emits_soft_skip_marker" "no SOFT-SKIP marker in wrapper output"
    fi
}

# ============================================================
# Test 23: test_soft_on_timeout_still_fails_real_assertion
#
# The soft-skip MUST NOT mask a genuine probe assertion failure. A probe that
# exits 1 (a real henyey-on-testnet break, NOT a timeout) under
# --soft-on-timeout must still exit non-zero, and GNU `timeout` propagates the
# child's exit code unchanged for non-timeout exits, so the wrapper exit is
# exactly 1 — not 0 and not soft-skipped.
# ============================================================
test_soft_on_timeout_still_fails_real_assertion() {
    local diag_dir="$TMPDIR_BASE/diag-test23"
    mkdir -p "$diag_dir"

    # Probe that exits 1 immediately (assertion failure, non-124).
    local probe
    probe=$(make_probe "test23" 1 0)

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test23.txt"
    local exit_code=0
    "$WRAPPER" \
        --soft-on-timeout \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 10 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || exit_code=$?

    # A non-124 assertion failure stays RED even under --soft-on-timeout, and
    # the child's exit code (1) propagates unchanged.
    if [[ $exit_code -eq 1 ]]; then
        tap_ok "test_soft_on_timeout_still_fails_real_assertion"
    else
        tap_not_ok "test_soft_on_timeout_still_fails_real_assertion" \
            "exit=$exit_code (expected 1 — a real assertion failure must NOT be soft-skipped)"
    fi

    # The SOFT-SKIP marker must NOT be emitted for a genuine assertion failure.
    if ! grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "soft_on_timeout_does_not_soft_skip_real_assertion"
    else
        tap_not_ok "soft_on_timeout_does_not_soft_skip_real_assertion" \
            "SOFT-SKIP marker emitted for a non-124 assertion failure (masks a real break)"
    fi
}

# ============================================================
# Test 24: test_soft_on_timeout_preserves_diagnostics
#
# A soft-skipped timeout must still produce attempt-* diagnostics dirs so a
# degraded-testnet event remains observable in the uploaded CI artifacts.
# ============================================================
test_soft_on_timeout_preserves_diagnostics() {
    local diag_dir="$TMPDIR_BASE/diag-test24"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test24" 0 999)

    local exit_code=0
    "$WRAPPER" \
        --soft-on-timeout \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>&1 || exit_code=$?

    # Soft-skip exits 0 but both timed-out attempts must have left diagnostics.
    if [[ -d "$diag_dir/attempt-1" && -d "$diag_dir/attempt-2" ]]; then
        tap_ok "test_soft_on_timeout_preserves_diagnostics"
    else
        tap_not_ok "test_soft_on_timeout_preserves_diagnostics" \
            "missing attempt diagnostics dirs on soft-skip (exit=$exit_code)"
    fi
}

# ============================================================
# Test 25: test_soft_on_timeout_off_by_default_preserves_red
#
# Scope guard: WITHOUT --soft-on-timeout, a double-timeout (124) on the testnet
# shard must still return the original non-zero exit — proving the default
# behavior is byte-identical for every existing caller / other shard and the
# soft-skip is strictly opt-in.
# ============================================================
test_soft_on_timeout_off_by_default_preserves_red() {
    local diag_dir="$TMPDIR_BASE/diag-test25"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test25" 0 999)

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test25.txt"
    local exit_code=0
    "$WRAPPER" \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || exit_code=$?

    # Default (no flag): a double-timeout stays RED, exactly as before.
    if [[ $exit_code -ne 0 ]]; then
        tap_ok "test_soft_on_timeout_off_by_default_preserves_red"
    else
        tap_not_ok "test_soft_on_timeout_off_by_default_preserves_red" \
            "exit=$exit_code (without the flag a timeout must stay non-zero)"
    fi

    # No SOFT-SKIP marker without the flag.
    if ! grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "no_soft_skip_marker_without_flag"
    else
        tap_not_ok "no_soft_skip_marker_without_flag" "SOFT-SKIP emitted without --soft-on-timeout"
    fi
}

# ============================================================
# Test 26: test_workflow_testnet_shard_uses_soft_timeout_and_tight_budget
#
# New coverage (#3272). The workflow must pass --soft-on-timeout and a tighter
# (sub-600s) per-probe --timeout to the testnet shard ONLY, and must NOT pass
# the soft flag to the pubnet/local shards (scope guard). Implemented via the
# matrix: the testnet entry carries a `soft_on_timeout` flag and a
# `probe_timeout` override; the probe loop appends --soft-on-timeout only when
# the matrix value is set, and uses the per-shard override when present.
# ============================================================
test_workflow_testnet_shard_uses_soft_timeout_and_tight_budget() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "workflow_testnet_shard_sets_soft_on_timeout" "workflow file not found"
        tap_not_ok "workflow_testnet_shard_has_sub_600_timeout" "workflow file not found"
        tap_not_ok "workflow_loop_passes_soft_flag_conditionally" "workflow file not found"
        tap_not_ok "workflow_pubnet_local_shards_have_no_soft_flag" "workflow file not found"
        return
    fi

    # The testnet matrix entry must set soft_on_timeout: true.
    local testnet_block
    testnet_block=$(awk '/network: testnet/{f=1} f{print} /network: pubnet/{if(f)exit}' "$WORKFLOW")
    if echo "$testnet_block" | grep -qE 'soft_on_timeout:[[:space:]]*true'; then
        tap_ok "workflow_testnet_shard_sets_soft_on_timeout"
    else
        tap_not_ok "workflow_testnet_shard_sets_soft_on_timeout" \
            "testnet matrix entry must set 'soft_on_timeout: true'"
    fi

    # The testnet matrix entry must set a tighter per-probe timeout < 600s.
    # `|| true` so a no-match doesn't abort under `set -e`.
    local testnet_timeout
    testnet_timeout=$( (echo "$testnet_block" | grep -oE 'probe_timeout:[[:space:]]*[0-9]+' || true) \
        | head -1 | grep -oE '[0-9]+' || true)
    if [[ -n "$testnet_timeout" && "$testnet_timeout" -lt 600 ]]; then
        tap_ok "workflow_testnet_shard_has_sub_600_timeout"
    else
        tap_not_ok "workflow_testnet_shard_has_sub_600_timeout" \
            "testnet matrix entry must set a sub-600s 'probe_timeout' override; got: '$testnet_timeout'"
    fi

    # The probe loop must append --soft-on-timeout conditionally (only when the
    # matrix flag is set), not unconditionally.
    if grep -q -- '--soft-on-timeout' "$WORKFLOW" && grep -qE 'soft_on_timeout' "$WORKFLOW"; then
        tap_ok "workflow_loop_passes_soft_flag_conditionally"
    else
        tap_not_ok "workflow_loop_passes_soft_flag_conditionally" \
            "probe loop must pass --soft-on-timeout gated on the matrix soft_on_timeout value"
    fi

    # Scope guard: pubnet and the NON-galexie local shards must NOT carry
    # soft_on_timeout: true. soft_on_timeout is scoped to exactly two shards —
    # testnet/core,horizon (#3272 external-liveness flake) and local/galexie
    # (#3563 galexie-image / Protocol-27 export incompat) — and must never
    # broadcast to the henyey-correctness-bearing local shards (core, rpc,
    # core,rpc,horizon) or pubnet. Match only the real YAML matrix KEY (an
    # actual `soft_on_timeout: true` line), not prose inside a `#` comment that
    # mentions the flag — so strip comment lines before grepping.
    local pubnet_block non_galexie_local_blocks
    pubnet_block=$(extract_pubnet_matrix_block "$WORKFLOW")
    # Non-galexie local blocks: from the first matrix include up to the testnet
    # entry, with the `enable: galexie` block (the line itself + its 3 override
    # keys) removed so the intentional #3563 de-gate doesn't trip this guard.
    non_galexie_local_blocks=$(awk '/include:/{f=1} f && /network: testnet/{exit} f{print}' "$WORKFLOW" \
        | grep -vE '^[[:space:]]*#' \
        | awk '/enable: galexie$/{skip=1} skip && /network:/ && !/enable: galexie$/{skip=0} !skip{print}')
    if ! echo "$pubnet_block" | grep -qE 'soft_on_timeout:[[:space:]]*true' \
        && ! echo "$non_galexie_local_blocks" | grep -qE 'soft_on_timeout:[[:space:]]*true'; then
        tap_ok "workflow_pubnet_nongalexie_local_shards_have_no_soft_flag"
    else
        tap_not_ok "workflow_pubnet_nongalexie_local_shards_have_no_soft_flag" \
            "soft_on_timeout must be scoped to testnet + local/galexie only (found on pubnet or a non-galexie local shard)"
    fi
}

# ============================================================
# Test 27: test_sigterm_ignoring_probe_is_force_killed_and_soft_skipped
#
# Root-cause regression for #3272. PR #3273's own testnet shard (run
# 27298458353) HUNG: the per-probe `timeout 240` never fired because the
# wrapper invoked GNU `timeout` WITHOUT a `--kill-after`/`-k` grace. The
# upstream probe is `go run quickstart/tests/<probe>.go`, and `go run` does NOT
# forward SIGTERM to the test binary it execs. So at the deadline `timeout`
# sent SIGTERM, the probe ignored it, and `timeout` then blocked in wait()
# FOREVER — never force-killing, never returning, hanging the whole step until
# the job wall-clock cancelled it ~54 min later. The soft-skip never triggered
# because the wrapper never returned at all.
#
# The fix: `timeout -k "${KILL_GRACE}" "$TIMEOUT" ...` so a SIGTERM-ignoring
# probe is force-SIGKILLed after the grace and `timeout` is guaranteed to
# return promptly. Empirically (GNU coreutils 8.32) a `-k` force-kill in a
# pipeline (the wrapper runs `timeout ... | tee`) returns exit 137 (128+9,
# SIGKILL) — NOT 124 — because `timeout` runs the child in its own process
# group and reports the group SIGKILL as 128+9. (`--foreground` would make it
# report 124, but in a PIPELINE `--foreground` re-introduces the hang, so it is
# deliberately NOT used.) Therefore the soft-skip path must treat BOTH 124 (a
# clean SIGTERM timeout) AND 137 (a timeout that required the `-k` SIGKILL
# escalation) as the timeout disposition.
#
# This test wires a fake probe that IGNORES SIGTERM (trap '' TERM; sleep 60)
# through the wrapper with a short --timeout (1s), a short KILL_GRACE (1s), and
# --soft-on-timeout. It asserts:
#   (a) the wrapper TERMINATES quickly — well under 30s — proving `-k` force-
#       killed the probe (WITHOUT `-k`, on current pre-fix code, the wrapper
#       hangs ~the full sleep 60 and this assertion FAILS), and
#   (b) the timeout disposition is taken — under --soft-on-timeout the wrapper
#       exits 0 with the grep-able SOFT-SKIP marker.
# Without the `-k` fix AND the 137-as-timeout soft-skip extension, this test
# FAILS (hang and/or non-zero exit with no SOFT-SKIP marker).
# ============================================================
test_sigterm_ignoring_probe_is_force_killed_and_soft_skipped() {
    local diag_dir="$TMPDIR_BASE/diag-test27"
    mkdir -p "$diag_dir"

    # Fake probe that IGNORES SIGTERM and sleeps far longer than the timeout +
    # grace. Without a `-k` force-kill, `timeout` sends SIGTERM (ignored) and
    # blocks in wait() until this sleep finishes — i.e. the wrapper hangs.
    local probe="$TMPDIR_BASE/probe-test27.sh"
    cat > "$probe" <<'EOF'
#!/bin/bash
trap '' TERM
sleep 60
EOF
    chmod +x "$probe"

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test27.txt"
    local exit_code=0
    local start end elapsed
    start=$(date +%s)
    # Cap the whole wrapper invocation at 30s with an OUTER timeout so a
    # pre-fix HANG fails the assertion fast instead of stalling the harness for
    # the full sleep 60. The outer timeout also uses -k so it can itself bound
    # a wrapper that wedged. A correctly-fixed wrapper returns in ~2s, well
    # under this 30s cap.
    KILL_GRACE=1s timeout -k 5s 30 "$WRAPPER" \
        --soft-on-timeout \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 1 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || exit_code=$?
    end=$(date +%s)
    elapsed=$((end - start))

    # (a) The wrapper must have terminated quickly (the outer 30s timeout did
    # NOT have to fire). If the inner wrapper hung, the OUTER timeout kills it
    # at 30s and exit_code becomes 124/137 from the OUTER timeout — caught by
    # both the elapsed assertion and the absence of a SOFT-SKIP marker.
    if [[ $elapsed -lt 25 ]]; then
        tap_ok "sigterm_ignoring_probe_terminates_quickly_via_kill_after"
    else
        tap_not_ok "sigterm_ignoring_probe_terminates_quickly_via_kill_after" \
            "wrapper did not return promptly (elapsed=${elapsed}s) — -k force-kill missing, probe hung"
    fi

    # (b) The timeout disposition was taken: under --soft-on-timeout the wrapper
    # exits 0 with the SOFT-SKIP marker. (A bounded-but-still-red wrapper would
    # exit non-zero with no marker.)
    if [[ $exit_code -eq 0 ]] && grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "sigterm_ignoring_probe_timeout_is_soft_skipped"
    else
        tap_not_ok "sigterm_ignoring_probe_timeout_is_soft_skipped" \
            "exit=$exit_code, SOFT-SKIP marker present=$(grep -q 'SOFT-SKIP' "$wrapper_log" && echo yes || echo no)"
    fi
}

# ============================================================
# Test 28: test_testnet_hang_watchdog_emits_process_dump_before_step_kill
#
# DIAGNOSTIC instrumentation coverage for #3286. The testnet "Run probes
# through wrapper" step now (a) carries a tight step-level timeout-minutes (25)
# so a hang FAILS FAST instead of running to the 45-min job cancel, and (b)
# launches scripts/ci/quickstart-hang-watchdog.sh in the background BEFORE the
# probe loop. After WATCHDOG_DELAY (< the step-timeout bound) the watchdog must
# dump the process tree + open fds to the diagnostics dir, so the snapshot is
# on disk BEFORE the step kill and is swept into the uploaded artifact.
#
# This test runs the SAME script the workflow runs (no inline-vs-test drift):
# it simulates a hang by running a hung foreground "probe" under an OUTER
# timeout that fires AFTER the watchdog delay (the simulated step kill), with
# the watchdog backgrounded just as the step body does. It asserts the dump
# file exists, is non-empty, and contains a process-table signature — i.e. the
# diagnostic data is captured before the kill. The hung probe + watchdog are
# reaped via a UNIQUE sentinel marker (pkill -f "$marker") so nothing leaks
# across runs or wedges the harness EXIT trap (Critic A).
# ============================================================
test_testnet_hang_watchdog_emits_process_dump_before_step_kill() {
    local watchdog="$REPO_ROOT/scripts/ci/quickstart-hang-watchdog.sh"
    if [[ ! -f "$watchdog" ]]; then
        tap_not_ok "watchdog_script_exists" "scripts/ci/quickstart-hang-watchdog.sh not found"
        tap_not_ok "watchdog_dump_written_before_step_kill" "watchdog script missing"
        tap_not_ok "watchdog_dump_has_process_signature" "watchdog script missing"
        return
    fi
    tap_ok "watchdog_script_exists"

    local diag_dir="$TMPDIR_BASE/diag-test28"
    mkdir -p "$diag_dir"
    local out_dir="$diag_dir/testnet-hang-watchdog"

    # Unique sentinel so cleanup never kills unrelated processes on a shared
    # CI runner (Critic A). The hung "probe" carries it in its argv.
    local marker="qs-hang-wd-test28-$$-${RANDOM}"
    local hang_sentinel="$TMPDIR_BASE/$marker.hung"

    # The hung "probe" is a SCRIPT whose PATH carries the unique marker, so
    # `pkill -f "$marker"` matches exactly this process and nothing else.
    #
    # It used to be `exec sleep 30 "$marker"` — which does NOT hang: GNU sleep
    # rejects the non-numeric extra operand ("invalid time interval") and exits
    # ~immediately, so the simulated step shell was already dead when the
    # watchdog fired. That passed only because the watchdog was fire-and-forget.
    # The watchdog now polls step liveness (#3768) so it can never outlive its
    # step, which makes a genuinely-hung step mandatory for this scenario.
    local hung_probe="$TMPDIR_BASE/$marker.probe.sh"
    cat > "$hung_probe" <<'EOF'
#!/usr/bin/env bash
sleep 30
EOF
    chmod +x "$hung_probe"

    # Reap any leftovers from this test on function return, scoped to the
    # unique marker (so it never kills unrelated processes).
    # shellcheck disable=SC2064
    trap "pkill -f '$marker' 2>/dev/null || true" RETURN

    # Simulated step body: capture STEP_PID, launch the watchdog with a SHORT
    # WATCHDOG_DELAY, then run a hung foreground 'probe' under an OUTER timeout
    # that fires AFTER the watchdog has written its dump (the simulated 25-min
    # step kill). The foreground probe carries the unique marker in its argv.
    local step_body="$TMPDIR_BASE/stepbody-test28.sh"
    cat > "$step_body" <<EOF
#!/usr/bin/env bash
set -u
STEP_PID=\$\$
# Watchdog fires at ~1s — well before the outer 3s simulated step kill.
WATCHDOG_DELAY=1 bash "$watchdog" "$out_dir" "\$STEP_PID" &
WATCHDOG_PID=\$!
# Hung 'probe' that outlives the outer timeout; \`exec\` keeps STEP_PID alive as
# the hung process, so the watchdog's step-liveness poll still sees a live step.
touch "$hang_sentinel"
exec "$hung_probe"
EOF
    chmod +x "$step_body"

    # Outer timeout = the simulated step-timeout-minutes kill. It fires at 3s,
    # AFTER the 1s watchdog delay, so the dump must already be on disk.
    timeout -k 2s 3 bash "$step_body" >/dev/null 2>&1 || true

    # Give the backgrounded watchdog's file write a brief moment to settle.
    local waited=0
    while [[ ! -s "$out_dir/dump.txt" && $waited -lt 5 ]]; do
        sleep 1
        waited=$((waited + 1))
    done

    # (a) The dump file exists and is non-empty AFTER the simulated step kill.
    if [[ -s "$out_dir/dump.txt" ]]; then
        tap_ok "watchdog_dump_written_before_step_kill"
    else
        tap_not_ok "watchdog_dump_written_before_step_kill" \
            "expected non-empty $out_dir/dump.txt after the simulated step kill"
    fi

    # (b) The dump contains a process-table signature (the PID column header
    # from ps -ejH / ps faux), proving real process state was captured.
    if grep -qE '\bPID\b' "$out_dir/dump.txt" 2>/dev/null; then
        tap_ok "watchdog_dump_has_process_signature"
    else
        tap_not_ok "watchdog_dump_has_process_signature" \
            "dump did not contain a process-table (PID column) signature"
    fi

    # Reap the hung probe + watchdog explicitly via the unique marker (the
    # RETURN trap also covers this, belt-and-suspenders).
    pkill -f "$marker" 2>/dev/null || true
}

# ============================================================
# Test 29: test_testnet_shard_renders_step_timeout_25_others_360
#
# PRIMARY correct-by-construction guard for #3286 (does NOT depend on catching
# a flaky CI hang). The whole diagnostic chain hinges on the testnet
# core,horizon "Run probes through wrapper" step rendering a TIGHT step-level
# `timeout-minutes` of 25: a step-timeout marks the STEP failed (so the
# `if: failure()` "Upload diagnostics" step runs and the watchdog dump uploads),
# whereas the 45-min JOB wall-clock is a *cancel* that does NOT reliably run
# subsequent steps. A previous attempt could only be validated by a hung CI run.
#
# This test instead EVALUATES the per-shard `${{ matrix.step_timeout_minutes ||
# 360 }}` expression exactly as GitHub Actions does (via
# scripts/ci/render-quickstart-step-timeout.py, which applies GHA's `||` falsy
# semantics: an unset matrix key is null/falsy -> 360; the testnet entry's
# explicit 25 is truthy -> 25). It asserts:
#   (a) the testnet/core,horizon shard renders timeout-minutes == 25, AND
#   (b) every OTHER shard (local/pubnet) renders the generous 360 default,
#       so the tight bound is scoped to exactly the hanging shard and no other.
# If the key is ever moved to the wrong entry, dropped, or the expression form
# changes so it stops resolving to 25, this assertion turns red — no hung run
# required.
# ============================================================
test_testnet_shard_renders_step_timeout_25_others_360() {
    local renderer="$REPO_ROOT/scripts/ci/render-quickstart-step-timeout.py"
    if [[ ! -f "$WORKFLOW" || ! -f "$renderer" ]]; then
        tap_not_ok "render_script_exists" "workflow or renderer not found"
        tap_not_ok "testnet_core_horizon_shard_renders_step_timeout_25" "renderer missing"
        tap_not_ok "non_testnet_shards_render_generous_default_360" "renderer missing"
        tap_not_ok "exactly_one_shard_carries_tight_step_timeout" "renderer missing"
        return
    fi
    tap_ok "render_script_exists"

    # Render <network>|<enable>|<timeout-minutes> for every matrix shard.
    # `|| true` so a renderer error (exit non-zero) doesn't abort under `set -e`;
    # an empty/failed render is caught by the assertions below.
    local rendered
    rendered=$(python3 "$renderer" "$WORKFLOW" 2>"$TMPDIR_BASE/render-test29.err" || true)

    # (a) The testnet core,horizon shard renders exactly 25.
    local testnet_val
    testnet_val=$( (echo "$rendered" | grep -E '^testnet\|core,horizon\|' || true) \
        | head -1 | awk -F'|' '{print $3}')
    if [[ "$testnet_val" == "25" ]]; then
        tap_ok "testnet_core_horizon_shard_renders_step_timeout_25"
    else
        tap_not_ok "testnet_core_horizon_shard_renders_step_timeout_25" \
            "expected 25, got '$testnet_val' (renderer err: $(tr '\n' ' ' < "$TMPDIR_BASE/render-test29.err"))"
    fi

    # (a2) The local/galexie shard also renders exactly 25 (#3563 soft-degate:
    # it carries step_timeout_minutes: 25 to fast-fail into the soft-skip in
    # minutes, mirroring the testnet shard — see the galexie matrix comment).
    local galexie_val
    galexie_val=$( (echo "$rendered" | grep -E '^local\|galexie\|' || true) \
        | head -1 | awk -F'|' '{print $3}')
    if [[ "$galexie_val" == "25" ]]; then
        tap_ok "local_galexie_shard_renders_step_timeout_25"
    else
        tap_not_ok "local_galexie_shard_renders_step_timeout_25" \
            "expected 25, got '$galexie_val' (renderer err: $(tr '\n' ' ' < "$TMPDIR_BASE/render-test29.err"))"
    fi

    # (b) Every OTHER shard (not testnet, not local/galexie) renders the
    # generous default (360). A shard that silently picked up a tight bound (or
    # a default drift) turns this red. testnet/core,horizon (#3286) and
    # local/galexie (#3563) are the only two intentionally-tight shards.
    local other_bad
    other_bad=$( (echo "$rendered" | grep -vE '^testnet\||^local\|galexie\|' || true) \
        | awk -F'|' '$3 != "360" {print}' )
    if [[ -z "$other_bad" && -n "$rendered" ]]; then
        tap_ok "other_shards_render_generous_default_360"
    else
        tap_not_ok "other_shards_render_generous_default_360" \
            "non-tight shard(s) not at 360: ${other_bad:-<no shards rendered>}"
    fi

    # (c) Exactly two shards carry the tight (< 360) step timeout — testnet
    # (#3286 diagnostic bound) and local/galexie (#3563 soft-degate). The tight
    # bound must stay surgically scoped to these two, never broadcast.
    local tight_count
    tight_count=$( (echo "$rendered" || true) | awk -F'|' '$3 != "" && $3 + 0 < 360 {c++} END{print c+0}')
    if [[ "$tight_count" == "2" ]]; then
        tap_ok "exactly_two_shards_carry_tight_step_timeout"
    else
        tap_not_ok "exactly_two_shards_carry_tight_step_timeout" \
            "expected exactly 2 shards (testnet + local/galexie) with timeout-minutes < 360, got $tight_count"
    fi
}

# ============================================================
# Test 30: test_capture_diagnostics_docker_calls_are_time_bounded
#
# ROOT-CAUSE regression for #3286 (the actual fix, option A). The
# instrumentation in #3289 captured the process tree on a real hung run and
# pinned the mechanism: `capture_diagnostics()` runs INSIDE `run_probe()` —
# AFTER the probe's `exit_code` is captured but BEFORE `run_probe` returns. On
# an overloaded CI docker daemon a `docker ps -a` / `docker logs` /
# `docker inspect` can wedge in uninterruptible D-state. The pre-fix code
# guards those calls only with `|| true`, which does NOT bound a hang (a process
# stuck in a syscall never reaches the `|| true`). So `capture_diagnostics`
# never returns, `run_probe` never returns, and the 124/137 `--soft-on-timeout`
# soft-skip — which lives in `main`, AFTER `run_probe` — is never reached. The
# step then runs to its budget and the job is cancelled (the #3286 signature).
#
# The fix bounds each docker call with `timeout 30 docker ...` (keeping the
# `|| true` so the timeout's own 124 is swallowed and the probe's already-
# captured disposition stays byte-identical). With the docker calls bounded,
# `run_probe` returns promptly and the EXISTING, already-correct soft-skip
# fires — no soft-skip broadening is made (that would risk masking a genuine
# assertion failure; test_soft_on_timeout_still_fails_real_assertion stays red).
#
# This test puts a FAKE `docker` first on PATH whose `docker ps -a` does
# `sleep 999` (the D-state stand-in — `command -v docker` then finds the fake,
# so the docker block in capture_diagnostics runs). It runs the wrapper on the
# retryable testnet/core,horizon shard with --soft-on-timeout and an
# always-timing-out probe, INSIDE an outer watchdog (sized above the FIXED
# wrapper's bounded diagnostics cost: two retried attempts each cap one
# `timeout 30 docker ps -a` at ~30s, ~60s total). It asserts:
#   (a) the wrapper RETURNS within the watchdog (outer rc != 124 — NOT killed),
#       proving capture_diagnostics no longer wedges, AND
#   (b) the probe disposition is preserved: wrapper exit 0 with the grep-able
#       SOFT-SKIP marker for the timed-out testnet probe.
# FAILS on origin/main: the unbounded `docker ps -a` wedges on the fake
# `sleep 999`, run_probe never returns, the soft-skip is never reached, the
# outer watchdog fires (rc 124) -> (a) fails. PASSES after the fix: the bounded
# `timeout 30 docker ps -a` returns, run_probe returns, the soft-skip fires ->
# exit 0 + SOFT-SKIP within seconds. The fake `docker`'s `sleep 999` is reaped
# via a UNIQUE sentinel (pkill -f "$marker") in a RETURN trap so nothing leaks
# across runs or wedges the harness EXIT trap (Critic A / Test 28 pattern).
# ============================================================
test_capture_diagnostics_docker_calls_are_time_bounded() {
    local diag_dir="$TMPDIR_BASE/diag-test30"
    mkdir -p "$diag_dir"

    # Unique sentinel so cleanup only ever kills THIS test's fake-docker sleep,
    # never an unrelated process on a shared CI runner (Critic A).
    local marker="qs-fakedocker-test30-$$-${RANDOM}"

    # Reap the fake-docker `sleep 999` on function return, scoped to the marker.
    # shellcheck disable=SC2064
    trap "pkill -f '$marker' 2>/dev/null || true" RETURN

    # Fake `docker` whose `ps -a` wedges (sleep 999, tagged with the unique
    # marker in its argv). Other subcommands return immediately. Placed first on
    # PATH so capture_diagnostics' `command -v docker` resolves to THIS docker.
    local fakebin="$TMPDIR_BASE/fakebin-test30"
    mkdir -p "$fakebin"
    cat > "$fakebin/docker" <<EOF
#!/bin/bash
# Fake docker for #3286 regression. \`docker ps -a\` wedges like a D-state
# daemon on an overloaded runner. The wedge is a real \`sleep 999\` whose argv[0]
# is set to the unique \$marker (via \`exec -a\`) so the test can reap it precisely
# with \`pkill -f "\$marker"\` and never touch an unrelated process. (A bare
# \`sleep 999 "\$marker"\` would treat the marker as a second time arg and exit
# immediately — NOT a hang — so the marker must go in argv[0], not argv[1].)
if [[ "\$1" == "ps" && "\$2" == "-a" ]]; then
    exec -a "$marker" sleep 999
fi
# \`docker ps -aq --filter ...\` enumeration and any other subcommand: emit
# nothing (no containers) and return immediately.
exit 0
EOF
    chmod +x "$fakebin/docker"

    # Probe that always sleeps -> always times out (124/137) on both attempts,
    # so capture_diagnostics runs (it is only called on a non-zero exit).
    local probe
    probe=$(make_probe "test30" 0 999)

    local wrapper_log="$TMPDIR_BASE/wrapper-log-test30.txt"
    local outer_rc=0
    # OUTER watchdog. It must exceed the FIXED wrapper's bounded
    # diagnostics cost: the probe times out on attempt 1 AND on the retry (the
    # testnet/core,horizon shard is retryable), so capture_diagnostics runs
    # twice, and each fixed `timeout 30 docker ps -a` consumes up to its full 30s
    # against the fake `sleep 999` (~2 x 30s = ~60s). 90s leaves headroom so the
    # FIXED wrapper returns inside the watchdog. On UNFIXED main the unbounded
    # `docker ps -a` sleeps 999s (>> 90s) and never returns, so this outer
    # timeout fires at 90s with rc 124 — exactly the assertion-(a) failure that
    # proves the bug. (The bound is the plan's production `timeout 30`; a faster
    # test bound is not available without weakening the fix under test.)
    PATH="$fakebin:$PATH" timeout -k 5s 90 "$WRAPPER" \
        --soft-on-timeout \
        --network testnet --enable "core,horizon" --probe horizon-core-up \
        --timeout 2 --diagnostics-dir "$diag_dir" \
        -- "$probe" >/dev/null 2>"$wrapper_log" || outer_rc=$?

    # (a) The wrapper RETURNED on its own — the outer watchdog did NOT have to
    # fire (rc 124 would be the outer timeout killing a wedged wrapper).
    if [[ $outer_rc -ne 124 ]]; then
        tap_ok "capture_diagnostics_docker_calls_do_not_wedge_run_probe"
    else
        tap_not_ok "capture_diagnostics_docker_calls_do_not_wedge_run_probe" \
            "outer watchdog fired (rc 124) — capture_diagnostics wedged on the D-state docker"
    fi

    # (b) The probe disposition is preserved: under --soft-on-timeout a testnet
    # double-timeout is a neutral soft-skip (exit 0 + SOFT-SKIP marker). The fix
    # must NOT alter pass/fail/soft-skip logic — only stop the diagnostics wedge.
    if [[ $outer_rc -eq 0 ]] && grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "capture_diagnostics_bound_preserves_soft_skip_disposition"
    else
        tap_not_ok "capture_diagnostics_bound_preserves_soft_skip_disposition" \
            "rc=$outer_rc, SOFT-SKIP present=$(grep -q 'SOFT-SKIP' "$wrapper_log" && echo yes || echo no)"
    fi

    # Reap the fake-docker `sleep 999` explicitly via the unique marker (the
    # RETURN trap also covers this, belt-and-suspenders).
    pkill -f "$marker" 2>/dev/null || true
}

# ============================================================
# Test: regression for #3835 — extract_pubnet_matrix_block must not SIGPIPE
#
# The harness runs under `set -euo pipefail`. A `producer-streams-file |
# reader-exits-early` pipeline lets the reader tear the pipe down before the
# producer's write() lands; the producer takes SIGPIPE, pipefail surfaces exit
# 141, and set -e aborts the harness mid-plan (observed at test 88/101 in CI).
# This guard drives the helper against a pubnet block whose post-`steps:`
# remainder exceeds the 64 KB pipe buffer — the condition that makes the race
# fire every time — and asserts the extraction does NOT die with SIGPIPE.
# ============================================================
test_pubnet_block_extraction_no_sigpipe_on_oversized_workflow() {
    local big="$TMPDIR_BASE/oversized-pubnet-workflow.yml"
    local pad
    pad=$(printf 'x%.0s' $(seq 1 60))
    {
        echo "    - network: pubnet"
        echo "      probes: a"
        echo "    steps:"
        # >64 KB of matrix keys AFTER `steps:`, so the upstream producer keeps
        # streaming long after the reader hits its exit condition.
        local i
        for i in $(seq 1 3000); do
            printf '      key%d: value-%s\n' "$i" "$pad"
        done
    } > "$big"

    local exit_code=0
    ( set -euo pipefail; extract_pubnet_matrix_block "$big" >/dev/null ) || exit_code=$?

    if [[ $exit_code -ne 141 ]]; then
        tap_ok "test_pubnet_block_extraction_no_sigpipe_on_oversized_workflow"
    else
        tap_not_ok "test_pubnet_block_extraction_no_sigpipe_on_oversized_workflow" \
            "SIGPIPE (exit 141) — pipe writer killed by early-exit reader"
    fi
}

# ============================================================
# Test: #3835 — extract_pubnet_matrix_block output preservation
#
# The SIGPIPE fix must be byte-identical to the original inline two-awk shape
# on the committed workflow, so none of the other assertions that consume the
# pubnet block change meaning. Reference = the original shape computed inline
# (safe here because the real block is well under the 64 KB pipe buffer).
# ============================================================
test_pubnet_block_extraction_byte_identical_to_original_shape() {
    local reference helper_out
    # `set +o pipefail` inside the (subshell) command substitution: the REFERENCE
    # is the original two-awk shape, which is exactly the SIGPIPE-racy pipeline
    # #3835 fixed in the helper — the reference half was left racy. The reader
    # awk exits at `steps:` (very early in the pubnet block) and the producer awk
    # keeps writing the rest of the file; whether the producer's later write()
    # lands before the reader's fd closes is pure scheduling. When it loses, the
    # producer dies with SIGPIPE, pipefail surfaces 141 and `set -e` aborts the
    # whole harness mid-plan. Reproduced 1-in-8 locally after the #3768 workflow
    # additions grew the post-`steps:` remainder 7953 -> 9952 bytes (0-in-8 on
    # the smaller pre-#3768 workflow) — i.e. a latent flake this change would
    # otherwise have made routine.
    #
    # Turning pipefail off changes the STATUS only, never the OUTPUT: any bytes
    # the producer fails to write are bytes the reader had already stopped
    # consuming, so `reference` is byte-identical either way and the test keeps
    # comparing against the original shape.
    reference=$(set +o pipefail; awk '/network: pubnet/{f=1} f{print}' "$WORKFLOW" \
        | awk '/^    steps:/{exit} {print}' | grep -vE '^[[:space:]]*#')
    helper_out=$(extract_pubnet_matrix_block "$WORKFLOW")
    if [[ "$helper_out" == "$reference" ]]; then
        tap_ok "test_pubnet_block_extraction_byte_identical_to_original_shape"
    else
        tap_not_ok "test_pubnet_block_extraction_byte_identical_to_original_shape" \
            "helper output diverged from the original two-awk shape"
    fi
}

# ============================================================
# Test 32: test_watchdog_samples_periodically_and_respects_cap
#
# Regression for #3768 (defect 3). The #3286 watchdog armed a SINGLE dump at a
# fixed T+1200s offset. On run 31745517379 that one sample landed at 23:11:08 —
# one second AFTER the 813-second silent window closed — so it captured a
# freshly-spawned HEALTHY probe (every PID `START 23:11`, `TIME 00:00:00`) and
# proved nothing about a survivor. A one-shot wall-clock sample cannot
# characterise an INTERVAL hang.
#
# The watchdog must now sample PERIODICALLY (a time series of distinct files, so
# several samples land inside a multi-minute wedge) and BOUNDEDLY (a hard cap so
# a pathological run cannot fill the runner disk or the uploaded artifact), and
# must SAY SO when the cap is hit rather than going silently quiet.
#
# FAILS on origin/main: the script takes exactly one dump and exits, so only
# `dump.txt` ever exists and there is no `dump-NN-*.txt` series at all.
# ============================================================
test_watchdog_samples_periodically_and_respects_cap() {
    local watchdog="$REPO_ROOT/scripts/ci/quickstart-hang-watchdog.sh"
    if [[ ! -f "$watchdog" ]]; then
        tap_not_ok "watchdog_takes_multiple_samples" "watchdog script missing"
        tap_not_ok "watchdog_respects_sample_cap" "watchdog script missing"
        tap_not_ok "watchdog_logs_when_cap_reached" "watchdog script missing"
        tap_not_ok "watchdog_keeps_dump_txt_for_back_compat" "watchdog script missing"
        return
    fi

    local out_dir="$TMPDIR_BASE/diag-test32/testnet-hang-watchdog"
    # Unique sentinel so cleanup never kills unrelated processes on a shared
    # CI runner. The fake "step" carries it in its argv (via its script path).
    local marker="qs-wd-periodic-$$-${RANDOM}"
    local fake_step="$TMPDIR_BASE/$marker.sh"
    cat > "$fake_step" <<'EOF'
#!/usr/bin/env bash
sleep 120
EOF
    chmod +x "$fake_step"
    # shellcheck disable=SC2064
    trap "pkill -f '$marker' 2>/dev/null || true" RETURN

    # Long-lived fake step shell: the watchdog polls it with `kill -0` and keeps
    # sampling while it lives, so the run terminates on the CAP, not on the step
    # exiting — which is exactly what this test is measuring.
    "$fake_step" &
    local step_pid=$!

    # Foreground run with a tight cap so the assertion is deterministic:
    # first sample at 1s, then every 1s, cap 3. The outer `timeout 30` is a
    # safety net only — if it ever fires (rc 124) the cap did not bound the loop.
    local log="$TMPDIR_BASE/watchdog-test32.log"
    local rc=0
    WATCHDOG_DELAY=1 WATCHDOG_INTERVAL=1 WATCHDOG_MAX_SAMPLES=3 WATCHDOG_POLL=1 \
        timeout 30 bash "$watchdog" "$out_dir" "$step_pid" > "$log" 2>&1 || rc=$?

    kill "$step_pid" 2>/dev/null || true

    # Count the per-sample dump files (the time series). `dump.txt` is the
    # back-compat alias and is deliberately excluded from this count.
    local samples
    samples=$(find "$out_dir" -maxdepth 1 -name 'dump-*.txt' 2>/dev/null | wc -l | tr -d ' ')

    # (a) MORE THAN ONE sample — the whole point: a periodic time series, not a
    #     single fixed-delay snapshot that can miss the hang interval entirely.
    if [[ "$samples" -gt 1 ]]; then
        tap_ok "watchdog_takes_multiple_samples"
    else
        tap_not_ok "watchdog_takes_multiple_samples" \
            "found $samples dump-*.txt file(s) (rc=$rc) — expected a periodic series (>1)"
    fi

    # (b) BOUNDED: exactly the cap, never more, and the loop returned on its own
    #     (rc 0, not the outer safety-net 124).
    if [[ "$samples" -eq 3 && "$rc" -eq 0 ]]; then
        tap_ok "watchdog_respects_sample_cap"
    else
        tap_not_ok "watchdog_respects_sample_cap" \
            "samples=$samples rc=$rc (expected exactly 3 samples and rc 0 — cap must bound the loop)"
    fi

    # (c) The cap must be ANNOUNCED, not silent, so a reader of the artifact can
    #     tell a truncated series from a hang that ended.
    if grep -qi 'cap reached' "$log" && grep -qi 'cap reached' "$out_dir/index.txt" 2>/dev/null; then
        tap_ok "watchdog_logs_when_cap_reached"
    else
        tap_not_ok "watchdog_logs_when_cap_reached" \
            "no cap-reached announcement on stderr and in index.txt"
    fi

    # (d) Back-compat: the original single-dump filename still exists (and holds
    #     a real process table), so any existing consumer keeps working.
    if [[ -s "$out_dir/dump.txt" ]] && grep -qE '\bPID\b' "$out_dir/dump.txt" 2>/dev/null; then
        tap_ok "watchdog_keeps_dump_txt_for_back_compat"
    else
        tap_not_ok "watchdog_keeps_dump_txt_for_back_compat" \
            "dump.txt missing/empty or lacks a process-table signature"
    fi

    pkill -f "$marker" 2>/dev/null || true
}

# ============================================================
# Test 33: test_watchdog_stops_when_step_pid_exits
#
# Scope guard for #3768. Making the watchdog periodic must NOT create a process
# that outlives the step — a backgrounded process holding the step's stdout is
# the exact wedge the watchdog exists to hunt, so it must never become one. The
# loop polls the step PID with `kill -0` and self-reaps within ~WATCHDOG_POLL
# seconds of the step exiting, independently of any signal reaching it.
# ============================================================
test_watchdog_stops_when_step_pid_exits() {
    local watchdog="$REPO_ROOT/scripts/ci/quickstart-hang-watchdog.sh"
    if [[ ! -f "$watchdog" ]]; then
        tap_not_ok "watchdog_self_reaps_when_step_exits" "watchdog script missing"
        tap_not_ok "watchdog_records_stop_reason" "watchdog script missing"
        return
    fi

    local out_dir="$TMPDIR_BASE/diag-test33/testnet-hang-watchdog"
    local marker="qs-wd-reap-$$-${RANDOM}"
    local fake_step="$TMPDIR_BASE/$marker.sh"
    cat > "$fake_step" <<'EOF'
#!/usr/bin/env bash
sleep 3
EOF
    chmod +x "$fake_step"
    # shellcheck disable=SC2064
    trap "pkill -f '$marker' 2>/dev/null || true" RETURN

    "$fake_step" &
    local step_pid=$!

    # Cap set high (30) so the ONLY way this returns quickly is the step-liveness
    # poll. The outer `timeout 25` is a safety net: rc 124 would mean the
    # watchdog kept sampling after its step died (i.e. it leaked).
    local log="$TMPDIR_BASE/watchdog-test33.log"
    local rc=0
    WATCHDOG_DELAY=1 WATCHDOG_INTERVAL=1 WATCHDOG_MAX_SAMPLES=30 WATCHDOG_POLL=1 \
        timeout 25 bash "$watchdog" "$out_dir" "$step_pid" > "$log" 2>&1 || rc=$?

    local samples
    samples=$(find "$out_dir" -maxdepth 1 -name 'dump-*.txt' 2>/dev/null | wc -l | tr -d ' ')

    # Returned on its own (rc 0, not the 124 safety net) and well short of the cap.
    if [[ "$rc" -eq 0 && "$samples" -ge 1 && "$samples" -lt 30 ]]; then
        tap_ok "watchdog_self_reaps_when_step_exits"
    else
        tap_not_ok "watchdog_self_reaps_when_step_exits" \
            "rc=$rc samples=$samples (expected rc 0 and 1..29 samples — it must stop when the step dies)"
    fi

    if grep -q 'stopped after' "$out_dir/index.txt" 2>/dev/null; then
        tap_ok "watchdog_records_stop_reason"
    else
        tap_not_ok "watchdog_records_stop_reason" "index.txt has no stop-reason line"
    fi

    pkill -f "$marker" 2>/dev/null || true
}

# ============================================================
# Test 34: test_budget_exhausted_soft_skips_before_starting_a_probe
#
# PRIMARY regression for #3768 (defect 2 — the actual red). The wrapper can only
# soft-skip an outcome it OBSERVES; a GitHub step-level `timeout-minutes` kill is
# invisible to it. On run 31745517379 every probe disposition was `exit 124` on a
# `--soft-on-timeout` shard (the exact environmental class #3272 de-gates), but
# `horizon-core-up` burned 19m33s of the 25-minute step budget, so the step cap
# fired during `horizon-ingesting` attempt 2 and the run went hard RED.
#
# With the step budget threaded in, a probe attempt that provably cannot finish
# must not be started at all: emit the grep-able SOFT-SKIP marker (with the
# distinct BUDGET-EXHAUSTED token so triage cannot confuse it with a genuine
# 124) and exit neutral 0.
#
# FAILS on origin/main: the budget env vars are ignored, so the wrapper starts a
# 240s attempt with no budget left and the step cap converts it to RED.
# ============================================================
test_budget_exhausted_soft_skips_before_starting_a_probe() {
    local diag_dir="$TMPDIR_BASE/diag-test34"
    mkdir -p "$diag_dir"

    # A probe that would hang forever if it were ever started.
    local probe
    probe=$(make_probe "test34" 0 999)

    # Step budget 60s, but the step started 600s ago → the budget is long gone.
    # Margin/floor left at their DEFAULTS so this also exercises the defaults.
    local now
    now=$(date -u +%s)
    local wrapper_log="$TMPDIR_BASE/wrapper-log-test34.txt"
    local started ended elapsed rc=0
    started=$(date -u +%s)
    STEP_BUDGET_SECONDS=60 STEP_START_EPOCH=$((now - 600)) \
        "$WRAPPER" \
            --soft-on-timeout \
            --network testnet --enable "core,horizon" --probe horizon-ingesting \
            --timeout 240 --diagnostics-dir "$diag_dir" \
            -- "$probe" >/dev/null 2>"$wrapper_log" || rc=$?
    ended=$(date -u +%s)
    elapsed=$((ended - started))

    # (a) Neutral exit, not RED.
    if [[ "$rc" -eq 0 ]]; then
        tap_ok "test_budget_exhausted_soft_skips_before_starting_a_probe"
    else
        tap_not_ok "test_budget_exhausted_soft_skips_before_starting_a_probe" \
            "rc=$rc (expected 0 — an exhausted step budget must be a neutral skip, not RED)"
    fi

    # (b) It returned IMMEDIATELY — proving no attempt was started (an attempt
    #     would have burned the 240s probe timeout).
    if [[ "$elapsed" -lt 10 ]]; then
        tap_ok "budget_skip_starts_no_probe_attempt"
    else
        tap_not_ok "budget_skip_starts_no_probe_attempt" \
            "wrapper took ${elapsed}s (expected <10s — it must not start an unfinishable attempt)"
    fi

    # (c) No attempt output at all.
    if [[ ! -e "$diag_dir/attempt-1-output.log" ]]; then
        tap_ok "budget_skip_leaves_no_attempt_output"
    else
        tap_not_ok "budget_skip_leaves_no_attempt_output" "attempt-1-output.log exists — an attempt was started"
    fi

    # (d) The stable, grep-able SOFT-SKIP marker is preserved AND carries the
    #     distinct BUDGET-EXHAUSTED token, so triage can never mistake a budget
    #     skip for a genuine probe timeout (which logs `exit 124`/`exit 137`).
    if grep -q 'SOFT-SKIP' "$wrapper_log" && grep -q 'BUDGET-EXHAUSTED' "$wrapper_log"; then
        tap_ok "budget_skip_emits_distinct_soft_skip_marker"
    else
        tap_not_ok "budget_skip_emits_distinct_soft_skip_marker" \
            "missing SOFT-SKIP and/or BUDGET-EXHAUSTED marker in wrapper output"
    fi

    # (e) A breadcrumb lands in the diagnostics dir so the skip is observable in
    #     the uploaded artifact, not only in the job log.
    if [[ -s "$diag_dir/budget-skip.txt" ]]; then
        tap_ok "budget_skip_writes_diagnostics_breadcrumb"
    else
        tap_not_ok "budget_skip_writes_diagnostics_breadcrumb" "no budget-skip.txt breadcrumb written"
    fi
}

# ============================================================
# Test 35: test_ample_budget_runs_probe_at_full_timeout
#
# Scope guard for #3768: with plenty of step budget left, budget-awareness must
# be a NO-OP — the probe runs, at the FULL per-probe --timeout, and no
# BUDGET-EXHAUSTED marker is emitted.
# ============================================================
test_ample_budget_runs_probe_at_full_timeout() {
    local diag_dir="$TMPDIR_BASE/diag-test35"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test35" 0 0)

    local now
    now=$(date -u +%s)
    local wrapper_log="$TMPDIR_BASE/wrapper-log-test35.txt"
    local rc=0
    STEP_BUDGET_SECONDS=3600 STEP_START_EPOCH="$now" \
        "$WRAPPER" \
            --soft-on-timeout \
            --network testnet --enable "core,horizon" --probe horizon-core-up \
            --timeout 10 --diagnostics-dir "$diag_dir" \
            -- "$probe" >/dev/null 2>"$wrapper_log" || rc=$?

    # (a) The probe actually RAN and passed — no skip.
    if [[ "$rc" -eq 0 && -e "$diag_dir/attempt-1-output.log" ]] && ! grep -q 'BUDGET-EXHAUSTED' "$wrapper_log"; then
        tap_ok "test_ample_budget_runs_probe_at_full_timeout"
    else
        tap_not_ok "test_ample_budget_runs_probe_at_full_timeout" \
            "rc=$rc (expected the probe to run with no budget skip when budget is ample)"
    fi

    # (b) It ran at the FULL per-probe budget — no cap applied.
    if grep -q 'timeout -k [0-9]*s 10s' "$wrapper_log" && ! grep -q 'Step budget short' "$wrapper_log"; then
        tap_ok "ample_budget_does_not_cap_attempt_timeout"
    else
        tap_not_ok "ample_budget_does_not_cap_attempt_timeout" \
            "attempt did not run at the full 10s per-probe budget"
    fi
}

# ============================================================
# Test 36: test_short_budget_still_reds_a_genuine_assertion_failure
#
# LOAD-BEARING safety contract for #3768. Budget-awareness must NEVER widen the
# soft-skip to non-timeout exits. When the step budget is short but not gone, the
# wrapper caps the attempt's `timeout` instead of skipping it, so a genuine probe
# assertion failure (`go` exits 1 — NOT 124/137/143) is still OBSERVED and still
# fails RED, with no SOFT-SKIP marker. A skip-everything-when-short design would
# silently mask a real henyey-on-testnet break; this asserts we do not.
# ============================================================
test_short_budget_still_reds_a_genuine_assertion_failure() {
    local diag_dir="$TMPDIR_BASE/diag-test36"
    mkdir -p "$diag_dir"

    # Genuine assertion failure: exits 1 immediately (well under any cap).
    local probe
    probe=$(make_probe "test36" 1 0)

    # usable = 100 - ~0 - 60 (default margin) ≈ 40s: above the 15s floor, but far
    # below the 240s per-probe timeout → the attempt is CAPPED, not skipped.
    local now
    now=$(date -u +%s)
    local wrapper_log="$TMPDIR_BASE/wrapper-log-test36.txt"
    local rc=0
    STEP_BUDGET_SECONDS=100 STEP_START_EPOCH="$now" \
        "$WRAPPER" \
            --soft-on-timeout \
            --network testnet --enable "core,horizon" --probe horizon-core-up \
            --timeout 240 --diagnostics-dir "$diag_dir" \
            -- "$probe" >/dev/null 2>"$wrapper_log" || rc=$?

    # (a) RED, with the child's exit code propagated unchanged.
    if [[ "$rc" -eq 1 ]]; then
        tap_ok "test_short_budget_still_reds_a_genuine_assertion_failure"
    else
        tap_not_ok "test_short_budget_still_reds_a_genuine_assertion_failure" \
            "rc=$rc (expected 1 — a real assertion failure must stay RED regardless of budget)"
    fi

    # (b) No soft-skip of ANY kind for a non-timeout exit.
    if ! grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "short_budget_does_not_soft_skip_assertion_failure"
    else
        tap_not_ok "short_budget_does_not_soft_skip_assertion_failure" \
            "SOFT-SKIP emitted for a non-timeout exit — this masks a real break"
    fi

    # (c) The attempt really did run under a CAPPED timeout (the branch that
    #     keeps the failure observable rather than skipping it).
    if grep -q 'Step budget short' "$wrapper_log" && [[ -e "$diag_dir/attempt-1-output.log" ]]; then
        tap_ok "short_budget_caps_attempt_instead_of_skipping"
    else
        tap_not_ok "short_budget_caps_attempt_instead_of_skipping" \
            "expected a capped attempt to have been run, not a skip"
    fi
}

# ============================================================
# Test 37: test_budget_awareness_is_inert_without_soft_on_timeout
#
# Scope guard for #3768: budget-awareness is gated on --soft-on-timeout, so it
# can only ever apply to the shards that already declared their TIMEOUTS
# non-gating (testnet/core,horizon and local/galexie). On every other shard the
# budget env vars must be completely inert — an exhausted budget must NOT create
# a neutral exit, and a failure must stay RED.
# ============================================================
test_budget_awareness_is_inert_without_soft_on_timeout() {
    local diag_dir="$TMPDIR_BASE/diag-test37"
    mkdir -p "$diag_dir"

    local probe
    probe=$(make_probe "test37" 1 0)

    local now
    now=$(date -u +%s)
    local wrapper_log="$TMPDIR_BASE/wrapper-log-test37.txt"
    local rc=0
    # Budget long gone, but NO --soft-on-timeout (a non-de-gated shard).
    STEP_BUDGET_SECONDS=60 STEP_START_EPOCH=$((now - 600)) \
        "$WRAPPER" \
            --network local --enable "core" --probe core \
            --timeout 240 --diagnostics-dir "$diag_dir" \
            -- "$probe" >/dev/null 2>"$wrapper_log" || rc=$?

    if [[ "$rc" -eq 1 ]] && ! grep -q 'SOFT-SKIP' "$wrapper_log"; then
        tap_ok "test_budget_awareness_is_inert_without_soft_on_timeout"
    else
        tap_not_ok "test_budget_awareness_is_inert_without_soft_on_timeout" \
            "rc=$rc (expected 1 with no SOFT-SKIP — budget skip must not leak to non-soft shards)"
    fi

    # The probe must actually have been run on a non-soft shard.
    if [[ -e "$diag_dir/attempt-1-output.log" ]]; then
        tap_ok "non_soft_shard_still_runs_the_probe_when_budget_is_gone"
    else
        tap_not_ok "non_soft_shard_still_runs_the_probe_when_budget_is_gone" \
            "no attempt output — the budget gate leaked to a non-soft shard"
    fi
}

# ============================================================
# Test 38: test_budget_skip_before_retry_reproduces_failing_run_shape
#
# Reproduces the exact shape of run 31745517379's fatal step: attempt 1 consumes
# its budget and times out, and there is no longer enough STEP budget for
# attempt 2. On origin/main the wrapper starts attempt 2 anyway and the step cap
# kills it mid-flight (hard RED, no soft-skip possible). With budget awareness,
# attempt 2 is never started and the shard ends neutral-green.
# ============================================================
test_budget_skip_before_retry_reproduces_failing_run_shape() {
    local diag_dir="$TMPDIR_BASE/diag-test38"
    mkdir -p "$diag_dir"

    # Always-timing-out probe (the stuck-sync signature).
    local probe
    probe=$(make_probe "test38" 0 999)

    # Budget arithmetic (margin 0, floor 3, budget 6, per-probe timeout 4):
    #   pre-attempt-1: usable = 6 - ~0 - 0 = ~6  >= floor 3 and >= timeout 4
    #                  → attempt 1 runs at the full 4s and times out (124).
    #   pre-retry:     usable = 6 - (>=4) - 0 <= 2 < floor 3
    #                  → attempt 2 is NEVER started; neutral BUDGET-EXHAUSTED skip.
    # Deterministic: attempt 1 always consumes >= 4s of the 6s budget.
    local now
    now=$(date -u +%s)
    local wrapper_log="$TMPDIR_BASE/wrapper-log-test38.txt"
    local rc=0
    STEP_BUDGET_SECONDS=6 STEP_START_EPOCH="$now" \
    BUDGET_MARGIN_SECONDS=0 BUDGET_MIN_ATTEMPT_SECONDS=3 KILL_GRACE=1s \
        "$WRAPPER" \
            --soft-on-timeout \
            --network testnet --enable "core,horizon" --probe horizon-ingesting \
            --timeout 4 --diagnostics-dir "$diag_dir" \
            -- "$probe" >/dev/null 2>"$wrapper_log" || rc=$?

    # (a) Neutral, not RED.
    if [[ "$rc" -eq 0 ]]; then
        tap_ok "test_budget_skip_before_retry_reproduces_failing_run_shape"
    else
        tap_not_ok "test_budget_skip_before_retry_reproduces_failing_run_shape" \
            "rc=$rc (expected 0 — budget-exhausted retry must be neutral, not RED)"
    fi

    # (b) Attempt 1 ran; attempt 2 was never started.
    if [[ -e "$diag_dir/attempt-1-output.log" && ! -e "$diag_dir/attempt-2-output.log" ]]; then
        tap_ok "budget_skip_suppresses_only_the_unfinishable_retry"
    else
        tap_not_ok "budget_skip_suppresses_only_the_unfinishable_retry" \
            "expected attempt-1 to have run and attempt-2 to have been skipped"
    fi

    # (c) It is reported as a BUDGET exhaustion, not as a probe timeout.
    if grep -q 'BUDGET-EXHAUSTED' "$wrapper_log"; then
        tap_ok "budget_skip_before_retry_is_labelled_budget_exhausted"
    else
        tap_not_ok "budget_skip_before_retry_is_labelled_budget_exhausted" \
            "no BUDGET-EXHAUSTED marker — a budget skip must be distinguishable from a genuine 124"
    fi
}

# ============================================================
# Test 39: test_workflow_wires_step_budget_and_periodic_watchdog
#
# Correct-by-construction workflow contract for #3768 (no CI hang required).
#   (a) The step mirrors its OWN `timeout-minutes` expression into
#       STEP_TIMEOUT_MINUTES — asserted by comparing the two parsed YAML values,
#       so the budget the wrapper sees can never drift from the cap that kills it.
#   (b) The step body exports STEP_START_EPOCH and STEP_BUDGET_SECONDS.
#   (c) The watchdog is armed PERIODICALLY (interval + cap knobs present) and the
#       one-shot `sleep "${WATCHDOG_DELAY:-1200}"` arming is gone.
#   (d) The watchdog stays testnet-only and is still reaped via the EXIT trap.
# ============================================================
test_workflow_wires_step_budget_and_periodic_watchdog() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "workflow_step_budget_matches_step_timeout_expression" "workflow file not found"
        tap_not_ok "workflow_exports_step_budget_to_wrapper" "workflow file not found"
        tap_not_ok "workflow_arms_watchdog_periodically" "workflow file not found"
        tap_not_ok "workflow_watchdog_stays_testnet_only_and_reaped" "workflow file not found"
        return
    fi

    # Assert on the EXECUTABLE content only: comment lines are stripped so a
    # comment that quotes the old one-shot arming (for context) cannot satisfy
    # — or falsify — any assertion below.
    local wf_code
    wf_code=$(grep -vE '^[[:space:]]*#' "$WORKFLOW")

    # (a) Parse the YAML and compare the two expressions exactly.
    local budget_match
    budget_match=$(python3 - "$WORKFLOW" <<'PY' 2>/dev/null || true
import sys, yaml
doc = yaml.safe_load(open(sys.argv[1]))
steps = doc["jobs"]["test"]["steps"]
step = next(s for s in steps if s.get("name") == "Run probes through wrapper")
env = step.get("env") or {}
print("MATCH" if str(step.get("timeout-minutes")) == str(env.get("STEP_TIMEOUT_MINUTES")) else "MISMATCH")
PY
)
    if [[ "$budget_match" == "MATCH" ]]; then
        tap_ok "workflow_step_budget_matches_step_timeout_expression"
    else
        tap_not_ok "workflow_step_budget_matches_step_timeout_expression" \
            "STEP_TIMEOUT_MINUTES != timeout-minutes expression ($budget_match) — the budget would drift from the cap"
    fi

    # (b) The step body must compute and EXPORT the budget so the wrapper sees it.
    if grep -q 'STEP_START_EPOCH="\$(date -u +%s)"' <<<"$wf_code" \
        && grep -q 'STEP_BUDGET_SECONDS=\$(( STEP_TIMEOUT_MINUTES \* 60 ))' <<<"$wf_code" \
        && grep -q 'export STEP_START_EPOCH STEP_BUDGET_SECONDS' <<<"$wf_code"; then
        tap_ok "workflow_exports_step_budget_to_wrapper"
    else
        tap_not_ok "workflow_exports_step_budget_to_wrapper" \
            "step body does not compute/export STEP_START_EPOCH + STEP_BUDGET_SECONDS"
    fi

    # (c) Periodic arming: interval + cap knobs present, one-shot arming gone.
    if grep -q 'WATCHDOG_INTERVAL' <<<"$wf_code" \
        && grep -q 'WATCHDOG_MAX_SAMPLES' <<<"$wf_code" \
        && grep -q 'WATCHDOG_DELAY' <<<"$wf_code" \
        && ! grep -qE '\([[:space:]]*sleep "\$\{WATCHDOG_DELAY' <<<"$wf_code"; then
        tap_ok "workflow_arms_watchdog_periodically"
    else
        tap_not_ok "workflow_arms_watchdog_periodically" \
            "watchdog still armed one-shot, or the interval/cap knobs are missing"
    fi

    # (d) Scope + hygiene guards, unchanged from #3286.
    if grep -q 'if \[\[ "\$NETWORK" == "testnet" \]\]; then' <<<"$wf_code" \
        && grep -q "kill \"\$WATCHDOG_PID\"" <<<"$wf_code"; then
        tap_ok "workflow_watchdog_stays_testnet_only_and_reaped"
    else
        tap_not_ok "workflow_watchdog_stays_testnet_only_and_reaped" \
            "watchdog is no longer testnet-only, or the reaping kill was dropped"
    fi
}

# --- Run all tests ---
# Set EMPIRICALLY: run the harness, read the `# PLAN MISMATCH: planned N,
# emitted M` line at the end, set this to M. Do not compute it by hand.
tap_plan 128

test_timeout_retry_on_targeted_shard
test_non_timeout_failure_no_retry
test_workflow_shard_probe_contract
test_double_timeout_fails
test_non_targeted_timeout_no_retry
test_local_galexie_soft_skip_timeout_only
test_success_no_retry_artifacts
test_upstream_contract_validation
test_workflow_uses_upstream_run_attempt_timeout_budget
test_timeout_budget_matches_upstream_formula
test_contract_pins_timeout_multiplier
test_retry_is_layered_on_top_of_budget
test_runner_shutdown_exit143_retries_on_targeted_shard
test_runner_shutdown_exit143_no_retry_on_non_targeted_shard
test_runner_shutdown_exit143_double_failure_fails
test_transient_retry_covers_whole_testnet_shard
test_exit143_retries_on_non_horizon_core_up_testnet_probe
test_exit137_retries_on_targeted_shard
test_exit137_no_retry_on_non_targeted_shard
test_exit137_double_failure_fails
test_in_run_rerun_job_removed
test_separate_workflow_run_retry_workflow
test_soft_on_timeout_testnet_timeout_is_neutral_skip
test_soft_on_timeout_still_fails_real_assertion
test_soft_on_timeout_preserves_diagnostics
test_soft_on_timeout_off_by_default_preserves_red
test_workflow_testnet_shard_uses_soft_timeout_and_tight_budget
test_sigterm_ignoring_probe_is_force_killed_and_soft_skipped
test_testnet_hang_watchdog_emits_process_dump_before_step_kill
test_testnet_shard_renders_step_timeout_25_others_360
test_capture_diagnostics_docker_calls_are_time_bounded
test_pubnet_block_extraction_no_sigpipe_on_oversized_workflow
test_pubnet_block_extraction_byte_identical_to_original_shape
test_watchdog_samples_periodically_and_respects_cap
test_watchdog_stops_when_step_pid_exits
test_budget_exhausted_soft_skips_before_starting_a_probe
test_ample_budget_runs_probe_at_full_timeout
test_short_budget_still_reds_a_genuine_assertion_failure
test_budget_awareness_is_inert_without_soft_on_timeout
test_budget_skip_before_retry_reproduces_failing_run_shape
test_workflow_wires_step_budget_and_periodic_watchdog

echo ""
echo "# Results: $PASS_COUNT/$TEST_COUNT passed, $FAIL_COUNT failed"

# TAP plan self-check. The `1..N` plan above is set EMPIRICALLY (run the harness,
# read the line below, set the plan to the emitted count, re-run). Emitting the
# mismatch explicitly — and failing on it — means a newly added assertion can
# never silently drift the plan out of sync with what is actually run.
if [[ $TEST_COUNT -ne $TAP_PLANNED ]]; then
    echo "# PLAN MISMATCH: planned $TAP_PLANNED, emitted $TEST_COUNT"
    echo "# Set the tap_plan call to $TEST_COUNT and re-run."
    exit 1
fi

if [[ $FAIL_COUNT -gt 0 ]]; then
    exit 1
fi
exit 0
