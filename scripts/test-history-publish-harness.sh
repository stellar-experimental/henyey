#!/usr/bin/env bash
# scripts/test-history-publish-harness.sh — TAP regression/coverage tests for the
# History Publish (Testnet) sync-vs-publish disposition classifier introduced in
# #3280.
#
# Background (#3280): the "History Publish (Testnet)" daily workflow runs
# scripts/test-history-publish.sh, which (phase 1) waits up to --timeout seconds
# for the node to sync testnet and publish its first checkpoint, then (phase 2)
# byte-compares that checkpoint against SDF's live testnet archive. The phase-1
# deadline used to be a single blanket `exit 1`, which conflated TWO very
# different outcomes:
#   * the node SYNCED but never published a checkpoint — a real publish
#     regression that must stay a hard red, AND
#   * the node NEVER reached sync in time — an environmental testnet-liveness
#     timeout (the chronic flake the issue tracks; same SHA passed/failed on
#     different days), which should be soft-skippable.
#
# The classifier (test-history-publish.sh --classify-only <data-dir>) decides the
# phase-1 deadline disposition from concrete signals already present on disk:
#   * a published HAS (history/.well-known/stellar-history.json, currentLedger>0)
#   * the node's sync marker in validator.log — the EXACT string the run loop
#     logs once it reaches Synced/Validating: "Node is synced"
#     (crates/app/src/run_cmd.rs::wait_for_sync). Pinned by a harness assertion
#     below so a log-wording drift breaks this harness loudly instead of silently
#     inverting the synced-vs-never-synced dispositions.
#
# Dispositions and exit codes (classify-only):
#   PUBLISHED         exit 0  — HAS present (caller proceeds to phase-2 compare)
#   PUBLISH-REGRESSION exit 1 — synced but no HAS by deadline (real regression)
#   SYNC-TIMEOUT      exit 1  — never synced, flag OFF (byte-identical default)
#   SYNC-TIMEOUT      exit 0  — never synced, --soft-on-sync-timeout (SOFT-SKIP)
#
# Run: bash scripts/test-history-publish-harness.sh
# Requires: bash 4+, jq

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/test-history-publish.sh"
WORKFLOW="$REPO_ROOT/.github/workflows/history-publish.yml"
RUN_CMD="$REPO_ROOT/crates/app/src/run_cmd.rs"

# The exact sync marker the run loop emits when the node reaches sync. This is
# the single source of truth shared between the classifier and the assertion
# pinning it (test_sync_marker_matches_run_loop_log below).
SYNC_MARKER="Node is synced"

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

# Build a synthetic data-dir fixture mirroring what test-history-publish.sh
# creates at runtime: a validator.log (optionally containing the sync marker)
# and an optional published HAS file.
#
# Args: <name> <synced: yes|no> <has_ledger: int|none>
make_fixture() {
    local name="$1"
    local synced="$2"
    local has_ledger="$3"
    local data_dir="$TMPDIR_BASE/fixture-$name"
    local log="$data_dir/validator.log"
    local has_dir="$data_dir/history/.well-known"

    mkdir -p "$data_dir/history/.well-known" "$data_dir/buckets"

    {
        echo "starting validator..."
        echo "closing ledger 1"
        echo "closing ledger 2"
        if [[ "$synced" == "yes" ]]; then
            echo "  INFO state=Synced $SYNC_MARKER"
        fi
        echo "closing ledger 3"
    } > "$log"

    if [[ "$has_ledger" != "none" ]]; then
        printf '{"currentLedger": %s}\n' "$has_ledger" > "$has_dir/stellar-history.json"
    fi

    echo "$data_dir"
}

# ============================================================
# Test 1: never synced + --soft-on-sync-timeout → neutral soft-skip
#
# The environmental testnet-liveness case (#3280): validator.log lacks the sync
# marker and no HAS was published. Under the opt-in flag this is a neutral
# exit 0 with a grep-able SOFT-SKIP marker. FAILS on origin/main: the flag does
# not exist and the deadline path is an unconditional `exit 1`.
# ============================================================
test_never_synced_soft_skip() {
    local data_dir
    data_dir=$(make_fixture "soft" no none)

    local out="$TMPDIR_BASE/out-soft.txt"
    local exit_code=0
    "$SCRIPT" --soft-on-sync-timeout --classify-only "$data_dir" >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "never_synced_with_flag_exits_zero"
    else
        tap_not_ok "never_synced_with_flag_exits_zero" "exit=$exit_code (expected 0 soft-skip)"
    fi

    if grep -q 'SOFT-SKIP' "$out"; then
        tap_ok "never_synced_with_flag_emits_soft_skip"
    else
        tap_not_ok "never_synced_with_flag_emits_soft_skip" "no SOFT-SKIP marker in output"
    fi

    # A soft-skip must NOT be mislabelled a publish regression.
    if ! grep -q 'PUBLISH-REGRESSION' "$out"; then
        tap_ok "never_synced_soft_skip_not_publish_regression"
    else
        tap_not_ok "never_synced_soft_skip_not_publish_regression" \
            "PUBLISH-REGRESSION emitted for an environmental sync timeout"
    fi
}

# ============================================================
# Test 2: synced but no checkpoint → hard red with PUBLISH-REGRESSION
#
# The correctness keystone (#3280): the node reached sync (marker present) but
# published no HAS by the deadline — a real publish regression. This must stay a
# hard red EVEN WITH --soft-on-sync-timeout, and emit the PUBLISH-REGRESSION
# marker (not SOFT-SKIP).
# ============================================================
test_synced_no_publish_hard_red() {
    local data_dir
    data_dir=$(make_fixture "regress" yes none)

    local out="$TMPDIR_BASE/out-regress.txt"
    local exit_code=0
    "$SCRIPT" --soft-on-sync-timeout --classify-only "$data_dir" >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "synced_no_publish_exits_nonzero"
    else
        tap_not_ok "synced_no_publish_exits_nonzero" "exit=$exit_code (expected hard red)"
    fi

    if grep -q 'PUBLISH-REGRESSION' "$out"; then
        tap_ok "synced_no_publish_emits_publish_regression"
    else
        tap_not_ok "synced_no_publish_emits_publish_regression" "no PUBLISH-REGRESSION marker"
    fi

    # Must NOT soft-skip a real regression even with the flag on.
    if ! grep -q 'SOFT-SKIP' "$out"; then
        tap_ok "synced_no_publish_not_soft_skipped"
    else
        tap_not_ok "synced_no_publish_not_soft_skipped" \
            "SOFT-SKIP emitted for a synced-but-no-publish regression (masks a real break)"
    fi
}

# ============================================================
# Test 3: never synced WITHOUT the flag → still hard red (byte-identical default)
#
# Scope guard: with the flag OFF (local/dev default and any non-testnet caller),
# the deadline path stays a hard red exactly as before — proving the soft-skip is
# strictly opt-in.
# ============================================================
test_never_synced_default_red() {
    local data_dir
    data_dir=$(make_fixture "default" no none)

    local out="$TMPDIR_BASE/out-default.txt"
    local exit_code=0
    "$SCRIPT" --classify-only "$data_dir" >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "never_synced_without_flag_stays_red"
    else
        tap_not_ok "never_synced_without_flag_stays_red" "exit=$exit_code (expected non-zero default)"
    fi

    if ! grep -q 'SOFT-SKIP' "$out"; then
        tap_ok "never_synced_without_flag_no_soft_skip"
    else
        tap_not_ok "never_synced_without_flag_no_soft_skip" "SOFT-SKIP emitted without the flag"
    fi
}

# ============================================================
# Test 4: published checkpoint → PUBLISHED disposition (proceed to compare)
#
# When a HAS with currentLedger>0 exists, the classifier reports PUBLISHED and
# exits 0 — the caller then proceeds to the (unchanged) phase-2 compare. This
# disposition is independent of the sync marker / soft flag.
# ============================================================
test_published_proceeds() {
    local data_dir
    data_dir=$(make_fixture "published" yes 63)

    local out="$TMPDIR_BASE/out-published.txt"
    local exit_code=0
    "$SCRIPT" --classify-only "$data_dir" >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]] && grep -q 'PUBLISHED' "$out"; then
        tap_ok "published_checkpoint_reports_published"
    else
        tap_not_ok "published_checkpoint_reports_published" \
            "exit=$exit_code, PUBLISHED present=$(grep -q 'PUBLISHED' "$out" && echo yes || echo no)"
    fi

    # A published checkpoint is neither a soft-skip nor a regression.
    if ! grep -q 'SOFT-SKIP' "$out" && ! grep -q 'PUBLISH-REGRESSION' "$out"; then
        tap_ok "published_checkpoint_no_other_markers"
    else
        tap_not_ok "published_checkpoint_no_other_markers" \
            "unexpected SOFT-SKIP/PUBLISH-REGRESSION marker on a published checkpoint"
    fi
}

# ============================================================
# Test 5: the sync marker is pinned to the real run-loop log string
#
# Critic-A mandate (#3280): the classifier's "node reached sync" detection must
# be tied to a CONCRETE signal that cannot silently rot. The run loop logs the
# sync transition exactly once via tracing::info!(..., "Node is synced") in
# crates/app/src/run_cmd.rs::wait_for_sync. This asserts (a) that exact string is
# still present in run_cmd.rs (so a future log-wording change breaks here loudly,
# forcing the classifier's grep to be updated in lockstep), and (b) that the
# classifier in test-history-publish.sh greps for that same marker string.
# ============================================================
test_sync_marker_matches_run_loop_log() {
    if [[ ! -f "$RUN_CMD" ]]; then
        tap_not_ok "sync_marker_present_in_run_loop" "run_cmd.rs not found at $RUN_CMD"
        tap_not_ok "classifier_greps_for_sync_marker" "run_cmd.rs not found"
        return
    fi

    # (a) The exact marker is still emitted by wait_for_sync.
    if grep -qF "\"$SYNC_MARKER\"" "$RUN_CMD"; then
        tap_ok "sync_marker_present_in_run_loop"
    else
        tap_not_ok "sync_marker_present_in_run_loop" \
            "run_cmd.rs no longer logs \"$SYNC_MARKER\" — update SYNC_MARKER and the classifier grep in lockstep"
    fi

    # (b) The classifier script greps for the same marker string.
    if grep -qF "$SYNC_MARKER" "$SCRIPT"; then
        tap_ok "classifier_greps_for_sync_marker"
    else
        tap_not_ok "classifier_greps_for_sync_marker" \
            "test-history-publish.sh does not grep for the pinned sync marker \"$SYNC_MARKER\""
    fi
}

# ============================================================
# Test 6: workflow wires --soft-on-sync-timeout on the testnet job only
#
# The daily testnet job must opt into the soft-skip (the whole point of #3280),
# and the run step must invoke the script with the flag. Text/structure
# assertion (the harness cannot run the GitHub Actions runner).
# ============================================================
test_workflow_wires_soft_flag() {
    if [[ ! -f "$WORKFLOW" ]]; then
        tap_not_ok "workflow_passes_soft_on_sync_timeout" "workflow not found"
        tap_not_ok "workflow_runs_history_publish_script" "workflow not found"
        return
    fi

    if grep -q -- '--soft-on-sync-timeout' "$WORKFLOW"; then
        tap_ok "workflow_passes_soft_on_sync_timeout"
    else
        tap_not_ok "workflow_passes_soft_on_sync_timeout" \
            "history-publish.yml must pass --soft-on-sync-timeout on the run step"
    fi

    if grep -q 'test-history-publish.sh' "$WORKFLOW"; then
        tap_ok "workflow_runs_history_publish_script"
    else
        tap_not_ok "workflow_runs_history_publish_script" \
            "workflow no longer runs scripts/test-history-publish.sh"
    fi
}

# --- Run all tests ---
tap_plan 14

test_never_synced_soft_skip
test_synced_no_publish_hard_red
test_never_synced_default_red
test_published_proceeds
test_sync_marker_matches_run_loop_log
test_workflow_wires_soft_flag

echo ""
echo "# Results: $PASS_COUNT/$TEST_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    exit 1
fi
exit 0
