#!/usr/bin/env bash
# cargo-target-reaper.test.sh — behavioral regression harness for
# scripts/cargo-target-reaper.sh (#3798).
#
# Every test stages a fake ~/data root under its own `mktemp -d` — never the
# repo tree, never the real ~/data — and drives the reaper with REAP_BASE_DIR /
# REAP_LOG pointed at that scratch. The liveness tests rely on the reaper
# scanning the host's real /proc, so they spawn a real process (or hold a real
# fd) rooted inside a staged candidate and assert it is skipped.
#
# Usage: bash scripts/cargo-target-reaper.test.sh
# Exit:  0 if all tests pass, 1 otherwise.
# Output: Test Anything Protocol (TAP).
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REAPER="$SCRIPT_DIR/cargo-target-reaper.sh"
SLEEP_BIN="$(command -v sleep)"

TEST_NUM=0
FAIL=0
ok()   { TEST_NUM=$((TEST_NUM + 1)); printf 'ok %d - %s\n' "$TEST_NUM" "$1"; }
notok(){ TEST_NUM=$((TEST_NUM + 1)); FAIL=$((FAIL + 1)); printf 'not ok %d - %s\n' "$TEST_NUM" "$1"
         shift; for line in "$@"; do printf '# %s\n' "$line"; done; }

# stage_cache ROOT REL_PARENT NAME AGE  — create ROOT/REL_PARENT/NAME/release/bin
# (a valid build-cache root) and set the cache dir mtime. AGE is a `touch -d`
# spec ('now' for a fresh dir). Echoes the cache dir path.
stage_cache() {
  local root="$1" parent="$2" name="$3" age="$4"
  local cache="$root/$parent/$name"
  mkdir -p "$cache/release"
  printf 'buildcache\n' > "$cache/release/bin"
  if [ "$age" = "now" ]; then
    touch "$cache"
  else
    touch -d "$age" "$cache"
  fi
  printf '%s\n' "$cache"
}

# run_reaper BASE LOG [args...] — run the reaper, echo combined stdout+stderr.
run_reaper() {
  local base="$1" log="$2"; shift 2
  REAP_BASE_DIR="$base" REAP_LOG="$log" bash "$REAPER" "$@" 2>&1
}

# ─── test 1: structural match on target + cargo-target with debug|release ───
test_matches() {
  local root; root="$(mktemp -d)"
  stage_cache "$root" a target        '30 days ago' >/dev/null
  stage_cache "$root" b cargo-target  '30 days ago' >/dev/null
  # A dir named `target` WITHOUT debug/ or release/ must not match.
  mkdir -p "$root/c/target/src"; touch -d '30 days ago' "$root/c/target"
  run_reaper "$root" "$root/reap.log" >/dev/null
  if [ ! -d "$root/a/target" ] && [ ! -d "$root/b/cargo-target" ] && [ -d "$root/c/target" ]; then
    ok "matches target/ and cargo-target/ with debug|release; skips plain target/"
  else
    notok "structural match" "a/target exists=$([ -d "$root/a/target" ] && echo y)" \
      "b/cargo-target exists=$([ -d "$root/b/cargo-target" ] && echo y)" \
      "c/target exists=$([ -d "$root/c/target" ] && echo y) (should be y)"
  fi
  rm -rf "$root"
}

# ─── test 2: no descent into a matched root (nested target counted once) ───
test_no_descend() {
  local root; root="$(mktemp -d)"
  local cache; cache="$(stage_cache "$root" p target '30 days ago')"
  # A nested build-cache-shaped dir inside the matched root.
  mkdir -p "$cache/debug/target/release"; printf 'x\n' > "$cache/debug/target/release/bin"
  touch -d '30 days ago' "$cache"
  local out; out="$(run_reaper "$root" "$root/reap.log" --dry-run)"
  local n; n="$(printf '%s\n' "$out" | grep -c '^\[REAP-dry\]')"
  if [ "$n" -eq 1 ] && printf '%s\n' "$out" | grep -q 'scanned 1 candidate'; then
    ok "no descent into matched root — nested target counted once"
  else
    notok "no-descend" "REAP-dry lines=$n (want 1)" "$out"
  fi
  rm -rf "$root"
}

# ─── test 3: age gate — old reaped, young kept ───
test_age_gate() {
  local root; root="$(mktemp -d)"
  stage_cache "$root" old target '30 days ago' >/dev/null
  stage_cache "$root" new target 'now'         >/dev/null
  local out; out="$(run_reaper "$root" "$root/reap.log")"
  if [ ! -d "$root/old/target" ] && [ -d "$root/new/target" ] \
     && printf '%s\n' "$out" | grep -q '\[SKIP-young\].*new/target'; then
    ok "age gate reaps old, keeps young"
  else
    notok "age gate" "old exists=$([ -d "$root/old/target" ] && echo y)" \
      "new exists=$([ -d "$root/new/target" ] && echo y)" "$out"
  fi
  rm -rf "$root"
}

# ─── test 4: liveness exe skip is dir-scoped ───
test_liveness_exe_skip() {
  local root; root="$(mktemp -d)"
  local live;    live="$(stage_cache "$root" proj target '30 days ago')"
  local sibling; sibling="$(stage_cache "$root" proj cargo-target '30 days ago')"
  # Exec a real sleeper FROM INSIDE the live candidate.
  cp "$SLEEP_BIN" "$live/release/sleeper"
  "$live/release/sleeper" 30 &
  local pid=$!
  # Give the kernel a moment to publish /proc/<pid>/exe.
  local i=0; while [ ! -e "/proc/$pid/exe" ] && [ $i -lt 50 ]; do i=$((i+1)); done
  local out; out="$(run_reaper "$root" "$root/reap.log")"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  if [ -d "$live/release" ] && [ ! -d "$sibling" ] \
     && printf '%s\n' "$out" | grep -q '\[SKIP-live\].*proj/target'; then
    ok "liveness exe skip is dir-scoped (live kept, idle sibling reaped)"
  else
    notok "liveness exe" "live exists=$([ -d "$live" ] && echo y)" \
      "sibling exists=$([ -d "$sibling" ] && echo y) (should be n)" "$out"
  fi
  rm -rf "$root"
}

# ─── test 5: liveness fd/maps skip (distinct from exe) ───
test_liveness_fd_skip() {
  local root; root="$(mktemp -d)"
  local live; live="$(stage_cache "$root" proj target '30 days ago')"
  printf 'lib\n' > "$live/release/lib"
  exec 7< "$live/release/lib"      # hold an fd open in THIS shell
  local out; out="$(run_reaper "$root" "$root/reap.log")"
  exec 7<&-                        # release it
  if [ -d "$live/release" ] && printf '%s\n' "$out" | grep -q '\[SKIP-live\].*proj/target'; then
    ok "liveness fd skip keeps a candidate held open by an fd"
  else
    notok "liveness fd" "live exists=$([ -d "$live" ] && echo y)" "$out"
  fi
  rm -rf "$root"
}

# ─── test 6: exempt allowlist ───
test_exempt_allowlist() {
  local root; root="$(mktemp -d)"
  stage_cache "$root" mainnet target '90 days ago' >/dev/null   # exempt
  stage_cache "$root" other   target '90 days ago' >/dev/null   # not exempt
  local out; out="$(run_reaper "$root" "$root/reap.log")"
  if [ -d "$root/mainnet/target" ] && [ ! -d "$root/other/target" ] \
     && printf '%s\n' "$out" | grep -q '\[SKIP-exempt\].*mainnet'; then
    ok "exempt allowlist never reaps mainnet even when old"
  else
    notok "exempt" "mainnet exists=$([ -d "$root/mainnet/target" ] && echo y) (should be y)" \
      "other exists=$([ -d "$root/other/target" ] && echo y) (should be n)" "$out"
  fi
  rm -rf "$root"
}

# ─── test 7: unlink-only reclamation + per-tree reporting ───
test_unlink_only_and_reporting() {
  local root; root="$(mktemp -d)"
  local cache; cache="$(stage_cache "$root" s target '30 days ago')"
  head -c 4096 /dev/zero > "$cache/release/big"
  mkdir -p "$root/keep"
  ln "$cache/release/big" "$root/keep/big"     # hardlink OUTSIDE the candidate
  touch -d '30 days ago' "$cache"
  local expected; expected="$(du -sb "$cache" | awk '{print $1}')"
  local out; out="$(run_reaper "$root" "$root/reap.log")"
  local reported; reported="$(printf '%s\n' "$out" | sed -n 's/^RECLAIMED: \([0-9]*\) bytes.*/\1/p')"
  # Source-level guard: no truncate call / no `>`-redirect targeting a
  # candidate. Comment lines are excluded so prose mentioning "truncate" in the
  # rationale does not trip the guard; `truncate(` requires an actual call.
  local danger
  danger="$(grep -nE 'truncate\(|>[[:space:]]*"?\$\{?(cand|CAND|target|TARGET)' "$REAPER" \
            | grep -vE '^[0-9]+:[[:space:]]*#' || true)"
  if [ ! -d "$cache" ] && [ -s "$root/keep/big" ] \
     && [ "$reported" = "$expected" ] && [ -z "$danger" ] \
     && grep -q 'shutil.rmtree' "$REAPER"; then
    ok "unlink-only reclamation: hardlink survives, per-tree du total matches, no truncate"
  else
    notok "unlink-only + reporting" "cache exists=$([ -d "$cache" ] && echo y)" \
      "keep/big kept=$([ -s "$root/keep/big" ] && echo y)" \
      "reported=$reported expected=$expected" "danger=[$danger]"
  fi
  rm -rf "$root"
}

# ─── test 8: dry-run deletes nothing ───
test_dry_run_deletes_nothing() {
  local root; root="$(mktemp -d)"
  local cache; cache="$(stage_cache "$root" s target '30 days ago')"
  local out; out="$(run_reaper "$root" "$root/reap.log" --dry-run)"
  if [ -d "$cache" ] && printf '%s\n' "$out" | grep -q '\[REAP-dry\]' \
     && printf '%s\n' "$out" | grep -q 'dry-run: nothing deleted'; then
    ok "dry-run reports a plan but deletes nothing"
  else
    notok "dry-run" "cache exists=$([ -d "$cache" ] && echo y) (should be y)" "$out"
  fi
  rm -rf "$root"
}

test_matches
test_no_descend
test_age_gate
test_liveness_exe_skip
test_liveness_fd_skip
test_exempt_allowlist
test_unlink_only_and_reporting
test_dry_run_deletes_nothing

printf '1..%d\n' "$TEST_NUM"
[ "$FAIL" -eq 0 ] || { printf '# %d test(s) failed\n' "$FAIL"; exit 1; }
exit 0
