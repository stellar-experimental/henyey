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
set -uo pipefail

ISSUE="${1:?issue number required}"
STATUS="${2:?status required}"

OWNER="stellar-experimental"
REPO="henyey"

# ── Preflight: flock is mandatory; fail closed (exit 1 = back off) if absent ──
# Hosts lacking util-linux must NOT silently fall back to the racy time-window
# scheme — that would reintroduce #2917. Backing off is the safe choice.
if ! command -v flock >/dev/null 2>&1; then
  echo "ERROR: flock not available — cannot acquire host-local lock; backing off (#2917)" >&2
  exit 1
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
  exit 1
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
  exit 1
fi

# ── We hold the lock. Self-assign @me (in-review picks included — #2909). ────
# Self-assign is necessary so /project-tick filters us out of future picker
# runs. It was empty during the #2909 incident for an in-review pick, so we
# always assign regardless of $STATUS.
gh issue edit "$ISSUE" --repo "$OWNER/$REPO" --add-assignee @me >/dev/null 2>&1 || true

# ── Post the sentinel comment (best-effort cross-host audit signal only). ────
# The authoritative same-host guard is the flock above; the sentinel records
# host + posted time + the OWNING TICK_PID (the long-lived process that holds
# the lock — NOT $$, which is this ephemeral shell that exits before dispatch).
OWNER_PID="${TICK_PID:-$$}"
TICK_ID="tick-$(date +%s%N)-$OWNER_PID"
SENTINEL_ID=$(gh api "repos/$OWNER/$REPO/issues/$ISSUE/comments" \
  --method POST \
  -f body="## 🔒 acquired-by:$TICK_ID

posted=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ), host=$(hostname), tick_pid=$OWNER_PID" \
  --jq '.id' 2>/dev/null || true)

# Emit the held-lock FD + path + sentinel id for the caller to retain.
echo "LOCK_FD=$LOCK_FD"
echo "LOCK_PATH=$LOCK_PATH"
echo "SENTINEL_ID=${SENTINEL_ID:-}"
echo "TICK_ID=$TICK_ID"
echo "Won acquisition on #$ISSUE (flock held by tick_pid=$OWNER_PID). Proceeding." >&2
exit 0
