---
name: monitor-tick
description: One tick of the henyey mainnet monitor — checks, metrics scan, deploy, status report
argument-hint: ""
---

# Monitor Tick

One invocation of the monitor loop. Intended to be called on a ~20-minute
cadence by an external orchestrator (crontab, systemd timer, CI) via
`Codex -p '/monitor-tick'`, or by a Codex-internal `/loop 20m /monitor-tick`.
The 60m deploy cool-down (check 10) is sized so each deployed binary gets
~3 ticks of observation before the next deploy is eligible.

## Preconditions

This skill expects `/home/tomer/data/monitor-loop.env` to exist — it is
written by `/monitor-loop` at startup and contains the runtime config.
If the file is missing, **bail out immediately** with:

```
ERROR: /home/tomer/data/monitor-loop.env not found — run /monitor-loop first.
```

### Skill version tracks `origin/main`, binary tracks `build_sha`

This skill (`SKILL.md`, `scripts/lib/eval-alarms.{sh,py}`,
`shared/metric-alarms.toml`) is documentation and tooling that evolves
independently of the validator binary. The deployed binary is pinned to
a specific commit recorded in `$HOME/data/$MONITOR_SESSION_ID/build_sha`,
but the skill always tracks `origin/main`.

Why: an operator may land a SKILL.md update (new check, new alarm, new
archive step) without rebuilding the validator. A tick that follows the
skill in a working tree pinned to the deployed sha will silently miss
those updates.

**Deploy-procedure convention:** when a tick builds and deploys a new
validator sha, it must return the working tree to `main` after the build
so subsequent ticks read the latest procedure:

```bash
git checkout <new-sha>
CARGO_TARGET_DIR=... cargo build --release -p henyey
# … Stop-PID, Rotate-log, Relaunch, atomic build_sha update …
git checkout main && git pull --ff-only   # ← restore working tree
```

If the working tree is detached at a deployed sha when a tick runs, the
tick must `git checkout main && git pull --ff-only` before reading any
file under `.Codex/skills/`, `scripts/lib/`, or `shared/`. The
deployed-sha tree may have stale paths that produce silent procedure
drift (missing alarms, missing archive step, etc.).

Audit at a glance:

```bash
git rev-parse HEAD          # working tree (should equal origin/main)
git rev-parse origin/main   # current upstream
```

Load the env file at the start of the tick:

```bash
set -a
source /home/tomer/data/monitor-loop.env
set +a
```

Variables provided by the env file (all required):

- `MONITOR_SESSION_ID` — 8-char session id, e.g. `74535976`
- `MONITOR_MODE` — `validator` or `watcher`
- `MONITOR_CONFIG` — path to the TOML config, e.g. `configs/validator-mainnet-rpc.toml`
- `MONITOR_ADMIN_PORT` — `11627` (validator) or `11727` (watcher)
- `MONITOR_RPC_PORT` — `8000` (validator) or empty (watcher)
- `MONITOR_RUN_FLAGS` — `--validator` (validator) or empty (watcher)

Optional (not required in the env file; read with a default at the point of use):

- `MONITOR_HOST_RAM_GB` — physical RAM of the host, in GB, used by the
  host-RAM-relative memory guardrail (check 4). **Default `61`** when unset, so
  existing 61 GB deployments are unaffected until an operator sets it (e.g. `32`
  on a 32 GB/no-swap box). See `eval_memory_guardrail` in
  `scripts/lib/monitor-decisions.sh`.

Throughout this skill, substitute those values for the `$MONITOR_*` references below.

### Skill checkout drift detection

After loading the env file, detect whether the working tree has drifted
from `origin/main`. This is *detection only* — the tick proceeds normally
regardless. The watch item surfaces in the daily summary so an operator
can investigate.

```bash
# Skill checkout drift detection: surface when working tree diverges from origin/main.
# Known false negatives: git fetch failure (offline/network) silently skips the check;
# checkouts predating this guard don't have it at all.
if git fetch origin main --quiet 2>/dev/null; then
  SKILL_SHA=$(git rev-parse HEAD)
  MAIN_SHA=$(git rev-parse origin/main)
  if [[ "$SKILL_SHA" != "$MAIN_SHA" ]]; then
    WATCH_ITEMS+=("skill_stale=HEAD@${SKILL_SHA:0:8} vs origin/main@${MAIN_SHA:0:8}")
  fi
fi
```

### Per-session state files

All files below live in `/home/tomer/data/$MONITOR_SESSION_ID/`:

| File | Purpose | Writer |
|------|---------|--------|
| `build_sha` | Cache of deployed binary's source commit (authoritative source: runtime `/info.commit_hash`; seeded by build step) | check 10 (after successful build or runtime self-heal) and `monitor-loop` step 5 |
| `last_ledger` | STUCK detection state (check 4) | check 4 |
| `last_ledger_count` | STUCK confirmation counter (check 4) | check 4 |
| `wipe_attempted_at` | Epoch of most recent (3a) wipe; read by (3d) post-wipe recurrence guard | check 3a (write), check 3d marker-clear rule (remove) |
| `tick-history.jsonl` | daily-summary aggregation | tick epilogue |
| `metrics/current.prom` | latest Prometheus scrape | check 12 |
| `metrics/prev.prom` | previous Prometheus scrape | check 12 |
| `metrics/scrape_identity` | process identity of the scrape now in prev.prom | check 12 |
| `metrics/ratio_snapshot` | counter-ratio history (check 12) | check 12 |
| `metrics/counter_streak_snapshot` | counter-streak state (check 12b) | check 12b ([metric-alarms](../shared/metric-alarms.toml)) |
| `metrics/anomaly_cooldown.json` | alert dedup state | check 9 |
| `metrics/catchup_stalled_state` | (3g) catchup-stalled frozen-lcl dwell state (`frozen_lcl`, `frozen_since_epoch`); sole reader/writer `classify_catchup_stalled` (#3799) | check 3g |
| `metrics/archive/` | Per-tick snapshot dirs (current.prom + prev.prom + metadata.env), rolling 500, atomic write | check 12 |
| `logs/monitor.log` | node stdout/stderr (rotated on restart) | node process |
| `cargo-target/` | cached build tree | cargo |
| `.alive` | session liveness marker | session startup |

**Cross-session state** (lives at `$HOME/data/`, outside any session dir):

| File | Purpose | Writer | Remover |
|------|---------|--------|---------|
| `deploy_quarantine.txt` | Commit SHAs that triggered a regression; auto-cleared by the deploy gate when their diff is no longer present in `origin/main`. Survives session wipes. | Deploy regression policy (skill, during rollback) | Auto-cleared on revert/refactor; operator can manually `quarantine_remove` |
| `$REPO_ROOT/.monitor-tick-filed.json` | Persistent dedup for post-wipe recurrence issue filing; 30-day TTL with auto-prune | check 3d post-wipe recurrence (write) | Auto-pruned (30-day TTL) |
| `$REPO_ROOT/.monitor-tick-filing.lock` | flock for serializing dedup file access during issue filing | check 3d | N/A (empty sentinel) |

## Session-dir-vanished detection

Immediately after loading `monitor-loop.env`, check whether the session
directory still exists. If not, this is a distinct (and severe) failure class
— the entire session state (binary, logs, metrics, tick history) was deleted
out-of-band.

```bash
source "$(git rev-parse --show-toplevel)/scripts/lib/monitor-decisions.sh"
check_session_wiped "$HOME/data" "/proc" "$MONITOR_SESSION_ID" \
  "$HOME/data/monitor-loop.env" || exit 1
```

The `check_session_wiped` function (defined in `scripts/lib/monitor-decisions.sh`):
- Sets `SESSION_WIPED` ("yes"/"no") and `SESSION_WIPED_PROCESS_ALIVE` ("yes"/"no")
- Returns 0 if not wiped or wiped-and-recoverable (recovery dirs created)
- Returns 1 if wiped, no process alive, env stale (>2h) — dirs NOT created, caller should exit
- Uses `_find_session_process` to scan `/proc/[0-9]*/exe` for a process matching the expected binary path (including `(deleted)` suffix)
- On return 0 with SESSION_WIPED=yes: recreates `{logs,cache,cargo-target,metrics}` subdirs

### Long-stale session detection

When the session dir exists (`SESSION_WIPED=no`), check whether the session
is long-abandoned — no process alive AND no recent tick/orchestrator activity.
This prevents auto-relaunching a node that was intentionally stopped hours or
days ago.

```bash
if [[ "$SESSION_WIPED" == "no" ]]; then
  check_long_stale_session "$HOME/data" "/proc" "$MONITOR_SESSION_ID" \
    "$HOME/data/monitor-loop.env" || exit 1
fi
```

The `check_long_stale_session` function (defined in `scripts/lib/monitor-decisions.sh`):
- Sets `LONG_STALE_SESSION` ("yes"/"no")
- Returns 0 if session is not long-stale (process alive, or `.alive`/env recent enough)
- Returns 1 if long-stale: no process, `.alive` age > 6h (or missing), env age > 24h
- Primary signal: `.alive` mtime (touched every tick at start). Fallback: env file mtime.
- Process-alive check (via `_find_session_process`) overrides staleness markers.
- On return 1: stderr error, caller should exit without relaunching.

### Recovery path when `SESSION_WIPED=yes`

**Case A: `SESSION_WIPED_PROCESS_ALIVE=yes`**
- Touch `.alive`.
- Skip checks dependent on log/metrics files (1, 5, 6, 7, 12).
- Attempt admin-port checks: (2) ledger progression, (8) RPC health, (9) OBSRVR
  — the live process may still respond to HTTP.
- Report status, `actions`, `watch`, and `wipe:` line per wipe-state
  composition table (row #3 or #6 depending on `MAINNET_WIPED`).
- File/comment issue per wipe-state issue-filing policy.

**Case B: `SESSION_WIPED_PROCESS_ALIVE=no`**
- Touch `.alive`.
- `FRESH_START` determined normally (mainnet.db likely absent → yes).
- Skip checks (1)–(9), (12) — no process, no logs, no metrics.
- **Rebuild the binary**:
  ```bash
  CARGO_TARGET_DIR=/home/tomer/data/$MONITOR_SESSION_ID/cargo-target \
    cargo build --release -p henyey
  ```
  If build succeeds, persist the deployed sha (atomic write):
  ```bash
  new_sha=$(git rev-parse HEAD)
  printf '%s\n' "$new_sha" > "/home/tomer/data/$MONITOR_SESSION_ID/build_sha.tmp"
  mv "/home/tomer/data/$MONITOR_SESSION_ID/build_sha.tmp" "/home/tomer/data/$MONITOR_SESSION_ID/build_sha"
  ```
  If build fails: report `OFFLINE`; emit `actions`/`watch`/`wipe:` per
  wipe-state composition table (row #5 or #8). **File/comment issue before
  exit** using wipe-state issue-filing policy. Then exit.
- **Relaunch** via standard Relaunch procedure.
- Report status, `actions`, `watch`, and `wipe:` line per wipe-state
  composition table (row #4 or #7 depending on `MAINNET_WIPED`).
- File/comment issue per wipe-state issue-filing policy.

## Mainnet-data-vanished detection

After the session-dir check, independently verify that mainnet data exists
(regardless of session-dir state). **Note:** The stale-env early-exit
terminates the tick before reaching this point — no wipe signal
is emitted in that case because no tick report is generated.

```bash
check_mainnet_wiped "$HOME/data"
```

The `check_mainnet_wiped` function (from `scripts/lib/monitor-decisions.sh`):
- Sets `MAINNET_WIPED` to "yes" if `$HOME/data/mainnet` directory is missing, "no" otherwise
- If mainnet/ dir exists but mainnet.db is missing, defer to FRESH_START logic
- MAINNET_WIPED only fires when the ENTIRE directory is gone

### Wipe-state composition

Both `SESSION_WIPED` and `MAINNET_WIPED` are fully determined at this point.
All downstream reporting derives wipe-related outputs from these two flags
using the truth table below. **This is the single source of truth** — Case A/B
and the status formatter reference this table; they do not independently define
wipe-related outputs.

#### Complete state truth table

| # | SESSION_WIPED | MAINNET_WIPED | Case | `actions` | `watch` | Status level | `wipe:` line | Issue title pattern |
|---|:---:|:---:|---|---|---|---|---|---|
| 1 | no | no | — | — | — | (normal) | (omitted) | — |
| 2 | no | yes | — | `"mainnet-data-wiped"` | `"wipe=mainnet-data"` | ACTION | `MAINNET DATA WIPED — catchup recovery` | `OFFLINE: mainnet data wiped out-of-band` |
| 3 | yes | no | A | `"session-wiped-process-alive"` | `"wipe=session-dir"` | ACTION | `SESSION DIR WIPED — process alive (PID N), operator intervention needed` | `OFFLINE: validator session wiped out-of-band` |
| 4 | yes | no | B-ok | `"session-wiped-recovery"` | `"wipe=session-dir"` | ACTION | `SESSION DIR WIPED — rebuilt + relaunched (new PID N)` | `OFFLINE: validator session wiped out-of-band` |
| 5 | yes | no | B-fail | `"session-wiped-rebuild-failed"` | `"wipe=session-dir"` | OFFLINE | `SESSION DIR WIPED — rebuild failed` | `OFFLINE: validator session wiped out-of-band` |
| 6 | yes | yes | A | `"session-wiped-process-alive"`, `"mainnet-data-wiped"` | `"wipe=session-dir"`, `"wipe=mainnet-data"` | ACTION | `SESSION DIR WIPED — process alive (PID N) + MAINNET DATA WIPED` | `OFFLINE: session + mainnet data wiped out-of-band` |
| 7 | yes | yes | B-ok | `"session-wiped-recovery"`, `"mainnet-data-wiped"` | `"wipe=session-dir"`, `"wipe=mainnet-data"` | ACTION | `SESSION DIR WIPED — rebuilt + relaunched (new PID N) + MAINNET DATA WIPED` | `OFFLINE: session + mainnet data wiped out-of-band` |
| 8 | yes | yes | B-fail | `"session-wiped-rebuild-failed"`, `"mainnet-data-wiped"` | `"wipe=session-dir"`, `"wipe=mainnet-data"` | OFFLINE | `SESSION DIR WIPED — rebuild failed + MAINNET DATA WIPED` | `OFFLINE: session + mainnet data wiped out-of-band` |

**Composition rule:** The `wipe:` line is built from up to two fragments
joined by ` + `:
1. Session fragment (from Case outcome): present when `SESSION_WIPED=yes`
2. `MAINNET DATA WIPED`: appended when `MAINNET_WIPED=yes`

When only mainnet is wiped (#2), append `— catchup recovery` to the mainnet
fragment. In combined cases (#6–8), omit this suffix because the recovery
mechanism is the session-wipe Case B relaunch (which handles FRESH_START
implicitly).

**Issue-filing policy:**
- File/comment **one** issue per tick, not per flag.
- **Dedup/search:** Search open issues by the canonical title pattern from the
  table above. If an open issue with that title exists, comment on it (adding
  new evidence). Otherwise create a new issue.
- **Combined wipe:** Use the combined title (`session + mainnet data wiped`).
  Do NOT also file a separate mainnet-only issue.
- **Label/severity:** Defer to the existing Filing Flow label policy. In
  general: OFFLINE status → `urgent`; ACTION with process alive → per policy
  (typically `urgent` since node state is uncertain).
- **Board-route:** after filing/commenting, add to project board per
  `scripts/lib/monitor-label-policy.md` routing rules (Backlog for
  actionable issues, Blocked for `not-ready`).

**Tick-history `watch` array:** Both `"wipe=session-dir"` and
`"wipe=mainnet-data"` may coexist in a single tick's watch array. The
daily-summary aggregator handles this — each entry is independent.

## Session-alive marker

After the session-dir-vanished detection (which ensures the dir exists),
touch a sentinel file at the **start of every tick**:

```bash
touch /home/tomer/data/$MONITOR_SESSION_ID/.alive
```

The `.alive` file's mtime = timestamp of last successful tick start. Cleanup
tooling uses this to determine session liveness (see monitor-loop cleanup
guards).

## Fresh-start state

Determine once at the top of the tick: if `/home/tomer/data/mainnet/mainnet.db`
does NOT exist, set `FRESH_START=yes` (sync deadline = 20m). Otherwise
`FRESH_START=no` (sync deadline = 15m). Use this when evaluating check (2).

## Crash-recovery state

If the node's previous run ended with a crash or wedge (SIGKILL), restart
begins from a stale lcl that can be hours behind mainnet tip; replay will
legitimately exceed the 15m clean-restart deadline.

Detect crash-recovery once at the top of the tick. The rule fires on
**either** of these signals (both gated by `uptime < 2h`, after which
recovery is considered complete regardless):

1. **Rotation signal**: the most recent log rotation in the session's
   `logs/` dir is a `.crashed-*`, `.stuck-*`, or `.frozen-*` (not a
   planned `.preredeploy-*`). Use `find` (not shell globs) to avoid
   zsh `NO_NOMATCH` failing the pipeline.

2. **Active-catchup signal** (fires even when the rotation was
   `.preredeploy-*`): the node is in `Catching Up` state AND uptime
   exceeds 5 minutes. Handles the case where a manual planned restart
   (which rotates as `preredeploy-*`) happens AFTER a wedge, leaving
   the persisted lcl hours stale.

```bash
CRASH_RECOVERY=no
PID=$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")
if [ -n "$PID" ]; then
  uptime_sec=$(ps -o etimes= -p "$PID" 2>/dev/null | tr -d ' ')
  uptime_sec=${uptime_sec:-0}
  if [ "$uptime_sec" -lt 7200 ]; then
    # Signal 1: newest rotation type
    logs_dir="/home/tomer/data/$MONITOR_SESSION_ID/logs"
    newest_rotation=$(find "$logs_dir" -maxdepth 1 -type f \
        \( -name 'monitor.log.crashed-*' \
        -o -name 'monitor.log.stuck-*' \
        -o -name 'monitor.log.frozen-*' \
        -o -name 'monitor.log.preredeploy-*' \) \
      -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
    case "$newest_rotation" in
      *.crashed-*|*.stuck-*|*.frozen-*) CRASH_RECOVERY=yes ;;
    esac

    # Signal 2: active catchup past the clean-restart window
    if [ "$CRASH_RECOVERY" = "no" ] && [ "$uptime_sec" -gt 300 ]; then
      node_state=$(curl -s -m 3 "http://localhost:$MONITOR_ADMIN_PORT/info" 2>/dev/null \
                   | python3 -c 'import sys,json; print(json.load(sys.stdin).get("state",""))' 2>/dev/null)
      if [ "$node_state" = "Catching Up" ]; then
        CRASH_RECOVERY=yes
      fi
    fi
  fi
fi
```

When `CRASH_RECOVERY=yes` and `FRESH_START=no`, the active sync deadline
extends to 60m and a progress carveout applies (see check (2)).

## Health checks

### Common procedures

Several checks share these procedures — define once, reference by name:

**Stop-PID** (graceful kill, fall through to SIGKILL):
```bash
kill "$PID"; for i in $(seq 1 10); do sleep 1; kill -0 "$PID" 2>/dev/null || break; done
kill -0 "$PID" 2>/dev/null && kill -9 "$PID" && sleep 2
```

**Relaunch** (preserves log via append redirection, clears any stale lockfile):
```bash
rm -f /home/tomer/data/mainnet/mainnet.lock
RUST_LOG=info nohup /home/tomer/data/$MONITOR_SESSION_ID/cargo-target/release/henyey \
  --mainnet run $MONITOR_RUN_FLAGS -c $MONITOR_CONFIG \
  >> /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log 2>&1 &
```

**Rotate-log** (preserve prior session's log under the given suffix):
```bash
mv /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log \
   /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log.<suffix>-$(date -u +%Y%m%dT%H%M%SZ) 2>/dev/null || true
```
Suffix per origin: `crashed` (process found dead), `frozen` (wedge per 3b),
`preredeploy` (planned restart for deploy).

**(1) Log scan** — `tail -n 500 /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log`.
Scan for hash mismatches ("hash mismatch", "HashMismatch", differing expected/actual
hashes), panics/crashes ("panic", "thread.*panicked", "SIGABRT", "SIGSEGV"),
ERROR-level log lines, assertion failures ("assertion failed").

**Match ERROR-level lines by log-level prefix only**, e.g. anchor with
`grep -E '^[^ ]+Z\s+ERROR\s'` (timestamp followed by the ERROR level token).
A naive case-insensitive `error` match falsely fires on INFO-level lines that
contain the word "error" as part of a message body — e.g. peer disconnect
lines like `INFO ... recv error: IO error: Connection reset by peer`. Treat
those as the normal overlay-churn INFO they are; do not flag.

**(2) Ledger progression & sync deadline** — persist ledger progression across
ticks so STUCK can be detected by a single invocation:

- Read `/home/tomer/data/$MONITOR_SESSION_ID/last_ledger` (if it exists) —
  format is `"<ledger>|<unix-timestamp>"`.
- Extract the current ledger from the most recent heartbeat event (`heartbeat=true`) in the log tail.
- If the file exists and its ledger equals the current ledger and the recorded
  timestamp is more than 600s old, flag STUCK.
- If the ledger has advanced or the file is missing, overwrite
  `/home/tomer/data/$MONITOR_SESSION_ID/last_ledger` with `"<current-ledger>|<now>"`.
- Check node uptime: `ps -o etime= -p $(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")`.
  Active deadline: **15m** when `FRESH_START=no` and `CRASH_RECOVERY=no`,
  **60m** when `CRASH_RECOVERY=yes`, **20m** when `FRESH_START=yes`.
- "Real-time sync" means RPC `age < 30s` — NOT just heartbeat event `gap=0`. Gap is
  the node's local view (`latest_ext - ledger`) and can stay at 0 even when
  the node is minutes behind the network. The authoritative wall-clock signal
  is RPC `age`.
- If uptime exceeds the active deadline AND the node is not in real-time sync:
  flag SYNC FAILURE if `gap > 5`, or `age > 30s`, or `heard_from_quorum=false`
  in the latest heartbeat event. Investigate the catchup path (checkpoint-boundary
  stalls, hash mismatches, event-loop freezes); do not just wait.
- **Progress carveout (only when `CRASH_RECOVERY=yes`)**: if lcl has advanced
  by ≥ 500 ledgers since the previous tick's `last_ledger`, the node is
  actively replaying — report CATCHING UP regardless of uptime. Flag SYNC
  FAILURE only when lcl stops advancing AND uptime exceeds 60m.
- **Fresh-start carveout (`FRESH_START=yes`, uptime < 20m)**: a large gap is
  expected during initial bucket apply — report CATCHING UP, not SYNC FAILURE.
- **Never-started corollary (issue #3799, shortens the 60m blind window)**: the
  60m crash-recovery deadline exists to accommodate a legitimate *long replay* —
  a pipeline that has not started is not a long replay. So when `CRASH_RECOVERY=yes`
  but the download-verify-replay pipeline has provably done nothing, report SYNC
  FAILURE at the **15m** mark instead of waiting 60m. All must hold:
  `apply_ledger_chain_success_total == 0` **AND** `apply_ledger_chain_failure_total == 0`
  (read from the `/metrics` scrape `current.prom` — a legitimate replay increments
  one within seconds), lcl has not advanced vs. the previous tick's `last_ledger`
  (read the prior value **before** the overwrite above), and uptime `>= 15m`. This
  is the check-(2) companion to the (3g) report-only detector. A node actively
  replaying (either counter non-zero, or lcl advancing) keeps the full 60m window
  via the Progress carveout above.

  ```bash
  PROM=/home/tomer/data/$MONITOR_SESSION_ID/metrics/current.prom
  APPLY_CHAIN_OK=$(awk '/^stellar_history_apply_ledger_chain_success_total /{printf "%d",$2}' "$PROM" 2>/dev/null)
  APPLY_CHAIN_FAIL=$(awk '/^stellar_history_apply_ledger_chain_failure_total /{printf "%d",$2}' "$PROM" 2>/dev/null)
  ```

**(3) Process alive** — find by `/proc/exe` path matching via `_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID"` (from `scripts/lib/monitor-decisions.sh`, sourced at skill init). This validates that the process binary is exactly `$HOME/data/$MONITOR_SESSION_ID/cargo-target/release/henyey` (including `(deleted)` suffix after rebuilds), scoping detection to the current session. Historical note: the original `pgrep -f 'henyey.*run'` form was abandoned because it false-matched parallel `Codex --print` agent processes; the intermediate `comm`-only replacement was abandoned because it false-matched any `henyey` process regardless of session (cross-session false positive, see #2467). If not running: Rotate-log with suffix `crashed`, then before Relaunch evaluate the **(3a) Repeated-FATAL state-wipe trigger** below.

**(3a) Repeated-FATAL state-wipe trigger** — a kill-loop on the same persisted state means the local lcl is corrupt and forward replay can never reconcile. When this happens, restart-without-wipe just accumulates crashed logs and stays offline. Detect and self-heal once per kill-loop:

```bash
logs_dir=/home/tomer/data/$MONITOR_SESSION_ID/logs
# Uses the shared detect_crash_state function from scripts/lib/monitor-decisions.sh
# (already sourced at skill init). Uses numeric mtime comparison (stat -c %Y),
# not -newermt, for portability and testability.
detect_crash_state "$logs_dir"
recent_count="$CRASH_RECENT_COUNT"
latest_crashed="$CRASH_LATEST_FILE"
hash_mismatch_signal="$CRASH_HASH_MISMATCH"
```

Trigger the wipe when ALL hold:
1. `recent_count >= 3` (3+ crashed rotations in the last 30 min — proves restart-without-fix isn't recovering)
2. `hash_mismatch_signal == "yes"` (the most recent crash logged the structured field `fatal_wipe_required=true` or the legacy prose "State wipe required before restart" — emitted by `trigger_fatal_shutdown()` for any unrecoverable local state corruption)
3. `FRESH_START=no` (don't fire on a fresh sync that hasn't completed yet)

When triggered:

```bash
# Stop any partially-running process first (defensive — should already be dead)
PID=$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")
[ -n "$PID" ] && kill "$PID" && sleep 5 && kill -0 "$PID" 2>/dev/null && kill -9 "$PID"

# Wipe the corrupt persisted state (recoverable from public network archive)
rm -f /home/tomer/data/mainnet/mainnet.db \
      /home/tomer/data/mainnet/mainnet.db-shm \
      /home/tomer/data/mainnet/mainnet.db-wal \
      /home/tomer/data/mainnet/mainnet.lock
rm -rf /home/tomer/data/mainnet/buckets

# Reset progression tracker so the next tick treats this as a fresh start
rm -f /home/tomer/data/$MONITOR_SESSION_ID/last_ledger

# Mark the wipe action — read by (3d) to detect post-wipe recurrence.
# Stores the wipe epoch; cleared by (3d) when stable-recovery condition holds.
date -u +%s > /home/tomer/data/$MONITOR_SESSION_ID/wipe_attempted_at
```

Then Relaunch. The next tick will see `FRESH_START=yes` (mainnet.db absent), apply the 20m sync deadline, and let the node fresh-catchup from network archive (typically ~10–15 min to validating, observed ~9 min on 2026-05-09).

File a new `urgent` GH issue documenting the wipe with the count of crashed rotations, the hash-mismatch evidence from the latest crashed log, and the cumulative downtime — this is a data point for whether the underlying recovery code path needs further hardening even though the immediate cause was already fixed. Board-route to Backlog: `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`

The 3-in-30-min trigger is self-rate-limiting only when the underlying fault is local state corruption — after a successful wipe + fresh-catchup, new `.crashed-*` rotations stop. **(3d) Post-wipe recurrence guard** below catches the alternate failure mode where the freshly-rebuilt state trips the same `fatal_wipe_required` signal — proof that the bug is in the apply path itself, not on disk — and prevents an infinite wipe→catchup→crash loop.

**(3d) Post-wipe recurrence guard** — when 3a's wipe doesn't fix the symptom (a new `crashed-*` rotation with `fatal_wipe_required=true` appears AFTER the wipe), the binary is the suspect, not local state. Looping 3a indefinitely wastes downtime and archive bandwidth. Stop relaunching, auto-quarantine the build SHA, and wait for operator-applied rollback or fix. Evaluate (3d) BEFORE (3a) in the dead-process path; if (3d) fires, skip (3a) and skip Relaunch.

```bash
marker=/home/tomer/data/$MONITOR_SESSION_ID/wipe_attempted_at
post_wipe_recurrence=no
if [ -s "$marker" ]; then
  wipe_epoch=$(cat "$marker" 2>/dev/null | tr -dc '0-9')
  # Use the same detect_crash_state result already populated for (3a):
  # CRASH_LATEST_FILE + CRASH_HASH_MISMATCH are valid in this scope.
  if [ -n "${CRASH_LATEST_FILE:-}" ] && [ -n "${wipe_epoch:-}" ]; then
    latest_mtime=$(stat -c %Y "$CRASH_LATEST_FILE" 2>/dev/null || echo 0)
    if [ "$latest_mtime" -gt "$wipe_epoch" ] && [ "${CRASH_HASH_MISMATCH:-no}" = "yes" ]; then
      post_wipe_recurrence=yes
    fi
  fi
fi
```

When `post_wipe_recurrence=yes`:

```bash
# 1. Auto-quarantine the build SHA. Idempotent — already-present is OK.
source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
build_sha=$(cat "/home/tomer/data/$MONITOR_SESSION_ID/build_sha" 2>/dev/null || true)
if [ -n "$build_sha" ]; then
  quarantine_append "$HOME/data/deploy_quarantine.txt" "$build_sha" \
    "post-wipe recurrence (3d) — see urgent issue"
fi

# 2. Do NOT relaunch. Do NOT fire (3a) again — wipe already proved insufficient.
# 3. Report OFFLINE with `actions: ["post-wipe-recurrence", "auto-quarantined"]`
#    and `watch: ["determinism-suspect-binary"]`.
```

File or comment on an `urgent` GH issue marking the binary as a determinism-suspect.
Title pattern: `"OFFLINE: post-wipe recurrence on <sha:8> — binary quarantined"`.
Body marker for dedup: `<!-- monitor-tick-recurrence-key: <build_sha> -->`.
Include the build SHA, the wipe epoch from the marker, the latest crashed-log
hash-mismatch evidence, and the cumulative downtime since wipe
(`$(date -u +%s) - wipe_epoch` seconds).

**Persistent dedup** prevents duplicate issue filing across consecutive
monitor-tick invocations (GitHub search index lag can cause races). The dedup
file `.monitor-tick-filed.json` at `$REPO_ROOT` records which build SHAs have
already had issues filed. This mirrors the alarm-regression dedup pattern in
`check-alarm-regression.sh:778–833`.

**Guard: empty build_sha.** If `build_sha` is empty or unreadable, do NOT
file an issue (the title, body marker, and dedup key all depend on it).
Report in tick output:
`actions: ["post-wipe-recurrence", "auto-quarantined", "filing-skipped-no-sha"]`
and `watch: ["build-sha-missing"]`. Do NOT enter the flock/dedup/filing path.

**All dedup + filing logic runs inside a flock** to serialize concurrent
tick invocations:

```bash
REPO_ROOT="$(git rev-parse --show-toplevel)"
DEDUP_FILE="$REPO_ROOT/.monitor-tick-filed.json"
DEDUP_LOCK="$REPO_ROOT/.monitor-tick-filing.lock"
EXPECTED_TITLE="OFFLINE: post-wipe recurrence on ${build_sha:0:8} — binary quarantined"
BODY_MARKER="<!-- monitor-tick-recurrence-key: $build_sha -->"

(
  flock -w 30 9 || {
    echo "Could not acquire monitor-tick filing lock after 30s; skipping." >&2
    exit 0
  }

  # --- Load and prune dedup file using shared library ---
  source "$REPO_ROOT/scripts/lib/dedup-filing.sh"
  DEDUP_DATA=$(dedup_load "$DEDUP_FILE")
  DEDUP_DATA=$(dedup_prune "$DEDUP_DATA" "30d")

  # --- Helper: post rate-limited daily comment on existing issue ---
  post_daily_comment() {
    local issue_num="$1"
    local today
    today=$(date -u +%Y-%m-%d)

    # Always ensure urgent label (idempotent, runs every hit regardless of rate limit)
    gh issue edit "$issue_num" --add-label urgent 2>/dev/null || true

    # Ensure issue is on project board (add-if-missing only, don't regress active status).
    # Query whether the issue already has a project item; only add to Backlog if absent.
    local on_board
    on_board=$(gh api graphql -f query='
      query($url: URI!) {
        resource(url: $url) {
          ... on Issue {
            projectItems(first: 1) { totalCount }
          }
        }
      }' -f url="https://github.com/stellar-experimental/henyey/issues/$issue_num" \
      --jq '.data.resource.projectItems.totalCount' 2>/dev/null) || on_board=""
    if [[ "$on_board" == "0" ]]; then
      bash .github/skills/shared/scripts/move-issue-status.sh "$issue_num" Backlog 2>/dev/null || true
    fi

    # Fast check: last_commented_date in dedup entry
    local last_date
    if entry_json=$(dedup_check "$DEDUP_DATA" "$build_sha" 2>/dev/null); then
      last_date=$(echo "$entry_json" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("last_commented_date",""))' 2>/dev/null) || last_date=""
    else
      last_date=""
    fi
    if [[ "$last_date" == "$today" ]]; then
      return 0
    fi

    local wipe_epoch downtime_sec downtime_hours
    wipe_epoch=$(cat "$marker" 2>/dev/null | tr -dc '0-9')
    downtime_sec=$(( $(date -u +%s) - ${wipe_epoch:-0} ))
    downtime_hours=$(( downtime_sec / 3600 ))
    local update_marker="<!-- monitor-tick-recurrence-update: $build_sha $today -->"

    local update_body="### Post-wipe recurrence update (${today})

**Build SHA**: \`$build_sha\`
**Wipe epoch**: $(date -u -d @"${wipe_epoch:-0}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "${wipe_epoch:-unknown}")
**Cumulative downtime since wipe**: ${downtime_hours}h (${downtime_sec}s)
**Latest crash log**: $(basename "${CRASH_LATEST_FILE:-unknown}")
**Hash mismatch**: ${CRASH_HASH_MISMATCH:-unknown}

Binary remains quarantined. Operator action required.

$update_marker"
    if gh issue comment "$issue_num" --body "$update_body" 2>/dev/null; then
      echo "Posted update on existing issue #$issue_num for ${build_sha:0:8}" >&2
      # Update last_commented_date in dedup
      DEDUP_DATA=$(dedup_update_field "$DEDUP_DATA" "$build_sha" "last_commented_date" "$today")
      dedup_write "$DEDUP_FILE" "$DEDUP_DATA"
    else
      echo "WARNING: Failed to post update on #$issue_num" >&2
    fi
  }

  # --- Dedup check: local file ---
  DEDUP_HIT_ISSUE=""
  if dedup_entry=$(dedup_check "$DEDUP_DATA" "$build_sha" 2>/dev/null); then
    DEDUP_HIT_ISSUE=$(echo "$dedup_entry" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("issue_number",""))' 2>/dev/null) || DEDUP_HIT_ISSUE=""
  fi

  if [ -n "$DEDUP_HIT_ISSUE" ]; then
    ISSUE_STATE=$(gh issue view "$DEDUP_HIT_ISSUE" --json state \
      --jq .state 2>/dev/null) || ISSUE_STATE="UNKNOWN"
    case "$ISSUE_STATE" in
      OPEN)
        echo "Skipping: issue #$DEDUP_HIT_ISSUE already filed for ${build_sha:0:8}" >&2
        post_daily_comment "$DEDUP_HIT_ISSUE"
        exit 0
        ;;
      CLOSED)
        echo "Issue #$DEDUP_HIT_ISSUE is CLOSED — will refile with reference" >&2
        CLOSED_PREDECESSOR="$DEDUP_HIT_ISSUE"
        DEDUP_DATA=$(dedup_remove "$DEDUP_DATA" "$build_sha")
        # Fall through to remote search + filing
        ;;
      *)
        echo "WARNING: Cannot verify issue #$DEDUP_HIT_ISSUE (state: $ISSUE_STATE) — suppressing filing" >&2
        # Safe side: do not clear dedup, do not file duplicate
        exit 0
        ;;
    esac
  fi

  # --- Remote search fallback (crash-safe: catches cases where dedup file was lost) ---
  EXISTING=$(gh issue list --label urgent --state open \
    --search "in:body \"monitor-tick-recurrence-key: $build_sha\"" \
    --json number,body 2>/dev/null) || EXISTING="[]"

  FOUND_DUP=false
  DUP_ISSUE_NUMBER=""
  while IFS= read -r candidate; do
    [[ -z "$candidate" ]] && continue
    c_body=$(echo "$candidate" | python3 -c \
      "import json,sys; print(json.load(sys.stdin).get('body',''))" 2>/dev/null) || continue
    c_number=$(echo "$candidate" | python3 -c \
      "import json,sys; print(json.load(sys.stdin).get('number',''))" 2>/dev/null) || continue
    if echo "$c_body" | grep -qF "$BODY_MARKER" 2>/dev/null; then
      FOUND_DUP=true
      DUP_ISSUE_NUMBER="$c_number"
      break
    fi
  done < <(echo "$EXISTING" | python3 -c \
    "import json,sys; [print(json.dumps(x)) for x in json.load(sys.stdin)]" 2>/dev/null)

  if [ "$FOUND_DUP" = true ]; then
    echo "Remote search found existing issue #$DUP_ISSUE_NUMBER — backfilling dedup" >&2
    DEDUP_DATA=$(dedup_record "$DEDUP_DATA" "$build_sha" "issue_number=$DUP_ISSUE_NUMBER" "last_commented_date=")
    dedup_write "$DEDUP_FILE" "$DEDUP_DATA"
    post_daily_comment "$DUP_ISSUE_NUMBER"
    exit 0
  fi

  # --- File new issue ---
  wipe_epoch=$(cat "$marker" 2>/dev/null | tr -dc '0-9')
  downtime_sec=$(( $(date -u +%s) - ${wipe_epoch:-0} ))
  downtime_hours=$(( downtime_sec / 3600 ))
  predecessor_ref=""
  if [ -n "${CLOSED_PREDECESSOR:-}" ]; then
    predecessor_ref="
**Related to #$CLOSED_PREDECESSOR (closed)** — recurrence on same SHA."
  fi

  ISSUE_BODY="$BODY_MARKER
## OFFLINE: post-wipe recurrence — binary quarantined

**Build SHA**: \`$build_sha\`
**Wipe epoch**: $(date -u -d @"${wipe_epoch:-0}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "${wipe_epoch:-unknown}")
**Cumulative downtime since wipe**: ${downtime_hours}h (${downtime_sec}s)
**Latest crash log**: $(basename "${CRASH_LATEST_FILE:-unknown}")
**Hash mismatch evidence**: ${CRASH_HASH_MISMATCH:-unknown}
${predecessor_ref}"

  N=$(gh issue create \
    --title "$EXPECTED_TITLE" \
    --body "$ISSUE_BODY" \
    --label "urgent" \
    2>/dev/null | grep -oP '\d+$') || N=""

  if [ -n "$N" ]; then
    echo "Filed post-wipe recurrence issue #$N for ${build_sha:0:8}" >&2
    bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog

    # Record in dedup file
    TODAY=$(date -u +%Y-%m-%d)
    DEDUP_DATA=$(dedup_record "$DEDUP_DATA" "$build_sha" "issue_number=$N" "last_commented_date=$TODAY")
    dedup_write "$DEDUP_FILE" "$DEDUP_DATA"
  else
    echo "ERROR: Failed to create post-wipe recurrence issue — will retry next tick" >&2
    # Do NOT record in dedup — next tick will retry
  fi

) 9>"$DEDUP_LOCK"
```

Board-route any newly filed issue to Backlog: `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`

**Recovery from (3d) is operator-driven by way of landing a revert/fix.** The deploy gate (section 10 step 3) blocks redeploy of the quarantined SHA only while its diff is still applied to `origin/main`. Operator action: revert or refactor the offending changes on `origin/main`; the next tick's gate auto-clears once the bad content is gone from HEAD, and rebuild + relaunch proceeds normally. The marker is cleared automatically by the recovery rule below — operators do not need to remove it manually.

**Marker-clear rule** — runs each tick when the process is alive. Clear `wipe_attempted_at` only when the binary has demonstrably stabilized:

```bash
marker=/home/tomer/data/$MONITOR_SESSION_ID/wipe_attempted_at
if [ -s "$marker" ] && [ -n "${PID:-}" ]; then
  PROC_START_EPOCH=$(stat -c %Y /proc/$PID/stat 2>/dev/null || echo 0)
  uptime_sec=$(( $(date -u +%s) - PROC_START_EPOCH ))
  has_fatal_wipe_evidence "$logs_dir" "$logs_dir/monitor.log"  # sets FATAL_WIPE_EVIDENCE
  ledger_advanced=no
  # last_ledger holds the previous tick's value, stashed before check (2) overwrites
  if [ -n "${PREV_LEDGER:-}" ] && [ -n "${LEDGER:-}" ] && [ "$LEDGER" -gt "$PREV_LEDGER" ]; then
    ledger_advanced=yes
  fi
  if [ "$uptime_sec" -gt 1800 ] \
     && [ "${FATAL_WIPE_EVIDENCE:-no}" = "no" ] \
     && [ "$ledger_advanced" = "yes" ]; then
    rm -f "$marker"
  fi
fi
```

All four conditions (alive, uptime > 30m, no `fatal_wipe_required` since proc start, last_ledger advanced) must hold simultaneously to clear the marker. Quarantine entries persist in the file, but the deploy gate auto-clears them once the bad commit's diff is no longer applied to `origin/main` (see Quarantine Clearance in section 10). Operators can manually `quarantine_remove` if needed.

**(3c) Soft-fail state-wipe trigger** — defense-in-depth for the case where
`trigger_fatal_shutdown()` signals exit but the process fails to terminate,
leaving it alive with `fatal_state_failure=true`, blocking all recovery, and
making no ledger progress. This complements (3a) which only fires post-mortem.
Evaluate (3c) BEFORE (3b) when the process IS alive. If (3c) fires, skip (3b)
— a wipe supersedes a plain restart.

```bash
logs_dir=/home/tomer/data/$MONITOR_SESSION_ID/logs
log_file="$logs_dir/monitor.log"

# PID (session-scoped via _find_session_process; skip (3c) if empty)
PID=$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")
[ -z "$PID" ] && : # skip — dead-process path (3a) handles this

# Process start time from /proc/$PID/stat mtime
PROC_START_EPOCH=$(stat -c %Y /proc/$PID/stat 2>/dev/null || echo 0)

# Uses shared functions from scripts/lib/monitor-decisions.sh
has_fatal_wipe_evidence "$logs_dir" "$log_file"
detect_soft_fail_blocked "$log_file" "$PROC_START_EPOCH"
```

Trigger the wipe when ALL hold:
1. `SOFT_FAIL_BLOCKED == "yes"` (WARN-level "Recovery escalation blocked" messages sustained for >= 5 min within current PID lifetime, most recent within 90s of now)
2. `FATAL_WIPE_EVIDENCE == "yes"` (`fatal_wipe_required=true` signal found in any crashed rotation OR the active log — no time window, confirms persistent state corruption)
3. `FRESH_START == "no"` (not a fresh sync)
4. **No ledger progress since previous tick** — `last_ledger` unchanged (stash previous tick's value before check (2) overwrites; compare against current ledger)

When triggered:

```bash
# 1. Stop the alive-but-stuck process
kill "$PID" && sleep 5
kill -0 "$PID" 2>/dev/null && kill -9 "$PID" && sleep 2

# 2. Rotate log (preserve evidence, consistent suffix with 3a)
mv "$log_file" "${log_file}.crashed-$(date -u +%Y%m%dT%H%M%SZ)" 2>/dev/null || true

# 3. Wipe corrupt persisted state (same artifacts as 3a)
rm -f /home/tomer/data/mainnet/mainnet.db \
      /home/tomer/data/mainnet/mainnet.db-shm \
      /home/tomer/data/mainnet/mainnet.db-wal \
      /home/tomer/data/mainnet/mainnet.lock
rm -rf /home/tomer/data/mainnet/buckets

# 4. Reset progression tracker
rm -f /home/tomer/data/$MONITOR_SESSION_ID/last_ledger
```

Then Relaunch. The next tick will see `FRESH_START=yes` (mainnet.db absent).

File a new `urgent` GH issue documenting the soft-fail wipe with: blocked
duration (`SOFT_FAIL_BLOCKED_DURATION_SEC`), evidence source
(`FATAL_WIPE_SOURCE`), and cumulative downtime. Use title pattern:
`"Soft-fail state wipe: fatal_state_failure stuck for {N}m"`. Always a new
issue (no dedup — each wipe is a distinct incident). Known prior incidents: #2363.
Board-route to Backlog: `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`

Self-limiting: after wipe, `FRESH_START=yes` blocks condition (3); new process
has no `fatal_state_failure` so condition (1) fails; log rotation removes old
blocked messages from active log.

**(3b) Wedge detection** — a process can be alive but have a frozen event
loop (watchdog fires, HTTP hangs, ledger progression stops). Check 3 alone
misses this because `pgrep` still finds the PID.

Flag WEDGE when BOTH:
1. `grep -E 'watchdog_freeze"?\s*[=:]\s*true|WATCHDOG: Event loop appears frozen' $LOG | tail -1` is present
   with a timestamp within the last 120s.
   (The structured field `watchdog_freeze=true` is the primary signal; the
   prose string is a legacy fallback. The `"?` accounts for JSON key quoting.)
2. `curl -s -m 3 http://localhost:$MONITOR_ADMIN_PORT/info` returns empty
   body or times out.

On WEDGE: Stop-PID, Rotate-log with suffix `frozen`, then Relaunch.
Always file a new `urgent`-labeled issue (wedge blocks validator operation).
Board-route to Backlog: `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
Recurrence-after-fix → NEW issue, not a comment on a closed one. Known prior
incidents: #1904, #1873, #1921, #1949.

**(3e) Stuck-but-alive SYNC FAILURE auto-restart** — a node can be alive AND
responsive (admin `/info` answers) yet make no ledger progress: a frozen local
`lcl`, climbing RPC `age`, RPC `unhealthy`, and RSS under the OOM floor. This is
the residual mode left after (3c) (owns the fatal-state-wipe case) and (3b)
(owns the UNRESPONSIVE / frozen-event-loop case). (3e) is the **mirror of (3b)**:
(3b) fires when the admin port is dead/timed-out; (3e) requires a *live,
answering* port — mutually exclusive by construction. It is a band-aid for root
cause #3218 (overlay SCP broadcast backpressure); the escalate-on-streak guard
surfaces a restart loop as `urgent` rather than silently masking the defect.

Evaluate (3e) **strictly last** in the alive-process path — order is
(3c) soft-fail-wipe → (3b) wedge → **(3e) stuck-alive-sync** — so it only catches
the residual responsive-but-frozen state. The decision is made by the pure
function `classify_stuck_alive_sync` (`scripts/lib/monitor-decisions.sh`, already
sourced), which is the **sole reader/writer** of the per-session streak state
file; the caller passes only the live `lcl` and the file path.

```bash
# Skip unless the process is alive AND this is NOT the wedge case (3b owns
# unresponsive). PROC_RESPONSIVE: admin /info answered non-empty within timeout.
[ -z "$PID" ] && : # skip — dead-process path (3a/3d) handles this
INFO_BODY=$(curl -s -m 3 "http://localhost:$MONITOR_ADMIN_PORT/info" 2>/dev/null)
PROC_RESPONSIVE=$([ -n "$INFO_BODY" ] && echo yes || echo no)
# NODE_STATE: the /info `state` field — "Synced!" (healthy/validating),
# "Catching up", "Booting", "Stopping" (crates/app/.../handlers/info.rs).
NODE_STATE=$(printf '%s' "$INFO_BODY" | grep -oP '"state"\s*:\s*"\K[^"]+' | head -1)
# CURRENT_LCL: local last-closed-ledger number (from /info `ledger.num`).
CURRENT_LCL=$(printf '%s' "$INFO_BODY" | grep -oP '"num"\s*:\s*\K[0-9]+' | head -1)
# AGE_SEC: RPC getHealth wall-clock `age` (the check-(2) staleness signal).
# RPC_STATUS: "healthy" | "unhealthy" from validator-mode getHealth.
# RSS_MB: the RSS measured in check (4) (reused; OOM gate owns RSS-over-floor).
STUCK_STATE_FILE="/home/tomer/data/$MONITOR_SESSION_ID/metrics/stuck_restart_state"
mkdir -p "$(dirname "$STUCK_STATE_FILE")"
classify_stuck_alive_sync "$NODE_STATE" "$CURRENT_LCL" "$AGE_SEC" "$RPC_STATUS" \
  "$RSS_MB" "$PROC_RESPONSIVE" "$STUCK_STATE_FILE"
```

`classify_stuck_alive_sync` sets `STUCK_ALIVE_SYNC`. It is `yes` only when ALL
six hold: state matches `valid|synced|track` (case-insensitive substring; never
`Catching up`/`Booting`); `PROC_RESPONSIVE=yes`; `RPC_STATUS=unhealthy`;
`AGE_SEC > 600`; frozen-lcl wall-clock dwell `>= 600s` (cadence-independent,
mirroring check (2)'s `<ledger>|<ts>` STUCK idiom — fires at ~10 min of freeze
regardless of tick spacing); and `RSS_MB < 16384`. Thresholds are named
constants at the top of the function for operator retuning. Guards: a **900s
cooldown** (`NOW - last_restart < 900` → `cooldown`, no restart this tick) and a
**max-3-restarts / 2h** guard (`>= 3` restarts in a 7200s rolling window →
`escalate`, no restart).

- On `STUCK_ALIVE_SYNC=yes`: **Stop-PID**, **Rotate-log** with suffix `stuck`,
  then **Relaunch** (verbatim reuse of the Common-procedures machinery). The
  `stuck` suffix is already a recognized rotation type (Crash-recovery signal 1
  and check (5) retention), so the next tick sees `CRASH_RECOVERY=yes` and
  extends the sync deadline to 60m — it will not false-fire SYNC FAILURE during
  the legitimate replay that follows the restart.
- On `STUCK_ALIVE_SYNC=escalate`: do **NOT** restart. File a single
  `urgent`-labeled issue (reusing the (3b)-wedge filing idiom) noting the
  restart streak and pointing at the unfixed root cause #3218 (overlay SCP
  broadcast backpressure / near-tip-stall recovery); if #3218 is closed,
  escalate to the operator. A restart loop is itself the signal that the root
  cause is unaddressed.
- On `STUCK_ALIVE_SYNC=cooldown` or `no`: report-only, no action this tick.

This trigger fires ONLY on the genuine stuck-but-alive signature and never
during legitimate transient sync/catch-up — a false restart on a consensus
validator is harmful. The 600s frozen-lcl dwell + six-way AND-gate + 900s
cooldown + max-3/2h guard enforce that conservatism.

**(3g) Catchup-stalled REPORT-ONLY detector** — the deterministic catchup wedge
(issue #3799; live reproducer #3702/#3797) that every other gate correctly
declines to fire on. A live, `/info`-answering node sits in `Catching Up` after
its startup catchup await never returns: `app.run()` (watchdog + main loop) is
never spawned, so the download-verify-replay pipeline never iterates. The node
freezes its `lcl` and does zero work while check (2) waits its full 60m
crash-recovery deadline. (3b) wedge needs a never-spawned watchdog + dead
`/info` (here /info answers); (3c) needs `fatal_wipe_required` (never emitted on
this path); (3e) excludes `Catching Up` **by design** and its exclusion is
load-bearing — a restart on a legitimately-replaying validator is harmful.

(3g) is the **mirror-sibling of (3e)**, mutually exclusive by construction: (3e)
requires `valid|synced|track`, (3g) requires `Catching Up`. It is **REPORT-ONLY**
— **no Stop/Rotate/Relaunch.** A restart would destroy the reproducer; the value
is in *noticing*, not acting. Evaluate (3g) in the alive path **strictly after**
(3c) soft-fail-wipe → (3b) wedge → (3e) stuck-alive-sync, reusing the
`NODE_STATE`/`CURRENT_LCL`/`PROC_RESPONSIVE` already computed by (3e) and
`STARTUP_PEAK_MB` from the check-7 startup-RSS snippet. The decision is made by
the pure function `classify_catchup_stalled` (`scripts/lib/monitor-decisions.sh`,
already sourced), the **sole reader/writer** of its own per-session state file
(`catchup_stalled_state` — NOT (3e)'s `stuck_restart_state`).

The discriminator: `henyey_startup_peak_anon_rss_mb` is set only *after* the
startup await returns, so it is **absent** (or the present-at-0 variant, `0`) on a
node whose await never returned. `classify_catchup_stalled` treats absent OR `0`
identically as "startup not complete"; a legitimately-replaying node has a
non-zero peak and is excluded.

```bash
UPTIME_SEC=$(ps -o etimes= -p "$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")" 2>/dev/null | tr -d ' ')
CATCHUP_STATE_FILE="/home/tomer/data/$MONITOR_SESSION_ID/metrics/catchup_stalled_state"
mkdir -p "$(dirname "$CATCHUP_STATE_FILE")"
classify_catchup_stalled "$NODE_STATE" "$CURRENT_LCL" "$STARTUP_PEAK_MB" \
  "$UPTIME_SEC" "$PROC_RESPONSIVE" "$CATCHUP_STATE_FILE" "$(date +%s)"
```

`classify_catchup_stalled` sets `CATCHUP_STALLED` ∈ `yes|no`. It is `yes` only
when ALL five hold: `NODE_STATE` matches `catching up` (case-insensitive
substring; disjoint from (3e)'s `valid|synced|track`); `PROC_RESPONSIVE=yes`
((3b) owns the unresponsive case); `STARTUP_PEAK_MB` empty or `0`;
`UPTIME_SEC >= 300`; and frozen-lcl wall-clock dwell `>= 600s` (cadence-independent,
mirroring check (2)'s `<ledger>|<ts>` STUCK idiom — the intentional ~10 min
first-detection latency is by design). Thresholds are named constants at the top
of the function.

- On `CATCHUP_STALLED=yes`: **do NOT restart.** Emit a WARNING banner (not
  ACTION — no corrective action is taken) and, using the `filing_search` dedup
  idiom, **search for the existing open catchup-stalled issue and comment on it**
  (do NOT open a fresh issue per tick while the condition persists),
  cross-linking #3702/#3797. Append `WATCH_ITEMS+=("catchup_stalled=1")`.
- On `CATCHUP_STALLED=no`: report-only, no action this tick.

**(4) Memory** — `ps -o rss= -p $(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")`, convert to MB.

The guardrail is **host-RAM-relative** (not absolute GB) so the same skill gives
early warning on a 32 GB/no-swap box without false-firing on a 61 GB box. The
thresholds live in the pure decision function `eval_memory_guardrail`
(`scripts/lib/monitor-decisions.sh`, already sourced), called with integer-MB
inputs. Pass:

- `RSS_MB` — the RSS just measured, in MB.
- `AVAIL_MB` — system `available` memory from `free -m` (NOT `free` — that
  excludes reclaimable kernel cache), in MB.
- `MONITOR_HOST_RAM_GB` — host physical RAM in GB, defaulting to `61`.
- the latest three `heap_components_mb` snapshots (`heap_prev2`, `heap_prev`,
  `heap_curr`) from the most recent `memory_report=true` / `Memory report
  summary` log entries (see check 7), so the function can test whether the
  latest two deltas both grew > 500 MB.

```bash
# AVAIL_MB from `free -m` (MemAvailable line); HEAP_* from the last three
# memory_report snapshots (see check 7). HOST_RAM_GB defaults to 61.
eval_memory_guardrail "$RSS_MB" "$AVAIL_MB" "${MONITOR_HOST_RAM_GB:-61}" \
  "$HEAP_PREV2_MB" "$HEAP_PREV_MB" "$HEAP_CURR_MB"
```

`eval_memory_guardrail` sets `MEMORY_GUARDRAIL_VERDICT`:

- `report-high-mem` — `RSS > 0.65 * host_ram` (21.3 GB @32, 40.6 GB @61). Flag
  HIGH MEMORY (report-only; no restart).
- `restart` — ALL of: `RSS > 0.75 * host_ram` (24 GB @32, 45.75 GB @61) AND
  `avail < 0.12 * host_ram` (~3.84 GB @32, ~7.3 GB @61) AND the latest two
  `heap_components_mb` deltas both grew > 500 MB. This gates on system pressure
  AND evidence of a real heap leak, so we don't kill a legit catchup (a transient
  cold-catchup RSS spike with heap NOT growing stays `report-high-mem`).
- `none` — otherwise.

When the verdict is `restart`: Stop-PID, Rotate-log suffix `crashed`, Relaunch.

**(5) Disk** — `df -h /home/tomer/data | tail -1`. If usage > 85%, flag LOW DISK.
Then keep the 3 most recent rotated archives **per category, by mtime**, via the
shared `prune_rotated_logs` helper (`scripts/lib/monitor-decisions.sh`, sourced
at skill init). The helper discovers every `monitor.log.<category>-*` category
present on disk — including legacy/other suffixes (`predeploy`, `coldcatchup`,
`stopped`, `prerestart`, `freshstart`, …), not just a hardcoded
`preredeploy/crashed/stuck/frozen` list — and orders retention by **mtime**, not
by lexical filename. mtime ordering is correct even when an infixed variant such
as `monitor.log.crashed-knit2lcl-20260615T192932Z` would otherwise sort its `k`
ahead of a bare `crashed-20260623T…` under `sort -r` and wrongly retain the OLD
log (#3616). It uses `find -printf` (not a shell glob) so it survives zsh
`NO_NOMATCH`:

```bash
logs_dir=/home/tomer/data/$MONITOR_SESSION_ID/logs
prune_rotated_logs "$logs_dir" 3   # keeps newest 3 per category by mtime; sets PRUNED_LOG_COUNT
```
Report how many files were removed (`PRUNED_LOG_COUNT`) if any.

**(6) Session disk** — `du -sh /home/tomer/data/$MONITOR_SESSION_ID/`
and `du -sh /home/tomer/data/mainnet/`. If combined > 200 GB, flag SESSION DISK HIGH.

**(7) Memory report** — `grep -E 'memory_report=true|Memory report summary' /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log | tail -1`.
If grep returns no output AND process uptime > 400s, flag WARNING
memory-report-missing (log format may have changed). Memory reports emit
every ~6 minutes, so an uptime below 400s legitimately has no entry yet on
a post-restart tick — report `memreport: N/A (warmup)` and move on.
Otherwise extract `jemalloc_allocated_mb`, `jemalloc_resident_mb`,
`fragmentation_pct`, `heap_components_mb`, `mmap_mb`, `unaccounted_mb`,
`unaccounted_sign`. If `fragmentation_pct > 50`, flag HIGH FRAGMENTATION.
If `unaccounted_mb > 1000` with sign `+`, note it (known jemalloc overhead,
not a bug — but verify `heap_components` is stable; if it is growing, investigate).

**Startup/catchup peak RSS** — surface the `henyey_startup_peak_anon_rss_mb`
gauge (issue #3226/#3227; sampler in `crates/ledger/src/peak_rss_sampler.rs`).
This captures the startup-restore + catchup peak that the `% 64` ledger-close
memory report misses (the ~27–29 GB cold-catchup transient). Read it from the
check-12 `/metrics` scrape (`current.prom`) rather than re-scraping; read the
attributed `phase` from the greppable log summary line:

```bash
PROM=/home/tomer/data/$MONITOR_SESSION_ID/metrics/current.prom
STARTUP_PEAK_MB=$(awk '/^henyey_startup_peak_anon_rss_mb /{printf "%d", $2}' "$PROM" 2>/dev/null)
# phase label lives in the log summary line, not the gauge:
#   "...startup_peak_anon_rss_mb=<N> phase=<phase>..."
STARTUP_PEAK_PHASE=$(grep -oE 'startup_peak_anon_rss_mb=[0-9]+ phase=[a-z_-]+' \
  /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log 2>/dev/null \
  | tail -1 | grep -oE 'phase=[a-z_-]+' | cut -d= -f2)
if [ -n "$STARTUP_PEAK_MB" ] && [ "$STARTUP_PEAK_MB" -gt 0 ]; then
  WATCH_ITEMS+=("startup_peak_mb=$STARTUP_PEAK_MB${STARTUP_PEAK_PHASE:+ (phase=$STARTUP_PEAK_PHASE)}")
fi
```

`STARTUP_PEAK_MB` is rendered on the `mem:` status line (see Output, `peak=`).
The `startup_peak_mb=` watch entry lets the daily summary track peak creep across
restarts. The gauge is set once at sampler stop (before the first ledger close),
so it is absent on a node that has not finished startup — treat empty/absent as
"no peak yet" and omit the watch entry, not a warning.

**(8) RPC health** (validator mode only — skip in watcher mode) —
`curl -s -X POST http://localhost:$MONITOR_RPC_PORT -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'`.
Verify response is non-empty and `status` is `healthy`. Check pruning:
`latestLedger - oldestLedger` should be bounded.
- Baseline (data only) is `retention_window` ledgers, plus a maintenance-cycle
  buffer (maintenance every 900s ≈ 180 ledgers at 5s/ledger).
- Since `57821bcf`/`25797e2e` (Apr 27), ledger headers and tx history are
  also held back from pruning to satisfy publishing. The new equilibrium is
  ~3× retention_window (~1050 for retention=360).
- If `gap > 3 × retention_window + 500` (~1580 for retention=360), flag
  PRUNING STALLED — that's "headers/tx-history protection plus an extra
  maintenance cycle's slack" exceeded, indicating real pruning failure.
- If RPC is not responding, flag RPC DOWN.

**(9) OBSRVR Radar** (validator mode only — skip in watcher mode) — get
public key from `curl -s http://localhost:$MONITOR_ADMIN_PORT/info`
(extract `public_key`). Then `curl -s https://radar.withobsrvr.com/api/v1/nodes/<PUBLIC_KEY>`.
Check:
- `isValidating` — if false and node running > 30 min, flag NOT VALIDATING.
- `validating24HoursPercentage` — if < 50 and running > 6 hours, flag LOW VALIDATION RATE.
- `lag` — if > 500, flag HIGH LAG.

If the API errors out, emit `obsrvr: N/A (api-error)` instead of omitting the field.

If the API returns a response but `latestLedger` or `updatedAt` is missing/null
(partial response), treat the entire radar result as stale / incomplete and
emit `obsrvr: N/A (api-incomplete)`. Do NOT evaluate `lag` in this case — a
`lag` value returned alongside a null `latestLedger`/`updatedAt` is a cached
aggregate from a prior observation window (observed post-restart: lag=8754
against a node that is in real-time sync with age=2s).

**(12) Metrics scan** — scrape `/metrics` and evaluate the alert catalog.

1. `mkdir -p /home/tomer/data/$MONITOR_SESSION_ID/metrics`.
2. `mv /home/tomer/data/$MONITOR_SESSION_ID/metrics/current.prom /home/tomer/data/$MONITOR_SESSION_ID/metrics/prev.prom 2>/dev/null || true`.
3. Capture process identity (before curl):
   ```bash
   PID=$(_find_session_process "$HOME/data" "/proc" "$MONITOR_SESSION_ID")
   # If PID is empty (process not running), set TICK_SKIPPED=true and skip to step 7 (archive).
   START_TICKS=$(awk '{print $22}' /proc/$PID/stat 2>/dev/null)
   # If /proc/$PID/stat is unreadable, set TICK_SKIPPED=true and skip to step 7 (archive).
   ```
4. `curl -s http://localhost:$MONITOR_ADMIN_PORT/metrics > /home/tomer/data/$MONITOR_SESSION_ID/metrics/current.prom`.
5. **Process identity check for prev.prom validity:**

   Verify PID stability across scrape:
   ```bash
   POST_TICKS=$(awk '{print $22}' /proc/$PID/stat 2>/dev/null)
   # If unreadable OR POST_TICKS != START_TICKS: process died or was replaced
   # during the scrape. Discard current.prom (truncate to empty). Do NOT write
   # scrape_identity. Set TICK_SKIPPED=true and skip to step 7 (archive).
   ```

   **Identity file:** `/home/tomer/data/$MONITOR_SESSION_ID/metrics/scrape_identity`

   This file describes the process that produced the scrape **now in `prev.prom`**.
   It is written at the end of this step with the current tick's identity, which
   becomes the prev.prom identity on the next tick (after step 2's `mv`).

   Format (version 1):
   ```
   version=1
   pid=<PID>
   start_ticks=<field 22 from /proc/$PID/stat>
   timestamp=<ISO8601>
   ```

   If `version` is not `1`, treat as malformed.

   Procedure:
   - If `scrape_identity` exists and is well-formed (`version=1`, contains `pid=`
     and `start_ticks=` lines):
     - Read `prev_pid` and `prev_start_ticks` from it.
     - If `prev_pid != PID` or `prev_start_ticks != START_TICKS`:
       **Process identity changed.** Set `PREV_PROM_INVALID=true`,
       reason=`process identity changed`.
     - If identity matches AND `prev.prom` is missing or empty:
       **No baseline data.** Set `PREV_PROM_INVALID=true`,
       reason=`no prev.prom`.
   - If `scrape_identity` does not exist OR is malformed (`version` ≠ `1`,
     missing required fields):
     Set `PREV_PROM_INVALID=true` unconditionally,
     reason=`no scrape_identity` or `scrape_identity malformed`.
     This handles rollout (existing sessions have prev.prom but no
     scrape_identity), manual deletion, and corruption.
   - **Gap-staleness check (`PREV_SCRAPE_AGE_SECONDS`, #3246):** the identity
     check above only catches a *process* change (PID/`start_ticks`). It does
     NOT catch the case where the monitor **loop** stalled across a long
     wall-clock gap while the validator **process survived** — same PID, so
     `PREV_PROM_INVALID=false`, but `prev.prom`/the snapshot baseline is now
     hours old. The counter-delta would then be computed across the whole gap
     and false-fire as an acute burst (the headline tick-184 `recovery-stalled`
     case). Compute `prev_scrape_age = now − timestamp(scrape_identity)` from
     the `timestamp=` field of `scrape_identity` (which describes the process
     that produced `prev.prom`) and export it for the evaluator:
     ```bash
     # prev_ts = ISO8601 timestamp from the EXISTING scrape_identity (the one
     # describing prev.prom), read BEFORE the fresh write below. If the file is
     # absent/malformed/has no timestamp, leave the age unknown (-1) — that case
     # is already covered by PREV_PROM_INVALID, and an unknown age is treated as
     # NOT stale (fail-safe).
     PREV_TS=$(awk -F= '/^timestamp=/{print $2}' \
       /home/tomer/data/$MONITOR_SESSION_ID/metrics/scrape_identity 2>/dev/null)
     if [ -n "$PREV_TS" ]; then
       PREV_EPOCH=$(date -u -d "$PREV_TS" +%s 2>/dev/null || echo "")
       NOW_EPOCH=$(date -u +%s)
       if [ -n "$PREV_EPOCH" ]; then
         export PREV_SCRAPE_AGE_SECONDS=$(( NOW_EPOCH - PREV_EPOCH ))
       else
         export PREV_SCRAPE_AGE_SECONDS=-1
       fi
     else
       export PREV_SCRAPE_AGE_SECONDS=-1
     fi
     ```
   - Write fresh identity file:
     ```bash
     printf "version=1\npid=%s\nstart_ticks=%s\ntimestamp=%s\n" \
       "$PID" "$START_TICKS" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
       > /home/tomer/data/$MONITOR_SESSION_ID/metrics/scrape_identity
     ```

   **Scrape failure / "recorder not installed" handling:** If the curl in step 4
   returns empty or contains "recorder not installed", the existing metrics scan
   skip logic halts processing before reaching this step's comparison logic.
   `scrape_identity` from the previous tick is left untouched (still correctly
   describes `prev.prom`). The fresh identity write is not reached, so no stale
   identity is written. On the next tick, step 2 will `mv current.prom prev.prom`
   (moving the empty scrape), and the "identity matches AND prev.prom is missing
   or empty" clause will set `PREV_PROM_INVALID=true`.

   **When `PREV_PROM_INVALID=true`:**
   - Skip all §COUNTERS delta checks for this tick.
   - Skip all §HISTOGRAMS delta checks for this tick (including the mean
     fallback `sum_delta / count_delta`).
   - For §GAUGES with "two consecutive ticks" persistence guards
     (`henyey_jemalloc_fragmentation_pct`, `henyey_scp_verify_input_backlog`,
     `henyey_overlay_fetch_channel_depth`): reset the persistence counter.
     The previous tick's gauge reading from `prev.prom` is from a different
     process incarnation and must not count toward the two-tick requirement.
     Evaluate the current gauge value normally but require a second consecutive
     breach on the next tick before firing.
   - All other §GAUGES are unaffected — they are point-in-time readings from
     `current.prom` only.
   - Do NOT skip ratio_snapshot or counter_streak_snapshot checks — they have
     their own independent PID/start_ticks invalidation logic and snapshot files.
     Independence is safe: each check reads PID/start_ticks from `/proc` and
     compares against its own snapshot.

   **Gap-stale baseline (`PREV_SCRAPE_AGE_SECONDS`, #3246):** derived in the
   evaluator as `gap_stale = age >= 0 and age >= GAP_STALE_THRESHOLD_SECONDS`
   (default `3600` ≈ 3× the nominal ~20-min tick). This is the **distinct**
   same-PID-old-baseline case: PID-change (#3206) trips `PREV_PROM_INVALID`
   FIRST; gap-stale is sequenced AFTER the identity check, so the two are never
   double-handled. Applied **asymmetrically** by alarm family:
   - **prev.prom families** (counter, counter-dynamic, histogram-p99): **SKIP**
     the acute fire this tick with a `gap-stale (prev age Xh)` reason — `prev.prom`
     is recomputed from the file each tick, so no persisted baseline to reseed.
   - **snapshot families** (counter-ratio, counter-streak — incl. the headline
     `recovery-stalled` burst): **RE-BASELINE** — rewrite the snapshot with
     current values and reset the streak to `0` (reusing the counter-reset path),
     because these snapshots PERSIST across ticks; a plain skip would leave the
     stale baseline and fire on the NEXT tick.
   - **Age-unknown (`-1`/absent)** is treated as NOT stale (fail-safe; a truly
     missing identity is already covered by `PREV_PROM_INVALID`).

6. **Counter reset handling**: for any counter, if `current < prev`, treat
   `delta = current` (defense-in-depth for within-incarnation counter resets).

7. **Archive snapshot** — archive the current tick's metrics and metadata for
   historical replay (see `scripts/dev/replay-alarms-on-history.sh`). This step
   runs regardless of whether evaluation was skipped (`TICK_SKIPPED` captures
   that). All ticks are archived to preserve accurate sequential replay state.

   ```bash
   ARCHIVE_DIR="$HOME/data/$MONITOR_SESSION_ID/metrics/archive"
   mkdir -p "$ARCHIVE_DIR"
   TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)
   SNAP_TMP="$ARCHIVE_DIR/${TIMESTAMP}.tmp"
   SNAP_FINAL="$ARCHIVE_DIR/${TIMESTAMP}"
   mkdir -p "$SNAP_TMP"

   # Copy prom files (may be empty/missing — expected for skipped ticks)
   cp "$HOME/data/$MONITOR_SESSION_ID/metrics/current.prom" "$SNAP_TMP/current.prom" 2>/dev/null || true
   cp "$HOME/data/$MONITOR_SESSION_ID/metrics/prev.prom"    "$SNAP_TMP/prev.prom"    2>/dev/null || true

   # Write metadata sidecar (marks directory as complete)
   cat > "$SNAP_TMP/metadata.env" << METAEOF
   ARCHIVE_VERSION=1
   TICK_SKIPPED=${TICK_SKIPPED:-false}
   PREV_PROM_INVALID=${PREV_PROM_INVALID:-false}
   PREV_SCRAPE_AGE_SECONDS=${PREV_SCRAPE_AGE_SECONDS:--1}
   WARMUP_TICKS_REMAINING=${WARMUP_TICKS_REMAINING:-0}
   FRESH_START=${FRESH_START:-no}
   CRASH_RECOVERY=${CRASH_RECOVERY:-no}
   UPTIME_SECONDS=${UPTIME_SECONDS:-0}
   MONITOR_MODE=${MONITOR_MODE:-validator}
   PID=${PID:-}
   START_TICKS=${START_TICKS:-}
   METAEOF

   # Atomic rename
   mv "$SNAP_TMP" "$SNAP_FINAL"

   # Retention: keep newest 500 complete archive dirs (ignore .tmp).
   # At ~20 min/tick ≈ 7 days of history. This bounds the replay window.
   # Pruned data is gone — not recoverable. For longer investigations,
   # bump retention or implement cold-archiving. See #2573 Gap 4.
   SNAPSHOTS=()
   while IFS= read -r -d '' d; do
     SNAPSHOTS+=("$d")
   done < <(find "$ARCHIVE_DIR" -maxdepth 1 -mindepth 1 -type d \
     ! -name '*.tmp' -print0 | sort -z)
   ARCHIVE_COUNT=${#SNAPSHOTS[@]}
   if [ "$ARCHIVE_COUNT" -gt 500 ]; then
     EXCESS=$((ARCHIVE_COUNT - 500))
     for ((i=0; i<EXCESS; i++)); do
       rm -rf "${SNAPSHOTS[$i]}"
     done
   fi

   # Clean up orphaned .tmp dirs from crashed prior ticks
   find "$ARCHIVE_DIR" -maxdepth 1 -name '*.tmp' -type d -mmin +5 \
     -exec rm -rf {} + 2>/dev/null || true
   ```

8. **Weekly alarm regression replay** — replay the archived metrics history
   through the current alarm catalog and compare against a stored baseline
   to detect regressions (alarms that were meaningfully active but have gone
   silent). This step only runs for validator mode — watcher keeps a reduced
   catalog (process/jemalloc/overlay only, no SCP/quorum/ratio/p99) that
   doesn't have action semantics worth regressing. Watcher mode emits no
   replay watch item.

   See `scripts/dev/check-alarm-regression.sh` for the baseline comparison,
   regression detection, and issue-filing logic.

   **Replay state machine** (validator-only): surfaces the highest-priority
   applicable state as a `replay_state=` watch item. States in precedence
   order: `failed` > `archive-too-small` > `stale` > `never-run`. When
   replay succeeded within 14 days, no watch item is emitted.

   | State | Condition | Emitted string |
   |-------|-----------|---------------|
   | `never-run` | No `replay-last-run.ts` (no successful replay ever) | `replay_state=never-run` |
   | `archive-too-small` | Replay ran but `evaluated_ticks < 100` | `replay_state=archive-too-small` |
   | `failed` | Replay script error, empty/invalid JSON, or `check-alarm-regression.sh` nonzero exit | `replay_state=failed` |
   | `stale` | `replay-last-run.ts` exists but age > 1209600s (14 days) | `replay_state=stale` |
   | ok | Last successful replay within 14 days, no failure this tick | (none) |

   `replay-last-run.ts` is written only on success — it records the
   timestamp of the last *successful* replay.

   ```bash
   # Step 8: Weekly alarm regression replay (validator-only)
   if [[ "$MONITOR_MODE" == "validator" ]]; then
     REPLAY_THROTTLE="$METRICS_DIR/replay-last-run.ts"
     NOW_TS=$(date +%s)
     RUN_REPLAY=false
     REPLAY_FAILED=false

     if [[ ! -f "$REPLAY_THROTTLE" ]]; then
       RUN_REPLAY=true
     else
       LAST_RUN=$(cat "$REPLAY_THROTTLE" 2>/dev/null || echo "0")
       ELAPSED=$((NOW_TS - LAST_RUN))
       if [[ $ELAPSED -ge 604800 ]]; then
         RUN_REPLAY=true
       fi
     fi

     if [[ "$RUN_REPLAY" == true ]]; then
       REPLAY_JSON=$("$REPO_ROOT/scripts/dev/replay-alarms-on-history.sh" \
         "$HOME/data/$MONITOR_SESSION_ID" --replay --json 2>/dev/null) || REPLAY_FAILED=true

       if [[ "$REPLAY_FAILED" != true ]] && [[ -n "$REPLAY_JSON" ]]; then
         EVAL_TICKS=$(echo "$REPLAY_JSON" | python3 -c \
           "import json,sys; print(json.load(sys.stdin).get('evaluated_ticks',0))" \
           2>/dev/null) || { EVAL_TICKS=0; REPLAY_FAILED=true; }

         if [[ "$REPLAY_FAILED" != true ]] && [[ "$EVAL_TICKS" -ge 100 ]]; then
           # Write current replay to temp file for regression check
           echo "$REPLAY_JSON" > "$METRICS_DIR/replay-current.json"
           if "$REPO_ROOT/scripts/dev/check-alarm-regression.sh" \
             "$HOME/data/$MONITOR_SESSION_ID" \
             --current "$METRICS_DIR/replay-current.json" 2>&1; then
             # Update throttle only on explicit success (exit 0).
             echo "$NOW_TS" > "$REPLAY_THROTTLE"
           else
             REPLAY_FAILED=true
           fi
           rm -f "$METRICS_DIR/replay-current.json"
         fi
       else
         # Empty REPLAY_JSON without prior failure flag → treat as failure
         REPLAY_FAILED=true
       fi
     fi

     # Determine replay_state watch item (precedence: failed > archive-too-small > stale > never-run)
     if [[ "$REPLAY_FAILED" == true ]]; then
       WATCH_ITEMS+=("replay_state=failed")
     elif [[ "$RUN_REPLAY" == true ]] && [[ "${EVAL_TICKS:-0}" -lt 100 ]]; then
       WATCH_ITEMS+=("replay_state=archive-too-small")
     elif [[ ! -f "$REPLAY_THROTTLE" ]]; then
       WATCH_ITEMS+=("replay_state=never-run")
     elif [[ -f "$REPLAY_THROTTLE" ]]; then
       LAST_SUCCESS=$(cat "$REPLAY_THROTTLE" 2>/dev/null || echo "0")
       STALE_AGE=$((NOW_TS - LAST_SUCCESS))
       if [[ $STALE_AGE -gt 1209600 ]]; then
         WATCH_ITEMS+=("replay_state=stale")
       fi
     fi
   fi
   ```

### Alarm evaluation via TOML catalog + Python evaluator

All alarm definitions (thresholds, extraction forms, gating, persistence guards,
filing metadata) are in [`shared/metric-alarms.toml`](../shared/metric-alarms.toml).
The evaluator script handles extraction, delta/ratio/p99/streak computation, and
emits structured JSON. Warmup exemptions, counter-reset handling, and
`PREV_PROM_INVALID` semantics are encoded in the evaluator — see the TOML
catalog for per-alarm gates and the evaluator source for edge-case behavior.

**Run the evaluator:**

```bash
# Set env vars from checks 3/10 state:
export PREV_PROM_INVALID=...     # true/false from scrape_identity check
export PREV_SCRAPE_AGE_SECONDS=... # prev.prom/snapshot age in seconds (gap-stale, #3246); -1/unset = unknown
export WARMUP_TICKS_REMAINING=... # 0/1/2 from restart detection
export FRESH_START=...            # yes/no
export CRASH_RECOVERY=...        # yes/no
export UPTIME_SECONDS=...        # from process uptime
export MONITOR_MODE=...          # validator/watcher
export PID=...                   # process PID
export START_TICKS=...           # field 22 from /proc/$PID/stat

eval_result=$(python3 scripts/lib/eval-alarms.py \
    --catalog .Codex/skills/shared/metric-alarms.toml \
    --current "$HOME/data/$MONITOR_SESSION_ID/metrics/current.prom" \
    --prev "$HOME/data/$MONITOR_SESSION_ID/metrics/prev.prom" \
    --state-dir "$HOME/data/$MONITOR_SESSION_ID/metrics")
```

> **Caution (#3209):** This canonical per-tick run is invoked **exactly once** and
> advances the streak/ratio snapshots. Any ad-hoc or repeat/diagnostic eval
> invocation within the same tick MUST pass `--no-snapshot-write` — otherwise the
> first run consumes the streak/ratio delta and the second run masks the alarm
> (reports `delta=0`). Also: never pipe the evaluator through `2>&1` into a JSON
> parser — telemetry is emitted on stderr, and merging it into stdout corrupts the
> JSON parse. Capture stderr separately (`2>telemetry.log` or `2>/dev/null`).

**Process the JSON output:**

1. Parse `eval_result` as JSON.
2. Read `aggregate.metrics_line`, `aggregate.metrics_ratio_line`, and
   `aggregate.recovery_stalled_line` for the status report.
3. For each alarm with `state == "firing"`:
   - Read `cooldown_key`, `cooldown_seconds`, `filing_title`, `filing_search`,
     `notes`, and `severity` from the alarm entry.
   - Apply the cooldown + filing workflow below (§Firing alerts).
   - For alarms with `notes`, apply the investigation guidance before filing
     (e.g., "Sample /metrics 5x at 2s intervals to verify before filing").
   - **Banner contribution by severity (#3653):** a firing **WARN/SYNC** alarm
     flips the MONITOR banner to WARNING (or escalates per the Output rules); a
     firing **NONC** alarm does **NOT** flip the banner — it is still reported on
     the `metrics:` line and filed/commented as `Non-critical:`, but the banner
     stays OK on its account. This keeps a known-benign chronic NONC condition
     (e.g. `inbound-auth-low`) from pinning the banner to WARNING every tick
     while a genuine WARN escalation (e.g. `inbound-auth-critical`) still surfaces.
4. Check stderr (evaluator telemetry) for `series_matched=0` lines — these
   indicate dead alarms that need investigation.

### Check 12b: Recovery-stalled streak (counter-based, independent of ratio checks)

> **Canonical constants:** Threshold values, snapshot path, and applicability
> are defined in [`shared/metric-alarms.toml`](../shared/metric-alarms.toml).
> This section is authoritative for the state machine *logic*; inline literals
> are cross-validated against the TOML by `scripts/test-monitor-skill-snippets.sh`.

This check tracks `henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"}`
using a streak-gated alert, independent of Check 12's ratio checks. It runs on
its own state machine because ratio checks are globally skipped during unsync
states (ledger age > 30s, gap > 5, etc.), but the recovery-stalled counter fires
precisely during recovery transitions when the node is briefly unsynced.

**Data source:** Reuses the same `/metrics` scrape result (`$metrics_body` /
`metrics/current.prom`) already fetched by check-12. Does NOT perform a
second `/metrics` fetch.

**Applicability:** Validator mode only. In watcher mode, skip Check 12b entirely
and omit the `recovery_stalled:` line from the status report.

**Snapshot file:** `/home/tomer/data/$MONITOR_SESSION_ID/metrics/counter_streak_snapshot`

Format:
```
version=1
pid=<PID>
start_ticks=<field 22 from /proc/$PID/stat>
counter_value=<value>
breach_streak=<N>
```

**PID/start_ticks check (always, even on skip):** Before evaluating skip
conditions, check PID/start_ticks against the stored snapshot. If the process
restarted (PID or start_ticks changed), invalidate the snapshot immediately.
This catches restarts during skip ticks and prevents comparing post-restart
counters to a pre-restart baseline.

**Skip conditions (skip Check 12b only when any is true):**
- `/metrics` fetch failed this tick (same condition check-12 detects)
- `/metrics` returns "recorder not installed"
- `forcing_catchup_behind` label missing from the scrape

On skip: write snapshot preserving existing `counter_value` value (or
0 if no prior snapshot) with `breach_streak=0`. Next healthy
tick compares against preserved value — does NOT enter "collecting baseline" after
a skip. Report `recovery_stalled: skipped (<reason>)`.

**Invalidation (reset streak AND enter "collecting baseline"):**
- PID or `start_ticks` changed (process restart) — checked before skip conditions
- Snapshot malformed, missing fields, or `version` ≠ `1`
- Current `counter_value` value < previous (counter reset)
- First tick after fresh start (no prior snapshot exists)

On invalidation: write new snapshot with current counter value and
`breach_streak=0`. Do NOT evaluate burst or streak (delta)
logic on invalidation ticks — this prevents false-firing the burst override on
the first tick after a restart where the counter jumps from 0 to the current
absolute value.

**Post-restart absolute-value fire (#3198):** After writing the fresh baseline,
evaluate the *absolute* `counter_value` value that the reset would
otherwise discard. If it meets `post_restart_absolute_threshold` (= `50`, from
`metric-alarms.toml`), fire WARN once on this invalidation tick instead of
reporting `collecting baseline`, and route through the Bug Filing Workflow.
This closes the blind spot from #3197/#3198: a self-recovering
`forcing_catchup_behind` stall that completes before the 15m clean-restart sync
deadline (so check (2) never reports SYNC FAILURE) and accrues its entire burst
across the single tick interval that straddles the baseline-reset tick (so the
cross-tick streak/burst machine never observes an incrementing delta). The
#3197 episode accrued 213 ticks; a clean restart reaching real-time sync
promptly accrues only a handful (well under 50). The fresh baseline + 7200s
cooldown prevent re-fire on subsequent ticks. The fire uses the absolute value,
not a cross-tick delta — it is robust *across* the reset because it triggers
*on* the reset. Both invalidation branches (PID/`start_ticks` change AND counter
reset) are covered, so the absolute baseline is never silently discarded.
Otherwise (absolute value below threshold, or threshold disabled): report
`recovery_stalled: collecting baseline` as before.

**Per-tick logic (not skipped AND not invalidated):**
```
delta = current(counter_value) - prev(counter_value)

if delta >= 10:
    # Immediate-fire override: large burst indicates sustained stalling.
    # Do NOT reset streak — keep incrementing; cooldown (7200s) handles dedup.
    breach_streak += 1
    → fire WARN, route through Bug Filing Workflow
elif delta >= 1:
    breach_streak += 1
    if breach_streak >= 3:
        → fire WARN, route through Bug Filing Workflow
else:  # delta == 0
    breach_streak = 0
```

Note: after skipped ticks where the counter value was preserved, the first
healthy tick may see a large accumulated delta spanning multiple monitor
intervals. This is acceptable — the burst threshold (≥10) catches sustained
trouble regardless of tick granularity.

**Post-restart warmup:** First tick writes baseline ("collecting baseline").
Second tick has a valid prev for delta comparison and begins normal evaluation.

**Alert identity and cooldown:**
- Cooldown key: `henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"}` (full selector-qualified)
- Cooldown period: 7200s (2h)
- Issue search: `gh issue list --search 'metrics: henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"}' --state open`
- Issue title on filing: `metrics: henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"} — sustained breach`
- Filing follows the standard Bug Filing Workflow (§Firing alerts — cooldown + filing)

**Integration with metrics aggregate:** Check 12b alerts are NOT counted in the
`metrics:` line (which tracks immediate-fire counter/gauge alerts). They appear
on their own `recovery_stalled:` line. A fired Check 12b alert does contribute
to overall tick severity — the tick is considered unhealthy when any alert fires.

**Status line:** `recovery_stalled:` (reported after `metrics_ratio:`):
- `recovery_stalled: ok (delta=0)` — no increment, streak reset
- `recovery_stalled: breach (delta=N, streak M/3)` — incrementing, below threshold
- `recovery_stalled: WARNING delta=N (M ticks) — investigating` — streak ≥ 3
- `recovery_stalled: WARNING delta=N (burst) — investigating` — immediate fire (delta ≥ 10)
- `recovery_stalled: WARNING absolute=N (post-restart) — investigating` — post-restart absolute fire (absolute ≥ 50 on a baseline-reset tick, #3198)
- `recovery_stalled: skipped (<reason>)` — metric missing or fetch failed
- `recovery_stalled: collecting baseline` — first tick after restart/invalidation (absolute value below the post-restart threshold)

**Rendering precedence** (determines the `metrics_ratio:` line format):

1. **Global skip** (all checks skipped for the same reason — e.g., not in sync,
   fetch failed, recorder not installed, missing counters, label mismatch):
   Use the collapsed form: `metrics_ratio: skipped (<reason>)`
2. **Collecting baseline** (no previous snapshot exists — first steady-state tick):
   `metrics_ratio: collecting baseline`
3. **Per-check reporting** (at least one check ran — whether ok, warning, or
   individually skipped for low volume or missing per-check inputs):
   Compose: `metrics_ratio: scp <scp_status>, apply <apply_status>, pending <pending_status>`

Examples:
- All healthy: `metrics_ratio: scp ok (accept=15%), apply ok (fail=8%), pending ok (too_old=3%)`
- One warning: `metrics_ratio: scp ok (accept=12%), apply WARNING fail=55%>50% (3 ticks) — investigating, pending ok (too_old=5%)`
- One skipped (low volume): `metrics_ratio: scp skipped (low volume), apply ok (fail=5%), pending ok (too_old=2%)`
- Pending skipped (missing counters): `metrics_ratio: scp ok (accept=15%), apply ok (fail=8%), pending skipped (missing counters)`
- Global skip: `metrics_ratio: skipped (not in sync)`
- Collecting: `metrics_ratio: collecting baseline`

`recovery_stalled:` examples (after `metrics_ratio:` in output):
- Healthy: `recovery_stalled: ok (delta=0)`
- Building streak: `recovery_stalled: breach (delta=2, streak 1/3)`
- Firing (streak): `recovery_stalled: WARNING delta=1 (3 ticks) — investigating`
- Firing (burst): `recovery_stalled: WARNING delta=15 (burst) — investigating`
- Firing (post-restart absolute, #3198): `recovery_stalled: WARNING absolute=213 (post-restart) — investigating`
- Skipped: `recovery_stalled: skipped (metric missing)`
- Baseline: `recovery_stalled: collecting baseline`

### Firing alerts — cooldown + filing

For each firing alert:

1. Read `/home/tomer/data/$MONITOR_SESSION_ID/metrics/anomaly_cooldown.json`
   (create empty `{}` if missing).
2. If `now - last_filed[<metric>] < 7200s` (2h), include the alert in the
   status report but SKIP file/comment.
3. Otherwise follow the BUG FILING WORKFLOW:
   - Search `gh issue list --search "metrics: <metric-name>" --state open`.
   - If one matches, `gh issue comment <N>` with the new evidence (current/prev
     values, delta, threshold, ledger, binary sha, sibling metrics). Apply the
     `urgent` label only if the metric breach blocks validator operation (per
     the Label policy in the Bug filing workflow); otherwise leave unlabeled.
     **Board-route:** if NOT on project board, add to Backlog.
   - If no match, `gh issue create` (append `--label urgent` only when the
     metric breach is operation-blocking; most metric alerts are non-urgent
     and should be filed without a label) with:
     - Title: `Non-critical: metrics: <metric>` (NONC tier) or `metrics: <metric> — <symptom>` (WARN/SYNC tier).
     - Body: current/prev values, delta, threshold, ledger, binary sha, related
       sibling metrics, file:line citation from `grep -n "<metric_name_without_prefix>" crates/ -r`,
       and a suggested fix.
     **Board-route:** `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
4. Update `anomaly_cooldown.json` with `{"<metric>": <now>}`.
5. For SYNC-tier alerts, ALSO update the `sync:` line in the status report
   (not just `metrics:`).

### Watcher mode

If `$MONITOR_MODE = watcher`, run check (12) with a reduced catalog: only
process (open_fds, max_fds), jemalloc, and overlay counters/gauges. Skip
SCP, quorum, herder_state, histogram p99 alerts, ratio checks, and
Check 12b (recovery-stalled streak). Omit the `recovery_stalled:` line
from watcher output entirely.

## Remote sync & redeploy

**(10) Remote sync** — first sanity-check the working tree:

- If `git status --porcelain` reports any output, ABORT the deploy path for
  this tick. Report: `DEPLOY SKIPPED (dirty tree)` with the list of dirty
  paths. Do not run `git pull` against a dirty tree; do not kill the node.
  Investigate the dirty tree before the next tick.
- If clean, `git fetch origin main`. If in detached HEAD state
  (`git symbolic-ref HEAD` fails), `git checkout main` first.

**Determine `deployed_sha`** — the sha of the source tree used to build the
currently-running binary. This decouples the deploy gate from git HEAD, which
can diverge from the running binary if an operator or CI pushes commits
without going through the skill's build+restart path.

```bash
BUILD_SHA_FILE="/home/tomer/data/$MONITOR_SESSION_ID/build_sha"
SESSION_DIR="/home/tomer/data/$MONITOR_SESSION_ID"

deployed_sha=""
deployed_sha_status=""

if [ -s "$BUILD_SHA_FILE" ]; then
  candidate=$(tr -d '[:space:]' < "$BUILD_SHA_FILE")
  if printf '%s' "$candidate" | grep -qE '^[0-9a-f]{40}$' \
     && git cat-file -e "${candidate}^{commit}" 2>/dev/null; then
    deployed_sha="$candidate"
    deployed_sha_status="ok"
  else
    deployed_sha_status="invalid"
  fi
elif [ -e "$SESSION_DIR/last_ledger" ] \
  || [ -e "$SESSION_DIR/metrics/current.prom" ] \
  || [ -e "$SESSION_DIR/tick-history.jsonl" ]; then
  # Other per-session state exists — monitor-loop should have written
  # build_sha but didn't (e.g. rollout of this change to an existing
  # session). Force a rebuild to restore the invariant.
  deployed_sha_status="invalid"
else
  # Truly fresh session dir — safe to fall back to today's behavior.
  deployed_sha=$(git rev-parse HEAD)
  deployed_sha_status="missing-fresh"
fi
```

**Runtime cross-validation** — use the running binary's embedded commit hash
as the authoritative source of truth. If `/info` reports a valid, locally-
reachable commit hash, it overrides `BUILD_SHA_FILE` (which becomes a cache).

```bash
# Cross-validate: running binary's commit_hash is authoritative.
running_hash=""
if info_json=$(curl -sf "http://localhost:$MONITOR_ADMIN_PORT/info" 2>/dev/null); then
  running_hash=$(printf '%s' "$info_json" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin).get("commit_hash",""))' 2>/dev/null)
fi

if [ -n "$running_hash" ] \
   && printf '%s' "$running_hash" | grep -qE '^[0-9a-f]{40}$' \
   && git cat-file -e "${running_hash}^{commit}" 2>/dev/null; then
  # Binary reports a valid, locally-reachable commit hash — authoritative.
  case "$deployed_sha_status" in
    ok)
      if [ "$deployed_sha" != "$running_hash" ]; then
        # BUILD_SHA_FILE disagrees with binary. Trust binary, repair file.
        printf '%s\n' "$running_hash" > "${BUILD_SHA_FILE}.tmp"
        mv "${BUILD_SHA_FILE}.tmp" "$BUILD_SHA_FILE"
        deployed_sha="$running_hash"
        # deployed_sha_status stays "ok"
      fi
      ;;
    invalid|missing-fresh)
      # File bad/missing but binary knows its commit — repair and use.
      printf '%s\n' "$running_hash" > "${BUILD_SHA_FILE}.tmp"
      mv "${BUILD_SHA_FILE}.tmp" "$BUILD_SHA_FILE"
      deployed_sha="$running_hash"
      deployed_sha_status="ok"
      ;;
  esac
fi
# If /info unreachable, commit_hash absent/empty/malformed, or SHA not in
# local git: no-op — fall through to existing logic unchanged.
```

| `/info` state | `commit_hash` field | `git cat-file -e` | Action |
|---|---|---|---|
| Unreachable | — | — | No-op |
| Reachable | Missing/empty/malformed | — | No-op |
| Reachable | Valid 40-char hex | Fails (not in local git) | No-op (prevents cache poisoning) |
| Reachable | Valid 40-char hex | Succeeds, matches `deployed_sha` | No-op |
| Reachable | Valid 40-char hex | Succeeds, differs from `deployed_sha` | Repair file, use runtime hash |

| State | Meaning | Gate behavior |
|-------|---------|---------------|
| `ok` | Valid commit known (from file or repaired from runtime) | Compare `deployed_sha` vs `origin/main` |
| `missing-fresh` | No file, no runtime hash, no other session state | Falls back to `HEAD` vs `origin/main` |
| `invalid` | File corrupt + runtime unavailable | Force rebuild unconditionally |

Report `deployed_sha_status` in the tick's status line.

**Gate comparison:**

```bash
origin_sha=$(git rev-parse origin/main)

case "$deployed_sha_status" in
  ok|missing-fresh)
    [ "$deployed_sha" = "$origin_sha" ] && gate_action="up-to-date" || gate_action="proceed"
    ;;
  invalid)
    gate_action="proceed-force-rebuild"
    ;;
esac
```

If `gate_action="up-to-date"`: no action (already up to date).

Otherwise enter the deploy path:

1. **Cool-down guard**: if node uptime is `< 60m` AND `CRASH_RECOVERY=no` (i.e.
   the previous tick deployed cleanly), SKIP DEPLOY this tick. Report:
   `DEPLOY DEFERRED (cool-down: uptime=<X>m < 60m)`. If `deployed_sha_status`
   is `invalid`, append `, build_sha: invalid` to the report so the operator
   knows a follow-up rebuild is pending once cool-down expires. Rationale:
   every restart costs ~1-2m of validation downtime + brief peer reconnect.
   Deploying multiple commits within minutes thrashes the validator without
   giving each version time to surface issues. The 60m floor was raised from
   the original 30m (#1944) on 2026-04-30 after observing too many quick-cycle
   deploys per day. Crash-recovery restarts are exempt because they're not
   voluntary deploys. Urgent fixes can override by killing the node manually
   before the next tick — the normal startup path will pick up the latest
   origin/main.
2. **Binary-relevance check** (the path/hunk heuristic only marks a CANDIDATE;
   the authoritative skip is gated by the binary byte-compare in step 2a):
   ```bash
   source "$(git rev-parse --show-toplevel)/scripts/lib/monitor-decisions.sh"

   if [ "$gate_action" = "proceed-force-rebuild" ]; then
     # invalid build_sha — skip heuristic, force rebuild
     needs_rebuild="yes"
   elif changed_paths=$(git diff --name-only "$deployed_sha" origin/main 2>/dev/null); then
     # ok or missing-fresh: classify every changed path. needs_rebuild stays
     # "no" only if EVERY path is provably non-binary-affecting; ANY ambiguity
     # flips to "yes" (fail-safe). skip_kind tracks the strongest skip reason
     # for the report ("no-binary-impact" vs "test-only").
     needs_rebuild="no"
     skip_kind="no-binary-impact"
     while IFS= read -r p; do
       [ -z "$p" ] && continue
       case "$(classify_path_binary_relevance "$p")" in
         no-impact) ;;                          # allowlisted — never compiled in
         test-only) skip_kind="test-only" ;;    # crates/*/tests/** integration test
         needs-hunk-check)
           # crates/**/src/**.rs: test-only ONLY if every changed line in THIS
           # file's diff is inside a #[cfg(test)]/mod tests region. Any ambiguity
           # (zero-context hunk, module deletion, boundary edit) → rebuild.
           if git diff "$deployed_sha" origin/main -- "$p" | diff_is_test_only; then
             skip_kind="test-only"
           else
             needs_rebuild="yes"; break
           fi
           ;;
         *) needs_rebuild="yes"; break ;;       # rebuild (fail-safe default)
       esac
     done <<< "$changed_paths"
   else
     # git diff failed — fail closed, force rebuild.
     needs_rebuild="yes"
   fi
   ```

   `classify_path_binary_relevance PATH` returns one of `no-impact` |
   `test-only` | `needs-hunk-check` | `rebuild`:
   - **`no-impact`** (allowlisted — never compiles into the release binary):
     `.github/`, `.Codex/`, `scripts/`, `docs/`, `metrics/`, root-level `*.md`
     (e.g. `README.md`, `AGENTS.md`), root-level git metadata dotfiles
     (`.gitignore`, `.gitattributes`, `.gitmodules`), `stellar-specs` /
     `stellar-specs/` submodule pointer changes, and container-image files
     (`Dockerfile`, `.dockerignore`, `*.dockerfile`) — `cargo` never reads them,
     so they cannot change the compiled binary; safe ONLY because the monitor
     runs the locally-compiled `release/henyey`, not the Docker image (the
     Dockerfile builds the separate SSC integration image). `docker-compose*.yml`
     is deliberately NOT allowlisted — it stays on the `rebuild` fail-safe.
   - **`test-only`**: `crates/<crate>/tests/**` — integration tests, a separate
     compilation target never linked into the `--release` lib/bin.
   - **`needs-hunk-check`**: `crates/**/src/**.rs` — defer to `diff_is_test_only`
     on that file's diff (returns 0 only if every changed line is strictly inside
     a `#[cfg(test)]`/`mod tests` region; ANY ambiguity → rebuild).
   - **`rebuild`** (FAIL-SAFE default): everything else — `Cargo.toml`,
     `Cargo.lock`, any `build.rs`, `configs/`, `.rs` outside `src/`/`tests/`,
     non-`.rs` files under `crates/`, or any unrecognized path. A false
     "rebuild" costs only a wasted build; a false "skip" could miss a deploy.

2a. **Authoritative binary byte-compare** (only when `needs_rebuild="no"`): the
    heuristic has marked the changeset a skip CANDIDATE, but shell-based Rust
    region parsing is brittle, so it does NOT by itself authorize a skip. Build
    `henyey` at BOTH `deployed_sha` and `origin/main` into the SAME scratch
    target dir with identical flags, then byte-compare the two produced
    binaries. Skip the rebuild+restart ONLY if the bytes are identical;
    otherwise fall through to the normal rebuild+restart path (step 3+).
    Build BOTH sides freshly in the same path — do NOT compare against the
    long-running on-disk binary, which may have been built under a different
    absolute path and so would never match (the carve-out would be inert).
    ```bash
    if [ "$needs_rebuild" = "no" ]; then
      CMP_TARGET="/home/tomer/data/$MONITOR_SESSION_ID/binrel-cmp-target"
      BIN_OLD="/home/tomer/data/$MONITOR_SESSION_ID/binrel-old"
      BIN_NEW="/home/tomer/data/$MONITOR_SESSION_ID/binrel-new"
      rm -rf "$CMP_TARGET"; mkdir -p "$CMP_TARGET"
      # Build at deployed_sha, then at origin/main, into the SAME target dir.
      git stash --include-untracked >/dev/null 2>&1 || true
      git checkout --quiet "$deployed_sha" \
        && CARGO_TARGET_DIR="$CMP_TARGET" cargo build --release -p henyey \
        && cp "$CMP_TARGET/release/henyey" "$BIN_OLD"
      git checkout --quiet origin/main \
        && CARGO_TARGET_DIR="$CMP_TARGET" cargo build --release -p henyey \
        && cp "$CMP_TARGET/release/henyey" "$BIN_NEW"
      if [ -f "$BIN_OLD" ] && [ -f "$BIN_NEW" ] && cmp -s "$BIN_OLD" "$BIN_NEW"; then
        binary_identical="yes"
      else
        binary_identical="no"   # heuristic miss OR non-reproducible build → rebuild
      fi
      rm -rf "$CMP_TARGET" "$BIN_OLD" "$BIN_NEW"
    fi
    ```
    On a **confirmed skip** (`needs_rebuild="no"` AND `binary_identical="yes"`):
    run `git pull --rebase` only, then advance `BUILD_SHA_FILE` to the new sha:
    ```bash
    new_sha=$(git rev-parse origin/main)
    printf '%s\n' "$new_sha" > "${BUILD_SHA_FILE}.tmp"
    mv "${BUILD_SHA_FILE}.tmp" "$BUILD_SHA_FILE"
    ```
    The bytes are confirmed byte-identical, so the running binary IS the
    origin/main binary — advancing `BUILD_SHA_FILE` is correct and fixes the
    per-tick re-trip (the gate no longer re-evaluates this same changeset every
    tick). Report:
    - `DEPLOY SYNCED (test-only: binary byte-identical — pulled <N> commits, no restart)` when `skip_kind="test-only"`;
    - `DEPLOY SYNCED (no-binary-impact: allowlisted paths only — pulled <N> commits, no restart)` when `skip_kind="no-binary-impact"`.

    If `binary_identical="no"` (a heuristic false-positive OR a
    non-reproducible build): set `needs_rebuild="yes"` and fall through to the
    normal build+restart path. A heuristic miss therefore costs at most one
    wasted build, NEVER a missed deploy.

    > NOTE on reproducibility: this depends on `cargo build --release` being
    > bit-identical across two same-path builds. If that does not hold in
    > practice, `binary_identical` will be `no` and the gate simply always
    > rebuilds — strictly no worse than the legacy "always rebuild on `crates/`"
    > behavior, just with a wasted build on test-only changes.
3. **Quarantine gate** (only reached when `needs_rebuild=yes`):
   ```bash
   # --- Quarantine gate ---
   source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
   # Auto-stamp resolved:<fix-sha> on any bundled-commit quarantine whose
   # recorded crash issue (`regression #N`) has a MERGED closing PR (#3258).
   # WRITE-ONLY + best-effort: it only ADDS a `resolved:` token (never clears);
   # any gh/network failure or open PR leaves the entry un-stamped. The clear
   # decision stays SOLELY with check_quarantine_active below, which re-gates
   # every stamp with `git merge-base --is-ancestor`. So a wrong/premature
   # stamp is still BLOCKED, and `|| true` keeps a gh hiccup from aborting.
   quarantine_autostamp "$HOME/data/deploy_quarantine.txt" || true
   check_quarantine_active "$HOME/data/deploy_quarantine.txt"
   if [ $? -eq 0 ]; then
     case "$QUARANTINE_STATUS" in
       blocked_unreadable)
         deploy_report="BLOCKED (quarantine file unreadable — fail-closed)" ;;
       blocked_git_error)
         deploy_report="BLOCKED (quarantine ancestry check failed for ${QUARANTINED_MATCH:0:8} — fail-closed)" ;;
       blocked_active)
         deploy_report="DEFERRED (quarantined: ${QUARANTINED_MATCH:0:8}'s diff still applied to origin/main — see ~/data/deploy_quarantine.txt)" ;;
     esac
     [ -n "$QUARANTINE_WARNINGS" ] && deploy_report="$deploy_report [WARN: $QUARANTINE_WARNINGS]"
     # Skip to status report with this deploy_report. Do not continue.
   fi
   ```
   If quarantined, report the appropriate status and exit the deploy path.
   Do NOT proceed to CI check, build, or restart. Parse/check warnings are
   appended to the `deploy:` status line regardless of outcome.

   **Content-aware semantics** (changed from the legacy ancestor-only check):
   `check_quarantine_active` blocks only when a quarantined SHA's *diff is
   still applied* to `origin/main`'s HEAD. The check uses
   `git diff <sha>^..<sha> | git apply --check --reverse`: if the diff
   reverse-applies cleanly the offending lines are still in the tree (BLOCK);
   if it fails (rejected hunks, missing files, context drift from a revert
   or refactor) the bad code has been removed (CLEAR for that entry).
   The legacy `check_quarantine_ancestry` name still works (it now aliases
   to `check_quarantine_active`), and the on-disk file format is unchanged.

   Consequence: a normal `git revert` (which adds a revert commit but leaves
   the quarantined SHA in ancestry) auto-clears the gate on the next tick.
   Operator no longer needs to manually `quarantine_remove` reverted SHAs.
4. Check CI status on origin/main: `gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'`.
   If any recent run has conclusion `failure`, do NOT deploy — route the failure
   through check (11) and wait.
5. If all conclusions are `success` (ignore `""` for in-progress and `cancelled`):
   `git pull --rebase`, `CARGO_TARGET_DIR=/home/tomer/data/$MONITOR_SESSION_ID/cargo-target cargo build --release -p henyey`.
6. If build succeeds:
   ```bash
   # Persist the just-built sha BEFORE Stop-PID. Atomic via tmp + mv.
   new_sha=$(git rev-parse HEAD)
   printf '%s\n' "$new_sha" > "${BUILD_SHA_FILE}.tmp"
   mv "${BUILD_SHA_FILE}.tmp" "$BUILD_SHA_FILE"
   ```
   Then: Stop-PID, Rotate-log suffix `preredeploy`, Relaunch.
   Report: `DEPLOY — pulled <N> commits (<old-sha>..<new-sha>), rebuilt, restarted at L<ledger>`.
7. If build fails: report `BUILD FAILED`, do NOT restart — the old binary is
   still running. Do NOT update `BUILD_SHA_FILE`. Route the build error through
   check (11).

## CI check workflow

**(11) CI check** — scope and levels of detection.

**(11a) Scope**: only inspect workflows that run on branch main.
`gh run list --branch main --limit 10 --json databaseId,name,status,conclusion,headSha,createdAt --jq '.[] | "\(.name)|\(.status)|\(.conclusion)|\(.headSha[:8])|\(.databaseId)|\(.createdAt)"'`.
Ignore runs triggered by PRs on other branches. Scan for completed runs with
conclusion `failure`.

**(11b) Job-level** (CRITICAL — catches continue-on-error failures): For the
latest completed run of EACH distinct workflow name (enumerate dynamically
from 11a, do NOT hard-code names), check individual jobs:
`gh run view <ID> --json jobs --jq '.jobs[] | select(.conclusion == "failure") | "\(.name)|\(.conclusion)"'`.
Workflows with continue-on-error jobs report run-level conclusion `success`
even when jobs fail — you MUST check job-level conclusions. If any jobs have
conclusion `failure`, treat it the same as a run-level failure.

**REPORTING RULE** — NEVER report `ci: all green` if ANY job has conclusion
`failure`, even if the run-level conclusion is `success`. The `ci:` line in
the status report MUST reflect the WORST job-level result across all
workflows. A continue-on-error job failure is NOT green — it is RED. Do not
qualify failures as "known", "pre-existing", or "cosmetic".

Only act on failures from the last 2 hours (compare `createdAt` with
`date -u +%Y-%m-%dT%H:%M:%SZ`). For each failure:

1. `gh run view <ID> --log-failed 2>&1 | tail -80`.
2. Categorize: build error, test failure, timeout, flaky, infrastructure.
3. **If the failure looks like CI automation rather than a real defect**
   — runner timeout outside test logic, network error fetching deps,
   GHA service hiccup, OOM-on-runner, "operation timed out" with no test
   stack, known-flaky test patterns (`test_core3_restart_rejoin_*` is a
   recurrer; see #1838 / #1939) — **rerun the failing jobs first**:
   ```bash
   attempts=$(gh run view <ID> --json attempt --jq .attempt)
   if [ "$attempts" -lt 2 ]; then
     gh run rerun <ID> --failed
   fi
   ```
   Report `CI RERUN ATTEMPTED — <workflow> jobs <names> on <sha>, attempt 1→2`
   and stop CI processing for this tick. The next tick will see the
   re-run conclusion. Don't rerun more than once — if the re-run also
   fails, the failure is real (or an unusually persistent flake) and
   warrants filing.
   If the failure is clearly NOT automation (build error from real code,
   test assertion mismatch, hash mismatch, panic from production code),
   skip the rerun and proceed to step 4.
4. Check for an existing open issue:
   `gh issue list --search "<workflow name + signature>" --state open`.
   If one matches, `gh issue comment <N>` with the new evidence (sha, log
   snippet, timestamp) and ensure it has the `urgent` label
   (`gh issue edit <N> --add-label urgent`) — failing CI on origin/main
   blocks deploy and meets the urgent criteria.
   **Board-route:** if NOT on project board, add to Backlog:
   `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
5. Otherwise, file a new issue: `gh issue create --label urgent --title "<workflow>: <short signature>" --body "..."` with investigation findings.
   **Board-route:** `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
6. Do NOT commit a fix. Report: `CI ISSUE FILED — <workflow> failed on <sha>, filed/commented #<N>`.

## Bug filing workflow

Applies to node bugs, metric alerts, and CI failures.

### Body delivery — use `--body-file`, not heredocs

When the issue body contains code blocks, **always write the body to a temp
file and pass `--body-file <path>`** rather than `--body "$(cat <<'EOF' ... EOF)"`.

Reason: heredocs nested inside double-quoted shell expressions tempt the agent
to backslash-escape backticks (`` \` ``), which GitHub renders literally — every
code fence breaks. There is no GitHub-side workaround once filed; the issue
must be re-edited via `gh issue edit <N> --body-file <path>`.

Pattern:

```bash
cat > /tmp/issue-body.md <<'EOF'
## Symptom
...
```rust
some code
```
...
EOF
gh issue create --title "..." --body-file /tmp/issue-body.md
# or for amendments:
gh issue edit <N> --body-file /tmp/issue-body.md
```

The single-quoted `'EOF'` makes the heredoc literal — no escaping needed for
backticks, dollar signs, etc. inside the body.

### Label policy

> **Canonical reference:** `scripts/lib/monitor-label-policy.md` is the
> single source of truth for labeling and board-routing rules. This section
> is a normative excerpt.

When creating or commenting on issues:

- **`urgent`** — file with `--label urgent`, OR `gh issue edit <N> --add-label urgent`
  on an existing issue, ONLY when the symptom blocks validator operation or
  consensus participation. Specifically:
  - hash mismatch (any kind)
  - wedged node (frozen event loop, watchdog auto-abort)
  - failing CI on origin/main blocking deploy
  - panic or crash from production code
  - SYNC FAILURE past the active deadline
  - OOM-driven restart
  - and similar runtime-blocking conditions
- **(no label)** — non-urgent issues: calibration, threshold tuning, NONC
  alerts, cosmetic noise, follow-up improvements. Downstream picks these up
  at lower priority.
- **`not-ready`** — reserved for tier-3 self-reflection issues that need
  operator decision before any code change. Do not use for regular bug
  filings.

**Label lifecycle:**
- `urgent` and `not-ready` are mutually exclusive in steady state.
- Monitors MAY add `urgent` to an existing unlabeled issue (escalation).
- Monitors do NOT remove labels (de-escalation is operator-only).
- If a `not-ready` issue becomes blocking: add `urgent`, comment with
  evidence. Both labels coexist temporarily until operator resolves.

### Filing flow

1. Identify the failing signature (ledger + error type for node bugs; metric
   + threshold for alerts; workflow + job + error type for CI).
2. Investigate to root cause — read source code, trace code paths.
3. Check for an existing open issue: `gh issue list --search "<keywords>" --state open`.
   If a match exists, verify its state is OPEN
   (`gh issue view <N> --json state -q .state`), `gh issue comment <N>` with
   new evidence, and apply the `urgent` label only if the recurrence meets
   the urgent criteria above.
   **Board-route:** if the issue is NOT on the project board, add it:
   `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
   (or `Blocked` if labeled `not-ready`). If already on board, preserve
   current status. STOP.
4. If no OPEN match, file a new issue with `gh issue create` — append
   `--label urgent` only if the symptom meets the urgent criteria above;
   otherwise omit the label. Body is a self-contained proposal (symptom,
   evidence, suspected root cause, fix sketch with file:line references).
   **Board-route:** add to project board:
   `bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`
   (or `Blocked` if labeled `not-ready`).
5. Do NOT spawn agents. Do NOT edit the main checkout. The next redeploy tick
   (check 10) will pick up whatever lands on main.

**Board routing failure:** If `move-issue-status.sh` fails, log as ACTION
in the status report. Retry on next tick. Escalate after 3 consecutive
failures (see `scripts/lib/monitor-label-policy.md`).

**Recurrence policy**: If a previously-filed bug recurs with material new
evidence, prefer commenting on the existing issue when it is the same bug at
the same site AND the issue is OPEN. If the prior issue is CLOSED, file a new
issue (label per the policy above) with `Related to #<prior> (closed)` in the
body and a note on why the prior fix did not cover this case; do NOT comment
on the closed issue. File a new issue (still referencing `Related to #<prior>`
with one-line scope-diff) when new evidence points at a different named
subsystem, phase/mark, root-cause hypothesis, or candidate site set.

**Commit policy**: the monitor does NOT commit code. All fixes are delegated
via `gh issue` + project board routing.

**Deploy regression policy**: If the node fails after a deploy
(see also `scripts/lib/monitor-label-policy.md`):

(a) Record the bad SHA BEFORE any rollback rebuild (`build_sha` will be
overwritten during rebuild):

```bash
bad_sha=$(cat "$BUILD_SHA_FILE")
```

(b) Append to quarantine file (idempotent):

```bash
source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
rc=0
quarantine_append "$HOME/data/deploy_quarantine.txt" "$bad_sha" "regression #<issue>" || rc=$?
if [ $rc -ne 0 ]; then
  echo "WARNING: quarantine_append failed (rc=$rc) — deploy gate may not block next tick" >&2
fi
```

(c) File or comment on a GitHub issue (label `urgent` since validator
operation is impacted) with the regression details (commit range, symptoms,
watchdog data). Board-route to Backlog:
`bash .github/skills/shared/scripts/move-issue-status.sh "$N" backlog`

(d) Restart the node on the last known-good binary (rebuild from the
previous commit) while waiting for the fix. Do NOT revert commits inline.

The quarantine gate (section 10, step 3) blocks re-deployment as long as
the quarantined SHA's *diff is still applied* to `origin/main`. Once a
`git revert` or refactor removes the offending content from HEAD, the
gate auto-clears on the next tick. No operator action required for the
common case.

### Quarantine Clearance

The deploy quarantine auto-clears for any entry whose content is no longer
present in `origin/main` HEAD. The typical flow is:

1. Bad commit lands → monitor-tick's deploy-regression policy appends the
   SHA to `~/data/deploy_quarantine.txt` and files an `urgent` issue.
2. Operator (or a follow-up PR) reverts/refactors the bad changes on
   `origin/main`.
3. Next monitor-tick re-evaluates the gate: the bad commit's diff no
   longer reverse-applies cleanly → gate clears → rebuild + restart
   proceeds on the next eligible cycle (subject to cool-down, CI green,
   binary-relevance).

Entries stay in the file as a historical log; they're harmless once their
content is gone. The file can grow unboundedly over time if you want a
deploy audit trail; trim it manually whenever convenient.

**Bundled commits (`resolved:<fix-sha>`)** — the content-check above blocks
if **any** hunk of the quarantined SHA still reverse-applies. A commit that
**bundled** a harmful change with desirable ones (e.g. #3238 mixed a harmful
`retain:false` with the benign `_rjem_malloc_conf` export + instrumentation)
can never auto-clear after a **partial** revert: the benign hunks are
intentionally kept on `origin/main`, so the per-hunk check stays
`blocked_active` forever (the tick-199 false-block, #3256).

**Automatic stamping (#3258).** For the common case — the auto-quarantine path
records the crash issue # in the entry's reason (`regression #N`, written by
§3d) — the deploy gate now stamps `resolved:<fix-sha>` **automatically every
tick**, via `quarantine_autostamp` run immediately before
`check_quarantine_active` (section 10's quarantine gate above). It follows
GitHub's **structured** linkage — not free-text commit-message scanning: parse
`#N` from the reason → ask GitHub which **MERGED** PR closed #N
(`closedByPullRequestsReferences`) → take that PR's `mergeCommit.oid`. So once
the fix PR merges, the entry auto-clears on the next tick with **no operator
action**. `quarantine_autostamp` is **write-only and best-effort**: it only
ADDS a `resolved:` token (never clears), and any gh/network/parse failure, an
open (non-MERGED) PR, a missing issue #, or a self-resolution leaves the entry
un-stamped — so the per-hunk content-check governs that entry. The clear
decision stays **solely** with `check_quarantine_active`, which independently
re-gates every stamp with `git merge-base --is-ancestor` — a wrong/premature
stamp is still BLOCKED.

**Manual stamping (fallback).** If the entry has no recorded issue # (e.g. a
hand-added quarantine), or the fix landed without a closing PR, stamp it once
yourself with the fix commit that resolved it:

```bash
source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
# <fix-sha> is the commit that removed the harmful part (e.g. #3251).
quarantine_resolve "$HOME/data/deploy_quarantine.txt" "$bad_sha" "$fix_sha"
```

This appends a `resolved:<fix-sha>` token to the entry. The gate **auto-clears**
that entry once `<fix-sha>` is an ancestor of `origin/main` (i.e. the fix has
merged) — no further per-tick action needed. It is **fail-safe**: a not-yet-
merged fix, a malformed token, a self-resolution (`<fix-sha>` == `<bad_sha>`),
or a git error all fall back to the per-hunk content-check (BLOCK).
`quarantine_remove` remains the override.

**Manual removal** (still supported, occasionally useful — e.g. an entry
that the content check can't decide cleanly because of a malformed diff
or a binary file):

```bash
source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
rc=0
quarantine_remove "$HOME/data/deploy_quarantine.txt" "$bad_sha" || rc=$?
if [ $rc -ne 0 ]; then
  echo "ERROR: quarantine_remove failed (rc=$rc) — entry may still be active" >&2
fi
```

**When the gate blocks unexpectedly** (e.g., a fix is partial or the
content-check has a false-positive on edge-case diffs), the `deploy:`
status line reports `DEFERRED (quarantined: <sha>'s diff still applied
to origin/main — ...)` every tick. The fastest way to investigate is:

```bash
# Show what the gate sees as "still applied"
cd ~/henyey-1
git diff <sha>^..<sha> | git apply --check --reverse 2>&1 | head -30
```

If the check is firing on a genuine remaining-application of the bad
code, complete the revert. If it's firing on an unrelated file (binary
or moved/renamed), use `quarantine_remove` to force-clear that entry.

**Emergency override**: To force deploy despite quarantine:

```bash
rm "$HOME/data/deploy_quarantine.txt"   # clears ALL quarantines
# or: remove a specific entry using the helper:
# source "$(git rev-parse --show-toplevel)/scripts/lib/deploy-quarantine.sh"
# quarantine_remove "$HOME/data/deploy_quarantine.txt" "<sha>"
```

## Investigation

For ANY anomaly, investigate to root cause — read source code, check logs,
trace code paths. Never dismiss as "expected". Produce a GitHub issue
(label per the Bug filing workflow's Label policy — `urgent` if it blocks
operation, otherwise no label) or comment on an existing one for every anomaly that isn't
immediately explained. **Only exception**: anomalies whose root cause turns
out to be literal expected-correct behavior per the code — document the code
path in the status report and skip the filing.

## Output

Print a multiline status report:

```
MONITOR <OK|WARNING|ACTION|OFFLINE> — L<ledger> — <timestamp>
  node:    mode=<MODE> session=<session-id> pid=<PID> fresh_start=<yes|no>
  wipe:    <per wipe-state composition table — omitted when no wipe>
  sync:    <synced | CATCHING UP (gap=N, uptime=Xm, deadline=<15m|20m|60m>[, startup_peak=<N|absent>, apply_chain=<succ>/<fail>]) | SYNC FAILURE (gap=N, uptime=Xm — filed/commented #<N>) | CATCHUP STALLED (uptime=Xm, startup_peak=absent, apply_chain=0/0 — commented #<N>)>
  mem:     <RSS_MB>MB rss | alloc=<alloc>MB resident=<resident>MB frag=<pct>%
           heap=<heap>MB mmap=<mmap>MB unaccounted=<sign><unaccounted>MB peak=<startup_peak|N/A>MB
  disk:    <used>/<total> (<pct>%) | session+data=<size>
  rpc:     <healthy|unhealthy|N/A> oldestL=<X> latestL=<Y> window=<Z>
  obsrvr:  <validating=<Y/N> val24h=<pct>% lag=<N> | N/A (watcher) | N/A (api-error)>
  metrics: <clean | N alerts (<metric1>,<metric2>,...) — filed/commented #<N>,#<M> | N alerts, K suppressed by cooldown | baselines skipped (<reason>) | baselines skipped (<reason>), N gauge alerts (<metric1>,...) — filed/commented #<N>>
  metrics_ratio: scp <ok (accept=X%) | skipped (reason) | WARNING accept=X%<5% (N ticks)>, apply <ok (fail=Y%) | skipped (reason) | WARNING fail=Y%>50% (N ticks) — investigating>, pending <ok (too_old=Z%) | skipped (reason) | WARNING too_old=Z%>50% (N ticks)> | collecting baseline
  recovery_stalled: <ok (delta=0) | breach (delta=N, streak M/3) | WARNING delta=N (M ticks) — investigating | WARNING delta=N (burst) — investigating | skipped (<reason>) | collecting baseline>
  deploy:  <up-to-date | DEFERRED (quarantined: <sha8> reachable from origin/main — see ~/data/deploy_quarantine.txt) | BLOCKED (quarantine file unreadable — fail-closed) | BLOCKED (quarantine ancestry check failed for <sha8> — fail-closed) | DEFERRED (cool-down: ...) | SYNCED (no-binary-impact: ...) | pulled N commits (old..new) | SKIPPED (dirty-tree|ci-red|build-failed, filed/commented #<N>)>
  ci:      <all green (run+job level) | WORKFLOW failed — filed/commented #<N> | WORKFLOW jobs FAILED (continue-on-error) — NAME|conclusion listed, filed/commented #<N>>
  self_reflect: <clean | fixed inline (<sha>: <short-desc>) | filed #<N> (urgent: <short-desc>) | filed #<N> (no-label: <short-desc>) | filed #<N> (not-ready: <short-desc>)>
```

The `wipe:` line is present when `SESSION_WIPED=yes` or `MAINNET_WIPED=yes`
(or both). Format per the wipe-state composition table above. When neither
fires, omit the line entirely.

Use WARNING for threshold breaches **of WARN/SYNC tier**. A firing **NONC-tier**
alarm (e.g. `inbound-auth-low`, see #3653) does **NOT** flip the banner to
WARNING: it is reported on the `metrics:` line and filed/commented as
`Non-critical:` per the Bug Filing Workflow, but the banner stays at the level
set by the WARN/SYNC/ACTION/OFFLINE signals only. This prevents a known-benign,
chronically-firing, consensus-non-impacting condition from pinning the banner to
WARNING on ~90% of ticks (alarm fatigue) while still surfacing it. A genuine
escalation (e.g. zero authenticated inbound → `inbound-auth-critical`, WARN) is
its own WARN-tier alarm and DOES flip the banner. Use ACTION when a corrective
action was taken (restart, deploy, filed a new issue, commented on an existing
issue, session-wipe recovery). Use OFFLINE when the node cannot be recovered in
this tick (e.g., rebuild failed after session wipe).
Use SYNC FAILURE (not WARNING) when the node has exceeded the active sync
deadline (15m populated / 20m fresh-start / 60m crash-recovery) but is not closing ledgers in
real-time — this is a bug that requires immediate investigation AND
filing/commenting on a GitHub issue (label `urgent` since SYNC FAILURE blocks
consensus). Board-route to Backlog per `scripts/lib/monitor-label-policy.md`.

### Tick history capture

After emitting the status report (and before self-reflection), append a
single JSON line to `/home/tomer/data/$MONITOR_SESSION_ID/tick-history.jsonl`
so `/daily-summary` can aggregate the last 24h of ticks:

```bash
# ts is computed inside the Python block — do NOT use shell expansion for timestamps.
HIST=/home/tomer/data/$MONITOR_SESSION_ID/tick-history.jsonl
python3 - <<'PY' >> "$HIST"
import json
from datetime import datetime, timezone
print(json.dumps({
  "ts":           datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "status":       "<OK|WARNING|ACTION|OFFLINE>",
  "ledger":       <current-ledger-int>,
  "build":        "<short-sha>",
  "deploys":      <0 or 1>,
  "warnings":     [<list of metric names that breached>],
  "actions":      [<list of action keywords: restart, deploy, filed-#N, session-wiped-recovery, session-wiped-process-alive, session-wiped-rebuild-failed, mainnet-data-wiped>],
  "self_reflect": "<clean | fixed-inline | filed-#N>",
  "watch":        ["<key>=<value>", ...]
}))
PY
```

> **`ts` contract:** The `ts` field records the wall-clock UTC time captured
> immediately before emitting the JSON line. Format: `YYYY-MM-DDTHH:MM:SSZ`
> (ISO 8601, always UTC). Acceptable skew: ≤ 60 seconds from real wall-clock
> time. Any drift > 60 s indicates a bug in the capture mechanism.

`watch` carries multi-tick non-incident concerns the daily summary should
surface — examples: `pruning_gap=2451`, `frag_pct=18`, `disk_pct=72`,
`wipe=session-dir`, `wipe=mainnet-data`. Add a key whenever the tick body
called out a value worth tracking but did not file an issue. Each entry MUST
be one line — the daily-summary aggregator parses with
`for ln in f: json.loads(ln)`.

## Self-reflection

After the status report is emitted, look back at THIS tick's output and
check for problems **in the monitor itself** — not in the node. The node
is covered by the `## Investigation` policy above. This section is about
bugs / miscalibrations in the tick logic, thresholds, catalog, or
detection code that fired false positives, failed silently, or rendered
contradictory output.

### What to look for

1. **False-positive alert**: a check fired, but investigation showed the
   underlying state is expected-correct. Root cause is the catalog
   threshold / rule, not the node. (Examples from this session:
   `scp_verifier_thread_state !=1` was inverted; `quorum_agree <4` fires
   between externalizations by design.)
2. **Silent failure**: a check returned empty / zero due to a tool bug.
   (Examples: zsh glob `NO_NOMATCH` killed a pipeline; a histogram
   metric name had a typo and silently resolved to `count_delta=0`
   which skipped evaluation.)
3. **Self-contradictory output**: `sync: synced` alongside
   `metrics: peer_count<8 WARN`, or similar internal inconsistency that
   suggests a missing warmup exemption or cross-check rule.
4. **Missing carveout**: a gauge / counter fires during a legitimate
   transient (startup, restart-warmup, crash-recovery replay) but has
   no exemption in the catalog.

If none of the above: report `self_reflect: clean` and stop.

### Three tiers of action

When an issue is found, choose a tier and act on it in the same tick.

**Tier 1 — Fix inline (trivial edit).**

Criteria (ALL must hold):
- Change is contained to `.Codex/skills/monitor-tick/SKILL.md`
  (no other files touched)
- Diff < 50 lines
- No new runtime dependency (no new file path, env var, or metric name
  that the skill depends on)
- The edit is a clear text change with an obvious correct value:
  typo fix, threshold adjustment to match an observed live baseline,
  shell-portability fix, adding a metric to an existing exemption
  list, rendering-template tweak
- The need is demonstrated by THIS tick's observation — not speculative

Action sequence (same pattern used for every skill edit in this repo):

```bash
# 1. Edit .Codex/skills/monitor-tick/SKILL.md
# 2. git add .Codex/skills/monitor-tick/SKILL.md
# 3. git commit -m "Monitor-tick: <what + why>" with Co-authored-by: Codex trailer
# 4. git push origin main (on reject: git pull --rebase && git push)
```

Report: `self_reflect: fixed inline (<short-sha>: <short-desc>)`.

**Tier 2 — File GH issue (non-trivial but codeable).**

Apply the Bug filing workflow's Label policy: most monitor-tick self-reflection
issues are non-urgent (calibration / threshold / catalog tuning) and should
be filed without a label. Only use `urgent` if the skill's miscalibration is
silently masking a real validator-blocking signal.

The issue is real and actionable but any of:
- Touches multiple files or crates
- Requires a design choice (which metric? which threshold value?
  which algorithm?)
- Needs verification beyond the tick (new test cases, reproducing
  with a build)
- Affects runtime contracts (env schema, file format, section
  ordering that another skill depends on)

Before filing, search for an existing open issue:
`gh issue list --search "monitor-tick: <keywords>" --state open`.
Comment with new evidence if a match exists; otherwise file.
Board-route per `scripts/lib/monitor-label-policy.md`: Backlog for
actionable issues, Blocked for `not-ready` issues.

Issue body MUST include:
- **Symptom**: one-line description of the false positive / silent
  failure / contradiction
- **Evidence**: exact tick output and command results that demonstrated
  the issue
- **Suspected root cause**: which rule / threshold / code path
- **Concrete fix sketch**: file:line references and proposed diff
  direction
- **Related to #<prior>** if it's a recurrence of something already
  filed

Title format:
- `Non-critical: monitor-tick: <description>` — for observability /
  calibration issues (noise reduction, threshold tuning) — file with no label
- `monitor-tick: <description>` — for correctness bugs (silent
  failures, contradictory output) — file with no label unless the bug
  is silently masking a real validator-blocking signal (then `urgent`)

Report: `self_reflect: filed #<N> (urgent: <short-desc>)` or
`self_reflect: filed #<N> (no-label: <short-desc>)`.

**Tier 3 — File `not-ready` GH issue (human input required).**

Use this tier when any of:
- The fix has product/ops policy implications (e.g., "should we
  monitor a new metric class?", "should we change the restart
  philosophy?", "should we broaden the auto-deploy trigger?")
- Ambiguous scope — fix A, B, or C would all work and the right
  choice depends on operator intent
- Touches the node code, another skill, or config defaults — the
  downstream fixer should not auto-pick this up without explicit
  operator direction

Issue body includes everything from Tier 2 plus an explicit
**"Human input required"** section listing the specific
decisions / options that need an operator answer.

Label: `not-ready`. Title format:
`monitor-tick: [needs-decision] <description>`.

Report: `self_reflect: filed #<N> (not-ready: <short-desc>)`.

### Boundaries

- **Scope is single-tick.** Do not retrospectively review prior ticks
  for drift. Cross-tick pattern detection is a separate concern.
- **Never suppresses a real node-side filing.** If a check flagged a
  SYNC FAILURE on the node and self-reflection concludes the detection
  was over-eager, the SYNC FAILURE filing still goes out this tick
  (real-or-not is downstream's call). File a separate Tier 2 issue to
  tune the detection.
- **Never re-opens or argues with already-filed issues** from this
  or prior ticks. Those stand as-is.
- **Trivial-fix bias**: when in doubt between Tier 1 and Tier 2,
  prefer Tier 2 (filing) over an aggressive inline edit. Inline edits
  affect every subsequent tick immediately; better to write up the
  rationale and let the operator review.
