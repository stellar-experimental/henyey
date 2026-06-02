#!/usr/bin/env bash
# test-acquire-issue-lock.sh — regression tests for acquire-issue-lock.sh,
# the host-local flock-based issue-acquisition guard introduced for issue #2917.
#
# Background (#2917): /project-tick's Step-4 acquisition used a sentinel-comment
# lock whose race check filtered comments to a 60-second window
# (`select(.created_at | fromdate > (now - 60))`). A sentinel older than 60s
# became invisible to a later tick's check, so the later tick declared itself
# winner and dispatched a DUPLICATE specialist while the original was still
# running (orphaned reviewer + head-scoped bounce double-increment + queue
# starvation + leaked 24G worktree — see issue #2917 / PR #2914 incident).
#
# The fix replaces the time-window predicate with OS-enforced mutual exclusion:
# a non-blocking `flock -n` on a per-issue lockfile under ~/data, held on an FD
# owned by the long-lived tick process for the full dispatch lifetime. A
# concurrent live tick's flock fails immediately → exit 1 (race-free).
#
# Tests (mocked `gh` via PATH, real `flock` on a real lockfile under ~/data):
#   1. test_flock_held_blocks_second_acquire
#        A live tick holds the issue's flock in this shell; a second acquire for
#        the same issue MUST exit 1, post no sentinel, dispatch nothing. This is
#        the exact #2917 incident — fails on origin/main (60s-window returns
#        "won"), passes post-fix (flock held for the whole dispatch regardless
#        of elapsed wall-time).
#   2. test_acquire_succeeds_when_lock_free
#        No competitor → exit 0, sentinel POSTed, `--add-assignee @me` invoked.
#   3. test_dead_holder_lock_is_acquirable
#        Take the flock in a subshell, kill it (FD closes → kernel releases the
#        lock), then acquire → exit 0. Proves auto-release-on-death: why no
#        manual reap is needed for prevention. Fails on main (no flock).
#   4. test_in_review_pick_sets_assignee
#        status `in-review` → assert `--add-assignee @me` invoked (it was empty
#        during the #2909 incident; in-review picks must self-assign too).
#   5. test_lost_race_writes_cooldown
#        Held-lock loss → assert /tmp/project-tick-cooldown-$LOOP_PID gets a
#        `<issue> <expiry>` line (the #2822 per-loop cooldown is preserved).
#   6. test_cross_session_serializes
#        The #2936 bounce defect: holder takes the lock at the host-stable
#        DEFAULT namespace, acquirer runs under a DIFFERENT CLAUDE_SESSION_ID
#        with no namespace override and MUST still exit 1. Fails on the old
#        per-session keying (acquirer locks a different inode → exit 0), passes
#        with the host-stable namespace. Tests 1/5 can't catch this because they
#        pre-hold under the same session/namespace they pass to the script.
#
# Usage:
#   bash .github/skills/shared/scripts/tests/test-acquire-issue-lock.sh
#
# Exits 0 if all tests pass, non-zero on any failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_SCRIPT="$SCRIPT_DIR/../acquire-issue-lock.sh"

if [ ! -x "$TARGET_SCRIPT" ]; then
  echo "FAIL: target script not found or not executable: $TARGET_SCRIPT" >&2
  exit 1
fi

if ! command -v flock >/dev/null 2>&1; then
  echo "SKIP: flock not available on this host; cannot run flock regression suite" >&2
  exit 0
fi

FAILED=0
PASSED=0

# Real (passwd-anchored) home — the script derives its lock dir from this, NOT
# from $HOME, so the test must mirror the same derivation to pre-hold the lock.
REAL_HOME="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
[ -n "$REAL_HOME" ] || REAL_HOME="$HOME"

# Tests run under a unique session id so their tick-locks dir is isolated and
# trivially cleaned up. Use a single test-run nonce.
RUN_NONCE="acqtest-$$-$(date +%s)"

# All lock dirs created by tests, removed in the EXIT trap.
CLEANUP_DIRS=()
cleanup() {
  local d
  for d in "${CLEANUP_DIRS[@]:-}"; do
    [ -n "$d" ] && rm -rf "$d" 2>/dev/null
  done
}
trap cleanup EXIT

# Compute the lockfile path the script will use for a given lock-namespace +
# issue, mirroring acquire-issue-lock.sh's derivation so the test can pre-hold
# it. The namespace is the value of PROJECT_TICK_LOCK_SESSION_ID the script is
# invoked with (default "project-tick"); it is host-stable and issue-scoped,
# NEVER keyed on the per-process CLAUDE_SESSION_ID (that was the #2917 defect:
# two ticks with distinct session ids locked distinct inodes, so flock never
# serialized them — see PR #2936 review bounce).
lock_path_for() {
  local namespace="$1" issue="$2"
  echo "$REAL_HOME/data/$namespace/tick-locks/$issue.lock"
}

# ---------------------------------------------------------------------------
# Build a mock `gh` in $1 that:
#   - records every invocation (one line per call) to $GH_CALLS_LOG
#   - for `api .../comments --method POST` prints a fake comment id (12345)
#   - succeeds (exit 0) for everything else (issue edit, comment DELETE, ...)
# ---------------------------------------------------------------------------
make_mock_gh() {
  local dir="$1"
  cat >"$dir/gh" <<'EOF'
#!/usr/bin/env bash
# Mock gh: log the call, fake a sentinel-post id, succeed otherwise.
printf '%s\n' "$*" >> "$GH_CALLS_LOG"
# Sentinel post: `gh api .../issues/<n>/comments --method POST ...`
for a in "$@"; do
  if [[ "$a" == "POST" ]]; then
    echo "12345"
    exit 0
  fi
done
exit 0
EOF
  chmod +x "$dir/gh"
}

# ---------------------------------------------------------------------------
# Test 1: a live holder's flock blocks a second acquire → exit 1, no dispatch.
# ---------------------------------------------------------------------------
test_flock_held_blocks_second_acquire() {
  local name="test_flock_held_blocks_second_acquire"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  local namespace="$RUN_NONCE-t1"
  local issue=4242
  local lockfile; lockfile="$(lock_path_for "$namespace" "$issue")"
  mkdir -p "$(dirname "$lockfile")"
  CLEANUP_DIRS+=("$REAL_HOME/data/$namespace")

  # A live tick holds the lock for the whole "dispatch".
  exec {hold_fd}>"$lockfile"
  if ! flock -n "$hold_fd"; then
    echo "FAIL: $name — could not pre-hold lock"; FAILED=$((FAILED+1)); rm -rf "$tmpdir"; return
  fi

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" PROJECT_TICK_LOCK_SESSION_ID="$namespace" \
           GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="t1" TICK_PID=$$ \
           bash "$TARGET_SCRIPT" "$issue" ready-for-doing 2>&1)
  exit_code=$?

  exec {hold_fd}>&-

  if [ "$exit_code" -ne 1 ]; then
    echo "FAIL: $name — expected exit 1 (lock held), got $exit_code"
    echo "  output: $output"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi
  if [ -f "$tmpdir/gh-calls.log" ] && grep -q "POST" "$tmpdir/gh-calls.log"; then
    echo "FAIL: $name — sentinel POST issued despite held lock"
    echo "  gh calls: $(cat "$tmpdir/gh-calls.log")"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1)); rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Test 2: free lock → exit 0, sentinel posted, assignee added.
# ---------------------------------------------------------------------------
test_acquire_succeeds_when_lock_free() {
  local name="test_acquire_succeeds_when_lock_free"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  local namespace="$RUN_NONCE-t2"
  local issue=4243
  CLEANUP_DIRS+=("$REAL_HOME/data/$namespace")

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" PROJECT_TICK_LOCK_SESSION_ID="$namespace" \
           GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="t2" TICK_PID=$$ \
           bash "$TARGET_SCRIPT" "$issue" ready-for-doing 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 0 ]; then
    echo "FAIL: $name — expected exit 0 (lock free), got $exit_code"
    echo "  output: $output"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi
  if ! grep -q "POST" "$tmpdir/gh-calls.log" 2>/dev/null; then
    echo "FAIL: $name — expected a sentinel POST"
    echo "  gh calls: $(cat "$tmpdir/gh-calls.log" 2>/dev/null)"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi
  if ! grep -q -- "--add-assignee @me" "$tmpdir/gh-calls.log" 2>/dev/null; then
    echo "FAIL: $name — expected '--add-assignee @me'"
    echo "  gh calls: $(cat "$tmpdir/gh-calls.log" 2>/dev/null)"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1)); rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Test 3: a dead holder's lock is acquirable (FD closed on death → released).
# ---------------------------------------------------------------------------
test_dead_holder_lock_is_acquirable() {
  local name="test_dead_holder_lock_is_acquirable"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  local namespace="$RUN_NONCE-t3"
  local issue=4244
  local lockfile; lockfile="$(lock_path_for "$namespace" "$issue")"
  mkdir -p "$(dirname "$lockfile")"
  CLEANUP_DIRS+=("$REAL_HOME/data/$namespace")

  # Hold the lock in a background process group, then kill the whole group and
  # poll until the lock is actually free. When the holder dies the kernel closes
  # the FD and releases the lock — no manual reap. We launch via `setsid` so the
  # holder is its own process group; `kill -9 -<pgid>` then guarantees we reap
  # the actual FD-holding process (bash may fork `sleep` as a distinct child, so
  # killing only the subshell PID can leave the real holder alive — flake seen
  # during development). We then poll `flock` to confirm release before acting.
  setsid bash -c 'exec {fd}>"'"$lockfile"'"; flock -n "$fd" || exit 1; sleep 30' &
  local holder_pid=$!
  sleep 0.3
  kill -9 -- "-$holder_pid" 2>/dev/null || kill -9 "$holder_pid" 2>/dev/null
  wait "$holder_pid" 2>/dev/null
  # Poll until the kernel has released the FD-held lock (kill -9 is async).
  local probe_fd waited=0
  while [ "$waited" -lt 50 ]; do
    exec {probe_fd}>"$lockfile"
    if flock -n "$probe_fd"; then exec {probe_fd}>&-; break; fi
    exec {probe_fd}>&-
    sleep 0.1; waited=$((waited + 1))
  done

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" PROJECT_TICK_LOCK_SESSION_ID="$namespace" \
           GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="t3" TICK_PID=$$ \
           bash "$TARGET_SCRIPT" "$issue" ready-for-doing 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 0 ]; then
    echo "FAIL: $name — expected exit 0 (dead holder released lock), got $exit_code"
    echo "  output: $output"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1)); rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Test 4: in-review pick self-assigns (@me) — closes the #2909 empty-assignee gap.
# ---------------------------------------------------------------------------
test_in_review_pick_sets_assignee() {
  local name="test_in_review_pick_sets_assignee"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  local namespace="$RUN_NONCE-t4"
  local issue=4245
  CLEANUP_DIRS+=("$REAL_HOME/data/$namespace")

  local output exit_code
  output=$(PATH="$tmpdir:$PATH" PROJECT_TICK_LOCK_SESSION_ID="$namespace" \
           GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="t4" TICK_PID=$$ \
           bash "$TARGET_SCRIPT" "$issue" in-review 2>&1)
  exit_code=$?

  if [ "$exit_code" -ne 0 ]; then
    echo "FAIL: $name — expected exit 0, got $exit_code"
    echo "  output: $output"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi
  if ! grep -q -- "--add-assignee @me" "$tmpdir/gh-calls.log" 2>/dev/null; then
    echo "FAIL: $name — in-review pick did not self-assign (@me)"
    echo "  gh calls: $(cat "$tmpdir/gh-calls.log" 2>/dev/null)"; FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1)); rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Test 5: a lost race writes a per-loop cooldown entry (#2822 preserved).
# ---------------------------------------------------------------------------
test_lost_race_writes_cooldown() {
  local name="test_lost_race_writes_cooldown"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  local namespace="$RUN_NONCE-t5"
  local issue=4246
  local loop_pid="t5-$$"
  local cooldown_file="/tmp/project-tick-cooldown-$loop_pid"
  rm -f "$cooldown_file"

  local lockfile; lockfile="$(lock_path_for "$namespace" "$issue")"
  mkdir -p "$(dirname "$lockfile")"
  CLEANUP_DIRS+=("$REAL_HOME/data/$namespace")

  # A live tick holds the lock → acquire loses the race.
  exec {hold_fd}>"$lockfile"
  if ! flock -n "$hold_fd"; then
    echo "FAIL: $name — could not pre-hold lock"; FAILED=$((FAILED+1)); rm -rf "$tmpdir"; return
  fi

  PATH="$tmpdir:$PATH" PROJECT_TICK_LOCK_SESSION_ID="$namespace" \
    GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="$loop_pid" TICK_PID=$$ \
    bash "$TARGET_SCRIPT" "$issue" ready-for-doing >/dev/null 2>&1
  local exit_code=$?

  exec {hold_fd}>&-

  if [ "$exit_code" -ne 1 ]; then
    echo "FAIL: $name — expected exit 1 (race lost), got $exit_code"
    FAILED=$((FAILED + 1)); rm -f "$cooldown_file"; rm -rf "$tmpdir"; return
  fi
  if [ ! -f "$cooldown_file" ]; then
    echo "FAIL: $name — cooldown file not written: $cooldown_file"
    FAILED=$((FAILED + 1)); rm -rf "$tmpdir"; return
  fi
  if ! grep -qE "^$issue [0-9]+$" "$cooldown_file"; then
    echo "FAIL: $name — cooldown file lacks '<issue> <expiry>' line"
    echo "  contents: $(cat "$cooldown_file")"; FAILED=$((FAILED + 1)); rm -f "$cooldown_file"; rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1)); rm -f "$cooldown_file"; rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Test 6: cross-session serialization — the #2917 / PR #2936 bounce defect.
#
# This is the test the previous (per-session-keyed) implementation could NOT
# catch: tests 1 and 5 pre-hold the lock under the SAME session id they pass to
# the script, so they pass even with a session-keyed path. The real deployment
# topology is two distinct copilot processes, each with its OWN CLAUDE_SESSION_ID,
# contending for the same issue. If the lock path is keyed on CLAUDE_SESSION_ID
# the two processes lock DIFFERENT inodes and flock never serializes them — the
# duplicate-dispatch race stays live.
#
# Here the holder takes the lock at the host-stable DEFAULT namespace path
# (the path the script derives when PROJECT_TICK_LOCK_SESSION_ID is unset),
# while the acquirer runs under a DIFFERENT CLAUDE_SESSION_ID and does NOT set
# PROJECT_TICK_LOCK_SESSION_ID. With a session-keyed path the acquirer would
# compute a different inode and exit 0 (FAIL — the bug). With the host-stable
# namespace the acquirer contends on the same inode and exits 1 (PASS).
# ---------------------------------------------------------------------------
test_cross_session_serializes() {
  local name="test_cross_session_serializes"
  local tmpdir; tmpdir="$(mktemp -d)"
  make_mock_gh "$tmpdir"

  # Use a unique issue number so this run never collides with a real lockfile
  # at the shared default namespace, but exercise the SAME default-namespace
  # derivation the script uses when PROJECT_TICK_LOCK_SESSION_ID is unset.
  local default_namespace="project-tick"
  local issue="4247$$"
  local lockfile; lockfile="$(lock_path_for "$default_namespace" "$issue")"
  mkdir -p "$(dirname "$lockfile")"
  # Clean up only the per-issue lockfile we create (the default-namespace dir
  # is shared with real ticks — do NOT rm -rf it).
  local created_lockfile="$lockfile"

  # Holder: a live tick under session A holds the default-namespace lock.
  exec {hold_fd}>"$lockfile"
  if ! flock -n "$hold_fd"; then
    echo "FAIL: $name — could not pre-hold lock"; FAILED=$((FAILED+1))
    rm -f "$created_lockfile"; rm -rf "$tmpdir"; return
  fi

  # Acquirer: a SECOND tick under a DIFFERENT session B, with NO namespace
  # override — it must still contend on the same host-stable inode and lose.
  local output exit_code
  output=$(PATH="$tmpdir:$PATH" \
           CLAUDE_SESSION_ID="session-B-$$-$(date +%s%N)" \
           GH_CALLS_LOG="$tmpdir/gh-calls.log" LOOP_PID="t6" TICK_PID=$$ \
           bash "$TARGET_SCRIPT" "$issue" ready-for-doing 2>&1)
  exit_code=$?

  exec {hold_fd}>&-

  if [ "$exit_code" -ne 1 ]; then
    echo "FAIL: $name — second session acquired despite a live cross-session holder"
    echo "       (lock is keyed on per-process session, not host-stable — #2917 race live)"
    echo "  expected exit 1, got $exit_code"
    echo "  output: $output"; FAILED=$((FAILED + 1))
    rm -f "$created_lockfile"; rm -rf "$tmpdir"; return
  fi
  if [ -f "$tmpdir/gh-calls.log" ] && grep -q "POST" "$tmpdir/gh-calls.log"; then
    echo "FAIL: $name — sentinel POST issued by the second session despite held lock"
    echo "  gh calls: $(cat "$tmpdir/gh-calls.log")"; FAILED=$((FAILED + 1))
    rm -f "$created_lockfile"; rm -rf "$tmpdir"; return
  fi

  echo "PASS: $name"; PASSED=$((PASSED + 1))
  rm -f "$created_lockfile"; rm -rf "$tmpdir"
}

# ---------------------------------------------------------------------------
# Run all tests.
# ---------------------------------------------------------------------------
test_flock_held_blocks_second_acquire
test_acquire_succeeds_when_lock_free
test_dead_holder_lock_is_acquirable
test_in_review_pick_sets_assignee
test_lost_race_writes_cooldown
test_cross_session_serializes

echo
echo "Results: ${PASSED} passed, ${FAILED} failed"
if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
exit 0
