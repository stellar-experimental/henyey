#!/usr/bin/env bash
# test-agent-worktree-contract.sh — TAP contract test for agent workspace placement.
#
# Verifies that /review-pr and /plan skill files enforce the ~/data workspace
# contract: all worktrees, cargo targets, and scratch dirs resolve under
# $HOME/data/$SESSION_ID/..., and both skills explicitly forbid repo-root or
# repo-parent worktree creation. Also verifies that .claude/skills/ copies
# remain synchronized with their .github/skills/ counterparts.
#
# Usage: bash scripts/test-agent-worktree-contract.sh
# Exit: 0 if all tests pass, 1 otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

REVIEW_PR_SKILL="$REPO_ROOT/.github/skills/review-pr/SKILL.md"
PLAN_SKILL="$REPO_ROOT/.github/skills/plan/SKILL.md"

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
# Test: review-pr workspace contract resolves under ~/data
# --------------------------------------------------------------------------
test_review_pr_workspace_contract_resolves_under_home_data() {
  local desc="review-pr workspace contract resolves under ~/data"

  # The skill must contain a reviewer workspace bootstrap that derives paths
  # under $HOME/data. We look for the documented pattern.
  if grep -q 'HOME/data/\$SESSION_ID/review-pr' "$REVIEW_PR_SKILL" ||
     grep -q 'HOME/data/\${SESSION_ID}/review-pr' "$REVIEW_PR_SKILL" ||
     grep -q '\~/data/\$SESSION_ID/review-pr' "$REVIEW_PR_SKILL" ||
     grep -q '\$HOME/data/.*review-pr' "$REVIEW_PR_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "SKILL.md does not contain a ~/data/\$SESSION_ID/review-pr workspace derivation"
  fi
}

# --------------------------------------------------------------------------
# Test: plan workspace contract resolves under ~/data
# --------------------------------------------------------------------------
test_plan_workspace_contract_resolves_under_home_data() {
  local desc="plan workspace contract resolves under ~/data"

  if grep -q 'HOME/data/\$SESSION_ID/plan' "$PLAN_SKILL" ||
     grep -q 'HOME/data/\${SESSION_ID}/plan' "$PLAN_SKILL" ||
     grep -q '\~/data/\$SESSION_ID/plan' "$PLAN_SKILL" ||
     grep -q '\$HOME/data/.*plan-\$ISSUE' "$PLAN_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "SKILL.md does not contain a ~/data/\$SESSION_ID/plan workspace derivation"
  fi
}

# --------------------------------------------------------------------------
# Test: skill prompts forbid repo-root worktrees
# --------------------------------------------------------------------------
test_skill_prompts_forbid_repo_root_worktrees() {
  local desc="skill prompts forbid repo-root worktrees"
  local review_has_guard=false
  local plan_has_guard=false

  # Check review-pr skill for explicit prohibition
  if grep -qi 'never.*worktree.*repo.*root\|never.*repo.*root.*worktree\|never.*create.*worktree.*outside.*~/data\|must not.*worktree.*outside.*\~/data\|do not.*create.*worktree.*outside\|never.*outside.*\$HOME/data\|must.*under.*\$HOME/data\|only.*under.*\$HOME/data' "$REVIEW_PR_SKILL"; then
    review_has_guard=true
  fi

  # Check plan skill for explicit prohibition
  if grep -qi 'never.*worktree.*repo.*root\|never.*repo.*root.*worktree\|never.*create.*worktree.*outside.*~/data\|must not.*worktree.*outside.*\~/data\|do not.*create.*worktree.*outside\|never.*outside.*\$HOME/data\|must.*under.*\$HOME/data\|only.*under.*\$HOME/data' "$PLAN_SKILL"; then
    plan_has_guard=true
  fi

  if $review_has_guard && $plan_has_guard; then
    tap_ok "$desc"
  else
    local missing=""
    $review_has_guard || missing="review-pr"
    $plan_has_guard || missing="${missing:+$missing, }plan"
    tap_not_ok "$desc" "Missing repo-root worktree prohibition in: $missing"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap is self-seeding (works with or without env vars)
# --------------------------------------------------------------------------
test_review_pr_self_seeding() {
  local desc="review-pr bootstrap is self-seeding (WORKTREE_BASE fallback)"

  # The skill should show a ${WORKTREE_BASE:-...} or SESSION_ID fallback pattern
  if grep -q 'WORKTREE_BASE:-\|SESSION_ID:-\|CLAUDE_SESSION_ID:-' "$REVIEW_PR_SKILL" ||
     grep -q 'WORKTREE_BASE:=' "$REVIEW_PR_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "No self-seeding fallback (e.g. \${WORKTREE_BASE:-...}) found in review-pr SKILL.md"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap is self-seeding (works with or without env vars)
# --------------------------------------------------------------------------
test_plan_self_seeding() {
  local desc="plan bootstrap is self-seeding (WORKTREE_BASE fallback)"

  if grep -q 'WORKTREE_BASE:-\|SESSION_ID:-\|CLAUDE_SESSION_ID:-' "$PLAN_SKILL" ||
     grep -q 'WORKTREE_BASE:=' "$PLAN_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "No self-seeding fallback (e.g. \${WORKTREE_BASE:-...}) found in plan SKILL.md"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr CARGO_TARGET_DIR resolves under ~/data
# --------------------------------------------------------------------------
test_review_pr_cargo_target_under_data() {
  local desc="review-pr CARGO_TARGET_DIR resolves under ~/data"

  if grep -q 'CARGO_TARGET_DIR.*HOME/data\|CARGO_TARGET_DIR.*~/data' "$REVIEW_PR_SKILL" ||
     grep -q 'CARGO_TARGET_DIR.*\$WORKTREE_BASE' "$REVIEW_PR_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "CARGO_TARGET_DIR not directed to ~/data in review-pr SKILL.md"
  fi
}

# --------------------------------------------------------------------------
# Test: plan CARGO_TARGET_DIR resolves under ~/data
# --------------------------------------------------------------------------
test_plan_cargo_target_under_data() {
  local desc="plan CARGO_TARGET_DIR resolves under ~/data"

  if grep -q 'CARGO_TARGET_DIR.*HOME/data\|CARGO_TARGET_DIR.*~/data' "$PLAN_SKILL" ||
     grep -q 'CARGO_TARGET_DIR.*\$WORKTREE_BASE' "$PLAN_SKILL"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "CARGO_TARGET_DIR not directed to ~/data in plan SKILL.md"
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
    # It's a symlink — verify it resolves to the .github copy
    local target resolved expected
    target="$(readlink "$claude_path")"
    # Guard: resolve the symlink target safely; broken/misdirected symlinks
    # must emit tap_not_ok rather than aborting the script under set -e.
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
    # Not a symlink — verify content is identical
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
    # It's a symlink — verify it resolves to the .github copy
    local target resolved expected
    target="$(readlink "$claude_path")"
    # Guard: resolve the symlink target safely; broken/misdirected symlinks
    # must emit tap_not_ok rather than aborting the script under set -e.
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
    # Not a symlink — verify content is identical
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
# Helper: extract the first fenced bash block from a markdown section.
# Usage: extract_bash_block FILE SECTION_HEADING
# Returns the content of the ```bash ... ``` block (without the fences).
# --------------------------------------------------------------------------
extract_bash_block() {
  local file="$1" heading="$2"
  if [ ! -r "$file" ]; then
    echo ""
    return 0
  fi
  sed -n "/^### *${heading}\|^## *${heading}/,/^## \|^### /{
    /^\`\`\`bash/,/^\`\`\`/{
      /^\`\`\`bash/d
      /^\`\`\`/d
      p
    }
  }" "$file" || { echo ""; return 0; }
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap rejects outside-$HOME/data overrides
# --------------------------------------------------------------------------
test_review_pr_bootstrap_rejects_outside_home_data_overrides() {
  local desc="review-pr bootstrap rejects outside-\$HOME/data overrides"

  # Extract the reviewer workspace contract bootstrap
  local snippet
  snippet=$(extract_bash_block "$REVIEW_PR_SKILL" "Reviewer workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract reviewer workspace contract bash block from review-pr SKILL.md"
    return
  fi

  # Run the snippet in a subshell with outside-$HOME/data overrides.
  # It should exit non-zero.
  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data" "$fake_home/not-under-data"

  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="test-session" \
    ISSUE="9999" \
    PR_NUM="1234" \
    WORKTREE_BASE="$fake_home/not-under-data/review-pr" \
    CARGO_TARGET_DIR="$fake_home/not-under-data/cargo-target" \
    bash -c "$snippet" 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Expected non-zero exit for outside-\$HOME/data overrides, got 0. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap rejects outside-$HOME/data overrides
# --------------------------------------------------------------------------
test_plan_bootstrap_rejects_outside_home_data_overrides() {
  local desc="plan bootstrap rejects outside-\$HOME/data overrides"

  # Extract the critic workspace contract bootstrap
  local snippet
  snippet=$(extract_bash_block "$PLAN_SKILL" "Critic workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract critic workspace contract bash block from plan SKILL.md"
    return
  fi

  # Run the snippet with outside-$HOME/data overrides — should exit non-zero.
  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data" "$fake_home/not-under-data"

  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="test-session" \
    ISSUE="9999" \
    WORKTREE_BASE="$fake_home/not-under-data/plan" \
    CARGO_TARGET_DIR="$fake_home/not-under-data/cargo-target" \
    bash -c "$snippet" 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Expected non-zero exit for outside-\$HOME/data overrides, got 0. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap preserves valid $HOME/data overrides
# --------------------------------------------------------------------------
test_review_pr_bootstrap_preserves_valid_home_data_overrides() {
  local desc="review-pr bootstrap preserves valid \$HOME/data overrides"

  local snippet
  snippet=$(extract_bash_block "$REVIEW_PR_SKILL" "Reviewer workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract reviewer workspace contract bash block from review-pr SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data/my-session/review-pr-1234"

  # Run with valid overrides under $HOME/data — should exit 0 and preserve values.
  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="test-session" \
    ISSUE="9999" \
    PR_NUM="1234" \
    WORKTREE_BASE="$fake_home/data/my-session/review-pr-1234" \
    CARGO_TARGET_DIR="$fake_home/data/my-session/review-pr-1234/cargo-target" \
    bash -c "$snippet"' && echo "WORKTREE_BASE=$WORKTREE_BASE" && echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"' 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_not_ok "$desc" "Expected exit 0 for valid overrides, got $exit_code. Output: $output"
    return
  fi

  # Verify the values survived (were not overwritten by defaults)
  if echo "$output" | grep -q "WORKTREE_BASE=$fake_home/data/my-session/review-pr-1234" &&
     echo "$output" | grep -q "CARGO_TARGET_DIR=$fake_home/data/my-session/review-pr-1234/cargo-target"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Valid overrides were not preserved. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap preserves valid $HOME/data overrides
# --------------------------------------------------------------------------
test_plan_bootstrap_preserves_valid_home_data_overrides() {
  local desc="plan bootstrap preserves valid \$HOME/data overrides"

  local snippet
  snippet=$(extract_bash_block "$PLAN_SKILL" "Critic workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract critic workspace contract bash block from plan SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data/my-session/plan-9999"

  # Run with valid overrides under $HOME/data — should exit 0 and preserve values.
  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="test-session" \
    ISSUE="9999" \
    WORKTREE_BASE="$fake_home/data/my-session/plan-9999" \
    CARGO_TARGET_DIR="$fake_home/data/my-session/plan-9999/cargo-target" \
    bash -c "$snippet"' && echo "WORKTREE_BASE=$WORKTREE_BASE" && echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"' 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_not_ok "$desc" "Expected exit 0 for valid overrides, got $exit_code. Output: $output"
    return
  fi

  # Verify the values survived
  if echo "$output" | grep -q "WORKTREE_BASE=$fake_home/data/my-session/plan-9999" &&
     echo "$output" | grep -q "CARGO_TARGET_DIR=$fake_home/data/my-session/plan-9999/cargo-target"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Valid overrides were not preserved. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap rejects traversal via CLAUDE_SESSION_ID
# --------------------------------------------------------------------------
test_review_pr_bootstrap_rejects_session_id_traversal() {
  local desc="review-pr bootstrap rejects CLAUDE_SESSION_ID path traversal"

  local snippet
  snippet=$(extract_bash_block "$REVIEW_PR_SKILL" "Reviewer workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract reviewer workspace contract bash block from review-pr SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data"

  # Use a traversal session ID — no WORKTREE_BASE/CARGO_TARGET_DIR overrides,
  # so the defaults are derived from CLAUDE_SESSION_ID. Should fail.
  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="../../escape" \
    PR_NUM="1234" \
    bash -c "$snippet" 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Expected non-zero exit for traversal session ID, got 0. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap rejects traversal via CLAUDE_SESSION_ID
# --------------------------------------------------------------------------
test_plan_bootstrap_rejects_session_id_traversal() {
  local desc="plan bootstrap rejects CLAUDE_SESSION_ID path traversal"

  local snippet
  snippet=$(extract_bash_block "$PLAN_SKILL" "Critic workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract critic workspace contract bash block from plan SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data"

  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="../../escape" \
    ISSUE="9999" \
    bash -c "$snippet" 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Expected non-zero exit for traversal session ID, got 0. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr bootstrap succeeds with normal CLAUDE_SESSION_ID (no overrides)
# --------------------------------------------------------------------------
test_review_pr_bootstrap_normal_session_id_succeeds() {
  local desc="review-pr bootstrap succeeds with normal CLAUDE_SESSION_ID"

  local snippet
  snippet=$(extract_bash_block "$REVIEW_PR_SKILL" "Reviewer workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract reviewer workspace contract bash block from review-pr SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data"

  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="abc123-normal-session" \
    PR_NUM="1234" \
    bash -c "$snippet"' && echo "WORKTREE_BASE=$WORKTREE_BASE"' 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_not_ok "$desc" "Expected exit 0 for normal session ID, got $exit_code. Output: $output"
    return
  fi

  if echo "$output" | grep -q "WORKTREE_BASE=$fake_home/data/abc123-normal-session/review-pr-1234"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Unexpected WORKTREE_BASE. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: plan bootstrap succeeds with normal CLAUDE_SESSION_ID (no overrides)
# --------------------------------------------------------------------------
test_plan_bootstrap_normal_session_id_succeeds() {
  local desc="plan bootstrap succeeds with normal CLAUDE_SESSION_ID"

  local snippet
  snippet=$(extract_bash_block "$PLAN_SKILL" "Critic workspace contract")

  if [ -z "$snippet" ]; then
    tap_not_ok "$desc" "Could not extract critic workspace contract bash block from plan SKILL.md"
    return
  fi

  local tmpdir
  tmpdir=$(mktemp -d "${HOME}/data/test-contract-XXXXXX")
  local fake_home="$tmpdir/fakehome"
  mkdir -p "$fake_home/data"

  local output exit_code=0
  output=$(
    HOME="$fake_home" \
    CLAUDE_SESSION_ID="abc123-normal-session" \
    ISSUE="9999" \
    bash -c "$snippet"' && echo "WORKTREE_BASE=$WORKTREE_BASE"' 2>&1
  ) || exit_code=$?

  rm -rf "$tmpdir"

  if [ "$exit_code" -ne 0 ]; then
    tap_not_ok "$desc" "Expected exit 0 for normal session ID, got $exit_code. Output: $output"
    return
  fi

  if echo "$output" | grep -q "WORKTREE_BASE=$fake_home/data/abc123-normal-session/plan-9999"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "Unexpected WORKTREE_BASE. Output: $output"
  fi
}

# --------------------------------------------------------------------------
# Test: review-pr bug-fix verification uses $REVIEWER_WORKTREE, not in-place checkout
# --------------------------------------------------------------------------
test_review_pr_bugfix_verification_uses_reviewer_worktree() {
  local desc="review-pr bug-fix verification uses \$REVIEWER_WORKTREE, not in-place checkout"

  # The bug-fix verification snippet (git fetch / git checkout / cargo test)
  # must be preceded by a cd into $REVIEWER_WORKTREE. If the snippet does a
  # bare "git checkout" without first entering the reviewer scratch workspace,
  # it would bypass the $HOME/data workspace contract.

  # The bug-fix verification snippet lives inside a blockquote with escaped
  # backtick fences (\`\`\`bash ... \`\`\`). Extract lines between them that
  # contain "git fetch origin pull/" and "git checkout".
  local in_block=false
  local block_content=""
  while IFS= read -r line; do
    # Match opening fence: lines containing \`\`\`bash (escaped backticks)
    if ! $in_block && echo "$line" | grep -q '\\`\\`\\`bash'; then
      in_block=true
      block_content=""
      continue
    fi
    # Match closing fence: \`\`\` without language tag
    if $in_block && echo "$line" | grep -q '\\`\\`\\`' && ! echo "$line" | grep -q '\\`\\`\\`bash'; then
      # End of block — check if it's the bug-fix verification block
      if echo "$block_content" | grep -q "git fetch origin pull/" && \
         echo "$block_content" | grep -q "git checkout"; then
        # Found it — verify it includes cd into REVIEWER_WORKTREE
        if echo "$block_content" | grep -q 'REVIEWER_WORKTREE'; then
          tap_ok "$desc"
          return
        else
          tap_not_ok "$desc" "Bug-fix verification block does git checkout without cd into \$REVIEWER_WORKTREE"
          return
        fi
      fi
      in_block=false
      continue
    fi
    if $in_block; then
      block_content+="$line"$'\n'
    fi
  done < "$REVIEW_PR_SKILL"

  tap_not_ok "$desc" "Could not find bug-fix verification code block in review-pr SKILL.md"
}

# --------------------------------------------------------------------------
# Run all tests
# --------------------------------------------------------------------------

# Ensure $HOME/data exists — CI runners (e.g. /home/runner) may not have it.
mkdir -p "$HOME/data"

echo "TAP version 13"

test_review_pr_workspace_contract_resolves_under_home_data
test_plan_workspace_contract_resolves_under_home_data
test_skill_prompts_forbid_repo_root_worktrees
test_review_pr_self_seeding
test_plan_self_seeding
test_review_pr_cargo_target_under_data
test_plan_cargo_target_under_data
test_claude_review_pr_synced
test_claude_plan_synced
test_review_pr_bootstrap_rejects_outside_home_data_overrides
test_plan_bootstrap_rejects_outside_home_data_overrides
test_review_pr_bootstrap_preserves_valid_home_data_overrides
test_plan_bootstrap_preserves_valid_home_data_overrides
test_review_pr_bootstrap_rejects_session_id_traversal
test_plan_bootstrap_rejects_session_id_traversal
test_review_pr_bootstrap_normal_session_id_succeeds
test_plan_bootstrap_normal_session_id_succeeds
test_review_pr_bugfix_verification_uses_reviewer_worktree

echo "1..$TEST_NUM"
echo "# pass: $PASS"
echo "# fail: $FAIL"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
