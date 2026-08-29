#!/usr/bin/env bash
#
# Regression + unit harness for the monitor-loop crontab watchdog:
#   scripts/lib/monitor-watchdog-decisions.sh   (pure decision logic)
#   scripts/monitor-watchdog.sh                 (I/O wrapper: locks, launch)
#
# Covers the three #3789 over-firing / harm mechanisms carried over from #3757:
#   1. STALE_SECS resize — a healthy-but-slow ~32-min completion cadence must
#      FIRE at the old 1800s bar (the bug) and SKIP at the new 4200s bar (fix).
#   2. flock in-flight guard — a watchdog tick that cannot take the shared
#      whole-tick lock logs "skipped: tick in flight" and launches nothing.
#   3. Fail CLOSED on an unparseable history tail — no fire, and the refire
#      marker is NOT written (contrast the old epoch-0 => unconditional fire).
#
# Usage:  bash scripts/test-monitor-watchdog.sh
# Output: Test Anything Protocol (TAP) on stdout.
# Exit:   0 if all pass, 1 otherwise.
# Portability: GNU/Linux only (GNU `date -d`, `stat -c`, flock, timeout).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB="$SCRIPT_DIR/lib/monitor-watchdog-decisions.sh"
WATCHDOG="$SCRIPT_DIR/monitor-watchdog.sh"

# Scratch honors the #2843 ~/data session contract when available, else a
# system mktemp (CI ephemeral runner).
if [[ -n "${CLAUDE_SESSION_ID:-}" && -d "${HOME}/data" ]]; then
  SCRATCH="$(mktemp -d "${HOME}/data/${CLAUDE_SESSION_ID}/test-monitor-watchdog.XXXXXX")"
else
  SCRATCH="$(mktemp -d)"
fi
trap 'rm -rf "$SCRATCH"' EXIT

# ── TAP plumbing ─────────────────────────────────────────────────────────────
TEST_NUM=0
FAIL=0
ok()    { TEST_NUM=$((TEST_NUM + 1)); printf 'ok %d - %s\n' "$TEST_NUM" "$1"; }
notok() { TEST_NUM=$((TEST_NUM + 1)); FAIL=$((FAIL + 1)); printf 'not ok %d - %s\n' "$TEST_NUM" "$1"
          shift; for l in "$@"; do printf '# %s\n' "$l"; done; }
is()    { # is DESC ACTUAL EXPECTED
          if [ "$2" = "$3" ]; then ok "$1"; else notok "$1" "expected: [$3]" "actual:   [$2]"; fi; }

# Source the pure decision lib.
if [ ! -f "$LIB" ]; then
  notok "decision lib present ($LIB)" "missing — cannot run decision tests"
else
  # shellcheck source=/dev/null
  source "$LIB"
fi

# ─────────────────────────────────────────────────────────────────────────────
# test_over_fire_threshold — the STALE_SECS resize, demonstrated in BOTH
# directions: a healthy 32-min completion cadence FIRES at the old 1800s bar
# (the #3757/#3789 over-firing bug) and SKIPS at the new 4200s bar (the fix).
# ─────────────────────────────────────────────────────────────────────────────
test_over_fire_threshold() {
  local now=1000000000
  local last=$(( now - 1920 ))   # 32 min ago
  local d_old d_new
  d_old=$(watchdog_should_fire "$now" "$last" 1800 0 1800)
  d_new=$(watchdog_should_fire "$now" "$last" 4200 0 1800)
  is "over-fire: 32-min cadence FIRES at STALE_SECS=1800 (the bug)" "$d_old" "fire stale"
  is "over-fire: 32-min cadence SKIPS at STALE_SECS=4200 (the fix)"  "$d_new" "skip fresh"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_fail_closed_on_unparseable_tail — a present-but-unparseable tail yields
# PARSE_ERROR => skip, and (integration) does NOT write the refire marker.
# ─────────────────────────────────────────────────────────────────────────────
test_fail_closed_on_unparseable_tail() {
  local h1="$SCRATCH/hist-notjson.jsonl" h2="$SCRATCH/hist-nots.jsonl"
  printf '%s\n' '{not json'            > "$h1"
  printf '%s\n' '{"ledger":42,"ok":true}' > "$h2"

  is "fail-closed: non-JSON tail => PARSE_ERROR"           "$(watchdog_last_epoch "$h1")" "PARSE_ERROR"
  is "fail-closed: JSON w/o ts|timestamp => PARSE_ERROR"   "$(watchdog_last_epoch "$h2")" "PARSE_ERROR"
  is "fail-closed: PARSE_ERROR decision is skip"           "$(watchdog_should_fire 1000000000 PARSE_ERROR 4200 0 1800)" "skip parse-error"

  # Integration: the wrapper must NOT write the marker on a PARSE_ERROR skip,
  # and must NOT launch the mocked claude. Contrast the old epoch-0 => fire.
  _mk_env
  cp "$h1" "$WD_HIST"
  ( eval "$WD_ENV"; bash "$WATCHDOG" ) >/dev/null 2>&1
  [ ! -f "$WD_MARKER" ] \
    && ok "fail-closed: refire marker NOT written on PARSE_ERROR skip" \
    || notok "fail-closed: refire marker NOT written on PARSE_ERROR skip" "marker exists: $WD_MARKER"
  [ ! -f "$WD_CLAUDE_SENTINEL" ] \
    && ok "fail-closed: headless tick NOT launched on PARSE_ERROR skip" \
    || notok "fail-closed: headless tick NOT launched on PARSE_ERROR skip" "sentinel exists"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_accepts_ts_and_timestamp_keys — both key names parse to the same epoch.
# ─────────────────────────────────────────────────────────────────────────────
test_accepts_ts_and_timestamp_keys() {
  local iso='2026-08-23T12:00:00Z'
  local want; want=$(date -u -d "$iso" +%s)
  local hts="$SCRATCH/hist-ts.jsonl" hto="$SCRATCH/hist-timestamp.jsonl"
  printf '%s\n' "{\"ts\":\"$iso\",\"ledger\":9}"        > "$hts"
  printf '%s\n' "{\"ledger\":9,\"timestamp\":\"$iso\"}" > "$hto"
  is "keys: \"ts\" parses"        "$(watchdog_last_epoch "$hts")" "$want"
  is "keys: \"timestamp\" parses" "$(watchdog_last_epoch "$hto")" "$want"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_fire_on_missing_history — a missing history file fires (fail open); the
# fail-closed guard must NOT over-correct into never firing on a dark loop.
# ─────────────────────────────────────────────────────────────────────────────
test_fire_on_missing_history() {
  is "missing-history: last_epoch => MISSING" "$(watchdog_last_epoch "$SCRATCH/does-not-exist.jsonl")" "MISSING"
  is "missing-history: decision => fire"      "$(watchdog_should_fire 1000000000 MISSING 4200 0 1800)"  "fire no-history"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_cooldown — stale but a firing happened within the cooldown => skip.
# ─────────────────────────────────────────────────────────────────────────────
test_cooldown() {
  local now=1000000000 last=$(( 1000000000 - 6000 )) recent=$(( 1000000000 - 600 ))
  is "cooldown: stale + recent fire => skip" "$(watchdog_should_fire "$now" "$last" 4200 "$recent" 1800)" "skip cooldown"
  is "cooldown: stale + old fire  => fire"   "$(watchdog_should_fire "$now" "$last" 4200 $(( now - 3600 )) 1800)" "fire stale"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_parses_without_python3 — the tail parse must not depend on python3 (a
# missing interpreter on cron's PATH silently blinded the watchdog 2026-07-22).
# ─────────────────────────────────────────────────────────────────────────────
test_parses_without_python3() {
  local iso='2026-08-23T12:00:00Z'
  local want; want=$(date -u -d "$iso" +%s)
  local h="$SCRATCH/hist-nopy.jsonl"
  printf '%s\n' "{\"ts\":\"$iso\"}" > "$h"
  # Minimal PATH that excludes any python3 shim, keeping only core coreutils.
  local out
  out=$(PATH=/usr/bin:/bin bash -c '
    source "'"$LIB"'"
    watchdog_last_epoch "'"$h"'"
  ')
  is "no-python3: tail still parses" "$out" "$want"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_flock_skips_when_lock_held — a background holder owns the shared tick
# lock; a stale-history watchdog run must log "skipped: tick in flight" and
# launch nothing.
# ─────────────────────────────────────────────────────────────────────────────
test_flock_skips_when_lock_held() {
  _mk_env
  # Stale history so the decision alone would be "fire".
  printf '%s\n' "{\"ts\":\"$(date -u -d '2 hours ago' +%FT%TZ)\"}" > "$WD_HIST"

  # Background holder takes TICK_LOCK for ~5s.
  ( flock -x 8; sleep 5 ) 8>"$WD_TICK_LOCK" &
  local holder=$!
  # Give the holder time to acquire.
  sleep 0.5

  ( eval "$WD_ENV"; bash "$WATCHDOG" ) >/dev/null 2>&1

  kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null

  grep -q 'skipped: tick in flight' "$WD_LOG" \
    && ok "flock: logs 'skipped: tick in flight' when lock held" \
    || notok "flock: logs 'skipped: tick in flight' when lock held" "log:" "$(cat "$WD_LOG" 2>/dev/null)"
  [ ! -f "$WD_CLAUDE_SENTINEL" ] \
    && ok "flock: headless tick NOT launched when lock held" \
    || notok "flock: headless tick NOT launched when lock held" "sentinel exists"
}

# ─────────────────────────────────────────────────────────────────────────────
# test_fires_and_launches_when_stale_and_free — stale history + free lock =>
# the mocked claude IS launched and the refire marker IS written.
# ─────────────────────────────────────────────────────────────────────────────
test_fires_and_launches_when_stale_and_free() {
  _mk_env
  printf '%s\n' "{\"ts\":\"$(date -u -d '2 hours ago' +%FT%TZ)\"}" > "$WD_HIST"

  ( eval "$WD_ENV"; bash "$WATCHDOG" ) >/dev/null 2>&1

  [ -f "$WD_CLAUDE_SENTINEL" ] \
    && ok "fire: headless tick launched when stale and lock free" \
    || notok "fire: headless tick launched when stale and lock free" "log:" "$(cat "$WD_LOG" 2>/dev/null)"
  [ -f "$WD_MARKER" ] \
    && ok "fire: refire marker written" \
    || notok "fire: refire marker written" "marker missing"
}

# _mk_env — create a fresh mock environment (temp DATA, mock claude, mock repo).
# Runs in the CURRENT shell (NOT command substitution) so it can set the WD_*
# globals used by post-run assertions. Also populates WD_ENV: the `export ...`
# block a subshell should `eval` to target this environment before invoking the
# watchdog under test.
_mk_env() {
  local base; base="$(mktemp -d "$SCRATCH/env.XXXXXX")"
  local data="$base/data" repo="$base/repo" bin="$base/bin"
  mkdir -p "$data" "$repo" "$bin"
  WD_HIST="$data/tick-history.jsonl"
  WD_MARKER="$data/monitor-watchdog.lastfire"
  WD_LOG="$data/monitor-watchdog.log"
  WD_LOCK="$data/monitor-watchdog.lock"
  WD_TICK_LOCK="$data/monitor-tick.lock"
  WD_CLAUDE_SENTINEL="$data/claude-was-launched"

  # Mock `claude`: record that it ran, then exit 0.
  cat > "$bin/claude" <<MOCK
#!/usr/bin/env bash
printf 'launched\n' > "$WD_CLAUDE_SENTINEL"
exit 0
MOCK
  chmod +x "$bin/claude"

  WD_ENV="$(cat <<ENV
export MONITOR_SESSION_ID=testsess
export WATCHDOG_DATA="$data"
export WATCHDOG_HIST="$WD_HIST"
export WATCHDOG_MARKER="$WD_MARKER"
export WATCHDOG_LOG="$WD_LOG"
export WATCHDOG_LOCK="$WD_LOCK"
export WATCHDOG_TICK_LOCK="$WD_TICK_LOCK"
export WATCHDOG_REPO="$repo"
export CLAUDE_BIN="$bin/claude"
export WATCHDOG_STALE_SECS=4200
export WATCHDOG_REFIRE_COOLDOWN=1800
export WATCHDOG_TICK_TIMEOUT=30
ENV
)"
}

# ── Run ──────────────────────────────────────────────────────────────────────
if type watchdog_should_fire >/dev/null 2>&1; then
  test_over_fire_threshold
  test_fail_closed_on_unparseable_tail
  test_accepts_ts_and_timestamp_keys
  test_fire_on_missing_history
  test_cooldown
  test_parses_without_python3
  test_flock_skips_when_lock_held
  test_fires_and_launches_when_stale_and_free
else
  notok "decision functions loaded" "watchdog_should_fire not defined — lib failed to source"
fi

printf '1..%d\n' "$TEST_NUM"
[ "$FAIL" -eq 0 ] || { printf '# %d test(s) failed\n' "$FAIL"; exit 1; }
exit 0
