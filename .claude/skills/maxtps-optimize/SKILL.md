---
name: maxtps-optimize
description: |
  Autonomous, hypothesis-driven optimizer for henyey's distributed max TPS,
  measured with the Supercluster MissionMaxTPSClassic (23-node tier-1) on a
  Namespace (nsc) cloud VM. Takes a target tx/s and iterates one minimally-scoped,
  parity-safe, proven hypothesis at a time until the target is reached or an
  iteration cap is hit, maintaining a measurements/hypotheses document and
  opening a PR per accepted change. Distinct from `perf-optimize`, which tunes the
  local single-shot apply-load benchmark; this skill optimizes the *networked*
  max-TPS ceiling end-to-end.
argument-hint: "<target-tps> [--max-iterations=N] [--band=lo-hi]"
---

Parse `$ARGUMENTS`:
- First token = `$TARGET` tx/s (required; if missing/invalid, ask for it, e.g. `/maxtps-optimize 400`).
- `--max-iterations=N` → `$MAX_ITERS` (default `10`).
- `--band=lo-hi` → `$BAND` initial search band (default derived from the measured baseline).

# maxtps-optimize — autonomous distributed max-TPS optimizer

Iteratively raise henyey's classic-payment max TPS (measured by Supercluster
`MissionMaxTPSClassic` on a single nsc 32×64 VM) toward `$TARGET`. One hypothesis
per iteration: instrument → change → prove parity → measure → accept/reject →
document → regenerate hypotheses.

## Autonomy contract (read first)

This is a **long-running autonomous loop. It does NOT stop for operator
intervention.** Concretely:

- **Never ask the operator a question and never pause for approval/confirmation.**
  Do not end a turn with "want me to continue?", "should I proceed?", or any
  request for direction. The only things that end the run are the **Stop
  conditions** below. If you find yourself about to ask — instead, pick the
  highest-value next iteration and run it.
- **Running out of *surgical* ideas is NOT a stop condition.** If no clean
  >5% parity-safe code lever is currently identifiable, that means your next
  iteration is a **deeper diagnostic** (add metrics → add logs → profile under
  load → core-vs-henyey comparison — see the *Diagnosis escalation ladder*),
  which will produce one. Keep going until a Stop condition fires.
- **Drive yourself across the long steps.** Image builds (~12 min) and mission
  runs (~10 min) are long; launch them in the background and use `ScheduleWakeup`
  to resume. The run-doc is your durable state — write enough to it each step
  that any resume can continue without operator input. Operator messages may
  arrive, but you must never *wait* for them.
- **Keep the instance alive for the whole run.** Provision ONE nsc instance and
  reuse it across every iteration. Do NOT tear it down between iterations or to
  "pause" — only at a true Stop condition (extend its duration if it nears
  expiry, or re-provision if it died).

Baseline context (see `docs/maxtps-baseline.md`): henyey ≈ 221–225 tx/s vs
stellar-core ≈ 1533 on this rig. The limiter is apply/consensus-side, **not**
loadgen and **not** raw CPU — ledgers close ~27–49% full with idle CPU and
occasional `NodeLostSyncException` near capacity. So the ceiling is most likely
*upstream of execution* (tx dissemination / nomination / admission per ledger).
Start diagnostic.

## Hard constraints (non-negotiable)

1. **Parity** — every kept change MUST preserve the observable/interop surface in
   `docs/PARITY.md` bit-for-bit (ledger header & hashes, `TransactionResult`,
   `LedgerCloseMeta`/`TransactionMeta` XDR, event XDR & ordering, SCP/overlay
   wire, history format, HTTP/JSON-RPC/CLI contracts, crypto outputs). Metrics,
   logging, internal architecture, and performance optimizations are explicitly
   divergeable — that's your working room. If a change alters anything observable,
   it is rejected regardless of speedup.
2. **One hypothesis per iteration + proof** — test exactly one mechanism at a
   time so the measured Δ is attributable, and keep a change only if it is
   *proven* to give a meaningful gain (>5%, below).
   - **Big swings are welcome.** The *change* can be as large and ambitious as
     the diagnosed bottleneck warrants — reworking a data structure, the
     flooding/queue path, a hot loop across crate boundaries, etc. Do **not**
     abandon a real, well-diagnosed bottleneck just because the fix isn't a
     one-liner; that's a normal optimization, not a reason to stop. Ambition/diff
     size is **not** limited.
   - **Commits stay minimal/atomic.** "Minimal scope" applies to *commits*, not
     ambition: each commit is the smallest coherent unit. Don't bundle unrelated
     changes; split incidental instrumentation, refactors, and the behavior
     change into separate atomic commits where reasonable; one accepted
     optimization = one focused PR (a large but cohesive change is fine as one
     commit — just don't lump multiple mechanisms together).
3. **Efficiency** — short probes, narrow bands, one reused instance. "Be
   efficient" governs *load runs* (don't launch long/wide missions needlessly) —
   it does **not** discourage image rebuilds for instrumentation or code changes;
   those are the expected per-iteration cost.

## Accept / reject bar

- **Accept** iff measured Δ **> 5%** over `current_best` on the short-probe
  **and** the parity gate passed. (Short-probe only — no canonical confirmation
  run; that was the chosen tradeoff for speed.)
- Otherwise **reject**: revert the behavior change. Keep instrumentation only if
  it is parity-safe and diagnostically useful.

## Stop conditions (the ONLY things that end the run)

End the run **only** when one of these is true — never otherwise (in particular,
never because you're unsure, lack a surgical lever, or want operator input):

1. `current_best ≥ $TARGET` (success), or
2. completed `$MAX_ITERS` iterations (count **every** iteration, diagnostic or
   code-change), or
3. a hard infra limit you cannot work around (e.g. nsc quota exhausted and
   re-provision fails, or the registry/cluster is down after retries).

"Exhausted all optimizations" is **not** a separate early stop: as long as
iterations remain under the cap, there is always a next iteration — if no code
lever is ready, the next iteration is a deeper diagnostic (see ladder). The loop
runs to the cap (or target) by construction.

### Diagnosis escalation ladder (use when no >5% code lever is ready)
Each rung is a valid iteration; climb it until a concrete, diagnosed,
parity-safe lever emerges, then implement+measure it:
1. **Scrape existing meters** (`/metrics`, `/info`) at passing and failing rates.
2. **Add metrics** (`crates/app/src/metrics.rs` catalog + refresh) for the
   suspected subsystem.
3. **Add targeted logs** (e.g. `tracing::info!(target:"maxtps_diag", …)`) on the
   hot path to capture per-round/per-tx detail; revert noisy logs after capture.
4. **Profile under load** (uftrace — see `perf-optimize-uftrace` +
   `docs/perf-hypotheses-uftrace.md`) to get function-level hot spots.
5. **Core-vs-henyey comparison**: run stellar-core at matched rates and compare
   the same signals to localize henyey's specific deficiency.
A diagnosed bottleneck that needs a substantial (non-surgical) change is still a
lever — implement it (constraint 2), don't stop.

---

## Phase 0 — Setup (once)

1. **Session dir** (per `AGENTS.md` storage rules): `SID=$(openssl rand -hex 4)`,
   `RUN=~/data/$SID/maxtps-opt`, `mkdir -p "$RUN"`. All scratch, logs, kubeconfig,
   cargo-target live under `~/data/$SID/`.
2. **Work on a branch** off `origin/main` (never commit to `main`):
   `git fetch origin && git checkout -B maxtps/opt-$SID origin/main`.
3. **Harness**: ensure `vendor/supercluster/src/FSLibrary/MaxTPSTest.fs` has the
   `SSC_MAXTPS_TXS_MULTIPLIER` knob (env-driven load window; default 1000). It is
   already present on `main`. Build the harness once:
   ```
   (cd vendor/supercluster && dotnet build src/App/App.fsproj --configuration Release)
   ```
4. **Launcher**: write `$RUN/run_mission.sh` from the template at the end of this
   skill (it wraps `dotnet … App.dll mission MaxTPSClassic` with the standard
   flags: `--install-network-delay false --core-http-via-pod-exec
   --genesis-test-account-count 23000 --probe-timeout 240`).
5. **Provision ONE nsc instance** for the whole run (reused across iterations):
   ```
   nsc instance create --ephemeral --enable=kubernetes:1.33 --machine_type 32x64 \
     --duration=6h --purpose "maxtps-optimize $SID" \
     --cidfile "$RUN/instance.id" --output_json_to "$RUN/instance.json" \
     --wait_kube_system --wait_timeout 8m
   INST=$(cat "$RUN/instance.id")
   nsc kubeconfig write "$INST"   # then copy the printed path to $RUN/kubeconfig.yaml
   nsc kubectl "$INST" get nodes  # confirm Ready
   ```
6. **Baseline measurement**: build + push the current-HEAD image and measure
   (see "Measurement procedure"). Tag `:opt-$SID-base`. Record the result as
   `current_best` and `base_commit`.
7. **Create the run doc** `docs/maxtps-optimization/<UTC-date>-target$TARGET.md`
   from the template below; fill the header + baseline + the seeded hypothesis
   backlog (all `pending`).

---

## Phase 1 — Iteration loop

Repeat until a stop condition fires. Each iteration tests exactly **one**
hypothesis.

1. **Pick** the highest-value `pending` hypothesis (first iteration: start with
   the diagnostic instrumentation one — you must know *where* the cap is before
   optimizing). Mark it `testing` in the doc.
2. **Instrument if needed** — add metrics/timers/counters via the catalog macro in
   `crates/app/src/metrics.rs`, recorded in `crates/app/src/app/ledger_close.rs`
   or `crates/ledger/src/execution/tx_set.rs` / `manager.rs`. Metrics are
   divergeable. An instrumentation-only iteration (no behavior change, just read
   `/metrics` after a near-ceiling run) is valid and cheap; use it to sharpen the
   next hypothesis. Record the captured numbers in the doc.
3. **Implement** the minimally-scoped change for the hypothesis.
4. **Parity gate (targeted)** — run the impacted crate's tests plus the relevant
   parity tests, e.g.:
   ```
   CARGO_TARGET_DIR=~/data/$SID/cargo-target cargo test -p <crate> --tests
   ```
   The change MUST NOT alter observable output. If a parity/consistency test
   fails or any observable bytes change → **reject** now (revert), document, next.
5. **Measure** — rebuild the image, deploy, short-probe binary search in a narrow
   band around `current_best` (see procedure). Compute `Δ = (max - current_best)/current_best`.
6. **Decide & land**:
   - **Accept** (Δ > 5% and parity gate green): set `current_best = max`; mark
     `accepted`; commit the change on the branch and **open a PR** (see "PR per
     accepted change"); rebuild the base image tag so the next iteration stacks on
     top.
   - **Reject**: `git checkout -- <changed files>` (keep diagnostic instrumentation
     if parity-safe and useful); mark `rejected`.
7. **Document & regenerate** — update the doc row (measurement, Δ, status, notes);
   add any NEW hypotheses the metrics suggest as `pending`. Loop.

---

## Measurement procedure (the efficient short-probe)

For each measurement (baseline and per-iteration):

1. **Build + push image** from repo root (reuse cargo cache via the build mount):
   ```
   nsc build --push -n nscr.io/k4jkul01t5rr0/henyey:opt-$SID-<label> . -f Dockerfile
   ```
   (`k4jkul01t5rr0` is the workspace registry; confirm with `nsc workspace describe`
   if a push 401s.)
2. **Clear pods** between runs: `nsc kubectl "$INST" -n default delete statefulset --all`, wait until 0 pods.
3. **SCREEN FIRST at the accept threshold (single step, fast go/no-go).** Before
   the full search, run ONE step at `SCREEN = ceil(current_best * 1.05)` — the
   exact bar a change must clear to be accepted. Force a single step by setting a
   band whose first midpoint is `SCREEN` and whose width is just over the
   threshold, e.g. `TX_RATE = SCREEN-7`, `MAX_TX_RATE = SCREEN+8`
   (`NUM_PREGEN = MAX_TX_RATE*65`), same `SSC_MAXTPS_TXS_MULTIPLIER=60`
   `SSC_LOADGEN_FASTFAIL_LEDGERS=10`. Read the first verdict:
   - **`SCREEN` FAILS** → the change cannot clear +5% → **reject immediately**,
     skip the full search. Cost: ~1 failing step (~2 min) instead of a full
     ~10-min mission. This is the common case and the main time saver.
   - **`SCREEN` PASSES** → promising → proceed to the full narrow-band search
     (step 4) to quantify the new `current_best` and confirm the gain.
   Always screen before a full mission; never run a full search for a change that
   can't even clear the threshold step.
4. **Run the full short-probe** (only if the screen passed) with a narrow band
   around `current_best`:
   - `LO = current_best` (a known-pass floor), `HI = ceil(min($TARGET, current_best*1.3))`.
   - `NUM_PREGEN = HI * 65` (cover the largest step's `HI*60` offered txns + headroom).
   ```
   cd "$RUN" && \
   SSC_MAXTPS_TXS_MULTIPLIER=60 SSC_LOADGEN_FASTFAIL_LEDGERS=10 \
   TX_RATE=$LO MAX_TX_RATE=$HI NUM_PREGEN=$NUM_PREGEN \
   ./run_mission.sh "nscr.io/k4jkul01t5rr0/henyey:opt-$SID-<label>" opt-<label> \
     > "$RUN/mission-<label>.log" 2>&1 &
   ```
5. **Parse** the result: `grep "Found max tx rate" "$RUN/mission-<label>.log"`.
   Failing steps abort early via the `Loadgen fast-fail:` path (~1–2 min);
   passing steps ~1–1.5 min. A full narrow search is typically <15 min.
6. **Health**: between polls, `nsc ssh "$INST" -- sh -c 'uptime; free -h | grep Mem; df -h / | tail -1'`
   (flag sustained load >28, mem-available <5Gi, disk >85%).

Why this is the chosen methodology (validated this session, see
`docs/maxtps-baseline.md`): `SSC_MAXTPS_TXS_MULTIPLIER=60` (1-min offer window)
reproduces the canonical ceiling within ~2%; `SSC_LOADGEN_FASTFAIL_LEDGERS=10`
cuts failing-step cost from ~3.5 min to ~1.9 min without changing the converged
max (the node-side wait-till-complete budget is 20 ledgers, parity with core).

> Note the ~2% short-probe upward bias: require Δ **> 5%** so accepted gains
> clear the noise floor. If a result lands at 5–8% and looks borderline, you may
> optionally confirm it with one `SSC_MAXTPS_TXS_MULTIPLIER=300` run before
> accepting — but default to not spending the time.

---

## PR per accepted change

Per `AGENTS.md`/`CLAUDE.md`:
```
git add <files> && git commit \
  -m "<imperative summary of the optimization>" \
  -m "maxtps-optimize iter N: <hypothesis>; <before>→<after> tx/s (+X%, short-probe). Parity: <tests run>." \
  -m "Co-authored-by: Claude Code <claude-code@anthropic.com>"
git push -u origin HEAD
gh pr create --repo stellar-experimental/henyey --base main --head <branch> \
  --title "..." --body "...measurements + parity evidence + run-doc link..."
```
Each accepted change is its own commit/PR, stacked on the prior accepted commit
so the next iteration measures on top of it.

---

## End of run (any stop condition)

1. **Full gate**: `CARGO_TARGET_DIR=~/data/$SID/cargo-target cargo test --all`,
   then invoke the `parity-check` skill on every crate touched by accepted
   changes. If anything fails, the corresponding change is NOT parity-safe — open
   a revert/fix and note it.
2. **Finalize the run doc**: summary table (all hypotheses + statuses), cumulative
   gain (baseline → final), and the list of opened PRs. Commit + push it; open a
   final PR for the doc (and reference the per-change PRs).
3. **Teardown**: `nsc instance destroy "$INST" --force` and verify it's gone
   (`nsc instance list`). Revert any uncommitted local harness patches.
4. **Report** to the operator: baseline → final tx/s, accepted vs rejected count,
   PR URLs, and the top remaining `pending` hypotheses for a future run.

---

## Seeded hypothesis backlog

Order is deliberate — diagnose before optimizing, because CPU is idle and ledgers
are under-filled (the cap is unlikely to be execution speed).

1. **(diagnostic, do first)** Ledger-fill instrumentation: per-ledger
   txns-applied, nominated tx-set size, and tx-queue/flood depth. Determines
   whether the cap is admission/nomination (consensus-side) or apply-side, and
   reframes the rest of the backlog.
2. **Tx admission / flood flow-control**: how many txs are admitted and flooded
   per ledger (`crates/overlay`, `crates/herder`; cf. #3575 backpressure, #3614
   live `maxTxSetSize`). Prime suspect given idle CPU + partial ledgers.
3. **`NodeLostSyncException` near capacity**: per-node apply-duration vs
   ledger-close time and sync state; a node that can't apply within the close
   window drops sync and caps the whole network.
4. **Ledger-close cadence**: phase-timer breakdown (timers already in
   `crates/app/src/metrics.rs`, recorded in `ledger_close.rs`) — is a commit /
   persist / bucket phase stretching close time under load?
5. **(deprioritized) Per-tx serial apply overhead**: `run_transactions_on_executor`
   and the per-tx `snapshot_delta` clone in `crates/ledger/src/execution/tx_set.rs`
   — only pursue if instrumentation shows `classic_exec` actually dominates close
   time.

Regenerate and re-rank from each iteration's measurements; do not treat this list
as fixed.

---

## Run-document template

Create at `docs/maxtps-optimization/<UTC-date>-target<TARGET>.md`:

```markdown
# maxtps-optimize run — <UTC date>, target <TARGET> tx/s

- Session: <SID> · Instance: <INST> (nsc 32×64) · Base commit: <sha>
- Methodology: MissionMaxTPSClassic, 23-node tier-1, short-probe
  (SSC_MAXTPS_TXS_MULTIPLIER=60, SSC_LOADGEN_FASTFAIL_LEDGERS=10),
  --install-network-delay false. Accept bar: Δ > 5%.
- **Current best: <N> tx/s** @ <commit/PR> · Target: <TARGET>

## Hypotheses

| # | hypothesis | instrumentation added | measurement | Δ vs best | status | notes / next |
|---|------------|-----------------------|-------------|-----------|--------|--------------|
| 0 | baseline   | —                     | <N> tx/s    | —         | —      | base image opt-<SID>-base |
| 1 | …          | …                     | …           | …         | pending| … |

## Summary (filled at end)
- Baseline → final: <A> → <B> tx/s (+X%, M of K hypotheses accepted)
- PRs: #… , #…
- Top remaining pending hypotheses: …
```

`status ∈ {pending, testing, accepted, rejected}`.

---

## Launcher template (`$RUN/run_mission.sh`)

```bash
#!/usr/bin/env bash
# Usage: run_mission.sh <IMAGE> <LABEL>   (env: TX_RATE MAX_TX_RATE NUM_PREGEN
#   SSC_MAXTPS_TXS_MULTIPLIER SSC_LOADGEN_FASTFAIL_LEDGERS)
set -uo pipefail
IMAGE="$1"; LABEL="$2"
export PATH="$HOME/.dotnet:$HOME/.local/bin:$PATH"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
RUNDIR="$(cd "$(dirname "$0")" && pwd)"
SSC="$(git -C "$RUNDIR" rev-parse --show-toplevel 2>/dev/null)/vendor/supercluster"
# If $RUN is outside the repo, point SSC at the worktree explicitly instead.
dotnet "$SSC/src/App/bin/Release/net8.0/App.dll" mission MaxTPSClassic \
  --kubeconfig "$RUNDIR/kubeconfig.yaml" --namespace default \
  --destination "$RUNDIR/artifacts-$LABEL" --core-http-via-pod-exec \
  --image "$IMAGE" --install-network-delay false --probe-timeout 240 \
  --tx-rate "${TX_RATE:-100}" --max-tx-rate "${MAX_TX_RATE:-300}" \
  --num-pregenerated-txs "${NUM_PREGEN:-100000}" --genesis-test-account-count 23000
```
(Point `SSC` at the built `vendor/supercluster` for whichever checkout has the
release `App.dll`; the env vars `SSC_MAXTPS_TXS_MULTIPLIER` / `SSC_LOADGEN_FASTFAIL_LEDGERS`
are read by the harness process, so export them when invoking.)

## Guardrails recap
- **Autonomous: never stop to ask the operator.** End only on a Stop condition
  (target / `$MAX_ITERS` / hard infra limit) — not on uncertainty or lack of a
  surgical lever. Out of code levers ⇒ run the next diagnostic rung, don't stop.
- Parity surface is sacred (`docs/PARITY.md`); metrics/internal/perf are free.
- One hypothesis per iteration for clean attribution; **big swings welcome**
  (ambition/diff size unlimited) but keep **commits atomic/minimal** — one
  accepted optimization = one focused PR; proven >5% to keep.
- One reused instance kept alive for the whole run; short probes; narrow bands.
- **Screen before you search**: one step at `current_best*1.05`; if it fails,
  reject the change there (no full mission). Only full-search promising changes.
- Self-drive across long build/run steps via background tasks + `ScheduleWakeup`;
  run-doc is resumable state.
- Tear the instance down **only at the end** (it costs money while alive).
