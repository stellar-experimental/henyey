#!/usr/bin/env bash
#
# Integration test: run a henyey testnet validator that publishes history,
# then compare the published checkpoint against SDF's testnet archive.
#
# Usage:
#   ./scripts/test-history-publish.sh                   # build + run
#   ./scripts/test-history-publish.sh --no-build        # skip cargo build
#   ./scripts/test-history-publish.sh --timeout 1200    # wait up to 20 min
#   ./scripts/test-history-publish.sh --checkpoint 63   # compare specific checkpoint
#   ./scripts/test-history-publish.sh --data-dir /tmp/x # use specific data directory
#   ./scripts/test-history-publish.sh --soft-on-sync-timeout  # neutral skip if testnet never synced
#   ./scripts/test-history-publish.sh --classify-only /path/to/data-dir  # offline classifier (tests)
#   ./scripts/test-history-publish.sh --stall-threshold 180   # in-loop stall detector threshold
#   ./scripts/test-history-publish.sh --classify-stall-only /path/to/validator.log 180  # offline stall classifier (tests)
#   ./scripts/test-history-publish.sh --emit-new-lines-only /path/to/validator.log 0  # offline live-tail diagnostic emitter (tests)
#
# Exit codes:
#   0 = checkpoint matches SDF archive (or an environmental sync-timeout was
#       soft-skipped under --soft-on-sync-timeout)
#   1 = mismatch, real error, or a genuine publish regression (synced but no
#       checkpoint published; see the disposition taxonomy below)
#
# --- In-loop stall detector + live diagnostic tail (#3741) ---
# #3732 disabled this fixture's internal watchdog auto-abort, which correctly
# stopped a still-progressing node from self-SIGABRTing under legitimate
# slow-verify load — but it also removed the only code path that turned "event
# loop badly stalled" into a clean, script-detected process death. Without it,
# a genuinely stalled-but-alive node just keeps grinding until GitHub Actions'
# own external job/run-level cancellation kills the whole runner — which skips
# even if: always()-gated steps, so Upload logs/Summary never run and NO
# validator.log evidence survives (confirmed on all 4 recurrences of this
# fixture's "waiting for first published checkpoint" symptom: #3280, #3707,
# #3727, #3741).
#
# Two independent, complementary mitigations (neither fixes the underlying
# SCP-verify/WAL-contention capacity problem — that stays #3702/#3266's scope):
#   1. Live diagnostic tail: every ~5s poll iteration, new validator.log lines
#      are grep-filtered (WATCHDOG|maxtps_scp|database is locked|straggler
#      timeout|db_write_ctx) and echoed into the step's own stdout, which
#      GitHub Actions captures even under an external job-level cancellation
#      (unlike the artifact upload, which does not survive it).
#   2. Stall-based clean exit: is_log_stalled() checks validator.log's mtime
#      against --stall-threshold (default 180s). If the node is still alive
#      (kill -0 succeeds) but the log has produced zero new bytes for that
#      long, this is treated as a genuine stall — DISPOSITION: STALL, always a
#      hard red (exit 1, never covered by --soft-on-sync-timeout), so the
#      workflow's own `if: failure()` Upload-logs step fires and the full
#      validator.log artifact is captured too.
#
# 180s was chosen against two observed external-cancellation timings for this
# fixture (~287s and ~1437s) and against the periodic progress-logging cadence
# already present during healthy-but-slow catchup (bucket-download progress,
# ledger-prefetch progress, and the watchdog's own 15s/30s warn/error tiers) —
# none of which leave a genuine 180s silent gap. Configurable via
# --stall-threshold without a code change if real-world tuning is needed.
#
# --- Phase-1 deadline disposition taxonomy (#3280) ---
# The test has two phases: phase 1 waits up to --timeout for the node to sync
# testnet and publish its first checkpoint (HAS); phase 2 byte-compares that
# checkpoint against SDF's live testnet archive (a parity cross-check no in-repo
# unit test replicates — it is ALWAYS a hard red on mismatch, never soft-skipped).
#
# The phase-1 deadline used to be a single blanket `exit 1`, which conflated two
# very different outcomes. It is now classified into three dispositions, decided
# from concrete on-disk signals (a published HAS, and the node's sync marker in
# validator.log):
#   1. PUBLISHED          — a checkpoint was published in time. Proceed to the
#                           (unchanged) phase-2 compare. Mismatch => hard red.
#   2. PUBLISH-REGRESSION — the node SYNCED (validator.log contains the run-loop
#                           sync marker "Node is synced", emitted by
#                           crates/app/src/run_cmd.rs::wait_for_sync) but published
#                           NO checkpoint by the deadline. This is a REAL publish
#                           regression — the issue's literal symptom — and stays a
#                           HARD RED (exit 1) even under --soft-on-sync-timeout.
#   3. SYNC-TIMEOUT       — the node NEVER reached sync by the deadline. This is
#                           an environmental testnet-liveness timeout (the chronic
#                           flake #3280 tracks: the same commit passes/fails on
#                           different days purely on testnet health). With
#                           --soft-on-sync-timeout it is a neutral exit 0 with a
#                           grep-able SOFT-SKIP marker (reusing the #3272/#3273
#                           SOFT-SKIP convention); WITHOUT the flag (the default,
#                           so local/dev behavior is byte-identical) it stays a
#                           hard red exit 1.
#
# The sync marker grep is pinned by scripts/test-history-publish-harness.sh so a
# future change to the run-loop log wording breaks the harness loudly instead of
# silently inverting dispositions 2 and 3.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$PROJECT_ROOT/target/release/henyey"

# Defaults
DO_BUILD=true
TIMEOUT=1200        # 20 minutes max wait for first checkpoint
CHECKPOINT=""      # auto-detect from published HAS
KEEP_DATA=false
DATA_DIR_OVERRIDE=""  # if set, use this instead of auto-generated path
# Opt-in (#3280, default OFF): convert a phase-1 SYNC-TIMEOUT disposition (node
# never reached sync by the deadline) into a neutral exit 0 + SOFT-SKIP marker.
# Default OFF => byte-identical local/dev behavior; a synced-but-no-publish
# PUBLISH-REGRESSION and a phase-2 compare mismatch ALWAYS stay hard red.
SOFT_ON_SYNC_TIMEOUT=false
# Offline test seam: when set, run only the deadline classifier against an
# existing data-dir (a validator.log + optional published HAS) and exit with the
# classified disposition — no validator/build/testnet required. Used by
# scripts/test-history-publish-harness.sh.
CLASSIFY_ONLY_DIR=""
# In-loop stall detector (#3741): if validator.log produces zero new bytes for
# this many seconds while the node process is still alive, the poll loop treats
# it as a genuine stall — DISPOSITION: STALL, always a hard red (never covered
# by --soft-on-sync-timeout; see the header comment above). Overridable via
# --stall-threshold.
STALL_THRESHOLD_SECS=180
# Offline test seam: when set (via --classify-stall-only <log_file>
# <threshold_secs>), run only is_log_stalled() against an existing log file and
# exit with the classified disposition — no validator/build/testnet required.
# Used by scripts/test-history-publish-harness.sh.
CLASSIFY_STALL_LOG=""
CLASSIFY_STALL_THRESHOLD=""
# Offline test seam: when set (via --emit-new-lines-only <log_file> <last_line>)
# run only emit_new_diagnostic_lines() against an existing log file, echo the
# new diagnostic-filtered slice, and print the new absolute offset as
# `NEW_LAST_LINE: <n>` — no validator/build/testnet required. Used by
# scripts/test-history-publish-harness.sh.
EMIT_NEW_LOG=""
EMIT_NEW_LAST_LINE=""

# Live diagnostic tail (#3741): grep filter for the known diagnostic families
# already root-caused for this exact fixture (#3727: SCP-verify falling behind
# under load) and the #3702 db_write_ctx/WAL-contention instrumentation.
# Grep-filtered (not a full firehose) so it doesn't meaningfully add to CI log
# volume or runner I/O load. Pinned by
# scripts/test-history-publish-harness.sh::test_diagnostic_grep_pins_known_signal_names
# so a future wording change in the Rust source breaks that test loudly instead
# of silently rotting this filter into a no-op. Defined here (up in the config
# region) so both the --emit-new-lines-only seam and the poll loop share one
# definition.
DIAG_GREP_PATTERN='WATCHDOG|maxtps_scp|database is locked|straggler timeout|db_write_ctx'

# The exact run-loop sync marker (crates/app/src/run_cmd.rs::wait_for_sync logs
# tracing::info!(..., "Node is synced") once on reaching Synced/Validating). This
# is the single source of truth for "the node reached sync"; pinned by the
# harness so a log-wording drift is caught loudly. See the taxonomy header.
SYNC_MARKER="Node is synced"

# SDF testnet reference archive
SDF_ARCHIVE="https://history.stellar.org/prd/core-testnet/core_testnet_001"

# Parse args
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build)             DO_BUILD=false; shift ;;
    --timeout)              TIMEOUT="$2"; shift 2 ;;
    --checkpoint)           CHECKPOINT="$2"; shift 2 ;;
    --keep-data)            KEEP_DATA=true; shift ;;
    --data-dir)             DATA_DIR_OVERRIDE="$2"; KEEP_DATA=true; shift 2 ;;
    --soft-on-sync-timeout) SOFT_ON_SYNC_TIMEOUT=true; shift ;;
    --classify-only)        CLASSIFY_ONLY_DIR="$2"; shift 2 ;;
    --stall-threshold)      STALL_THRESHOLD_SECS="$2"; shift 2 ;;
    --classify-stall-only)  CLASSIFY_STALL_LOG="$2"; CLASSIFY_STALL_THRESHOLD="$3"; shift 3 ;;
    --emit-new-lines-only)  EMIT_NEW_LOG="$2"; EMIT_NEW_LAST_LINE="$3"; shift 3 ;;
    -h|--help)
      sed -n '3,14p' "$0" | sed 's/^# \?//'
      exit 0 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

# --- Phase-1 deadline classifier (the disposition seam, #3280) ---
# Decide the phase-1 deadline disposition for <data-dir> from concrete on-disk
# signals, honoring $SOFT_ON_SYNC_TIMEOUT. Emits a single grep-able disposition
# line and returns:
#   PUBLISHED          -> return 0 (caller proceeds to phase-2 compare)
#   PUBLISH-REGRESSION -> return 1 (synced but no checkpoint: a real regression)
#   SYNC-TIMEOUT       -> return 0 if --soft-on-sync-timeout (emits SOFT-SKIP),
#                         else return 1 (byte-identical default hard red)
# Kept as a standalone function with no side effects beyond stdout so the harness
# can drive it offline via --classify-only.
classify_deadline() {
  local data_dir="$1"
  local log_file="$data_dir/validator.log"
  local has_file="$data_dir/history/.well-known/stellar-history.json"

  # Disposition 1: a checkpoint was published (HAS present, currentLedger > 0).
  if [[ -f "$has_file" ]]; then
    local current_ledger
    current_ledger=$(jq -r '.currentLedger' "$has_file" 2>/dev/null || echo "0")
    if [[ "$current_ledger" =~ ^[0-9]+$ ]] && [[ "$current_ledger" -gt 0 ]]; then
      echo "DISPOSITION: PUBLISHED (currentLedger=$current_ledger) — proceeding to phase-2 compare"
      return 0
    fi
  fi

  # Did the node reach sync? Grep the run-loop sync marker in validator.log.
  local synced=false
  if [[ -f "$log_file" ]] && grep -qF "$SYNC_MARKER" "$log_file"; then
    synced=true
  fi

  if [[ "$synced" == true ]]; then
    # Disposition 2: synced but no checkpoint published — a REAL publish
    # regression. Always a hard red, even under --soft-on-sync-timeout.
    echo "DISPOSITION: PUBLISH-REGRESSION — node reached sync ('$SYNC_MARKER') but published no checkpoint by the deadline (hard red, #3280)"
    return 1
  fi

  # Disposition 3: never reached sync — environmental testnet-liveness timeout.
  if [[ "$SOFT_ON_SYNC_TIMEOUT" == true ]]; then
    echo "=== SOFT-SKIP: testnet never reached sync by the deadline (environmental, not a henyey publish failure) ===" >&2
    echo "=== SOFT-SKIP: DISPOSITION SYNC-TIMEOUT treated as neutral (#3280); validator.log/HAS artifacts preserved ===" >&2
    echo "DISPOSITION: SYNC-TIMEOUT (soft-skipped) — node never reached sync; neutral exit 0 under --soft-on-sync-timeout"
    return 0
  fi
  echo "DISPOSITION: SYNC-TIMEOUT — node never reached sync by the deadline (hard red; pass --soft-on-sync-timeout to neutralize, #3280)"
  return 1
}

# Offline test seam: classify an existing data-dir and exit. No build/validator.
if [[ -n "$CLASSIFY_ONLY_DIR" ]]; then
  CLASSIFY_EXIT=0
  classify_deadline "$CLASSIFY_ONLY_DIR" || CLASSIFY_EXIT=$?
  exit $CLASSIFY_EXIT
fi

# --- In-loop stall detector (#3741) ---
# Pure mtime-staleness check: has <log_file> produced zero new bytes for at
# least <threshold_secs>? No side effects beyond the return code, so the
# harness can drive it offline via --classify-stall-only against a synthetic
# fixture with a manipulated mtime (`touch -d`).
#
# Return convention (bash truthiness: 0 = success = "true"):
#   0 -> stalled     (log_file missing its mtime freshness bar)
#   1 -> not stalled (log_file updated within threshold_secs, or unreadable)
# The CLI seam below and the poll-loop call site both treat a 0 return as "the
# node is stalled" and act accordingly (DISPOSITION: STALL, hard exit 1).
is_log_stalled() {
  local log_file="$1"
  local now_epoch="$2"
  local threshold_secs="$3"

  if [[ ! -f "$log_file" ]]; then
    # No log yet at all — not a "stall" in the sense this detector cares
    # about; the pre-existing process-liveness and HAS-file checks in the
    # poll loop handle "nothing has happened yet" separately.
    return 1
  fi

  local mtime
  mtime=$(stat -c %Y "$log_file" 2>/dev/null) || return 1

  local age=$(( now_epoch - mtime ))
  if [[ $age -ge $threshold_secs ]]; then
    return 0
  fi
  return 1
}

# Offline test seam: classify a single validator.log for staleness and exit.
# --classify-stall-only <log_file> <threshold_secs>
#   exit 0 -> not stalled
#   exit 1 -> stalled (DISPOSITION: STALL, tail printed)
# No build/validator required.
if [[ -n "$CLASSIFY_STALL_LOG" ]]; then
  if is_log_stalled "$CLASSIFY_STALL_LOG" "$(date +%s)" "$CLASSIFY_STALL_THRESHOLD"; then
    echo "Last 200 lines of $CLASSIFY_STALL_LOG:"
    tail -200 "$CLASSIFY_STALL_LOG" 2>/dev/null || true
    echo
    echo "DISPOSITION: STALL — validator.log produced no new output for >= ${CLASSIFY_STALL_THRESHOLD}s (hard red, #3741)"
    exit 1
  fi
  echo "DISPOSITION: not stalled — validator.log updated within ${CLASSIFY_STALL_THRESHOLD}s"
  exit 0
fi

# --- Incremental live-tail diagnostic echo (#3741, #3745) ---
# Read the new slice of <log_file> starting after <last_line>, echo the lines
# matching <pattern> to stdout, and report the new absolute line offset via the
# LAST_LOG_LINE_OUT global. Pure except for that stdout + the one global, so the
# harness can drive it offline via --emit-new-lines-only.
#
# The offset is derived from EXACTLY the slice that was read in a SINGLE `tail`
# (not from an independent second `wc -l < file`), which is the fix for the
# #3745 TOCTOU: with only one read there is no window for the log to grow
# between a line-count and the tail, so the recorded offset can never lag what
# was actually emitted and already-seen lines are never reprinted. Anything
# appended after this single read is simply picked up on the next poll.
emit_new_diagnostic_lines() {
  local log_file="$1"
  local last_line="$2"
  local pattern="$3"

  local slice
  slice=$(tail -n "+$(( last_line + 1 ))" "$log_file" 2>/dev/null || true)

  if [[ -z "$slice" ]]; then
    # No new content (or unreadable log) — no advance, nothing printed. Mirrors
    # the old `CURRENT_LOG_LINES > LAST_LOG_LINE` guard's no-op branch.
    LAST_LOG_LINE_OUT="$last_line"
    return 0
  fi

  # `consumed` counts the lines in the captured slice. Note bash `$(...)` strips
  # trailing newlines, so trailing blank line(s) are under-counted and may be
  # re-read next poll — harmless, since blank lines never match the filter, so
  # no visible duplicate.
  local consumed
  consumed=$(printf '%s\n' "$slice" | wc -l)
  LAST_LOG_LINE_OUT=$(( last_line + consumed ))

  printf '%s\n' "$slice" | grep -E "$pattern" || true
}

# Offline test seam: emit the new diagnostic-filtered slice of a single log
# starting after <last_line>, print the new absolute offset, and exit 0.
# --emit-new-lines-only <log_file> <last_line>
if [[ -n "$EMIT_NEW_LOG" ]]; then
  LAST_LOG_LINE_OUT=0
  emit_new_diagnostic_lines "$EMIT_NEW_LOG" "$EMIT_NEW_LAST_LINE" "$DIAG_GREP_PATTERN"
  echo "NEW_LAST_LINE: $LAST_LOG_LINE_OUT"
  exit 0
fi

# --- Data dirs ---
if [[ -n "$DATA_DIR_OVERRIDE" ]]; then
  DATA_DIR="$DATA_DIR_OVERRIDE"
else
  DATA_DIR="$PROJECT_ROOT/data/publish-test-$$"
fi
HISTORY_DIR="$DATA_DIR/history"
DB_PATH="$DATA_DIR/validator.db"
BUCKET_DIR="$DATA_DIR/buckets"
CONFIG_FILE="$DATA_DIR/validator.toml"
LOG_FILE="$DATA_DIR/validator.log"
NODE_PID=""

mkdir -p "$HISTORY_DIR" "$BUCKET_DIR"

cleanup() {
  if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
    echo "Stopping validator (pid $NODE_PID)..."
    kill "$NODE_PID" 2>/dev/null || true
    wait "$NODE_PID" 2>/dev/null || true
  fi
  if [[ "$KEEP_DATA" == "false" ]]; then
    echo "Cleaning up $DATA_DIR"
    rm -rf "$DATA_DIR"
  else
    echo "Data kept at $DATA_DIR"
  fi
}
trap cleanup EXIT

# --- Build ---
if [[ "$DO_BUILD" == "true" ]]; then
  echo "Building henyey (release)..."
  cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" -p henyey 2>&1
  echo "Build complete."
  echo
fi

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Binary not found at $BINARY"
  echo "Run with --no-build only if already built."
  exit 1
fi

# --- Generate ephemeral node seed ---
# We need a keypair for the validator. Generate one.
SEED_OUTPUT=$("$BINARY" new-keypair 2>&1)
NODE_SEED=$(echo "$SEED_OUTPUT" | grep -oP 'S[A-Z0-9]{55}' | head -1)
if [[ -z "$NODE_SEED" ]]; then
  echo "ERROR: Failed to generate node keypair"
  echo "$SEED_OUTPUT"
  exit 1
fi
echo "Generated ephemeral node seed: ${NODE_SEED:0:4}..."

# Pick a random high port for the overlay listener
PEER_PORT=$((30000 + RANDOM % 10000))
echo "Using overlay peer port: $PEER_PORT"

# --- Generate config ---
# Render configs/test-history-publish.toml with runtime values.
# That fixture is covered by crates/app/src/config.rs::test_shipped_config_files_parse,
# so any future schema-narrowing change is caught at unit-test time rather
# than silently in nightly History Publish.
TEMPLATE="$PROJECT_ROOT/configs/test-history-publish.toml"
if [[ ! -f "$TEMPLATE" ]]; then
  echo "ERROR: template not found at $TEMPLATE"
  exit 1
fi

# Escape sed replacement metacharacters: \, &, and | (our delimiter).
# Required because path values come from --data-dir and are caller-controlled.
escape_sed_repl() {
  printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

NODE_SEED_ESC=$(escape_sed_repl "$NODE_SEED")
DB_PATH_ESC=$(escape_sed_repl "$DB_PATH")
BUCKET_DIR_ESC=$(escape_sed_repl "$BUCKET_DIR")
HISTORY_DIR_ESC=$(escape_sed_repl "$HISTORY_DIR")

sed \
  -e "s|__NODE_SEED__|$NODE_SEED_ESC|g" \
  -e "s|__DB_PATH__|$DB_PATH_ESC|g" \
  -e "s|__BUCKET_DIR__|$BUCKET_DIR_ESC|g" \
  -e "s|__HISTORY_DIR__|$HISTORY_DIR_ESC|g" \
  -e "s|^peer_port = .*# __PEER_PORT__.*\$|peer_port = $PEER_PORT|" \
  "$TEMPLATE" \
  > "$CONFIG_FILE"

# Fail fast if any placeholder slipped through (missing marker, typo, etc.).
if grep -qE '__[A-Z_]+__' "$CONFIG_FILE"; then
  echo "ERROR: rendered config still contains placeholders:"
  grep -nE '__[A-Z_]+__' "$CONFIG_FILE"
  exit 1
fi

echo "Config written to $CONFIG_FILE"
echo "History will be published to $HISTORY_DIR"
echo

# --- Initialize database ---
echo "Initializing database..."
"$BINARY" --config "$CONFIG_FILE" --testnet new-db 2>&1
echo "Database initialized."
echo

# --- Initialize local history archive ---
echo "Initializing local history archive..."
"$BINARY" --config "$CONFIG_FILE" --testnet new-hist local 2>&1
echo "Local history archive initialized."
echo

# --- Start validator ---
echo "Starting validator..."
"$BINARY" --config "$CONFIG_FILE" --testnet run --validator > "$LOG_FILE" 2>&1 &
NODE_PID=$!
echo "Validator started (pid $NODE_PID), logging to $LOG_FILE"
echo

# --- Poll for published checkpoint ---
HAS_FILE="$HISTORY_DIR/.well-known/stellar-history.json"
echo "Waiting for first published checkpoint (timeout: ${TIMEOUT}s, stall threshold: ${STALL_THRESHOLD_SECS}s)..."

# DIAG_GREP_PATTERN is defined up in the config region so both the poll loop
# below and the --emit-new-lines-only test seam share one definition.
LAST_LOG_LINE=0

START_TIME=$(date +%s)
while true; do
  ELAPSED=$(( $(date +%s) - START_TIME ))
  if [[ $ELAPSED -ge $TIMEOUT ]]; then
    echo "ERROR: Timed out after ${TIMEOUT}s waiting for checkpoint"
    echo "Last 50 lines of validator log:"
    tail -50 "$LOG_FILE"
    echo
    # Classify the deadline outcome (#3280): a synced-but-no-publish is a real
    # PUBLISH-REGRESSION (hard red); a never-synced is an environmental
    # SYNC-TIMEOUT (soft-skippable under --soft-on-sync-timeout). At the deadline
    # no HAS exists, so classify_deadline returns either PUBLISH-REGRESSION
    # (exit 1) or SYNC-TIMEOUT (exit 0 if the flag is set, else exit 1).
    DEADLINE_EXIT=0
    classify_deadline "$DATA_DIR" || DEADLINE_EXIT=$?
    exit $DEADLINE_EXIT
  fi

  # Check if the process is still alive
  if ! kill -0 "$NODE_PID" 2>/dev/null; then
    echo "ERROR: Validator process died"
    echo "Last 50 lines of validator log:"
    tail -50 "$LOG_FILE"
    exit 1
  fi

  # In-loop stall detector (#3741): the node process is alive (checked above)
  # but validator.log has produced zero new bytes for >= $STALL_THRESHOLD_SECS.
  # This is a genuine, previously-invisible stall (the class of failure that
  # used to end in an artifact-less external GitHub Actions cancellation) —
  # always a hard red, never covered by --soft-on-sync-timeout (a stalled-but-
  # alive process is meaningfully different from "testnet never became
  # reachable"). Exiting here (rather than letting the job run to an external
  # cancellation) makes the workflow's own `if: failure()` Upload-logs step
  # fire, so the full validator.log artifact is captured too.
  if is_log_stalled "$LOG_FILE" "$(date +%s)" "$STALL_THRESHOLD_SECS"; then
    echo "Last 200 lines of validator log:"
    tail -200 "$LOG_FILE"
    echo
    echo "DISPOSITION: STALL — validator.log produced no new output for >= ${STALL_THRESHOLD_SECS}s while the validator process was still alive (hard red, #3741)"
    exit 1
  fi

  # Incremental live-tail (#3741): echo any new validator.log lines matching
  # the diagnostic filter into this step's own stdout, which GitHub Actions
  # captures even under an external job-level cancellation (unlike the
  # artifact upload). No background process/PID cleanup needed — this stays
  # inside the existing single-threaded 5s poll loop.
  emit_new_diagnostic_lines "$LOG_FILE" "$LAST_LOG_LINE" "$DIAG_GREP_PATTERN"
  LAST_LOG_LINE="$LAST_LOG_LINE_OUT"

  # Check for published HAS
  if [[ -f "$HAS_FILE" ]]; then
    # Read the currentLedger from the HAS
    CURRENT_LEDGER=$(jq -r '.currentLedger' "$HAS_FILE" 2>/dev/null || echo "0")
    if [[ "$CURRENT_LEDGER" -gt 0 ]]; then
      echo "Published checkpoint found! currentLedger=$CURRENT_LEDGER (after ${ELAPSED}s)"
      break
    fi
  fi

  # Print progress every 30 seconds
  if [[ $(( ELAPSED % 30 )) -eq 0 ]] && [[ $ELAPSED -gt 0 ]]; then
    echo "  Still waiting... (${ELAPSED}s elapsed)"
  fi

  sleep 5
done

echo

# --- Stop validator ---
echo "Stopping validator..."
kill "$NODE_PID" 2>/dev/null || true
wait "$NODE_PID" 2>/dev/null || true
NODE_PID=""
echo "Validator stopped."
echo

# --- Determine checkpoint to compare ---
if [[ -z "$CHECKPOINT" ]]; then
  CHECKPOINT="$CURRENT_LEDGER"
fi
echo "Comparing checkpoint $CHECKPOINT"
echo

# --- Run comparison ---
echo "Running compare-checkpoint..."
echo "  Local:     file://$HISTORY_DIR"
echo "  Reference: $SDF_ARCHIVE"
echo

"$BINARY" --testnet compare-checkpoint \
  --local-archive "file://$HISTORY_DIR" \
  --remote-archive "$SDF_ARCHIVE" \
  --checkpoint "$CHECKPOINT"

EXIT_CODE=$?

if [[ $EXIT_CODE -eq 0 ]]; then
  echo
  echo "SUCCESS: Checkpoint $CHECKPOINT matches SDF testnet archive"
else
  echo
  echo "FAILURE: Checkpoint $CHECKPOINT has mismatches (exit code $EXIT_CODE)"
fi

exit $EXIT_CODE
