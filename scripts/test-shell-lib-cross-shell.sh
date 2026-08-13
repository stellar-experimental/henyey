#!/usr/bin/env bash
# test-shell-lib-cross-shell.sh — dual-shell (bash + zsh) regression guard for
# the sourced shell-helper layer under scripts/lib/.
#
# The project orchestrator and its sub-agents run under zsh, but scripts/lib/*.sh
# are authored for bash. Several latent cross-shell defects (zsh has no
# BASH_SOURCE; zsh does not word-split unquoted parameters by default; a fresh
# shell per Bash tool call breaks "source once" assumptions) can silently break
# pipeline behavior. This harness sources every SOURCED library under BOTH bash
# and zsh from a cwd != scripts/lib (with per-helper required-env stubs) and
# asserts clean rc + expected functions defined, plus three behavioral
# round-trips. The zsh leg TAP-skips if zsh is absent; CI installs zsh so the
# zsh leg actually runs there.
#
# Usage: bash scripts/test-shell-lib-cross-shell.sh
# Exit: 0 if all tests pass (or skip), 1 otherwise.
#
# Output: Test Anything Protocol (TAP).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LIB_DIR="$REPO_ROOT/scripts/lib"

# All scratch lives under a mktemp dir (CI ephemeral runner). Locally, honor a
# ~/data session dir if one is exported, to keep with the #2843 contract; else
# fall back to a system mktemp.
if [[ -n "${CLAUDE_SESSION_ID:-}" && -d "${HOME}/data" ]]; then
  SCRATCH="$(mktemp -d "${HOME}/data/${CLAUDE_SESSION_ID}/shell-lib-xshell.XXXXXX")"
else
  SCRATCH="$(mktemp -d)"
fi
trap 'rm -rf "$SCRATCH"' EXIT

# A foreign cwd (NOT scripts/lib) used for every source/exec, so that any
# self-location bug (resolving against the caller cwd instead of the script
# dir) is exposed.
FOREIGN_CWD="$SCRATCH"

# ─────────────────────────────────────────────────────────────────────────────
# Manifest: sourced libraries vs executable wrappers.
#
# SOURCED_LIBS — documented "source this" libraries with idempotent _LOADED
# guards. Sourced by skills/tests. The cross-shell test SOURCES these.
# EXEC_WRAPPERS — documented executable wrappers (# Usage:, set -euo pipefail,
# exec ...). These are RUN via their bash shebang, never sourced; the test only
# smoke-checks them (never sources).
# ─────────────────────────────────────────────────────────────────────────────
SOURCED_LIBS=(
  agent-worktree-contract.sh
  dedup-filing.sh
  deploy-quarantine.sh
  monitor-decisions.sh
  review-pr-merge.sh
  review-pr-verdicts.sh
  pipeline-anomaly-log.sh
)

EXEC_WRAPPERS=(
  eval-alarms.sh
)

# Expected functions per sourced library (a representative, load-bearing set;
# if any is missing after a clean source, the lib failed to define its surface).
expected_funcs_for() {
  case "$1" in
    agent-worktree-contract.sh)
      echo "_contract_real_home canonicalize_contract_path require_home_data_path require_session_prefix plan_critic_bootstrap review_pr_bootstrap do_bootstrap assert_no_repo_tree_scratch" ;;
    dedup-filing.sh)
      echo "dedup_load dedup_prune dedup_check dedup_record dedup_remove dedup_update_field dedup_write" ;;
    deploy-quarantine.sh)
      echo "parse_quarantine_file check_quarantine_active check_quarantine_ancestry quarantine_append quarantine_remove quarantine_resolve quarantine_autostamp quarantine_resolved_is_ve_green" ;;
    monitor-decisions.sh)
      echo "check_session_wiped check_long_stale_session detect_crash_state cleanup_guard prune_rotated_logs prune_metrics_archive" ;;
    review-pr-merge.sh)
      echo "attempt_merge classify_linked_pr_state is_auto_merge_armed has_armed_waiting_comment check_armed_pr_health" ;;
    review-pr-verdicts.sh)
      echo "fetch_reviewer_verdict_comments latest_reviewer_verdict_state validate_reviewer_verdict_shape classify_reviewer" ;;
    pipeline-anomaly-log.sh)
      echo "anomaly_log_path anomaly_log_append anomaly_log_dump anomaly_log_clear" ;;
    *)
      echo "" ;;
  esac
}

# Per-helper required-env stubs (Critic A): set before sourcing so a missing
# precondition is never mistaken for a BASH_SOURCE / cross-shell regression.
# Emitted as shell assignments prepended to the source command.
env_stubs_for() {
  case "$1" in
    # eval-alarms reads MONITOR_SESSION_ID; harmless to stub broadly even for
    # libs that don't read it.
    *)
      echo "export MONITOR_SESSION_ID=xshell-stub-session; export CLAUDE_SESSION_ID=\"\${CLAUDE_SESSION_ID:-xshell-stub-session}\";" ;;
  esac
}

# ─────────────────────────────────────────────────────────────────────────────
# TAP plumbing
# ─────────────────────────────────────────────────────────────────────────────
TEST_NUM=0
FAIL=0
HAVE_ZSH=0
if command -v zsh >/dev/null 2>&1; then HAVE_ZSH=1; fi

ok()   { TEST_NUM=$((TEST_NUM + 1)); printf 'ok %d - %s\n' "$TEST_NUM" "$1"; }
notok(){ TEST_NUM=$((TEST_NUM + 1)); FAIL=$((FAIL + 1)); printf 'not ok %d - %s\n' "$TEST_NUM" "$1"
         shift; for line in "$@"; do printf '# %s\n' "$line"; done; }
skip() { TEST_NUM=$((TEST_NUM + 1)); printf 'ok %d - %s # SKIP %s\n' "$TEST_NUM" "$1" "$2"; }

# run_in_shell SHELL_BIN CODE  — run CODE in a fresh `SHELL_BIN -c`, from the
# foreign cwd. Echoes nothing; sets globals RC and OUT (stdout+stderr merged is
# NOT what we want — we capture them separately).
# Sets: _RC, _STDOUT, _STDERR.
run_in_shell() {
  local shbin="$1" code="$2"
  local out_f err_f
  out_f="$(mktemp "$SCRATCH/out.XXXXXX")"
  err_f="$(mktemp "$SCRATCH/err.XXXXXX")"
  ( cd "$FOREIGN_CWD" && "$shbin" -c "$code" ) >"$out_f" 2>"$err_f"
  _RC=$?
  _STDOUT="$(cat "$out_f")"
  _STDERR="$(cat "$err_f")"
  rm -f "$out_f" "$err_f"
}

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 1: every SOURCED lib sources clean under bash AND zsh.
#   (all-sourced-libs-clean-source)
# Asserts: rc==0 AND empty stderr AND every expected function defined.
# ─────────────────────────────────────────────────────────────────────────────
assert_sourced_lib_clean() {
  local shbin="$1" lib="$2" shname="$3"
  local libpath="$LIB_DIR/$lib"
  local funcs; funcs="$(expected_funcs_for "$lib")"
  local stubs; stubs="$(env_stubs_for "$lib")"

  # Build a check that sources then verifies each expected function is defined.
  local checkcode="$stubs source '$libpath' || { echo 'SOURCE_RC_NONZERO' >&2; exit 3; };"
  local f
  for f in $funcs; do
    checkcode+=" command -v $f >/dev/null 2>&1 || { echo 'MISSING_FUNC:$f' >&2; exit 4; };"
  done
  checkcode+=" exit 0"

  run_in_shell "$shbin" "$checkcode"

  local label="source-clean[$shname]: $lib"
  if [[ "$_RC" -ne 0 ]]; then
    notok "$label" "rc=$_RC" "stderr: ${_STDERR}"
    return
  fi
  if [[ -n "$_STDERR" ]]; then
    notok "$label" "non-empty stderr on clean source:" "$_STDERR"
    return
  fi
  ok "$label"
}

for lib in "${SOURCED_LIBS[@]}"; do
  assert_sourced_lib_clean bash "$lib" bash
done
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  for lib in "${SOURCED_LIBS[@]}"; do
    assert_sourced_lib_clean zsh "$lib" zsh
  done
else
  for lib in "${SOURCED_LIBS[@]}"; do
    skip "source-clean[zsh]: $lib" "zsh not installed"
  done
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 2: dedup-zsh-script-path  (BUG-FIX regression test)
#   Source dedup-filing.sh from a foreign cwd; assert _DEDUP_SCRIPT resolves to
#   <repo>/scripts/lib/dedup-filing.py under BOTH shells. FAILS on main under
#   zsh (resolves to <cwd>/dedup-filing.py).
# ─────────────────────────────────────────────────────────────────────────────
EXPECTED_DEDUP="$LIB_DIR/dedup-filing.py"
assert_dedup_script_path() {
  local shbin="$1" shname="$2"
  run_in_shell "$shbin" "source '$LIB_DIR/dedup-filing.sh'; printf '%s' \"\$_DEDUP_SCRIPT\""
  local label="dedup-zsh-script-path[$shname]: _DEDUP_SCRIPT resolves to script dir"
  if [[ "$_RC" -ne 0 ]]; then
    notok "$label" "rc=$_RC" "stderr: $_STDERR"; return
  fi
  if [[ "$_STDOUT" != "$EXPECTED_DEDUP" ]]; then
    notok "$label" "expected: $EXPECTED_DEDUP" "got:      $_STDOUT"; return
  fi
  ok "$label"
}
assert_dedup_script_path bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_dedup_script_path zsh zsh
else
  skip "dedup-zsh-script-path[zsh]: _DEDUP_SCRIPT resolves to script dir" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 3: eval-alarms-zsh-no-paramnotset
#   Run `zsh scripts/lib/eval-alarms.sh` with a stubbed (non-existent) session;
#   assert stderr contains NO "BASH_SOURCE[0]: parameter not set". The python
#   invocation will fail (no metrics) — that's fine; we only assert the
#   self-location no longer aborts under zsh. FAILS on main.
# ─────────────────────────────────────────────────────────────────────────────
assert_eval_alarms_no_paramnotset() {
  local shbin="$1" shname="$2"
  local wrapper="$LIB_DIR/eval-alarms.sh"
  local err_f; err_f="$(mktemp "$SCRATCH/ea-err.XXXXXX")"
  ( cd "$FOREIGN_CWD" && MONITOR_SESSION_ID="xshell-nonexistent-$$" "$shbin" "$wrapper" ) >/dev/null 2>"$err_f"
  local stderr; stderr="$(cat "$err_f")"; rm -f "$err_f"
  local label="eval-alarms-no-paramnotset[$shname]: no BASH_SOURCE parameter-not-set abort"
  if printf '%s' "$stderr" | grep -qF "BASH_SOURCE[0]: parameter not set"; then
    notok "$label" "stderr contained BASH_SOURCE parameter-not-set:" "$stderr"
    return
  fi
  ok "$label"
}
assert_eval_alarms_no_paramnotset bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_eval_alarms_no_paramnotset zsh zsh
else
  skip "eval-alarms-no-paramnotset[zsh]: no BASH_SOURCE parameter-not-set abort" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 4: functions-defined-both-shells (anti-regression for the
#   already-landed agent-worktree-contract.sh zsh fix).
#   Covered by assert_sourced_lib_clean for the contract helper, but we add an
#   explicit dedicated assertion so the named guard is unmistakable in TAP.
# ─────────────────────────────────────────────────────────────────────────────
assert_contract_funcs_defined() {
  local shbin="$1" shname="$2"
  local funcs; funcs="$(expected_funcs_for agent-worktree-contract.sh)"
  local code="source '$LIB_DIR/agent-worktree-contract.sh' || exit 3;"
  local f
  for f in $funcs; do
    code+=" command -v $f >/dev/null 2>&1 || { echo MISSING:$f; exit 4; };"
  done
  code+=" echo ALL_DEFINED"
  run_in_shell "$shbin" "$code"
  local label="contract-functions-defined[$shname]"
  if [[ "$_RC" -eq 0 && "$_STDOUT" == "ALL_DEFINED" ]]; then
    ok "$label"
  else
    notok "$label" "rc=$_RC" "stdout: $_STDOUT" "stderr: $_STDERR"
  fi
}
assert_contract_funcs_defined bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_contract_funcs_defined zsh zsh
else
  skip "contract-functions-defined[zsh]" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 5: merge-flags-separated-both-shells (anti-regression #2954).
#   attempt_merge → _review_pr_exec_merge must pass --squash and --admin as TWO
#   distinct args, not one "--squash --admin" string. A REVIEW_PR_MERGE_CMD mock
#   records ARGC and the individual flags. Under correct behavior the mock sees
#   pr_num, repo, --squash, --admin → ARGC=4 with the two flags distinct.
# ─────────────────────────────────────────────────────────────────────────────
assert_merge_flags_separated() {
  local shbin="$1" shname="$2"
  local probe="$SCRATCH/merge-probe-$shname.txt"
  rm -f "$probe"
  # Mock: a function that writes ARGC and each arg on its own line, then exits 0
  # so attempt_merge treats it as a successful merge.
  local code="
    export REVIEW_PR_SCRATCH_DIR='$SCRATCH/merge-scratch-$shname';
    mkdir -p \"\$REVIEW_PR_SCRATCH_DIR\";
    source '$LIB_DIR/review-pr-merge.sh';
    mock_merge() { { echo \"ARGC=\$#\"; for a in \"\$@\"; do echo \"ARG=\$a\"; done; } >'$probe'; return 0; }
    export REVIEW_PR_MERGE_CMD=mock_merge;
    out=\"\$(attempt_merge 4242)\";
    echo \"OUT=\$out\"
  "
  run_in_shell "$shbin" "$code"
  local label="merge-flag-separation[$shname]: --squash and --admin arrive as distinct args"
  if [[ ! -f "$probe" ]]; then
    notok "$label" "mock probe file not written" "rc=$_RC stdout=$_STDOUT stderr=$_STDERR"
    return
  fi
  local argc squash admin
  argc="$(grep -E '^ARGC=' "$probe" | head -1 | cut -d= -f2)"
  squash="$(grep -cxF 'ARG=--squash' "$probe")"
  admin="$(grep -cxF 'ARG=--admin' "$probe")"
  if [[ "$argc" == "4" && "$squash" == "1" && "$admin" == "1" ]]; then
    ok "$label"
  else
    notok "$label" "ARGC=$argc (want 4)" "--squash count=$squash --admin count=$admin (want 1/1)" "probe:" "$(cat "$probe")"
  fi
}
assert_merge_flags_separated bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_merge_flags_separated zsh zsh
else
  skip "merge-flag-separation[zsh]: --squash and --admin arrive as distinct args" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 5.5: quarantine-autostamp-roundtrip-both-shells (#3258).
#   quarantine_autostamp parses the crash issue # from an entry's reason,
#   queries gh for the merged closing PR's merge SHA, and stamps resolved:<sha>
#   via quarantine_resolve. Its inner loop reads THREE index-aligned globals in
#   lockstep over FDs 3/4/5 — the highest cross-shell risk in the new code.
#   This roundtrip mocks `gh` (so no network), runs autostamp under each shell
#   from a foreign cwd, and asserts the resolved token was stamped onto the
#   entry. The gh mock mirrors the real --jq shapes: issue-view emits the PR #,
#   pr-view emits the 40-hex merge SHA only when state==MERGED.
# ─────────────────────────────────────────────────────────────────────────────
assert_autostamp_roundtrip() {
  local shbin="$1" shname="$2"
  local qfile="$SCRATCH/autostamp-$shname.txt"
  local bad="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local fix="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  printf '%s regression #4242\n' "$bad" > "$qfile"
  # Mock gh: issue view → PR number; pr view → merge SHA (state==MERGED).
  local code="
    source '$LIB_DIR/deploy-quarantine.sh' || exit 3;
    gh() {
      case \"\$1\" in
        issue) printf '%s\n' 707 ;;
        pr) printf '%s\n' '$fix' ;;
        *) return 0 ;;
      esac
    };
    quarantine_autostamp '$qfile';
    parse_quarantine_file '$qfile';
    printf '%s' \"\$QUARANTINE_RESOLVED\"
  "
  run_in_shell "$shbin" "$code"
  local label="quarantine-autostamp-roundtrip[$shname]: reason #N → stamps resolved:<merge-sha>"
  if [[ "$_RC" -ne 0 ]]; then
    notok "$label" "rc=$_RC" "stderr: $_STDERR"; return
  fi
  if [[ "$_STDOUT" != "$fix" ]]; then
    notok "$label" "expected resolved: $fix" "got: '$_STDOUT'"; return
  fi
  ok "$label"
}
assert_autostamp_roundtrip bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_autostamp_roundtrip zsh zsh
else
  skip "quarantine-autostamp-roundtrip[zsh]: reason #N → stamps resolved:<merge-sha>" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 5.6: quarantine-hold-until-both-shells (#3711).
#   check_quarantine_active's hold:until-#<N> sentinel pins an entry to a
#   GitHub issue's lifecycle: an OPEN (or indeterminate) issue must BLOCK
#   (fail-closed) even when the per-hunk content-check would CLEAR (drifted
#   diff — the #3711 false-clear); a confirmed CLOSED issue releases the
#   sentinel and falls through to the normal logic. The gh call goes through
#   `timeout 15 gh ...` — timeout exec()s a REAL binary from PATH, so a
#   shell-function gh mock is invisible to it and the test uses a PATH shim
#   (driven by exported _GH_HOLD_STATE). git IS mockable as a function: the
#   sha is an ancestor and every hunk fails to reverse-apply, so WITHOUT the
#   sentinel the entry clears.
# ─────────────────────────────────────────────────────────────────────────────
HOLD_SHIM_DIR="$SCRATCH/hold-gh-shim"
mkdir -p "$HOLD_SHIM_DIR"
cat > "$HOLD_SHIM_DIR/gh" <<'SHIM'
#!/usr/bin/env bash
[ "${_GH_HOLD_RC:-0}" -ne 0 ] && exit "${_GH_HOLD_RC}"
[ -n "${_GH_HOLD_STATE:-}" ] && printf '%s\n' "$_GH_HOLD_STATE"
exit 0
SHIM
chmod +x "$HOLD_SHIM_DIR/gh"

assert_hold_until_roundtrip() {
  local shbin="$1" shname="$2"
  local qfile="$SCRATCH/hold-$shname.txt"
  local bad="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  printf '%s hold:until-#3702 restore-from-disk wedge hold\n' "$bad" > "$qfile"
  local code="
    export PATH='$HOLD_SHIM_DIR':\"\$PATH\";
    source '$LIB_DIR/deploy-quarantine.sh' || exit 3;
    git() {
      case \"\$1\" in
        merge-base) return 0 ;;
        diff) printf 'diff --git a/f.rs b/f.rs\nindex 1111111..2222222 100644\n--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,3 @@ mod m {\n+    drifted_away\n }\n'; return 0 ;;
        apply) return 1 ;;
        *) return 0 ;;
      esac
    };
    export _GH_HOLD_STATE=OPEN;
    rc_open=0; check_quarantine_active '$qfile' || rc_open=\$?;
    open_status=\$QUARANTINE_STATUS;
    export _GH_HOLD_STATE=CLOSED;
    rc_closed=0; check_quarantine_active '$qfile' || rc_closed=\$?;
    printf 'open=%s/%s closed=%s/%s' \"\$rc_open\" \"\$open_status\" \"\$rc_closed\" \"\$QUARANTINE_STATUS\"
  "
  run_in_shell "$shbin" "$code"
  local label="quarantine-hold-until[$shname]: OPEN blocks (fail-closed), CLOSED falls through"
  if [[ "$_RC" -eq 0 && "$_STDOUT" == "open=0/blocked_active closed=1/clear" ]]; then
    ok "$label"
  else
    notok "$label" "rc=$_RC" "stdout: $_STDOUT" "stderr: $_STDERR"
  fi
}
assert_hold_until_roundtrip bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_hold_until_roundtrip zsh zsh
else
  skip "quarantine-hold-until[zsh]: OPEN blocks (fail-closed), CLOSED falls through" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 5.7: prune-metrics-archive-roundtrip-both-shells (#3724).
#   prune_metrics_archive orders the archive by MTIME (array-free single find),
#   so it evicts the true-oldest snapshots even when the archive holds mixed
#   timestamp-name formats — a lexical name sort put every dash-format dir ahead
#   of every compact-format dir and deleted the newest data. The array-free form
#   is the load-bearing zsh guard: the old inline snippet indexed a bash array
#   from 0, but zsh arrays are 1-indexed, so `${SNAPSHOTS[0]}` was empty and one
#   deletion no-op'd every tick. This roundtrip builds a mixed-format fixture
#   (500 recent dash dirs + 2 old compact dirs, mtimes inverted vs names), runs
#   prune under each shell, and asserts EXACTLY the 2 oldest-mtime (compact)
#   dirs are evicted and all 500 recent dash dirs survive — identical count and
#   eviction set under bash and zsh.
# ─────────────────────────────────────────────────────────────────────────────
assert_prune_metrics_archive_roundtrip() {
  local shbin="$1" shname="$2"
  local adir="$SCRATCH/prune-archive-$shname"
  rm -rf "$adir"; mkdir -p "$adir"
  local base=1700000000 i
  # 500 recent dash-format dirs (newest mtime), lexically FIRST.
  for i in $(seq 1 500); do
    local d="$adir/2026-07-16T00:00:00.$(printf '%09d' "$i")Z"
    mkdir -p "$d"
    touch -d "@$(( base + 100000 + i ))" "$d"
  done
  # 2 compact-format dirs — lexically LAST ('2' > '-') but OLDEST by mtime.
  local c1="$adir/20260617T000000Z" c2="$adir/20260618T000000Z"
  mkdir -p "$c1" "$c2"
  touch -d "@$(( base + 1 ))" "$c1"
  touch -d "@$(( base + 2 ))" "$c2"

  local code="
    source '$LIB_DIR/monitor-decisions.sh' || exit 3;
    prune_metrics_archive '$adir' 500;
    printf '%s' \"\$PRUNED_ARCHIVE_COUNT\"
  "
  run_in_shell "$shbin" "$code"
  local label="prune-metrics-archive-roundtrip[$shname]: mtime-sort evicts oldest compact, keeps 500 recent dash"
  if [[ "$_RC" -ne 0 ]]; then
    notok "$label" "rc=$_RC" "stderr: $_STDERR"; return
  fi
  local remaining
  remaining=$(find "$adir" -maxdepth 1 -mindepth 1 -type d | wc -l | tr -d ' ')
  if [[ "$_STDOUT" == "2" && "$remaining" == "500" && ! -d "$c1" && ! -d "$c2" ]]; then
    ok "$label"
  else
    notok "$label" "PRUNED_ARCHIVE_COUNT=$_STDOUT (want 2)" "remaining=$remaining (want 500)" \
      "compact1 exists=$( [[ -d "$c1" ]] && echo yes || echo no )" \
      "compact2 exists=$( [[ -d "$c2" ]] && echo yes || echo no )"
  fi
}
assert_prune_metrics_archive_roundtrip bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_prune_metrics_archive_roundtrip zsh zsh
else
  skip "prune-metrics-archive-roundtrip[zsh]: mtime-sort evicts oldest compact, keeps 500 recent dash" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 6: anomaly-roundtrip-fresh-shells-both (anti-regression #3133).
#   Append in one fresh shell, dump in a SECOND fresh shell (separate process),
#   both pointed at the same PIPELINE_ANOMALY_LOG; assert the line persists.
# ─────────────────────────────────────────────────────────────────────────────
assert_anomaly_roundtrip() {
  local shbin="$1" shname="$2"
  local logf="$SCRATCH/anomaly-$shname.log"
  rm -f "$logf"
  local marker="xshell-roundtrip-$shname-$$"
  # Fresh shell #1: append.
  run_in_shell "$shbin" "
    export PIPELINE_ANOMALY_LOG='$logf';
    source '$LIB_DIR/pipeline-anomaly-log.sh';
    anomaly_log_append 'merge-helper-fallback' '$marker' || { echo APPEND_FAIL >&2; exit 5; }
  "
  if [[ "$_RC" -ne 0 ]]; then
    notok "anomaly-log-roundtrip[$shname]: append/dump persists across fresh shells" \
      "append rc=$_RC" "stderr: $_STDERR"
    return
  fi
  # Fresh shell #2 (separate process): dump.
  run_in_shell "$shbin" "
    export PIPELINE_ANOMALY_LOG='$logf';
    source '$LIB_DIR/pipeline-anomaly-log.sh';
    anomaly_log_dump
  "
  local label="anomaly-log-roundtrip[$shname]: append/dump persists across fresh shells"
  if [[ "$_RC" -eq 0 ]] && printf '%s' "$_STDOUT" | grep -qF "$marker"; then
    ok "$label"
  else
    notok "$label" "dump rc=$_RC" "dump did not contain marker '$marker'" "stdout: $_STDOUT" "stderr: $_STDERR"
  fi
}
assert_anomaly_roundtrip bash bash
if [[ "$HAVE_ZSH" -eq 1 ]]; then
  assert_anomaly_roundtrip zsh zsh
else
  skip "anomaly-log-roundtrip[zsh]: append/dump persists across fresh shells" "zsh not installed"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Assertion group 7: executable wrappers smoke-check (never sourced).
#   For each EXEC_WRAPPER, `bash -n` parses cleanly (syntax check). We do NOT
#   source these (they exec and replace the shell).
# ─────────────────────────────────────────────────────────────────────────────
for w in "${EXEC_WRAPPERS[@]}"; do
  if bash -n "$LIB_DIR/$w" 2>/dev/null; then
    ok "exec-wrapper-parses[bash -n]: $w"
  else
    notok "exec-wrapper-parses[bash -n]: $w" "bash -n reported a syntax error"
  fi
done

# ─────────────────────────────────────────────────────────────────────────────
# TAP plan line + exit.
# ─────────────────────────────────────────────────────────────────────────────
printf '1..%d\n' "$TEST_NUM"
if [[ "$HAVE_ZSH" -eq 0 ]]; then
  printf '# NOTE: zsh not installed — zsh legs were SKIPPED. CI installs zsh so the zsh leg runs there.\n'
fi
if [[ "$FAIL" -gt 0 ]]; then
  printf '# FAILED: %d test(s) failed\n' "$FAIL"
  exit 1
fi
printf '# All %d tests passed\n' "$TEST_NUM"
exit 0
