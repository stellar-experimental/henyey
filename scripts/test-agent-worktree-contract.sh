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
CONTRACT_HELPER="$REPO_ROOT/scripts/lib/agent-worktree-contract.sh"

PASS=0
FAIL=0
TEST_NUM=0

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
  home_data="$(realpath -m "$HOME/data")"
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
  home_data="$(realpath -m "$HOME/data")"
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
  home_data="$(realpath -m "$HOME/data")"

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
# Run all tests
# --------------------------------------------------------------------------
echo "TAP version 13"

test_plan_bootstrap_rejects_hostile_worktree_base
test_plan_bootstrap_rejects_hostile_cargo_target_dir
test_plan_bootstrap_rejects_sibling_prefix_worktree_base
test_plan_bootstrap_accepts_safe_preseeded_home_data_paths
test_review_pr_bootstrap_rejects_hostile_worktree_base_and_cargo_target
test_review_pr_bootstrap_rejects_sibling_prefix
test_review_pr_bootstrap_requires_home_data_workspace
test_default_bootstrap_layouts_stay_under_home_data
test_skill_files_reference_shared_contract_helper
test_claude_review_pr_synced
test_claude_plan_synced

echo "1..$TEST_NUM"
echo "# pass: $PASS"
echo "# fail: $FAIL"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
