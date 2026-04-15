---
name: production-ops
description: Run henyey as a production-grade Stellar client — sweep, track, fix, clean
argument-hint: [--fresh]
---

Parse `$ARGUMENTS`:
- If `--fresh` is present, set `$FRESH = true`. Otherwise set `$FRESH = false`.

# Production Operations

Operate henyey as a production-grade Stellar mainnet client. This involves
two concurrent workloads (sweep historical ledgers and track the live
network), automated bug fixing, code maintenance, and disk housekeeping.

**Mainnet operation is explicitly authorized** — this overrides the
testnet-only guideline in CLAUDE.md.

## Startup

1. Read `docs/SWEEP_STATUS.md` to determine current progress. If
   `$FRESH = true`, ignore prior sweep state and plan to start from
   L59501312 (first protocol-24 checkpoint).
2. Generate a session ID (8-char random hex). All session data goes
   under `~/data/<session-id>/`.
3. Create directories:
   ```
   mkdir -p ~/data/<session-id>/{logs,cache,cargo-target}
   ```
4. Build the binary:
   ```
   CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release
   ```
5. Create git worktrees for concurrent operation (use `--detach` since
   `main` is already checked out):
   ```
   git worktree add --detach ~/data/<session-id>/worktree-tracker HEAD
   git worktree add --detach ~/data/<session-id>/worktree-sweeper-1 HEAD
   git worktree add --detach ~/data/<session-id>/worktree-sweeper-2 HEAD
   ```
6. Start the status monitor background loop (see Status Updates below).

## Tasks

Tasks are listed in priority order. When the tracker and sweeper both
surface bugs simultaneously, prioritize the tracker (it is live).

### Task 1: Tracker (highest priority)

Run a mainnet validator that tracks the live network. Operates in the
`worktree-tracker` worktree.

**Start the validator:**
```
~/data/<session-id>/cargo-target/release/henyey run --validator \
  -c configs/validator-mainnet.toml \
  2>&1 | tee ~/data/<session-id>/logs/tracker.log
```

**Monitor:** Continuously tail the tracker log for errors, hash
mismatches, panics, crashes, or any other irregular logs (unexpected
warnings, assertion failures, stuck ledger progression, unusual resource
usage patterns, etc.). When an issue is found:
1. Follow the Bug Fix Workflow below.
2. Rebuild the binary.
3. Restart the validator.

### Task 2: Sweepers (2 concurrent)

Verify historical mainnet ledger ranges using verify-execution. Up to 2
verify-execution processes run concurrently, each in its own worktree.

**Worktrees:** `worktree-sweeper-1` and `worktree-sweeper-2`. Create
both during startup:
```
git worktree add --detach ~/data/<session-id>/worktree-sweeper-1 HEAD
git worktree add --detach ~/data/<session-id>/worktree-sweeper-2 HEAD
```

**Resume point:** Read from `docs/SWEEP_STATUS.md`. Assign each sweeper
a non-overlapping chunk starting from the first unverified ledger.

**Chunk size:** 100,000 ledgers per invocation.

**Cache directories:** Each sweeper MUST use its own cache directory to
avoid corruption from concurrent bucket downloads:
```
~/data/<session-id>/cache-1   # for sweeper-1
~/data/<session-id>/cache-2   # for sweeper-2
```

**Invocation (per sweeper):**
```
cd ~/data/<session-id>/worktree-sweeper-N && \
~/data/<session-id>/cargo-target/release/henyey verify-execution \
  --mainnet --from <START> --to <END> \
  --stop-on-error --quiet \
  --cache-dir ~/data/<session-id>/cache-N \
  2>&1 | tee ~/data/<session-id>/logs/sweep-<START>-<END>.log
```

**On success:** Update `docs/SWEEP_STATUS.md` with the verified range.
Clean stale cache from the completed range (bucket files that won't be
reused by the next chunk):
```
# Remove checkpoint-specific cache entries for completed ranges
find ~/data/<session-id>/cache -name "*.xdr*" -mmin +30 -delete 2>/dev/null
```
Advance that sweeper to the next unverified chunk.

**On error:**
1. Mark the range from chunk start to (failed ledger - 1) as **CLEAN**
   in `docs/SWEEP_STATUS.md` — those ledgers were verified successfully
   and must not be scanned again.
2. Follow the Bug Fix Workflow below.
3. After the fix, rebuild the binary and resume from the **failed
   ledger** (not the chunk start). Use a new chunk that starts at the
   failed ledger and ends at the original chunk end.

**On unfixable bug (3 failed attempts):** Document the issue in
`docs/SWEEP_STATUS.md` under a "Known Issues" section with: ledger
number, error type, investigation notes, and attempted fixes. Skip past
the problematic ledger and continue with the next chunk.

**Goal:** Keep sweeping forward until all available ledgers are verified.

### Task 3: Post-Commit Review

After every bug-fix commit from Task 1 or Task 2:
- Run `/review-fix --apply` on the commit.
- If the review surfaces issues, fix them before resuming the
  sweep/tracker.

### Task 4: Code Maintenance (daily)

Run at least once per day, or after completing a sweep range:
- `/cleanup --apply` for all crates that had changes.
- `/document` for all crates that had changes.
- `/parity-check --apply` for all crates that had changes.

### Task 5: Disk Housekeeping (every 4 hours)

Run a cleanup pass every 4 hours:
- Check disk usage: `df -h ~/data/`
- Remove old cargo target dirs from dead sessions (sessions with no
  running processes).
- Remove log files older than 48 hours.
- Remove stale cache files from completed sweep ranges.
- If disk usage exceeds 85% after cleanup, alert in the status summary
  and aggressively prune older session data.

## Bug Fix Workflow

When a hash mismatch, error, or crash is encountered:

1. **Identify** the failing ledger number and error type from the log.
2. **Reproduce** with a targeted offline test:
   ```
   henyey verify-execution --from <LEDGER> --to <LEDGER> \
     --stop-on-error --show-diff
   ```
3. **Write a failing unit test** that isolates the bug. The test must
   fail before the fix.
4. **Fix the code** in the main worktree (not a task worktree).
5. **Verify** the unit test passes.
6. **Run `cargo test --all`** to check for regressions.
7. **Commit** the fix and regression test together:
   ```
   git add <files>
   git commit -m "<Imperative description of fix>"
   ```
8. **Push** immediately: `git push` (if rejected, `git pull --rebase && git push`).
9. **Run `/review-fix --apply`** on the commit.
10. **Rebuild** the binary:
    ```
    CARGO_TARGET_DIR=~/data/<session-id>/cargo-target cargo build --release
    ```
11. **Pull the fix** into all worktrees:
    ```
    cd ~/data/<session-id>/worktree-tracker && git pull --rebase
    cd ~/data/<session-id>/worktree-sweeper-1 && git pull --rebase
    cd ~/data/<session-id>/worktree-sweeper-2 && git pull --rebase
    ```
12. **Update `docs/SWEEP_STATUS.md`** with the bug details and fix.
13. **Commit and push** the updated SWEEP_STATUS.md:
    ```
    git add docs/SWEEP_STATUS.md
    git commit -m "SWEEP_STATUS.md: document <bug-id> fix"
    git push
    ```
14. **Resume** the sweep or restart the tracker.

## State & Persistence

All state is designed to survive session restarts. A new session reads
the persisted state and picks up where the previous session left off.

- **`docs/SWEEP_STATUS.md`**: Source of truth for sweep progress. Updated
  after each completed chunk, bug fix, or known issue. Contains:
  - Verified ranges (clean)
  - In-progress ranges
  - Bugs found and fixed (with commit hashes)
  - Known issues (unfixable bugs)
  - Current sweep frontier
- **Logs**: `~/data/<session-id>/logs/` — sweep, tracker, and review logs.
- **Binary**: `~/data/<session-id>/cargo-target/release/henyey`

On session restart:
1. Read `docs/SWEEP_STATUS.md` for sweep state.
2. Check for running processes from previous sessions.
3. Clean up orphaned worktrees: `git worktree prune`.
4. Create fresh worktrees and resume.

## Status Updates

Run a background loop that prints a status summary every 10 minutes:

```
while true; do
  sleep 600
  # Gather and print status
done
```

Each summary includes:

```
═══ PRODUCTION OPS STATUS — <timestamp> ═══
Session: <session-id>

SWEEP
  Frontier:    L<current>
  Verified:    L59501312 – L<latest clean>  (<N> ledgers, <percent>%)
  In progress: L<start> – L<end>
  Remaining:   ~<N> ledgers to tip

TRACKER
  Public Key: <public key from startup log>
  Peer Port:  <peer_port from config>
  HTTP Port:  <http port from config>
  Status:     <synced/behind/down>
  Ledger:     L<latest>
  Uptime:     <duration>
  Errors:     <count since start>

BUGS
  Fixed:    <N> (this session: <M>)
  Open:     <N>

DISK
  ~/data usage: <size> / <total> (<percent>%)
  Session:      <size>

FOLLOW LOGS
  Tracker:    tail -f ~/data/<session-id>/logs/tracker.log
  Sweeper 1:  tail -f ~/data/<session-id>/logs/sweep-<START1>-<END1>.log
  Sweeper 2:  tail -f ~/data/<session-id>/logs/sweep-<START2>-<END2>.log
═══════════════════════════════════════════
```

## Concurrency Model

- **Tracker** and **Sweepers** run in separate git worktrees so they can
  operate independently without interfering with each other's working
  directory state.
- Up to **2 sweeper processes** run concurrently on non-overlapping
  ledger ranges. Each sweeper uses its own worktree.
- Bug fixes are committed on `main` in the primary repo, then pulled
  into all worktrees via `git pull --rebase`.
- The status monitor runs as a background loop independent of all tasks.

## Teardown

When stopping (user interrupts or all ledgers verified):
1. Stop the tracker validator gracefully.
2. Stop any running verify-execution process.
3. Remove worktrees:
   ```
   git worktree remove ~/data/<session-id>/worktree-tracker
   git worktree remove ~/data/<session-id>/worktree-sweeper-1
   git worktree remove ~/data/<session-id>/worktree-sweeper-2
   ```
4. Print a final status summary.
5. Do NOT remove logs or cache — they may be useful for debugging.

## Guidelines

- Always build with `--release` — debug builds are too slow for mainnet.
- Keep `docs/SWEEP_STATUS.md` up to date — it is the contract between
  sessions.
- **Never re-scan clean ranges.** Do not run verify-execution on ranges
  already marked clean in SWEEP_STATUS.md unless `$FRESH = true`. When
  resuming after a bug fix, start from the exact failed ledger, not
  from the beginning of the chunk.
- When fixing bugs, follow the test-first workflow strictly. Do not skip
  writing a failing test.
- Commit bug fixes immediately after the test passes. Do not batch fixes.
- **Push after every fix commit** — do not accumulate unpushed commits.
- **Commit and push `docs/SWEEP_STATUS.md`** immediately after documenting
  a new bug or fix.
- **Daily commit**: Commit and push the sweep status report at least once
  per day, even if no bugs were found. Use message format:
  `SWEEP_STATUS.md: daily update — <summary>`.
- If a push is rejected, pull with rebase and retry.
- All commits must include the appropriate `Co-authored-by` trailer per
  CLAUDE.md.
