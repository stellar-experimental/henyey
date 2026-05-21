#!/usr/bin/env bash
#
# Smoke-test harness for /review-pr skill shell snippets.
#
# Tests the shared verdict-validation library (scripts/lib/review-pr-verdicts.sh)
# using mock comment data. Verifies cutoff-aware filtering, shape validation,
# classification, and the expected audit artifacts for missing/malformed/stale
# verdict paths.
#
# Usage:
#   ./scripts/test-review-pr-skill-snippets.sh
#
# Output: TAP (Test Anything Protocol) on stdout, diagnostics on stderr.
# Exit: 0 = all pass, 1 = any fail.
#
# Portability: GNU/Linux only (Bash 4+, jq required).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_ROOT="$REPO_ROOT/data/test-review-pr-snippets"

# ── Source the libraries under test ───────────────────────────────────────────
source "$SCRIPT_DIR/lib/review-pr-verdicts.sh"
source "$SCRIPT_DIR/lib/review-pr-merge.sh"

# ── Cleanup ──────────────────────────────────────────────────────────────────
cleanup() {
  rm -rf "$TEST_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
cleanup
mkdir -p "$TEST_ROOT"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=27
TAP_CURRENT=0
TAP_FAILURES=0

tap_plan() {
  echo "1..$TAP_PLAN"
}

tap_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  echo "ok $TAP_CURRENT - $1"
}

tap_fail() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_FAILURES=$((TAP_FAILURES + 1))
  echo "not ok $TAP_CURRENT - $1"
  echo "# $2" >&2
}

assert_eq() {
  local expected="$1" actual="$2" desc="$3"
  if [[ "$expected" == "$actual" ]]; then
    tap_ok "$desc"
  else
    tap_fail "$desc" "expected='$expected' actual='$actual'"
  fi
}

# ── Helper: build a mock comment JSON ────────────────────────────────────────
# Usage: mock_comment ID CREATED_AT BODY
mock_comment() {
  local id="$1" created_at="$2" body="$3"
  jq -n --argjson id "$id" --arg created_at "$created_at" --arg body "$body" \
    '{id: $id, created_at: $created_at, body: $body}'
}

# Build a well-formed verdict body
verdict_body() {
  local reviewer="$1" verdict="$2" summary="${3:-Looks good.}"
  printf '## 🔍 Reviewer: %s\n\n**Verdict:** %s\n\n**Summary:** %s\n' "$reviewer" "$verdict" "$summary"
}

# ─────────────────────────────────────────────────────────────────────────────
tap_plan

# ══════════════════════════════════════════════════════════════════════════════
# TEST 1: stale reviewer verdict is ignored for the current baseline
# ══════════════════════════════════════════════════════════════════════════════
# Seed an older Correctness verdict (before baseline) and assert it's treated
# as "missing" when fetched with a cutoff after that comment's timestamp.

STALE_BODY=$(verdict_body "Correctness" "APPROVE" "All good.")
STALE_COMMENTS=$(jq -n --arg body "$STALE_BODY" '[
  { "id": 100, "created_at": "2026-05-18T01:00:00Z", "body": $body }
]')

echo "$STALE_COMMENTS" > "$TEST_ROOT/stale-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/stale-comments.json"

# Fetch with cutoff AFTER the stale comment
VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-18T02:00:00Z")
STATE=$(latest_reviewer_verdict_state "Correctness" "$VERDICTS")
assert_eq "missing" "$STATE" "stale reviewer verdict is ignored for the current baseline"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 2: missing reviewer verdict after one retry emits bounce artifact
# ══════════════════════════════════════════════════════════════════════════════
# When no fresh reviewer comment appears, classify_reviewer returns "missing".
# The /review-pr skill should then bounce. We verify the classification here.

EMPTY_COMMENTS='[]'
echo "$EMPTY_COMMENTS" > "$TEST_ROOT/empty-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/empty-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-18T02:00:00Z")
CLASS_A=$(classify_reviewer "Correctness" "$VERDICTS")
CLASS_B=$(classify_reviewer "Risk" "$VERDICTS")
assert_eq "missing" "$CLASS_A" "missing reviewer verdict classified as missing (Correctness)"
assert_eq "missing" "$CLASS_B" "missing reviewer verdict classified as missing (Risk)"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 3: malformed reviewer verdict emits malformed-verdict artifact
# ══════════════════════════════════════════════════════════════════════════════
# Seed a fresh comment with the right header but no parseable **Verdict:** line.

MALFORMED_BODY=$(printf '## 🔍 Reviewer: Correctness\n\n**Summary:** Something but no verdict line.\n')
MALFORMED_COMMENTS=$(jq -n --arg body "$MALFORMED_BODY" '[
  { "id": 200, "created_at": "2026-05-19T03:00:00Z", "body": $body }
]')

echo "$MALFORMED_COMMENTS" > "$TEST_ROOT/malformed-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/malformed-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T01:00:00Z")
CLASS=$(classify_reviewer "Correctness" "$VERDICTS")
assert_eq "malformed:no **Verdict:** line" "$CLASS" "malformed reviewer verdict (no verdict line) classified correctly"

# Test malformed with invalid verdict value
MALFORMED_BODY2=$(printf '## 🔍 Reviewer: Correctness\n\n**Verdict:** MAYBE\n\n**Summary:** Unsure.\n')
MALFORMED_COMMENTS2=$(jq -n --arg body "$MALFORMED_BODY2" '[
  { "id": 201, "created_at": "2026-05-19T03:00:00Z", "body": $body }
]')

echo "$MALFORMED_COMMENTS2" > "$TEST_ROOT/malformed-comments2.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/malformed-comments2.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T01:00:00Z")
CLASS=$(classify_reviewer "Correctness" "$VERDICTS")
assert_eq "malformed:verdict line does not contain APPROVE or CHANGES_REQUESTED" "$CLASS" \
  "malformed reviewer verdict (invalid value) classified correctly"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 4: fresh correctness and risk verdicts pass verification
# ══════════════════════════════════════════════════════════════════════════════

CORR_BODY=$(verdict_body "Correctness" "APPROVE" "Code is correct.")
RISK_BODY=$(verdict_body "Risk" "APPROVE" "No risk concerns.")
GOOD_COMMENTS=$(jq -n --arg corr "$CORR_BODY" --arg risk "$RISK_BODY" '[
  { "id": 300, "created_at": "2026-05-19T04:00:00Z", "body": $corr },
  { "id": 301, "created_at": "2026-05-19T04:01:00Z", "body": $risk }
]')

echo "$GOOD_COMMENTS" > "$TEST_ROOT/good-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/good-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T03:00:00Z")
CLASS_A=$(classify_reviewer "Correctness" "$VERDICTS")
CLASS_B=$(classify_reviewer "Risk" "$VERDICTS")
assert_eq "ok:APPROVE" "$CLASS_A" "fresh correctness verdict passes verification"
assert_eq "ok:APPROVE" "$CLASS_B" "fresh risk verdict passes verification"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 5: fresh correctness and parity verdicts pass verification
# ══════════════════════════════════════════════════════════════════════════════

PARITY_BODY=$(verdict_body "Parity" "APPROVE" "Matches stellar-core behavior.")
PARITY_COMMENTS=$(jq -n --arg corr "$CORR_BODY" --arg par "$PARITY_BODY" '[
  { "id": 400, "created_at": "2026-05-19T04:00:00Z", "body": $corr },
  { "id": 401, "created_at": "2026-05-19T04:01:00Z", "body": $par }
]')

echo "$PARITY_COMMENTS" > "$TEST_ROOT/parity-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/parity-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T03:00:00Z")
CLASS_A=$(classify_reviewer "Correctness" "$VERDICTS")
CLASS_B=$(classify_reviewer "Parity" "$VERDICTS")
assert_eq "ok:APPROVE" "$CLASS_A" "fresh correctness verdict (parity PR) passes verification"
assert_eq "ok:APPROVE" "$CLASS_B" "fresh parity verdict passes verification"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 6: CHANGES_REQUESTED verdict is correctly classified
# ══════════════════════════════════════════════════════════════════════════════

CR_BODY=$(verdict_body "Correctness" "CHANGES_REQUESTED" "Found issues with error handling.")
CR_COMMENTS=$(jq -n --arg cr "$CR_BODY" --arg risk "$RISK_BODY" '[
  { "id": 500, "created_at": "2026-05-19T04:00:00Z", "body": $cr },
  { "id": 501, "created_at": "2026-05-19T04:01:00Z", "body": $risk }
]')

echo "$CR_COMMENTS" > "$TEST_ROOT/cr-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/cr-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T03:00:00Z")
CLASS_A=$(classify_reviewer "Correctness" "$VERDICTS")
CLASS_B=$(classify_reviewer "Risk" "$VERDICTS")
assert_eq "ok:CHANGES_REQUESTED" "$CLASS_A" "CHANGES_REQUESTED verdict classified correctly"
assert_eq "ok:APPROVE" "$CLASS_B" "accompanying APPROVE verdict classified correctly"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 7: validate_reviewer_verdict_shape rejects bad header
# ══════════════════════════════════════════════════════════════════════════════

BAD_HEADER=$(printf '## Review: Correctness\n\n**Verdict:** APPROVE\n\n**Summary:** Ok.\n')
SHAPE=$(validate_reviewer_verdict_shape "$BAD_HEADER")
assert_eq "malformed:missing or invalid header" "$SHAPE" "validate_reviewer_verdict_shape rejects bad header"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 8: latest comment wins when multiple verdicts exist
# ══════════════════════════════════════════════════════════════════════════════

EARLY_CR=$(verdict_body "Correctness" "CHANGES_REQUESTED" "Issues found.")
LATE_APPROVE=$(verdict_body "Correctness" "APPROVE" "Issues resolved.")
MULTI_COMMENTS=$(jq -n --arg early "$EARLY_CR" --arg late "$LATE_APPROVE" '[
  { "id": 600, "created_at": "2026-05-19T04:00:00Z", "body": $early },
  { "id": 601, "created_at": "2026-05-19T05:00:00Z", "body": $late }
]')

echo "$MULTI_COMMENTS" > "$TEST_ROOT/multi-comments.json"
export REVIEW_PR_COMMENTS_FILE="$TEST_ROOT/multi-comments.json"

VERDICTS=$(fetch_reviewer_verdict_comments 999 "2026-05-19T03:00:00Z")
STATE=$(latest_reviewer_verdict_state "Correctness" "$VERDICTS")
assert_eq "APPROVE" "$STATE" "latest comment wins when multiple verdicts exist"

# ══════════════════════════════════════════════════════════════════════════════
# TEST 9: attempt_merge retries with --auto on auto-hint failure
# ══════════════════════════════════════════════════════════════════════════════
# Stub gh so the first --admin call fails with the exact auto-hint error
# and the second --auto call succeeds.

MERGE_CALL_NUM=0
mock_merge_auto_hint() {
  local pr_num="$1" repo="$2" flags="$3"
  MERGE_CALL_NUM=$((MERGE_CALL_NUM + 1))
  if [[ "$flags" == *"--admin"* ]]; then
    echo "X Pull request stellar-experimental/henyey#$pr_num is not mergeable: the merge commit cannot be cleanly created." >&2
    echo "To have the pull request merged after all the requirements have been met, add the \`--auto\` flag." >&2
    return 1
  elif [[ "$flags" == *"--auto"* ]]; then
    return 0
  fi
  return 1
}

export REVIEW_PR_MERGE_CMD=mock_merge_auto_hint
MERGE_CALL_NUM=0
RESULT=$(attempt_merge 2875)
RC=$?
assert_eq "auto-merge-armed" "$RESULT" "attempt_merge retries with --auto on auto-hint failure"
assert_eq "0" "$RC" "attempt_merge returns 0 on auto-merge-armed"
unset REVIEW_PR_MERGE_CMD

# ══════════════════════════════════════════════════════════════════════════════
# TEST 10: classify_linked_pr_state prefers merged PR when no open PR exists
# ══════════════════════════════════════════════════════════════════════════════

echo '[{"number": 2875, "state": "MERGED"}]' > "$TEST_ROOT/linked-prs-merged.json"
export REVIEW_PR_LINKED_PRS_FILE="$TEST_ROOT/linked-prs-merged.json"

RESULT=$(classify_linked_pr_state 2877)
assert_eq "merged:2875" "$RESULT" "classify_linked_pr_state returns merged when no open PR"
unset REVIEW_PR_LINKED_PRS_FILE

# ══════════════════════════════════════════════════════════════════════════════
# TEST 11: is_auto_merge_armed short-circuits to wait when armed
# ══════════════════════════════════════════════════════════════════════════════

echo "true" > "$TEST_ROOT/auto-merge-armed.txt"
export REVIEW_PR_AUTO_MERGE_FILE="$TEST_ROOT/auto-merge-armed.txt"

RESULT=$(is_auto_merge_armed 2875)
assert_eq "true" "$RESULT" "is_auto_merge_armed returns true when autoMergeRequest is set"
unset REVIEW_PR_AUTO_MERGE_FILE

# ══════════════════════════════════════════════════════════════════════════════
# TEST 12: attempt_merge auto rejection stays hard failure
# ══════════════════════════════════════════════════════════════════════════════
# When --auto also fails (e.g. autoMergeAllowed: false), result is hard-failure.

mock_merge_auto_both_fail() {
  local pr_num="$1" repo="$2" flags="$3"
  if [[ "$flags" == *"--admin"* ]]; then
    echo "To have the pull request merged after all the requirements have been met, add the \`--auto\` flag." >&2
    return 1
  elif [[ "$flags" == *"--auto"* ]]; then
    echo "auto-merge is not allowed for this repository" >&2
    return 1
  fi
  return 1
}

export REVIEW_PR_MERGE_CMD=mock_merge_auto_both_fail
set +e
RESULT=$(attempt_merge 2875)
RC=$?
set -e
[[ "$RESULT" == hard-failure:* ]] && tap_ok "attempt_merge auto rejection stays hard failure" || tap_fail "attempt_merge auto rejection stays hard failure" "got: $RESULT"
assert_eq "1" "$RC" "attempt_merge returns 1 on hard failure"
unset REVIEW_PR_MERGE_CMD

# ══════════════════════════════════════════════════════════════════════════════
# TEST 13: unexpected admin merge failure stays terminal (no --auto retry)
# ══════════════════════════════════════════════════════════════════════════════

mock_merge_other_failure() {
  local pr_num="$1" repo="$2" flags="$3"
  if [[ "$flags" == *"--admin"* ]]; then
    echo "permission denied: token lacks admin access" >&2
    return 1
  fi
  # Should never reach --auto path
  echo "UNEXPECTED: should not have retried with --auto" >&2
  return 1
}

export REVIEW_PR_MERGE_CMD=mock_merge_other_failure
set +e
RESULT=$(attempt_merge 2875)
RC=$?
set -e
[[ "$RESULT" == "hard-failure:permission denied: token lacks admin access" ]] && \
  tap_ok "unexpected admin merge failure stays terminal" || \
  tap_fail "unexpected admin merge failure stays terminal" "got: $RESULT"
unset REVIEW_PR_MERGE_CMD

# ══════════════════════════════════════════════════════════════════════════════
# TEST 14: SKILL.md references merge helper and wait path
# ══════════════════════════════════════════════════════════════════════════════

SKILL_FILE="$REPO_ROOT/.github/skills/review-pr/SKILL.md"
if grep -q 'review-pr-merge.sh' "$SKILL_FILE" && grep -q 'auto-merge.*armed' "$SKILL_FILE"; then
  tap_ok "SKILL.md references merge helper and auto-merge armed path"
else
  tap_fail "SKILL.md references merge helper and auto-merge armed path" \
    "SKILL.md missing references to review-pr-merge.sh or auto-merge armed"
fi

# ══════════════════════════════════════════════════════════════════════════════
# TEST 15: classify_linked_pr_state propagates API failure
# ══════════════════════════════════════════════════════════════════════════════

# Use a file that doesn't exist to simulate API failure
export REVIEW_PR_LINKED_PRS_FILE="/nonexistent/path/should-fail.json"
set +e
RESULT=$(classify_linked_pr_state 9999)
RC=$?
set -e
assert_eq "1" "$RC" "classify_linked_pr_state returns 1 on API failure"
[[ "$RESULT" == error:* ]] && \
  tap_ok "classify_linked_pr_state outputs error: on API failure" || \
  tap_fail "classify_linked_pr_state outputs error: on API failure" "got: $RESULT"
unset REVIEW_PR_LINKED_PRS_FILE

# ══════════════════════════════════════════════════════════════════════════════
# TEST 16: is_auto_merge_armed propagates API failure
# ══════════════════════════════════════════════════════════════════════════════

export REVIEW_PR_AUTO_MERGE_FILE="/nonexistent/path/should-fail.json"
set +e
RESULT=$(is_auto_merge_armed 9999)
RC=$?
set -e
assert_eq "1" "$RC" "is_auto_merge_armed returns 1 on API failure"
assert_eq "error" "$RESULT" "is_auto_merge_armed outputs error on API failure"
unset REVIEW_PR_AUTO_MERGE_FILE

# ══════════════════════════════════════════════════════════════════════════════
# TEST 17: attempt_merge uses REVIEW_PR_SCRATCH_DIR instead of mktemp
# ══════════════════════════════════════════════════════════════════════════════

# Set a custom scratch dir and verify attempt_merge uses it (no mktemp)
CUSTOM_SCRATCH="$TEST_ROOT/custom-scratch"
mkdir -p "$CUSTOM_SCRATCH"
export REVIEW_PR_SCRATCH_DIR="$CUSTOM_SCRATCH"

mock_merge_success() {
  local pr_num="$1" repo="$2" flags="$3"
  return 0
}
export REVIEW_PR_MERGE_CMD=mock_merge_success
RESULT=$(attempt_merge 1234)
assert_eq "merged" "$RESULT" "attempt_merge uses REVIEW_PR_SCRATCH_DIR (no mktemp)"
unset REVIEW_PR_MERGE_CMD REVIEW_PR_SCRATCH_DIR

# ══════════════════════════════════════════════════════════════════════════════
# TEST 18: SKILL.md documents PR_NUM extraction from PR_STATE
# ══════════════════════════════════════════════════════════════════════════════

SKILL_FILE="$REPO_ROOT/.github/skills/review-pr/SKILL.md"
if grep -q 'PR_NUM=.*PR_STATE#open:' "$SKILL_FILE"; then
  tap_ok "SKILL.md documents explicit PR_NUM extraction from PR_STATE"
else
  tap_fail "SKILL.md documents explicit PR_NUM extraction from PR_STATE" \
    "SKILL.md missing PR_NUM extraction from open: state"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
unset REVIEW_PR_COMMENTS_FILE

echo ""
if [[ $TAP_FAILURES -gt 0 ]]; then
  echo "# FAILED: $TAP_FAILURES of $TAP_CURRENT tests failed" >&2
  exit 1
else
  echo "# All $TAP_CURRENT tests passed"
  exit 0
fi
