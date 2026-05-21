#!/usr/bin/env bash
#
# Shared merge helpers for the /review-pr skill.
#
# Provides:
#   - attempt_merge PR_NUM  — try admin merge, classify failure, retry with --auto
#   - classify_linked_pr_state ISSUE — distinguish OPEN, MERGED, CLOSED-without-merge
#   - is_auto_merge_armed PR_NUM — check if PR has autoMergeRequest set
#
# Requires: Bash 4+, jq, gh CLI authenticated.
# Does NOT set shell options (set -e, -u, etc.) — callers control strictness.
# Idempotent: safe to source multiple times.
#

[[ -n "${_REVIEW_PR_MERGE_LOADED:-}" ]] && return 0
_REVIEW_PR_MERGE_LOADED=1

# ─────────────────────────────────────────────────────────────────────────────
# attempt_merge PR_NUM
#
# Attempts to merge the PR using admin squash merge. On failure, classifies the
# error:
#   - If the error contains the exact "add the `--auto` flag" hint from GitHub,
#     retries with `gh pr merge --squash --auto` (deferred merge).
#   - Any other failure is reported as-is (hard failure).
#
# Output on stdout: one of:
#   "merged"           — immediate merge succeeded
#   "auto-merge-armed" — deferred merge enabled via --auto
#   "hard-failure:<stderr>" — unrecoverable merge error
#
# Returns: 0 on merged or auto-merge-armed, 1 on hard failure.
# ─────────────────────────────────────────────────────────────────────────────
attempt_merge() {
  local pr_num="$1"
  local repo="${REVIEW_PR_REPO:-stellar-experimental/henyey}"

  local stderr_file
  stderr_file=$(mktemp)

  # Try admin merge first
  if _review_pr_exec_merge "$pr_num" "$repo" "--squash --admin" "$stderr_file"; then
    rm -f "$stderr_file"
    echo "merged"
    return 0
  fi

  local stderr_content
  stderr_content=$(cat "$stderr_file")

  # Classify the failure: does it contain the exact auto-merge hint?
  if _is_auto_hint_failure "$stderr_content"; then
    # Retry with --auto (without --admin since GH CLI rejects that combo)
    if _review_pr_exec_merge "$pr_num" "$repo" "--squash --auto" "$stderr_file"; then
      rm -f "$stderr_file"
      echo "auto-merge-armed"
      return 0
    fi

    # --auto also failed — hard failure
    stderr_content=$(cat "$stderr_file")
    rm -f "$stderr_file"
    echo "hard-failure:$stderr_content"
    return 1
  fi

  # Not an auto-hint failure — hard failure
  rm -f "$stderr_file"
  echo "hard-failure:$stderr_content"
  return 1
}

# ─────────────────────────────────────────────────────────────────────────────
# classify_linked_pr_state ISSUE
#
# Given an issue number, queries all linked PRs and returns the state:
#   "open:<pr_number>"    — an open PR is linked
#   "merged:<pr_number>"  — no open PR, but a merged one exists
#   "closed:<pr_number>"  — PR was closed without merge
#   "missing"             — no linked PRs at all
#
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
classify_linked_pr_state() {
  local issue_num="$1"
  local repo="${REVIEW_PR_REPO:-stellar-experimental/henyey}"

  local linked_prs
  linked_prs=$(_review_pr_fetch_linked_prs "$issue_num" "$repo")

  if [[ -z "$linked_prs" || "$linked_prs" == "null" ]]; then
    echo "missing"
    return 0
  fi

  # Check for OPEN first
  local open_pr
  open_pr=$(echo "$linked_prs" | jq -r '[.[] | select(.state == "OPEN")] | .[0].number // empty')
  if [[ -n "$open_pr" ]]; then
    echo "open:$open_pr"
    return 0
  fi

  # Check for MERGED
  local merged_pr
  merged_pr=$(echo "$linked_prs" | jq -r '[.[] | select(.state == "MERGED")] | .[0].number // empty')
  if [[ -n "$merged_pr" ]]; then
    echo "merged:$merged_pr"
    return 0
  fi

  # Check for CLOSED (without merge)
  local closed_pr
  closed_pr=$(echo "$linked_prs" | jq -r '[.[] | select(.state == "CLOSED")] | .[0].number // empty')
  if [[ -n "$closed_pr" ]]; then
    echo "closed:$closed_pr"
    return 0
  fi

  echo "missing"
}

# ─────────────────────────────────────────────────────────────────────────────
# is_auto_merge_armed PR_NUM
#
# Checks whether a PR has autoMergeRequest set (deferred auto-merge enabled).
#
# Output: "true" or "false" on stdout.
# Returns: 0 always.
# ─────────────────────────────────────────────────────────────────────────────
is_auto_merge_armed() {
  local pr_num="$1"
  local repo="${REVIEW_PR_REPO:-stellar-experimental/henyey}"

  local auto_merge
  auto_merge=$(_review_pr_fetch_auto_merge_state "$pr_num" "$repo")

  if [[ "$auto_merge" == "true" ]]; then
    echo "true"
  else
    echo "false"
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# Internal helpers
# ─────────────────────────────────────────────────────────────────────────────

# _review_pr_exec_merge PR_NUM REPO FLAGS STDERR_FILE
# Execute gh pr merge with given flags. Mockable via REVIEW_PR_MERGE_CMD.
# Returns: exit code of the merge command.
_review_pr_exec_merge() {
  local pr_num="$1"
  local repo="$2"
  local flags="$3"
  local stderr_file="$4"

  if [[ -n "${REVIEW_PR_MERGE_CMD:-}" ]]; then
    # Test mode: call the mock function
    $REVIEW_PR_MERGE_CMD "$pr_num" "$repo" "$flags" 2>"$stderr_file"
  else
    gh pr merge "$pr_num" --repo "$repo" $flags 2>"$stderr_file"
  fi
}

# _is_auto_hint_failure STDERR_CONTENT
# Returns 0 if stderr contains the exact GitHub hint to add --auto flag.
_is_auto_hint_failure() {
  local stderr="$1"
  # Match the exact GitHub error pattern
  if echo "$stderr" | grep -qF "add the \`--auto\` flag"; then
    return 0
  fi
  # Also match without backtick formatting (some GH CLI versions)
  if echo "$stderr" | grep -qF "add the --auto flag"; then
    return 0
  fi
  return 1
}

# _review_pr_fetch_linked_prs ISSUE REPO
# Fetches linked PRs for an issue. Mockable via REVIEW_PR_LINKED_PRS_FILE.
_review_pr_fetch_linked_prs() {
  local issue_num="$1"
  local repo="$2"

  if [[ -n "${REVIEW_PR_LINKED_PRS_FILE:-}" ]]; then
    cat "$REVIEW_PR_LINKED_PRS_FILE"
  else
    local owner repo_name
    owner=$(echo "$repo" | cut -d/ -f1)
    repo_name=$(echo "$repo" | cut -d/ -f2)
    gh api graphql -F num="$issue_num" -f query="
      query(\$num: Int!) {
        repository(owner: \"$owner\", name: \"$repo_name\") {
          issue(number: \$num) {
            closedByPullRequestsReferences(first: 5) {
              nodes { number state }
            }
          }
        }
      }
    " --jq '.data.repository.issue.closedByPullRequestsReferences.nodes'
  fi
}

# _review_pr_fetch_auto_merge_state PR_NUM REPO
# Fetches whether autoMergeRequest is set. Mockable via REVIEW_PR_AUTO_MERGE_FILE.
_review_pr_fetch_auto_merge_state() {
  local pr_num="$1"
  local repo="$2"

  if [[ -n "${REVIEW_PR_AUTO_MERGE_FILE:-}" ]]; then
    cat "$REVIEW_PR_AUTO_MERGE_FILE"
  else
    gh pr view "$pr_num" --repo "$repo" --json autoMergeRequest \
      --jq '.autoMergeRequest != null'
  fi
}
