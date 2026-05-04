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
  The heartbeat event `gap = latest_ext - ledger` is the node's *local* view —
  gap=0 only says the node has closed everything *it has observed* as
  externalized. The node can be behind network tip by many ledgers while
  still reporting gap=0 if it hasn't received those externalization messages
  yet (or is processing them slower than the network produces them). The
  authoritative wall-clock signal is RPC `getHealth` `age` (= `now -
  close_time` of the latest-closed ledger). Stellar mainnet closes every
  ~5s, so a healthy validator always sits in the 0-20s age band. If `age`
  is persistently > 30s (RPC `status=unhealthy`) across multiple ticks on
  a non-fresh-start node past the 15m deadline, the node is **lagging
  network tip** — treat this as SYNC FAILURE even when heartbeat event gap=0.
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
| `henyey_recovery_stalled_tick_total{reason="forcing_catchup_behind"}` | ≥ 1 | WARN | Recovery forced catchup while behind consensus (Form 2 labeled extraction; excludes `backoff_active` and `forcing_catchup_not_behind`) |
| `stellar_overlay_timeout_idle_total` + `_straggler_total` (sum) | 5× prior-tick sum | WARN | Overlay churn burst |
| `stellar_overlay_error_read_total` + `_write_total` (sum) | ≥ 50 | WARN | Overlay I/O errors |
| `henyey_archive_cache_refresh_error_total` | ≥ 1 | NONC | Archive fetch failing |
| `henyey_archive_cache_refresh_timeout_total` | ≥ 3 | NONC | Archive fetch slow |

**D. Ratio checks — fire on sustained ratio breach (3 consecutive ticks)**

These ratio-based checks replace the earlier absolute-delta thresholds for
`stellar_ledger_apply_failure_total`, `henyey_scp_post_verify_drops_total`,
and `stellar_herder_pending_too_old_total`, which were fragile (false-negative
on slow degradation, baseline drift with traffic volume). Ratio checks are
traffic-proportional and self-calibrating.

| Check | Numerator | Denominator | Threshold | Min denom delta | Severity | Rationale |
|-------|-----------|-------------|-----------|-----------------|----------|-----------|
| SCP post-verify acceptance rate | `delta(henyey_scp_post_verify_total{reason="accepted"})` + `delta(henyey_scp_post_verify_total{reason="processed_directly"})` | `sum delta(henyey_scp_post_verify_total{reason="..."})` across all 13 labels | < 0.05 (less than 5% accepted) for 3 ticks | 500 | WARN (→ Bug Filing) | Baseline acceptance ~10-20%; <5% sustained means almost nothing reaches SCP |
| Transaction apply failure rate | `delta(stellar_ledger_apply_failure_total)` | `delta(stellar_ledger_apply_failure_total)` + `delta(stellar_ledger_apply_success_total)` | > 0.50 (over 50% fail) for 3 ticks | 200 | WARN (investigate) | Normal bad-tx traffic is <50%; sustained >50% suggests apply-engine bug |
| Pending too-old rate | `delta(stellar_herder_pending_too_old_total)` | `delta(stellar_herder_pending_received_total)` | > 0.50 (over 50% too old) for 3 ticks | 100 | WARN | Overlay lag or stale-envelope flood; sustained >50% means most incoming envelopes reference already-closed slots |

**Ratio check skip conditions** (skip all ratio checks when any is true):
- `FRESH_START=yes` (replaying history)
- heartbeat event `gap` > 5 (catching up)
- `stellar_ledger_age_current_seconds > 30` (not in real-time sync — works in both validator and watcher modes)
- Process uptime < 10 minutes (warmup)
- `/metrics` fetch fails
- `/metrics` returns "recorder not installed"
- Any required counter missing or invalid
- Post-verify label set ≠ expected 13 labels (`invalid_sig`, `panic`, `drift_range`, `drift_close_time`, `drift_cannot_receive`, `self_message`, `non_quorum`, `buffered`, `duplicate`, `too_far`, `buffer_full`, `processed_directly`, `accepted`)

On any skip: empty the ratio snapshot, reset all breach streak counters to 0.

**Ratio snapshot** persisted at `~/data/<session-id>/metrics/ratio_snapshot`:
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
Invalidate on PID/start_ticks change, malformed snapshot, or counter reset (current < previous).

**SCP denominator rationale:** Includes all 13 post-verify outcomes (not just errors). Normal
outcomes (`duplicate`, `buffered`, `non_quorum`, `self_message`) appear at healthy-state
proportions. The ratio measures "of everything that went through post-verify, what fraction
reached SCP?" The 5% threshold is well below the healthy ~10-20% baseline. The older
`henyey_scp_post_verify_drops_total` counter is NOT used — it counts only
`EnvelopeState::{TooOld, Invalid, InvalidSignature}`, a subset of non-accepted outcomes.

**Apply-failure policy:** When the alert fires, investigate in the same tick. If evidence points
to a henyey apply-engine bug, file/comment via Bug Filing Workflow. If expected bad-tx traffic,
report as WARNING without filing.

**Thresholds are provisional** — tune after 1-2 weeks of production data.

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
| `henyey_overlay_fetch_channel_depth` | > 500 for two consecutive ticks | WARN | Overlay fetch backpressure (live gauge, not monotonic `_max`) |
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
| `henyey_ledger_close_handle_complete_seconds` | > 0.5s bucket | WARN | Close-complete processing regression (metadata, persist-input prep, finalizer dispatch, overlay, tx queue) |
| `henyey_ledger_close_dispatch_to_join_seconds` | > 5s bucket | WARN | Ledger close spawn_blocking wall-clock + tokio scheduler delay regression |
| `henyey_ledger_close_post_complete_seconds` | > 0.5s bucket | WARN | Post-close lifecycle work regression (persist pipeline, history publish, consensus trigger, SCP/fetch drains) |
| `henyey_ledger_close_tx_exec_seconds` | > 1s bucket | WARN | Tx execution regression |
| `henyey_ledger_close_soroban_exec_seconds` | > 1s bucket | WARN | Soroban execution regression |
| `henyey_ledger_close_commit_seconds` | > 0.5s bucket | WARN | Commit-phase regression |
| `henyey_ledger_close_soroban_state_seconds` | > 0.5s bucket | WARN | Loaded-state prep regression |
| `henyey_ledger_close_complete_tx_queue_seconds` | > 0.5s bucket | WARN | Tx-queue bookkeeping regression |

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
(`stellar_overlay_*_total`, `henyey_overlay_fetch_channel_depth`).
Skip SCP, quorum, herder_state, histogram p99 alerts, and ratio checks.

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
   - **Handle `(deleted)` paths**: If `readlink` returns a path with
     ` (deleted)` suffix (e.g., `/home/tomer/data/ab12cd34/logs/monitor.log (deleted)`),
     the session directory was wiped while the process was running.
     Use the shared library function:
     ```bash
     source "$(git rev-parse --show-toplevel)/scripts/lib/monitor-decisions.sh"
     proc_stdout=$(readlink /proc/<pid>/fd/1)
     session_id=$(recover_session_from_stdout "$HOME/data" "$proc_stdout") || {
       echo "ERROR: could not recover session-id from stdout path"
       exit 1
     }
     ```
     The `recover_session_from_stdout` function (from `scripts/lib/monitor-decisions.sh`):
     - Detects `(deleted)` suffix, emits warning to stderr, strips suffix, extracts session-id
     - Creates `{logs,cache,cargo-target,metrics}` subdirs and `.alive` for deleted paths
     - For normal paths: just extracts session-id with no side effects
     - Returns 1 on malformed input (no `/data/<segment>/` pattern found)
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
   touch ~/data/<session-id>/.alive
   ```

5. **Build the binary** (henyey crate only, matching the redeploy step in
   the loop; rebuilding the full workspace on every startup is wasteful
   and has caused build-flag drift in the past):
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey
   ```
   Immediately after the build succeeds, persist the deployed sha (atomic
   write via tmp + mv) to seed the `build_sha` cache for `monitor-tick`
   check 10. Note: the authoritative source of the deployed commit is the
   running binary's `/info.commit_hash` endpoint; this file is a cache that
   monitor-tick will self-heal if it ever disagrees with the runtime value.
   ```
   new_sha=$(git rev-parse HEAD)
   printf '%s\n' "$new_sha" > ~/data/<session-id>/build_sha.tmp
   mv ~/data/<session-id>/build_sha.tmp ~/data/<session-id>/build_sha
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

8. **Write the tick env file** so `/monitor-tick` can source its runtime
   config on every invocation. Write atomically (temp file + rename):
   ```bash
   cat > ~/data/monitor-loop.env.tmp <<EOF
   MONITOR_SESSION_ID=<session-id>
   MONITOR_MODE=<validator|watcher>
   MONITOR_CONFIG=<config-path>
   MONITOR_ADMIN_PORT=<11627 for validator, 11727 for watcher>
   MONITOR_RPC_PORT=<8000 for validator, empty for watcher>
   MONITOR_RUN_FLAGS=<--validator for validator, empty for watcher>
   MONITOR_BINARY=~/data/<session-id>/cargo-target/release/henyey
   EOF
   mv ~/data/monitor-loop.env.tmp ~/data/monitor-loop.env
   ```

9. **Print a startup summary** and cron setup instructions:
   ```
   ═══ MONITOR LOOP STARTED ═══
   Session:  <session-id>
   Mode:     <validator|watcher>
   Config:   configs/<config-file>
   Binary:   <resolved binary path>
   Log:      ~/data/<session-id>/logs/monitor.log
   PID:      <pid>
   Attached: <yes|no>
   Env:      ~/data/monitor-loop.env

   To run ticks every 20 minutes via an external cron (recommended —
   survives across Claude sessions), add this crontab line:
     3,23,43 * * * * cd /home/tomer/henyey-1 && claude -p '/monitor-tick' >> /tmp/monitor-tick.log 2>&1

   Or, to run ticks inside this Claude session only:
     /loop 20m /monitor-tick
   ════════════════════════════════
   ```

   The tick logic lives in the `/monitor-tick` skill (self-contained,
   reads `~/data/monitor-loop.env` for runtime config). Do NOT embed the
   tick prompt in a `/loop` invocation here — the `/monitor-tick` skill
   is the single source of truth.

## Tick skill

The tick logic lives in the separate `/monitor-tick` skill
(`.claude/skills/monitor-tick/SKILL.md`). `/monitor-tick` is self-contained:
it sources `~/data/monitor-loop.env` at each invocation, runs the health
checks, scrapes `/metrics`, evaluates the alert catalog, handles deploy
and CI check, and prints the status report.

Single source of truth: **all tick logic lives in `/monitor-tick`**. No
tick logic should be copy-pasted into this file or into cron prompts.

Invocation options:

- **External cron** (recommended — survives across Claude sessions):
  ```
  */10 * * * * cd /home/tomer/henyey-1 && claude -p '/monitor-tick' >> /tmp/monitor-tick.log 2>&1
  ```
- **Claude session loop** (dies when Claude exits):
  ```
  /loop 10m /monitor-tick
  ```

For the authoritative catalog (thresholds, metric names, rules), see
`/monitor-tick/SKILL.md`. The human-readable reference catalog in
[Metrics Scan](#metrics-scan) above is informational; the live catalog
is in the tick skill.

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
   grep -E 'memory_report=true|Memory report summary' ~/data/<session-id>/logs/monitor.log | tail -5
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
   grep_heartbeat_lines ~/data/<session-id>/logs/monitor.log 5
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
   grep_heartbeat_lines ~/data/<session-id>/logs/monitor.log 1
   ```
   Check `peers` count. If 0, the node can't broadcast to anyone.

5. **Check lag** — if `lag` from the OBSRVR API is very high (>1000),
   the node may be too far behind to participate in real-time consensus.
   Check whether ledger close is keeping up with the network.

### Low Disk (> 85% usage)

> ⚠️ **NEVER delete `~/data/mainnet/` or any of its contents during disk
> cleanup.** This is the validator's live persisted state (database + buckets).
> Only the (3a) Repeated-FATAL state-wipe trigger in monitor-tick may remove
> it. Disk reclamation targets old session subtrees only.

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
     `~/data/<old-session-id>/cargo-target/` where the session passes the
     three-layer guard below.
   - Stale cache directories from old sessions:
     `~/data/<old-session-id>/cache/` (same criteria).
   - Old log files: `~/data/<old-session-id>/logs/` (same criteria).
   - Rust incremental build caches: `find ~/data/*/cargo-target -name
     incremental -type d` — these can be safely removed (apply layer 1 only).

   **Three-layer cleanup guard** — apply before deleting any subtree of a
   candidate session directory. Use the shared library function:
   ```bash
   source "$(git rev-parse --show-toplevel)/scripts/lib/monitor-decisions.sh"
   active_session=$(grep '^MONITOR_SESSION_ID=' ~/data/monitor-loop.env 2>/dev/null | cut -d= -f2)
   result=$(cleanup_guard "$HOME/data" "/proc" "$candidate" "$active_session" 3600)
   if [[ "$result" != "PASS" ]]; then
     echo "$result"
     continue
   fi
   # All guards passed — safe to delete subtrees
   ```

   The `cleanup_guard` function (from `scripts/lib/monitor-decisions.sh`):
   - Layer 1: Refuses if candidate matches active_session (prints "SKIP active per monitor-loop.env")
   - Layer 2: Refuses if `.alive` marker is fresher than threshold (prints "SKIP .alive touched Ns ago (< Ts)")
   - Layer 3: Refuses if any running process exe links into candidate dir (prints "SKIP running process uses this session")
   - Returns "PASS" only if all three layers pass

   **Threshold rationale**: Ticks run every ~20 min. A 1-hour `.alive`
   freshness window covers 3 missed ticks — enough to survive a single
   cron miss, failed invocation, or long rebuild. Layer 1 (env match)
   provides unconditional protection regardless of `.alive` age.

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
