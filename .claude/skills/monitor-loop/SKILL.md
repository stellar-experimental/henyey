---
name: monitor-loop
description: Run and monitor a henyey mainnet node, detecting bugs and filing issues
argument-hint: [--watcher]
---

Parse `$ARGUMENTS`:
- If `--watcher` is present, set `$MODE = watcher`. Otherwise set `$MODE = validator`.

# Monitor Loop

Run a henyey mainnet node and monitor it for errors. When bugs are found,
investigate to root cause, file GitHub issues, and delegate fixes to spawned
agents. All henyey issues are in scope: node bugs, CI failures, testnet
parity bugs, performance regressions, infrastructure issues.

**Mainnet operation is explicitly authorized** — this overrides the
testnet-only guideline in CLAUDE.md.

## Commit Policy

> **Maintenance note:** this policy is also restated inside the Loop Prompt
> Template so that the cron context (which runs without this skill file) has
> the same rules. Any change here must be mirrored there.

- **NEVER revert commits made by other developers.** If a deployed commit
  causes a regression (deadlock, crash, sync failure), file a GitHub issue
  with the details and restart the node on a known-good binary. Do not
  force-push, revert, or reset commits you didn't author.
- **CI fixes for others' commits**: Fix compilation errors or clippy
  issues caused by incomplete refactors, but do NOT revert the original
  commits. Push a fix-forward commit instead.
- **Your own commits**: You may revert your own commits if they cause
  regressions.

## Fix-Routing Policy

Two classes of fix exist in this skill; treat them differently.

- **CI fixes — inline, in the monitoring process.** CI failures block deploy
  and are usually small (typo, clippy, trivial refactor fallout, workflow
  YAML). Read logs, fix the code in the main checkout, commit, push. The
  monitoring loop is allowed to do this work directly. This is the one
  exception to "do not implement fixes inline."
- **Node bugs — delegated to a spawned Agent that runs `/plan-do-review`
  on the filed issue.** Hash mismatches, crashes, consensus/sync failures,
  pruning defects, memory leaks. The monitor's job ends at filing the
  issue; a spawned Agent takes over and runs the `/plan-do-review <N>`
  skill, which handles adversarial proposal critique, implementation,
  review-fix iteration, and landing the change. The monitor does **not**
  run a separate worktree-Agent, review the diff, or cherry-pick — all
  of that lives inside `/plan-do-review`. The next redeploy tick rebuilds
  and restarts the node whenever `/plan-do-review` has landed its fix on
  main.

If you are unsure whether to delegate at all, err toward filing an issue
and spawning the `/plan-do-review` Agent.

Because the monitor is a single-investigator process (only one
`/monitor-loop` holds the concurrency lock at a time — see Startup step
1), the investigation phase itself — reading source files, grepping for
symbols, running `git log`, reading the running node's log — can
happen directly in the main checkout. The "isolate edits" concern only
applies to *writing* code: investigation is read-only, and pretending
otherwise adds ceremony without safety. The moment the work turns into
code edits, it MUST go through `/plan-do-review` (or the CI-fix inline
path); never edit the main checkout to fix a node bug.

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
  with the root cause and proposed fix. If it reveals a missing feature
  or config issue, file it with the code-level explanation. Spawn an
  agent to implement the fix if appropriate.
- **File everything.** Every anomaly that isn't immediately explained
  by reading the code should result in a GitHub issue. Even if you
  can't fix it now, the issue documents the finding for future work.
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
  download logic to find and fix the root cause.

## Bug Investigation Workflow

When a bug is detected (hash mismatch, error, crash, sync failure):

1. **Investigate to root cause directly in the main checkout** — read
   source code, check logs, trace code paths, grep, `git log`, read the
   running node's log file. No worktree needed: only one monitor-loop
   runs at a time (concurrency lock) and investigation is read-only.
   Document findings with file:line references.
2. **File a GitHub issue** with the investigation findings, root cause
   analysis, and proposed fix approach using `gh issue create`. Capture
   the issue number `N` returned by `gh`. Write the issue body as a
   proposal `/plan-do-review` can consume: clear symptom, evidence,
   suspected root cause, and a concrete fix sketch with file:line
   references.
3. **Spawn an Agent that runs `/plan-do-review <N>`** on the filed issue.
   `/plan-do-review` handles everything downstream — adversarial
   proposal critique, implementation (in its own isolated workspace),
   iterative review-fix, and landing the change. The monitor's
   responsibility ends here: no worktree management, no cherry-pick,
   no diff review, no merge decision. Those all live inside
   `/plan-do-review`.
4. **Do NOT edit the main checkout to fix a node bug.** Investigation
   reads in the main checkout are fine; code edits for a node-bug fix
   are not — those go through `/plan-do-review`. CI-only fixes (build
   errors, test failures, clippy, workflow YAML) are exempt per the
   Fix-Routing Policy.

The next redeploy tick (check 10) will pick up whatever `/plan-do-review`
lands on main — rebuild, kill, restart. The bug-fix progress itself is
tracked on the issue and by `/plan-do-review`'s own reporting; the
monitor does not block on it.

### Recurrence + new evidence → re-spawn `/plan-do-review` autonomously

When a previously-filed bug (open, already had one or more `/plan-do-review`
runs) recurs, the monitor's default is **not** "wait and see"; it is
"did the recurrence give us new evidence, and if so, re-spawn
`/plan-do-review <N>`". Specifically, re-spawn when any of the
following is true versus the state at the last `/plan-do-review` run:

- The bug reproduces on a different commit or a different process,
  strengthening the "this is deterministic, not a one-off" case.
- New instrumentation output has landed on the issue (log lines,
  stack dumps, `/proc/wchan`, profile data, metric deltas) that
  the prior run didn't have access to.
- A negative finding rules out the prior run's root-cause hypothesis
  (e.g. the prior proposal blamed compute backpressure but fresh
  telemetry shows zero compute-side warnings).
- The candidate site set has narrowed (specific locks, functions,
  ledger numbers) in a way the prior proposal didn't cover.

Do not wait for the user to ask "is there enough information now?" —
the decision rule is "would the next `/plan-do-review` run have
strictly more information than the last one?" If yes, spawn it; brief
the new agent with pointers to the new comments so it doesn't retrace
the prior ground.

What the monitor does NOT do on recurrence:
- Re-file a duplicate issue (`/plan-do-review` re-runs against the
  existing issue number).
- Wait for the existing `/plan-do-review` agent to finish before
  queueing the next one if the existing agent is already running on
  stale evidence. Exception: if the existing agent is still in its
  proposal-convergence stage and the new evidence is purely additive,
  posting the new evidence as a comment on the issue (so the running
  agent picks it up) may be sufficient; escalate to a re-spawn only if
  the existing agent is past the proposal stage.

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

   **(a) Process found — attach mode.** Print its PID and ask the user
   whether to attach or kill+restart. If attaching:
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
   mkdir -p ~/data/<session-id>/{logs,cache,cargo-target}
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
(2) Ledger progression & sync deadline — persist ledger progression across ticks so STUCK can be detected by a single invocation: (a) read ~/data/<session-id>/last_ledger (if it exists) — format is "<ledger>|<unix-timestamp>". (b) extract the current ledger from the most recent Heartbeat line in the log tail. (c) if the file exists and its ledger equals the current ledger and the recorded timestamp is more than 600s old, flag STUCK. (d) if the ledger has advanced or the file is missing, overwrite ~/data/<session-id>/last_ledger with "<current-ledger>|<now>". Additionally, check node uptime: run ps -o etime= -p $(pgrep -f 'henyey.*run' | head -1). Compare uptime against the deadline from FRESH_START. If uptime exceeds the deadline and the node is not yet in real-time sync: check the latest Heartbeat for the gap between `ledger` and `latest_ext` — if gap > 5, or if RPC status is "unhealthy", or if `heard_from_quorum=false`, flag as SYNC FAILURE. This is a bug, not a transient condition. Do NOT report it as a WARNING and wait. Investigate the catchup path: check for checkpoint-boundary stalls ("failed to download header"), hash mismatches, or event loop freezes in the log. If FRESH_START=yes and uptime is under 4h, a large gap is expected — report CATCHING UP instead of SYNC FAILURE.
(3) Process alive — run: pgrep -af 'henyey.*run'. If not running, before relaunching: (i) `rm -f ~/data/mainnet/mainnet.lock` to clear any stale lockfile the dead process left behind; (ii) preserve the prior session's log so its final lines are post-mortem-debuggable — `mv ~/data/<session-id>/logs/monitor.log ~/data/<session-id>/logs/monitor.log.crashed-$(date -u +%Y%m%dT%H%M%SZ) 2>/dev/null || true`; (iii) relaunch with **append** redirection so interleaving restart pathways don't nuke history: RUST_LOG=info nohup ~/data/<session-id>/cargo-target/release/henyey --mainnet run <RUN_FLAGS> -c <CONFIG> >> ~/data/<session-id>/logs/monitor.log 2>&1 &
(4) Memory — run: ps -o rss= -p $(pgrep -f 'henyey.*run' | head -1) and convert to MB. If RSS > 12 GB, flag HIGH MEMORY. If RSS > 16 GB or free memory < 4 GB, restart the node (kill <PID>, wait 10s, kill -9 if still alive, then relaunch as in check 3).
(5) Disk — run: df -h ~/data | tail -1. If usage > 85%, flag LOW DISK.
(6) Session disk — run: du -sh ~/data/<session-id>/ and du -sh ~/data/mainnet/. If combined > 200 GB, flag SESSION DISK HIGH.
(7) Memory report — run: grep 'Memory report summary' ~/data/<session-id>/logs/monitor.log | tail -1. If grep returns no output, flag WARNING memory-report-missing (log format may have changed). Otherwise extract jemalloc_allocated_mb, jemalloc_resident_mb, fragmentation_pct, heap_components_mb, mmap_mb, unaccounted_mb, unaccounted_sign. If fragmentation_pct > 50, flag HIGH FRAGMENTATION. If unaccounted_mb > 1000 with sign "+", note it (known jemalloc overhead, not a bug — but verify heap_components is stable; if heap_components is growing, investigate).
(8) RPC health (validator mode only — skip in watcher mode; RPC_PORT=<RPC_PORT>) — run: curl -s -X POST http://localhost:<RPC_PORT> -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'. Verify: response is non-empty, status is "healthy". Check pruning: latestLedger - oldestLedger should be <= retention_window + 100. If gap > retention_window + 500, flag PRUNING STALLED. If RPC is not responding, flag RPC DOWN.
(9) OBSRVR Radar (validator mode only — skip in watcher mode) — get public key from: curl -s http://localhost:<ADMIN_PORT>/info (extract public_key). Then: curl -s https://radar.withobsrvr.com/api/v1/nodes/<PUBLIC_KEY>. Check: isValidating (if false and node running > 30 min, flag NOT VALIDATING), validating24HoursPercentage (if < 50 and running > 6 hours, flag LOW VALIDATION RATE), lag (if > 500, flag HIGH LAG). If API errors, emit "obsrvr: N/A (api-error)" in the status line instead of omitting the field.

REMOTE SYNC & REDEPLOY:
(10) Remote sync — first sanity-check the working tree: (pre-a) if git status --porcelain reports any output, ABORT the deploy path for this tick — the previous CI-fix commit hasn't been finalized or there are local edits. Report: DEPLOY SKIPPED (dirty tree) with the list of dirty paths. Do not run git pull against a dirty tree; do not kill the node. Fix the dirty tree (commit, stash, or investigate) before the next tick. (a) If clean, run: git fetch origin main. If in detached HEAD state (git symbolic-ref HEAD fails), run git checkout main first. Then compare: git rev-parse HEAD vs git rev-parse origin/main. If they differ (origin/main is ahead): (b) check CI status on origin/main — run: gh run list --branch main --limit 3 --json conclusion --jq '.[].conclusion'. If any recent run has conclusion "failure", do NOT deploy — instead, immediately investigate and fix the CI failure using the CI FIX WORKFLOW below (check 11). After pushing the fix, wait for the next loop iteration to deploy. (c) If all conclusions are "success" (ignore "" for in-progress and "cancelled"): git pull --rebase, (d) CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release -p henyey, (e) if build succeeds: kill the node (kill <PID>, wait 10s, kill -9 if still alive), restart with same command from check (3), report: DEPLOY — pulled <N> commits (<old-sha>..<new-sha>), rebuilt, restarted at L<ledger>, (f) if build fails: report BUILD FAILED, do NOT restart — the old binary is still running. Route the build error through the CI FIX WORKFLOW below. If HEAD == origin/main: no action (already up to date).

COMMIT POLICY (must match the skill body's Commit Policy section; update both together):
- NEVER revert commits made by other developers. If a deployed commit causes a regression, file a GitHub issue and restart on a known-good binary.
- CI fixes for others' commits: fix-forward only, do NOT revert.
- You may revert your own commits if they cause regressions.

FIX-ROUTING POLICY (must match the skill body's Fix-Routing Policy section):
- CI fixes (build errors, test failures, clippy, workflow YAML) are handled INLINE in the main checkout by this loop.
- Node bugs (hash mismatches, crashes, consensus/sync failures, pruning defects, memory leaks) are DELEGATED: file a GitHub issue, then spawn an Agent whose task is to run `/plan-do-review <N>` on that issue. Do NOT run a separate worktree Agent, review diffs, or cherry-pick — adversarial critique, implementation, review-fix iteration, and landing all live inside `/plan-do-review`. The next redeploy tick (check 10) will pick up whatever it lands on main.

DEPLOY REGRESSION POLICY:
If the node fails after a deploy: (a) file a GitHub issue with the regression details (commit range, symptoms, WATCHDOG data), (b) if the regression was from YOUR commits, revert and fix forward, (c) if the regression was from ANOTHER developer's commits, restart the node on the last known-good binary (rebuild from the previous commit) but do NOT revert their commits — file the issue and let them fix it.

CI FIX WORKFLOW:
(11) CI check — scope and levels of detection:
  (11a) Scope: only inspect workflows that run on branch main. Run: gh run list --branch main --limit 10 --json databaseId,name,status,conclusion,headSha,createdAt --jq '.[] | "\(.name)|\(.status)|\(.conclusion)|\(.headSha[:8])|\(.databaseId)|\(.createdAt)"'. Ignore runs triggered by PRs on other branches. Scan for completed runs with conclusion "failure".
  (11b) Job-level (CRITICAL — catches continue-on-error failures): For the latest completed run of EACH distinct workflow name (e.g. ci, quickstart, verify-execution, history-publish — enumerate dynamically from 11a, do NOT hard-code "Quickstart"), check individual jobs: gh run view <ID> --json jobs --jq '.jobs[] | select(.conclusion == "failure") | "\(.name)|\(.conclusion)"'. Workflows with continue-on-error jobs report run-level conclusion "success" even when jobs fail — you MUST check job-level conclusions. If any jobs have conclusion "failure", treat it the same as a run-level failure.
REPORTING RULE — NEVER report "ci: all green" if ANY job has conclusion "failure", even if the run-level conclusion is "success". The ci: line in the status report MUST reflect the WORST job-level result across all workflows. A continue-on-error job failure is NOT "green" — it is RED. Do not qualify failures as "known", "pre-existing", or "cosmetic". A failure that persists across multiple commits is MORE urgent, not less — it means no one has fixed it.
Compare createdAt with current UTC time (date -u +%Y-%m-%dT%H:%M:%SZ) — only investigate failures from the last 2 hours. CI failures are bugs — they MUST be investigated and fixed immediately, never deferred to "next cycle". For each failure: (a) gh run view <ID> --log-failed 2>&1 | tail -80, (b) categorize: build error, test failure, timeout, infrastructure, (c) investigate root cause and fix the code. Before committing, verify you are on main: git symbolic-ref --short HEAD must equal "main"; if not, git checkout main first. (d) cargo test --all, commit, push, report: CI FIX — <workflow> failed on <sha>, fixed in <commit>. The goal is all-green CI — no persistent failures are acceptable. If CI is red, this check takes priority over all other checks except process-alive.

INVESTIGATION: For ANY anomaly, investigate to root cause — read source code, check logs, trace code paths. Never dismiss as "expected". File a GitHub issue for every anomaly that isn't immediately explained.

BUG FIX WORKFLOW (node bugs only — CI fixes go through check 11): If a hash mismatch, error, or crash is found: (1) identify failing ledger and error type, (2) investigate to root cause — read source code, trace code paths, (3) file a GitHub issue with findings using `gh issue create` and capture the returned issue number N; write the body as a proposal that `/plan-do-review` can consume (symptom, evidence, suspected root cause, fix sketch with file:line references), (4) spawn an Agent whose sole task is to run `/plan-do-review <N>` on the filed issue — that skill handles adversarial critique, implementation, review-fix iteration, and landing the change. Do NOT edit the main checkout to fix a node bug. Do NOT run a separate worktree Agent, review the diff yourself, or cherry-pick — all of that lives inside `/plan-do-review`. The next redeploy tick (check 10) will pick up whatever `/plan-do-review` lands on main.

OUTPUT: Print a multiline status report:
MONITOR <OK|WARNING|ACTION> — L<ledger> — <timestamp>
  node:   mode=<MODE> session=<session-id> pid=<PID> fresh_start=<yes|no>
  sync:   <synced | CATCHING UP (gap=N, uptime=Xm, deadline=<15m|4h>) | SYNC FAILURE (gap=N, uptime=Xm — investigating)>
  mem:    <RSS_MB>MB rss | alloc=<alloc>MB resident=<resident>MB frag=<pct>%
          heap=<heap>MB mmap=<mmap>MB unaccounted=<sign><unaccounted>MB
  disk:   <used>/<total> (<pct>%) | session+data=<size>
  rpc:    <healthy|unhealthy|N/A> oldestL=<X> latestL=<Y> window=<Z>
  obsrvr: <validating=<Y/N> val24h=<pct>% lag=<N> | N/A (watcher) | N/A (api-error)>
  deploy: <up-to-date | pulled N commits (old..new) | SKIPPED (dirty-tree|ci-red|build-failed)>
  ci:     <all green (run+job level) | WORKFLOW failed | WORKFLOW jobs FAILED (continue-on-error) — NAME|conclusion listed>
Use WARNING for threshold breaches. Use ACTION when a corrective action was taken (restart, deploy, fix). Use SYNC FAILURE (not WARNING) when the node has exceeded the active sync deadline (15m populated / 4h fresh-start) but is not closing ledgers in real-time — this is a bug that requires immediate investigation.
```

## CI Fix Workflow

When a CI run completes with `conclusion: "failure"` (detected by
check 11 in the loop, or by check 10 when a deploy is blocked):

1. **Get logs**: `gh run view <ID> --log-failed 2>&1 | tail -80`.
2. **Categorize** the failure:
   - **Build error** — compilation failure, missing dependency
   - **Test failure** — a test assertion failed
   - **Flaky test** — test passes locally but fails intermittently in CI
   - **Timeout** — job exceeded time limit
   - **Infrastructure** — runner issue, network error, GitHub outage
3. **Fix** based on category:
   - **Build error**: Read the compiler error, fix the code, run
     `cargo build --all` locally.
   - **Test failure**: Reproduce locally with
     `cargo test -p <crate> <test_name>`. Read the test and the code
     it exercises. Fix the code (or the test if the test is wrong).
   - **Flaky test**: Fix the flakiness — increase timeout, add retry
     logic, fix the race condition. Do NOT disable or `#[ignore]` the
     test.
   - **Timeout**: Check if a test is doing too much work or hanging.
     Fix the root cause.
   - **Infrastructure**: Fix the workflow config in
     `.github/workflows/`. If it's a transient GitHub outage, re-run
     the job: `gh run rerun <ID> --failed`.
4. **Verify locally**: `cargo test --all`.
5. **Check branch before pushing.** Run `git symbolic-ref --short HEAD`. It
   must equal `main`. If it does not, `git checkout main` first. Never
   push a CI fix from a detached HEAD or a feature branch.
6. **Commit and push**: Follow CLAUDE.md commit guidelines.
7. **Verify CI goes green**: `gh run list --branch main --limit 3` after push.
   If the new run also fails, repeat from step 1.
7. **Report**: `CI FIX — <workflow> failed on <sha>, root cause:
   <description>, fixed in <commit>`.

CI failures are treated with the same urgency as node errors. A red
CI blocks deploys and means the codebase has a known defect. Do not
defer, do not mark as "will investigate later".

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
4. Print a final status: uptime, latest ledger seen, bugs found/fixed.
5. Do NOT remove logs or cache — they may be useful for debugging.

Any `/plan-do-review` Agents spawned during the session keep running
independently — tearing down the monitor does not cancel them. That is
intentional: if the monitor stops, the fix-it pipeline should still
land its work so the next invocation starts from a better state.

## Guidelines

- Always build with `--release` — debug builds are too slow for mainnet.
- All commits must include the appropriate `Co-authored-by` trailer per
  CLAUDE.md.
- All henyey issues are in scope: mainnet bugs, testnet parity bugs, CI
  failures, performance regressions, infrastructure problems.
- **Push after every fix commit** — do not accumulate unpushed commits.
- **Node-bug fixes come from spawned Agents that run `/plan-do-review <N>`
  on the filed issue.** The monitor's role is to detect, investigate,
  file, and spawn — not to implement, review, or merge. All of that
  lives inside `/plan-do-review`.
- CI-only fixes are inline per the Fix-Routing Policy.
