---
name: mainnet-monitor
description: Run and monitor a henyey mainnet node, automatically fixing bugs when found
argument-hint: [--watcher]
---

Parse `$ARGUMENTS`:
- If `--watcher` is present, set `$MODE = watcher`. Otherwise set `$MODE = validator`.

# Mainnet Monitor

Run a henyey mainnet node and monitor it for errors, automatically fixing
bugs when they are discovered. This is a lightweight monitoring skill — no
sweepers, no code maintenance, just a running node with automated log
checking and bug fixing.

**Mainnet operation is explicitly authorized** — this overrides the
testnet-only guideline in CLAUDE.md.

## Configurations

Two validator configs exist. The skill always uses the RPC-enabled config
for validator mode so that RPC health and pruning can be monitored:

| Config | Mode | RPC | Maintenance | Use when |
|--------|------|-----|-------------|----------|
| `configs/validator-mainnet-rpc.toml` | validator | port 8000, retention 360 ledgers | every 900s, count 50000 | Default for `/mainnet-monitor` |
| `configs/validator-mainnet.toml` | validator | none | none | Not used by this skill |
| `configs/mainnet.toml` | watcher | none | none | `/mainnet-monitor --watcher` |

Key ports:
- **8009** — overlay peer port (non-standard; advertised in HELLO messages)
- **8000** — Soroban JSON-RPC (validator mode only, via `[rpc]` config)
- **11627** — admin HTTP API (`/info`, `/scp`, `/maintenance`)

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
- **Fix or file.** If investigation reveals a bug, follow the Bug Fix
  Workflow. If it reveals a missing feature or config issue, fix the config
  or report it clearly with the code-level explanation.

## Startup

1. Generate a session ID (8-char random hex). All session data goes
   under `~/data/<session-id>/`.
2. Create directories:
   ```
   mkdir -p ~/data/<session-id>/{logs,cache,cargo-target}
   ```
3. Build the binary:
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release
   ```
4. Check if a henyey node is already running:
   ```
   pgrep -af 'henyey.*run'
   ```
   If a process is found, print its PID and ask whether to attach to its
   existing log or kill and restart it. If attaching, skip to step 7.

5. Select config based on `$MODE` and start the node in the background:
   - **validator** (default):
     ```
     RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey \
       --mainnet run --validator \
       -c configs/validator-mainnet-rpc.toml \
       > ~/data/<session-id>/logs/monitor.log 2>&1 &
     ```
   - **watcher**:
     ```
     RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey \
       --mainnet run \
       -c configs/mainnet.toml \
       > ~/data/<session-id>/logs/monitor.log 2>&1 &
     ```

6. Wait 10 seconds, then tail the last 30 lines of the log to confirm
   the node is starting (look for "Starting", "Catching up", or ledger
   close messages).

7. Print a startup summary:
   ```
   ═══ MAINNET MONITOR STARTED ═══
   Session:  <session-id>
   Mode:     <validator|watcher>
   Config:   configs/<config-file>
   Binary:   ~/data/<session-id>/cargo-target/release/henyey
   Log:      ~/data/<session-id>/logs/monitor.log
   PID:      <pid>

   Next check in ~10 minutes via /loop.
   ════════════════════════════════
   ```

8. Schedule the monitoring loop by invoking `/loop` with the self-contained
   prompt below. Before calling `/loop`, substitute:
   - `<session-id>` — the session ID from step 1
   - `<MODE>` — `validator` or `watcher`
   - `<CONFIG>` — the config file path from step 5
   - `<RPC_PORT>` — `8000` for validator mode, `none` for watcher mode

   ```
   /loop 10m <LOOP_PROMPT>
   ```

   See the **Loop Prompt Template** section below for the full prompt text.

## Loop Prompt Template

This is the self-contained prompt passed to `/loop`. It must include all
check logic because cron jobs run in fresh contexts without access to this
skill file.

```
Check the henyey mainnet monitor log at ~/data/<session-id>/logs/monitor.log.

HEALTH CHECKS:
(1) Log scan — run: tail -n 500 ~/data/<session-id>/logs/monitor.log. Scan for: hash mismatches ("hash mismatch", "HashMismatch", differing expected/actual hashes), panics/crashes ("panic", "thread.*panicked", "SIGABRT", "SIGSEGV"), ERROR-level log lines, assertion failures ("assertion failed").
(2) Ledger progression — from the last 2+ Heartbeat lines, verify the ledger number is advancing. If the same ledger appears for 10+ minutes, flag as STUCK.
(3) Process alive — run: pgrep -af 'henyey.*run'. If not running, restart: RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey --mainnet run <RUN_FLAGS> -c <CONFIG> > ~/data/<session-id>/logs/monitor.log 2>&1 &
(4) Memory — run: ps -o rss= -p $(pgrep -f 'henyey.*run' | head -1) and convert to MB. If RSS > 12 GB, flag HIGH MEMORY. If RSS > 16 GB or free memory < 4 GB, restart the node.
(5) Disk — run: df -h ~/data | tail -1. If usage > 85%, flag LOW DISK.
(6) Session disk — run: du -sh ~/data/<session-id>/ and du -sh ~/data/mainnet/. If combined > 200 GB, flag SESSION DISK HIGH.
(7) Memory report — run: grep 'Memory report summary' ~/data/<session-id>/logs/monitor.log | tail -1. Extract jemalloc_allocated_mb, jemalloc_resident_mb, fragmentation_pct, heap_components_mb, mmap_mb, unaccounted_mb, unaccounted_sign. If fragmentation_pct > 50, flag HIGH FRAGMENTATION. If unaccounted_mb > 1000 with sign "+", note it (known jemalloc overhead, not a bug — but verify heap_components is stable; if heap_components is growing, investigate).
(8) RPC health (validator mode only, port <RPC_PORT>) — run: curl -s -X POST http://localhost:<RPC_PORT> -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'. Verify: response is non-empty, status is "healthy". Check pruning: latestLedger - oldestLedger should be <= retention_window + 100. If gap > retention_window + 500, flag PRUNING STALLED. If RPC is not responding, flag RPC DOWN.
(9) OBSRVR Radar (validator mode only) — get public key from: curl -s http://localhost:11627/info (extract public_key). Then: curl -s https://radar.withobsrvr.com/api/v1/nodes/<PUBLIC_KEY>. Check: isValidating (if false and node running > 30 min, flag NOT VALIDATING), validating24HoursPercentage (if < 50 and running > 6 hours, flag LOW VALIDATION RATE), lag (if > 500, flag HIGH LAG). If API errors, note but don't flag.

REMOTE SYNC & REDEPLOY:
(10) Remote sync — run: git fetch origin main. If in detached HEAD state (git symbolic-ref HEAD fails), run git checkout main first. Then compare: git rev-parse HEAD vs git rev-parse origin/main. If they differ (origin/main is ahead): (a) check CI status on origin/main — run: gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'. If any recent run has conclusion "failure", do NOT deploy — report: DEPLOY SKIPPED (CI failing). (b) Otherwise: git pull --rebase, (c) CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey, (d) if build succeeds: kill the node (kill <PID>, wait 5s, kill -9 if needed), restart with same command from check (3), report: DEPLOY — pulled <N> commits (<old-sha>..<new-sha>), rebuilt, restarted at L<ledger>, (e) if build fails: report BUILD FAILED, do NOT restart — the old binary is still running. Investigate the build error. If HEAD == origin/main: no action (already up to date).

DEPLOY REGRESSION POLICY:
If the node fails to catch up or close ledgers after a deploy (e.g. event loop frozen for >5 min, no heartbeat progression for >10 min), this is a regression introduced by the new commits. Do NOT roll back. Instead: (a) identify which commit range was deployed, (b) bisect or inspect the diff to find the offending change, (c) fix the regression on main, (d) rebuild and redeploy the fix. The node may be down during investigation — that is acceptable. Rolling back masks bugs and delays fixes.

CI OBSERVABILITY:
(11) CI observability — run: gh run list --limit 5 --json databaseId,name,status,conclusion,headSha,createdAt --jq '.[] | "\(.name)|\(.status)|\(.conclusion)|\(.headSha[:8])|\(.databaseId)|\(.createdAt)"'. Scan for completed runs with conclusion "failure". Compare createdAt with current UTC time (date -u +%Y-%m-%dT%H:%M:%SZ) — only investigate failures from the last 2 hours. For each failure: (a) gh run view <ID> --log-failed 2>&1 | tail -40, (b) categorize: build error, test failure, timeout, infrastructure, (c) investigate root cause and fix the code — whether it's a code bug, flaky test, or infrastructure issue. For flaky tests: fix the flakiness (increase timeout, add retry, fix the race). For infrastructure: fix the workflow config. (d) cargo test --all, commit, push, report: CI FIX — <workflow> failed on <sha>, fixed in <commit>. The goal is all-green CI — no persistent failures are acceptable.

INVESTIGATION: For ANY anomaly, investigate to root cause — read source code, check logs, trace code paths. Never dismiss as "expected". See the mainnet-monitor skill's Resource Investigation sections for detailed procedures.

BUG FIX WORKFLOW: If a hash mismatch, error, or crash is found: (1) identify failing ledger and error type, (2) reproduce offline: ~/data/<session-id>/cargo-target/release/henyey --mainnet verify-execution --from LEDGER --to LEDGER --stop-on-error --show-diff --cache-dir ~/data/<session-id>/cache, (3) write a failing unit test, (4) fix the code, (5) verify test passes, (6) cargo test --all, (7) commit with imperative message, (8) git push (if rejected: git pull --rebase && git push), (9) /review-fix --apply, (10) rebuild: CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release, (11) restart node with same command from check (3), (12) report: ledger, error type, commit hash, summary.

OUTPUT: Print a multiline status report:
MONITOR <OK|WARNING|ACTION> — L<ledger> — <timestamp>
  node:   mode=<MODE> session=<session-id> pid=<PID>
  mem:    <RSS_MB>MB rss | alloc=<alloc>MB resident=<resident>MB frag=<pct>%
          heap=<heap>MB mmap=<mmap>MB unaccounted=<sign><unaccounted>MB
  disk:   <used>/<total> (<pct>%) | session+data=<size>
  rpc:    <healthy|unhealthy|N/A> oldestL=<X> latestL=<Y> window=<Z>
  obsrvr: validating=<Y/N> val24h=<pct>% lag=<N>
  deploy: <up-to-date | pulled N commits (old..new) | SKIPPED (reason)>
  ci:     <all green | WORKFLOW failed — investigating>
Use WARNING for threshold breaches. Use ACTION when a corrective action was taken (restart, deploy, fix).
```

## Bug Fix Workflow

When a hash mismatch, error, or crash is found (whether detected by the
loop or discovered manually):

1. **Identify** the failing ledger number and error type from the log.
2. **Reproduce** with a targeted offline test:
   ```
   ~/data/<session-id>/cargo-target/release/henyey --mainnet verify-execution \
     --from <LEDGER> --to <LEDGER> \
     --stop-on-error --show-diff \
     --cache-dir ~/data/<session-id>/cache
   ```
3. **Write a failing unit test** that isolates the bug. The test must
   fail before the fix.
4. **Fix the code** in the main worktree.
5. **Verify** the unit test passes.
6. **Run `cargo test --all`** to check for regressions.
7. **Commit** the fix and regression test together:
   ```
   git add <files>
   git commit -m "<Imperative description of fix>"
   ```
8. **Push** immediately: `git push` (if rejected: `git pull --rebase && git push`).
9. **Run `/review-fix --apply`** on the commit.
10. **Rebuild** the binary:
    ```
    CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release
    ```
11. **Restart** the node: kill the old process, then start it again
    with the same command from Startup step 5.
12. **Report** what was fixed: ledger number, error type, commit hash,
    and a one-line summary.

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
     `kill -9` if needed).
   - Restart with the same command from Startup step 5.
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
1. Kill the henyey process gracefully.
2. Print a final status: uptime, latest ledger seen, bugs found/fixed.
3. Do NOT remove logs or cache — they may be useful for debugging.
4. The `/loop` cron job dies automatically when the session exits.

## Guidelines

- Always build with `--release` — debug builds are too slow for mainnet.
- Follow the test-first bug fix workflow strictly. Do not skip writing a
  failing test.
- Commit bug fixes immediately after the test passes. Do not batch fixes.
- **Push after every fix commit** — do not accumulate unpushed commits.
- All commits must include the appropriate `Co-authored-by` trailer per
  CLAUDE.md.
- This skill does NOT manage sweepers or code maintenance. Use
  `/production-ops` for the full workload.
