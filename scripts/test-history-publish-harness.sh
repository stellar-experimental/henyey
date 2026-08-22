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
APP_MOD="$REPO_ROOT/crates/app/src/app/mod.rs"
LIFECYCLE="$REPO_ROOT/crates/app/src/app/lifecycle.rs"

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

# ============================================================
# Tests 7-8: in-loop stall detector (#3741)
#
# Background (#3741): #3732 disabled the fixture's internal watchdog auto-abort
# because it was killing a still-progressing node under legitimate slow-verify
# load. That removed the only code path that turned "event loop badly stalled"
# into a clean, script-detected process death — so now the node just keeps
# grinding until GitHub Actions' own external job/run-level cancellation kills
# the whole runner, which skips even if: always()-gated steps (Upload
# logs/Summary never run), leaving zero validator.log evidence for any of the
# four recurrences (#3280, #3707, #3727, #3741).
#
# is_log_stalled() (driven via the --classify-stall-only <log_file>
# <threshold_secs> CLI seam) is a pure mtime-staleness check: if validator.log
# has produced zero new bytes for threshold_secs while the harness's own
# process liveness check still says the node is alive, this is a genuine stall
# distinct from "testnet never became reachable" (SYNC-TIMEOUT) — always a hard
# red (DISPOSITION: STALL, exit 1, never covered by --soft-on-sync-timeout), so
# the workflow's existing `if: failure()` Upload-logs step fires and the full
# validator.log artifact is captured.
# ============================================================

# Build a synthetic validator.log fixture with a controllable mtime.
# Args: <name> <mtime_offset_secs>  (0 = now/fresh; >0 = that many seconds ago)
make_stall_fixture() {
    local name="$1"
    local mtime_offset_secs="$2"
    local data_dir="$TMPDIR_BASE/stall-fixture-$name"
    local log="$data_dir/validator.log"

    mkdir -p "$data_dir"
    echo "closing ledger 1" > "$log"

    if [[ "$mtime_offset_secs" -gt 0 ]]; then
        touch -d "@$(( $(date +%s) - mtime_offset_secs ))" "$log"
    fi

    echo "$log"
}

# ------------------------------------------------------------
# Test 7: a validator.log with a stale mtime (no forward progress for longer
# than the threshold) is flagged as a stall — hard red, DISPOSITION: STALL.
# FAILS on origin/main: --classify-stall-only does not exist (the script hits
# the `*) echo "Unknown arg: $1"; exit 1 ;;` branch, exit code happens to be 1
# for the wrong reason, and NO "DISPOSITION: STALL" marker is ever printed).
# ------------------------------------------------------------
test_stall_detector_flags_stale_log() {
    local log
    log=$(make_stall_fixture "stale" 600)  # mtime 10 minutes in the past

    local out="$TMPDIR_BASE/out-stall-stale.txt"
    local exit_code=0
    "$SCRIPT" --classify-stall-only "$log" 180 >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -ne 0 ]]; then
        tap_ok "stale_log_stall_detector_exits_nonzero"
    else
        tap_not_ok "stale_log_stall_detector_exits_nonzero" "exit=$exit_code (expected 1, stalled)"
    fi

    if grep -q 'DISPOSITION: STALL' "$out"; then
        tap_ok "stale_log_stall_detector_emits_disposition_stall"
    else
        tap_not_ok "stale_log_stall_detector_emits_disposition_stall" \
            "no DISPOSITION: STALL marker in output: $(cat "$out")"
    fi
}

# ------------------------------------------------------------
# Test 8: a validator.log with a fresh mtime (progress within the threshold)
# is NOT flagged — exit 0, no STALL marker. Same origin/main failure mode as
# test 7 (flag doesn't exist yet).
# ------------------------------------------------------------
test_stall_detector_ignores_fresh_log() {
    local log
    log=$(make_stall_fixture "fresh" 0)

    local out="$TMPDIR_BASE/out-stall-fresh.txt"
    local exit_code=0
    "$SCRIPT" --classify-stall-only "$log" 180 >"$out" 2>&1 || exit_code=$?

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "fresh_log_stall_detector_exits_zero"
    else
        tap_not_ok "fresh_log_stall_detector_exits_zero" "exit=$exit_code (expected 0, not stalled)"
    fi

    if ! grep -q 'DISPOSITION: STALL' "$out"; then
        tap_ok "fresh_log_stall_detector_no_stall_marker"
    else
        tap_not_ok "fresh_log_stall_detector_no_stall_marker" \
            "unexpected DISPOSITION: STALL marker for a fresh log"
    fi
}

# ============================================================
# Test 9: the live-tail diagnostic grep filter is pinned to real signal names
#
# The script's incremental live-tail (new validator.log lines echoed into the
# step's own GitHub-captured stdout every poll iteration) is filtered through a
# grep for known diagnostic families, so it doesn't flood CI log volume. This
# pins those family tokens to (a) their presence in the script's filter, and
# (b) the actual strings still emitted by the Rust source that produced them
# (crates/app/src/app/mod.rs's "WATCHDOG: ..." events, #3727/#3732; and
# crates/app/src/app/lifecycle.rs's db_write_ctx = "peer-record-update",
# #3702/#3704) — mirroring test_sync_marker_matches_run_loop_log's "pin the
# string so wording drift breaks loudly" pattern.
# ============================================================
test_diagnostic_grep_pins_known_signal_names() {
    if [[ ! -f "$APP_MOD" ]] || [[ ! -f "$LIFECYCLE" ]]; then
        tap_not_ok "diagnostic_filter_pins_all_known_families" "source files not found"
        tap_not_ok "watchdog_marker_present_in_app_mod" "source files not found"
        tap_not_ok "db_write_ctx_marker_present_in_lifecycle" "source files not found"
        return
    fi

    # (a) The script's live-tail filter must reference each known diagnostic
    # family token established by #3727's root-cause capture and the #3702
    # db_write_ctx instrumentation.
    local families=("WATCHDOG" "maxtps_scp" "database is locked" "straggler timeout" "db_write_ctx")
    local missing=""
    for f in "${families[@]}"; do
        if ! grep -qF "$f" "$SCRIPT"; then
            missing="$missing [$f]"
        fi
    done
    if [[ -z "$missing" ]]; then
        tap_ok "diagnostic_filter_pins_all_known_families"
    else
        tap_not_ok "diagnostic_filter_pins_all_known_families" \
            "test-history-publish.sh live-tail filter missing:$missing"
    fi

    # (b) Cross-check those tokens are still emitted by the real source, so a
    # future wording change breaks this harness loudly instead of silently
    # rotting the live-tail filter into a no-op for that family.
    if grep -qF "WATCHDOG:" "$APP_MOD"; then
        tap_ok "watchdog_marker_present_in_app_mod"
    else
        tap_not_ok "watchdog_marker_present_in_app_mod" \
            "crates/app/src/app/mod.rs no longer emits a WATCHDOG: marker"
    fi

    if grep -qF 'db_write_ctx = "peer-record-update"' "$LIFECYCLE"; then
        tap_ok "db_write_ctx_marker_present_in_lifecycle"
    else
        tap_not_ok "db_write_ctx_marker_present_in_lifecycle" \
            "crates/app/src/app/lifecycle.rs no longer emits db_write_ctx = \"peer-record-update\""
    fi
}

# ============================================================
# Tests 10-12: the live-tail diagnostic echo advances its line offset by
# exactly the number of lines it actually read (#3745).
#
# Root cause the tests pin: the old inline live-tail block read validator.log
# twice — a `wc -l` line count then a separate `tail -n +N`. If the log grew
# between the two reads, the recorded offset (from the earlier, smaller count)
# lagged what tail emitted, so the next poll re-tailed from the same offset and
# reprinted already-seen diagnostic lines. The fix collapses this to a single
# read whose new offset is derived from exactly the emitted slice, driven
# offline via the --emit-new-lines-only <log_file> <last_line> seam.
#
# FAIL on origin/main: --emit-new-lines-only does not exist (the script hits
# the `*) echo "Unknown arg: $1"; exit 1 ;;` branch — exit 1, no NEW_LAST_LINE:
# marker, matching lines never emitted). Same seam-absent precedent as the
# stall-detector tests 7/8.
# ============================================================

# Build a 5-line synthetic validator.log with exactly 2 diagnostic-matching
# lines (WATCHDOG and db_write_ctx families).
make_tail_fixture() {
    local name="$1"
    local data_dir="$TMPDIR_BASE/tail-fixture-$name"
    local log="$data_dir/validator.log"

    mkdir -p "$data_dir"
    {
        echo "closing ledger 1"
        echo "WATCHDOG: scp verify falling behind under load"
        echo "closing ledger 2"
        echo 'db_write_ctx = "peer-record-update" slow write'
        echo "closing ledger 3"
    } > "$log"

    echo "$log"
}

# ------------------------------------------------------------
# Test 10: reported offset equals the number of lines actually read/emitted.
# A fresh 5-line log (2 matching) tailed from offset 0 must emit exactly those
# 2 diagnostic lines and report NEW_LAST_LINE: 5 — the invariant the TOCTOU
# broke (old code advanced by an independent `wc -l` that could differ from
# what tail read).
# ------------------------------------------------------------
test_live_tail_offset_matches_consumed_lines() {
    local log
    log=$(make_tail_fixture "offset")

    local out="$TMPDIR_BASE/out-tail-offset.txt"
    local exit_code=0
    "$SCRIPT" --emit-new-lines-only "$log" 0 >"$out" 2>&1 || exit_code=$?

    local emitted
    emitted=$(grep -v '^NEW_LAST_LINE:' "$out" | grep -c .)
    if [[ "$emitted" -eq 2 ]] \
        && grep -q 'WATCHDOG' "$out" && grep -q 'db_write_ctx' "$out"; then
        tap_ok "live_tail_emits_exactly_the_matching_lines"
    else
        tap_not_ok "live_tail_emits_exactly_the_matching_lines" \
            "expected 2 diagnostic lines (WATCHDOG + db_write_ctx), got $emitted: $(cat "$out")"
    fi

    if grep -q '^NEW_LAST_LINE: 5$' "$out"; then
        tap_ok "live_tail_offset_equals_lines_read"
    else
        tap_not_ok "live_tail_offset_equals_lines_read" \
            "expected NEW_LAST_LINE: 5, got: $(cat "$out")"
    fi
}

# ------------------------------------------------------------
# Test 11: idempotent no-reprint on an unchanged file — the direct
# anti-duplicate assertion for the reported symptom. After consuming the 5-line
# log (offset 5), re-tailing the unchanged file from offset 5 must emit no
# diagnostic line, report NEW_LAST_LINE: 5, and exit 0.
# ------------------------------------------------------------
test_live_tail_no_reprint_on_second_call() {
    local log
    log=$(make_tail_fixture "noreprint")

    local out="$TMPDIR_BASE/out-tail-noreprint.txt"
    local exit_code=0
    "$SCRIPT" --emit-new-lines-only "$log" 5 >"$out" 2>&1 || exit_code=$?

    local emitted
    emitted=$(grep -v '^NEW_LAST_LINE:' "$out" | grep -c .)
    if [[ "$emitted" -eq 0 ]]; then
        tap_ok "live_tail_no_reprint_of_seen_lines"
    else
        tap_not_ok "live_tail_no_reprint_of_seen_lines" \
            "expected no diagnostic lines re-emitted, got $emitted: $(cat "$out")"
    fi

    if grep -q '^NEW_LAST_LINE: 5$' "$out"; then
        tap_ok "live_tail_offset_unchanged_on_no_growth"
    else
        tap_not_ok "live_tail_offset_unchanged_on_no_growth" \
            "expected NEW_LAST_LINE: 5, got: $(cat "$out")"
    fi

    if [[ $exit_code -eq 0 ]]; then
        tap_ok "live_tail_no_reprint_exits_zero"
    else
        tap_not_ok "live_tail_no_reprint_exits_zero" "exit=$exit_code (expected 0)"
    fi
}

# ------------------------------------------------------------
# Test 12: correct incremental progress across an append. After consuming the
# 5-line log (offset 5), append 3 lines (1 matching); tailing from offset 5
# must emit exactly the 1 new matching line and report NEW_LAST_LINE: 8.
# ------------------------------------------------------------
test_live_tail_incremental_append() {
    local log
    log=$(make_tail_fixture "append")

    {
        echo "closing ledger 4"
        echo "straggler timeout waiting on peer"
        echo "closing ledger 5"
    } >> "$log"

    local out="$TMPDIR_BASE/out-tail-append.txt"
    local exit_code=0
    "$SCRIPT" --emit-new-lines-only "$log" 5 >"$out" 2>&1 || exit_code=$?

    local emitted
    emitted=$(grep -v '^NEW_LAST_LINE:' "$out" | grep -c .)
    if [[ "$emitted" -eq 1 ]] && grep -q 'straggler timeout' "$out"; then
        tap_ok "live_tail_emits_only_new_matching_line"
    else
        tap_not_ok "live_tail_emits_only_new_matching_line" \
            "expected 1 new diagnostic line (straggler timeout), got $emitted: $(cat "$out")"
    fi

    if grep -q '^NEW_LAST_LINE: 8$' "$out"; then
        tap_ok "live_tail_offset_advances_across_append"
    else
        tap_not_ok "live_tail_offset_advances_across_append" \
            "expected NEW_LAST_LINE: 8, got: $(cat "$out")"
    fi
}

# --- Run all tests ---
tap_plan 28

test_never_synced_soft_skip
test_synced_no_publish_hard_red
test_never_synced_default_red
test_published_proceeds
test_sync_marker_matches_run_loop_log
test_workflow_wires_soft_flag
test_stall_detector_flags_stale_log
test_stall_detector_ignores_fresh_log
test_diagnostic_grep_pins_known_signal_names
test_live_tail_offset_matches_consumed_lines
test_live_tail_no_reprint_on_second_call
test_live_tail_incremental_append

echo ""
echo "# Results: $PASS_COUNT/$TEST_COUNT passed, $FAIL_COUNT failed"

if [[ $FAIL_COUNT -gt 0 ]]; then
    exit 1
fi
exit 0
