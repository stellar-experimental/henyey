#!/usr/bin/env bash
# Usage: bounce-cap-check.sh <issue_number> <pr_number>
#
# Counts /review-pr "Bounce-Back Cycle" comments on issue $ISSUE that are
# newer than the current PR head commit OR newer than the latest
# "## Review: Reset" marker comment (whichever is later). Used by the
# /review-pr skill to decide whether the bounce-back cap (≥3) has been
# reached for the CURRENT code, not for the issue's lifetime.
#
# Exit code IS the verdict:
#   0  count < 3  (proceed with normal review)
#   1  count ≥ 3  (cycle cap reached — caller must post block comment)
#
# Stdout: single human-readable line with the count and baseline source,
# intended to be embedded verbatim in the caller's bounce/block PR comment.
# Example:
#   COUNT=2/3 BASELINE=HEAD_PUSHED@2026-05-17T12:34:56Z (no Reset marker)
#   COUNT=3/3 BASELINE=RESET_MARKER@2026-05-18T01:23:45Z (head pushed earlier)
#
# The algorithm and jq queries are copied VERBATIM from
# .claude/skills/review-pr/SKILL.md (Step 2 as of commit c51e8ac9) so the
# script is the single source of truth — the SKILL.md prose should not
# re-describe the algorithm, only invoke this script. See issue #2787 for
# the rationale (agents paraphrased the prose into the old lifetime-count
# rule when the prose drifted from a simpler interpretation).
set -euo pipefail

ISSUE="${1:?issue number required}"
PR_NUM="${2:?PR number required}"

OWNER="stellar-experimental"
REPO="henyey"

# ── Step 1: head-commit pushed-at timestamp ──────────────────────────────────
# Fresh push = new baseline; old bounces against earlier commits don't carry
# forward.
HEAD_PUSHED_ISO=$(gh pr view "$PR_NUM" --repo "$OWNER/$REPO" \
  --json commits --jq '.commits | sort_by(.committedDate) | last | .committedDate')

if [ -z "$HEAD_PUSHED_ISO" ]; then
  echo "ERROR: could not fetch HEAD_PUSHED_ISO for PR #$PR_NUM" >&2
  exit 2
fi

# ── Step 2: latest "## Review: Reset" marker, if any ─────────────────────────
# Manual escape hatch for cases where the head didn't change but the external
# cause did (e.g., main was broken, now green).
RESET_AT_ISO=$(gh api "repos/$OWNER/$REPO/issues/$ISSUE/comments" --paginate \
  --jq '[.[] | select(.body | startswith("## Review: Reset"))] | sort_by(.created_at) | last.created_at // ""')

# ── Step 3: convert both to epoch for unambiguous numeric comparison ─────────
HEAD_PUSHED_EPOCH=$(date -u -d "$HEAD_PUSHED_ISO" +%s)
if [ -n "$RESET_AT_ISO" ]; then
  RESET_AT_EPOCH=$(date -u -d "$RESET_AT_ISO" +%s)
else
  RESET_AT_EPOCH=0
fi

# ── Step 4: baseline = max(HEAD_PUSHED, RESET_AT) ────────────────────────────
if [ "$RESET_AT_EPOCH" -gt "$HEAD_PUSHED_EPOCH" ]; then
  BASELINE_EPOCH=$RESET_AT_EPOCH
  BASELINE_SOURCE="RESET_MARKER@$RESET_AT_ISO (head pushed earlier)"
else
  BASELINE_EPOCH=$HEAD_PUSHED_EPOCH
  if [ -n "$RESET_AT_ISO" ]; then
    BASELINE_SOURCE="HEAD_PUSHED@$HEAD_PUSHED_ISO (newer than Reset marker)"
  else
    BASELINE_SOURCE="HEAD_PUSHED@$HEAD_PUSHED_ISO (no Reset marker)"
  fi
fi

# ── Step 5: count bounces strictly newer than baseline ───────────────────────
# jq's fromdate parses ISO-8601 into epoch seconds — numeric, not lexical.
COUNT=$(gh api "repos/$OWNER/$REPO/issues/$ISSUE/comments" --paginate \
  --jq "[.[] | select(.body | startswith(\"## Review: Bounce-Back Cycle\")) |
        select((.created_at | fromdate) > $BASELINE_EPOCH)] | length")

# ── Step 6: emit verdict ─────────────────────────────────────────────────────
echo "COUNT=$COUNT/3 BASELINE=$BASELINE_SOURCE"

if [ "$COUNT" -ge 3 ]; then
  exit 1
fi
exit 0
