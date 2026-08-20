#!/usr/bin/env bash
#
# Deploy quarantine helpers for monitor-tick and monitor-loop skills.
#
# Provides testable, reusable functions for:
#   - Parsing deploy_quarantine.txt (validate SHAs, skip comments/blanks)
#   - Checking ancestry reachability (with fail-closed error handling)
#   - Appending entries idempotently
#   - Removing entries atomically
#
# File format (deploy_quarantine.txt):
#   - Line-oriented, default-whitespace-separated fields
#   - First field: 40 lowercase hex chars (commit SHA)
#   - Remaining fields: optional free-text reason (single line)
#   - An optional `resolved:<40-hex>` token MAY appear anywhere in the reason
#     (token-boundary-anchored). It records the fix commit that resolves the
#     quarantine: once that fix SHA is an ancestor of origin/main (MERGED) AND
#     is VE-green (a fix-containing sha has passed `Verify Execution (Mainnet)`,
#     so it is an actual deploy target — #3632), the entry auto-clears (see
#     check_quarantine_active). Additive — absent on legacy entries, which keep
#     the per-hunk content-check as their backstop.
#   - An optional `hold:until-#<N>` sentinel token MAY appear anywhere in the
#     reason (canonically right after the SHA):
#         <sha> hold:until-#<N> <reason...>
#     It pins the entry to the LIFECYCLE OF GITHUB ISSUE #N instead of the
#     textual presence of the SHA's diff. check_quarantine_active honors it
#     BEFORE the resolved-token auto-clear and the per-hunk content-check:
#     while issue #N is not verifiably CLOSED (issue OPEN, empty gh output,
#     gh error, timeout, offline) the entry BLOCKS — fail-closed. Only a
#     confirmed CLOSED state releases the sentinel, after which the entry
#     falls through to the normal resolved/content-check logic. Rationale
#     (#3711): the per-hunk content-check backstop DECAYS as main evolves —
#     once every hunk of the quarantined commit drifts, the gate false-clears
#     even though the behavioral regression the hold protects against (e.g.
#     #3702) is still open. quarantine_autostamp NEVER stamps `resolved:`
#     onto a hold:until entry — the sentinel is manual-lifecycle-only (this
#     also prevents the #3708 bundled-`#N` false-clear class on exactly the
#     entries that must not auto-clear).
#   - An optional `MANUAL-CLEAR-ONLY` marker (#3708) MAY appear anywhere in the
#     reason. It is matched as a LITERAL, CASE-SENSITIVE, boundary-anchored
#     token (operators must use the exact spelling). It pins the entry to
#     operator-only lifecycle: quarantine_autostamp NEVER auto-stamps such an
#     entry, and check_quarantine_active NEVER auto-clears it via the resolved
#     token (defense-in-depth — even a legacy/manual `resolved:` stamp cannot
#     false-clear it); the entry stays governed by the per-hunk content-check
#     until the operator removes it. Use it when a reason must bundle sibling/
#     family refs but the real blocker is a specific still-open issue.
#   - AUTOSTAMP FAIL-CLOSED RULE (#3708): quarantine_autostamp stamps `resolved:`
#     only when the reason references EXACTLY ONE distinct `#N` issue ref. A
#     reason bundling more than one distinct `#N` (`#3582/#3702 family`) is
#     ambiguous about which ref is the blocker, so a closed+merged sibling must
#     never stamp over a still-open real blocker — such entries are left
#     un-stamped and governed by the content-check + manual removal. (An entry
#     whose prose incidentally mentions a second `#N` is likewise left
#     un-stamped — an acceptable, recoverable over-block, never a false-clear.)
#   - Lines starting with # (after optional whitespace) are comments
#   - Blank/whitespace-only lines are skipped
#   - CRLF is stripped during parsing
#
# Concurrency: single-writer assumption (one monitor-tick agent at a time).
#
# Requires: Bash 4+, GNU/Linux (awk, grep, printf, mv).
# Does NOT set shell options — callers control strictness.
# Idempotent: safe to source multiple times.
#

[[ -n "${_DEPLOY_QUARANTINE_LOADED:-}" ]] && return 0
_DEPLOY_QUARANTINE_LOADED=1

# ─────────────────────────────────────────────────────────────────────────────
# parse_quarantine_file QUARANTINE_FILE
#
# Read-only. Parses the quarantine file into structured globals.
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#
# Sets globals:
#   QUARANTINE_ENTRIES  - Newline-separated valid SHAs (order preserved)
#   QUARANTINE_RESOLVED - Newline-separated resolved-fix SHAs, ONE PER ENTRY in
#                         the SAME order as QUARANTINE_ENTRIES. A line is the
#                         40-hex fix SHA when the entry carried a valid,
#                         token-boundary-anchored `resolved:<sha>`, or "-" when
#                         it did not. The two globals are index-aligned.
#   QUARANTINE_REASONS  - Newline-separated reason text (everything after the
#                         SHA on the entry's line), ONE PER ENTRY in the SAME
#                         order as QUARANTINE_ENTRIES — index-aligned with both
#                         QUARANTINE_ENTRIES and QUARANTINE_RESOLVED. A bare-SHA
#                         entry (no reason) contributes an EMPTY line so the
#                         lockstep three-FD read in quarantine_autostamp never
#                         skews. Reasons are single-line by construction
#                         (quarantine_append strips \n\t\r), so newline-joining
#                         is lossless. Used by quarantine_autostamp to recover
#                         the recorded crash issue # (`regression #N`, #3258).
#   QUARANTINE_WARNINGS - Comma-space-separated warning messages
#
# Returns:
#   0 — file parsed (missing/empty is OK; malformed entries produce warnings)
#   1 — file exists but is unreadable (fail-closed)
# ─────────────────────────────────────────────────────────────────────────────
parse_quarantine_file() {
  local file="$1"
  QUARANTINE_ENTRIES=""
  QUARANTINE_RESOLVED=""
  QUARANTINE_REASONS=""
  QUARANTINE_WARNINGS=""

  # Missing or empty file: clear, no warnings
  if [[ ! -e "$file" ]]; then
    return 0
  fi
  if [[ ! -s "$file" ]]; then
    return 0
  fi

  # Unreadable file: fail-closed
  if [[ ! -r "$file" ]]; then
    QUARANTINE_WARNINGS="unreadable: $file"
    return 1
  fi

  local line sha _rest
  while IFS= read -r line || [[ -n "$line" ]]; do
    # Strip CRLF
    line="${line%$'\r'}"

    # Trim leading whitespace
    local trimmed="${line#"${line%%[![:space:]]*}"}"

    # Skip blank lines
    [[ -z "$trimmed" ]] && continue

    # Skip comments
    [[ "$trimmed" == \#* ]] && continue

    # Extract first whitespace-delimited field
    read -r sha _rest <<< "$trimmed"

    # Validate: exactly 40 lowercase hex chars
    if [[ "$sha" =~ ^[0-9a-f]{40}$ ]]; then
      # Extract an optional `resolved:<40-hex>` token from the reason field.
      # Anchored on token boundaries (start-or-whitespace before, end-or-
      # whitespace after) so a substring like `unresolved:...` or a >40-hex
      # blob never false-matches. Default "-" (no resolved annotation).
      # Done via grep (not the `=~` capture array) so the extraction is
      # identical under bash and zsh — zsh populates `$match`, not
      # `$BASH_REMATCH`, for `[[ =~ ]]` capture groups (#3256).
      local resolved="-" _resolved_tok
      # `|| true`: grep exits 1 on no-match; the `|| true` keeps this from
      # tripping a caller's `set -e`/`pipefail` (the no-token case is normal).
      _resolved_tok="$(printf '%s' " $_rest " | grep -oE '[[:space:]]resolved:[0-9a-f]{40}[[:space:]]' | head -n1 || true)"
      if [[ -n "$_resolved_tok" ]]; then
        # Strip the leading-space + `resolved:` prefix and the trailing space.
        _resolved_tok="${_resolved_tok#"${_resolved_tok%%[![:space:]]*}"}"
        _resolved_tok="${_resolved_tok%"${_resolved_tok##*[![:space:]]}"}"
        resolved="${_resolved_tok#resolved:}"
      fi
      if [[ -n "$QUARANTINE_ENTRIES" ]]; then
        QUARANTINE_ENTRIES+=$'\n'"$sha"
        QUARANTINE_RESOLVED+=$'\n'"$resolved"
        QUARANTINE_REASONS+=$'\n'"$_rest"
      else
        QUARANTINE_ENTRIES="$sha"
        QUARANTINE_RESOLVED="$resolved"
        QUARANTINE_REASONS="$_rest"
      fi
    else
      local warning="malformed: ${sha:0:12}"
      [[ ${#sha} -gt 12 ]] && warning+="..."
      if [[ -n "$QUARANTINE_WARNINGS" ]]; then
        QUARANTINE_WARNINGS+=", $warning"
      else
        QUARANTINE_WARNINGS="$warning"
      fi
    fi
  done < "$file"

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_split_hunks
#
# Read-only filter. Reads a unified `git diff` on stdin and emits it split into
# one self-contained patch per hunk on stdout, with each per-hunk patch
# terminated by a NUL byte (so callers can iterate with `read -r -d ''` even
# when a patch body contains blank lines). Each emitted patch carries the full
# per-file header (`diff --git`/`old mode`/`new mode`/`index`/`---`/`+++`/etc.)
# that preceded the `@@` hunk, so it is independently apply-checkable.
#
# We split per-HUNK (not per-file) deliberately: when a quarantined commit's
# harmful hunk and an unrelated drifted hunk live in the SAME file, a per-file
# patch is still all-or-nothing under `git apply --check` and would false-clear
# (the drifted hunk fails → the whole file's patch fails). Per-hunk is the
# minimal granularity that isolates "is THIS harmful hunk still present?".
#
# `filterdiff` is not installed, so this is a small awk program. It tracks the
# current file header (everything from a `diff --git` line up to and including
# the `+++` line) and, on each `@@` line, emits header + that single hunk's
# body (up to the next `@@` or `diff --git`), NUL-terminated. Binary stanzas,
# pure mode/rename changes, and other no-`@@` content produce zero hunks (no
# false "applies"); the caller's fail-safe covers the zero-hunk case.
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_split_hunks() {
  awk '
    function flush_hunk() {
      if (in_hunk) {
        printf "%s%s", header, hunk
        printf "%c", 0
        hunk = ""
        in_hunk = 0
      }
    }
    /^diff --git / {
      flush_hunk()
      header = $0 "\n"
      in_header = 1
      next
    }
    # File-header lines accumulate into header until the first @@.
    in_header && /^@@/ {
      in_header = 0
    }
    in_header {
      header = header $0 "\n"
      next
    }
    /^@@/ {
      # New hunk for the current file: emit the previous one first.
      flush_hunk()
      in_hunk = 1
      hunk = $0 "\n"
      next
    }
    {
      if (in_hunk) hunk = hunk $0 "\n"
    }
    END { flush_hunk() }
  '
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_any_hunk_present SHA
#
# Read-only + git subprocess. Splits `git diff <sha>^..<sha>` into per-hunk
# patches and reverse-apply-checks EACH hunk independently. Returns:
#   0 — at least one hunk still reverse-applies (harmful content PRESENT → BLOCK)
#   1 — every hunk fails to reverse-apply (content genuinely removed → CLEAR)
#   2 — fail-safe: `git diff` errored, produced no output, or produced output
#       from which no hunks could be parsed (treat as PRESENT → BLOCK)
#
# FAIL-SAFE direction: this gate guards deploys of a binary already proven to
# crash the validator. A false BLOCK only delays a deploy (recoverable); a
# false CLEAR re-crashes the node (catastrophic). So every uncertain case errs
# toward BLOCK. The per-hunk loop captures each hunk's rc WITHOUT letting a
# non-zero (expected: rejected hunk) abort under a caller's `set -e`.
#
# OVER-BLOCK is INTENTIONAL: "any hunk present → block" keeps a commit with
# MIXED harmful+benign hunks blocked while a BENIGN hunk survives (e.g. #3238
# added both `retain:false` (harmful) AND the `_rjem_malloc_conf` export
# (benign — #3251 keeps it); after the harmful line is removed, the benign
# export remains, so this stays blocked). This is fail-safe and by design: the
# content-check is a BACKSTOP — the operator removes the quarantine ENTRY once
# a fix is verified. We deliberately do NOT try to auto-distinguish
# harmful-vs-benign hunks (that is the rejected content-signature approach,
# which would require per-entry harmful-line identification + a schema change).
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_any_hunk_present() {
  local sha="$1"
  local diff_out diff_rc=0

  # Capture the full commit diff. A hard git error (missing parent, etc.) or
  # empty output is fail-safe → PRESENT.
  diff_out="$(git diff "${sha}^..${sha}" 2>/dev/null)" || diff_rc=$?
  if [[ "$diff_rc" -ne 0 ]]; then
    return 2
  fi
  if [[ -z "$diff_out" ]]; then
    return 2
  fi

  local any_present=0 n_hunks=0 patch
  # Iterate NUL-delimited per-hunk patches. `read -r -d ''` keeps blank lines
  # inside a hunk intact and behaves identically under bash and zsh.
  while IFS= read -r -d '' patch; do
    [[ -z "$patch" ]] && continue
    n_hunks=$((n_hunks + 1))
    # rc=1 (hunk rejected) is NORMAL — must not abort the loop under set -e.
    if printf '%s' "$patch" | git apply --check --reverse 2>/dev/null; then
      any_present=1
    fi
  done < <(printf '%s\n' "$diff_out" | _quarantine_split_hunks)

  # Non-empty diff but zero hunks parsed (pure rename/mode/binary, or a parser
  # gap): fail-safe → PRESENT. Never silently treat "couldn't parse" as CLEAR.
  if [[ "$n_hunks" -eq 0 ]]; then
    return 2
  fi

  if [[ "$any_present" -eq 1 ]]; then
    return 0
  fi
  return 1
}

# ─────────────────────────────────────────────────────────────────────────────
# quarantine_resolved_is_ve_green SHA   (issue #3632)
#
# VE-green oracle for the resolved-token auto-clear in check_quarantine_active.
# Returns 0 iff SHA (or one of its descendants that is on main) is itself a
# VALID DEPLOY TARGET — i.e. has a `success` `Verify Execution (Mainnet)` run.
# Returns non-zero (1) when no such run exists or on any gh/parse error.
#
# WHY THIS EXISTS: a resolved-token records the fix PR's MERGE commit. On a busy
# `main` that merge lands long before the daily `Verify Execution (Mainnet)`
# cron validates it. The old auto-clear (`merge-base --is-ancestor <fix> main`)
# cleared the quarantine the instant the fix MERGED — but the deploy gate
# (`select_latest_green_deploy_target`) ships the LATEST VE-green sha, which in
# that window is still a PRE-FIX commit. Clearing on merge therefore green-lit
# shipping a binary that does NOT contain the fix (#3632). Gating the clear on
# the fix being VE-green mirrors the selector's own notion of "deployable" so
# the quarantine only lifts once a fix-containing sha is actually a deploy
# target.
#
# The "(or a descendant on main)" allowance: VE runs on whatever sha was HEAD at
# the cron, which is usually a DESCENDANT of the fix merge, not the merge commit
# itself. A green VE on any on-main descendant of <fix> proves a fix-containing
# binary passed VE, so it satisfies the clear. We therefore accept a `success`
# VE run whose headSha is <fix> OR has <fix> as an ancestor (and is itself on
# main). Cross-shell ancestry uses the same `is_ancestor` oracle convention as
# monitor-decisions.sh.
#
# OVERRIDABLE FOR TESTS: like monitor-decisions.sh's `is_ancestor`, this is a
# hermetic injection point. Tests define `quarantine_resolved_is_ve_green`
# before sourcing (or shadow it) to drive the predicate without touching gh.
# The `command -v` guard (NOT bash-only `declare -F`) keeps the default-define
# correct under zsh, where `declare` aliases `typeset` (#3592).
#
# FAIL-SAFE: any uncertainty (no green VE run, gh/network/parse error, no
# locally-resolvable ancestry) returns non-zero → the caller does NOT clear and
# falls through to the per-hunk content-check (BLOCK backstop). A false
# "not-VE-green" only delays a deploy (recoverable); a false "VE-green" would
# clear a quarantine for a not-yet-verified fix (the #3632 hazard).
# ─────────────────────────────────────────────────────────────────────────────
if ! command -v quarantine_resolved_is_ve_green >/dev/null 2>&1; then
  quarantine_resolved_is_ve_green() {
    local fix_sha="$1"
    [[ "$fix_sha" =~ ^[0-9a-f]{40}$ ]] || return 1
    local repo="${QUARANTINE_GH_REPO:-stellar-experimental/henyey}"

    # Pull recent successful `Verify Execution (Mainnet)` head SHAs (newest
    # first). The workflow `name:` is literally "Verify Execution (Mainnet)".
    # `--jq` keeps only completed/success records and emits their headSha, one
    # per line. Any gh/network error → empty → fail-safe non-zero below.
    local ve_heads
    ve_heads="$(gh run list --repo "$repo" \
                  --workflow "Verify Execution (Mainnet)" --branch main --limit 30 \
                  --json headSha,status,conclusion \
                  --jq '.[] | select(.status=="completed" and .conclusion=="success") | .headSha' \
                2>/dev/null)" || return 1
    [[ -z "$ve_heads" ]] && return 1

    # A green VE on <fix> itself, or on any on-main DESCENDANT of <fix>, proves
    # a fix-containing binary passed VE. `is_ancestor <fix> <ve_head>` is 0 when
    # ve_head == fix or ve_head is a descendant. We require ve_head be on main
    # too (ancestor of origin/main HEAD) so a stale/off-main green never counts.
    local ve_head
    while IFS= read -r ve_head; do
      [[ "$ve_head" =~ ^[0-9a-f]{40}$ ]] || continue
      if git merge-base --is-ancestor "$fix_sha" "$ve_head" 2>/dev/null \
         && git merge-base --is-ancestor "$ve_head" origin/main 2>/dev/null; then
        return 0
      fi
    done <<< "$ve_heads"

    return 1
  }
fi

# ─────────────────────────────────────────────────────────────────────────────
# check_quarantine_active QUARANTINE_FILE
#
# Read-only + git subprocess. Determines if any quarantined SHA's *content*
# is still present in origin/main HEAD. Fail-closed on file-unreadable and
# git errors.
#
# Semantics differ from a pure ancestry check: a quarantined SHA only blocks
# deploy if its diff is still applied to origin/main. Once the offending
# changes are reverted, refactored away, or otherwise no longer present at
# the same lines, the gate auto-clears for that entry. This means a normal
# `git revert` (which adds a revert commit but leaves the bad SHA in
# ancestry) unblocks deploys without operator intervention.
#
# Algorithm per entry:
#   1. If SHA is NOT an ancestor of origin/main → not deployed, skip.
#   1b. hold:until-#<N> sentinel (#3711), checked BEFORE steps 2 and 3
#      (numbered 1b so the long-standing step-2/step-3 references elsewhere
#      stay valid): if the entry's reason carries `hold:until-#<N>`, query the
#      issue state (`timeout 15 gh issue view <N> --json state -q .state`).
#      Only a confirmed `CLOSED` releases the sentinel — the entry then falls
#      through to steps 2/3 as if the token were absent. EVERY other outcome
#      (issue OPEN, empty output, gh error, timeout, offline) FAILS CLOSED:
#      QUARANTINE_STATUS=blocked_active / QUARANTINED_MATCH=<sha>, exactly
#      like an active content match. Rationale: the step-3 content-check
#      backstop decays as main evolves (once every hunk drifts it
#      false-clears), which is the wrong semantics for "hold all deploys
#      until an open issue is fixed" (#3702's hold false-cleared this way).
#   2. resolved-SHA auto-clear (#3256, VE-green-gated for #3632): if the entry
#      carries a valid `resolved:<fix-sha>` token, the fix SHA is NOT the
#      entry's own SHA (self-resolution guard), that fix SHA is an ancestor of
#      origin/main (`git merge-base --is-ancestor` → 0, i.e. the fix MERGED),
#      AND the fix is VE-green (`quarantine_resolved_is_ve_green` → 0: a
#      fix-containing sha has a `success` `Verify Execution (Mainnet)` run, so
#      it is an actual deploy target), the fix has genuinely landed AND is
#      deployable → CLEAR this entry (skip the content-check). This is the ONLY
#      way an annotated bundled-commit quarantine auto-clears while benign hunks
#      of the same commit are intentionally retained (the tick-199 false-block:
#      #3238 bundled a harmful `retain:false` hunk removed by #3251 with benign
#      hunks kept on main; the per-hunk check below stays BLOCKED forever
#      because the benign hunks legitimately reverse-apply). An operator/
#      automation stamps `resolved:` once; the gate auto-clears once the fix is
#      both merged and VE-green.
#      WHY VE-GREEN (#3632): the deploy gate ships the LATEST VE-green sha, not
#      origin HEAD. On a busy main a fix MERGES long before the daily VE cron
#      validates it; in that window the latest VE-green sha is still a PRE-FIX
#      commit, so clearing on merge alone let the gate ship the pre-fix binary —
#      the exact deploy the hold existed to prevent.
#      FAIL-SAFE: a missing token, self-resolution, a not-yet-merged fix
#      (`--is-ancestor` → 1), a git error on the resolved-ancestry check
#      (rc>=128), OR a merged-but-not-yet-VE-green fix all FALL THROUGH to the
#      per-hunk content-check below — the resolved path can only CLEAR, never
#      relax the existing backstop.
#   3. If the entry was not auto-cleared by step 2 → split its diff
#      (`git diff sha^..sha`) into per-hunk patches and reverse-apply-check
#      EACH hunk independently (`_quarantine_any_hunk_present`). The entry is
#      ACTIVE (BLOCK) if ANY hunk still reverse-applies — i.e. at least one
#      harmful hunk's content is still in the tree. It CLEARs only when ALL
#      hunks fail to reverse-apply (genuinely reverted/removed). This is the
#      byte-for-byte #3253 backstop for every un-annotated entry.
#
#      Why per-hunk and not whole-commit (the #3248 fix): `git apply --check`
#      is all-or-nothing, so the old whole-commit reverse-apply false-CLEARED
#      whenever a LATER unrelated commit drifted the context of ANY one hunk
#      of a multi-file quarantined commit — even though the harmful hunk was
#      fully intact. Per-hunk isolates each hunk's presence. Per-FILE is
#      insufficient (same-file drift still false-clears all-or-nothing).
#   4. Hard git errors (rc>=128 from merge-base, missing parent), an empty or
#      unparseable diff, or a non-empty diff yielding zero hunks all FAIL-SAFE
#      toward BLOCK (a false block delays a deploy; a false clear re-crashes
#      the validator).
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#
# Sets globals:
#   QUARANTINE_STATUS  - Machine-readable: blocked_unreadable | blocked_active
#                        | blocked_git_error | clear
#   QUARANTINED_MATCH  - Matched SHA, "UNREADABLE", or "" (empty if clear)
#   QUARANTINE_WARNINGS - Accumulated warnings from parse + checks
#
# Returns:
#   0 — quarantined (deploy should be blocked)
#   1 — clear (no quarantine match, safe to proceed)
# ─────────────────────────────────────────────────────────────────────────────
check_quarantine_active() {
  local file="$1"
  QUARANTINE_STATUS="clear"
  QUARANTINED_MATCH=""

  parse_quarantine_file "$file"
  local parse_rc=$?

  # File unreadable: fail-closed
  if [[ "$parse_rc" -eq 1 ]]; then
    QUARANTINE_STATUS="blocked_unreadable"
    QUARANTINED_MATCH="UNREADABLE"
    return 0
  fi

  # No entries: clear
  if [[ -z "$QUARANTINE_ENTRIES" ]]; then
    QUARANTINE_STATUS="clear"
    return 1
  fi

  # Iterate QUARANTINE_ENTRIES and the index-aligned QUARANTINE_RESOLVED /
  # QUARANTINE_REASONS in lockstep over three FDs. `read -r ... <&3 && read -r
  # ... <&4 && read -r ... <&5` advances all streams one line per loop and
  # behaves identically under bash and zsh (same idiom as quarantine_autostamp).
  local sha resolved reason merge_base_rc present_rc resolved_rc
  local hold_issue hold_state
  while IFS= read -r sha <&3 && IFS= read -r resolved <&4 && IFS= read -r reason <&5; do
    [[ -z "$sha" ]] && continue

    # Step 1: ancestry check. If SHA isn't reachable, its content can't
    # be in origin/main HEAD → skip.
    merge_base_rc=0
    git merge-base --is-ancestor "$sha" origin/main 2>/dev/null || merge_base_rc=$?

    if [[ "$merge_base_rc" -ge 128 ]]; then
      # Git error on ancestry check — fail-closed
      local warning="ancestry-check-error: ${sha:0:8} (rc=$merge_base_rc)"
      if [[ -n "$QUARANTINE_WARNINGS" ]]; then
        QUARANTINE_WARNINGS+=", $warning"
      else
        QUARANTINE_WARNINGS="$warning"
      fi
      QUARANTINE_STATUS="blocked_git_error"
      QUARANTINED_MATCH="$sha"
      return 0
    fi
    if [[ "$merge_base_rc" -eq 1 ]]; then
      # Not in ancestry — content cannot be present. Skip.
      continue
    fi

    # Step 1b: hold:until-#<N> sentinel (#3711). A hold entry is pinned to the
    # lifecycle of GitHub issue #N, NOT to the textual presence of the SHA's
    # diff — the step-3 content-check backstop DECAYS as main evolves (once
    # every hunk of the quarantined commit drifts, it false-clears even though
    # the behavioral regression the hold protects against is still open). Only
    # a confirmed CLOSED issue releases the sentinel; every uncertain outcome
    # (issue OPEN, empty output, gh error, timeout, offline) FAILS CLOSED to
    # blocked_active, exactly like an active content match. Token extraction
    # uses grep (not the `=~` capture array) so it is identical under bash and
    # zsh — zsh populates `$match`, not `$BASH_REMATCH` (#3256). `|| true`
    # keeps the normal no-token case from tripping a caller's set -e/pipefail.
    hold_issue="$(printf '%s' " $reason " | grep -oE 'hold:until-#[0-9]+' | head -n1 || true)"
    if [[ -n "$hold_issue" ]]; then
      hold_issue="${hold_issue#hold:until-#}"
      # `timeout 15` bounds a hung gh/network call. Any failure path (non-zero
      # rc, no output) leaves hold_state empty → NOT "CLOSED" → fail-closed.
      hold_state="$(timeout 15 gh issue view "$hold_issue" \
                      --repo "${QUARANTINE_GH_REPO:-stellar-experimental/henyey}" \
                      --json state -q .state 2>/dev/null)" || hold_state=""
      if [[ "$hold_state" != "CLOSED" ]]; then
        # Issue still open OR state indeterminate → BLOCK (fail-closed).
        local warning="hold-until-active: ${sha:0:8} (#${hold_issue} state=${hold_state:-unknown})"
        if [[ -n "$QUARANTINE_WARNINGS" ]]; then
          QUARANTINE_WARNINGS+=", $warning"
        else
          QUARANTINE_WARNINGS="$warning"
        fi
        QUARANTINE_STATUS="blocked_active"
        QUARANTINED_MATCH="$sha"
        return 0
      fi
      # Issue is CLOSED → sentinel released. Fall through to the resolved-token
      # (step 2) and per-hunk content-check (step 3) logic for this entry.
    fi

    # Step 2: resolved-SHA auto-clear (#3256, tightened for #3632). If this
    # entry carries a valid `resolved:<fix-sha>` that is NOT its own SHA
    # (self-resolution guard), the fix SHA is an ancestor of origin/main (the
    # fix has MERGED), AND the fix is VE-green (a fix-containing sha has passed
    # `Verify Execution (Mainnet)` → is an actual deploy target) → CLEAR this
    # entry. Every uncertain case (no token, self-resolution, not-yet-merged,
    # git error, OR merged-but-not-yet-VE-green) FALLS THROUGH to the per-hunk
    # content-check below — the resolved path can only clear, never weaken the
    # backstop.
    #
    # WHY VE-GREEN AND NOT MERGE (#3632): the deploy gate ships the LATEST
    # VE-green sha (`select_latest_green_deploy_target`), not origin HEAD. On a
    # busy main the fix MERGES long before the daily VE cron validates it; in
    # that window the latest VE-green sha is still a PRE-FIX commit. Clearing on
    # merge alone let the gate ship that pre-fix binary — the exact deploy the
    # hold existed to prevent. Requiring VE-green mirrors the selector's own
    # notion of "deployable", so the quarantine lifts only once a fix-containing
    # sha is actually a deploy target.
    # Defense-in-depth (#3708): a MANUAL-CLEAR-ONLY entry NEVER auto-clears via
    # the resolved token — even a legacy/manual `resolved:` stamp must not
    # false-clear it. Skip step 2 entirely and fall through to the per-hunk
    # content-check (step 3), parallel to how a hold:until entry is honored in
    # both quarantine_autostamp and here. This is an isolated block: it only
    # suppresses the step-2 clear for such entries; the resolved-token logic is
    # otherwise unchanged.
    if _quarantine_is_manual_clear_only "$reason"; then
      : # manual-clear-only → do not auto-clear; defer to the content-check
    elif [[ "$resolved" =~ ^[0-9a-f]{40}$ && "$resolved" != "$sha" ]]; then
      resolved_rc=0
      git merge-base --is-ancestor "$resolved" origin/main 2>/dev/null || resolved_rc=$?
      if [[ "$resolved_rc" -eq 0 ]] && quarantine_resolved_is_ve_green "$resolved"; then
        # Fix commit is on main AND VE-green → the quarantine is resolved →
        # CLEAR. (Merged-but-not-yet-VE-green falls through to BLOCK below.)
        continue
      fi
      # resolved_rc == 1 (fix not yet merged), rc>=128 (git error), OR merged
      # but not yet VE-green (#3632): do NOT clear — fall through to the
      # per-hunk content-check (fail-closed).
    fi

    # Step 3: per-hunk content check. The SHA is an ancestor; ask whether ANY
    # hunk of its diff still reverse-applies (harmful content present). rc 0 →
    # at least one hunk present → BLOCK. rc 1 → all hunks gone → CLEAR for this
    # entry. rc 2 → fail-safe (diff error / empty / zero hunks) → BLOCK.
    present_rc=0
    _quarantine_any_hunk_present "$sha" || present_rc=$?

    if [[ "$present_rc" -eq 0 ]]; then
      # At least one hunk's harmful content is still present → BLOCK.
      QUARANTINE_STATUS="blocked_active"
      QUARANTINED_MATCH="$sha"
      return 0
    fi
    if [[ "$present_rc" -ge 2 ]]; then
      # Fail-safe: could not confidently determine absence → BLOCK.
      local warning="content-check-indeterminate: ${sha:0:8} (rc=$present_rc)"
      if [[ -n "$QUARANTINE_WARNINGS" ]]; then
        QUARANTINE_WARNINGS+=", $warning"
      else
        QUARANTINE_WARNINGS="$warning"
      fi
      QUARANTINE_STATUS="blocked_active"
      QUARANTINED_MATCH="$sha"
      return 0
    fi
    # present_rc == 1: every hunk failed to reverse-apply → content removed. Skip.
  done 3<<< "$QUARANTINE_ENTRIES" 4<<< "$QUARANTINE_RESOLVED" 5<<< "$QUARANTINE_REASONS"

  # No active match
  QUARANTINE_STATUS="clear"
  return 1
}

# Backward-compat alias: monitor-tick previously called
# `check_quarantine_ancestry`. The new content-aware check is a strict
# improvement (only blocks when the bad commit's diff is still applied),
# so the alias points at the new function. Existing call sites continue
# to work without modification.
check_quarantine_ancestry() {
  check_quarantine_active "$@"
}

# ─────────────────────────────────────────────────────────────────────────────
# quarantine_append QUARANTINE_FILE SHA REASON
#
# I/O: creates/appends to the quarantine file. Idempotent — does not add
# duplicate entries.
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#   SHA             - 40 lowercase hex chars
#   REASON          - Optional free-text reason (sanitized to single printable line)
#
# Returns:
#   0 — appended successfully or already present (no-op)
#   1 — invalid SHA format
#   2 — I/O error (mkdir, read, or write failure)
# ─────────────────────────────────────────────────────────────────────────────
quarantine_append() {
  local file="$1" sha="$2" reason="${3:-}"

  # Validate SHA
  if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    return 1
  fi

  # Sanitize reason: strip control chars, truncate
  if [[ -n "$reason" ]]; then
    reason=$(printf '%s' "$reason" | tr -d '\n\t\r' | tr -cd '[:print:]')
    reason="${reason:0:200}"
  fi

  # Ensure parent directory exists
  local dir
  dir=$(dirname "$file")
  if ! mkdir -p "$dir" 2>/dev/null; then
    return 2
  fi

  # If file exists, check for duplicate
  if [[ -e "$file" ]]; then
    if [[ ! -r "$file" ]]; then
      # Cannot verify idempotency — fail
      return 2
    fi
    if awk -v sha="$sha" '$1 == sha { found=1; exit } END { exit !found }' "$file" 2>/dev/null; then
      # Already present
      return 0
    fi
  fi

  # Append entry
  if [[ -n "$reason" ]]; then
    printf '%s %s\n' "$sha" "$reason" >> "$file" 2>/dev/null || return 2
  else
    printf '%s\n' "$sha" >> "$file" 2>/dev/null || return 2
  fi

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# quarantine_remove QUARANTINE_FILE SHA
#
# I/O: atomically removes ALL entries matching SHA from the quarantine file.
# Idempotent — returns success if SHA is not present or file is missing.
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#   SHA             - 40 lowercase hex chars
#
# Returns:
#   0 — removed or not present (including missing file)
#   1 — invalid SHA format
#   2 — I/O error (read, awk, or mv failure)
# ─────────────────────────────────────────────────────────────────────────────
quarantine_remove() {
  local file="$1" sha="$2"

  # Validate SHA
  if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    return 1
  fi

  # Missing file: nothing to remove
  if [[ ! -e "$file" ]]; then
    return 0
  fi

  # Unreadable file: cannot safely modify
  if [[ ! -r "$file" ]]; then
    return 2
  fi

  # Atomic removal via tmp+mv
  local tmpfile="${file}.tmp"
  if ! awk -v sha="$sha" '$1 != sha' "$file" > "$tmpfile" 2>/dev/null; then
    rm -f "$tmpfile" 2>/dev/null
    return 2
  fi

  if ! mv "$tmpfile" "$file" 2>/dev/null; then
    rm -f "$tmpfile" 2>/dev/null
    return 2
  fi

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# quarantine_resolve QUARANTINE_FILE SHA RESOLVED_SHA
#
# I/O: atomically stamps a `resolved:<RESOLVED_SHA>` token onto EVERY entry
# whose first field is SHA. This records the fix commit that resolves the
# quarantine; check_quarantine_active auto-clears the entry once RESOLVED_SHA
# is an ancestor of origin/main (#3256). Idempotent: any pre-existing
# `resolved:<...>` token on a matching entry is replaced (canonicalized to a
# single token), so repeated calls converge. Non-matching entries are left
# byte-for-byte unchanged. Mirrors quarantine_remove's atomic tmp+mv posture.
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#   SHA             - 40 lowercase hex chars (the quarantined commit)
#   RESOLVED_SHA    - 40 lowercase hex chars (the fix commit)
#
# Returns:
#   0 — stamped, already stamped (no-op), SHA not present, or missing file
#   1 — invalid SHA format (either argument), or self-resolution
#       (SHA == RESOLVED_SHA — rejected; would defeat the gate)
#   2 — I/O error (read, awk, or mv failure)
# ─────────────────────────────────────────────────────────────────────────────
quarantine_resolve() {
  local file="$1" sha="$2" resolved="$3"

  # Validate both SHAs.
  if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
    return 1
  fi
  if [[ ! "$resolved" =~ ^[0-9a-f]{40}$ ]]; then
    return 1
  fi
  # Self-resolution guard: a commit cannot resolve itself (it is always its own
  # ancestor, which would permanently auto-clear the gate). Reject up front.
  if [[ "$sha" == "$resolved" ]]; then
    return 1
  fi

  # Missing file: nothing to stamp.
  if [[ ! -e "$file" ]]; then
    return 0
  fi

  # Unreadable file: cannot safely modify.
  if [[ ! -r "$file" ]]; then
    return 2
  fi

  # Atomic rewrite via tmp+mv. For every line whose first field == sha, strip
  # any existing `resolved:<40hex>` token(s) and append the canonical one. The
  # token boundary (leading space + trailing word-boundary) mirrors the parse
  # regex so we never mangle a free-text reason.
  local tmpfile="${file}.tmp"
  if ! awk -v sha="$sha" -v resolved="$resolved" '
    {
      if ($1 == sha) {
        # Remove any prior resolved:<40hex> token (boundary-anchored).
        gsub(/[ \t]resolved:[0-9a-f]{40}([ \t]|$)/, " ", $0)
        sub(/[ \t]+$/, "", $0)
        print $0 " resolved:" resolved
      } else {
        print $0
      }
    }
  ' "$file" > "$tmpfile" 2>/dev/null; then
    rm -f "$tmpfile" 2>/dev/null
    return 2
  fi

  if ! mv "$tmpfile" "$file" 2>/dev/null; then
    rm -f "$tmpfile" 2>/dev/null
    return 2
  fi

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_issue_in_reason REASON
#
# Read-only, pure (no subprocess). Echoes the FIRST GitHub issue number `#N`
# found in REASON, or nothing if there is none. The crash issue # is recorded
# by monitor-tick §3d as `regression #<issue>` in the entry's reason; #3260's
# parser preserves arbitrary reason text. The parse EXCLUDES any `resolved:`
# token span first (Critic A on #3258): a `resolved:<40-hex>` token contains no
# `#`, so a hex digit-run can never be mis-read as an issue#, but we strip the
# token span explicitly so the contract holds even if the token convention
# changes. A single grep (no `=~` capture array, so the extraction is identical
# under bash and zsh — zsh populates `$match`, not `$BASH_REMATCH`).
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_issue_in_reason() {
  local reason="$1"
  # Excise any resolved:<40-hex> token span (boundary-anchored), so we never
  # scan inside it for a `#N`. Replace with a space to keep word boundaries.
  reason="$(printf '%s' " $reason " | sed -E 's/[[:space:]]resolved:[0-9a-f]{40}([[:space:]])/ \1/g')"
  # First `#<digits>` occurrence. `grep -oE` + head emits just the matched
  # token; strip the leading `#`. `|| true` keeps a no-match (grep rc=1) from
  # tripping a caller's set -e/pipefail.
  local tok
  tok="$(printf '%s' "$reason" | grep -oE '#[0-9]+' | head -n1 || true)"
  [[ -n "$tok" ]] && printf '%s' "${tok#\#}"
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_is_manual_clear_only REASON   (issue #3708)
#
# Read-only, pure (no subprocess). Returns 0 iff REASON carries the literal,
# boundary-anchored `MANUAL-CLEAR-ONLY` marker; non-zero otherwise. The match is
# CASE-SENSITIVE and exact — operators must use the precise token so a lowercase
# or paraphrased mention in prose never trips it. The token pins the entry to
# operator-only lifecycle: quarantine_autostamp never stamps `resolved:` onto it
# and check_quarantine_active never auto-clears it via the resolved token. Uses
# the same padded-space + grep boundary idiom as the resolved:/hold: tokens so
# the extraction is identical under bash and zsh (#3256).
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_is_manual_clear_only() {
  local reason="$1"
  printf '%s' " $reason " | grep -qE '[[:space:]]MANUAL-CLEAR-ONLY[[:space:]]'
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_distinct_issue_ref_count REASON   (issue #3708)
#
# Read-only, pure (no subprocess beyond grep/sort/wc). Echoes the count of
# DISTINCT `#N` issue refs in REASON, after excising any `resolved:<40-hex>`
# token span (reusing _quarantine_issue_in_reason's excision so a fix SHA is
# never scanned for a `#N`). quarantine_autostamp fails closed when this is >1:
# a reason bundling sibling/family refs (`#3582/#3702 family`) is ambiguous
# about WHICH ref is the blocker, so a closed+merged sibling must never stamp
# `resolved:` over a still-open real blocker — stamp only when there is exactly
# one issue ref (#3708). `|| true` keeps grep's no-match rc=1 from tripping a
# caller's pipefail; wc counts the newline-terminated matches (0 on no match).
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_distinct_issue_ref_count() {
  local reason="$1"
  reason="$(printf '%s' " $reason " | sed -E 's/[[:space:]]resolved:[0-9a-f]{40}([[:space:]])/ \1/g')"
  printf '%s' "$reason" \
    | { grep -oE '#[0-9]+' || true; } \
    | sort -u | wc -l | tr -d '[:space:]'
}

# ─────────────────────────────────────────────────────────────────────────────
# _quarantine_fix_sha_for_issue ISSUE_NUMBER
#
# gh subprocess (best-effort, read-only). Resolves the merge-commit SHA of the
# MERGED PR that closed ISSUE_NUMBER, via GitHub's STRUCTURED linkage — NOT
# free-text commit-message scanning:
#   1. `gh issue view N --json closedByPullRequestsReferences` → the PR numbers
#      GitHub authoritatively records as closing issue N (issue-scoped: immune
#      to any global PR-list recency window — Critic A on #3258).
#   2. For each such PR, `gh pr view P --json state,mergeCommit` and take
#      `.mergeCommit.oid` ONLY when `.state == "MERGED"` (an open/draft PR has
#      a null mergeCommit and must never stamp — fail-safe).
# Echoes the first valid 40-hex merge SHA found, or nothing. Any `gh`/network/
# parse failure → echoes nothing (caller skips → no stamp → #3260's gate
# governs). The `--jq` shapes here are EXACTLY what the production code consumes
# and what the snippet-test `gh` mock must mirror (Critic A).
# ─────────────────────────────────────────────────────────────────────────────
_quarantine_fix_sha_for_issue() {
  local issue="$1"
  local repo="${QUARANTINE_GH_REPO:-stellar-experimental/henyey}"
  local prs pr sha

  # Issue-side linkage: the PR numbers that close this issue (one per line).
  prs="$(gh issue view "$issue" --repo "$repo" \
           --json closedByPullRequestsReferences \
           --jq '.closedByPullRequestsReferences[]?.number' 2>/dev/null)" || return 0
  [[ -z "$prs" ]] && return 0

  while IFS= read -r pr; do
    [[ -z "$pr" ]] && continue
    # Merge SHA, ONLY when the PR is MERGED (else --jq emits nothing).
    sha="$(gh pr view "$pr" --repo "$repo" \
             --json state,mergeCommit \
             --jq 'select(.state=="MERGED") | .mergeCommit.oid' 2>/dev/null)" || continue
    if [[ "$sha" =~ ^[0-9a-f]{40}$ ]]; then
      printf '%s' "$sha"
      return 0
    fi
  done <<< "$prs"

  return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# quarantine_autostamp QUARANTINE_FILE
#
# Best-effort, full-autonomy auto-stamping (#3258). For each entry NOT already
# carrying a `resolved:` token AND NOT carrying a `hold:until-#<N>` sentinel
# (manual-lifecycle-only, #3711 — see below), follow GitHub's structured
# linkage to the merge-commit SHA of the MERGED PR that closed the entry's
# recorded crash issue (`regression #N`, written by monitor-tick §3d), and
# stamp it via the existing #3260 `quarantine_resolve`.
#
# This function is WRITE-ONLY and STRICTLY MONOTONIC: it can only ADD a
# `resolved:` token (delegating to quarantine_resolve, which is atomic tmp+mv,
# idempotent, and self-resolution-guarded). It NEVER removes or weakens an
# entry, and it contains NO clear logic. The SOLE clear authority remains
# `check_quarantine_active` (#3260), which independently re-gates every stamp
# with `git merge-base --is-ancestor <resolved> origin/main`. Therefore a
# wrong / unmerged / premature stamp is still BLOCKED by that ancestor gate,
# and a missing stamp leaves the per-hunk content-check in charge — a
# catastrophic auto-clear of a still-harmful tree is unreachable by
# construction. `check_quarantine_active` is intentionally NOT modified.
#
# FAIL-OPEN toward NOT-stamping: a missing/unparseable issue #, no closing PR,
# an open (non-MERGED) PR, a self-resolution, or ANY gh/network/parse error
# all skip that entry (leave it un-stamped). The function never aborts the tick.
#
# Call this ONCE PER TICK, BEFORE check_quarantine_active.
#
# Arguments:
#   QUARANTINE_FILE - Path to deploy_quarantine.txt
#
# Env (optional):
#   QUARANTINE_GH_REPO - owner/name for gh queries (default
#                        stellar-experimental/henyey)
#
# Returns:
#   0 — always (best-effort; per-entry failures are silently skipped). Never
#       aborts; never propagates a gh/parse error.
# ─────────────────────────────────────────────────────────────────────────────
quarantine_autostamp() {
  local file="$1"

  # Missing/empty/unreadable file: nothing to stamp. (check_quarantine_active
  # independently handles the unreadable case as fail-closed BLOCK.)
  [[ -e "$file" && -s "$file" && -r "$file" ]] || return 0

  parse_quarantine_file "$file" || return 0
  [[ -z "$QUARANTINE_ENTRIES" ]] && return 0

  # Iterate the three index-aligned globals in lockstep over three FDs. The
  # `read <&3 && read <&4 && read <&5` pattern advances all streams one line
  # per loop and behaves identically under bash and zsh (#3256).
  local sha resolved reason issue fix_sha
  while IFS= read -r sha <&3 && IFS= read -r resolved <&4 && IFS= read -r reason <&5; do
    [[ -z "$sha" ]] && continue
    # Already stamped → leave untouched, do NOT query gh (idempotent + cheap).
    [[ "$resolved" != "-" ]] && continue

    # hold:until-#<N> sentinel entries are MANUAL-LIFECYCLE-ONLY (#3711):
    # never stamp `resolved:` onto them. The `#N` in a hold token is the
    # ISSUE THE HOLD WAITS ON, not a crash regression whose merged closing PR
    # should lift the gate — stamping it would recreate the #3708
    # bundled-`#N` false-clear class on exactly the entries that must not
    # auto-clear. check_quarantine_active releases the hold only once the
    # issue is CLOSED; the operator removes the entry.
    if printf '%s' " $reason " | grep -qE 'hold:until-#[0-9]+'; then
      continue
    fi

    # #3708 fail-closed guards against false-clearing on a merged SIBLING issue.
    # A reason may bundle several refs (`#3582/#3702 family`) or be flagged
    # operator-only; in either case a closed+merged sibling must NOT stamp
    # `resolved:` over a still-open real blocker:
    #   (1) MANUAL-CLEAR-ONLY means "never auto-stamp" — the operator lifts the
    #       entry by hand once the real blocker is fixed.
    #   (2) more than one DISTINCT `#N` is ambiguous about which ref is the
    #       blocker — stamp only when there is exactly one issue ref.
    # Both are strictly ADDITIONAL skips (never a new clear): the entry stays
    # governed by check_quarantine_active's per-hunk content-check + manual
    # removal, exactly like the hold:until manual-lifecycle skip above.
    if _quarantine_is_manual_clear_only "$reason"; then
      continue
    fi
    if [[ "$(_quarantine_distinct_issue_ref_count "$reason")" -gt 1 ]]; then
      continue
    fi

    # Recover the crash issue # from the reason. No `#N` → skip (the gate's
    # per-hunk content-check governs this entry).
    issue="$(_quarantine_issue_in_reason "$reason")"
    [[ -z "$issue" ]] && continue

    # Resolve the merged fix SHA via structured GitHub linkage (best-effort).
    fix_sha="$(_quarantine_fix_sha_for_issue "$issue")"
    # No merged closing PR, open PR, or any gh error → skip (no stamp).
    [[ "$fix_sha" =~ ^[0-9a-f]{40}$ ]] || continue
    # Self-resolution guard (also enforced by quarantine_resolve): a commit
    # cannot resolve itself. Skip rather than let quarantine_resolve reject.
    [[ "$fix_sha" == "$sha" ]] && continue

    # WRITE-ONLY stamp. Any rc from quarantine_resolve is non-fatal here —
    # best-effort; the gate still governs. `|| true` keeps set -e happy.
    quarantine_resolve "$file" "$sha" "$fix_sha" || true
  done 3<<< "$QUARANTINE_ENTRIES" 4<<< "$QUARANTINE_RESOLVED" 5<<< "$QUARANTINE_REASONS"

  return 0
}
