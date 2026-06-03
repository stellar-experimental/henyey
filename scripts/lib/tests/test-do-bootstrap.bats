#!/usr/bin/env bash
#
# Tests for do_bootstrap in scripts/lib/agent-worktree-contract.sh (issue #2979/#2978).
#
# Despite the .bats extension, this file does NOT use BATS @test blocks: each
# test is a plain bash function named test_*, and the file ships its own
# TAP-style runner (see main() at the bottom). It is invoked with `bash`, not
# the BATS runner — running it under real `bats` would execute 0 @test blocks.
# This matches the convention used by .github/skills/shared/tests/bounce-cap-check.bats;
# BATS is not part of the project's required toolchain.
#
# What we assert:
#   1. With CLAUDE_SESSION_ID unset, do_bootstrap exports DO_WORKSPACE (the
#      canonical absolute workspace dir) and a write->read round-trip of the
#      persisted marker targets the IDENTICAL absolute path — i.e. /review-pr
#      reads exactly the dir /do created, with no date/$HOME recombination.
#   2. do_bootstrap rejects hostile pre-seeded WORKTREE_BASE / CARGO_TARGET_DIR
#      overrides (outside ~/data, ../ traversal, cross-session prefix), and on
#      rejection writes no marker.
#
# Usage:
#   bash scripts/lib/tests/test-do-bootstrap.bats     # the only supported runner
#
# Output: TAP. Exit: 0 if all tests pass, non-zero otherwise.

set -uo pipefail

# ── Locate the library under test ────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
LIB_UNDER_TEST="$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

# Passwd-derived real home — the trust anchor the contract uses. We compute it
# here independently so tests stage their fixtures under the SAME root the
# helper will validate against (immune to $HOME poisoning in the test env).
REAL_HOME="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
[[ -z "$REAL_HOME" ]] && REAL_HOME="$(eval echo "~$(id -un)")"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_CURRENT=0
TAP_FAILURES=0
TAP_OUTPUT=()

tap_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_OUTPUT+=("ok $TAP_CURRENT - $1")
}

tap_fail() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_FAILURES=$((TAP_FAILURES + 1))
  TAP_OUTPUT+=("not ok $TAP_CURRENT - $1")
  if [ -n "${2:-}" ]; then
    TAP_OUTPUT+=("  ---")
    TAP_OUTPUT+=("  $2")
    TAP_OUTPUT+=("  ...")
  fi
}

# Run a callback in a clean subshell with the library sourced and the relevant
# env vars cleared, so each test is independent.
# Usage: run_in_clean_env "<bash code>"; result in $LAST_STDOUT / $LAST_EXIT.
run_in_clean_env() {
  local code="$1"
  LAST_STDOUT="$(
    env -u CLAUDE_SESSION_ID -u SESSION_ID -u WORKTREE_BASE -u CARGO_TARGET_DIR -u DO_WORKSPACE \
      bash -c '
        set -uo pipefail
        source "'"$LIB_UNDER_TEST"'"
        '"$code"'
      ' 2>&1
  )"
  LAST_EXIT=$?
}

# ── Tests ────────────────────────────────────────────────────────────────────

# Test 1: write->read round-trip is identical with CLAUDE_SESSION_ID unset.
# do_bootstrap must export DO_WORKSPACE and the persisted marker must read back
# to the IDENTICAL absolute path with no date/$HOME recombination on read.
test_do_bootstrap_roundtrip_identical_unset_session() {
  local marker
  marker="$(mktemp)"
  run_in_clean_env '
    do_bootstrap 2979 || { echo "BOOTSTRAP_FAILED"; exit 1; }
    # /do persists the resolved absolute path (A.2).
    printf "%s" "$DO_WORKSPACE" > "'"$marker"'"
    # /review-pr reads it back verbatim (Step 7.4) — no recombination.
    read_back="$(cat "'"$marker"'")"
    if [ "$read_back" = "$DO_WORKSPACE" ] && [ -n "$DO_WORKSPACE" ]; then
      echo "ROUNDTRIP_OK:$DO_WORKSPACE"
    else
      echo "ROUNDTRIP_MISMATCH:write=$DO_WORKSPACE:read=$read_back"
    fi
  '
  rm -f "$marker"
  if [ "$LAST_EXIT" -eq 0 ] && echo "$LAST_STDOUT" | grep -q "ROUNDTRIP_OK:"; then
    tap_ok "do_bootstrap roundtrip identical with CLAUDE_SESSION_ID unset"
  else
    tap_fail "do_bootstrap roundtrip identical with CLAUDE_SESSION_ID unset" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# Test 2: DO_WORKSPACE lives under <real-home>/data and ends in do-<issue>.
test_do_bootstrap_workspace_under_home_data() {
  run_in_clean_env '
    do_bootstrap 2979 || { echo "BOOTSTRAP_FAILED"; exit 1; }
    echo "WS:$DO_WORKSPACE"
  '
  if [ "$LAST_EXIT" -eq 0 ] \
     && echo "$LAST_STDOUT" | grep -q "WS:$REAL_HOME/data/" \
     && echo "$LAST_STDOUT" | grep -q "/do-2979"; then
    tap_ok "do_bootstrap DO_WORKSPACE is under <real-home>/data/.../do-2979"
  else
    tap_fail "do_bootstrap DO_WORKSPACE is under <real-home>/data/.../do-2979" "exit=$LAST_EXIT stdout=$LAST_STDOUT real_home=$REAL_HOME"
  fi
}

# Test 3: also exports CARGO_TARGET_DIR under the workspace.
test_do_bootstrap_exports_cargo_target() {
  run_in_clean_env '
    do_bootstrap 2979 || { echo "BOOTSTRAP_FAILED"; exit 1; }
    echo "CT:$CARGO_TARGET_DIR"
  '
  if [ "$LAST_EXIT" -eq 0 ] && echo "$LAST_STDOUT" | grep -q "CT:$REAL_HOME/data/" \
     && echo "$LAST_STDOUT" | grep -q "/do-2979/cargo-target"; then
    tap_ok "do_bootstrap exports CARGO_TARGET_DIR under workspace"
  else
    tap_fail "do_bootstrap exports CARGO_TARGET_DIR under workspace" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# Test 4: hostile WORKTREE_BASE outside ~/data is rejected (return 1, no DO_WORKSPACE).
test_do_bootstrap_rejects_outside_home_data() {
  run_in_clean_env '
    type do_bootstrap >/dev/null 2>&1 || { echo "NO_FUNC"; exit 1; }
    WORKTREE_BASE="/tmp/evil-do-2979"
    if do_bootstrap 2979; then
      echo "ACCEPTED_BAD:$DO_WORKSPACE"
    else
      echo "REJECTED:DO_WORKSPACE=[${DO_WORKSPACE:-}]"
    fi
  '
  if echo "$LAST_STDOUT" | grep -q "REJECTED:DO_WORKSPACE=\[\]"; then
    tap_ok "do_bootstrap rejects WORKTREE_BASE outside ~/data and clears DO_WORKSPACE"
  else
    tap_fail "do_bootstrap rejects WORKTREE_BASE outside ~/data" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# Test 5: hostile ../ traversal escaping ~/data is rejected.
test_do_bootstrap_rejects_dotdot_traversal() {
  run_in_clean_env '
    type do_bootstrap >/dev/null 2>&1 || { echo "NO_FUNC"; exit 1; }
    WORKTREE_BASE="'"$REAL_HOME"'/data/../../etc/do-2979"
    if do_bootstrap 2979; then
      echo "ACCEPTED_BAD:$DO_WORKSPACE"
    else
      echo "REJECTED:DO_WORKSPACE=[${DO_WORKSPACE:-}]"
    fi
  '
  if echo "$LAST_STDOUT" | grep -q "REJECTED:DO_WORKSPACE=\[\]"; then
    tap_ok "do_bootstrap rejects ../ traversal escaping ~/data"
  else
    tap_fail "do_bootstrap rejects ../ traversal escaping ~/data" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# Test 6: cross-session prefix override (under ~/data but wrong session/issue
# dir) is rejected by the session-prefix guard.
test_do_bootstrap_rejects_cross_session_prefix() {
  run_in_clean_env '
    type do_bootstrap >/dev/null 2>&1 || { echo "NO_FUNC"; exit 1; }
    SESSION_ID="sess-a"
    # Override points under ~/data but at a DIFFERENT session/issue dir.
    WORKTREE_BASE="'"$REAL_HOME"'/data/sess-b/do-9999"
    if do_bootstrap 2979; then
      echo "ACCEPTED_BAD:$DO_WORKSPACE"
    else
      echo "REJECTED:DO_WORKSPACE=[${DO_WORKSPACE:-}]"
    fi
  '
  if echo "$LAST_STDOUT" | grep -q "REJECTED:DO_WORKSPACE=\[\]"; then
    tap_ok "do_bootstrap rejects cross-session prefix override"
  else
    tap_fail "do_bootstrap rejects cross-session prefix override" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# Test 7: hostile CARGO_TARGET_DIR outside ~/data is rejected.
test_do_bootstrap_rejects_bad_cargo_target() {
  run_in_clean_env '
    type do_bootstrap >/dev/null 2>&1 || { echo "NO_FUNC"; exit 1; }
    CARGO_TARGET_DIR="/tmp/evil-cargo"
    if do_bootstrap 2979; then
      echo "ACCEPTED_BAD:$CARGO_TARGET_DIR"
    else
      echo "REJECTED:DO_WORKSPACE=[${DO_WORKSPACE:-}]"
    fi
  '
  if echo "$LAST_STDOUT" | grep -q "REJECTED:DO_WORKSPACE=\[\]"; then
    tap_ok "do_bootstrap rejects CARGO_TARGET_DIR outside ~/data"
  else
    tap_fail "do_bootstrap rejects CARGO_TARGET_DIR outside ~/data" "exit=$LAST_EXIT stdout=$LAST_STDOUT"
  fi
}

# ── Runner ───────────────────────────────────────────────────────────────────
main() {
  test_do_bootstrap_roundtrip_identical_unset_session
  test_do_bootstrap_workspace_under_home_data
  test_do_bootstrap_exports_cargo_target
  test_do_bootstrap_rejects_outside_home_data
  test_do_bootstrap_rejects_dotdot_traversal
  test_do_bootstrap_rejects_cross_session_prefix
  test_do_bootstrap_rejects_bad_cargo_target

  echo "1..$TAP_CURRENT"
  for line in "${TAP_OUTPUT[@]}"; do
    echo "$line"
  done
  echo "# tests: $TAP_CURRENT  failures: $TAP_FAILURES"
  if [ "$TAP_FAILURES" -ne 0 ]; then
    exit 1
  fi
  exit 0
}

main "$@"
