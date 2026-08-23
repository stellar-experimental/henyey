#!/usr/bin/env bash
# replay-alarms-on-history.sh — Alarm regression check with historical replay.
#
# Modes:
#   Default:  Single-evaluation of the most recent current.prom/prev.prom pair.
#   --replay: Evaluate all archived snapshot triplets chronologically, producing
#             per-alarm fired-count summaries across the retention window.
#
# Usage:
#   scripts/dev/replay-alarms-on-history.sh [SESSION_DIR]
#   scripts/dev/replay-alarms-on-history.sh [SESSION_DIR] --replay [--window N] [--alarm NAME]
#   scripts/dev/replay-alarms-on-history.sh [SESSION_DIR] --replay --json
#
# SESSION_DIR defaults to the most recent session under ~/data/.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVAL_SCRIPT="$REPO_ROOT/scripts/lib/eval-alarms.py"
CATALOG="$REPO_ROOT/.claude/skills/shared/metric-alarms.toml"

if [[ ! -f "$EVAL_SCRIPT" ]]; then
  echo "ERROR: eval-alarms.py not found at $EVAL_SCRIPT" >&2
  exit 1
fi
if [[ ! -f "$CATALOG" ]]; then
  echo "ERROR: metric-alarms.toml not found at $CATALOG" >&2
  exit 1
fi

# Parse arguments
SESSION_DIR=""
REPLAY_MODE=false
JSON_MODE=false
WINDOW=""
ALARM_FILTER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --replay)
      REPLAY_MODE=true
      shift
      ;;
    --json)
      JSON_MODE=true
      shift
      ;;
    --window)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --window requires a numeric argument" >&2
        exit 1
      fi
      WINDOW="$2"
      if ! [[ "$WINDOW" =~ ^[0-9]+$ ]] || [[ "$WINDOW" -lt 1 ]]; then
        echo "ERROR: --window must be a positive integer, got '$WINDOW'" >&2
        exit 1
      fi
      shift 2
      ;;
    --alarm)
      if [[ $# -lt 2 ]]; then
        echo "ERROR: --alarm requires a name argument" >&2
        exit 1
      fi
      ALARM_FILTER="$2"
      shift 2
      ;;
    *)
      if [[ -z "$SESSION_DIR" ]]; then
        SESSION_DIR="$1"
      else
        echo "ERROR: Unexpected argument: $1" >&2
        exit 1
      fi
      shift
      ;;
  esac
done

# Validate flag combinations
if [[ "$JSON_MODE" == true ]] && [[ "$REPLAY_MODE" != true ]]; then
  echo "ERROR: --json requires --replay" >&2
  exit 1
fi
if [[ "$JSON_MODE" == true ]] && [[ -n "$WINDOW" ]]; then
  echo "ERROR: --window cannot be used with --json (JSON always covers full archive)" >&2
  exit 1
fi
if [[ "$JSON_MODE" == true ]] && [[ -n "$ALARM_FILTER" ]]; then
  echo "ERROR: --alarm cannot be used with --json (JSON always covers all alarms)" >&2
  exit 1
fi

# Find session directory if not specified
if [[ -z "$SESSION_DIR" ]]; then
  for d in "$HOME"/data/*/metrics; do
    if [[ -d "$d" ]]; then
      parent="$(dirname "$d")"
      if [[ -z "$SESSION_DIR" ]] || [[ "$parent" -nt "$SESSION_DIR" ]]; then
        SESSION_DIR="$parent"
      fi
    fi
  done
  if [[ -z "$SESSION_DIR" ]]; then
    echo "ERROR: No session directory with metrics/ found under ~/data/" >&2
    exit 1
  fi
fi

METRICS_DIR="$SESSION_DIR/metrics"
ARCHIVE_DIR="$METRICS_DIR/archive"

# Validate a metadata.env file has all required v1 keys and valid types.
# Usage: validate_metadata <dir-path>
# Returns 0 on success, 1 on failure (with error message on stderr).
# Sources the metadata.env into the current shell on success.
validate_metadata() {
  local dir="$1"
  local meta="$dir/metadata.env"

  if [[ ! -f "$meta" ]]; then
    echo "ERROR: Corrupt archive at $dir: metadata.env not found" >&2
    return 1
  fi

  # Clear all schema variables before sourcing to prevent stale state
  # from a prior snapshot leaking into this one's validation.
  unset ARCHIVE_VERSION TICK_SKIPPED PREV_PROM_INVALID WARMUP_TICKS_REMAINING \
        FRESH_START CRASH_RECOVERY UPTIME_SECONDS MONITOR_MODE PID START_TICKS \
        2>/dev/null || true

  if ! source "$meta" 2>/dev/null; then
    echo "ERROR: Corrupt archive at $dir: metadata.env is not parseable" >&2
    return 1
  fi

  # Accept the known-good set of schema versions. v1 was the original inert
  # constant; v2 (#3791) is the current canonical single-writer schema. Readers
  # must branch on the version rather than fail-close on anything but 1, or
  # every archive the monitor writes after #3791 is rejected as corrupt.
  case "${ARCHIVE_VERSION:-}" in
    1|2) ;;
    *)
      echo "ERROR: Corrupt archive at $dir: ARCHIVE_VERSION=${ARCHIVE_VERSION:-missing} (expected 1 or 2)" >&2
      return 1
      ;;
  esac

  # Validate required keys are present (not unset)
  local required_keys="TICK_SKIPPED PREV_PROM_INVALID WARMUP_TICKS_REMAINING FRESH_START CRASH_RECOVERY UPTIME_SECONDS MONITOR_MODE"
  for key in $required_keys; do
    if [[ -z "${!key+x}" ]]; then
      echo "ERROR: Corrupt archive at $dir: missing required key $key" >&2
      return 1
    fi
  done

  # PID and START_TICKS must be present as keys but may be empty strings
  # (empty is valid for skipped ticks where process was not found)
  for key in PID START_TICKS; do
    if [[ -z "${!key+x}" ]]; then
      echo "ERROR: Corrupt archive at $dir: missing required key $key" >&2
      return 1
    fi
  done

  # Validate boolean fields
  for key in TICK_SKIPPED PREV_PROM_INVALID; do
    local val="${!key}"
    if [[ "$val" != "true" ]] && [[ "$val" != "false" ]]; then
      echo "ERROR: Corrupt archive at $dir: $key='$val' (expected true/false)" >&2
      return 1
    fi
  done

  # Validate numeric fields
  for key in WARMUP_TICKS_REMAINING UPTIME_SECONDS; do
    local val="${!key}"
    if ! [[ "$val" =~ ^[0-9]+$ ]]; then
      echo "ERROR: Corrupt archive at $dir: $key='$val' (expected non-negative integer)" >&2
      return 1
    fi
  done

  # Validate enum fields
  if [[ "$FRESH_START" != "yes" ]] && [[ "$FRESH_START" != "no" ]]; then
    echo "ERROR: Corrupt archive at $dir: FRESH_START='$FRESH_START' (expected yes/no)" >&2
    return 1
  fi
  if [[ "$CRASH_RECOVERY" != "yes" ]] && [[ "$CRASH_RECOVERY" != "no" ]]; then
    echo "ERROR: Corrupt archive at $dir: CRASH_RECOVERY='$CRASH_RECOVERY' (expected yes/no)" >&2
    return 1
  fi
  if [[ "$MONITOR_MODE" != "validator" ]] && [[ "$MONITOR_MODE" != "watcher" ]]; then
    echo "ERROR: Corrupt archive at $dir: MONITOR_MODE='$MONITOR_MODE' (expected validator/watcher)" >&2
    return 1
  fi

  return 0
}

# --- Replay mode ---
if [[ "$REPLAY_MODE" == true ]]; then
  # Discover complete snapshot directories (those containing metadata.env)
  SNAPSHOTS=()
  if [[ -d "$ARCHIVE_DIR" ]]; then
    # Enumerate by MTIME (ascending), not lexical name: the archive may hold
    # mixed timestamp-name formats (dash-format RFC-3339 and compact), and a
    # name sort places every dash dir ahead of every compact dir regardless of
    # real age, corrupting the chronological sequential-replay order (#3724).
    while IFS= read -r d; do
      [[ -n "$d" ]] || continue
      if [[ -f "$d/metadata.env" ]]; then
        SNAPSHOTS+=("$d")
      else
        echo "WARNING: Skipping incomplete archive entry: $d (no metadata.env)" >&2
      fi
    done < <(find "$ARCHIVE_DIR" -maxdepth 1 -mindepth 1 -type d \
      ! -name '*.tmp' -printf '%T@ %p\n' 2>/dev/null | sort -n | cut -d' ' -f2-)
  fi

  if [[ ${#SNAPSHOTS[@]} -eq 0 ]]; then
    if [[ "$JSON_MODE" == true ]]; then
      echo '{"schema_version":1,"evaluated_ticks":0,"skipped_ticks":0,"error_ticks":0,"total_snapshots":0,"first_ts":"","last_ts":"","alarms":{}}'
    else
      echo "No archived snapshots found in $ARCHIVE_DIR"
    fi
    exit 0
  fi

  # Create a fresh state directory for this replay invocation
  REPLAY_STATE_DIR=$(mktemp -d "${METRICS_DIR}/replay-state-XXXXXX")
  trap 'rm -rf "$REPLAY_STATE_DIR"' EXIT

  # Evaluate all snapshots chronologically, collecting JSON results
  TOTAL=${#SNAPSHOTS[@]}
  SKIPPED_COUNT=0
  EVALUATED_COUNT=0
  ERROR_COUNT=0
  RESULTS_FILE=$(mktemp "${METRICS_DIR}/replay-results-XXXXXX")
  FIRST_TS=""
  LAST_TS=""

  for snap_dir in "${SNAPSHOTS[@]}"; do
    ts_name="$(basename "$snap_dir")"

    # Track time range
    if [[ -z "$FIRST_TS" ]]; then
      FIRST_TS="$ts_name"
    fi
    LAST_TS="$ts_name"

    # Validate and source metadata
    if ! validate_metadata "$snap_dir"; then
      rm -f "$RESULTS_FILE"
      exit 1
    fi

    # Handle skipped ticks — preserve state dir, skip evaluation
    if [[ "${TICK_SKIPPED:-false}" == "true" ]]; then
      SKIPPED_COUNT=$((SKIPPED_COUNT + 1))
      continue
    fi

    # Build eval command with archived env vars
    CURRENT_FILE="$snap_dir/current.prom"
    PREV_FILE="$snap_dir/prev.prom"

    # Run evaluator with archived metadata
    RESULT=$(PREV_PROM_INVALID="${PREV_PROM_INVALID:-false}" \
      WARMUP_TICKS_REMAINING="${WARMUP_TICKS_REMAINING:-0}" \
      FRESH_START="${FRESH_START:-no}" \
      CRASH_RECOVERY="${CRASH_RECOVERY:-no}" \
      UPTIME_SECONDS="${UPTIME_SECONDS:-0}" \
      MONITOR_MODE="${MONITOR_MODE:-validator}" \
      PID="${PID:-}" \
      START_TICKS="${START_TICKS:-}" \
      python3 "$EVAL_SCRIPT" \
        --catalog "$CATALOG" \
        --current "$CURRENT_FILE" \
        --prev "$PREV_FILE" \
        --state-dir "$REPLAY_STATE_DIR" 2>/dev/null) || true

    if [[ -n "$RESULT" ]]; then
      echo "$RESULT" >> "$RESULTS_FILE"
      EVALUATED_COUNT=$((EVALUATED_COUNT + 1))
    else
      ERROR_COUNT=$((ERROR_COUNT + 1))
    fi
  done

  # Generate output (JSON or human-readable table)
  python3 -c "
import json, sys

results_file = sys.argv[1]
window = int(sys.argv[2]) if sys.argv[2] else 0
alarm_filter = sys.argv[3] if sys.argv[3] else ''
evaluated = int(sys.argv[4])
skipped = int(sys.argv[5])
first_ts = sys.argv[6]
last_ts = sys.argv[7]
session_dir = sys.argv[8]
catalog = sys.argv[9]
json_mode = sys.argv[10] == 'true'
error_count = int(sys.argv[11])
total_snapshots = int(sys.argv[12])

# Parse all results using streaming JSON decoder (evaluator outputs
# multi-line pretty-printed JSON with indent=2)
all_results = []
with open(results_file) as f:
    content = f.read()
decoder = json.JSONDecoder()
idx = 0
while idx < len(content):
    remaining = content[idx:].lstrip()
    if not remaining:
        break
    try:
        obj, end = decoder.raw_decode(remaining)
        all_results.append(obj)
        idx = len(content) - len(remaining) + end
    except json.JSONDecodeError:
        break

# Apply window filter (display only last N evaluated ticks, table mode only)
if not json_mode and window > 0 and window < len(all_results):
    display_results = all_results[-window:]
    displayed = window
else:
    display_results = all_results
    displayed = len(all_results)

# Aggregate per-alarm states
alarm_counts = {}  # name -> {firing, breach, ok, baseline, skip}
for result in display_results:
    for alarm in result.get('alarms', []):
        name = alarm.get('name', 'unknown')
        state = alarm.get('state', 'unknown')
        if name not in alarm_counts:
            alarm_counts[name] = {'firing': 0, 'breach': 0, 'ok': 0, 'baseline': 0, 'skip': 0}
        if state == 'firing':
            alarm_counts[name]['firing'] += 1
        elif state == 'breach':
            alarm_counts[name]['breach'] += 1
        elif state in ('ok', 'clear'):
            alarm_counts[name]['ok'] += 1
        elif state == 'collecting_baseline':
            alarm_counts[name]['baseline'] += 1
        elif state == 'skipped':
            alarm_counts[name]['skip'] += 1
        else:
            alarm_counts[name]['skip'] += 1

if json_mode:
    # JSON output mode
    output = {
        'schema_version': 1,
        'evaluated_ticks': evaluated,
        'skipped_ticks': skipped,
        'error_ticks': error_count,
        'total_snapshots': total_snapshots,
        'first_ts': first_ts,
        'last_ts': last_ts,
        'alarms': alarm_counts,
    }
    json.dump(output, sys.stdout)
    print()
    sys.exit(0)

# Apply alarm filter (table mode only)
if alarm_filter:
    matching = {k: v for k, v in alarm_counts.items() if alarm_filter in k}
    if not matching:
        print(f\"No alarm matching '{alarm_filter}' found\", file=sys.stderr)
        sys.exit(1)
    alarm_counts = matching

# Print summary table
print('=== Alarm History Replay ===')
print(f'Session:   {session_dir}')
print(f'Evaluated: {evaluated} ticks ({skipped} skipped, {error_count} errors)')
if window > 0 and window < evaluated:
    print(f'Displayed: {displayed} of {evaluated} (--window {window})')
print(f'Range:     {first_ts} -> {last_ts}')
print(f'Catalog:   {catalog}')
print()

if not alarm_counts:
    print('No alarm results to display.')
    sys.exit(0)

# Table header
name_width = max(30, max(len(n) for n in alarm_counts) + 2)
header = f\"{'Alarm Name':<{name_width}} | {'Firing':>6} | {'Breach':>6} | {'OK':>6} | {'Baseline':>8} | {'Skip':>4}\"
separator = '-' * name_width + '-|' + '--------|--------|--------|----------|------'
print(header)
print(separator)

# Table rows (sorted by name)
firing_count = 0
breach_count = 0
for name in sorted(alarm_counts.keys()):
    c = alarm_counts[name]
    print(f\"{name:<{name_width}} | {c['firing']:>6} | {c['breach']:>6} | {c['ok']:>6} | {c['baseline']:>8} | {c['skip']:>4}\")
    if c['firing'] > 0:
        firing_count += 1
    if c['breach'] > 0:
        breach_count += 1

print()
print(f'Alarms with any firings: {firing_count}')
print(f'Alarms with any breaches: {breach_count}')
" "$RESULTS_FILE" "${WINDOW:-0}" "$ALARM_FILTER" "$EVALUATED_COUNT" "$SKIPPED_COUNT" "$FIRST_TS" "$LAST_TS" "$SESSION_DIR" "$CATALOG" "$JSON_MODE" "$ERROR_COUNT" "$TOTAL"

  rm -f "$RESULTS_FILE"
  exit 0
fi

# --- Default mode: single-pair evaluation ---

CURRENT="$METRICS_DIR/current.prom"
PREV="$METRICS_DIR/prev.prom"

echo "=== Alarm Regression Check ==="
echo "Session:  $SESSION_DIR"
echo "Catalog:  $CATALOG"
echo ""

# Step 1: Schema validation
echo "--- Step 1: Schema validation (--validate-only) ---"
if python3 "$EVAL_SCRIPT" --catalog "$CATALOG" --validate-only 2>&1; then
  echo "✓ Schema validation passed"
else
  echo "✗ Schema validation FAILED"
  exit 1
fi
echo ""

# Step 2: Single-evaluation pass (if snapshot pair exists)
if [[ -f "$CURRENT" ]] && [[ -f "$PREV" ]]; then
  echo "--- Step 2: Single-evaluation pass ---"
  echo "current.prom: $CURRENT ($(wc -l < "$CURRENT") lines)"
  echo "prev.prom:    $PREV ($(wc -l < "$PREV") lines)"
  echo ""

  STATE_DIR=$(mktemp -d "${METRICS_DIR}/eval-state-XXXXXX")
  trap 'rm -rf "$STATE_DIR"' EXIT

  # Use metadata from latest archive snapshot if available
  EVAL_ENV_ARGS="MONITOR_MODE=validator UPTIME_SECONDS=900 WARMUP_TICKS_REMAINING=0"
  if [[ -d "$ARCHIVE_DIR" ]]; then
    LATEST_SNAP=""
    # Newest-by-MTIME wins (last line of an ascending mtime sort), not the
    # lexical-max name — mixed dash/compact name formats make a name sort
    # misidentify the "latest" snapshot (#3724).
    while IFS= read -r d; do
      [[ -n "$d" ]] || continue
      if [[ -f "$d/metadata.env" ]]; then
        LATEST_SNAP="$d"
      fi
    done < <(find "$ARCHIVE_DIR" -maxdepth 1 -mindepth 1 -type d \
      ! -name '*.tmp' -printf '%T@ %p\n' 2>/dev/null | sort -n | cut -d' ' -f2-)
    if [[ -n "$LATEST_SNAP" ]] && [[ -f "$LATEST_SNAP/metadata.env" ]]; then
      if validate_metadata "$LATEST_SNAP"; then
        echo "Using metadata from latest archive: $(basename "$LATEST_SNAP")"
        EVAL_ENV_ARGS="PREV_PROM_INVALID=${PREV_PROM_INVALID:-false} WARMUP_TICKS_REMAINING=${WARMUP_TICKS_REMAINING:-0} FRESH_START=${FRESH_START:-no} CRASH_RECOVERY=${CRASH_RECOVERY:-no} UPTIME_SECONDS=${UPTIME_SECONDS:-0} MONITOR_MODE=${MONITOR_MODE:-validator} PID=${PID:-} START_TICKS=${START_TICKS:-}"
        echo ""
      else
        echo "WARNING: Latest archive metadata is corrupt, using defaults" >&2
      fi
    fi
  fi

  RESULT=$(env $EVAL_ENV_ARGS \
    python3 "$EVAL_SCRIPT" \
    --catalog "$CATALOG" \
    --current "$CURRENT" \
    --prev "$PREV" \
    --state-dir "$STATE_DIR" 2>/dev/null) || true

  if [[ -z "$RESULT" ]]; then
    echo "✗ Evaluation produced no output"
    exit 1
  fi

  # Parse and display results
  python3 -c "
import json, sys
try:
    data = json.loads(sys.argv[1])
    alarms = data.get('alarms', [])
    firing = [a for a in alarms if a.get('state') == 'firing']
    breach = [a for a in alarms if a.get('state') == 'breach']
    skipped = [a for a in alarms if a.get('state') == 'skipped']
    ok = [a for a in alarms if a.get('state') in ('ok', 'clear')]
    baseline = [a for a in alarms if a.get('state') == 'collecting_baseline']
    other = [a for a in alarms if a.get('state') not in ('firing', 'breach', 'skipped', 'ok', 'clear', 'collecting_baseline')]

    print(f'Total alarms evaluated: {len(alarms)}')
    print(f'  Firing:   {len(firing)}')
    print(f'  Breach:   {len(breach)}')
    print(f'  OK:       {len(ok)}')
    print(f'  Baseline: {len(baseline)}')
    print(f'  Skipped:  {len(skipped)}')
    if other:
        print(f'  Other:    {len(other)}')
    print()

    if firing:
        print('FIRING alarms:')
        for a in firing:
            print(f'  - {a[\"name\"]}: {a.get(\"details\", \"\")}')
        print()

    if breach:
        print('BREACH alarms (building persistence):')
        for a in breach:
            print(f'  - {a[\"name\"]}: {a.get(\"details\", \"\")}')
        print()

    if baseline:
        print('COLLECTING BASELINE:')
        for a in baseline:
            print(f'  - {a[\"name\"]}')
        print()

    if skipped:
        print('SKIPPED alarms (exempt or gated):')
        for a in skipped:
            print(f'  - {a[\"name\"]}')
        print()

    print('✓ Evaluation complete')
except Exception as e:
    print(f'Parse error: {e}', file=sys.stderr)
    print(sys.argv[1][:500])
    sys.exit(1)
" "$RESULT"
else
  echo "--- Step 2: SKIPPED (no snapshot pair found) ---"
  echo "  Expected: $CURRENT"
  echo "  Expected: $PREV"
  echo ""
  echo "NOTE: Use --replay to evaluate archived snapshot history."
fi
