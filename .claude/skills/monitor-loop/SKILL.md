---
name: monitor-loop
description: Run and monitor a henyey mainnet node, detecting bugs and filing issues
argument-hint: [--watcher]
---

Parse `$ARGUMENTS`:
- If `--watcher` is present, set `$MODE = watcher`. Otherwise set `$MODE = validator`.

# Monitor Loop

Run a henyey mainnet node and monitor it for errors. When bugs are found,
investigate to root cause and file (or comment on) a GitHub issue labeled
`ready`. A separate downstream process picks up `ready`-labeled issues
and commits the fix — the monitor never edits code. All henyey issues
are in scope: node bugs, CI failures, testnet parity bugs, performance
regressions, infrastructure issues.

**Mainnet operation is explicitly authorized** — this overrides the
testnet-only guideline in CLAUDE.md.

## Commit Policy

> **Maintenance note:** this policy is also restated inside the Loop Prompt
> Template so that the cron context (which runs without this skill file) has
> the same rules. Any change here must be mirrored there.

- **The monitor does NOT commit code.** All fixes — node bugs and CI
  failures — are delegated via `gh issue create --label ready` (or a
  comment on an existing issue). A separate downstream process picks up
  `ready`-labeled issues and commits the fix.
- If a deployed commit causes a regression, file/comment a `ready`-labeled
  issue and restart the node on the last known-good binary while waiting
  for the fix.

## Fix-Routing Policy

**All fixes — both CI failures and node bugs — are delegated via
`gh issue` with the `ready` label.** The monitor never implements a
fix inline. A separate downstream process watches for `ready`-labeled
issues and drives them to completion.

- **CI failures** (build errors, test failures, clippy, workflow YAML,
  flaky tests, timeouts): investigate to root cause, then file a GH
  issue with `--label ready`.
- **Node bugs** (hash mismatches, crashes, consensus/sync failures,
  pruning defects, memory leaks): investigate to root cause, then file
  a GH issue with `--label ready`.

In both cases:

1. **Before filing, always check for an existing open issue.** Use
   `gh issue list --search "<symptom or signature>" --state open` (or
   `--state all` when relevant) and skim the results. If one already
   covers the same symptom/subsystem, **comment** on it with the new
   evidence instead of filing a duplicate. Only file a new issue when
   the existing one is stale/closed, or when the evidence points at a
   different root cause (see Recurrence Policy below).
2. The monitor does **not** spawn agents, open worktrees, review diffs,
   or cherry-pick. The downstream pickup process handles all of that.
3. The next redeploy tick rebuilds and restarts the node whenever a
   fix has landed on main.

If you are unsure whether to file at all, err toward filing (or
commenting on an existing issue).

Because the monitor is a single-investigator process (only one
`/monitor-loop` holds the concurrency lock at a time — see Startup step
1), the investigation phase itself — reading source files, grepping for
symbols, running `git log`, reading the running node's log — happens
directly in the main checkout. Investigation is read-only; the monitor
never edits the main checkout.

## Configurations

Two validator configs exist. The skill always uses the RPC-enabled config
for validator mode so that RPC health and pruning can be monitored:

| Config | Mode | RPC | Maintenance | Use when |
|--------|------|-----|-------------|----------|
| `configs/validator-mainnet-rpc.toml` | validator | port 8000, retention 360 ledgers | every 900s, count 50000 | Default for `/monitor-loop` |
| `configs/validator-mainnet.toml` | validator | none | none | Not used by this skill |
| `configs/mainnet.toml` | watcher | none | none | `/monitor-loop --watcher` |

Key ports:
- **8009** — overlay peer port (non-standard; advertised in HELLO messages)
- **8000** — Soroban JSON-RPC (validator mode only, via `[rpc]` config)
- **11627** — admin HTTP API in validator mode (`/info`, `/scp`, `/maintenance`)
- **11727** — admin HTTP API in watcher mode (`configs/mainnet.toml` sets `[http] port = 11727`)

Throughout this skill `<ADMIN_PORT>` means 11627 for validator mode and 11727
for watcher mode. All admin-endpoint examples below use the validator port;
substitute 11727 when running under `--watcher`.

## Investigation Policy

**Every anomaly must be investigated to root cause.** When a check reveals
something unexpected — a threshold exceeded, a warning in logs, a value that
doesn't change when it should — you MUST investigate. Specifically:

- **Never rationalize away an issue.** "This is probably expected" or "it
  should fix itself" are not acceptable conclusions. If you don't know why
  something is happening, that's a bug until proven otherwise.
- **Follow the evidence.** Read the relevant source code, check the database,
  examine the data structures. The answer is in the code, not in speculation.
- **Trace the full path.** If maintenance is running but pruning isn't
  working, read the maintenance code to understand what it actually prunes.
  If a value isn't changing, find the code that's supposed to change it.
- **Report what you found.** Even if the investigation reveals the behavior
  is correct, document *why* — citing the specific code path, not just
  a guess.
- **File issues.** If investigation reveals a bug, file a GitHub issue
  with `--label ready`, root cause, and a proposed fix. If it reveals a
  missing feature or config issue, file it with the code-level
  explanation. Do NOT spawn agents; the downstream pickup process
  handles the fix.
- **File everything, including non-critical misbehavior.** Every anomaly
  that is not literal expected-correct behavior results in a GitHub issue
  (or a comment on an existing one — always search first). This includes
  low-severity oddities that do not require action right now: spurious log
  lines, metrics that drift without visible effect, counters that reset
  when they shouldn't, log levels that seem wrong for the observed state,
  and any other "huh, that's odd" finding. **Do NOT decide a thing is
  too small to file.** Rationale: today's harmless noise is tomorrow's
  diagnostic confusion — a spurious `Recovery stalled` log hides a real
  stall the next time it fires; a wrongly-named metric breaks a dashboard
  panel silently. Filing keeps institutional memory out of any single
  operator's head. Prefix such issues `Non-critical:` in the title so
  triage can prioritize, but file them.
- **Sync deadline is 15 minutes — with a fresh-start carveout.** A mainnet
  validator with an existing, populated data directory must complete catchup
  and begin closing ledgers in real-time within 15 minutes of startup. If the
  node is still catching up, stuck at checkpoint boundaries, or showing RPC
  "unhealthy" after 15 minutes on a populated DB, this is a bug — not a
  normal startup delay.
  **Fresh-start carveout:** if `~/data/mainnet/mainnet.db` does not exist at
  the start of the loop tick, the node is performing initial bucket download
  + replay. Use a 4-hour deadline instead. The loop determines fresh-start
  status by checking for the DB file at each tick.
  In the non-fresh case: do not report the 15m breach as a WARNING and wait.
  Investigate the catchup path, the buffered-catchup code, and the checkpoint
  download logic, then route through the Bug / CI-Failure Filing Workflow.
- **"Real-time sync" means `age < 30s` (RPC healthy), NOT just `gap == 0`.**
  The Heartbeat `gap = latest_ext - ledger` is the node's *local* view —
  gap=0 only says the node has closed everything *it has observed* as
  externalized. The node can be behind network tip by many ledgers while
  still reporting gap=0 if it hasn't received those externalization messages
  yet (or is processing them slower than the network produces them). The
  authoritative wall-clock signal is RPC `getHealth` `age` (= `now -
  close_time` of the latest-closed ledger). Stellar mainnet closes every
  ~5s, so a healthy validator always sits in the 0-20s age band. If `age`
  is persistently > 30s (RPC `status=unhealthy`) across multiple ticks on
  a non-fresh-start node past the 15m deadline, the node is **lagging
  network tip** — treat this as SYNC FAILURE even when Heartbeat gap=0.
  File/comment per the Bug Filing Workflow. Do NOT downgrade it to OK
  just because heard_from_quorum=true and gap=0.

## Metrics Scan

The node exposes 152 Prometheus metrics at `http://localhost:<ADMIN_PORT>/metrics`
(39 counters, 81 gauges, 32 histograms). Each tick samples `/metrics` and
evaluates an alert catalog against it. Alerts that fire follow the
**Bug / CI-Failure Filing Workflow** below — search existing issues, comment
on a match, else file a new `ready`-labeled issue. Non-critical alerts get a
`Non-critical:` title prefix; the filing workflow is identical.

### Session state layout

Each tick persists two snapshots and a cooldown map under the session dir:

```
~/data/<session-id>/metrics/
  current.prom            # snapshot taken this tick
  prev.prom               # snapshot from previous tick (rotated from current)
  anomaly_cooldown.json   # { "<metric-name>": <unix-ts-of-last-file> }
```

Two snapshots is enough for tick-over-tick deltas on counters and histograms.
Cooldown is 2 hours per metric: if `now - last_filed < 7200s`, include the
alert in the status report but skip the file/comment step.

### Restart handling

When the node restarts, histogram/counter values reset. Rule: **if any
counter's `current` < `prev`, treat this tick's delta as `current` (not
`current - prev`)**. Also applies to histograms (bucket and count).
For the first 2 ticks after a detected restart (from check 3 or check 10),
skip `henyey_jemalloc_fragmentation_pct` gauge checks (post-restart
warmup lands near 30–45% and settles to ~18% within 10 min) and skip
counter-started-at-zero alerts.

### Alert catalog — phase 1

Three signal families, each with a specific evaluator. The machine-parseable
version lives inline in check (12) of the Loop Prompt Template below; this
table is the human reference.

**A. Counters — fire on delta threshold (delta = current - prev)**

| Metric | Delta threshold | Severity | Rationale |
|--------|-----------------|----------|-----------|
| `stellar_herder_lost_sync_total` | ≥ 1 | SYNC | Node fell out of Tracking — always a bug on a steady-state node |
| `henyey_post_catchup_hard_reset_total` | ≥ 1 | ACTION | Recovery fired |
| `henyey_recovery_stalled_tick_total` | ≥ 5 | WARN | Recovery loop not progressing |
| `stellar_ledger_apply_failure_total` | ≥ 90000 | WARN | Mainnet baseline measured ~8755/tick (~80 fails/ledger) on 2026-04-22; threshold at ~10× baseline |
| `stellar_herder_pending_too_old_total` | ≥ 100 | WARN | Pending pool rejecting stale envelopes; overlay lag |
| `stellar_overlay_timeout_idle_total` + `_straggler_total` (sum) | 5× prior-tick sum | WARN | Overlay churn burst |
| `stellar_overlay_error_read_total` + `_write_total` (sum) | ≥ 50 | WARN | Overlay I/O errors |
| `henyey_archive_cache_refresh_error_total` | ≥ 1 | NONC | Archive fetch failing |
| `henyey_archive_cache_refresh_timeout_total` | ≥ 3 | NONC | Archive fetch slow |
| `henyey_scp_post_verify_drops_total` | ≥ 2000000 | WARN | Mainnet baseline measured ~171971/tick on 2026-04-22 (broad overlay broadcast → TooOld/Invalid at ~318/s); threshold at ~10× baseline |

Mainnet closes ~120 ledgers per tick. Thresholds above for `ledger_apply_failure_total`
and `scp_post_verify_drops_total` are empirically set at ~10× the 2026-04-22 mainnet
baseline; they are blunt absolute-delta checks and may false-negative on slow, sustained
degradation. Follow-up: replace with ratio checks
(`ledger_apply_failure / (success + failure)` and `scp_post_verify_drops / scp_envelope_receive`).

**B. Gauges — fire on absolute threshold**

| Metric | Threshold | Severity | Notes |
|--------|-----------|----------|-------|
| `stellar_peer_count` | < 8 | WARN | Validator needs ≥ 8 peers for reliable quorum |
| `henyey_jemalloc_fragmentation_pct` | > 50 for two consecutive ticks | WARN | Matches existing log rule; "two ticks" filters warmup |
| `stellar_ledger_age_current_seconds` | > 30 | SYNC | Backup source for the RPC `age` check |
| `stellar_herder_state` | != 2 when uptime > 15m | SYNC | 0=bootstrap, 1=catching up, 2=synced |
| `henyey_scp_verify_input_backlog` | > 100 | WARN | SCP verifier queue growing |
| `henyey_scp_verifier_thread_state` | != 0 | WARN | 0=Running, 1=Stopping, 2=Dead (`crates/app/src/app/types.rs:310-311`); only Running is healthy |
| `stellar_herder_pending_envelopes` | > 2000 | WARN | Herder backpressure |
| `henyey_overlay_fetch_channel_depth_max` | > 500 | WARN | Overlay fetch backpressure |
| `henyey_process_open_fds` / `henyey_process_max_fds` | > 0.85 | WARN | FD exhaustion imminent |
| `henyey_herder_drift_max_seconds` | > 10 | NONC | Clock/close-time drift |

"Two consecutive ticks": alert fires only if `prev.prom` also breached.

**Intentionally not included** (these were in an earlier draft but removed after first-tick
validation showed them to be noise, not signal):

- `stellar_quorum_agree` / `stellar_quorum_missing` / `stellar_quorum_fail_at` —
  these read from `herder.quorum_health()` (`crates/herder/src/herder.rs:2554-2578`),
  which returns counts for the *tracking slot's* `QuorumInfo`. Between slot
  externalizations (i.e., most of the time when /metrics is scraped), `agree=0`
  and `missing=<all quorum nodes>` is legitimate — the slot has already moved
  on. `fail_at = total - threshold` is a config-derived constant, not a
  time-varying health signal. A useful threshold here would require a
  ledger-close-triggered snapshot, not a mid-slot scrape. Revisit when that
  instrumentation exists.

**C. Histograms — fire on p99 bucket threshold of per-tick-delta**

Per-tick histogram delta algorithm:
1. For each `<metric>_bucket{le="X"}` compute `bucket_delta[le] = current - prev`.
2. `count_delta = <metric>_count_current - <metric>_count_prev`.
3. If `count_delta < 20`: skip (too few samples).
4. Cumulative bucket delta at upper edge L is `sum(bucket_delta[le] for le <= L)`.
   Find smallest L where cumulative ≥ `0.99 * count_delta`. That L is the p99 upper bound.

Bucket edges for close-path histograms: `{0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1, 5, 30, +Inf}`
seconds (verified from live scrape).

| Metric | p99 threshold | Severity | Rationale |
|--------|---------------|----------|-----------|
| `henyey_ledger_close_cycle_seconds` | > 5s bucket | WARN | Includes inter-close wait; > 5s = close ran long |
| `henyey_ledger_close_tx_exec_seconds` | > 1s bucket | WARN | Tx execution regression |
| `henyey_ledger_close_soroban_exec_seconds` | > 1s bucket | WARN | Soroban execution regression |
| `henyey_ledger_close_commit_seconds` | > 0.5s bucket | WARN | Commit-phase regression |
| `henyey_ledger_close_soroban_state_seconds` | > 0.5s bucket | WARN | Loaded-state prep regression |
| `henyey_close_complete_tx_queue_seconds` | > 0.5s bucket | WARN | Tx-queue bookkeeping regression |

Mean check (`sum_delta / count_delta`) is a cheaper alternative; fire on
whichever breaches. Coarse bucket-upper-bound avoids false positives from
histogram bucket granularity.

### Issue body template for metric alerts

```
## Symptom
<one-line: metric name, current value, threshold>

## Evidence (L<ledger>, binary <sha>)
- Current: <metric>=<value>
- Previous tick: <metric>=<value>
- Delta: <value>
- Threshold: <threshold>

## Related metrics
<sibling gauges/counters that clarify root cause>

## Suspected root cause
<investigation — grep where the metric is registered, read the hot path>

## Suggested fix
<file:line citation>
```

Titles:
- `Non-critical: metrics: <metric-name> breached threshold (<value> > <threshold>)` for NONC.
- `metrics: <metric-name> — <short symptom>` for WARN.
- `metrics: <metric-name> — <short symptom>` for SYNC (and update `sync:` status line).

### Watcher mode

Watcher mode (`--watcher`) exposes `/metrics` on port 11727 but has no
validator/quorum/SCP state. Run check (12) with a reduced catalog:
process (`henyey_process_open_fds`, `_max_fds`), jemalloc, overlay
(`stellar_overlay_*_total`, `henyey_overlay_fetch_channel_depth_max`).
Skip SCP, quorum, herder_state, and histogram p99 alerts.

## Bug / CI-Failure Filing Workflow

Use this workflow for both node bugs and CI failures — they route
identically now (file a `ready`-labeled GH issue; no inline fix).

1. **Investigate to root cause directly in the main checkout** — read
   source code, check logs, trace code paths, grep, `git log`, read the
   running node's log file. No worktree needed: only one monitor-loop
   runs at a time (concurrency lock) and investigation is read-only.
   Document findings with file:line references.
2. **Check for an existing open issue first.** Run
   `gh issue list --search "<symptom keywords>" --state open` (and
   optionally `--state all` to catch recently-closed/regressed ones).
   Read the candidates. If one matches the same symptom/subsystem:
   - **Comment on it** via `gh issue comment <N>` with the new evidence
     (ledger number, timestamp, metric deltas, log snippet, etc.).
   - Do NOT file a duplicate.
   - If the existing issue is missing the `ready` label and the
     evidence warrants action, add it:
     `gh issue edit <N> --add-label ready`.
3. **Otherwise, file a new issue** using
   `gh issue create --label ready`. The issue body should be a
   self-contained proposal (clear symptom, evidence, suspected root
   cause, concrete fix sketch with file:line references) so the
   downstream pickup process has everything it needs. Capture the issue
   number `N` returned by `gh` for the status report.
4. **Do NOT spawn an agent.** A separate process watches for
   `ready`-labeled issues and drives them to a fix.
5. **Do NOT edit the main checkout.** All fixes go through the
   downstream pickup process — including CI failures.

The next redeploy tick (check 10) will pick up whatever lands on main —
rebuild, kill, restart. The monitor does not block on the fix.

### Recurrence: comment on existing issue by default, file new issue only when scope shifts

When a previously-filed bug recurs, the default is **comment on the
existing issue** with the new evidence. The mandatory existence check
in the Bug / CI-Failure Filing Workflow (step 2) already enforces this:
search first, comment if found.

File a NEW `ready`-labeled issue ONLY when the recurrence's evidence
points at a materially different scope than the existing issue —
specifically:

- Different named subsystem (e.g. prior issue was about SCP envelope
  emission; new evidence is about archive-publish backoff).
- Different phase/mark in the timeline (e.g. prior was `phase=2
  fetch_resp`; new is `tx_queue_background_wait_ms`).
- Different root-cause hypothesis (e.g. prior blamed compute
  backpressure; fresh telemetry rules that out and points at a lock).
- Different candidate site set (specific functions/files).

In that case, include `Related to #<prior>` + a one-line scope-diff in
the new issue body, and post a back-link comment on the prior issue so
future readers can trace the lineage.

**Comment on the existing issue** (do NOT file a duplicate) when the
recurrence is:

- Same symptom at the same site with additive incremental data (e.g.
  "reproduces at L61340500 too with the same stack signature", "also
  seen at 17:48:06Z with `stuck_duration=124s`").
- Additional instrumentation output (log lines, metric tables,
  `/proc/wchan`, stack dumps) that reinforces or refines the existing
  hypothesis without changing it.
- A reproduction on a different commit of the same bug — attach the
  new sha to the existing issue.

If the existing issue is CLOSED (a prior fix landed) but the symptom
returns, file a NEW issue — the old one describes a fixed state and
re-opening it muddles the history. Reference the closed issue with
`Related to #<prior> (closed)` and a note on why the prior fix did not
cover this case.

The monitor does not spawn agents on any issue. The downstream pickup
process watches for `ready`-labeled issues and handles the fix.

## Startup

1. **Concurrency lock.** Acquire a single-holder lock so a second
   `/monitor-loop` invocation cannot race this one on git pull / build /
   kill-restart:
   ```
   mkdir -p ~/data
   exec 9> ~/data/monitor-loop.lock
   if ! flock -n 9; then
     # Another monitor-loop is already running.
     echo "ABORT: another /monitor-loop holds ~/data/monitor-loop.lock."
     echo "Attach to it (inspect its log) or stop it first."
     exit 1
   fi
   ```
   The lock FD stays open for the lifetime of the conversation.

2. **Check if a henyey node is already running:**
   ```
   pgrep -af 'henyey.*run'
   ```
   Two branches:

   **(a) Process found — attach mode (default).** Attach silently to
   the running process; do NOT kill it. The loop is normally invoked
   from cron / `/loop` with no user available to prompt, so attaching
   is the safe default. If the user explicitly wants a fresh restart,
   they can kill the node themselves first.
   - Recover the session directory from the process's stdout fd:
     ```
     readlink /proc/<pid>/fd/1
     ```
     The result is the original `monitor.log` path; take its parent's
     parent as `<session-id>` root. Example:
     `/home/tomer/data/ab12cd34/logs/monitor.log` → `<session-id>=ab12cd34`.
   - Verify the running binary:
     ```
     readlink /proc/<pid>/exe
     ```
     Record this as the Binary line in the startup summary.
   - Skip steps 3–7 (directory creation, build, start, startup-log check)
     and go straight to step 8 with the recovered `<session-id>`.

   **(b) No process — fresh start.** Continue with step 3.

3. **Generate a session ID** (8-char random hex). All session data goes
   under `~/data/<session-id>/`.

4. **Create directories:**
   ```
   mkdir -p ~/data/<session-id>/{logs,cache,cargo-target,metrics}
   ```

5. **Build the binary** (henyey crate only, matching the redeploy step in
   the loop; rebuilding the full workspace on every startup is wasteful
   and has caused build-flag drift in the past):
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
   ```

6. **Select config based on `$MODE` and start the node in the background:**
   - **validator** (default):
     ```
     RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey \
       --mainnet run --validator \
       -c configs/validator-mainnet-rpc.toml \
       >> ~/data/<session-id>/logs/monitor.log 2>&1 &
     ```
   - **watcher**:
     ```
     RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey \
       --mainnet run \
       -c configs/mainnet.toml \
       >> ~/data/<session-id>/logs/monitor.log 2>&1 &
     ```

7. **Verify the node started.** Wait 10 seconds, then tail the last 30
   lines of the log to confirm (look for "Starting", "Catching up", or
   ledger close messages).

8. **Print a startup summary:**
   ```
   ═══ MONITOR LOOP STARTED ═══
   Session:  <session-id>
   Mode:     <validator|watcher>
   Config:   configs/<config-file>
   Binary:   <resolved binary path>
   Log:      ~/data/<session-id>/logs/monitor.log
   PID:      <pid>
   Attached: <yes|no>

   Next check in ~10 minutes via /loop.
   ════════════════════════════════
   ```

9. **Schedule the monitoring loop** by invoking `/loop` with the
   self-contained prompt below. Before calling `/loop`, substitute:
   - `<session-id>` — the session ID recovered (attach) or generated
   - `<MODE>` — `validator` or `watcher`
   - `<CONFIG>` — the config file path used at step 6
   - `<RPC_PORT>` — `8000` for validator mode; omit RPC checks in watcher
   - `<ADMIN_PORT>` — `11627` (validator) or `11727` (watcher)
   - `<RUN_FLAGS>` — `--validator` for validator mode, empty for watcher

   ```
   /loop 10m <LOOP_PROMPT>
   ```

   See the **Loop Prompt Template** section below for the full prompt text.

## Loop Prompt Template

This is the self-contained prompt passed to `/loop`. It must include all
check logic because cron jobs run in fresh contexts without access to this
skill file.

> Maintenance note: the COMMIT POLICY block inside this template is a copy
> of the one in the skill body. Keep them in sync when editing either.

```
Check the henyey mainnet monitor log at ~/data/<session-id>/logs/monitor.log.

Determine fresh-start state once up-front: if ~/data/mainnet/mainnet.db does NOT exist, set FRESH_START=yes (sync deadline = 4h). Otherwise FRESH_START=no (sync deadline = 15m). Use this when evaluating check (2).

HEALTH CHECKS:
(1) Log scan — run: tail -n 500 ~/data/<session-id>/logs/monitor.log. Scan for: hash mismatches ("hash mismatch", "HashMismatch", differing expected/actual hashes), panics/crashes ("panic", "thread.*panicked", "SIGABRT", "SIGSEGV"), ERROR-level log lines, assertion failures ("assertion failed").
(2) Ledger progression & sync deadline — persist ledger progression across ticks so STUCK can be detected by a single invocation: (a) read ~/data/<session-id>/last_ledger (if it exists) — format is "<ledger>|<unix-timestamp>". (b) extract the current ledger from the most recent Heartbeat line in the log tail. (c) if the file exists and its ledger equals the current ledger and the recorded timestamp is more than 600s old, flag STUCK. (d) if the ledger has advanced or the file is missing, overwrite ~/data/<session-id>/last_ledger with "<current-ledger>|<now>". Additionally, check node uptime: run ps -o etime= -p $(pgrep -f 'henyey.*run' | head -1). Compare uptime against the deadline from FRESH_START. If uptime exceeds the deadline and the node is not yet in real-time sync: check the latest Heartbeat for the gap between `ledger` and `latest_ext` — if gap > 5, or if RPC status is "unhealthy" (i.e. `age` > 30s, meaning the node's latest-closed ledger lags wall-clock), or if `heard_from_quorum=false`, flag as SYNC FAILURE. **"Real-time sync" means `age < 30s`, NOT just Heartbeat gap=0** — gap is the node's local view (`latest_ext - ledger`) and stays at 0 even when the node is minutes behind the network if it hasn't received those externalization messages yet. The authoritative wall-clock signal is RPC `age`. If `age` is persistently > 30s for a non-fresh-start node past the 15m deadline, the node is lagging network tip; treat as SYNC FAILURE even with gap=0 and heard_from_quorum=true. This is a bug, not a transient condition. Do NOT report it as a WARNING and wait. Investigate the catchup path: check for checkpoint-boundary stalls ("failed to download header"), hash mismatches, or event loop freezes in the log. If FRESH_START=yes and uptime is under 4h, a large gap is expected — report CATCHING UP instead of SYNC FAILURE.
(3) Process alive — run: pgrep -af 'henyey.*run'. If not running, before relaunching: (i) `rm -f ~/data/mainnet/mainnet.lock` to clear any stale lockfile the dead process left behind; (ii) preserve the prior session's log so its final lines are post-mortem-debuggable — `mv ~/data/<session-id>/logs/monitor.log ~/data/<session-id>/logs/monitor.log.crashed-$(date -u +%Y%m%dT%H%M%SZ) 2>/dev/null || true`; (iii) relaunch with **append** redirection so interleaving restart pathways don't nuke history: RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey --mainnet run <RUN_FLAGS> -c <CONFIG> >> ~/data/<session-id>/logs/monitor.log 2>&1 &
(4) Memory — run: ps -o rss= -p $(pgrep -f 'henyey.*run' | head -1) and convert to MB. If RSS > 12 GB, flag HIGH MEMORY. If RSS > 16 GB or system `available` memory (from `free -m`) < 4 GB, restart the node (kill <PID>, wait 10s, kill -9 if still alive, then relaunch as in check 3). Use the `available` column — NOT `free` — to avoid false positives from reclaimable kernel cache (buff/cache).
(5) Disk — run: df -h ~/data | tail -1. If usage > 85%, flag LOW DISK. Then clean up old rotated log archives (keep 3 most recent per category): if test -d ~/data/<session-id>/logs; then ls -1r ~/data/<session-id>/logs/monitor.log.preredeploy-* 2>/dev/null | tail -n +4 | xargs -r rm -f; ls -1r ~/data/<session-id>/logs/monitor.log.crashed-* 2>/dev/null | tail -n +4 | xargs -r rm -f; ls -1r ~/data/<session-id>/logs/monitor.log.stuck-* 2>/dev/null | tail -n +4 | xargs -r rm -f; fi. The ISO 8601 timestamp suffix sorts lexicographically, so ls -1r (reverse alphabetical) gives newest-first; tail -n +4 skips the 3 newest and outputs the rest for deletion. Report how many files were removed if any.
(6) Session disk — run: du -sh ~/data/<session-id>/ and du -sh ~/data/mainnet/. If combined > 200 GB, flag SESSION DISK HIGH.
(7) Memory report — run: grep 'Memory report summary' ~/data/<session-id>/logs/monitor.log | tail -1. If grep returns no output, flag WARNING memory-report-missing (log format may have changed). Otherwise extract jemalloc_allocated_mb, jemalloc_resident_mb, fragmentation_pct, heap_components_mb, mmap_mb, unaccounted_mb, unaccounted_sign. If fragmentation_pct > 50, flag HIGH FRAGMENTATION. If unaccounted_mb > 1000 with sign "+", note it (known jemalloc overhead, not a bug — but verify heap_components is stable; if heap_components is growing, investigate).
(8) RPC health (validator mode only — skip in watcher mode; RPC_PORT=<RPC_PORT>) — run: curl -s -X POST http://localhost:<RPC_PORT> -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'. Verify: response is non-empty, status is "healthy". Check pruning: latestLedger - oldestLedger should be <= retention_window + 250 (allows one full maintenance cycle's worth of new ledgers — maintenance every 900s ≈ 180 ledgers at 5s/ledger, +70 headroom). If gap > retention_window + 500, flag PRUNING STALLED. If RPC is not responding, flag RPC DOWN.
(9) OBSRVR Radar (validator mode only — skip in watcher mode) — get public key from: curl -s http://localhost:<ADMIN_PORT>/info (extract public_key). Then: curl -s https://radar.withobsrvr.com/api/v1/nodes/<PUBLIC_KEY>. Check: isValidating (if false and node running > 30 min, flag NOT VALIDATING), validating24HoursPercentage (if < 50 and running > 6 hours, flag LOW VALIDATION RATE), lag (if > 500, flag HIGH LAG). If API errors, emit "obsrvr: N/A (api-error)" in the status line instead of omitting the field.
(12) Metrics scan — scrape /metrics and evaluate alert catalog. (a) Ensure ~/data/<session-id>/metrics exists: mkdir -p ~/data/<session-id>/metrics. (b) Rotate: mv ~/data/<session-id>/metrics/current.prom ~/data/<session-id>/metrics/prev.prom 2>/dev/null || true. (c) Scrape: curl -s http://localhost:<ADMIN_PORT>/metrics > ~/data/<session-id>/metrics/current.prom. (d) Restart detection: for any counter, if current < prev, treat delta as current (node restarted this tick or last tick). If check (3) or (10) restarted the node this tick OR the prior tick, skip jemalloc_fragmentation_pct gauge alert and skip counter-started-at-zero alerts (warmup allowance — 2 ticks).

(e) Evaluate METRIC ALERT CATALOG against current.prom (and deltas vs prev.prom):

COUNTERS (delta ≥ threshold): stellar_herder_lost_sync_total ≥1 SYNC; henyey_post_catchup_hard_reset_total ≥1 ACTION; henyey_recovery_stalled_tick_total ≥5 WARN; stellar_ledger_apply_failure_total ≥90000 WARN (baselined ~8755/tick 2026-04-22, threshold ~10× baseline); stellar_herder_pending_too_old_total ≥100 WARN; (stellar_overlay_timeout_idle_total + stellar_overlay_timeout_straggler_total) 5× prior-tick-sum WARN; (stellar_overlay_error_read_total + stellar_overlay_error_write_total) ≥50 WARN; henyey_archive_cache_refresh_error_total ≥1 NONC; henyey_archive_cache_refresh_timeout_total ≥3 NONC; henyey_scp_post_verify_drops_total ≥2000000 WARN (baselined ~172k/tick 2026-04-22, threshold ~10× baseline).

GAUGES (absolute threshold on current): stellar_peer_count <8 WARN; henyey_jemalloc_fragmentation_pct >50 on two consecutive ticks WARN; stellar_ledger_age_current_seconds >30 SYNC; stellar_herder_state !=2 when uptime >15m SYNC; henyey_scp_verify_input_backlog >100 WARN; henyey_scp_verifier_thread_state !=0 WARN (0=Running, 1=Stopping, 2=Dead; only Running is healthy); stellar_herder_pending_envelopes >2000 WARN; henyey_overlay_fetch_channel_depth_max >500 WARN; (henyey_process_open_fds / henyey_process_max_fds) >0.85 WARN; henyey_herder_drift_max_seconds >10 NONC. [quorum_agree / quorum_missing / quorum_fail_at intentionally NOT monitored — they snapshot the tracking slot's QuorumInfo and return false-positive noise between externalizations; see skill body.]

HISTOGRAMS (p99 bucket of per-tick delta): for each histogram H, compute bucket_delta[le] = H_bucket_current[le] - H_bucket_prev[le], count_delta = H_count_current - H_count_prev. Skip if count_delta <20. Cumulative bucket delta at upper edge L = sum(bucket_delta[le] for le <= L); smallest L where cumulative ≥ 0.99 * count_delta is the p99 upper bound. Thresholds: henyey_ledger_close_cycle_seconds p99 >5s WARN; henyey_ledger_close_tx_exec_seconds p99 >1s WARN; henyey_ledger_close_soroban_exec_seconds p99 >1s WARN; henyey_ledger_close_commit_seconds p99 >0.5s WARN; henyey_ledger_close_soroban_state_seconds p99 >0.5s WARN; henyey_close_complete_tx_queue_seconds p99 >0.5s WARN. Mean check (sum_delta/count_delta) is cheaper — fire on whichever breaches.

(f) For each firing alert: read ~/data/<session-id>/metrics/anomaly_cooldown.json (create if missing). If last-filed-for-this-metric is within 7200s (2h), include in status report but SKIP file/comment. Otherwise follow BUG FILING WORKFLOW (step 3 below): search `gh issue list --search "metrics: <metric-name>" --state open`, comment on match (with new evidence) and ensure `ready` label, else `gh issue create --label ready` with title `Non-critical: metrics: <metric>` (NONC tier) or `metrics: <metric> — <symptom>` (WARN/SYNC tier) and body including: current/prev values, delta, threshold, ledger, binary sha, related sibling metrics, a file:line citation from `grep -n "<metric_name_without_prefix>" crates/ -r`, and a suggested fix. On fire, update anomaly_cooldown.json with {"<metric>": <now>}. For SYNC-tier alerts, ALSO update the sync: line in the status report (not just metrics:).

(g) Watcher mode (--watcher): run with reduced catalog — only process (open_fds, max_fds), jemalloc, and overlay counters/gauges. Skip SCP, quorum, herder_state, and histogram p99 alerts.

REMOTE SYNC & REDEPLOY:
(10) Remote sync — first sanity-check the working tree: (pre-a) if git status --porcelain reports any output, ABORT the deploy path for this tick — there are local edits that shouldn't exist (the monitor never edits the checkout). Report: DEPLOY SKIPPED (dirty tree) with the list of dirty paths. Do not run git pull against a dirty tree; do not kill the node. Investigate the dirty tree before the next tick. (a) If clean, run: git fetch origin main. If in detached HEAD state (git symbolic-ref HEAD fails), run git checkout main first. Then compare: git rev-parse HEAD vs git rev-parse origin/main. If they differ (origin/main is ahead): (b) check CI status on origin/main — run: gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'. If any recent run has conclusion "failure", do NOT deploy — route the failure through check 11 (file/comment a `ready`-labeled issue) and wait. (c) If all conclusions are "success" (ignore "" for in-progress and "cancelled"): git pull --rebase, (d) CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey, (e) if build succeeds: preserve the log — `mv ~/data/<session-id>/logs/monitor.log ~/data/<session-id>/logs/monitor.log.preredeploy-$(date -u +%Y%m%dT%H%M%SZ) 2>/dev/null || true`; then kill the node (kill <PID>, wait 10s, kill -9 if still alive), restart with the launch command from check (3)(iii) (append redirection), report: DEPLOY — pulled <N> commits (<old-sha>..<new-sha>), rebuilt, restarted at L<ledger>, (f) if build fails: report BUILD FAILED, do NOT restart — the old binary is still running. Route the build error through check 11 (file/comment a `ready`-labeled issue). If HEAD == origin/main: no action (already up to date).

COMMIT POLICY (must match the skill body; update both together):
- The monitor does NOT commit code. All fixes — node bugs and CI failures — are delegated via `gh issue` with the `ready` label (or a comment on an existing issue). A separate downstream process picks up `ready`-labeled issues and commits the fix.

FIX-ROUTING POLICY (must match the skill body's Fix-Routing Policy section):
- All fixes (both CI failures and node bugs) are DELEGATED via `gh issue --label ready`. The monitor never implements a fix inline.
- ALWAYS check for an existing open issue first via `gh issue list --search "<keywords>" --state open` before creating a new one. If one covers the same symptom/subsystem, comment on it with the new evidence and (if missing) add the `ready` label via `gh issue edit <N> --add-label ready`. Only create a new issue when no existing one matches.
- Do NOT spawn agents, open worktrees, review diffs, or cherry-pick. The next redeploy tick (check 10) will pick up whatever lands on main.

DEPLOY REGRESSION POLICY:
If the node fails after a deploy: (a) file or comment on a `ready`-labeled GitHub issue with the regression details (commit range, symptoms, WATCHDOG data), (b) restart the node on the last known-good binary (rebuild from the previous commit) while waiting for the fix. Do NOT revert commits inline — let the downstream pickup process handle it.

CI CHECK WORKFLOW:
(11) CI check — scope and levels of detection:
  (11a) Scope: only inspect workflows that run on branch main. Run: gh run list --branch main --limit 10 --json databaseId,name,status,conclusion,headSha,createdAt --jq '.[] | "\(.name)|\(.status)|\(.conclusion)|\(.headSha[:8])|\(.databaseId)|\(.createdAt)"'. Ignore runs triggered by PRs on other branches. Scan for completed runs with conclusion "failure".
  (11b) Job-level (CRITICAL — catches continue-on-error failures): For the latest completed run of EACH distinct workflow name (e.g. ci, quickstart, verify-execution, history-publish — enumerate dynamically from 11a, do NOT hard-code "Quickstart"), check individual jobs: gh run view <ID> --json jobs --jq '.jobs[] | select(.conclusion == "failure") | "\(.name)|\(.conclusion)"'. Workflows with continue-on-error jobs report run-level conclusion "success" even when jobs fail — you MUST check job-level conclusions. If any jobs have conclusion "failure", treat it the same as a run-level failure.
REPORTING RULE — NEVER report "ci: all green" if ANY job has conclusion "failure", even if the run-level conclusion is "success". The ci: line in the status report MUST reflect the WORST job-level result across all workflows. A continue-on-error job failure is NOT "green" — it is RED. Do not qualify failures as "known", "pre-existing", or "cosmetic".
Compare createdAt with current UTC time (date -u +%Y-%m-%dT%H:%M:%SZ) — only act on failures from the last 2 hours. For each failure: (a) gh run view <ID> --log-failed 2>&1 | tail -80, (b) categorize: build error, test failure, timeout, flaky, infrastructure, (c) **check for an existing open issue** via `gh issue list --search "<workflow name + signature>" --state open`. If one matches, `gh issue comment <N>` with the new evidence (sha, log snippet, timestamp) and ensure it has the `ready` label (`gh issue edit <N> --add-label ready`). (d) Otherwise, file a new issue: `gh issue create --label ready --title "<workflow>: <short signature>" --body "..."` with investigation findings. Do NOT commit a fix. Report: CI ISSUE FILED — <workflow> failed on <sha>, filed/commented #<N>.

INVESTIGATION: For ANY anomaly, investigate to root cause — read source code, check logs, trace code paths. Never dismiss as "expected". Produce a `ready`-labeled GitHub issue (or comment on an existing one) for every anomaly that isn't immediately explained.

BUG FILING WORKFLOW (applies to both node bugs and CI failures): (1) identify the failing signature (ledger + error type for node bugs; workflow + job + error type for CI), (2) investigate to root cause — read source code, trace code paths, (3) **check for an existing open issue** via `gh issue list --search "<keywords>" --state open`; if a match exists, **verify its state is OPEN** (via `gh issue view <N> --json state -q .state` — the issue could have closed between a prior tick and this one) and then `gh issue comment <N>` with the new evidence and ensure the `ready` label is set, and STOP here. (4) If no OPEN match, file a new issue using `gh issue create --label ready` with a self-contained proposal body (symptom, evidence, suspected root cause, fix sketch with file:line references) so the downstream pickup process has everything it needs. (5) Do NOT spawn agents. Do NOT edit the main checkout. The next redeploy tick (check 10) will pick up whatever lands on main.

RECURRENCE POLICY: If a previously-filed bug recurs with material new evidence, **prefer commenting on the existing issue** when it is the same bug at the same site AND the issue is OPEN — most recurrences land here. **If the prior issue is CLOSED**, file a new `ready`-labeled issue with `Related to #<prior> (closed)` in the body and a note on why the prior fix did not cover this case; do NOT comment on the closed issue (closed issues describe a fixed state; adding recurrence comments muddles that history and doesn't route to pickup). Also file a new issue (still referencing `Related to #<prior>` with one-line scope-diff) when new evidence points at a different named subsystem, a different phase/mark, a different root-cause hypothesis, or a different candidate site set. Do NOT open a duplicate of the same bug at the same site when the prior is OPEN.

OUTPUT: Print a multiline status report:
MONITOR <OK|WARNING|ACTION> — L<ledger> — <timestamp>
  node:   mode=<MODE> session=<session-id> pid=<PID> fresh_start=<yes|no>
  sync:   <synced | CATCHING UP (gap=N, uptime=Xm, deadline=<15m|4h>) | SYNC FAILURE (gap=N, uptime=Xm — filed/commented #<N>)>
  mem:    <RSS_MB>MB rss | alloc=<alloc>MB resident=<resident>MB frag=<pct>%
          heap=<heap>MB mmap=<mmap>MB unaccounted=<sign><unaccounted>MB
  disk:   <used>/<total> (<pct>%) | session+data=<size>
  rpc:    <healthy|unhealthy|N/A> oldestL=<X> latestL=<Y> window=<Z>
  obsrvr: <validating=<Y/N> val24h=<pct>% lag=<N> | N/A (watcher) | N/A (api-error)>
  metrics: <clean | N alerts (<metric1>,<metric2>,...) — filed/commented #<N>,#<M> | N alerts, K suppressed by cooldown>
  deploy: <up-to-date | pulled N commits (old..new) | SKIPPED (dirty-tree|ci-red|build-failed, filed/commented #<N>)>
  ci:     <all green (run+job level) | WORKFLOW failed — filed/commented #<N> | WORKFLOW jobs FAILED (continue-on-error) — NAME|conclusion listed, filed/commented #<N>>
Use WARNING for threshold breaches. Use ACTION when a corrective action was taken (restart, deploy, filed a new issue, commented on an existing issue). Use SYNC FAILURE (not WARNING) when the node has exceeded the active sync deadline (15m populated / 4h fresh-start) but is not closing ledgers in real-time — this is a bug that requires immediate investigation AND filing/commenting on a `ready`-labeled issue.
```

## Resource Investigation

When a memory or disk alert is triggered, investigate before taking action.

### High Memory (RSS > 12 GB)

Note: With jemalloc as the default allocator, a mainnet validator's
steady-state RSS is typically 7-9 GB (allocated ~5.2 GB, ~30-40%
fragmentation). Major components: module_cache (~1.9 GB), offers (~0.9 GB),
soroban_data (~0.5 GB), bucket_list_heap (~0.5 GB), soroban_code (~0.5 GB).
These are fundamental data structures, not leaks.

1. **Collect details**:
   ```
   ps -o rss=,vsz=,etime= -p <PID>
   ```
   Check whether RSS is still growing by comparing with the previous
   check, or sample twice 60 seconds apart.

   Also check the jemalloc memory report trend from the log:
   ```
   grep 'Memory report summary' ~/data/<session-id>/logs/monitor.log | tail -5
   ```
   This shows jemalloc allocated vs resident (fragmentation) over time.

2. **Check for a leak**: If RSS has grown by more than 1 GB since the
   last check (or is consistently growing across multiple checks), this
   likely indicates a memory leak. Growth during the first 30 minutes
   after startup is expected (cache warmup, bucket loading).

3. **Capture diagnostic info**:
   ```
   cat /proc/<PID>/status | grep -E 'VmRSS|VmPeak|VmSwap|Threads'
   cat /proc/<PID>/smaps_rollup
   ```
   Also check per-component memory breakdown:
   ```
   grep 'Memory report component' ~/data/<session-id>/logs/monitor.log | tail -20
   ```
   This shows which component (module_cache, offers, soroban_data, etc.)
   is consuming the most memory and whether any component is growing.
   Record the latest ledger number and uptime. Check the log for any
   unusual patterns around the time memory started growing (e.g., large
   transaction sets, merge activity, catchup).

4. **If the cause isn't obvious** from process stats and logs, read the
   source code for the hot path (ledger close, Soroban execution, bucket
   merges) to understand allocation patterns. Profile or trace what's
   consuming memory rather than guessing.

5. **If RSS exceeds 16 GB or available system memory is < 4 GB**:
   This is critical. Restart the node to prevent OOM kill:
   - Kill the process gracefully (`kill <PID>`, wait 10s, then
     `kill -9` if needed). This 10s window must match the loop prompt's
     kill timing in check (10e).
   - Restart with the same command from Startup step 6.
   - Report: `RESOURCE ACTION — restarted node due to memory pressure
     (RSS was <X> GB at L<ledger>)`.

6. **If RSS is between 12-16 GB and stable (not growing)**: Flag it but
   do not restart. Report: `RESOURCE WARNING — RSS <X> GB at L<ledger>,
   stable — monitoring`.

### Session Disk Growth (> 200 GB total)

This tracks disk consumed specifically by this monitor session and its
mainnet data, independent of other processes on the machine.

1. **Measure**:
   ```
   du -sh ~/data/<session-id>/
   du -sh ~/data/mainnet/
   ```
   Sum both for the total. The mainnet data directory holds the
   database, buckets, and history — it is the primary growth driver.

2. **Drill into the mainnet data directory**:
   ```
   du -sh ~/data/mainnet/*/ | sort -rh | head -10
   ls -lhS ~/data/mainnet/buckets/ | head -20
   ```
   Check whether bucket files are accumulating (old permanent buckets
   not being cleaned up after merges) or the database is growing
   unexpectedly.

3. **If growth is unexpected**, read the source code that writes to the
   growing directory to understand what's being written and whether cleanup
   is implemented. For example, if buckets are accumulating, read the merge
   and cleanup code to verify old buckets are being removed after merges.

4. **Safe cleanup within the session**:
   - Incremental build caches:
     `rm -rf ~/data/<session-id>/cargo-target/release/incremental`
   - Old verify-execution cache entries (if any):
     `du -sh ~/data/<session-id>/cache/ && ls ~/data/<session-id>/cache/ | head`
     Remove cache entries older than 7 days if the cache exceeds 10 GB.
   - Check for stale `.tmp` or partial bucket files in the mainnet
     data directory.

5. **Report**: Include the breakdown in the monitor line. If growth is
   abnormal (e.g., buckets directory doubled), report:
   `RESOURCE WARNING — session+data disk at <X> GB (buckets: <Y> GB,
   db: <Z> GB) — investigating`.

### Pruning Stalled

(Validator mode only — watcher mode does not run RPC or maintenance.)

When the gap between `latestLedger` and `oldestLedger` significantly exceeds
the configured `retention_window` (check the `[rpc]` section in the config
for the exact value; default is 360 ledgers):

1. **Check if maintenance is running** — look for "Performing database
   maintenance" or "Maintenance complete" in recent logs:
   ```
   grep -i 'maintenance' ~/data/<session-id>/logs/monitor.log | tail -20
   ```

2. **Check if maintenance is taking too long** — look for "Maintenance took
   too long" warnings in the log.

3. **Read the maintenance/pruning source code** to understand what is
   actually being pruned. Search for the maintenance handler, trace what
   `count` controls, and verify that RPC data (ledger entries, transaction
   results, events) is included in the pruning path — not just ledger
   headers or other data. If the maintenance code doesn't prune RPC data,
   that's a bug or missing feature to fix.
   ```
   grep -r 'maintenance\|prune\|retention' crates/ --include='*.rs' -l
   ```
   Read the relevant files to understand the pruning pipeline.

4. **Check DB size**:
   ```
   ls -lh ~/data/mainnet/mainnet.db
   ```

5. **Trigger manual maintenance** if it hasn't run recently:
   ```
   curl -X POST http://localhost:11627/maintenance?count=100000
   ```

6. **If the gap keeps growing** after manual maintenance, the `count`
   parameter may be too low for the data volume. Increase it in the config
   or trigger with a higher count:
   ```
   curl -X POST http://localhost:11627/maintenance?count=500000
   ```

7. **Re-check** after manual maintenance:
   ```
   curl -s -X POST http://localhost:8000 -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'
   ```
   Verify the gap is decreasing. If not, report:
   `RESOURCE WARNING — pruning stalled, gap=<N> after manual maintenance`.
   Revisit the source code findings from step 3 to determine if pruning
   covers all necessary data types.

### OBSRVR Not Validating

(Validator mode only.)

When the OBSRVR Radar API reports `isValidating: false` or low
`validating24HoursPercentage` despite the node running and closing ledgers:

1. **Check local SCP status** — verify the node is emitting EXTERNALIZE:
   ```
   curl -s 'http://localhost:11627/scp?limit=1'
   ```
   Verify: `is_externalized: true`, `fully_validated: true`, and
   `ballot_phase: "Externalize"` for the tracking slot. If
   `fully_validated` is `false`, the node is not broadcasting its
   EXTERNALIZE envelopes — investigate the SCP envelope emission path
   in `crates/scp/src/ballot/envelope.rs` (`send_latest_envelope`).

2. **Check heartbeat for EXTERNALIZE counts**:
   ```
   grep 'Heartbeat' ~/data/<session-id>/logs/monitor.log | tail -5
   ```
   Look at `scp_sent_ext` — this should be incrementing roughly once
   per ledger close (~5 seconds). If it's 0 or not growing, the node
   is not emitting EXTERNALIZE envelopes.

3. **Check broadcast logs**:
   ```
   grep 'Broadcast SCP.*EXTERNALIZE' ~/data/<session-id>/logs/monitor.log | tail -10
   ```
   If no EXTERNALIZE broadcasts appear, check whether the node is
   fast-forwarding all slots (via `force_externalize`) instead of
   participating in real-time consensus. Fast-forwarded slots do not
   emit EXTERNALIZE — the node must process the tracking slot through
   the normal SCP ballot protocol.

4. **Check peer connectivity** — the OBSRVR crawler connects as an
   overlay peer and listens for SCP messages. Verify the node has
   inbound peers and is advertising the correct port:
   ```
   grep 'Heartbeat' ~/data/<session-id>/logs/monitor.log | tail -1
   ```
   Check `peers` count. If 0, the node can't broadcast to anyone.

5. **Check lag** — if `lag` from the OBSRVR API is very high (>1000),
   the node may be too far behind to participate in real-time consensus.
   Check whether ledger close is keeping up with the network.

### Low Disk (> 85% usage)

1. **Identify large consumers**:
   ```
   du -sh ~/data/*/ | sort -rh | head -10
   ```
   Then drill into the largest directories:
   ```
   du -sh ~/data/<largest-dir>/*/ | sort -rh | head -10
   ```

2. **Safe cleanup candidates** (delete without asking):
   - Cargo build artifacts from old sessions:
     `~/data/<old-session-id>/cargo-target/` where the session is not
     the current one and no henyey process is using that binary.
   - Stale cache directories from old sessions:
     `~/data/<old-session-id>/cache/` (same criteria).
   - Old log files: `~/data/<old-session-id>/logs/` (same criteria).
   - Rust incremental build caches: `find ~/data/*/cargo-target -name
     incremental -type d` — these can be safely removed.
   Before deleting, verify no running process references the directory:
   `ls -la /proc/*/exe 2>/dev/null | grep <session-id>`.

3. **Investigate if no obvious cleanup**: If disk usage is high and
   there are no stale sessions to clean, check whether the mainnet data
   directory (`~/data/mainnet/` or similar) is growing unexpectedly:
   ```
   du -sh ~/data/mainnet/*/  | sort -rh | head -10
   ls -lhrt ~/data/mainnet/buckets/ | tail -20
   ```
   Look for bucket files that are unreasonably large or accumulating.
   Check log files that may be growing unbounded.

4. **If disk exceeds 95%**: This is critical. Perform safe cleanup
   immediately (step 2). If still above 95% after cleanup, report to
   the user: `RESOURCE CRITICAL — disk at <pct>% after cleanup,
   manual intervention needed`.

5. **Report**: After any cleanup, report what was removed and the new
   disk usage percentage.

## Teardown

When stopping (user interrupts):
1. Kill the henyey process gracefully (`kill <PID>`, wait 10s, `kill -9`
   if still alive).
2. **Explicitly cancel the `/loop` schedule.** `/loop` persists across
   conversation turns and will not auto-cancel on teardown — if left
   running it will keep trying to tail a log whose process you just
   killed and will restart the node on the next tick. Cancel the
   scheduled wakeup / recurring job through the loop-management
   interface the user set it up with (for `ScheduleWakeup`-based loops,
   simply do not re-schedule; for CronCreate-based loops, call
   `CronDelete` on the registered trigger).
3. **Release the concurrency lock** by letting the shell close fd 9
   (it closes automatically on exit; no explicit step required unless
   the lock FD was kept alive by a spawned subshell).
4. Print a final status: uptime, latest ledger seen, issues filed/commented.
5. Do NOT remove logs or cache — they may be useful for debugging.

The monitor does not spawn agents. Node-bug issues filed with the `ready`
label persist in GitHub and are picked up by a separate downstream
process regardless of whether the monitor is still running.

## Guidelines

- Always build with `--release` — debug builds are too slow for mainnet.
- All henyey issues are in scope: mainnet bugs, testnet parity bugs, CI
  failures, performance regressions, infrastructure problems.
- **All fixes are delegated** via `gh issue create --label ready` (or a
  comment on an existing issue). The monitor's role is to detect,
  investigate, and file — never to commit, push, review, merge, or
  spawn fix agents. A separate downstream process consumes
  `ready`-labeled issues.
- **Always search for an existing issue before filing** — see the
  Bug / CI-Failure Filing Workflow.
