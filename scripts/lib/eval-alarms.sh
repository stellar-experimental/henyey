#!/usr/bin/env bash
# Wrapper script for eval-alarms.py.
# Resolves session paths and invokes the Python evaluator.
#
# Usage: eval-alarms.sh
#
# Required env vars:
#   MONITOR_SESSION_ID  — session directory name under ~/data/
#   MONITOR_ADMIN_PORT  — (not used here, but set by caller)
#
# Optional env vars (passed through to eval-alarms.py):
#   PREV_PROM_INVALID, WARMUP_TICKS_REMAINING, FRESH_START,
#   CRASH_RECOVERY, UPTIME_SECONDS, MONITOR_MODE, PID, START_TICKS
set -euo pipefail

# Cross-shell self-location (defensive). This wrapper is normally RUN via its
# bash shebang, but a `zsh scripts/lib/eval-alarms.sh` invocation must not abort
# at this line with `BASH_SOURCE[0]: parameter not set` (zsh has no BASH_SOURCE,
# and `set -u` is active above). Under bash ${BASH_SOURCE[0]} is this file; under
# zsh the :- fallback yields ${(%):-%x} (the running script's path). Bash never
# evaluates the literal zsh default. Resolves to the script dir under both
# shells from any cwd (#3137).
_eval_alarms_src="${BASH_SOURCE[0]:-${(%):-%x}}"
SCRIPT_DIR="$(cd "$(dirname "$_eval_alarms_src")" && pwd)"
unset _eval_alarms_src
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

SESSION_DIR="${HOME}/data/${MONITOR_SESSION_ID:?MONITOR_SESSION_ID required}"

exec python3 "$SCRIPT_DIR/eval-alarms.py" \
    --catalog "$REPO_ROOT/.claude/skills/shared/metric-alarms.toml" \
    --current "$SESSION_DIR/metrics/current.prom" \
    --prev "$SESSION_DIR/metrics/prev.prom" \
    --state-dir "$SESSION_DIR/metrics"
