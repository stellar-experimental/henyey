#!/usr/bin/env bash
#
# Smoke-test harness for the /project-loop + /project-tick pick-priority and
# flaky-retry guidance.
#
# Regression guard for issue #3191: two pipeline-prioritization gaps surfaced
# when an `urgent` Quickstart-CI regression (#3185) sat unpicked for ~1hr while
# the orchestrator ground through lower-priority work, and the blind flaky
# "horizon core up" re-run masked the fact that the same check was already red
# on `main` (a systematic regression, not a flake). This harness asserts that
# the skills codify:
#   (1) an Urgent override at the very top of the pick order, present in BOTH
#       project-tick/SKILL.md Step 3 (the canonical order) AND project-loop's
#       Step D inline restatement (so the two skills cannot drift),
#   (2) the "triage urgents immediately — first, not a triage bypass" rule in
#       project-loop,
#   (3) a flaky-retry main-health pre-check that, on a systematic red across
#       recent `main` commits, escalates a `regression`/`infra-outage` `urgent`
#       issue (with the green→red commit boundary) and does NOT silently re-run,
#       re-running only when `main` is green and the failure is PR-isolated.
#
# COUPLING: these assertions key on STABLE tokens (`urgent`, `override` /
# `outranks` / `ahead of`, `regression`, `infra-outage`, `main` + `green`,
# `green`→`red`) rather than full sentences, so benign rewording of either
# SKILL.md is tolerated. If you reword the guidance, keep these tokens (or
# update this test in the same commit).
#
# Usage:
#   bash scripts/test-project-loop-skill-snippets.sh
#
# Output: TAP (Test Anything Protocol) on stdout, diagnostics on stderr.
# Exit: 0 = all pass, 1 = any fail.
#
# Portability: GNU/Linux only (Bash 4+, grep).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TICK_MD="$REPO_ROOT/.claude/skills/project-tick/SKILL.md"
LOOP_MD="$REPO_ROOT/.claude/skills/project-loop/SKILL.md"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=6
TAP_CURRENT=0
TAP_FAILURES=0

tap_plan() { echo "1..$TAP_PLAN"; }

tap_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  echo "ok $TAP_CURRENT - $1"
}

tap_not_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_FAILURES=$((TAP_FAILURES + 1))
  echo "not ok $TAP_CURRENT - $1"
  if [[ -n "${2:-}" ]]; then
    echo "# $2" >&2
  fi
}

# assert_match DESC PATTERN FILE
# Pass if (PCRE-less) ERE PATTERN matches at least one line of FILE.
assert_match() {
  local desc="$1" pattern="$2" file="$3"
  if grep -Eiq -- "$pattern" "$file"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "pattern not found: /$pattern/ in $file"
  fi
}

# ── Preconditions ────────────────────────────────────────────────────────────
if [[ ! -f "$TICK_MD" ]]; then
  echo "Bail out! project-tick SKILL.md not found at $TICK_MD" >&2
  exit 1
fi
if [[ ! -f "$LOOP_MD" ]]; then
  echo "Bail out! project-loop SKILL.md not found at $LOOP_MD" >&2
  exit 1
fi

tap_plan

# ── Gap 1: Urgent override at the top of the canonical pick order ─────────────
# project-tick Step 3 must lead with an Urgent override that sorts urgents above
# all state-priority tiers (an urgent backlog item outranks a non-urgent
# in-review item). On origin/main, Step 3 rule 1 is "Close-WIP-first state
# priority" and urgent is only a within-state tiebreaker → this FAILS.
assert_match "project-tick Step 3 leads with an Urgent override above state priority" \
  'urgent.*(override|outranks|ahead of|jump|very top|top of the pick)' "$TICK_MD"

# project-loop Step D restates the order inline and must lead with the urgent
# override too (so the two skills do not drift). FAILS on main.
assert_match "project-loop Step D inline ordering leads with the urgent override" \
  'urgent.*(override|outranks|ahead of|jump|very top|top of the pick)' "$LOOP_MD"

# Triage-urgents-immediately, and explicitly NOT a triage bypass (urgents still
# flow through /triage, they just go first). FAILS on main.
assert_match "project-loop triages urgents immediately — first, not a triage bypass" \
  '(triage.*urgent|urgent.*triage).*(immediately|first|never.*defer|untriaged)|not a (triage )?bypass' "$LOOP_MD"

# ── Gap 2: flaky-retry main-health guard ──────────────────────────────────────
# Before re-running the flaky check, the loop must check whether the same check
# is red on main HEAD + recent commits (by check name). FAILS on main.
assert_match "flaky bullet has a main-health pre-check before re-run" \
  '(main).*(HEAD|recent|last .*commit|recent .*commit)' "$LOOP_MD"

# A systematic red (≥2 main commits) is classified as a regression / infra
# outage and escalated as an urgent issue with the green→red commit boundary,
# and the re-run is conditioned on main being green. FAILS on main.
assert_match "systematic red escalates a regression/infra-outage urgent issue (green→red boundary, re-run only when main green)" \
  '(regression|infra-outage).*urgent|urgent.*(regression|infra-outage)' "$LOOP_MD"

# Do NOT silently re-run on the systematic path: a "NOT flaky" / "do not
# re-run" classification tied to the systematic-regression case. The existing
# line-144 "Do not re-run OTHER failing checks" does not match — this must name
# the systematic / NOT-flaky / regression classification. FAILS on main.
assert_match "flaky guard does NOT silently re-run on a systematic main failure (NOT flaky)" \
  '(not flaky|NOT flaky|do *not re-?run.*(systematic|regression|main)|(systematic|regression).*do *not re-?run)' "$LOOP_MD"

# ── Summary ──────────────────────────────────────────────────────────────────
if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES of $TAP_PLAN assertions failed" >&2
  exit 1
fi
echo "# all $TAP_PLAN assertions passed" >&2
exit 0
