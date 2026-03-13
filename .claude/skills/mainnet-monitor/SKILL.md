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

5. Select config and command based on `$MODE`:
   - **validator** (default):
     ```
     ~/data/<session-id>/cargo-target/release/henyey run --validator \
       -c configs/validator-mainnet.toml \
       2>&1 | tee ~/data/<session-id>/logs/monitor.log
     ```
   - **watcher**:
     ```
     ~/data/<session-id>/cargo-target/release/henyey run \
       -c configs/mainnet.toml \
       2>&1 | tee ~/data/<session-id>/logs/monitor.log
     ```
   Start the node in the background.

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

8. Schedule the monitoring loop by invoking `/loop` with a fully
   self-contained prompt. Substitute the real `<session-id>`, `<RUN_CMD>`
   (the full run command from step 5), and `<MODE>` before calling `/loop`:

   ```
   /loop 10m Check the henyey mainnet monitor log at ~/data/<session-id>/logs/monitor.log. Run: tail -n 500 ~/data/<session-id>/logs/monitor.log. Scan for: (1) hash mismatches (lines containing "hash mismatch", "HashMismatch", or differing expected/actual hashes), (2) panics or crashes ("panic", "thread.*panicked", "SIGABRT", "SIGSEGV"), (3) ERROR-level log lines, (4) assertion failures ("assertion failed"), (5) stuck ledger progression (same ledger number for the last 10+ minutes). Also check if the process is alive: pgrep -af 'henyey.*run'. If the process is not running, restart it in the background: <RUN_CMD>. If everything looks healthy, print one line: MONITOR OK — L<latest-ledger> — <timestamp> — mode: <MODE> — session: <session-id>. If a bug is found, follow the Bug Fix Workflow: (1) identify the failing ledger number and error type from the log, (2) reproduce offline: ~/data/<session-id>/cargo-target/release/henyey --mainnet verify-execution --from LEDGER --to LEDGER --stop-on-error --show-diff --cache-dir ~/data/<session-id>/cache, (3) write a failing unit test that isolates the bug — it must fail before the fix, (4) fix the code in the main worktree, (5) verify the unit test passes, (6) run cargo test --all to check for regressions, (7) commit fix and regression test together with an imperative message, (8) git push (if rejected: git pull --rebase && git push), (9) run /review-fix --apply on the commit, (10) rebuild: CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release, (11) kill the old henyey process and restart it in the background: <RUN_CMD>, (12) report the fix: ledger number, error type, commit hash, one-line summary.
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
