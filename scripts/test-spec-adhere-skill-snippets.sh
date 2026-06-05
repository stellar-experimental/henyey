#!/usr/bin/env bash
#
# Smoke-test harness for the /spec-adhere skill guidance.
#
# Regression guard for issue #3158: the /spec-adhere audit had a cross-crate
# blind spot — it searched only the spec-named crate (e.g. crates/overlay for
# OVERLAY_SPEC) and missed implementations wired in crates/app (the event loop
# owns overlay/herder/scp dispatch). This produced false-positive "Absent"
# findings (e.g. #3069/#3070/#3076 — survey/flood subsystem implemented in
# crates/app/src/app/{survey_impl,tx_flooding}.rs while crates/overlay's
# SurveyManager is dead code). This harness asserts that SKILL.md codifies:
#   (a) crates/app as a secondary search target for OVERLAY_SPEC and HERDER_SPEC,
#   (b) an all-crates search pass before classifying (not just the named crate),
#   (c) a reachability/instantiation check that distinguishes a live
#       implementation from a dead scaffold, and a rule forbidding "Absent"
#       without grepping all crates first.
#
# COUPLING: these assertions key on STABLE tokens (crates/app, "all crates" /
# "entire crates/" tree, "instantiated" / "reachable") rather than full
# sentences, so benign rewording of SKILL.md is tolerated. If you reword the
# guidance, keep these tokens (or update this test in the same commit).
#
# Usage:
#   bash scripts/test-spec-adhere-skill-snippets.sh
#
# Output: TAP (Test Anything Protocol) on stdout, diagnostics on stderr.
# Exit: 0 = all pass, 1 = any fail.
#
# Portability: GNU/Linux only (Bash 4+, grep).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SKILL_MD="$REPO_ROOT/.claude/skills/spec-adhere/SKILL.md"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=8
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

# assert_match DESC PATTERN [FILE]
# Pass if PCRE-less ERE PATTERN matches at least one line of FILE (default SKILL_MD).
assert_match() {
  local desc="$1" pattern="$2" file="${3:-$SKILL_MD}"
  if grep -Eiq -- "$pattern" "$file"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "pattern not found: /$pattern/ in $file"
  fi
}

# assert_section_match DESC SECTION_START SECTION_END PATTERN
# Extract the lines between SECTION_START and SECTION_END (sed BRE addresses,
# both inclusive) and assert PATTERN matches within that slice. This scopes the
# assertion to the relevant procedure step so an incidental match elsewhere in
# the doc does not mask a missing edit.
assert_section_match() {
  local desc="$1" start="$2" end="$3" pattern="$4"
  local slice
  slice="$(sed -n "/$start/,/$end/p" "$SKILL_MD")"
  if printf '%s\n' "$slice" | grep -Eiq -- "$pattern"; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "pattern not found: /$pattern/ within section [$start .. $end]"
  fi
}

# ── Preconditions ────────────────────────────────────────────────────────────
if [[ ! -f "$SKILL_MD" ]]; then
  echo "Bail out! spec-adhere SKILL.md not found at $SKILL_MD" >&2
  exit 1
fi

tap_plan

# ── (a) Mapping table: crates/app is a secondary target for OVERLAY & HERDER ──
# The OVERLAY_SPEC and HERDER_SPEC rows must reference crates/app so the auditor
# knows the event-loop wiring lives there.
assert_match "OVERLAY_SPEC mapping row references crates/app" \
  '^\|[[:space:]]*\`?OVERLAY_SPEC\`?[[:space:]]*\|.*crates/app'
assert_match "HERDER_SPEC mapping row references crates/app" \
  '^\|[[:space:]]*\`?HERDER_SPEC\`?[[:space:]]*\|.*crates/app'

# ── (b) Step 2: an all-crates search pass, not just the named crate ───────────
# Must instruct grepping the entire crates/ tree before classifying.
assert_section_match "Step 2 instructs an all-crates / entire crates tree search" \
  '### Step 2' '### Step 3' '(all crates|every crate|entire \`?crates/|whole \`?crates/|across all crates)'

# ── (c) Step 3: reachability check + no-Absent-without-all-crates-grep rule ───
assert_section_match "Step 3 contains a reachability / instantiation check" \
  '### Step 3' '### Step 4' '(instantiat|reachab|actually called|production (caller|control path)|dead (code|scaffold))'
assert_section_match "Step 3 forbids reporting Absent without an all-crates grep" \
  '### Step 3' '### Step 4' '(Absent.*(all crates|every crate|entire \`?crates/)|(all crates|every crate|entire \`?crates/).*Absent)'

# ── Reinforcing assertions (document the intent in stable tokens) ─────────────
# The dead-scaffold caveat must name the crates/app-owns-live-wiring rule so the
# auditor does not treat an exported-but-unwired library symbol as "the impl".
assert_match "doc explains crates/app owns the live overlay/herder/scp wiring" \
  'crates/app.*(wiring|dispatch|event loop|event-loop)'
assert_match "doc warns library crates may contain dead / unwired code" \
  '(dead|unwired|never instantiat|exported.*never).*(code|scaffold|symbol)'
assert_match "doc references the cross-crate grep as a concrete auditor step" \
  '(grep|search).*(all crates|entire \`?crates/|whole \`?crates/|every crate|crates/app)'

# ── Summary ──────────────────────────────────────────────────────────────────
if [[ "$TAP_FAILURES" -gt 0 ]]; then
  echo "# $TAP_FAILURES of $TAP_PLAN assertions failed" >&2
  exit 1
fi
echo "# all $TAP_PLAN assertions passed" >&2
exit 0
