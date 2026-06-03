#!/usr/bin/env bash
# agent-worktree-contract.sh — Shared helper for enforcing the ~/data workspace contract.
#
# All agent scratch work (worktrees, cargo targets, critic checkouts) must live
# under <real-home>/data where <real-home> is derived from the passwd entry (not
# the potentially-poisoned $HOME env var). This library provides canonicalization
# and enforcement helpers that skill bootstraps source before deriving workspace paths.
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

# _contract_real_home — Returns the real home directory from the passwd database.
# This is the trust anchor — immune to $HOME poisoning.
_contract_real_home() {
  local pw_home
  pw_home="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
  if [[ -z "$pw_home" ]]; then
    # Fallback: if getent is unavailable (unlikely on Linux), use ~user expansion
    pw_home="$(eval echo "~$(id -un)")"
  fi
  echo "$pw_home"
}

# canonicalize_contract_path <path>
# Resolve symlinks and collapse .. traversals without requiring the path to exist.
# Tries GNU realpath -m first, falls back to Python os.path.realpath at a known
# absolute path, then fails closed (returns non-zero) if neither backend is available.
#
# SECURITY: The Python fallback uses absolute paths (/usr/bin/python3, /usr/bin/python)
# rather than PATH lookup. This prevents PATH-poisoning attacks where a malicious
# python3 stub could feed arbitrary output into the canonicalization trust chain.
canonicalize_contract_path() {
  local path="$1"
  local result

  # Fast path: GNU realpath -m (works on Linux with coreutils)
  if result="$(realpath -m "$path" 2>/dev/null)" && [[ -n "$result" ]]; then
    echo "$result"
    return 0
  fi

  # Fallback: Python os.path.realpath at known absolute paths (immune to PATH poisoning)
  local py
  for py in /usr/bin/python3 /usr/bin/python /usr/local/bin/python3; do
    if [[ -x "$py" ]]; then
      if result="$("$py" -c "import os, sys; print(os.path.realpath(sys.argv[1]))" "$path" 2>/dev/null)" && [[ -n "$result" ]]; then
        echo "$result"
        return 0
      fi
    fi
  done

  echo "ERROR: canonicalize_contract_path: no supported canonicalization backend available (need realpath -m or /usr/bin/python3)" >&2
  return 1
}

# require_home_data_path <path> <var_name>
# Validates that a resolved path lives under <real-home>/data. Exits non-zero with
# an error message if the path escapes the contract boundary. The trust anchor is
# the passwd-derived home, not $HOME, to prevent HOME-poisoning attacks.
require_home_data_path() {
  local path="$1"
  local var_name="$2"
  local canonical
  if ! canonical="$(canonicalize_contract_path "$path")" || [[ -z "$canonical" ]]; then
    echo "ERROR: $var_name='$path' could not be canonicalized (no backend available)" >&2
    return 1
  fi
  local real_home
  real_home="$(_contract_real_home)"
  local home_data
  if ! home_data="$(canonicalize_contract_path "$real_home/data")" || [[ -z "$home_data" ]]; then
    echo "ERROR: could not canonicalize home data path '$real_home/data'" >&2
    return 1
  fi

  # Directory-boundary check: accept exact <real-home>/data or paths under it.
  # A plain prefix check would incorrectly accept sibling paths like ~/data-evil.
  if [[ "$canonical" != "$home_data" && "$canonical" != "$home_data/"* ]]; then
    echo "ERROR: $var_name='$path' resolves to '$canonical' which is outside '$home_data'" >&2
    return 1
  fi
  echo "$canonical"
}

# require_session_prefix <canonical_path> <expected_prefix> <var_name>
# Validates that a canonical path lives under the expected session/issue prefix.
# This enforces per-session isolation: pre-seeded overrides cannot redirect to a
# shared or cross-session directory under ~/data.
require_session_prefix() {
  local canonical="$1"
  local expected_prefix="$2"
  local var_name="$3"
  local canon_prefix
  if ! canon_prefix="$(canonicalize_contract_path "$expected_prefix")" || [[ -z "$canon_prefix" ]]; then
    echo "ERROR: could not canonicalize expected prefix '$expected_prefix'" >&2
    return 1
  fi

  if [[ "$canonical" != "$canon_prefix" && "$canonical" != "$canon_prefix/"* ]]; then
    echo "ERROR: $var_name='$canonical' is not under expected session prefix '$canon_prefix'" >&2
    return 1
  fi
}

# plan_critic_bootstrap <issue> <critic_id>
# Derives and validates the full workspace layout for a /plan critic.
# Exports: WORKTREE_BASE, CARGO_TARGET_DIR, CRITIC_WORKTREE
# Validates pre-seeded env vars against both ~/data boundary AND session prefix.
# On failure, clears all derived vars to prevent stale-env escape.
plan_critic_bootstrap() {
  local issue="$1"
  local critic_id="$2"
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}}"
  local real_home
  real_home="$(_contract_real_home)"

  # The expected session prefix for plan operations
  local expected_prefix="$real_home/data/$session_id/plan-$issue"

  # Capture incoming overrides before clearing, so we can validate them.
  local incoming_base="${WORKTREE_BASE:-}"
  local incoming_cargo="${CARGO_TARGET_DIR:-}"

  # Clear derived vars on entry to prevent stale values from surviving a failure.
  unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE

  # Derive or validate WORKTREE_BASE
  local candidate_base="${incoming_base:-$real_home/data/$session_id/plan-$issue}"
  if ! WORKTREE_BASE="$(require_home_data_path "$candidate_base" "WORKTREE_BASE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_base" ]]; then
    if ! require_session_prefix "$WORKTREE_BASE" "$expected_prefix" "WORKTREE_BASE"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
      return 1
    fi
  fi
  export WORKTREE_BASE

  # Derive or validate CARGO_TARGET_DIR
  local candidate_cargo="${incoming_cargo:-$WORKTREE_BASE/cargo-target}"
  if ! CARGO_TARGET_DIR="$(require_home_data_path "$candidate_cargo" "CARGO_TARGET_DIR")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_cargo" ]]; then
    if ! require_session_prefix "$CARGO_TARGET_DIR" "$expected_prefix" "CARGO_TARGET_DIR"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR CRITIC_WORKTREE
      return 1
    fi
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
# Validates pre-seeded env vars against both ~/data boundary AND session prefix.
# On failure, clears all derived vars to prevent stale-env escape.
review_pr_bootstrap() {
  local issue="$1"
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}}"
  local real_home
  real_home="$(_contract_real_home)"

  # The expected session prefix for review-pr operations
  local expected_prefix="$real_home/data/$session_id/review-pr-$issue"

  # Capture incoming overrides before clearing, so we can validate them.
  local incoming_base="${WORKTREE_BASE:-}"
  local incoming_cargo="${CARGO_TARGET_DIR:-}"

  # Clear derived vars on entry to prevent stale values from surviving a failure.
  unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE

  # Derive or validate WORKTREE_BASE
  local candidate_base="${incoming_base:-$real_home/data/$session_id/review-pr-$issue}"
  if ! WORKTREE_BASE="$(require_home_data_path "$candidate_base" "WORKTREE_BASE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_base" ]]; then
    if ! require_session_prefix "$WORKTREE_BASE" "$expected_prefix" "WORKTREE_BASE"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
      return 1
    fi
  fi
  export WORKTREE_BASE

  # Derive or validate CARGO_TARGET_DIR
  local candidate_cargo="${incoming_cargo:-$WORKTREE_BASE/cargo-target}"
  if ! CARGO_TARGET_DIR="$(require_home_data_path "$candidate_cargo" "CARGO_TARGET_DIR")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_cargo" ]]; then
    if ! require_session_prefix "$CARGO_TARGET_DIR" "$expected_prefix" "CARGO_TARGET_DIR"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR REVIEWER_WORKTREE
      return 1
    fi
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

# do_bootstrap <issue>
# Derives and validates the full workspace layout for a /do implementation run.
# Exports: WORKTREE_BASE, CARGO_TARGET_DIR, DO_WORKTREE, DO_WORKSPACE
# DO_WORKTREE is the implementation worktree (under ~/data, NOT in the repo tree).
# DO_WORKSPACE is the canonical, fully-resolved absolute session-workspace dir
# (== WORKTREE_BASE) — the SINGLE source of truth for the directory /review-pr
# reaps on merge (issue #2979/#2978). /do persists this exact string to a
# session-independent marker; /review-pr reads and `rm -rf`s it verbatim, with no
# downstream recombination of the session id or $HOME, so the write path and the
# read path cannot diverge regardless of CLAUDE_SESSION_ID state or $HOME value.
# Validates pre-seeded env vars against both ~/data boundary AND session prefix.
# On failure, clears all derived vars to prevent stale-env escape.
do_bootstrap() {
  local issue="$1"
  local session_id="${CLAUDE_SESSION_ID:-${SESSION_ID:-$(date +%Y%m%d-%H%M%S)}}"
  local real_home
  real_home="$(_contract_real_home)"

  # The expected session prefix for do operations
  local expected_prefix="$real_home/data/$session_id/do-$issue"

  # Capture incoming overrides before clearing, so we can validate them.
  local incoming_base="${WORKTREE_BASE:-}"
  local incoming_cargo="${CARGO_TARGET_DIR:-}"

  # Clear derived vars on entry to prevent stale values from surviving a failure.
  unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE

  # Derive or validate WORKTREE_BASE
  local candidate_base="${incoming_base:-$real_home/data/$session_id/do-$issue}"
  if ! WORKTREE_BASE="$(require_home_data_path "$candidate_base" "WORKTREE_BASE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_base" ]]; then
    if ! require_session_prefix "$WORKTREE_BASE" "$expected_prefix" "WORKTREE_BASE"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE
      return 1
    fi
  fi
  export WORKTREE_BASE

  # Derive or validate CARGO_TARGET_DIR
  local candidate_cargo="${incoming_cargo:-$WORKTREE_BASE/cargo-target}"
  if ! CARGO_TARGET_DIR="$(require_home_data_path "$candidate_cargo" "CARGO_TARGET_DIR")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE
    return 1
  fi
  # Enforce session-prefix isolation for pre-seeded overrides
  if [[ -n "$incoming_cargo" ]]; then
    if ! require_session_prefix "$CARGO_TARGET_DIR" "$expected_prefix" "CARGO_TARGET_DIR"; then
      unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE
      return 1
    fi
  fi
  export CARGO_TARGET_DIR

  # Derive the implementation worktree (under ~/data, never inside the repo).
  DO_WORKTREE="$WORKTREE_BASE/worktree"
  if ! DO_WORKTREE="$(require_home_data_path "$DO_WORKTREE" "DO_WORKTREE")"; then
    unset WORKTREE_BASE CARGO_TARGET_DIR DO_WORKTREE DO_WORKSPACE
    return 1
  fi
  export DO_WORKTREE

  # DO_WORKSPACE is the canonical session-workspace dir to reap on merge. It is
  # exactly the validated, canonicalized WORKTREE_BASE — a stable alias so the
  # persist/reap contract refers to one named, fully-resolved absolute path
  # rather than re-deriving it from the session id downstream.
  DO_WORKSPACE="$WORKTREE_BASE"
  export DO_WORKSPACE
}

# assert_no_repo_tree_scratch <repo_root>
# Detection-only guard (NO deletion, NO recursive sweeps) that fails fast if any
# of the fixed, enumerated agent-scratch leak patterns (issue #2843) exists in
# the repo working tree or as a "<basename>-pr<N>" sibling of the repo parent.
# Prints each offender to stderr and returns non-zero if any are found.
#
# This is a fixed enumeration of the OBSERVED leak patterns — intentionally not
# a generalized filesystem janitor. Skills invoke it as a pre/post assertion so a
# leak is caught while still on disk; CI exercises it via a planted fixture.
assert_no_repo_tree_scratch() {
  local repo_root="$1"
  if [[ -z "$repo_root" ]]; then
    echo "ERROR: assert_no_repo_tree_scratch: repo_root argument is required" >&2
    return 2
  fi

  # Fixed enumeration of observed out-of-~/data scratch directory names.
  local leak_dirs=(
    ".review-data"
    ".review-worktrees"
    ".worktrees"
    ".copilot-tmp"
    ".opencode/worktrees"
  )

  local found=0
  local d
  for d in "${leak_dirs[@]}"; do
    if [[ -e "$repo_root/$d" ]]; then
      echo "ERROR: agent scratch leak detected in repo tree: $repo_root/$d (see #2843)" >&2
      found=1
    fi
  done

  # Sibling worktrees named "<basename>-pr<N>" alongside the repo (e.g. a
  # /tmp/henyey-pr2797 sibling, or <repo>-pr42). Match only the parent dir.
  # Enumerate via `find` (depth 1) rather than a shell glob: a bare glob is
  # not portable across shells — zsh's default `nomatch` makes an unmatched
  # pattern a hard error, while bash leaves the literal string. `find` is
  # immune to both and to the no-match case.
  local parent base sib
  parent="$(dirname "$repo_root")"
  base="$(basename "$repo_root")"
  if [[ -d "$parent" ]]; then
    while IFS= read -r sib; do
      [[ -n "$sib" ]] || continue
      echo "ERROR: agent scratch leak detected as repo sibling: $sib (see #2843)" >&2
      found=1
    done < <(find "$parent" -mindepth 1 -maxdepth 1 -name "$base-pr*" 2>/dev/null)
  fi

  if [[ "$found" -ne 0 ]]; then
    return 1
  fi
  return 0
}
