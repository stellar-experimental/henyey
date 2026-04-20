---
name: quickstart-fix
description: Find and fix the most recent Quickstart GHA failure — reproduce locally, fix trivially or delegate to /plan-do-review, monitor until green
argument-hint: "[--run-id <id>] [--shard <name>] [--no-build]"
---

Parse `$ARGUMENTS`:
- `--run-id <id>` — explicit Quickstart run ID to investigate. Default: most recent main-branch Quickstart run that has ≥1 failed job.
- `--shard <name>` — focus a single shard (`local/rpc`, `testnet/core,horizon`, etc.). Default: all failed shards in the chosen run.
- `--no-build` — skip `cargo build --release -p henyey` in Stage 3. Useful when iterating locally.

# Quickstart Fix

Drive a Quickstart GHA failure from detection → local reproduction → fix
or delegation → green. The Quickstart workflow
(`.github/workflows/quickstart.yml`) is the project's most complex CI
surface: it builds a `stellar/quickstart` image with henyey replacing
`stellar-core`, then runs a matrix of shards (`local/core`, `local/rpc`,
`local/core,rpc,horizon`, `local/galexie`, `testnet/core,horizon`,
`pubnet/core,rpc,horizon`). `continue-on-error` was removed in
`981da2e7` so every shard must pass — this skill is how we get back to
green.

This skill is scoped to **Quickstart failures on the `main` branch**.
`CI` and `Push on main` workflow failures are handled elsewhere — see
the `CI FIX WORKFLOW` section in `.claude/skills/monitor-loop/SKILL.md`.

## Prerequisites

- Working tree clean and on `main`: `git symbolic-ref --short HEAD`
  must equal `main`. If not, abort with `checkout main first;
  quickstart-fix only operates on main`.
- `docker` running and available to the invoking user.
- `gh` authenticated against `stellar-experimental/henyey`.
- `scripts/quickstart-local.sh` present (it is the reproduction harness).

## Workflow

The skill runs linearly through six stages. Emit a one-line status at
the start of each stage so the driver knows where it is (`Stage N:
<shard> <signature>` is a good default).

### Stage 1 — Identify the target run

1. List recent completed Quickstart runs on `main`:
   ```
   gh run list --branch main --workflow Quickstart --limit 10 \
     --json databaseId,status,conclusion,createdAt,headSha
   ```
2. Filter to `status=completed` in the last 6h.
3. **Critical — check job-level conclusions, not just run-level.** A
   run may report `conclusion=success` while individual jobs failed,
   especially on older commits that still had `continue-on-error`. For
   each candidate run:
   ```
   gh run view <id> --json jobs \
     --jq '.jobs[] | select(.conclusion=="failure") | .name'
   ```
4. Pick the target: `--run-id` if supplied; else the most recent run
   with ≥1 failed job. If all recent runs are clean, exit with
   `no failing Quickstart runs in the last 6h on main` and stop.

### Stage 2 — Collect evidence

For each failing job (or the one matching `--shard`):

1. **Top-level failure excerpt.** `gh run view <run-id> --log-failed
   2>&1 | tail -200`. Keep the exit-condition lines (panic messages,
   timeout notices, assertion failures, test failure summaries).

2. **Identify the shard** from the job name. Job names look like
   `build / 3 test / 2 test (testing-with-pr, amd64, local, rpc , )`
   → `network=local, enable=rpc`. Record both for Stage 3.

3. **Download the shard log artifact.** Quickstart uploads per-shard
   log bundles named `logs-testing-with-pr-<arch>-test-<N>`. These
   contain `/var/log/supervisor/*` from the test container —
   specifically the files that actually show what went wrong:
   ```
   gh run download <run-id> -n logs-testing-with-pr-amd64-test-<N> \
     -D /tmp/quickstart-fix-<run-id>/log-<shard>
   ```
   Enumerate available artifacts first:
   ```
   gh api repos/stellar-experimental/henyey/actions/runs/<run-id>/artifacts \
     --jq '.artifacts[] | "\(.id)|\(.name)"'
   ```

4. **Grep the logs for failure signatures.** Interesting locations:
   - `log/supervisor/stellar-core-stderr*.log` — henyey primary core.
   - `log/supervisor/stellar-rpc-stdout*.log` — stellar-rpc **and**
     its captive-core (henyey subprocess). Captive-core's output is
     prefixed `subservice=stellar-core` in this file — do not look
     for a separate captive-core log.
   - `log/supervisor/horizon-stdout*.log` — horizon service state.

   Signatures to extract (`grep -E`):
   ```
   Heartbeat|Recovery stalled|heard_from_quorum=false|ERROR|panic|
   assertion failed|waiting for ready state|thread.*panicked
   ```

5. **Summarize to the driver** in one sentence:
   `shard=<network>/<enable> failing with <signature> on commit <sha>
   (run URL: <url>)`.

### Stage 3 — Reproduce locally

1. **Use the existing harness.** `scripts/quickstart-local.sh` already
   supports every flag we need; DO NOT build new tooling. Invoke with
   shard-matching args:
   ```
   ./scripts/quickstart-local.sh \
     --enable <enable> \
     --network <network> \
     --no-test \
     --keep \
     --timeout 300
   ```
   - `--no-test` skips the harness's own sanity tests so we can
     observe the raw symptom.
   - `--keep` leaves the container running for inspection.
   - Honor `--no-build` (passed through to skip `cargo build --release
     -p henyey`).

2. **Tail the container logs.** `docker logs -f henyey-quickstart` or
   `make quickstart-logs`. For captive-core / rpc / core internals,
   `docker exec henyey-quickstart tail -n 500
   /var/log/supervisor/stellar-rpc-stdout*.log`.

3. **Confirm the signature.** If the same signature from Stage 2
   appears (same stuck ledger, same panic, same timeout), the bug is
   reproducible — proceed to Stage 4.

4. **Non-reproducible case.** If local runs pass cleanly despite CI
   failing, treat as CI-environment-specific (runner kernel, image
   versions, transient network). In that case:
   - File a `flaky-test` issue with the evidence from Stage 2 and the
     exact local-run command that did NOT reproduce.
   - Spawn `/plan-do-review` on the issue (Stage 5b).
   - Do not attempt an inline fix.

5. **Clean up** when done inspecting: `make quickstart-stop` or
   `docker rm -f henyey-quickstart`.

### Stage 4 — Triage: inline vs delegate

Apply these rules strictly; they are the decision boundary.

**Inline fix** only if ALL are true:
- Edit scope ≤ 3 files.
- Edit ≤ 50 LOC total.
- Category is one of:
  - `.github/workflows/*.yml` tweaks
  - `scripts/quickstart-*` tweaks
  - `Dockerfile.quickstart-local` tweaks
  - Test timeout adjustments in `crates/*/tests/`
  - Clippy/fmt/build errors
  - Config-file typos
- The fix does NOT touch any of:
  - `crates/app/src/app/consensus.rs`
  - `crates/app/src/app/catchup_impl.rs`
  - `crates/app/src/app/persist.rs`
  - `crates/app/src/app/overlay*/` or anything under `crates/overlay/`
  - `crates/scp/`
  - `crates/herder/`
  - `crates/ledger/manager.rs`
  - Any multi-crate coordination path

**Delegate to `/plan-do-review`** if ANY is true:
- Fix touches the consensus/catchup/overlay/scp/herder/ledger-manager
  surface listed above.
- Root-cause discussion requires reading > 3 files to explain.
- Fix requires a new test to prevent regression.
- Driver is uncertain whether this is inline-eligible.

**Rule of thumb: err toward delegation.** The `/plan-do-review`
adversarial-critic cycle catches context the driver will miss on
single-pass triage. Same convention as `monitor-loop`'s
`FIX-ROUTING POLICY`. Precedent: on 2026-04-18, `b7777353` (the
`archive_checkpoint_cache.clear()` removal) was committed inline — it
met the "≤ 50 LOC" test but touched `crates/app/src/app/consensus.rs`,
so the rule above would now delegate it. That's the intent: favor
delegation in this surface area even when the fix is small, because
adversarial review catches subtle regressions a driver rushing to
close CI will miss.

### Stage 5a — Inline fix path

1. Confirm on main: `git symbolic-ref --short HEAD` → `main`.
2. Apply the edit.
3. Verify locally:
   - `cargo test --all` (scope with `-p <crate>` if iteration is
     tight, but run the full suite at least once before pushing).
   - Rebuild: `cargo build --release -p henyey`.
   - Re-run the repro: same `scripts/quickstart-local.sh` invocation
     from Stage 3 but WITHOUT `--no-test` — let the sanity tests run
     and confirm they pass. The symptom from Stage 2 must not recur.
4. Commit with the standard trailer:
   ```
   Co-authored-by: Claude Code <claude-code@anthropic.com>
   ```
   Commit message format:
   `CI FIX — Quickstart/<shard> failed with <signature>, fixed by <one-line summary>`
5. Push. On rejection: `git pull --rebase && git push`.

### Stage 5b — Delegation path

1. **File a `/plan-do-review`-consumable issue.** Use `gh issue
   create` with a template body containing:
   - **Title**: `Quickstart <shard> fails with <signature> (run <id>)`
   - **Symptom** section: one paragraph on what goes wrong observably.
   - **Evidence** section: log excerpts from Stage 2, annotated with
     file paths inside the artifact bundle.
   - **Repro steps** section: the exact `scripts/quickstart-local.sh
     --enable … --network … --no-test --keep` invocation from
     Stage 3 that reproduces the symptom, plus the expected
     signature.
   - **Suspected root cause** (optional): if Stage 2 grep surfaced
     a plausible site, note it with file:line; otherwise leave blank.
   - **Candidate sites**: file:line references found during
     investigation.
   - **Related**: link the CI run URL.

   Capture the returned issue number as `N`.

2. **Spawn an Agent** whose sole task is to run `/plan-do-review <N>`.
   Use `subagent_type: general-purpose`. Brief the agent with:
   - The issue number.
   - A reminder that the repro instructions in the issue body are
     the verification step — do not land anything that doesn't
     eliminate the symptom under that exact invocation.
   - A note that this is delegated from `/quickstart-fix`, so the
     agent is self-driving end-to-end (adversarial critique,
     implementation, review-fix iteration, landing). The driver will
     not intervene.

3. **Skill's role ends at spawn.** Proceed to Stage 6 to monitor.

### Stage 6 — Monitor until green

1. Wait for the next Quickstart run on `main`. Use `Monitor` (see
   below) to avoid busy-polling.
2. On completion, apply Stage 1 logic: fetch run-level AND
   job-level conclusions. Green means **every job** in the Quickstart
   run is `conclusion=success`, not just the run itself.
3. **Green → success.** Print a one-line summary:
   `Quickstart green on <sha> — fix landed via <commit | issue #N>`.
4. **Same failure recurs** → re-enter Stage 2 with the new run ID;
   iterate. This means the fix was incomplete or papered over the
   symptom.
5. **Different failure** → file separately per the umbrella-avoidance
   convention documented in
   `.claude/skills/monitor-loop/SKILL.md` "Recurrence + new evidence"
   section. Do not pile onto the first issue.

**Monitoring template.** Spawn a `Monitor` to avoid polling:
```
Monitor(
  description="Quickstart completion on main",
  timeout_ms=3600000,
  persistent=false,
  command='''
    while true; do
      line=$(gh run list --branch main --limit 1 --workflow Quickstart \
        --json status,conclusion,headSha \
        --jq \'.[0] | "\\(.status)|\\(.conclusion)|\\(.headSha[:8])"\')
      state=$(echo "$line" | cut -d"|" -f1)
      if [ "$state" = "completed" ]; then
        echo "$line"; exit 0
      fi
      sleep 60
    done
  ''',
)
```

## Worked examples

### Trivial (inline)

- **Run**: `4c7b327e` (2026-04-18)
- **Shard**: CI workflow, `test_core3_restart_rejoin_over_loopback`
  (technically not Quickstart — included here because the triage rule
  is the same).
- **Signature**: `assertion failed: sim.have_all_app_nodes_externalized`
  after 21.68s (20s budget too tight for CI runner).
- **Triage**: inline. Edit ≤ 3 files, < 10 LOC, category "test
  timeout adjustment in `crates/*/tests/`".
- **Fix**: bumped the loopback variant's pre-restart
  `wait_for_app_ledger_close` 20s → 45s and peer-count waits 5s →
  10s, mirroring `4c73f458` on the TCP variant.
- **Commit message**:
  `Bump test_core3_restart_rejoin_over_loopback timeouts for CI flakiness`

### Non-trivial (should have been delegated)

- **Run**: `17dbf229` (2026-04-18)
- **Shard**: `local/rpc`
- **Signature**: `stellar-rpc: waiting for ready state, 4 minutes...`
  captive-core stuck at `ledger=13 latest_ext=0 heard_from_quorum=false
  scp_sent_ext=0` while `archive_cache: Got current ledger from archive
  ledger=719`.
- **Triage**: `archive_checkpoint_cache.clear()` call in
  `crates/app/src/app/consensus.rs::trigger_recovery_catchup`. Touches
  the consensus surface → Stage 4 says delegate. The Agent that made
  this fix committed inline because the trace was already in hand,
  but the rule above codifies "delegate for anything in
  `crates/app/src/app/`" because adversarial critique would have
  flagged e.g. whether the clear might be needed after a fatal catchup
  recovery; inline fixes skip that review.
- **Fix (under the rule)**: file issue with the
  `Recovery catchup skipped: archive hasn't published checkpoint yet`
  log as evidence, `crates/app/src/app/consensus.rs:853` as the
  candidate site, and the `./scripts/quickstart-local.sh --enable rpc
  --network local --no-test --keep` invocation as the repro step.
  Spawn `/plan-do-review <N>`.

Use the trivial worked example as the shape for commit messages; use
the non-trivial worked example as the shape for issue bodies.

## Teardown

- Stop the repro container: `make quickstart-stop` (or
  `docker rm -f henyey-quickstart`).
- Clean up downloaded artifact dirs under `/tmp/quickstart-fix-*`.
- Cancel any armed `Monitor` tasks before returning control.

## Guidelines

- **Always check job-level conclusions.** Run-level `success` can hide
  failures — this skill must never trust run-level alone.
- **Reproduce before fixing.** An unreproducible CI failure is
  environmental; don't patch code blindly on evidence you can't
  confirm locally.
- **One Quickstart run at a time.** If multiple recent runs failed for
  the same reason, fix once and let Stage 6 verify the next run is
  green. If they failed for DIFFERENT reasons, handle each
  independently via separate skill invocations — don't bundle.
- **Don't modify `scripts/quickstart-local.sh` or `Dockerfile.quickstart-local`
  as part of a Quickstart-fix commit.** Those are infrastructure; if
  they need changes, that's a separate PR (category: CI-fix inline,
  but review-worthy).
- **Push-after-fix policy matches monitor-loop**: push immediately
  after an inline fix passes local verification. Don't accumulate
  unpushed commits.
