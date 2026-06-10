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
#     quarantine: once that fix SHA is an ancestor of origin/main, the entry
#     auto-clears (see check_quarantine_active). Additive — absent on legacy
#     entries, which keep the per-hunk content-check as their backstop.
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
      else
        QUARANTINE_ENTRIES="$sha"
        QUARANTINE_RESOLVED="$resolved"
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
#   2. resolved-SHA auto-clear (#3256): if the entry carries a valid
#      `resolved:<fix-sha>` token, the fix SHA is NOT the entry's own SHA
#      (self-resolution guard), and that fix SHA is an ancestor of origin/main
#      (`git merge-base --is-ancestor` → 0), the fix has genuinely landed →
#      CLEAR this entry (skip the content-check). This is the ONLY way an
#      annotated bundled-commit quarantine auto-clears while benign hunks of
#      the same commit are intentionally retained (the tick-199 false-block:
#      #3238 bundled a harmful `retain:false` hunk removed by #3251 with benign
#      hunks kept on main; the per-hunk check below stays BLOCKED forever
#      because the benign hunks legitimately reverse-apply). An operator/
#      automation stamps `resolved:` once; the gate auto-clears on merge.
#      FAIL-SAFE: a missing token, self-resolution, a not-yet-merged fix
#      (`--is-ancestor` → 1), OR a git error on the resolved-ancestry check
#      (rc>=128) all FALL THROUGH to the per-hunk content-check below — the
#      resolved path can only CLEAR, never relax the existing backstop.
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

  # Iterate QUARANTINE_ENTRIES and the index-aligned QUARANTINE_RESOLVED in
  # lockstep over two FDs. `read -r ... <&3 && read -r ... <&4` advances both
  # streams one line per loop and behaves identically under bash and zsh.
  local sha resolved merge_base_rc present_rc resolved_rc
  while IFS= read -r sha <&3 && IFS= read -r resolved <&4; do
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

    # Step 2: resolved-SHA auto-clear (#3256). If this entry carries a valid
    # `resolved:<fix-sha>` that is NOT its own SHA (self-resolution guard) and
    # the fix SHA is an ancestor of origin/main, the fix has genuinely landed
    # → CLEAR this entry. Every uncertain case (no token, self-resolution,
    # not-yet-merged, git error) FALLS THROUGH to the per-hunk content-check
    # below — the resolved path can only clear, never weaken the backstop.
    if [[ "$resolved" =~ ^[0-9a-f]{40}$ && "$resolved" != "$sha" ]]; then
      resolved_rc=0
      git merge-base --is-ancestor "$resolved" origin/main 2>/dev/null || resolved_rc=$?
      if [[ "$resolved_rc" -eq 0 ]]; then
        # Fix commit is on main → the quarantine is resolved → CLEAR.
        continue
      fi
      # resolved_rc == 1 (fix not yet merged) OR rc>=128 (git error): do NOT
      # clear — fall through to the per-hunk content-check (fail-closed).
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
  done 3<<< "$QUARANTINE_ENTRIES" 4<<< "$QUARANTINE_RESOLVED"

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
