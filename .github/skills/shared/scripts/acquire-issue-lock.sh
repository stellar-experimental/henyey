#!/usr/bin/env bash
# acquire-issue-lock.sh — host-local, race-free issue acquisition for /project-tick.
#
# Usage:
#   acquire-issue-lock.sh <issue_number> <status>
#
# Single source of truth for /project-tick Step 4 (mirrors the bounce-cap-check.sh
# extraction pattern: the SKILL.md prose invokes this script, it does NOT
# re-describe the algorithm). See issue #2917.
#
# WHY THIS EXISTS (#2917)
# -----------------------
# The previous Step-4 guard posted a sentinel comment, slept a grace window, then
# decided the winner by scanning sentinel comments filtered to a 60-second window:
#     select(.created_at | fromdate > (now - 60))
# A sentinel older than 60s was invisible to a LATER tick's check, so the later
# tick declared itself winner and dispatched a DUPLICATE specialist while the
# original was still running (review-pr can run 40+ min). Result: orphaned
# reviewer, head-scoped bounce double-increment risk, ready-for-doing queue
# starvation, and a leaked 24G in-repo worktree (PR #2914 incident).
#
# FIX
# ---
# Replace the time-window predicate with OS-enforced mutual exclusion: a
# non-blocking `flock -n` on a per-issue lockfile under a HOST-STABLE namespace
# in ~/data (keyed only on the host + issue number, NOT the per-process session
# — see #2936), held on an FD
# owned by the LONG-LIVED tick process (TICK_PID) for the entire dispatch
# lifetime. A concurrent live tick's flock fails immediately → exit 1 (kernel-
# atomic, race-free, independent of elapsed wall-time). The lock auto-releases
# when the FD closes on tick exit — so even a crashed/killed tick frees it with
# no manual reaping (which is why reap-on-override is cleanly separable into the
# follow-up #2934). The sentinel comment is retained as a board-visible audit
# artifact and best-effort cross-host signal only; the authoritative same-host
# guard is the flock.
#
# OUTPUT CONTRACT
# ---------------
# On success (exit 0): the FD that holds the lock is printed on stdout as
#   LOCK_FD=<n>
#   LOCK_PATH=<path>
#   SENTINEL_ID=<id>
# The caller (the tick process) MUST keep this FD open across Step-5 dispatch so
# the lock is held for the whole dispatch; it is released implicitly when the FD
# closes (Step 6, or on tick exit/crash). Because a sub-shell-scoped flock would
# release at the end of this script, this script does NOT take the lock in a
# sub-shell — it opens the FD in the calling shell via `exec {fd}>` so the FD
# survives into the caller when this file is *sourced*. When this file is *run*
# (not sourced) the FD lives only for the script's lifetime, which is fine for
# tests but the tick process should source it (see SKILL.md Step 4).
#
# Exit codes:
#   0  acquired — lock held, self-assigned @me, sentinel posted (SENTINEL_ID emitted)
#   1  back off — lock held by a live tick (or flock missing / preflight failed);
#                 no sentinel posted, nothing dispatched; a #2822 per-loop
#                 cooldown line is appended for the picker to skip this issue.
# ── Source-safe shell-option handling (#2934 FD-survival fix) ────────────────
# This file is `source`d by the tick process (see the termination note below),
# so a bare top-level `set -uo pipefail` would LEAK those options into the
# caller's shell and could abort an otherwise-fine tick on a later unset-var
# reference. Snapshot the caller's option state first, apply our options for the
# script body, and restore the caller's state on every termination path (in
# `_acq_end`). When RUN directly there is no caller to protect, so the snapshot
# is a harmless no-op.
_ACQ_SAVED_OPTS="$(set +o)"
set -uo pipefail

# ── Source-safe termination (#2934 FD-survival fix) ──────────────────────────
# The tick process MUST `source` this script (NOT run it in a `$(...)` command-
# substitution subshell) so the lock FD opened below via `exec {LOCK_FD}>` stays
# open in the caller's shell and the flock is held across Step-5 dispatch. A
# command-substitution subshell would close that FD the instant `$()` returns,
# silently releasing the lock and reintroducing #2917 — and worse, letting a
# second tick's reaper positively-identify and KILL the still-live first
# dispatch (the PR #2960 round-1 blocking defect). So this file is designed to
# be sourced. When sourced, a top-level `exit` would terminate the CALLER's
# shell, so every termination here goes through `_acq_end <rc>`, which `return`s
# when sourced (leaving the caller alive with LOCK_FD held) and `exit`s when run
# directly (the standalone/test path). It is detected once, at parse time.
case "${BASH_SOURCE[0]}" in
  "$0") _ACQ_SOURCED=0 ;;   # executed directly (bash acquire-issue-lock.sh ...)
  *)    _ACQ_SOURCED=1 ;;   # sourced (. acquire-issue-lock.sh ...)
esac
# `_acq_end <rc>` exits the process when this file was RUN directly. When it was
# SOURCED it is a no-op (a `return` inside a function only pops the function, not
# the sourced script), so every call site pairs it with a top-level
# `return <rc>` which IS valid in a sourced script and unwinds it cleanly:
#     _acq_end 1; return 1     # sourced: _acq_end no-ops, `return 1` unwinds
#                              # run:     _acq_end 1 exits, `return` never runs
_acq_end() {
  [ "$_ACQ_SOURCED" -eq 0 ] && exit "${1:-0}"
  # Sourced: restore the caller's original shell options so we don't leak
  # `set -u`/`pipefail` into the tick shell, then let the call site `return`.
  eval "$_ACQ_SAVED_OPTS" 2>/dev/null || true
  return 0
}

ISSUE="${1:?issue number required}"
STATUS="${2:?status required}"

OWNER="stellar-experimental"
REPO="henyey"

# ── Preflight: flock is mandatory; fail closed (exit 1 = back off) if absent ──
# Hosts lacking util-linux must NOT silently fall back to the racy time-window
# scheme — that would reintroduce #2917. Backing off is the safe choice.
if ! command -v flock >/dev/null 2>&1; then
  echo "ERROR: flock not available — cannot acquire host-local lock; backing off (#2917)" >&2
  _acq_end 1; return 1
fi

# ── Derive the lock path under the ~/data workspace contract root (#2843) ────
# Source the contract helper so the lock lives under <real-home>/data and is
# validated against the boundary (no $HOME-poisoning escape).
_SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The contract lib lives at repo-root/scripts/lib/agent-worktree-contract.sh;
# from .github/skills/shared/scripts that is ../../../../scripts/lib.
_CONTRACT_LIB="$_SELF_DIR/../../../../scripts/lib/agent-worktree-contract.sh"
if [ -f "$_CONTRACT_LIB" ]; then
  # shellcheck source=/dev/null
  . "$_CONTRACT_LIB"
fi

# ── Validate $ISSUE as a numeric issue id (#2936 review) ─────────────────────
# $ISSUE is interpolated into the lockfile path and passed to `gh`. Reject
# anything non-numeric to prevent path traversal / unexpected lockfile names.
if ! [[ "$ISSUE" =~ ^[0-9]+$ ]]; then
  echo "ERROR: issue id '$ISSUE' is not numeric; refusing to derive a lock path; backing off" >&2
  _acq_end 1; return 1
fi

# ── Lock namespace: host-stable and issue-scoped, NOT per-process (#2917) ────
# The lock identity MUST depend only on the host + $ISSUE — never on the
# per-process CLAUDE_SESSION_ID. Keying on CLAUDE_SESSION_ID was the defect
# flagged in the PR #2936 review: the real deployment topology is two distinct
# copilot processes (each with its own CLAUDE_SESSION_ID) contending for the
# same issue, so a session-keyed path made them lock DIFFERENT inodes and
# `flock` never serialized them — the duplicate-dispatch race stayed live.
#
# Use a fixed namespace ("project-tick") so all ticks/loops on the host share
# one lockfile per issue. PROJECT_TICK_LOCK_SESSION_ID overrides the namespace
# for TEST isolation only; it defaults to the constant and is never derived
# from the session. Real ticks leave it unset → they all share "project-tick".
LOCK_NAMESPACE="${PROJECT_TICK_LOCK_SESSION_ID:-project-tick}"

# Derive the contract root from the passwd-anchored real home (immune to
# $HOME poisoning) when the helper is available; otherwise fall back to $HOME.
if command -v _contract_real_home >/dev/null 2>&1; then
  _REAL_HOME="$(_contract_real_home)"
else
  _REAL_HOME="$HOME"
fi
LOCK_DIR="$_REAL_HOME/data/$LOCK_NAMESPACE/tick-locks"
LOCK_PATH="$LOCK_DIR/$ISSUE.lock"

# Validate the lock dir is inside the contract boundary when the helper is
# available. Fail closed on a boundary violation.
if command -v require_home_data_path >/dev/null 2>&1; then
  if ! require_home_data_path "$LOCK_DIR" "LOCK_DIR" >/dev/null; then
    echo "ERROR: lock dir '$LOCK_DIR' is outside the ~/data contract boundary; backing off" >&2
    exit 1
  fi
fi

mkdir -p "$LOCK_DIR" 2>/dev/null || true

# ── Take the non-blocking flock ──────────────────────────────────────────────
# Open the lockfile on a new FD in THIS shell (not a sub-shell). When the tick
# process sources this script, the FD survives into the tick process and the
# lock is held for the whole dispatch lifetime. `flock -n` returns non-zero
# immediately if another live process holds the lock.
exec {LOCK_FD}>"$LOCK_PATH"
if ! flock -n "$LOCK_FD"; then
  # A live tick already holds this issue's lock → back off (the #2917 guard).
  echo "race lost on #$ISSUE — live tick holds the lock; backing off" >&2
  exec {LOCK_FD}>&-
  # Record a 5-minute per-loop cooldown so the next tick from THIS loop skips
  # this issue and falls through to lower-priority actionable items (#2822).
  COOLDOWN_FILE="/tmp/project-tick-cooldown-${LOOP_PID:-default}"
  echo "$ISSUE $(( $(date +%s) + 300 ))" >> "$COOLDOWN_FILE"
  _acq_end 1; return 1
fi

# ── We hold the lock — reap any orphaned PRIOR dispatch for this issue (#2934).
# Holding the flock proves no other live LOCK-HOLDER exists; it does NOT by
# itself prove the prior dispatch's whole tree is gone. That is precisely the
# residual window: the prior tick (the lock holder) died and the kernel released
# its FD-scoped flock, but a specialist it had forked as a CHILD may still be
# running detached. The newest sentinel records that prior dispatch's process-
# group + leader start-time; reap re-verifies that identity against /proc and
# kills the group ONLY if the recorded leader is still positively ALIVE (same
# host + PGID + start-time match + signalable). A gone/reused/EPERM leader is
# never signalled — its workspace is still reclaimed. This runs BEFORE we
# overwrite the sentinel below (so reap reads the PRIOR owner's identity, not
# our own). Best-effort and non-fatal: a reap failure must never block dispatch.
_REAP_SCRIPT="$_SELF_DIR/reap-stale-dispatch.sh"
if [ -x "$_REAP_SCRIPT" ]; then
  "$_REAP_SCRIPT" "$ISSUE" || true
fi

# ── Self-assign @me (in-review picks included — #2909). ──────────────────────
# Self-assign is necessary so /project-tick filters us out of future picker
# runs. It was empty during the #2909 incident for an in-review pick, so we
# always assign regardless of $STATUS.
gh issue edit "$ISSUE" --repo "$OWNER/$REPO" --add-assignee @me >/dev/null 2>&1 || true

# ── Self-record THIS dispatch's process-group identity (#2934 / #2956). ──────
# The sentinel must carry an identity that reap-stale-dispatch.sh can later use
# to positively identify (and only then kill) THIS dispatch's orphaned process-
# group if we die with a surviving child. The robust source is THIS process's
# own /proc, NOT an env var handed down from the loop after the child launched
# (that env export can never reach an already-exec'd child — the round-2
# correctness concern, #2956). This script is sourced by the long-lived tick
# (the copilot dispatch), so /proc/self's pgid IS the dispatch group leader,
# provided the loop launched the dispatch as its own group leader via setsid
# (#2957). We record the leader's pid (== pgid) and its start-time (field 22 of
# /proc/<pgid>/stat, clock-ticks-since-boot — monotonic, not recycled in a boot)
# so the reaper can detect PID reuse and refuse to signal a recycled pid (#2958).
_self_pgid=""; _self_starttime=""
if _stat_line="$(cat /proc/self/stat 2>/dev/null)"; then
  # comm (field 2) may contain spaces/parens; strip through the final ") ".
  _stat_rest="${_stat_line##*) }"
  # shellcheck disable=SC2086
  set -- $_stat_rest
  # In the post-comm fields, pgrp is index 3, starttime is index 20.
  _self_pgid="${3:-}"
  _self_starttime="${20:-}"
fi
# Re-read the GROUP LEADER's start-time (the leader pid == _self_pgid). When the
# dispatch is its own group leader these are identical; reading the leader's own
# /proc entry is what the reaper compares against, so record that explicitly.
_leader_starttime="$_self_starttime"
if [ -n "$_self_pgid" ] && _ldr_line="$(cat "/proc/$_self_pgid/stat" 2>/dev/null)"; then
  _ldr_rest="${_ldr_line##*) }"
  # shellcheck disable=SC2086
  set -- $_ldr_rest
  _leader_starttime="${20:-$_self_starttime}"
fi

# ── Post the sentinel comment (best-effort cross-host audit signal only). ────
# The authoritative same-host guard is the flock above; the sentinel records
# host + posted time + the OWNING TICK_PID (the long-lived loop process — kept
# for backward-compat / cross-host kill-0 liveness) PLUS the per-dispatch
# process-group identity (dispatch_pgid + dispatch_starttime) used by the reaper.
OWNER_PID="${TICK_PID:-$$}"
TICK_ID="tick-$(date +%s%N)-$OWNER_PID"
SENTINEL_ID=$(gh api "repos/$OWNER/$REPO/issues/$ISSUE/comments" \
  --method POST \
  -f body="## 🔒 acquired-by:$TICK_ID

posted=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ), host=$(hostname), tick_pid=$OWNER_PID, dispatch_pgid=${_self_pgid:-}, dispatch_starttime=${_leader_starttime:-}" \
  --jq '.id' 2>/dev/null || true)

# Emit the held-lock FD + path + sentinel id for the caller to retain.
echo "LOCK_FD=$LOCK_FD"
echo "LOCK_PATH=$LOCK_PATH"
echo "SENTINEL_ID=${SENTINEL_ID:-}"
echo "TICK_ID=$TICK_ID"
echo "Won acquisition on #$ISSUE (flock held by tick_pid=$OWNER_PID). Proceeding." >&2
_acq_end 0; return 0
