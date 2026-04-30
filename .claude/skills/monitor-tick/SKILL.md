---
name: monitor-tick
description: One tick of the henyey mainnet monitor — checks, metrics scan, deploy, status report
---

# Monitor Tick

One invocation of the monitor loop. Intended to be called on a ~10-minute
cadence by an external orchestrator (crontab, systemd timer, CI) via
`claude -p '/monitor-tick'`, or by a Claude-internal `/loop 10m /monitor-tick`.

## Preconditions

This skill expects `/home/tomer/data/monitor-loop.env` to exist — it is
written by `/monitor-loop` at startup and contains the runtime config.
If the file is missing, **bail out immediately** with:

```
ERROR: /home/tomer/data/monitor-loop.env not found — run /monitor-loop first.
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

Throughout this skill, substitute those values for the `$MONITOR_*` references below.

## Fresh-start state

Determine once at the top of the tick: if `/home/tomer/data/mainnet/mainnet.db`
does NOT exist, set `FRESH_START=yes` (sync deadline = 4h). Otherwise
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
PID=$(for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = "henyey" ] && basename $p; done | head -1)
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
- Extract the current ledger from the most recent Heartbeat line in the log tail.
- If the file exists and its ledger equals the current ledger and the recorded
  timestamp is more than 600s old, flag STUCK.
- If the ledger has advanced or the file is missing, overwrite
  `/home/tomer/data/$MONITOR_SESSION_ID/last_ledger` with `"<current-ledger>|<now>"`.
- Check node uptime: `ps -o etime= -p $(for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = "henyey" ] && basename $p; done | head -1)`.
  Active deadline: **15m** when `FRESH_START=no` and `CRASH_RECOVERY=no`,
  **60m** when `CRASH_RECOVERY=yes`, **4h** when `FRESH_START=yes`.
- "Real-time sync" means RPC `age < 30s` — NOT just Heartbeat `gap=0`. Gap is
  the node's local view (`latest_ext - ledger`) and can stay at 0 even when
  the node is minutes behind the network. The authoritative wall-clock signal
  is RPC `age`.
- If uptime exceeds the active deadline AND the node is not in real-time sync:
  flag SYNC FAILURE if `gap > 5`, or `age > 30s`, or `heard_from_quorum=false`
  in the latest Heartbeat. Investigate the catchup path (checkpoint-boundary
  stalls, hash mismatches, event-loop freezes); do not just wait.
- **Progress carveout (only when `CRASH_RECOVERY=yes`)**: if lcl has advanced
  by ≥ 500 ledgers since the previous tick's `last_ledger`, the node is
  actively replaying — report CATCHING UP regardless of uptime. Flag SYNC
  FAILURE only when lcl stops advancing AND uptime exceeds 60m.
- **Fresh-start carveout (`FRESH_START=yes`, uptime < 4h)**: a large gap is
  expected during initial bucket apply — report CATCHING UP, not SYNC FAILURE.

**(3) Process alive** — find by `comm` not `pgrep -f`: `for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = "henyey" ] && basename $p; done`. The earlier `pgrep -f 'henyey.*run'` form is unsafe in environments with parallel `claude --print` agent processes whose prompt args contain "henyey" — they false-match, yielding wrong PIDs for kill/restart. If not running: Rotate-log with suffix `crashed`, then before Relaunch evaluate the **(3a) Repeated-FATAL state-wipe trigger** below.

**(3a) Repeated-FATAL state-wipe trigger** — a kill-loop on the same persisted state means the local lcl is corrupt and forward replay can never reconcile. When this happens, restart-without-wipe just accumulates crashed logs and stays offline. Detect and self-heal once per kill-loop:

```bash
logs_dir=/home/tomer/data/$MONITOR_SESSION_ID/logs
recent_crashed=$(find "$logs_dir" -maxdepth 1 -type f -name 'monitor.log.crashed-*' \
  -newermt '30 minutes ago' -printf '%T@ %p\n' 2>/dev/null | sort -rn)
recent_count=$(printf '%s\n' "$recent_crashed" | grep -c .)
latest_crashed=$(printf '%s\n' "$recent_crashed" | head -1 | cut -d' ' -f2-)
hash_mismatch_signal="no"
if [ -n "$latest_crashed" ] && grep -qE 'FATAL: pre-close hash mismatch|Catchup failed:.*Header hash mismatch' "$latest_crashed" 2>/dev/null; then
  hash_mismatch_signal="yes"
fi
```

Trigger the wipe when ALL hold:
1. `recent_count >= 3` (3+ crashed rotations in the last 30 min — proves restart-without-fix isn't recovering)
2. `hash_mismatch_signal == "yes"` (the most recent crash is hash-mismatch family — the symptom that uniquely identifies persisted state corruption rather than a transient bug)
3. `FRESH_START=no` (don't fire on a fresh sync that hasn't completed yet)

When triggered:

```bash
# Stop any partially-running process first (defensive — should already be dead)
PID=$(for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = "henyey" ] && basename $p; done | head -1)
[ -n "$PID" ] && kill "$PID" && sleep 5 && kill -0 "$PID" 2>/dev/null && kill -9 "$PID"

# Wipe the corrupt persisted state (recoverable from public network archive)
rm -f /home/tomer/data/mainnet/mainnet.db \
      /home/tomer/data/mainnet/mainnet.db-shm \
      /home/tomer/data/mainnet/mainnet.db-wal \
      /home/tomer/data/mainnet/mainnet.lock
rm -rf /home/tomer/data/mainnet/buckets

# Reset progression tracker so the next tick treats this as a fresh start
rm -f /home/tomer/data/$MONITOR_SESSION_ID/last_ledger
```

Then Relaunch. The next tick will see `FRESH_START=yes` (mainnet.db absent), apply the 4h sync deadline, and let the node fresh-catchup from network archive (~10–20 min to validating).

File a new `urgent` GH issue documenting the wipe with the count of crashed rotations, the hash-mismatch evidence from the latest crashed log, and the cumulative downtime — this is a data point for whether the underlying recovery code path needs further hardening even though the immediate cause was already fixed.

The trigger is self-rate-limiting: after a wipe, the new `.crashed-*` rotations stop accumulating (the symptom is gone), so the 3-in-30-min window can't fire again until something else goes wrong.

**(3b) Wedge detection** — a process can be alive but have a frozen event
loop (watchdog fires, HTTP hangs, ledger progression stops). Check 3 alone
misses this because `pgrep` still finds the PID.

Flag WEDGE when BOTH:
1. `grep 'WATCHDOG: Event loop appears frozen' $LOG | tail -1` is present
   with a timestamp within the last 120s.
2. `curl -s -m 3 http://localhost:$MONITOR_ADMIN_PORT/info` returns empty
   body or times out.

On WEDGE: Stop-PID, Rotate-log with suffix `frozen`, then Relaunch.
Always file a new `urgent`-labeled issue (wedge blocks validator operation).
Recurrence-after-fix → NEW issue, not a comment on a closed one. Known prior
incidents: #1904, #1873, #1921, #1949.

**(4) Memory** — `ps -o rss= -p $(for p in /proc/[0-9]*; do [ "$(cat $p/comm 2>/dev/null)" = "henyey" ] && basename $p; done | head -1)`, convert to MB.

- If `RSS > 12 GB`, flag HIGH MEMORY (report-only; no restart).
- **Restart condition** — restart only if ALL hold (this gates on system
  pressure AND evidence of a real heap leak, so we don't kill a legit catchup):
  1. `RSS > 16 GB`, AND
  2. system `available` memory from `free -m` (NOT `free` — that excludes
     reclaimable kernel cache) `< 8 GB`, AND
  3. latest two `Memory report summary` entries both show `heap_components_mb`
     growing by > 500 MB vs the earlier snapshot.
- Restart: Stop-PID, Rotate-log suffix `crashed`, Relaunch.

**(5) Disk** — `df -h /home/tomer/data | tail -1`. If usage > 85%, flag LOW DISK.
Then keep the 3 most recent rotated archives per category (the ISO 8601
timestamp suffix sorts lexicographically, so `sort -r` gives newest-first;
use `find -printf` not shell glob to survive zsh `NO_NOMATCH`):

```bash
logs_dir=/home/tomer/data/$MONITOR_SESSION_ID/logs
[ -d "$logs_dir" ] && for pat in preredeploy crashed stuck frozen; do
  find "$logs_dir" -maxdepth 1 -type f -name "monitor.log.$pat-*" \
    -printf '%f\n' 2>/dev/null | sort -r | tail -n +4 \
    | while read -r f; do rm -f "$logs_dir/$f"; done
done
```
Report how many files were removed if any.

**(6) Session disk** — `du -sh /home/tomer/data/$MONITOR_SESSION_ID/`
and `du -sh /home/tomer/data/mainnet/`. If combined > 200 GB, flag SESSION DISK HIGH.

**(7) Memory report** — `grep 'Memory report summary' /home/tomer/data/$MONITOR_SESSION_ID/logs/monitor.log | tail -1`.
If grep returns no output AND process uptime > 400s, flag WARNING
memory-report-missing (log format may have changed). Memory reports emit
every ~6 minutes, so an uptime below 400s legitimately has no entry yet on
a post-restart tick — report `memreport: N/A (warmup)` and move on.
Otherwise extract `jemalloc_allocated_mb`, `jemalloc_resident_mb`,
`fragmentation_pct`, `heap_components_mb`, `mmap_mb`, `unaccounted_mb`,
`unaccounted_sign`. If `fragmentation_pct > 50`, flag HIGH FRAGMENTATION.
If `unaccounted_mb > 1000` with sign `+`, note it (known jemalloc overhead,
not a bug — but verify `heap_components` is stable; if it is growing, investigate).

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
3. `curl -s http://localhost:$MONITOR_ADMIN_PORT/metrics > /home/tomer/data/$MONITOR_SESSION_ID/metrics/current.prom`.
4. **Counter reset handling**: for any counter, if `current < prev`, treat
   `delta = current` (process restarted).

**Post-restart warmup exemptions** — for the first 2 ticks after a detected
restart (check 3, check 10, or `CRASH_RECOVERY=yes`), skip these alerts.
Overlay handshake + jemalloc arena stabilization + from-zero counters take
~10 minutes to settle.

- `henyey_jemalloc_fragmentation_pct > 50` (post-restart frag ramps ~35-45%, settles to ~18%)
- `stellar_peer_count < 8` (peer count ramps 0 → 10+ over the first 10 min)
- `stellar_overlay_inbound_authenticated < 3`
- `stellar_scp_timing_externalized_seconds > 3`
- `stellar_scp_timing_nominated_seconds > 2`
- counter-started-at-zero delta alerts (any catalog entry whose `prev` is 0)

### Metric alert catalog

Catalog-wide notes:
- All entries below are subject to the warmup exemptions above and the cooldown
  rule in §Firing alerts.
- "Synced-only gating" = `uptime > 15m AND CRASH_RECOVERY=no AND FRESH_START=no`.
  These gauges fire only when the node should be in real-time sync; the
  authoritative sync check is (2). The catchup phases are legitimate
  non-synced states.

**COUNTERS** (fire on `delta ≥ threshold` per tick):

- `stellar_herder_lost_sync_total` ≥1 → SYNC
- `henyey_post_catchup_hard_reset_total` ≥1 → ACTION
- `henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"}` ≥1 → WARN
  (Form 2 extraction; alert identity = the full selector-qualified name. Other
  reasons are informational/duplicative — `backoff_active` ticks routinely;
  `forcing_catchup_not_behind` is debug; the fast-track caller emits its own WARN.)
- `(stellar_overlay_timeout_idle_total + stellar_overlay_timeout_straggler_total)` ≥5× prior-tick-sum → WARN
- `(stellar_overlay_error_read_total + stellar_overlay_error_write_total)` ≥50 → WARN
- `henyey_archive_cache_refresh_error_total` ≥1 → NONC
- `henyey_archive_cache_refresh_timeout_total` ≥3 → NONC

`stellar_ledger_apply_failure_total`, `henyey_scp_post_verify_drops_total`,
and `stellar_herder_pending_too_old_total` are covered by the ratio checks
below (traffic-proportional and self-calibrating); do NOT check absolute deltas.

**GAUGES** (fire on absolute threshold against current snapshot):

- `stellar_peer_count` <8 → WARN
- `stellar_overlay_inbound_authenticated` <3 → WARN (synced-only gating; healthy fleet has 50+ inbound. Aggregate `peer_count` can be ≥8 from outbound while inbound starves consensus; this catches that case)
- `stellar_ledger_age_current_seconds` >30 → SYNC (synced-only gating)
- `stellar_herder_state` !=2 → SYNC (synced-only gating)
- `henyey_jemalloc_fragmentation_pct` >50 on two consecutive ticks → WARN
- `henyey_scp_verify_input_backlog` >100 on two consecutive ticks → WARN (single snapshots routinely spike to 100+ during slot externalize bursts and drain to 0 within seconds — gate on persistence. Sample /metrics 5x at 2s intervals to verify before filing)
- `henyey_scp_verifier_thread_state` !=0 → WARN (0=Running, 1=Stopping, 2=Dead)
- `stellar_herder_pending_envelopes` >2000 → WARN
- `henyey_overlay_fetch_channel_depth_max` >500 → WARN
- `(henyey_process_open_fds / henyey_process_max_fds)` >0.85 → WARN
- `henyey_herder_drift_max_seconds` >10 → NONC
- `stellar_scp_timing_externalized_seconds` >10 → WARN (this is a SLOT-CYCLE metric:
  first-envelope-received → self-externalize, naturally ~4-6s on mainnet's 5s slots.
  The earlier `>3` threshold was a misread of #1934 — it compared against stellar-core's
  `mFirstToSelfExternalizeLag` which measures a different, much narrower window (any-node
  externalize → self-externalize, ~0.3-0.5s healthy). Until we expose a matching metric,
  alert only on >2x normal slot-cycle = real degradation)
- `stellar_scp_timing_nominated_seconds` >7 → WARN (also a SLOT-CYCLE metric measured from
  first local nomination vote to self-externalize; healthy floor ~3-5s on mainnet)

`quorum_agree` / `quorum_missing` / `quorum_fail_at` are intentionally NOT
monitored — they snapshot the tracking slot's QuorumInfo and return
false-positive noise between externalizations; see `monitor-loop/SKILL.md`
Metrics Scan section for the code-path explanation.

**HISTOGRAMS** (fire on p99 bucket of per-tick delta): for each histogram H,

```
bucket_delta[le] = H_bucket_current[le] - H_bucket_prev[le]
count_delta = H_count_current - H_count_prev
```

Skip if `count_delta < 20`. Compute cumulative bucket delta at upper edge L:
`sum(bucket_delta[le] for le ≤ L)`. The smallest L where cumulative ≥
`0.99 * count_delta` is the p99 upper bound.

Thresholds (all WARN):

- `henyey_ledger_close_handle_complete_seconds` p99 >0.5s
- `henyey_ledger_close_dispatch_to_join_seconds` p99 >5s
- `henyey_ledger_close_post_complete_seconds` p99 >0.5s
- `henyey_ledger_close_tx_exec_seconds` p99 >1s
- `henyey_ledger_close_soroban_exec_seconds` p99 >1s
- `henyey_ledger_close_commit_seconds` p99 >0.5s
- `henyey_ledger_close_soroban_state_seconds` p99 >0.5s
- `henyey_ledger_close_complete_tx_queue_seconds` p99 >0.5s

Mean check (`sum_delta / count_delta`) is a cheaper fallback — fire on whichever breaches.

### Metric extraction forms

Counter catalog entries use one of three extraction forms. When adding a new
counter, check the emission site in `crates/app/src/metrics.rs` to determine
whether the metric is scalar or labeled, and use the appropriate form.

**Form 1 — Scalar counter** (most counters):
```bash
cur=$(grep -E '^<metric_name> ' current.prom | awk '{printf "%d", $2}')
prev=$(grep -E '^<metric_name> ' prev.prom | awk '{printf "%d", $2}')
```

**Form 2 — Single labeled series** (extract one specific label value):
```bash
cur=$(grep -E '^<metric_name>\{<label>="<value>"\} ' current.prom | awk '{printf "%d", $2}')
prev=$(grep -E '^<metric_name>\{<label>="<value>"\} ' prev.prom | awk '{printf "%d", $2}')
```

**Form 3 — Sum of explicit labeled series** (sum specific label values):
```bash
cur=$(awk '/^<metric_name>\{<label>="<value1>"\}|^<metric_name>\{<label>="<value2>"\}/ {sum+=$NF} END{printf "%d", sum+0}' current.prom)
prev=$(awk '/^<metric_name>\{<label>="<value1>"\}|^<metric_name>\{<label>="<value2>"\}/ {sum+=$NF} END{printf "%d", sum+0}' prev.prom)
```

For all forms, delta handling is identical:
- Extract `prev` using the same pattern against `prev.prom` (default 0 if absent)
- If `current < prev`: counter reset (node restarted) → `delta = current`
- Otherwise: `delta = current - prev`

**Label-presence validation:** For labeled counters, validate that the
expected label set is present before extracting. This catches silent
renames or removals that would weaken an alert. If the expected label set
is missing or mutated, **skip the alert entirely** rather than treating
missing series as zero. Follow the existing pattern used for
`henyey_scp_post_verify_total` (lines 406-413):
```bash
labels=$(grep -oP '^<metric_name>\{<label>="\K[^"]+' current.prom | sort)
expected=$(printf '%s\n' <label1> <label2> ... | sort)
if [ "$labels" != "$expected" ]; then
  # report: <metric>: skipped (label set mismatch)
fi
```

### Ratio checks — sustained breach detection

Three ratio-based checks replace the former absolute-delta thresholds for
`stellar_ledger_apply_failure_total`, `henyey_scp_post_verify_drops_total`,
and `stellar_herder_pending_too_old_total`.
These fire only after **3 consecutive ticks** of threshold breach, avoiding
transient spikes.

**Required inputs** — global (all must be present, non-empty, numeric; if any
missing or invalid, skip ratio checks entirely, empty the ratio snapshot,
reset streaks):

1. `stellar_ledger_age_current_seconds` — sync gating
2. `stellar_ledger_apply_success_total`
3. `stellar_ledger_apply_failure_total`
4. `henyey_scp_post_verify_total{reason="accepted"}`
5. `henyey_scp_post_verify_total{reason="processed_directly"}`
6. Sum of all 13 `henyey_scp_post_verify_total{reason="..."}` lines (`pv_total_sum`)

**Per-check required inputs** — these are NOT part of the global required set.
If missing, only the pending check (Check 3) skips; SCP and apply proceed
normally:

7. `stellar_herder_pending_too_old_total`
8. `stellar_herder_pending_received_total`

**Post-verify label-set validation:** After extracting the 13
`henyey_scp_post_verify_total{reason="..."}` lines, validate that the exact set
of reason labels is: `invalid_sig`, `panic`, `drift_range`, `drift_close_time`,
`drift_cannot_receive`, `self_message`, `non_quorum`, `buffered`, `duplicate`,
`too_far`, `buffer_full`, `processed_directly`, `accepted`. If any label is
missing or unexpected labels appear, treat as missing counters (partial scrape
or label change).

**Skip conditions** (skip all ratio checks when any is true):
- `FRESH_START=yes`
- Heartbeat gap > 5
- `stellar_ledger_age_current_seconds > 30`
- Process uptime < 10 minutes
- `/metrics` fetch fails (curl error or empty response)
- `/metrics` returns "recorder not installed"
- Any required counter missing or invalid
- Post-verify label set ≠ expected 13 labels

On any skip: **empty** `/home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot`,
reset all streak counters to 0, report `metrics_ratio: skipped (<reason>)`.

**Snapshot file:** `/home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot`

Format:
```
version=1
pid=<PID>
start_ticks=<field 22 from /proc/$PID/stat>
timestamp=<ISO8601>
apply_success=<value>
apply_failure=<value>
pv_accepted=<value>
pv_processed_directly=<value>
pv_total_sum=<value>
apply_breach_streak=<N>
scp_breach_streak=<N>
pending_too_old=<value>
pending_received=<value>
pending_breach_streak=<N>
```

**Process identity:** `start_ticks` = field 22 from `/proc/$PID/stat` (starttime
in clock ticks since boot). Extract with `awk '{print $22}' /proc/$PID/stat`.
Locale-independent, catches PID reuse.

**Invalidation:**
- PID or `start_ticks` changed → discard, write new baseline, reset streaks
- Snapshot malformed, missing fields, or `version` ≠ `1` → discard
- Any current counter < previous value → discard (counter reset)

**Metric extraction** — illustrative pseudocode (not literal shell; each bail
point must stop ratio-check processing for this tick):

```bash
metrics_body=$(curl -s http://localhost:$MONITOR_ADMIN_PORT/metrics)

# Bail on fetch failure — stop ratio checks for this tick
if [ -z "$metrics_body" ]; then
  > /home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot
  # report: metrics_ratio: skipped (fetch failed)
  # STOP — do not proceed to extraction or ratio evaluation
fi

# Bail if recorder not installed — stop ratio checks
if echo "$metrics_body" | grep -q 'metrics recorder not installed'; then
  > /home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot
  # report: metrics_ratio: skipped (recorder not installed)
  # STOP
fi

ledger_age=$(echo "$metrics_body" | grep -E '^stellar_ledger_age_current_seconds ' | awk '{printf "%d", $2}')
apply_success=$(echo "$metrics_body" | grep -E '^stellar_ledger_apply_success_total ' | awk '{printf "%d", $2}')
apply_failure=$(echo "$metrics_body" | grep -E '^stellar_ledger_apply_failure_total ' | awk '{printf "%d", $2}')
pv_accepted=$(echo "$metrics_body" | grep -E '^henyey_scp_post_verify_total\{reason="accepted"\} ' | awk '{printf "%d", $2}')
pv_processed=$(echo "$metrics_body" | grep -E '^henyey_scp_post_verify_total\{reason="processed_directly"\} ' | awk '{printf "%d", $2}')

# Validate exact 13-label set — STOP if mismatch
pv_labels=$(echo "$metrics_body" | grep -oP '^henyey_scp_post_verify_total\{reason="\K[^"]+' | sort)
expected_labels=$(printf '%s\n' accepted buffer_full buffered drift_cannot_receive drift_close_time drift_range duplicate invalid_sig non_quorum panic processed_directly self_message too_far | sort)
if [ "$pv_labels" != "$expected_labels" ]; then
  > /home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot
  # report: metrics_ratio: skipped (label set mismatch)
  # STOP
fi

pv_total_sum=$(echo "$metrics_body" | grep -E '^henyey_scp_post_verify_total\{reason="[^"]+"\} ' | awk '{sum+=$2} END {printf "%d", sum}')

# Validate all 6 global values present and numeric — STOP if any invalid
for v in "$ledger_age" "$apply_success" "$apply_failure" "$pv_accepted" "$pv_processed" "$pv_total_sum"; do
  if [ -z "$v" ] || ! echo "$v" | grep -qE '^[0-9]+$'; then
    > /home/tomer/data/$MONITOR_SESSION_ID/metrics/ratio_snapshot
    # report: metrics_ratio: skipped (missing counters)
    # STOP
  fi
done

# Per-check inputs for pending (Check 3) — if missing, only Check 3 skips
pending_too_old=$(echo "$metrics_body" | grep -E '^stellar_herder_pending_too_old_total ' | awk '{printf "%d", $2}')
pending_received=$(echo "$metrics_body" | grep -E '^stellar_herder_pending_received_total ' | awk '{printf "%d", $2}')
pending_counters_valid=true
for v in "$pending_too_old" "$pending_received"; do
  if [ -z "$v" ] || ! echo "$v" | grep -qE '^[0-9]+$'; then
    pending_counters_valid=false
    break
  fi
done

# henyey_recovery_stalled_tick_total: extract via Form 2 (see §Metric extraction
# forms). Validate expected 3-label set {backoff_active, forcing_catchup_behind,
# forcing_catchup_not_behind}; skip the alert entirely on mismatch. Alert when
# the delta of the forcing_catchup_behind series ≥ 1.
```

**Check 1: SCP post-verify acceptance rate**
- Numerator delta: `delta(pv_accepted) + delta(pv_processed)`
- Denominator delta: `delta(pv_total_sum)`
- Alert: `(numerator / denominator) < 0.05` (less than 5% accepted) for 3 consecutive ticks
- Min denominator delta: 500 (below → `scp: skipped (low volume)`, reset scp streak)
- Baseline acceptance on mainnet: ~10-20%. <5% sustained = nearly nothing reaching SCP.
- On breach: increment `scp_breach_streak`. If streak ≥ 3, route through Bug Filing Workflow
  (investigate verifier thread, stale envelopes, sync state).
- **On healthy tick** (ratio ≥ 0.05 with sufficient volume): reset `scp_breach_streak` to 0.

**Check 2: Transaction apply failure rate**
- Numerator delta: `delta(apply_failure)`
- Denominator delta: `delta(apply_failure) + delta(apply_success)`
- Alert: `(numerator / denominator) > 0.50` (over 50% fail) for 3 consecutive ticks
- Min denominator delta: 200 (below → `apply: skipped (low volume)`, reset apply streak)
- On breach: increment `apply_breach_streak`. If streak ≥ 3, investigate in same tick.
  If evidence points to henyey apply-engine bug, file/comment via Bug Filing Workflow.
  If expected bad-tx traffic (spam wave, known rejections), report as WARNING without filing.
- **On healthy tick** (ratio ≤ 0.50 with sufficient volume): reset `apply_breach_streak` to 0.

**Check 3: Pending too-old rate**
- Numerator delta: `delta(pending_too_old)`
- Denominator delta: `delta(pending_received)`
- Alert: `(numerator / denominator) > 0.50` (over 50% too old) for 3 consecutive ticks
- Min denominator delta: 100 (below → `pending: skipped (low volume)`, reset pending streak)
- **Per-check skip:** If `pending_counters_valid` is false (missing counters), skip this check
  only — report `pending: skipped (missing counters)`, reset `pending_breach_streak` to 0.
  SCP and apply checks proceed normally.
- On breach: increment `pending_breach_streak`. If streak ≥ 3, report as WARNING
  (overlay lag or stale-envelope flood). Investigate overlay peer health and slot progression.
- **On healthy tick** (ratio ≤ 0.50 with sufficient volume): reset `pending_breach_streak` to 0.

**Per-check state machine** (each check independently):
- **Skip** (global skip, low volume, or missing data) → reset that check's streak to 0
- **Healthy** (ratio within threshold, sufficient volume) → reset streak to 0
- **Breach** (ratio exceeds threshold, sufficient volume) → increment streak

**Per-check low-volume:** When only one check's denominator delta is below its
minimum, that check skips (streak resets to 0) and the others proceed normally.

**Thresholds are provisional** — tune after 1-2 weeks of production data.

**Status report:** Each check independently reports one of: `ok (value)`,
`skipped (reason)`, `WARNING value (N ticks)`, or `collecting baseline`.

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
   - If no match, `gh issue create` (append `--label urgent` only when the
     metric breach is operation-blocking; most metric alerts are non-urgent
     and should be filed without a label) with:
     - Title: `Non-critical: metrics: <metric>` (NONC tier) or `metrics: <metric> — <symptom>` (WARN/SYNC tier).
     - Body: current/prev values, delta, threshold, ledger, binary sha, related
       sibling metrics, file:line citation from `grep -n "<metric_name_without_prefix>" crates/ -r`,
       and a suggested fix.
4. Update `anomaly_cooldown.json` with `{"<metric>": <now>}`.
5. For SYNC-tier alerts, ALSO update the `sync:` line in the status report
   (not just `metrics:`).

### Watcher mode

If `$MONITOR_MODE = watcher`, run check (12) with a reduced catalog: only
process (open_fds, max_fds), jemalloc, and overlay counters/gauges. Skip
SCP, quorum, herder_state, histogram p99 alerts, and ratio checks.

## Remote sync & redeploy

**(10) Remote sync** — first sanity-check the working tree:

- If `git status --porcelain` reports any output, ABORT the deploy path for
  this tick. Report: `DEPLOY SKIPPED (dirty tree)` with the list of dirty
  paths. Do not run `git pull` against a dirty tree; do not kill the node.
  Investigate the dirty tree before the next tick.
- If clean, `git fetch origin main`. If in detached HEAD state
  (`git symbolic-ref HEAD` fails), `git checkout main` first.
- Compare `git rev-parse HEAD` vs `git rev-parse origin/main`.

If they differ (origin/main is ahead):

1. **Cool-down guard**: if node uptime is `< 60m` AND `CRASH_RECOVERY=no` (i.e.
   the previous tick deployed cleanly), SKIP DEPLOY this tick. Report:
   `DEPLOY DEFERRED (cool-down: uptime=<X>m < 60m)`. Rationale: every restart
   costs ~1-2m of validation downtime + brief peer reconnect. Deploying multiple
   commits within minutes thrashes the validator without giving each version
   time to surface issues. The 60m floor was raised from the original 30m
   (#1944) on 2026-04-30 after observing too many quick-cycle deploys per day.
   Crash-recovery restarts are exempt because they're not voluntary deploys.
   Urgent fixes can override by killing the node manually before the next
   tick — the normal startup path will pick up the latest origin/main.
2. Check CI status on origin/main: `gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'`.
   If any recent run has conclusion `failure`, do NOT deploy — route the failure
   through check (11) and wait.
3. If all conclusions are `success` (ignore `""` for in-progress and `cancelled`):
   `git pull --rebase`, `CARGO_TARGET_DIR=/home/tomer/data/$MONITOR_SESSION_ID/cargo-target cargo build --release -p henyey`.
4. If build succeeds: Stop-PID, Rotate-log suffix `preredeploy`, Relaunch.
   Report: `DEPLOY — pulled <N> commits (<old-sha>..<new-sha>), rebuilt, restarted at L<ledger>`.
5. If build fails: report `BUILD FAILED`, do NOT restart — the old binary is
   still running. Route the build error through check (11).

If `HEAD == origin/main`: no action (already up to date).

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
5. Otherwise, file a new issue: `gh issue create --label urgent --title "<workflow>: <short signature>" --body "..."` with investigation findings.
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

### Filing flow

1. Identify the failing signature (ledger + error type for node bugs; metric
   + threshold for alerts; workflow + job + error type for CI).
2. Investigate to root cause — read source code, trace code paths.
3. Check for an existing open issue: `gh issue list --search "<keywords>" --state open`.
   If a match exists, verify its state is OPEN
   (`gh issue view <N> --json state -q .state`), `gh issue comment <N>` with
   new evidence, and apply the `urgent` label only if the recurrence meets
   the urgent criteria above. STOP.
4. If no OPEN match, file a new issue with `gh issue create` — append
   `--label urgent` only if the symptom meets the urgent criteria above;
   otherwise omit the label. Body is a self-contained proposal (symptom,
   evidence, suspected root cause, fix sketch with file:line references).
5. Do NOT spawn agents. Do NOT edit the main checkout. The next redeploy tick
   (check 10) will pick up whatever lands on main.

**Recurrence policy**: If a previously-filed bug recurs with material new
evidence, prefer commenting on the existing issue when it is the same bug at
the same site AND the issue is OPEN. If the prior issue is CLOSED, file a new
issue (label per the policy above) with `Related to #<prior> (closed)` in the
body and a note on why the prior fix did not cover this case; do NOT comment
on the closed issue. File a new issue (still referencing `Related to #<prior>`
with one-line scope-diff) when new evidence points at a different named
subsystem, phase/mark, root-cause hypothesis, or candidate site set.

**Commit policy**: the monitor does NOT commit code. All fixes are delegated
via `gh issue`.

**Deploy regression policy**: If the node fails after a deploy, (a) file or
comment on a GitHub issue (label `urgent` since validator operation is
impacted) with the regression details (commit range, symptoms, watchdog
data); (b) restart the node on the last known-good binary (rebuild from the
previous commit) while waiting for the fix. Do NOT revert commits inline.

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
MONITOR <OK|WARNING|ACTION> — L<ledger> — <timestamp>
  node:    mode=<MODE> session=<session-id> pid=<PID> fresh_start=<yes|no>
  sync:    <synced | CATCHING UP (gap=N, uptime=Xm, deadline=<15m|60m|4h>) | SYNC FAILURE (gap=N, uptime=Xm — filed/commented #<N>)>
  mem:     <RSS_MB>MB rss | alloc=<alloc>MB resident=<resident>MB frag=<pct>%
           heap=<heap>MB mmap=<mmap>MB unaccounted=<sign><unaccounted>MB
  disk:    <used>/<total> (<pct>%) | session+data=<size>
  rpc:     <healthy|unhealthy|N/A> oldestL=<X> latestL=<Y> window=<Z>
  obsrvr:  <validating=<Y/N> val24h=<pct>% lag=<N> | N/A (watcher) | N/A (api-error)>
  metrics: <clean | N alerts (<metric1>,<metric2>,...) — filed/commented #<N>,#<M> | N alerts, K suppressed by cooldown>
  metrics_ratio: scp <ok (accept=X%) | skipped (reason) | WARNING accept=X%<5% (N ticks)>, apply <ok (fail=Y%) | skipped (reason) | WARNING fail=Y%>50% (N ticks) — investigating>, pending <ok (too_old=Z%) | skipped (reason) | WARNING too_old=Z%>50% (N ticks)> | collecting baseline
  deploy:  <up-to-date | pulled N commits (old..new) | SKIPPED (dirty-tree|ci-red|build-failed, filed/commented #<N>)>
  ci:      <all green (run+job level) | WORKFLOW failed — filed/commented #<N> | WORKFLOW jobs FAILED (continue-on-error) — NAME|conclusion listed, filed/commented #<N>>
  self_reflect: <clean | fixed inline (<sha>: <short-desc>) | filed #<N> (urgent: <short-desc>) | filed #<N> (no-label: <short-desc>) | filed #<N> (not-ready: <short-desc>)>
```

Use WARNING for threshold breaches. Use ACTION when a corrective action was
taken (restart, deploy, filed a new issue, commented on an existing issue).
Use SYNC FAILURE (not WARNING) when the node has exceeded the active sync
deadline (15m populated / 4h fresh-start) but is not closing ledgers in
real-time — this is a bug that requires immediate investigation AND
filing/commenting on a GitHub issue (label `urgent` since SYNC FAILURE blocks consensus).

### Tick history capture

After emitting the status report (and before self-reflection), append a
single JSON line to `/home/tomer/data/$MONITOR_SESSION_ID/tick-history.jsonl`
so `/daily-summary` can aggregate the last 24h of ticks:

```bash
HIST=/home/tomer/data/$MONITOR_SESSION_ID/tick-history.jsonl
python3 - <<PY >> "$HIST"
import json
print(json.dumps({
  "ts":           "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "status":       "<OK|WARNING|ACTION|OFFLINE>",
  "ledger":       <current-ledger-int>,
  "build":        "<short-sha>",
  "deploys":      <0 or 1>,
  "warnings":     [<list of metric names that breached>],
  "actions":      [<list of action keywords: restart, deploy, filed-#N>],
  "self_reflect": "<clean | fixed-inline | filed-#N>",
  "watch":        ["<key>=<value>", ...]
}))
PY
```

`watch` carries multi-tick non-incident concerns the daily summary should
surface — examples: `pruning_gap=2451`, `frag_pct=18`, `disk_pct=72`. Add
a key whenever the tick body called out a value worth tracking but did
not file an issue. Each entry MUST be one line — the daily-summary
aggregator parses with `for ln in f: json.loads(ln)`.

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
- Change is contained to `.claude/skills/monitor-tick/SKILL.md`
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
# 1. Edit .claude/skills/monitor-tick/SKILL.md
# 2. git add .claude/skills/monitor-tick/SKILL.md
# 3. git commit -m "Monitor-tick: <what + why>" with Co-authored-by: Claude Code trailer
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
