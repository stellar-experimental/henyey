#!/usr/bin/env bash
#
# test-mission-3292-scripts.sh — CI smoke test for the SSC mission #3292 helper
# scripts. Exercises their command-assembly and assertion logic WITHOUT any live
# infra (`nsc`, k8s, curl), so the launch/assert paths are covered in CI.
#
# Covers:
#   - bash -n syntax of both helper scripts
#   - launch-mission-3292.sh --dry-run assembles the verified nsc commands and
#     the run-dir layout without invoking nsc
#   - assert-mission-3292.sh --self-check validates its JSON-extraction and
#     seq+hash agreement / mismatch logic against built-in fixtures
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCH="$SCRIPT_DIR/launch-mission-3292.sh"
ASSERT="$SCRIPT_DIR/assert-mission-3292.sh"

fail=0
check() {
  local desc="$1"; shift
  if "$@"; then
    echo "PASS: $desc"
  else
    echo "FAIL: $desc"; fail=1
  fi
}

echo "=== SSC mission #3292 script harness ==="

# 1. Syntax.
check "launch script bash -n" bash -n "$LAUNCH"
check "assert script bash -n" bash -n "$ASSERT"

# 2. Launch dry-run assembles commands without invoking nsc.
TMP_RUNS="$(mktemp -d)"
trap 'rm -rf "$TMP_RUNS"' EXIT
DRY_OUT="$(bash "$LAUNCH" --dry-run --runs-dir "$TMP_RUNS/run")"
echo "$DRY_OUT" | grep -q "MODE:       DRY RUN" \
  && echo "PASS: launch dry-run mode banner" || { echo "FAIL: dry-run banner"; fail=1; }
echo "$DRY_OUT" | grep -q "nsc build -f Dockerfile --platform linux/amd64 --push -n" \
  && echo "PASS: launch assembles build+push command" || { echo "FAIL: build command"; fail=1; }
echo "$DRY_OUT" | grep -q "nsc create --ephemeral" \
  && echo "PASS: launch assembles nsc create" || { echo "FAIL: nsc create"; fail=1; }
echo "$DRY_OUT" | grep -q "operator" \
  && echo "PASS: launch flags operator-owned mission RUN" || { echo "FAIL: operator handoff"; fail=1; }
# run-dir layout command assembled (dry-run prints but does not execute it)
echo "$DRY_OUT" | grep -q "mkdir -p $TMP_RUNS/run/logs $TMP_RUNS/run/ssc" \
  && echo "PASS: launch assembles run-dir layout" || { echo "FAIL: run-dir layout command"; fail=1; }
# dry-run must NOT touch live infra: nothing created on disk
[ ! -e "$TMP_RUNS/run" ] \
  && echo "PASS: dry-run created nothing on disk (no live nsc/fs writes)" || { echo "FAIL: dry-run touched the filesystem"; fail=1; }

# 3. Assert self-check.
check "assert self-check (offline logic)" bash "$ASSERT" --self-check

echo
if [ "$fail" -eq 0 ]; then
  echo "=== ALL SSC mission #3292 script checks PASSED ==="
  exit 0
else
  echo "=== SSC mission #3292 script checks FAILED ==="
  exit 1
fi
