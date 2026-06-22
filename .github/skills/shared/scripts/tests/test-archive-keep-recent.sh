#!/usr/bin/env bash
# test-archive-keep-recent.sh — regression tests for the count-based
# `--keep-recent N` selection mode of archive-stale-done.sh.
#
# Tests (all use SKIP_PREFLIGHT=1 + --dry-run + a mocked `gh`, so no network
# call and no real archive mutation is made):
#   1. test_keep_recent_archives_all_but_n
#        Mock returns 13 CLOSED items with distinct closedAt. With
#        --keep-recent 10, asserts exactly the 3 OLDEST are selected (the 10
#        most-recently-closed are kept) — regardless of age cutoff.
#   2. test_keep_recent_noop_when_within_limit
#        Mock returns 5 CLOSED items. With --keep-recent 10, asserts nothing
#        is archived and the script exits 0 with the "No surplus" message.
#   3. test_keep_recent_rejects_non_integer
#        --keep-recent abc → exit 2 with a structured error (arg validation).
#
# Usage:
#   bash .github/skills/shared/scripts/tests/test-archive-keep-recent.sh
#
# Exits 0 if all tests pass, non-zero on any failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_SCRIPT="$SCRIPT_DIR/../archive-stale-done.sh"

if [ ! -x "$TARGET_SCRIPT" ]; then
  echo "FAIL: target script not found or not executable: $TARGET_SCRIPT" >&2
  exit 1
fi

FAILED=0
PASSED=0

# Build a mock `gh` that returns a single items page with the given number of
# CLOSED items, numbered 1..N with closedAt = 2026-06-<DD>T00:00:00Z so item
# #N is the most-recently-closed. Writes the mock to $1/gh.
make_mock_gh_with_n_items() {
  local dir="$1" n="$2" i dd nodes=""
  for ((i = 1; i <= n; i++)); do
    printf -v dd '%02d' "$i"
    nodes+="{\"id\":\"I_${i}\",\"isArchived\":false,\"content\":{\"number\":${i},\"state\":\"CLOSED\",\"closedAt\":\"2026-06-${dd}T00:00:00Z\"}}"
    [ "$i" -lt "$n" ] && nodes+=","
  done
  cat >"$dir/gh" <<EOF
#!/usr/bin/env bash
# Mock gh: return one items page with ${n} CLOSED items for the main fetch.
if [[ "\$1" == "api" && "\$2" == "graphql" ]]; then
  cat <<'JSON'
{"data":{"organization":{"projectV2":{"items":{"pageInfo":{"endCursor":null,"hasNextPage":false},"nodes":[${nodes}]}}}}}
JSON
  exit 0
fi
exit 0
EOF
  chmod +x "$dir/gh"
}

# ---------------------------------------------------------------------------
# Test 1: 13 items, keep 10 → archive the 3 oldest (#1,#2,#3) only.
# ---------------------------------------------------------------------------
test_keep_recent_archives_all_but_n() {
  local name="test_keep_recent_archives_all_but_n"
  local tmpdir; tmpdir="$(mktemp -d)"; trap 'rm -rf "$tmpdir"' RETURN
  make_mock_gh_with_n_items "$tmpdir" 13

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" GH_TOKEN="dummy" SKIP_PREFLIGHT=1 \
           bash "$TARGET_SCRIPT" --dry-run --keep-recent 10 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 0 ]; then
    echo "FAIL: $name — expected exit 0, got $exit_code"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  if ! grep -q "Dry-run complete: 3 item(s) would be archived." <<<"$output"; then
    echo "FAIL: $name — expected exactly 3 items archived"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  # The 3 oldest must be selected; the newest (and the boundary #4) must NOT.
  local must_have=("#1 " "#2 " "#3 ") must_not=("#4 " "#13 ")
  local tok
  for tok in "${must_have[@]}"; do
    if ! grep -q "would archive ${tok}" <<<"$output"; then
      echo "FAIL: $name — expected '${tok}' to be archived"; echo "  output: $output"
      FAILED=$((FAILED + 1)); return
    fi
  done
  for tok in "${must_not[@]}"; do
    if grep -q "would archive ${tok}" <<<"$output"; then
      echo "FAIL: $name — '${tok}' must be KEPT, not archived"; echo "  output: $output"
      FAILED=$((FAILED + 1)); return
    fi
  done
  echo "PASS: $name"; PASSED=$((PASSED + 1))
}

# ---------------------------------------------------------------------------
# Test 2: 5 items, keep 10 → no-op, exit 0.
# ---------------------------------------------------------------------------
test_keep_recent_noop_when_within_limit() {
  local name="test_keep_recent_noop_when_within_limit"
  local tmpdir; tmpdir="$(mktemp -d)"; trap 'rm -rf "$tmpdir"' RETURN
  make_mock_gh_with_n_items "$tmpdir" 5

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" GH_TOKEN="dummy" SKIP_PREFLIGHT=1 \
           bash "$TARGET_SCRIPT" --dry-run --keep-recent 10 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 0 ]; then
    echo "FAIL: $name — expected exit 0, got $exit_code"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  if ! grep -q "No surplus items to archive" <<<"$output"; then
    echo "FAIL: $name — expected 'No surplus items to archive'"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  echo "PASS: $name"; PASSED=$((PASSED + 1))
}

# ---------------------------------------------------------------------------
# Test 3: non-integer --keep-recent → exit 2 with structured error.
# ---------------------------------------------------------------------------
test_keep_recent_rejects_non_integer() {
  local name="test_keep_recent_rejects_non_integer"
  local tmpdir; tmpdir="$(mktemp -d)"; trap 'rm -rf "$tmpdir"' RETURN
  make_mock_gh_with_n_items "$tmpdir" 1

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" GH_TOKEN="dummy" SKIP_PREFLIGHT=1 \
           bash "$TARGET_SCRIPT" --dry-run --keep-recent abc 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 2 ]; then
    echo "FAIL: $name — expected exit 2, got $exit_code"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  if ! grep -q "ERROR: --keep-recent requires a non-negative integer" <<<"$output"; then
    echo "FAIL: $name — expected structured validation error"; echo "  output: $output"
    FAILED=$((FAILED + 1)); return
  fi
  echo "PASS: $name"; PASSED=$((PASSED + 1))
}

test_keep_recent_archives_all_but_n
test_keep_recent_noop_when_within_limit
test_keep_recent_rejects_non_integer

echo
echo "Results: ${PASSED} passed, ${FAILED} failed"
if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
exit 0
