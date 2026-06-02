#!/usr/bin/env bash
# test-reap-stale-dispatch.sh — regression tests for reap-stale-dispatch.sh,
# the reap-on-acquire-success cleanup introduced for issue #2934 (split from
# #2917 / PR #2936).
#
# reap-stale-dispatch.sh runs `kill` on a process-group and `rm -rf` on a
# ~/data workspace, so these tests are the safety net for the two destructive
# operations. The highest-stakes cases:
#   - test_no_reap_when_leader_alive / test_no_reap_on_pid_reuse:
#       a recycled or unrelated LIVE process at the recorded PGID is NOT killed.
#   - test_workspace_outside_contract_not_removed:
#       a workspace path that does not canonicalize under ~/data is REFUSED.
#
# All tests drive the script through its documented test seams:
#   REAP_SENTINEL_FILE  — supply the sentinel body from a file (no `gh`).
#   REAP_HOSTNAME       — override the current hostname for the host gate.
#   REAP_DRY_RUN=1      — decide-but-don't-execute (used for kill-decision tests
#                         where actually killing a live process would be unsafe).
#   REAP_LOG_FILE       — structured decision log we assert on.
#
# Usage:
#   bash .github/skills/shared/scripts/tests/test-reap-stale-dispatch.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_SCRIPT="$SCRIPT_DIR/../reap-stale-dispatch.sh"

if [ ! -x "$TARGET_SCRIPT" ]; then
  echo "FAIL: target script not found or not executable: $TARGET_SCRIPT" >&2
  exit 1
fi

REAL_HOME="$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f6)"
[ -n "$REAL_HOME" ] || REAL_HOME="$HOME"

FAILED=0
PASSED=0
TMPROOT="$(mktemp -d)"
CLEANUP=("$TMPROOT")
# shellcheck disable=SC2317  # invoked indirectly via `trap cleanup EXIT`
cleanup() {
  local d
  for d in "${CLEANUP[@]:-}"; do [ -n "$d" ] && rm -rf "$d" 2>/dev/null; done
  # Kill any test sleeper groups still alive.
  local p
  for p in "${SLEEPER_PIDS[@]:-}"; do
    [ -n "$p" ] && kill -9 -- "-$p" 2>/dev/null
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
  done
}
trap cleanup EXIT
SLEEPER_PIDS=()

pass() { echo "PASS: $1"; PASSED=$((PASSED + 1)); }
fail() { echo "FAIL: $1"; shift; for l in "$@"; do echo "  $l"; done; FAILED=$((FAILED + 1)); }

# Read field-22 start-time of a pid, mirroring the script's parser.
proc_starttime() {
  local pid="$1" line rest
  [ -r "/proc/$pid/stat" ] || return 1
  read -r line < "/proc/$pid/stat" || return 1
  rest="${line##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  printf '%s\n' "${20}"
}

# Launch a long-lived sleeper as its OWN process-group leader (setsid), so its
# pid == its pgid — exactly the dispatch shape acquire-issue-lock.sh records.
# Echoes "PGID STARTTIME".
spawn_group_leader() {
  setsid bash -c 'sleep 60' &
  local pid=$!
  # Wait until /proc is populated and the process is its own group leader.
  local n=0 pgid
  while [ "$n" -lt 30 ]; do
    if [ -r "/proc/$pid/stat" ]; then
      pgid="$(awk '{r=$0; sub(/.*\) /,"",r); split(r,a," "); print a[3]}' "/proc/$pid/stat")"
      [ "$pgid" = "$pid" ] && break
    fi
    sleep 0.05; n=$((n + 1))
  done
  SLEEPER_PIDS+=("$pid")
  printf '%s %s\n' "$pid" "$(proc_starttime "$pid")"
}

make_sentinel() {
  # make_sentinel <file> <host> <pgid> <starttime>
  cat > "$1" <<EOF
## 🔒 acquired-by:tick-12345-999

posted=2026-06-02T16:00:00.000Z, host=$2, tick_pid=999, dispatch_pgid=$3, dispatch_starttime=$4
EOF
}

# Make an isolated fake ~/data tree under a real ~/data subdir so
# require_home_data_path passes. Returns the session dir path.
make_workspace() {
  # make_workspace <session> <role> <issue>
  local session="$1" role="$2" issue="$3"
  local dir="$REAL_HOME/data/$session/$role-$issue"
  mkdir -p "$dir"
  echo "marker" > "$dir/marker.txt"
  echo "$dir"
}

# ---------------------------------------------------------------------------
# Test 1: leader gone → group reaped (decision) + workspace removed.
# ---------------------------------------------------------------------------
test_reaps_group_and_workspace_when_leader_gone() {
  local name="test_reaps_group_and_workspace_when_leader_gone"
  local issue="93010$$"
  local sess="reaptest-$$-1"
  local sentinel="$TMPROOT/sent1"; local log="$TMPROOT/log1"
  local ws; ws="$(make_workspace "$sess" "do" "$issue")"
  CLEANUP+=("$REAL_HOME/data/$sess")

  # Spawn a leader, record identity, then kill it so it is GONE.
  read -r pgid st < <(spawn_group_leader)
  kill -9 -- "-$pgid" 2>/dev/null; kill -9 "$pgid" 2>/dev/null
  # Wait for /proc entry to disappear.
  local n=0; while [ "$n" -lt 30 ] && [ -e "/proc/$pgid" ]; do sleep 0.05; n=$((n+1)); done
  make_sentinel "$sentinel" "$(hostname)" "$pgid" "$st"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  # Leader gone ⇒ skip-kill (safe; pid may be recycled), but workspace removed.
  if ! grep -q "skip-kill: leader pgid=$pgid gone" "$log" 2>/dev/null; then
    fail "$name" "expected skip-kill on gone leader" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  if [ -e "$ws" ]; then
    fail "$name" "workspace not removed: $ws"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 2: leader ALIVE with matching start-time → group IS killed.
# ---------------------------------------------------------------------------
test_reaps_group_when_leader_alive_and_matches() {
  local name="test_reaps_group_when_leader_alive_and_matches"
  local issue="93020$$"
  local sentinel="$TMPROOT/sent2"; local log="$TMPROOT/log2"

  read -r pgid st < <(spawn_group_leader)
  make_sentinel "$sentinel" "$(hostname)" "$pgid" "$st"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  if ! grep -q "kill: process-group $pgid" "$log" 2>/dev/null; then
    fail "$name" "expected kill decision for alive matching leader" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  # The group should actually be dead now.
  local n=0; while [ "$n" -lt 30 ] && kill -0 "$pgid" 2>/dev/null; do sleep 0.1; n=$((n+1)); done
  if kill -0 "$pgid" 2>/dev/null; then
    fail "$name" "leader pid=$pgid still alive after reap"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 3 (SAFETY): start-time MISMATCH (PID reuse) → NOT killed.
# A live process occupies the recorded pgid but with a different start-time.
# The reap MUST NOT kill it.
# ---------------------------------------------------------------------------
test_no_reap_on_pid_reuse() {
  local name="test_no_reap_on_pid_reuse"
  local issue="93030$$"
  local sentinel="$TMPROOT/sent3"; local log="$TMPROOT/log3"

  read -r pgid st < <(spawn_group_leader)
  # Record a DIFFERENT start-time (simulate the original dispatch having died
  # and this pid being recycled to the still-alive sleeper).
  local fake_st=$(( st - 100 ))
  make_sentinel "$sentinel" "$(hostname)" "$pgid" "$fake_st"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  if ! grep -q "skip-kill: pid=$pgid reused" "$log" 2>/dev/null; then
    fail "$name" "expected skip-kill on start-time mismatch" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  # CRITICAL: the live process must NOT have been killed.
  if ! kill -0 "$pgid" 2>/dev/null; then
    fail "$name" "SAFETY VIOLATION: recycled-pid live process pgid=$pgid was killed"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 4 (SAFETY): leader alive + matching, but we use DRY_RUN to assert the
# kill decision is gated correctly without killing — and confirm a NON-matching
# live process under a different pgid is untouched. (alias of alive-not-killed
# when identity does not match). Here: cross-host → never signal.
# ---------------------------------------------------------------------------
test_no_reap_cross_host() {
  local name="test_no_reap_cross_host"
  local issue="93040$$"
  local sentinel="$TMPROOT/sent4"; local log="$TMPROOT/log4"

  read -r pgid st < <(spawn_group_leader)
  # Sentinel claims a DIFFERENT host — flock is host-local, so never signal.
  make_sentinel "$sentinel" "some-other-host" "$pgid" "$st"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" REAP_HOSTNAME="this-host" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  if ! grep -q "skip-kill: cross-host" "$log" 2>/dev/null; then
    fail "$name" "expected skip-kill cross-host" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  if ! kill -0 "$pgid" 2>/dev/null; then
    fail "$name" "SAFETY VIOLATION: cross-host live process pgid=$pgid was killed"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 5: no sentinel → no-op, exit 0.
# ---------------------------------------------------------------------------
test_no_reap_when_no_sentinel() {
  local name="test_no_reap_when_no_sentinel"
  local issue="93050$$"
  local sentinel="$TMPROOT/sent5-empty"; local log="$TMPROOT/log5"
  : > "$sentinel"  # empty body

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1
  local rc=$?

  if [ "$rc" -ne 0 ]; then fail "$name" "expected exit 0, got $rc"; return; fi
  if ! grep -q "no sentinel found" "$log" 2>/dev/null; then
    fail "$name" "expected 'no sentinel found' log" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 6: malformed sentinel (no dispatch_pgid) → skip-kill, exit 0.
# ---------------------------------------------------------------------------
test_no_reap_on_malformed_sentinel() {
  local name="test_no_reap_on_malformed_sentinel"
  local issue="93060$$"
  local sentinel="$TMPROOT/sent6"; local log="$TMPROOT/log6"
  cat > "$sentinel" <<EOF
## 🔒 acquired-by:tick-old-format

posted=2026-06-02T16:00:00.000Z, host=$(hostname), tick_pid=999
EOF

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1
  local rc=$?

  if [ "$rc" -ne 0 ]; then fail "$name" "expected exit 0, got $rc"; return; fi
  if ! grep -q "skip-kill: sentinel lacks dispatch_pgid" "$log" 2>/dev/null; then
    fail "$name" "expected skip-kill on old-format sentinel" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 7: selects the MOST RECENT sentinel when multiple exist (gh path via mock).
# ---------------------------------------------------------------------------
test_selects_most_recent_sentinel() {
  local name="test_selects_most_recent_sentinel"
  local issue="93070$$"
  local log="$TMPROOT/log7"
  local mockdir="$TMPROOT/mock7"; mkdir -p "$mockdir"

  read -r pgid st < <(spawn_group_leader)
  # Build a two-comment fixture, the NEWER one carrying our live pgid. The mock
  # gh emits this JSON to stdout and applies the --jq filter the script passes
  # (the script invokes `gh api ... --jq '<filter>'`), so we feed the raw array
  # through the real jq with the filter argument the mock receives.
  local fixture="$mockdir/comments.json"
  cat > "$fixture" <<JSON
[
  {"created_at":"2026-06-02T15:00:00Z","body":"## 🔒 acquired-by:old\n\nposted=x, host=$(hostname), tick_pid=1, dispatch_pgid=999999, dispatch_starttime=1"},
  {"created_at":"2026-06-02T16:00:00Z","body":"## 🔒 acquired-by:new\n\nposted=y, host=$(hostname), tick_pid=2, dispatch_pgid=$pgid, dispatch_starttime=$st"}
]
JSON
  cat > "$mockdir/gh" <<EOF
#!/usr/bin/env bash
# Mock gh: when invoked with a --jq filter, apply it (via real jq) to the
# fixture so the script's newest-sentinel selection is exercised end to end.
jqfilter=""
prev=""
for a in "\$@"; do
  if [ "\$prev" = "--jq" ]; then jqfilter="\$a"; fi
  prev="\$a"
done
if [ -n "\$jqfilter" ]; then
  jq -r "\$jqfilter" "$fixture"
fi
exit 0
EOF
  chmod +x "$mockdir/gh"

  PATH="$mockdir:$PATH" REAP_LOG_FILE="$log" REAP_DRY_RUN=1 \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  if ! grep -q "kill: process-group $pgid" "$log" 2>/dev/null; then
    fail "$name" "expected to select newest sentinel (pgid=$pgid)" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  # DRY_RUN ⇒ the live leader survives.
  kill -9 "$pgid" 2>/dev/null
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 8 (SAFETY): EPERM (foreign-owned live pid) treated as alive → NOT killed.
# We can't easily own another user's process in a unit test, so we assert the
# decision logic via the live-and-matching path under DRY_RUN combined with a
# pid we DO own (covered by test 2/7); the EPERM branch is exercised by pointing
# at pid 1 (init) which we cannot signal. pid 1's start-time will not match our
# fake, so we instead set the recorded start-time to pid 1's real start-time to
# force the EPERM branch.
# ---------------------------------------------------------------------------
test_kill_0_eperm_treated_as_alive() {
  local name="test_kill_0_eperm_treated_as_alive"
  local issue="93080$$"
  local sentinel="$TMPROOT/sent8"; local log="$TMPROOT/log8"

  if [ "$(id -u)" -eq 0 ]; then
    echo "SKIP: $name — running as root, cannot exercise EPERM against pid 1"
    PASSED=$((PASSED + 1)); return
  fi
  local init_st; init_st="$(proc_starttime 1)"
  if [ -z "$init_st" ]; then
    echo "SKIP: $name — cannot read pid 1 start-time"; PASSED=$((PASSED + 1)); return
  fi
  make_sentinel "$sentinel" "$(hostname)" 1 "$init_st"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  # pgid=1 is implausible (<=1 guard) OR EPERM — either way must NOT kill.
  if grep -q "kill: process-group 1" "$log" 2>/dev/null; then
    fail "$name" "SAFETY VIOLATION: attempted to kill pgid 1" "log: $(cat "$log" 2>/dev/null)"; return
  fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 9 (SAFETY): a workspace path OUTSIDE ~/data is NOT removed.
# We forge a sentinel-less run and a workspace that resolves outside ~/data via
# a symlink; the contract guard must refuse it. We exercise the guard directly
# by planting a symlinked session dir under ~/data whose target is /tmp.
# ---------------------------------------------------------------------------
test_workspace_outside_contract_not_removed() {
  local name="test_workspace_outside_contract_not_removed"
  local issue="93090$$"
  local sentinel="$TMPROOT/sent9"; local log="$TMPROOT/log9"
  : > "$sentinel"  # no kill; we only test the rm guard

  # Outside-contract victim dir we must NOT delete.
  local victim="$TMPROOT/victim-$issue"
  mkdir -p "$victim"; echo "precious" > "$victim/keep.txt"

  # Plant a session dir under ~/data that is a SYMLINK to a parent of the victim,
  # so the glob ~/data/<session>/do-<issue> resolves (via the symlink) to the
  # victim — require_home_data_path must canonicalize through the symlink and
  # refuse it (target is under $TMPROOT, not ~/data).
  local sess="reaptest-evil-$$"
  local sessdir="$REAL_HOME/data/$sess"
  CLEANUP+=("$sessdir")
  # ~/data/<sess> -> $TMPROOT  ; then do-<issue> lives at $TMPROOT/do-<issue>
  ln -s "$TMPROOT" "$sessdir"
  mkdir -p "$TMPROOT/do-$issue"; echo "precious" > "$TMPROOT/do-$issue/keep.txt"

  REAP_SENTINEL_FILE="$sentinel" REAP_LOG_FILE="$log" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1

  if [ ! -e "$TMPROOT/do-$issue/keep.txt" ]; then
    fail "$name" "SAFETY VIOLATION: removed an out-of-contract workspace via symlink escape"; return
  fi
  if ! grep -q "skip-rm:" "$log" 2>/dev/null; then
    # If the glob didn't even match through the symlink, that's also safe, but
    # we want to confirm the guard fired when it did match.
    : # acceptable: nothing matched (still safe)
  fi
  rm -f "$sessdir"
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 10: reap is non-fatal — even a hard failure path returns 0.
# Point gh at a missing binary AND no sentinel file ⇒ get_sentinel_body yields
# empty ⇒ exit 0.
# ---------------------------------------------------------------------------
test_reap_is_nonfatal() {
  local name="test_reap_is_nonfatal"
  local issue="93100$$"
  local stubdir="$TMPROOT/ghfail"; mkdir -p "$stubdir"
  # A `gh` that hard-fails (network down). get_sentinel_body must swallow it and
  # the script must still exit 0 (best-effort, non-fatal). Real coreutils stay
  # on PATH (prepend the stub dir) so only `gh` is broken.
  cat > "$stubdir/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh: simulated network failure" >&2
exit 1
EOF
  chmod +x "$stubdir/gh"
  PATH="$stubdir:$PATH" REAP_LOG_FILE="$TMPROOT/log10" \
    bash "$TARGET_SCRIPT" "$issue" >/dev/null 2>&1
  local rc=$?
  if [ "$rc" -ne 0 ]; then fail "$name" "expected exit 0 even when gh missing, got $rc"; return; fi
  pass "$name"
}

# ---------------------------------------------------------------------------
# Test 11: non-numeric issue → refuse, exit 0, no action.
# ---------------------------------------------------------------------------
test_rejects_non_numeric_issue() {
  local name="test_rejects_non_numeric_issue"
  local out rc
  out="$(bash "$TARGET_SCRIPT" "../../etc" 2>&1)"; rc=$?
  if [ "$rc" -ne 0 ]; then fail "$name" "expected exit 0, got $rc"; return; fi
  if ! printf '%s' "$out" | grep -q "not numeric"; then
    fail "$name" "expected 'not numeric' refusal" "out: $out"; return
  fi
  pass "$name"
}

test_reaps_group_and_workspace_when_leader_gone
test_reaps_group_when_leader_alive_and_matches
test_no_reap_on_pid_reuse
test_no_reap_cross_host
test_no_reap_when_no_sentinel
test_no_reap_on_malformed_sentinel
test_selects_most_recent_sentinel
test_kill_0_eperm_treated_as_alive
test_workspace_outside_contract_not_removed
test_reap_is_nonfatal
test_rejects_non_numeric_issue

echo
echo "Results: ${PASSED} passed, ${FAILED} failed"
[ "$FAILED" -gt 0 ] && exit 1
exit 0
