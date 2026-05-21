#!/usr/bin/env bash
# agent-worktree-contract.sh — Shared helper for enforcing the ~/data workspace contract.
#
# All agent scratch work (worktrees, cargo targets, critic checkouts) must live
# under $HOME/data. This library provides canonicalization and enforcement helpers
# that skill bootstraps source before deriving workspace paths.
#
# Usage:
#   source scripts/lib/agent-worktree-contract.sh
#   require_home_data_path "$SOME_PATH" "SOME_PATH"
#   # or use the role-specific helpers:
#   plan_critic_bootstrap "$ISSUE" "critic-a"
#   review_pr_bootstrap "$ISSUE"

# NOTE: This file is meant to be sourced by callers. Do NOT set shell options
# (set -e, set -u, set -o pipefail, etc.) here — doing so mutates the caller's
# shell state, which is an operational regression risk. All functions below use
# explicit guards and return codes instead of relying on global shell options.

# canonicalize_contract_path <path>
# Resolve symlinks and collapse .. traversals without requiring the path to exist.
# Uses realpath -m (GNU coreutils) for canonical resolution.
canonicalize_contract_path() {
  local path="$1"
  realpath -m "$path"
}

# require_home_data_path <path> <var_name>
# Validates that a resolved path lives under $HOME/data. Exits non-zero with an
# error message if the path escapes the contract boundary.
require_home_data_path() {
  local path="$1"
  local var_name="$2"
  local canonical
  canonical="$(canonicalize_contract_path "$path")"
  local home_data
  home_data="$(canonicalize_contract_path "$HOME/data")"

  # Directory-boundary check: accept exact $HOME/data or paths under $HOME/data/.
  # A plain prefix check would incorrectly accept sibling paths like $HOME/data-evil.
  if [[ "$canonical" != "$home_data" && "$canonical" != "$home_data/"* ]]; then
    echo "ERROR: $var_name='$path' resolves to '$canonical' which is outside '$home_data'" >&2
    return 1
  fi
  echo "$canonical"
}

# validate_and_set_worktree_base <candidate> <var_name>
# If candidate is set, validate it stays under $HOME/data. Returns the canonical path.
validate_and_set_worktree_base() {
  local candidate="$1"
  local var_name="$2"
  require_home_data_path "$candidate" "$var_name"
}

# validate_and_set_cargo_target_dir <candidate> <var_name>
# If candidate is set, validate it stays under $HOME/data. Returns the canonical path.
validate_and_set_cargo_target_dir() {
  local candidate="$1"
  local var_name="$2"
  require_home_data_path "$candidate" "$var_name"
}

# plan_critic_bootstrap <issue> <critic_id>
# Derives and validates the full workspace layout for a /plan critic.
# Exports: WORKTREE_BASE, CARGO_TARGET_DIR, CRITIC_WORKTREE
# Respects pre-seeded env vars but validates them; rejects hostile values.
# On failure, clears all derived vars to prevent stale-env escape.
plan_critic_bootstrap() {
  local issue="$1"
  local critic_id="$2"
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}}"

  # Capture incoming overrides before clearing, so we can validate them.
  local incoming_base="${WORKTREE_BASE:-}"
  local incoming_cargo="${CARGO_TARGET_DIR:-}"

  # Clear derived vars on entry to prevent stale values from surviving a failure.
  unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE

  # Derive or validate WORKTREE_BASE
  local candidate_base="${incoming_base:-$HOME/data/$session_id/plan-$issue}"
  if ! WORKTREE_BASE="$(require_home_data_path "$candidate_base" "WORKTREE_BASE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
    return 1
  fi
  export WORKTREE_BASE

  # Derive or validate CARGO_TARGET_DIR
  local candidate_cargo="${incoming_cargo:-$WORKTREE_BASE/cargo-target}"
  if ! CARGO_TARGET_DIR="$(require_home_data_path "$candidate_cargo" "CARGO_TARGET_DIR")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
    return 1
  fi
  export CARGO_TARGET_DIR

  # Derive critic-specific worktree
  CRITIC_WORKTREE="$WORKTREE_BASE/$critic_id"
  if ! CRITIC_WORKTREE="$(require_home_data_path "$CRITIC_WORKTREE" "CRITIC_WORKTREE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
    return 1
  fi
  export CRITIC_WORKTREE
}

# review_pr_bootstrap <issue>
# Derives and validates the full workspace layout for a /review-pr reviewer.
# Exports: WORKTREE_BASE, CARGO_TARGET_DIR, REVIEWER_WORKTREE
# Respects pre-seeded env vars but validates them; rejects hostile values.
# On failure, clears all derived vars to prevent stale-env escape.
review_pr_bootstrap() {
  local issue="$1"
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}}"

  # Capture incoming overrides before clearing, so we can validate them.
  local incoming_base="${WORKTREE_BASE:-}"
  local incoming_cargo="${CARGO_TARGET_DIR:-}"

  # Clear derived vars on entry to prevent stale values from surviving a failure.
  unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE

  # Derive or validate WORKTREE_BASE
  local candidate_base="${incoming_base:-$HOME/data/$session_id/review-pr-$issue}"
  if ! WORKTREE_BASE="$(require_home_data_path "$candidate_base" "WORKTREE_BASE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
    return 1
  fi
  export WORKTREE_BASE

  # Derive or validate CARGO_TARGET_DIR
  local candidate_cargo="${incoming_cargo:-$WORKTREE_BASE/cargo-target}"
  if ! CARGO_TARGET_DIR="$(require_home_data_path "$candidate_cargo" "CARGO_TARGET_DIR")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
    return 1
  fi
  export CARGO_TARGET_DIR

  # Derive reviewer-specific worktree
  REVIEWER_WORKTREE="$WORKTREE_BASE/reviewer"
  if ! REVIEWER_WORKTREE="$(require_home_data_path "$REVIEWER_WORKTREE" "REVIEWER_WORKTREE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
    return 1
  fi
  export REVIEWER_WORKTREE
}
