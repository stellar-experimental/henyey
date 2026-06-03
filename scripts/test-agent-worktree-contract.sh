#!/usr/bin/env bash
# test-agent-worktree-contract.sh — TAP contract test for agent workspace placement.
#
# Verifies that /review-pr and /plan skill bootstraps enforce the ~/data workspace
# contract: all worktrees, cargo targets, and scratch dirs resolve under
# $HOME/data/$SESSION_ID/..., and hostile/traversal env overrides are rejected.
# Also verifies that .claude/skills/ copies remain synchronized with their
# .github/skills/ counterparts and that skill docs reference the shared helper.
#
# Usage: bash scripts/test-agent-worktree-contract.sh
# Exit: 0 if all tests pass, 1 otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

REVIEW_PR_SKILL="$REPO_ROOT/.github/skills/review-pr/SKILL.md"
PLAN_SKILL="$REPO_ROOT/.github/skills/plan/SKILL.md"
DO_SKILL="$REPO_ROOT/.github/skills/do/SKILL.md"
CONTRACT_HELPER="$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

# Observed disk-leak scratch patterns (issue #2843). These are out-of-~/data
# scratch dirs that prior /do, /review-pr, and /plan runs created. Every skill
# that spawns sub-agents must explicitly forbid these, and the detection guard
# (assert_no_repo_tree_scratch) must recognize them as leaks.
LEAK_PATTERNS=(
  ".review-data"
  ".review-worktrees"
  ".worktrees"
  ".copilot-tmp"
  ".opencode/worktrees"
)

PASS=0
FAIL=0
TEST_NUM=0

# Portable real-home lookup — reuses the contract helper's own fallback logic
# so the tests can run on macOS/BSD where getent is unavailable.
_test_real_home() {
  # shellcheck disable=SC1090
  ( source "$CONTRACT_HELPER" && _contract_real_home )
}

tap_ok() {
  TEST_NUM=$((TEST_NUM + 1))
  PASS=$((PASS + 1))
  echo "ok $TEST_NUM - $1"
}

tap_not_ok() {
  TEST_NUM=$((TEST_NUM + 1))
  FAIL=$((FAIL + 1))
  echo "not ok $TEST_NUM - $1"
  echo "#   $2"
}

# --------------------------------------------------------------------------
# Test: plan bootstrap rejects hostile WORKTREE_BASE
# --------------------------------------------------------------------------
test_plan_bootstrap_rejects_hostile_worktree_base() {
  local desc="plan bootstrap rejects hostile WORKTREE_BASE"

  # Test 1: Absolute path outside $HOME/data
  local output
  if output=$(WORKTREE_BASE="/tmp/evil" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (absolute outside)" "Should have failed but succeeded: $output"
    return
  fi

  # Test 2: Traversal that escapes $HOME/data
  if output=$(WORKTREE_BASE="$HOME/data/../escape" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (traversal)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: plan bootstrap rejects hostile CARGO_TARGET_DIR independently
# --------------------------------------------------------------------------
test_plan_bootstrap_rejects_hostile_cargo_target_dir() {
  local desc="plan bootstrap rejects hostile CARGO_TARGET_DIR"

  # Valid WORKTREE_BASE but hostile CARGO_TARGET_DIR
  local output
  if output=$(WORKTREE_BASE="$HOME/data/test-session/plan-999" \
    CARGO_TARGET_DIR="/tmp/evil-cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (absolute outside)" "Should have failed but succeeded: $output"
    return
  fi

  # Traversal CARGO_TARGET_DIR
  if output=$(WORKTREE_BASE="$HOME/data/test-session/plan-999" \
    CARGO_TARGET_DIR="$HOME/data/../escape/cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (traversal)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: plan bootstrap accepts safe pre-seeded $HOME/data paths
# --------------------------------------------------------------------------
test_plan_bootstrap_accepts_safe_preseeded_home_data_paths() {
  local desc="plan bootstrap accepts safe pre-seeded HOME/data paths"

  local output
  if ! output=$(WORKTREE_BASE="$HOME/data/my-session/plan-42" \
    CARGO_TARGET_DIR="$HOME/data/my-session/plan-42/cargo-target" \
    CLAUDE_SESSION_ID="my-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 42 critic-b && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    tap_not_ok "$desc" "Should have succeeded but failed: $output"
    return
  fi

  # Verify all paths are under $HOME/data (directory-boundary check, not prefix)
  local home_data
  home_data="$(source "$CONTRACT_HELPER" && canonicalize_contract_path "$HOME/data")"
  # Each output line must be exactly $home_data or start with $home_data/
  local line
  while IFS= read -r line; do
    if [[ -n "$line" && "$line" != "$home_data" && "$line" != "$home_data/"* ]]; then
      tap_not_ok "$desc" "Path not under \$HOME/data: $line"
      return
    fi
  done <<< "$output"

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap rejects hostile WORKTREE_BASE and CARGO_TARGET_DIR
# --------------------------------------------------------------------------
test_review_pr_bootstrap_rejects_hostile_worktree_base_and_cargo_target() {
  local desc="review-pr bootstrap rejects hostile WORKTREE_BASE and CARGO_TARGET_DIR"

  # Hostile WORKTREE_BASE
  local output
  if output=$(WORKTREE_BASE="/var/tmp/evil" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100" 2>&1); then
    tap_not_ok "$desc (hostile base)" "Should have failed but succeeded: $output"
    return
  fi

  # Hostile CARGO_TARGET_DIR with valid base
  if output=$(WORKTREE_BASE="$HOME/data/test-session/review-pr-100" \
    CARGO_TARGET_DIR="/opt/evil/cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100" 2>&1); then
    tap_not_ok "$desc (hostile cargo)" "Should have failed but succeeded: $output"
    return
  fi

  # Traversal
  if output=$(WORKTREE_BASE="$HOME/data/../../etc/evil" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100" 2>&1); then
    tap_not_ok "$desc (traversal)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap requires HOME/data workspace
# --------------------------------------------------------------------------
test_review_pr_bootstrap_requires_home_data_workspace() {
  local desc="review-pr bootstrap requires HOME/data workspace"

  local output
  if ! output=$(WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-sess" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 200 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    tap_not_ok "$desc" "Default bootstrap failed: $output"
    return
  fi

  local home_data
  home_data="$(source "$CONTRACT_HELPER" && canonicalize_contract_path "$HOME/data")"
  local line
  while IFS= read -r line; do
    if [[ -n "$line" && "$line" != "$home_data" && "$line" != "$home_data/"* ]]; then
      tap_not_ok "$desc" "Some default paths not under \$HOME/data: $line"
      return
    fi
  done <<< "$output"

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: default bootstrap layouts stay under $HOME/data
# --------------------------------------------------------------------------
test_default_bootstrap_layouts_stay_under_home_data() {
  local desc="default bootstrap layouts stay under HOME/data"

  # Plan critic with no pre-seeded vars
  local plan_out
  if ! plan_out=$(WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="default-test" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 55 critic-c && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    tap_not_ok "$desc" "Plan default failed: $plan_out"
    return
  fi

  # Review-pr with no pre-seeded vars
  local review_out
  if ! review_out=$(WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="default-test" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 55 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    tap_not_ok "$desc" "Review default failed: $review_out"
    return
  fi

  local home_data
  home_data="$(source "$CONTRACT_HELPER" && canonicalize_contract_path "$HOME/data")"

  # Verify plan paths (directory-boundary check)
  local line
  while IFS= read -r line; do
    if [[ -n "$line" && "$line" != "$home_data" && "$line" != "$home_data/"* ]]; then
      tap_not_ok "$desc" "Plan paths escape \$HOME/data: $line"
      return
    fi
  done <<< "$plan_out"

  # Verify review paths (directory-boundary check)
  while IFS= read -r line; do
    if [[ -n "$line" && "$line" != "$home_data" && "$line" != "$home_data/"* ]]; then
      tap_not_ok "$desc" "Review paths escape \$HOME/data: $line"
      return
    fi
  done <<< "$review_out"

  # Verify expected structure
  if ! echo "$plan_out" | grep -q "data/default-test/plan-55"; then
    tap_not_ok "$desc" "Plan layout missing expected session/plan structure: $plan_out"
    return
  fi
  if ! echo "$review_out" | grep -q "data/default-test/review-pr-55"; then
    tap_not_ok "$desc" "Review layout missing expected session/review-pr structure: $review_out"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: skill files reference shared contract helper
# --------------------------------------------------------------------------
test_skill_files_reference_shared_contract_helper() {
  local desc="skill files reference shared contract helper"

  local plan_refs=false
  local review_refs=false

  if grep -q 'agent-worktree-contract.sh\|plan_critic_bootstrap' "$PLAN_SKILL"; then
    plan_refs=true
  fi

  if grep -q 'agent-worktree-contract.sh\|review_pr_bootstrap' "$REVIEW_PR_SKILL"; then
    review_refs=true
  fi

  if $plan_refs && $review_refs; then
    tap_ok "$desc"
  else
    local missing=""
    $plan_refs || missing="plan"
    $review_refs || missing="${missing:+$missing, }review-pr"
    tap_not_ok "$desc" "Missing helper reference in: $missing"
  fi
}

# --------------------------------------------------------------------------
# Test: .claude/skills/review-pr is synchronized with .github/skills/review-pr
# --------------------------------------------------------------------------
test_claude_review_pr_synced() {
  local desc=".claude/skills/review-pr is synchronized with .github/skills/review-pr"
  local claude_path="$REPO_ROOT/.claude/skills/review-pr"
  local github_path="$REPO_ROOT/.github/skills/review-pr"

  if [ -L "$claude_path" ]; then
    local target resolved expected
    target="$(readlink "$claude_path")"
    if resolved="$(cd "$(dirname "$claude_path")" && cd "$target" 2>/dev/null && pwd)"; then
      if expected="$(cd "$github_path" 2>/dev/null && pwd)"; then
        if [ "$resolved" = "$expected" ]; then
          tap_ok "$desc (symlink)"
        else
          tap_not_ok "$desc" "Symlink points to $resolved, expected $expected"
        fi
      else
        tap_not_ok "$desc" "Expected path '$github_path' does not exist"
      fi
    else
      tap_not_ok "$desc" "Symlink target '$target' does not resolve"
    fi
  elif [ -d "$claude_path" ]; then
    if diff -r "$claude_path" "$github_path" > /dev/null 2>&1; then
      tap_ok "$desc (identical copy)"
    else
      tap_not_ok "$desc" ".claude/skills/review-pr differs from .github/skills/review-pr"
    fi
  else
    tap_not_ok "$desc" ".claude/skills/review-pr does not exist"
  fi
}

# --------------------------------------------------------------------------
# Test: .claude/skills/plan is synchronized with .github/skills/plan
# --------------------------------------------------------------------------
test_claude_plan_synced() {
  local desc=".claude/skills/plan is synchronized with .github/skills/plan"
  local claude_path="$REPO_ROOT/.claude/skills/plan"
  local github_path="$REPO_ROOT/.github/skills/plan"

  if [ -L "$claude_path" ]; then
    local target resolved expected
    target="$(readlink "$claude_path")"
    if resolved="$(cd "$(dirname "$claude_path")" && cd "$target" 2>/dev/null && pwd)"; then
      if expected="$(cd "$github_path" 2>/dev/null && pwd)"; then
        if [ "$resolved" = "$expected" ]; then
          tap_ok "$desc (symlink)"
        else
          tap_not_ok "$desc" "Symlink points to $resolved, expected $expected"
        fi
      else
        tap_not_ok "$desc" "Expected path '$github_path' does not exist"
      fi
    else
      tap_not_ok "$desc" "Symlink target '$target' does not resolve"
    fi
  elif [ -d "$claude_path" ]; then
    if diff -r "$claude_path" "$github_path" > /dev/null 2>&1; then
      tap_ok "$desc (identical copy)"
    else
      tap_not_ok "$desc" ".claude/skills/plan differs from .github/skills/plan"
    fi
  else
    tap_not_ok "$desc" ".claude/skills/plan does not exist"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap rejects sibling-prefix WORKTREE_BASE (e.g. $HOME/data-evil)
# --------------------------------------------------------------------------
test_plan_bootstrap_rejects_sibling_prefix_worktree_base() {
  local desc="plan bootstrap rejects sibling-prefix WORKTREE_BASE"

  # Sibling-prefix: $HOME/data-evil shares the $HOME/data string prefix
  local output
  if output=$(WORKTREE_BASE="$HOME/data-evil/plan-999" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (data-evil)" "Should have failed but succeeded: $output"
    return
  fi

  # Sibling-prefix: $HOME/data2
  if output=$(WORKTREE_BASE="$HOME/data2/plan-999" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 999 critic-a" 2>&1); then
    tap_not_ok "$desc (data2)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap rejects sibling-prefix paths
# --------------------------------------------------------------------------
test_review_pr_bootstrap_rejects_sibling_prefix() {
  local desc="review-pr bootstrap rejects sibling-prefix paths"

  # Sibling-prefix WORKTREE_BASE
  local output
  if output=$(WORKTREE_BASE="$HOME/data-evil/review-100" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100" 2>&1); then
    tap_not_ok "$desc (base: data-evil)" "Should have failed but succeeded: $output"
    return
  fi

  # Valid base but sibling-prefix CARGO_TARGET_DIR
  if output=$(WORKTREE_BASE="$HOME/data/test-session/review-pr-100" \
    CARGO_TARGET_DIR="$HOME/data-evil/cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100" 2>&1); then
    tap_not_ok "$desc (cargo: data-evil)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: sourcing the helper does not mutate caller shell options
# --------------------------------------------------------------------------
test_sourcing_preserves_caller_shell_options() {
  local desc="sourcing helper preserves caller shell options"

  # Run in a subshell where nounset and pipefail are explicitly OFF,
  # source the helper, then verify they remain OFF.
  local output
  output=$(bash -c '
    # Ensure nounset and pipefail are OFF
    set +u
    set +o pipefail

    # Capture initial state
    before_u=$(set +o | grep nounset)
    before_p=$(set +o | grep pipefail)

    source "'"$CONTRACT_HELPER"'"

    # Capture state after sourcing
    after_u=$(set +o | grep nounset)
    after_p=$(set +o | grep pipefail)

    if [[ "$before_u" != "$after_u" ]]; then
      echo "FAIL: nounset changed from [$before_u] to [$after_u]"
      exit 1
    fi
    if [[ "$before_p" != "$after_p" ]]; then
      echo "FAIL: pipefail changed from [$before_p] to [$after_p]"
      exit 1
    fi

    # Also verify the helper still works (hostile path rejected)
    if require_home_data_path "/tmp/evil" "TEST" 2>/dev/null; then
      echo "FAIL: hostile path was not rejected after sourcing"
      exit 1
    fi

    echo "OK"
  ' 2>&1)

  if [[ "$output" == "OK" ]]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "$output"
  fi
}

# --------------------------------------------------------------------------
# Test: stale CRITIC_WORKTREE is cleared after plan bootstrap rejection
# --------------------------------------------------------------------------
test_plan_bootstrap_clears_stale_vars_on_failure() {
  local desc="plan bootstrap clears stale vars on failure (stale-env escape)"

  # Pre-seed CRITIC_WORKTREE with a stale path outside $HOME/data, then call
  # plan_critic_bootstrap with a hostile WORKTREE_BASE. After the bootstrap
  # fails, CRITIC_WORKTREE must be empty — not the stale value.
  local output
  output=$(CRITIC_WORKTREE="/tmp/stale-critic" \
    WORKTREE_BASE="/tmp/evil-base" \
    CARGO_TARGET_DIR="" \
    CLAUDE_SESSION_ID="test-session" \
    bash -c '
      source "'"$CONTRACT_HELPER"'"
      plan_critic_bootstrap 999 critic-a
      rc=$?
      echo "rc=$rc"
      echo "CRITIC_WORKTREE=${CRITIC_WORKTREE:-EMPTY}"
      echo "WORKTREE_BASE=${WORKTREE_BASE:-EMPTY}"
      echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-EMPTY}"
    ' 2>&1)

  # Bootstrap must fail
  if ! echo "$output" | grep -q "rc=1"; then
    tap_not_ok "$desc" "Expected rc=1, got: $output"
    return
  fi

  # All derived vars must be cleared (not stale)
  if echo "$output" | grep -q "CRITIC_WORKTREE=/tmp/stale-critic"; then
    tap_not_ok "$desc" "CRITIC_WORKTREE still has stale value after failure: $output"
    return
  fi
  if ! echo "$output" | grep -q "CRITIC_WORKTREE=EMPTY"; then
    tap_not_ok "$desc" "CRITIC_WORKTREE not cleared: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: stale REVIEWER_WORKTREE is cleared after review-pr bootstrap rejection
# --------------------------------------------------------------------------
test_review_pr_bootstrap_clears_stale_vars_on_failure() {
  local desc="review-pr bootstrap clears stale vars on failure (stale-env escape)"

  # Pre-seed REVIEWER_WORKTREE with a stale path, give hostile WORKTREE_BASE.
  local output
  output=$(REVIEWER_WORKTREE="/tmp/stale-reviewer" \
    WORKTREE_BASE="/tmp/evil-base" \
    CARGO_TARGET_DIR="" \
    CLAUDE_SESSION_ID="test-session" \
    bash -c '
      source "'"$CONTRACT_HELPER"'"
      review_pr_bootstrap 100
      rc=$?
      echo "rc=$rc"
      echo "REVIEWER_WORKTREE=${REVIEWER_WORKTREE:-EMPTY}"
      echo "WORKTREE_BASE=${WORKTREE_BASE:-EMPTY}"
      echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-EMPTY}"
    ' 2>&1)

  # Bootstrap must fail
  if ! echo "$output" | grep -q "rc=1"; then
    tap_not_ok "$desc" "Expected rc=1, got: $output"
    return
  fi

  # All derived vars must be cleared
  if echo "$output" | grep -q "REVIEWER_WORKTREE=/tmp/stale-reviewer"; then
    tap_not_ok "$desc" "REVIEWER_WORKTREE still has stale value after failure: $output"
    return
  fi
  if ! echo "$output" | grep -q "REVIEWER_WORKTREE=EMPTY"; then
    tap_not_ok "$desc" "REVIEWER_WORKTREE not cleared: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: documented bootstrap snippet shape fails closed on hostile override
# --------------------------------------------------------------------------
test_documented_snippet_fails_closed() {
  local desc="documented bootstrap snippet fails closed (no mkdir on hostile env)"

  # Simulate the exact documented snippet from plan/SKILL.md and review-pr/SKILL.md
  # with hostile pre-seeded vars. The || exit 1 guard must prevent mkdir from running.
  local plan_output
  plan_output=$(WORKTREE_BASE="/tmp/evil" \
    CRITIC_WORKTREE="/tmp/stale-critic" \
    CARGO_TARGET_DIR="" \
    CLAUDE_SESSION_ID="test-session" \
    bash -c '
      REPO_ROOT="'"$(cd "$REPO_ROOT" && pwd)"'"
      source "$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"
      ISSUE=999
      plan_critic_bootstrap "$ISSUE" "critic-a" || exit 1
      mkdir -p "$CRITIC_WORKTREE"
      echo "REACHED_MKDIR"
    ' 2>&1) || true

  if echo "$plan_output" | grep -q "REACHED_MKDIR"; then
    tap_not_ok "$desc (plan)" "mkdir was reached despite hostile WORKTREE_BASE"
    return
  fi

  local review_output
  review_output=$(WORKTREE_BASE="/tmp/evil" \
    REVIEWER_WORKTREE="/tmp/stale-reviewer" \
    CARGO_TARGET_DIR="" \
    CLAUDE_SESSION_ID="test-session" \
    bash -c '
      REPO_ROOT="'"$(cd "$REPO_ROOT" && pwd)"'"
      source "$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"
      ISSUE=100
      review_pr_bootstrap "$ISSUE" || exit 1
      mkdir -p "$REVIEWER_WORKTREE"
      echo "REACHED_MKDIR"
    ' 2>&1) || true

  if echo "$review_output" | grep -q "REACHED_MKDIR"; then
    tap_not_ok "$desc (review-pr)" "mkdir was reached despite hostile WORKTREE_BASE"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: HOME poisoning is defeated (passwd-derived trust anchor)
# --------------------------------------------------------------------------
test_home_poisoning_defeated() {
  local desc="HOME poisoning is defeated by passwd-derived trust anchor"

  # Get the real home from passwd for reference
  local real_home
  real_home="$(_test_real_home)"

  # Poison HOME to a fake directory. The contract should still validate
  # against the real home (from passwd), not the poisoned $HOME.
  local output

  # Case 1: Path under fake HOME/data should be rejected
  if output=$(HOME="/tmp/fakehome" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 200 && echo \$WORKTREE_BASE" 2>&1); then
    # If it succeeded, the derived path should be under the REAL home, not fake
    if echo "$output" | grep -q "/tmp/fakehome"; then
      tap_not_ok "$desc" "Used poisoned HOME: $output"
      return
    fi
    # It used real home — that's correct behavior
    if echo "$output" | grep -q "$real_home/data"; then
      tap_ok "$desc"
      return
    fi
    tap_not_ok "$desc" "Unexpected path: $output"
    return
  fi

  # Bootstrap failed — check if it correctly rejected the poisoned-HOME path
  if echo "$output" | grep -q "outside"; then
    # Correctly rejected because /tmp/fakehome/data is not under real home
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Unexpected failure: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: HOME poisoning with explicit override to fake home's data dir
# --------------------------------------------------------------------------
test_home_poisoning_explicit_override() {
  local desc="HOME poisoning with explicit override rejected"

  local real_home
  real_home="$(_test_real_home)"

  # Explicitly set WORKTREE_BASE to a path under the fake HOME/data
  local output
  if output=$(HOME="/tmp/fakehome" \
    WORKTREE_BASE="/tmp/fakehome/data/test-session/review-pr-200" \
    CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 200 && echo \$WORKTREE_BASE" 2>&1); then
    tap_not_ok "$desc" "Should have failed but succeeded: $output"
    return
  fi

  # Should fail because /tmp/fakehome/data is not under real_home/data
  if echo "$output" | grep -q "outside"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Wrong failure reason: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: shared in-bounds directory rejected (session-prefix enforcement)
# --------------------------------------------------------------------------
test_shared_inbounds_directory_rejected() {
  local desc="shared in-bounds directory rejected by session-prefix check"

  local real_home
  real_home="$(_test_real_home)"

  # Case 1: WORKTREE_BASE under $HOME/data but wrong session/issue prefix
  local output
  if output=$(WORKTREE_BASE="$real_home/data/shared/review-pr-100" \
    CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100 && echo \$WORKTREE_BASE" 2>&1); then
    tap_not_ok "$desc (review shared base)" "Should have failed but succeeded: $output"
    return
  fi

  # Case 2: CARGO_TARGET_DIR under ~/data but wrong prefix
  if output=$(WORKTREE_BASE="" \
    CARGO_TARGET_DIR="$real_home/data/shared/cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100 && echo \$CARGO_TARGET_DIR" 2>&1); then
    tap_not_ok "$desc (review shared cargo)" "Should have failed but succeeded: $output"
    return
  fi

  # Case 3: plan_critic_bootstrap with cross-session WORKTREE_BASE
  if output=$(WORKTREE_BASE="$real_home/data/other-session/plan-42" \
    CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 42 critic-a && echo \$WORKTREE_BASE" 2>&1); then
    tap_not_ok "$desc (plan cross-session)" "Should have failed but succeeded: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: correct session-prefix overrides still accepted
# --------------------------------------------------------------------------
test_correct_session_prefix_overrides_accepted() {
  local desc="correct session-prefix overrides accepted"

  local real_home
  real_home="$(_test_real_home)"

  # Exact prefix match — should succeed
  local output
  if ! output=$(WORKTREE_BASE="$real_home/data/my-session/plan-42" \
    CARGO_TARGET_DIR="$real_home/data/my-session/plan-42/cargo-target" \
    CLAUDE_SESSION_ID="my-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 42 critic-b && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    tap_not_ok "$desc (plan)" "Should have succeeded but failed: $output"
    return
  fi

  # Review-pr with correct prefix
  if ! output=$(WORKTREE_BASE="$real_home/data/my-session/review-pr-100" \
    CARGO_TARGET_DIR="$real_home/data/my-session/review-pr-100/cargo-target" \
    CLAUDE_SESSION_ID="my-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 100 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    tap_not_ok "$desc (review)" "Should have succeeded but failed: $output"
    return
  fi

  # Subdirectory of expected prefix — should also succeed
  if ! output=$(WORKTREE_BASE="$real_home/data/my-session/plan-42/subdir" \
    CARGO_TARGET_DIR="$real_home/data/my-session/plan-42/subdir/cargo" \
    CLAUDE_SESSION_ID="my-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 42 critic-c && echo \$WORKTREE_BASE" 2>&1); then
    tap_not_ok "$desc (subdirectory)" "Should have succeeded but failed: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: bootstraps fall back when realpath is missing from PATH
# --------------------------------------------------------------------------
test_bootstraps_fallback_when_realpath_is_missing() {
  local desc="bootstraps fall back when realpath is missing from PATH"

  local real_home
  real_home="$(_test_real_home)"

  # Create a stub directory with a realpath that always fails (simulates missing)
  local stub_dir
  stub_dir="$(mktemp -d)"
  cat > "$stub_dir/realpath" << 'STUB'
#!/usr/bin/env bash
# Stub: simulate realpath not being available
exit 127
STUB
  chmod +x "$stub_dir/realpath"

  # Put stub first in PATH so it shadows the real realpath
  local output
  if ! output=$(PATH="$stub_dir:$PATH" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="fallback-test" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 77 critic-a && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    rm -rf "$stub_dir"
    tap_not_ok "$desc (plan)" "Plan bootstrap failed when realpath missing: $output"
    return
  fi

  # Verify each exported variable is individually non-empty and under real home
  local worktree_base cargo_target_dir derived_worktree
  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "CRITIC_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$stub_dir"
      tap_not_ok "$desc" "Path not under real home/data: $p"
      return
    fi
  done

  # Also test review_pr_bootstrap
  if ! output=$(PATH="$stub_dir:$PATH" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="fallback-test" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 77 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    rm -rf "$stub_dir"
    tap_not_ok "$desc (review)" "Review bootstrap failed when realpath missing: $output"
    return
  fi

  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review REVIEWER_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$stub_dir"
      tap_not_ok "$desc" "Review path not under real home/data: $p"
      return
    fi
  done

  rm -rf "$stub_dir"
  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: bootstraps fall back when realpath rejects -m flag
# --------------------------------------------------------------------------
test_bootstraps_fallback_when_realpath_rejects_dash_m() {
  local desc="bootstraps fall back when realpath rejects -m flag"

  local real_home
  real_home="$(_test_real_home)"

  # Create a fake realpath that rejects -m but otherwise exists
  local stub_dir
  stub_dir="$(mktemp -d)"
  cat > "$stub_dir/realpath" << 'STUB'
#!/usr/bin/env bash
# Stub realpath that rejects -m (simulates BSD realpath)
for arg in "$@"; do
  if [[ "$arg" == "-m" || "$arg" == "--canonicalize-missing" ]]; then
    echo "realpath: invalid option -- 'm'" >&2
    exit 1
  fi
done
# Without -m, just pass through (but won't resolve missing paths)
/usr/bin/realpath "$@" 2>/dev/null || exit 1
STUB
  chmod +x "$stub_dir/realpath"

  # Put stub first in PATH
  local output
  if ! output=$(PATH="$stub_dir:$PATH" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="fallback-test" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 78 critic-a && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    rm -rf "$stub_dir"
    tap_not_ok "$desc (plan)" "Plan bootstrap failed when realpath rejects -m: $output"
    return
  fi

  # Verify each exported variable is individually non-empty and under real home
  local worktree_base cargo_target_dir derived_worktree
  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "CRITIC_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$stub_dir"
      tap_not_ok "$desc" "Path not under real home/data: $p"
      return
    fi
  done

  # Also test review_pr_bootstrap
  if ! output=$(PATH="$stub_dir:$PATH" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="fallback-test" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 78 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    rm -rf "$stub_dir"
    tap_not_ok "$desc (review)" "Review bootstrap failed when realpath rejects -m: $output"
    return
  fi

  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$stub_dir"
    tap_not_ok "$desc" "Review REVIEWER_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$stub_dir"
      tap_not_ok "$desc" "Review path not under real home/data: $p"
      return
    fi
  done

  rm -rf "$stub_dir"
  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: symlinked HOME alias overrides are accepted
# --------------------------------------------------------------------------
test_symlinked_home_alias_overrides_are_accepted() {
  local desc="symlinked HOME alias overrides are accepted"

  local real_home
  real_home="$(_test_real_home)"

  # Create a symlink alias that points at the real home directory
  local link_dir
  link_dir="$(mktemp -d)"
  local link_home="$link_dir/link-home"
  ln -s "$real_home" "$link_home"

  # Set HOME to the symlink alias and pass overrides through it
  local output
  if ! output=$(HOME="$link_home" \
    WORKTREE_BASE="$link_home/data/sym-session/plan-99" \
    CARGO_TARGET_DIR="$link_home/data/sym-session/plan-99/cargo-target" \
    CLAUDE_SESSION_ID="sym-session" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 99 critic-a && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    rm -rf "$link_dir"
    tap_not_ok "$desc (plan)" "Plan bootstrap failed with symlinked HOME: $output"
    return
  fi

  # Verify each exported variable is individually non-empty and under real home/data
  local worktree_base cargo_target_dir derived_worktree
  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "CRITIC_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$link_dir"
      tap_not_ok "$desc" "Path not normalized to real home/data: $p (expected under $real_home/data/)"
      return
    fi
  done

  # Also test review_pr_bootstrap with symlinked HOME
  if ! output=$(HOME="$link_home" \
    WORKTREE_BASE="$link_home/data/sym-session/review-pr-99" \
    CARGO_TARGET_DIR="$link_home/data/sym-session/review-pr-99/cargo-target" \
    CLAUDE_SESSION_ID="sym-session" \
    bash -c "source '$CONTRACT_HELPER' && review_pr_bootstrap 99 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$REVIEWER_WORKTREE" 2>&1); then
    rm -rf "$link_dir"
    tap_not_ok "$desc (review)" "Review bootstrap failed with symlinked HOME: $output"
    return
  fi

  worktree_base=$(sed -n '1p' <<< "$output")
  cargo_target_dir=$(sed -n '2p' <<< "$output")
  derived_worktree=$(sed -n '3p' <<< "$output")

  if [[ -z "$worktree_base" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "Review WORKTREE_BASE is empty (fail-open)"
    return
  fi
  if [[ -z "$cargo_target_dir" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "Review CARGO_TARGET_DIR is empty (fail-open)"
    return
  fi
  if [[ -z "$derived_worktree" ]]; then
    rm -rf "$link_dir"
    tap_not_ok "$desc" "Review REVIEWER_WORKTREE is empty (fail-open)"
    return
  fi
  for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
    if [[ "$p" != "$real_home/data/"* ]]; then
      rm -rf "$link_dir"
      tap_not_ok "$desc" "Review path not normalized to real home/data: $p"
      return
    fi
  done

  rm -rf "$link_dir"
  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: poisoned python3 on PATH does not bypass canonicalization
# --------------------------------------------------------------------------
test_poisoned_python_on_path_ignored() {
  local desc="poisoned python3 on PATH is ignored (uses absolute path only)"

  local real_home
  real_home="$(_test_real_home)"

  # Create a stub directory with:
  # - realpath that always fails (forces Python fallback)
  # - python3 that prints attacker-controlled output (/etc)
  # - python that also prints attacker-controlled output
  local stub_dir
  stub_dir="$(mktemp -d)"
  cat > "$stub_dir/realpath" << 'STUB'
#!/usr/bin/env bash
exit 127
STUB
  chmod +x "$stub_dir/realpath"

  cat > "$stub_dir/python3" << 'STUB'
#!/usr/bin/env bash
# Malicious: always outputs /etc regardless of input
echo "/etc"
STUB
  chmod +x "$stub_dir/python3"

  cat > "$stub_dir/python" << 'STUB'
#!/usr/bin/env bash
echo "/etc"
STUB
  chmod +x "$stub_dir/python"

  # Run bootstrap with poisoned PATH — the helper should use /usr/bin/python3
  # (absolute path) and ignore the stub. If /usr/bin/python3 doesn't exist,
  # it should fail closed rather than using the poisoned PATH python3.
  local output
  if output=$(PATH="$stub_dir:$PATH" \
    WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="poison-test" \
    bash -c "source '$CONTRACT_HELPER' && plan_critic_bootstrap 77 critic-a && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$CRITIC_WORKTREE" 2>&1); then
    # Bootstrap succeeded — verify paths are NOT /etc (attacker-controlled)
    local worktree_base cargo_target_dir derived_worktree
    worktree_base=$(sed -n '1p' <<< "$output")
    cargo_target_dir=$(sed -n '2p' <<< "$output")
    derived_worktree=$(sed -n '3p' <<< "$output")

    for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
      if [[ "$p" == "/etc"* ]]; then
        rm -rf "$stub_dir"
        tap_not_ok "$desc" "PATH-poisoned python3 was trusted! Got path: $p"
        return
      fi
    done

    # If it succeeded and paths are correct, the absolute-path fallback worked
    for p in "$worktree_base" "$cargo_target_dir" "$derived_worktree"; do
      if [[ -z "$p" || "$p" != "$real_home/data/"* ]]; then
        rm -rf "$stub_dir"
        tap_not_ok "$desc" "Unexpected path: '$p' (expected under $real_home/data/)"
        return
      fi
    done
  else
    # Bootstrap failed — that's acceptable (fail-closed) as long as it didn't
    # succeed with attacker output. Verify the error output doesn't contain /etc
    # as an accepted path.
    if [[ "$output" == *"resolves to '/etc'"* ]] && [[ "$output" != *"outside"* ]]; then
      rm -rf "$stub_dir"
      tap_not_ok "$desc" "Bootstrap accepted poisoned /etc path before failing"
      return
    fi
    # Fail-closed is the correct behavior when realpath and absolute-path python
    # are both unavailable — this is NOT a bypass.
  fi

  rm -rf "$stub_dir"
  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: do_bootstrap rejects hostile WORKTREE_BASE/CARGO_TARGET_DIR, sibling
# prefixes, and cross-session bases; clears stale DO_WORKTREE on failure.
# --------------------------------------------------------------------------
test_do_bootstrap_rejects_hostile_and_sibling_paths() {
  local desc="do_bootstrap rejects hostile and sibling paths"

  local real_home
  real_home="$(_test_real_home)"

  # Hostile absolute WORKTREE_BASE outside ~/data
  local output
  if output=$(WORKTREE_BASE="/tmp/evil" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 999" 2>&1); then
    tap_not_ok "$desc (hostile base)" "Should have failed but succeeded: $output"
    return
  fi

  # Traversal escaping ~/data
  if output=$(WORKTREE_BASE="$HOME/data/../escape" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 999" 2>&1); then
    tap_not_ok "$desc (traversal)" "Should have failed but succeeded: $output"
    return
  fi

  # Sibling-prefix ~/data-evil
  if output=$(WORKTREE_BASE="$HOME/data-evil/do-999" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 999" 2>&1); then
    tap_not_ok "$desc (sibling prefix)" "Should have failed but succeeded: $output"
    return
  fi

  # Hostile CARGO_TARGET_DIR with valid base
  if output=$(WORKTREE_BASE="$real_home/data/test-session/do-999" \
    CARGO_TARGET_DIR="/tmp/evil-cargo" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 999" 2>&1); then
    tap_not_ok "$desc (hostile cargo)" "Should have failed but succeeded: $output"
    return
  fi

  # Cross-session base under ~/data but wrong session prefix
  if output=$(WORKTREE_BASE="$real_home/data/other-session/do-999" \
    CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="test-session" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 999" 2>&1); then
    tap_not_ok "$desc (cross-session)" "Should have failed but succeeded: $output"
    return
  fi

  # Stale-env escape: stale DO_WORKTREE must be cleared after a rejection
  output=$(DO_WORKTREE="/tmp/stale-do" \
    WORKTREE_BASE="/tmp/evil-base" \
    CARGO_TARGET_DIR="" \
    CLAUDE_SESSION_ID="test-session" \
    bash -c '
      source "'"$CONTRACT_HELPER"'"
      do_bootstrap 999
      rc=$?
      echo "rc=$rc"
      echo "DO_WORKTREE=${DO_WORKTREE:-EMPTY}"
    ' 2>&1)
  if ! echo "$output" | grep -q "rc=1"; then
    tap_not_ok "$desc (stale-clear rc)" "Expected rc=1, got: $output"
    return
  fi
  if echo "$output" | grep -q "DO_WORKTREE=/tmp/stale-do"; then
    tap_not_ok "$desc (stale-clear)" "DO_WORKTREE retained stale value: $output"
    return
  fi
  if ! echo "$output" | grep -q "DO_WORKTREE=EMPTY"; then
    tap_not_ok "$desc (stale-clear empty)" "DO_WORKTREE not cleared: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: do_bootstrap default layout resolves under ~/data/<session>/do-<issue>
# --------------------------------------------------------------------------
test_do_bootstrap_default_under_home_data() {
  local desc="do_bootstrap default layout under HOME/data/<session>/do-<issue>"

  local output
  if ! output=$(WORKTREE_BASE="" CARGO_TARGET_DIR="" CLAUDE_SESSION_ID="default-test" \
    bash -c "source '$CONTRACT_HELPER' && do_bootstrap 55 && echo \$WORKTREE_BASE && echo \$CARGO_TARGET_DIR && echo \$DO_WORKTREE" 2>&1); then
    tap_not_ok "$desc" "Default do_bootstrap failed: $output"
    return
  fi

  local home_data
  home_data="$(source "$CONTRACT_HELPER" && canonicalize_contract_path "$HOME/data")"
  local line
  while IFS= read -r line; do
    if [[ -n "$line" && "$line" != "$home_data" && "$line" != "$home_data/"* ]]; then
      tap_not_ok "$desc" "Path escapes \$HOME/data: $line"
      return
    fi
  done <<< "$output"

  if ! echo "$output" | grep -q "data/default-test/do-55"; then
    tap_not_ok "$desc" "Layout missing expected session/do structure: $output"
    return
  fi
  # DO_WORKTREE must be the worktree subdir, not the bare base.
  if ! echo "$output" | grep -q "data/default-test/do-55/worktree"; then
    tap_not_ok "$desc" "DO_WORKTREE missing worktree subdir: $output"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: /do uses the shared contract helper, not an in-repo worktree
# --------------------------------------------------------------------------
test_do_skill_uses_contract_helper_not_repo_tree() {
  local desc="do/SKILL.md uses do_bootstrap and no in-repo worktree"

  if [[ ! -f "$DO_SKILL" ]]; then
    tap_not_ok "$desc" "do/SKILL.md not found at $DO_SKILL"
    return
  fi

  # Must reference the shared helper bootstrap.
  if ! grep -q 'do_bootstrap' "$DO_SKILL"; then
    tap_not_ok "$desc" "do/SKILL.md does not reference do_bootstrap"
    return
  fi
  if ! grep -q 'agent-worktree-contract.sh' "$DO_SKILL"; then
    tap_not_ok "$desc" "do/SKILL.md does not source the contract helper"
    return
  fi

  # Must NOT create a worktree inside the repo tree ($REPO_ROOT/data/do-...).
  if grep -Eq 'REPO_ROOT/data/do-' "$DO_SKILL"; then
    tap_not_ok "$desc" "do/SKILL.md still references in-repo \$REPO_ROOT/data/do- worktree"
    return
  fi

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: every spawning skill enumerates the observed forbidden leak patterns
# --------------------------------------------------------------------------
test_skill_prompts_forbid_known_leak_patterns() {
  local desc="skill prompts forbid all observed leak patterns"

  local skill_file pat missing
  for skill_file in "$DO_SKILL" "$PLAN_SKILL" "$REVIEW_PR_SKILL"; do
    if [[ ! -f "$skill_file" ]]; then
      tap_not_ok "$desc" "Skill file not found: $skill_file"
      return
    fi
    for pat in "${LEAK_PATTERNS[@]}"; do
      if ! grep -qF "$pat" "$skill_file"; then
        tap_not_ok "$desc" "$skill_file does not forbid leak pattern '$pat'"
        return
      fi
    done
    # Also require an explicit /tmp prohibition.
    if ! grep -qF '/tmp' "$skill_file"; then
      tap_not_ok "$desc" "$skill_file does not mention forbidden /tmp scratch"
      return
    fi
    missing=""
  done
  : "${missing:-}"

  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Test: assert_no_repo_tree_scratch detects planted leak dirs, passes clean tree
# --------------------------------------------------------------------------
test_assert_no_repo_tree_scratch_detects_leak_dirs() {
  local desc="assert_no_repo_tree_scratch detects leak dirs and passes clean tree"

  local fixture
  fixture="$(mktemp -d)"
  # Build a fake repo tree.
  mkdir -p "$fixture/repo/crates/foo"
  touch "$fixture/repo/crates/foo/lib.rs"

  # Clean tree → guard must return 0.
  local output
  if ! output=$(source "$CONTRACT_HELPER" && assert_no_repo_tree_scratch "$fixture/repo" 2>&1); then
    rm -rf "$fixture"
    tap_not_ok "$desc (clean)" "Guard failed on clean tree: $output"
    return
  fi

  # Plant a leak dir → guard must return non-zero AND name the offender.
  mkdir -p "$fixture/repo/.review-data/pr1/target"
  if output=$(source "$CONTRACT_HELPER" && assert_no_repo_tree_scratch "$fixture/repo" 2>&1); then
    rm -rf "$fixture"
    tap_not_ok "$desc (planted)" "Guard passed despite planted .review-data: $output"
    return
  fi
  if ! echo "$output" | grep -qF ".review-data"; then
    rm -rf "$fixture"
    tap_not_ok "$desc (names offender)" "Guard did not name the offender: $output"
    return
  fi

  # Remove the leak, plant a sibling-prefixed worktree (<basename>-pr<N>).
  rm -rf "$fixture/repo/.review-data"
  mkdir -p "$fixture/repo-pr42/target"
  if output=$(source "$CONTRACT_HELPER" && assert_no_repo_tree_scratch "$fixture/repo" 2>&1); then
    rm -rf "$fixture"
    tap_not_ok "$desc (sibling)" "Guard passed despite sibling repo-pr42: $output"
    return
  fi
  if ! echo "$output" | grep -qF "repo-pr42"; then
    rm -rf "$fixture"
    tap_not_ok "$desc (names sibling)" "Guard did not name the sibling offender: $output"
    return
  fi

  rm -rf "$fixture"
  tap_ok "$desc"
}

# --------------------------------------------------------------------------
# Run all tests
# --------------------------------------------------------------------------
echo "TAP version 13"

test_plan_bootstrap_rejects_hostile_worktree_base
test_plan_bootstrap_rejects_hostile_cargo_target_dir
test_plan_bootstrap_rejects_sibling_prefix_worktree_base
test_plan_bootstrap_accepts_safe_preseeded_home_data_paths
test_plan_bootstrap_clears_stale_vars_on_failure
test_review_pr_bootstrap_rejects_hostile_worktree_base_and_cargo_target
test_review_pr_bootstrap_rejects_sibling_prefix
test_review_pr_bootstrap_requires_home_data_workspace
test_review_pr_bootstrap_clears_stale_vars_on_failure
test_default_bootstrap_layouts_stay_under_home_data
test_documented_snippet_fails_closed
test_sourcing_preserves_caller_shell_options
test_home_poisoning_defeated
test_home_poisoning_explicit_override
test_shared_inbounds_directory_rejected
test_correct_session_prefix_overrides_accepted
test_skill_files_reference_shared_contract_helper
test_claude_review_pr_synced
test_claude_plan_synced
test_bootstraps_fallback_when_realpath_is_missing
test_bootstraps_fallback_when_realpath_rejects_dash_m
test_symlinked_home_alias_overrides_are_accepted
test_poisoned_python_on_path_ignored
test_do_bootstrap_rejects_hostile_and_sibling_paths
test_do_bootstrap_default_under_home_data
test_do_skill_uses_contract_helper_not_repo_tree
test_skill_prompts_forbid_known_leak_patterns
test_assert_no_repo_tree_scratch_detects_leak_dirs

echo "1..$TEST_NUM"
echo "# pass: $PASS"
echo "# fail: $FAIL"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
