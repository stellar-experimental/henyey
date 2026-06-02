#!/usr/bin/env bash
# reap-stale-dispatch.sh — reclaim a dead prior dispatch's orphaned process-group
# and ~/data workspace, invoked from acquire-issue-lock.sh once a tick wins the
# host-local flock for an issue. See issue #2934 (split from #2917 / PR #2936).
#
# WHY THIS EXISTS (#2934)
# ----------------------
# #2917's flock guard auto-releases the per-issue lock when the holding tick's
# FD closes — so a crashed/killed tick frees the lock with no manual reaping.
# But there is a residual window: if the tick PROCESS dies while a specialist it
# dispatched as a child survives the parent, the kernel releases the lock while
# the orphaned specialist keeps running. A new tick then acquires the lock and
# dispatches a SECOND specialist for the same issue (the #2917 duplicate-dispatch
# shape, triggered by parent death). This script closes that window: when a tick
# wins the lock, it reaps any orphaned PRIOR dispatch's process-group and its
# ~/data workspace before dispatching its own specialist.
#
# SAFETY MODEL (this script runs `kill` and `rm -rf` in the live pipeline)
# -----------------------------------------------------------------------
# Two independent positive-identity gates protect the destructive kill:
#
#   1. SAME HOST. The sentinel records host=<hostname>. We only signal when the
#      recorded host equals the current hostname. flock is host-local, so on a
#      multi-host fleet a cross-host sentinel owner cannot be our orphan — never
#      signal it. (Single-host is the live deployment; this is forward-proofing.)
#
#   2. PGID + START-TIME MATCH. The sentinel records the dead dispatch's
#      process-GROUP-leader pid (== pgid, because the launcher makes each
#      dispatch its own group leader via `setsid`) AND that leader's start-time
#      (/proc/<pid>/stat field 22, in clock ticks since boot — monotonic and
#      NOT recycled within a boot). Before signalling we re-read
#      /proc/<pgid>/stat:
#        - /proc entry ABSENT          ⇒ leader is GONE. The numeric PGID may
#          have been recycled to an unrelated live group, so we MUST NOT blind-
#          kill the bare numeric group. We SKIP the kill entirely and proceed
#          only to the workspace rm (which touches no live process). (#2958)
#        - start-time MISMATCH         ⇒ PID was reused by a DIFFERENT process.
#          The original dispatch is gone; the live process at that pid is some
#          unrelated program. SKIP the kill. (#2958)
#        - start-time MATCH, signalable ⇒ POSITIVELY the original dispatch
#          leader, still alive. This is the only case we kill the group.
#        - kill -0 returns EPERM        ⇒ a live process we don't own occupies
#          the pid (recycled to another user). Treat as alive/foreign → SKIP.
#
# In short: we kill ONLY a positively-verified-alive original dispatch group
# (host + pid + start-time all match). A gone-or-reused leader never triggers a
# kill — its workspace is still reclaimed (rm is guarded by require_home_data_path
# and cannot harm a live process).
#
# WORKSPACE REAP
# --------------
# The prior dispatch's scratch lives at ~/data/<session>/{plan,review-pr,do}-<issue>.
# We glob ~/data/*/{plan,review-pr,do}-<issue>, validate EACH candidate with
# require_home_data_path (refusing anything that does not canonicalize under
# <real-home>/data — e.g. a symlink escape), and `rm -rf` only the validated
# ones. No in-repo path is touched (the $REPO_ROOT/data/do-<issue> worktree
# stays with #2843, per Critic C scope cut).
#
# Best-effort and NON-FATAL throughout: every failure path returns 0 so a reap
# problem never blocks the winning tick from dispatching. The caller invokes us
# guarded (`|| true`).
#
# Usage:
#   reap-stale-dispatch.sh <issue_number>
#
# Test seams (env overrides, used ONLY by the unit tests; unset in production):
#   REAP_SENTINEL_FILE   — read sentinel body from this file instead of `gh api`.
#   REAP_HOSTNAME        — override the "current hostname" used for the host gate.
#   REAP_DRY_RUN=1       — log the kill/rm decisions but do not execute them.
#   REAP_LOG_FILE        — append a structured decision log here (for assertions).
set -uo pipefail

ISSUE="${1:?issue number required}"

# ── Validate $ISSUE as a numeric id (interpolated into globs) ────────────────
if ! [[ "$ISSUE" =~ ^[0-9]+$ ]]; then
  echo "reap: issue id '$ISSUE' is not numeric; refusing to reap" >&2
  exit 0
fi

OWNER="stellar-experimental"
REPO="henyey"

_SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_CONTRACT_LIB="$_SELF_DIR/../../../../scripts/lib/agent-worktree-contract.sh"
if [ -f "$_CONTRACT_LIB" ]; then
  # shellcheck source=/dev/null
  . "$_CONTRACT_LIB"
fi

# ── Structured decision logging (for tests + operational audit) ──────────────
_reap_log() {
  echo "reap[#$ISSUE]: $*" >&2
  if [ -n "${REAP_LOG_FILE:-}" ]; then
    printf '%s\n' "$*" >> "$REAP_LOG_FILE"
  fi
}

# ── Read the process-group-leader start-time from /proc, robustly ────────────
# /proc/<pid>/stat field 2 (comm) may contain spaces and parens, so we strip
# through the FINAL ") " before splitting. In the post-comm fields (1-indexed)
# pgrp is index 3 and starttime is index 20. Prints the start-time, or empty +
# return 1 if the entry is absent/unreadable.
_proc_starttime() {
  local pid="$1" line rest
  [ -r "/proc/$pid/stat" ] || return 1
  read -r line < "/proc/$pid/stat" || return 1
  rest="${line##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  [ -n "${20:-}" ] || return 1
  printf '%s\n' "${20}"
}

# ── Recover the prior dispatch identity from the newest sentinel ─────────────
# The sentinel body (posted by acquire-issue-lock.sh) contains a line:
#   posted=<ts>, host=<hostname>, tick_pid=<loop_pid>, dispatch_pgid=<pgid>, dispatch_starttime=<ticks>
# We select the MOST RECENT "## 🔒 acquired-by:" sentinel (the latest acquirer
# before us) and parse host/dispatch_pgid/dispatch_starttime from it.
get_sentinel_body() {
  if [ -n "${REAP_SENTINEL_FILE:-}" ]; then
    cat "$REAP_SENTINEL_FILE" 2>/dev/null
    return 0
  fi
  # Newest sentinel comment body (created_at descending → first match).
  gh api "repos/$OWNER/$REPO/issues/$ISSUE/comments" --paginate \
    --jq '[.[] | select(.body | startswith("## 🔒 acquired-by:"))]
          | sort_by(.created_at) | last | .body // empty' 2>/dev/null
}

SENTINEL_BODY="$(get_sentinel_body)"
if [ -z "$SENTINEL_BODY" ]; then
  _reap_log "no sentinel found — nothing to reap; proceeding"
  exit 0
fi

# Parse the identity fields. Missing/unparseable fields ⇒ we still attempt the
# workspace reap (safe) but skip the kill (no positive identity).
sentinel_host="$(printf '%s\n' "$SENTINEL_BODY" | grep -oE 'host=[^,[:space:]]+' | head -1 | cut -d= -f2)"
sentinel_pgid="$(printf '%s\n' "$SENTINEL_BODY" | grep -oE 'dispatch_pgid=[0-9]+' | head -1 | cut -d= -f2)"
sentinel_starttime="$(printf '%s\n' "$SENTINEL_BODY" | grep -oE 'dispatch_starttime=[0-9]+' | head -1 | cut -d= -f2)"

CURRENT_HOST="${REAP_HOSTNAME:-$(hostname)}"

# ── Kill gate: only signal a positively-verified-alive original dispatch ─────
maybe_kill_group() {
  if [ -z "$sentinel_pgid" ] || [ -z "$sentinel_starttime" ]; then
    _reap_log "skip-kill: sentinel lacks dispatch_pgid/dispatch_starttime (old-format or self-record absent)"
    return 0
  fi
  if ! [[ "$sentinel_pgid" =~ ^[0-9]+$ ]] || [ "$sentinel_pgid" -le 1 ]; then
    _reap_log "skip-kill: implausible dispatch_pgid='$sentinel_pgid'"
    return 0
  fi
  if [ "$sentinel_host" != "$CURRENT_HOST" ]; then
    _reap_log "skip-kill: cross-host (sentinel host='$sentinel_host' != current='$CURRENT_HOST')"
    return 0
  fi

  local live_starttime
  if ! live_starttime="$(_proc_starttime "$sentinel_pgid")"; then
    # Leader /proc entry absent ⇒ gone. The numeric PGID may have been recycled
    # to an unrelated live group; blind `kill -- -<pgid>` is unsafe. SKIP. (#2958)
    _reap_log "skip-kill: leader pgid=$sentinel_pgid gone (no /proc entry); not blind-killing a possibly-recycled group"
    return 0
  fi
  if [ "$live_starttime" != "$sentinel_starttime" ]; then
    # PID reused by a different process ⇒ original dispatch gone. SKIP. (#2958)
    _reap_log "skip-kill: pid=$sentinel_pgid reused (start-time $live_starttime != recorded $sentinel_starttime); not killing unrelated live process"
    return 0
  fi

  # start-time matches ⇒ positively the original dispatch leader. Confirm we may
  # signal it before touching the group. Probe the LEADER pid (not the group) so
  # we can read errno cleanly:
  #   rc 0     → alive and ours → kill the group.
  #   ESRCH    → leader exited between the start-time read and now → nothing to
  #              kill (the group is empty); skip (no-op, not an error).
  #   EPERM    → a live process we don't own holds the pid → foreign → skip.
  local kill0_err
  kill0_err="$(kill -0 "$sentinel_pgid" 2>&1)"
  local kill0_rc=$?
  if [ "$kill0_rc" -ne 0 ]; then
    case "$kill0_err" in
      *[Pp]ermission*|*EPERM*)
        _reap_log "skip-kill: pid=$sentinel_pgid is EPERM (foreign-owned live process); not signalling" ;;
      *)
        _reap_log "skip-kill: pid=$sentinel_pgid leader exited (ESRCH); group empty, nothing to kill" ;;
    esac
    return 0
  fi

  _reap_log "kill: process-group $sentinel_pgid (verified original dispatch leader, start-time matched)"
  if [ "${REAP_DRY_RUN:-0}" = "1" ]; then
    return 0
  fi
  # TERM the whole group, give it a moment, then KILL any survivors.
  kill -TERM -- "-$sentinel_pgid" 2>/dev/null || true
  local n=0
  while [ "$n" -lt 20 ] && kill -0 -- "-$sentinel_pgid" 2>/dev/null; do
    sleep 0.1; n=$((n + 1))
  done
  kill -KILL -- "-$sentinel_pgid" 2>/dev/null || true
  return 0
}

# ── Workspace reap: ~/data/*/{plan,review-pr,do}-<issue>, contract-guarded ───
reap_workspaces() {
  local real_home
  if command -v _contract_real_home >/dev/null 2>&1; then
    real_home="$(_contract_real_home)"
  else
    real_home="$HOME"
  fi

  local role candidate canonical
  shopt -s nullglob
  # NB: "do" is quoted so the parser does not read it as the loop's `do` keyword.
  for role in "plan" "review-pr" "do"; do
    for candidate in "$real_home"/data/*/"$role-$ISSUE"; do
      [ -e "$candidate" ] || continue
      if ! command -v require_home_data_path >/dev/null 2>&1; then
        _reap_log "skip-rm: contract helper unavailable; refusing to rm '$candidate'"
        continue
      fi
      if ! canonical="$(require_home_data_path "$candidate" "REAP_WORKSPACE" 2>/dev/null)"; then
        _reap_log "skip-rm: '$candidate' failed require_home_data_path (outside ~/data); refusing"
        continue
      fi
      _reap_log "rm: workspace '$canonical'"
      if [ "${REAP_DRY_RUN:-0}" = "1" ]; then
        continue
      fi
      rm -rf "$canonical" 2>/dev/null || _reap_log "warn: rm failed for '$canonical' (non-fatal)"
    done
  done
  shopt -u nullglob
}

maybe_kill_group
reap_workspaces
exit 0
