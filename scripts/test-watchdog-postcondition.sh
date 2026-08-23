#!/usr/bin/env bash
#
# Regression harness for the crontab watchdog post-condition detection (#3780).
#
# Incident: on 2026-07-28/29 the mainnet monitor ran ~24h with zero coverage
# while `monitor-watchdog.sh` logged 49 consecutive "successful" ticks. The org
# monthly spend limit made `claude -p` refuse to run and **exit 0**; the
# watchdog recorded `exit=$?` (the CLI's status, not the tick's) and could not
# tell a 1-second refusal from a completed tick. Worse, the refire-cooldown
# MARKER was written *before* the launch, so each no-op burned the full 30-min
# cooldown, and nothing escalated.
#
# This harness pins the fix:
#   1. `watchdog_classify_outcome` distinguishes a real tick (a NEW
#      tick-history.jsonl row, above a duration floor) from a refusal no-op,
#      an unexplained empty no-op, and a suspiciously-fast row write.
#   2. Both watchdogs write the cooldown MARKER **only** on a real `success`,
#      so a refusal no-op does not burn the cooldown.
#   3. Both watchdogs log a distinct `outcome=<class>` line instead of a bare
#      `exit=0`.
#
# COUPLING: assertions key on the stable outcome tokens (`success`,
# `noop-refusal`, `noop-empty`, `suspect-fast`) and the MARKER-write behavior,
# not on log wording. If you reword a log line keep the `outcome=<class>`
# token, or update this test in the same commit.
#
# Usage:   bash scripts/test-watchdog-postcondition.sh
# Output:  TAP on stdout, diagnostics on stderr.
# Exit:    0 = all pass, 1 = any fail.
# Portability: GNU/Linux (Bash 4+, coreutils, git).
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB="$SCRIPT_DIR/lib/watchdog-postcondition.sh"
MONITOR_WD="$SCRIPT_DIR/monitor-watchdog.sh"
LOOP_WD="$SCRIPT_DIR/project-loop-watchdog.sh"

# ── TAP state ────────────────────────────────────────────────────────────────
TAP_PLAN=15
TAP_CURRENT=0
TAP_FAILURES=0

tap_plan() { echo "1..$TAP_PLAN"; }
tap_ok() { TAP_CURRENT=$((TAP_CURRENT + 1)); echo "ok $TAP_CURRENT - $1"; }
tap_not_ok() {
  TAP_CURRENT=$((TAP_CURRENT + 1))
  TAP_FAILURES=$((TAP_FAILURES + 1))
  echo "not ok $TAP_CURRENT - $1"
  [ -n "${2:-}" ] && echo "# $2" >&2
  return 0
}

# assert_eq DESC EXPECTED ACTUAL
assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    tap_ok "$desc"
  else
    tap_not_ok "$desc" "expected [$expected], got [$actual]"
  fi
}

# assert_no_file DESC PATH
assert_no_file() {
  if [ ! -e "$2" ]; then tap_ok "$1"; else tap_not_ok "$1" "file exists: $2"; fi
}

# assert_file DESC PATH
assert_file() {
  if [ -e "$2" ]; then tap_ok "$1"; else tap_not_ok "$1" "file missing: $2"; fi
}

# assert_grep DESC PATTERN FILE
assert_grep() {
  if [ -f "$3" ] && grep -Eq -- "$2" "$3"; then
    tap_ok "$1"
  else
    tap_not_ok "$1" "pattern /$2/ not found in $3"
  fi
}

TMPROOT="$(mktemp -d)"
cleanup() { rm -rf "$TMPROOT"; }
trap cleanup EXIT

tap_plan

# ── Preconditions / lib load ─────────────────────────────────────────────────
if [ ! -f "$LIB" ]; then
  echo "# lib not found at $LIB — classifier assertions will fail" >&2
else
  # shellcheck source=scripts/lib/watchdog-postcondition.sh
  . "$LIB"
fi

# Wrapper that tolerates the lib being absent (so the harness reports clean TAP
# failures pre-fix instead of a hard bail).
classify() {
  if command -v watchdog_classify_outcome >/dev/null 2>&1; then
    watchdog_classify_outcome "$@"
  else
    echo "MISSING-LIB"
  fi
}
histcount() {
  if command -v watchdog_hist_count >/dev/null 2>&1; then
    watchdog_hist_count "$@"
  else
    echo "MISSING-LIB"
  fi
}

# ── Unit: watchdog_hist_count ────────────────────────────────────────────────
assert_eq "hist_count of a missing file is 0" "0" "$(histcount "$TMPROOT/nope.jsonl")"

hf="$TMPROOT/hist.jsonl"
printf '{"ts":"a"}\n{"ts":"b"}\n{"ts":"c"}\n' > "$hf"
assert_eq "hist_count counts rows in an existing file" "3" "$(histcount "$hf")"

# ── Unit: watchdog_classify_outcome branches ─────────────────────────────────
spend="$TMPROOT/spend.out"
printf "You've hit your org's monthly spend limit · run /usage-credits to ask your admin\n" > "$spend"
assert_eq "spend-limit no-op (no new row, refusal text) classifies noop-refusal" \
  "noop-refusal" "$(classify 100 100 2 "$spend")"

normal="$TMPROOT/normal.out"
printf 'monitor-tick 2851 complete: node Validating, wrote history row\n' > "$normal"
assert_eq "real tick (new row, above floor) classifies success" \
  "success" "$(classify 100 101 372 "$normal")"

empty="$TMPROOT/empty.out"
: > "$empty"
assert_eq "no new row with no recognized reason classifies noop-empty" \
  "noop-empty" "$(classify 100 100 40 "$empty")"

assert_eq "new row under the duration floor classifies suspect-fast" \
  "suspect-fast" "$(classify 100 101 3 "$normal")"

# ── E2E: monitor-watchdog.sh does NOT burn cooldown on a spend-limit no-op ────
run_monitor_e2e() {
  local kind="$1" fx="$TMPROOT/mon-$kind"
  local data="$fx/data" sess="$fx/data/sess" repo="$fx/repo" bin="$fx/claude"
  mkdir -p "$sess" "$repo"
  local hist="$sess/tick-history.jsonl"
  : > "$hist"   # empty history => stale => the watchdog fires
  # Stub CLI.
  cat > "$bin" <<EOF
#!/usr/bin/env bash
if [ "$kind" = "spend" ]; then
  echo "You've hit your org's monthly spend limit · run /usage-credits to ask your admin"
  exit 0
else
  sleep 2
  printf '{"ts":"2026-08-23T05:00:00Z","tick":1}\n' >> "$hist"
  exit 0
fi
EOF
  chmod +x "$bin"
  DATA="$data" SESS="$sess" HIST="$hist" REPO="$repo" CLAUDE_BIN="$bin" \
    WATCHDOG_DUR_FLOOR=1 ESCALATE_AFTER=3 \
    bash "$MONITOR_WD" >/dev/null 2>&1
  # Export the fixture paths for the caller's assertions.
  E2E_MARKER="$data/monitor-watchdog.lastfire"
  E2E_LOG="$data/monitor-watchdog.log"
  E2E_FAILSTREAK="$data/monitor-watchdog.failstreak"
}

if [ -f "$MONITOR_WD" ]; then
  run_monitor_e2e spend
  assert_no_file "monitor-watchdog: spend-limit no-op does NOT write the cooldown MARKER" "$E2E_MARKER"
  assert_grep   "monitor-watchdog: spend-limit no-op logs outcome=noop-refusal" 'outcome=noop-refusal' "$E2E_LOG"
  assert_grep   "monitor-watchdog: spend-limit no-op increments the failstreak" '^1$' "$E2E_FAILSTREAK"

  run_monitor_e2e real
  assert_file "monitor-watchdog: real tick writes the cooldown MARKER" "$E2E_MARKER"
  assert_grep "monitor-watchdog: real tick logs outcome=success" 'outcome=success' "$E2E_LOG"
else
  echo "# $MONITOR_WD absent — 5 monitor e2e assertions will fail" >&2
  tap_not_ok "monitor-watchdog: spend-limit no-op does NOT write the cooldown MARKER" "script absent"
  tap_not_ok "monitor-watchdog: spend-limit no-op logs outcome=noop-refusal" "script absent"
  tap_not_ok "monitor-watchdog: spend-limit no-op increments the failstreak" "script absent"
  tap_not_ok "monitor-watchdog: real tick writes the cooldown MARKER" "script absent"
  tap_not_ok "monitor-watchdog: real tick logs outcome=success" "script absent"
fi

# ── E2E: project-loop-watchdog.sh (clone path via a local bare remote) ────────
if [ -f "$LOOP_WD" ]; then
  fx="$TMPROOT/loop"; data="$fx/data"; sess="$fx/data/project-loop"
  mkdir -p "$sess"
  hist="$sess/tick-history.jsonl"; : > "$hist"
  # Seed a local bare remote so the watchdog's `git clone --depth 1` works offline.
  seed="$fx/seed"; bare="$fx/remote.git"
  git init -q "$seed"
  git -C "$seed" -c user.email=t@e -c user.name=t commit -q --allow-empty -m init
  git clone -q --bare "$seed" "$bare" >/dev/null 2>&1
  bin="$fx/claude"
  cat > "$bin" <<'EOF'
#!/usr/bin/env bash
echo "You've hit your org's monthly spend limit · run /usage-credits to ask your admin"
exit 0
EOF
  chmod +x "$bin"
  DATA="$data" SESS="$sess" HIST="$hist" REMOTE="$bare" CLAUDE_BIN="$bin" \
    WATCHDOG_DUR_FLOOR=1 ESCALATE_AFTER=3 \
    bash "$LOOP_WD" >/dev/null 2>&1
  assert_no_file "project-loop-watchdog: spend-limit no-op does NOT write the cooldown MARKER" "$data/project-loop-watchdog.lastfire"
  assert_grep   "project-loop-watchdog: spend-limit no-op logs outcome=noop-refusal" 'outcome=noop-refusal' "$data/project-loop-watchdog.log"
else
  echo "# $LOOP_WD absent — 2 loop e2e assertions will fail" >&2
  tap_not_ok "project-loop-watchdog: spend-limit no-op does NOT write the cooldown MARKER" "script absent"
  tap_not_ok "project-loop-watchdog: spend-limit no-op logs outcome=noop-refusal" "script absent"
fi

# ── Static: both watchdog scripts parse clean ────────────────────────────────
if [ -f "$MONITOR_WD" ] && bash -n "$MONITOR_WD" 2>/dev/null; then
  tap_ok "monitor-watchdog.sh parses clean (bash -n)"
else
  tap_not_ok "monitor-watchdog.sh parses clean (bash -n)" "absent or syntax error"
fi
if bash -n "$LOOP_WD" 2>/dev/null; then
  tap_ok "project-loop-watchdog.sh parses clean (bash -n)"
else
  tap_not_ok "project-loop-watchdog.sh parses clean (bash -n)" "syntax error"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
if [ "$TAP_FAILURES" -gt 0 ]; then
  echo "# $TAP_FAILURES of $TAP_PLAN assertions failed" >&2
  exit 1
fi
echo "# all $TAP_PLAN assertions passed" >&2
exit 0
