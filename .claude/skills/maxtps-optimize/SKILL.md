---
name: maxtps-optimize
description: |
  Autonomous, hypothesis-driven optimizer for henyey's distributed max TPS,
  measured with the Supercluster MissionMaxTPSClassic (23-node tier-1) on a
  Namespace (nsc) cloud VM. Default mode is `frontier`: remove bottlenecks one
  proven, parity-safe hypothesis at a time until the raw simulated network
  (latency/throughput) is the demonstrated binding constraint — stellar-core's
  number is a reference floor, not a ceiling. Maintains a
  measurements/hypotheses document and opens a PR per accepted change.
  Distinct from `perf-optimize`, which tunes the local single-shot apply-load
  benchmark; this skill optimizes the *networked* max-TPS ceiling end-to-end.
argument-hint: "[frontier|<target-tps>] [--max-iterations=N]"
---

Parse `$ARGUMENTS`:
- First token = mode: `frontier` (default if missing) or a numeric `$TARGET` tx/s.
  In frontier mode there is no numeric target — the run ends on the
  **network-bound proof** (below) or the iteration cap. A numeric `$TARGET`
  adds "reached the target" as an extra stop condition; everything else is
  identical.
- `--max-iterations=N` → `$MAX_ITERS` (default `10`).

# maxtps-optimize — autonomous distributed max-TPS optimizer

Maximize henyey's classic-payment **sustained** max TPS (measured by
Supercluster `MissionMaxTPSClassic` on a single nsc 32×64 VM). One hypothesis
per iteration: attribute the current binder → change → prove parity → measure
→ accept/reject → document → re-attribute.

**Core is not the ceiling.** stellar-core's numbers (burst 1530 / sustained
1522 on this rig) are a diagnostic reference, not the target. henyey's
networking and apply stack are more parallel than core's single-threaded main
loop; within the fixed overlay/SCP wire protocols henyey should be able to
*beat* core. The run is done when the bottleneck is provably the simulated
network itself — not any henyey code path.

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
  runs (~10 min short / ~25 min sustained) are long; launch them in the
  background and use `ScheduleWakeup` to resume. The run-doc is your durable
  state — write enough to it each step that any resume can continue without
  operator input. Operator messages may arrive, but you must never *wait* for
  them.
- **Keep the instance alive for the whole run.** Provision ONE nsc instance and
  reuse it across every iteration. Do NOT tear it down between iterations or to
  "pause" — only at a true Stop condition (extend its duration if it nears
  expiry, or re-provision if it died).

Baseline context (2026-07-03, post-PR #3712): henyey burst ≈ **1510** tx/s
(measured *before* the WAL fix — re-measure first, it may be higher now) and
sustained ∈ **[1400, 1450)**; stellar-core burst 1530 / sustained 1522 on the
same instance class. The prior campaigns' root causes and refuted dead ends
are in `docs/maxtps-optimization/2026-07-02-target2000.md`, PR #3712, and
memory `project_maxtps_sustained_fix.md` — read them before re-chasing
anything (laggard queues, leader-computation divergence, and validate-on-fetch
cost are already refuted; WAL-checkpoint stalls and nomination fetch
starvation are already fixed).

## Hard constraints (non-negotiable)

1. **Parity** — every kept change MUST preserve the observable/interop surface in
   `docs/PARITY.md` bit-for-bit (ledger header & hashes, `TransactionResult`,
   `LedgerCloseMeta`/`TransactionMeta` XDR, event XDR & ordering, SCP/overlay
   wire, history format, HTTP/JSON-RPC/CLI contracts, crypto outputs). Metrics,
   logging, internal architecture, and performance optimizations are explicitly
   divergeable — that's your working room. If a change alters anything observable,
   it is rejected regardless of speedup.
2. **Cadence is fixed** — the 5 s expected ledger close time is mission-standard
   config, identical for core, and stays untouched so numbers remain comparable.
   TPS gains must come from fuller ledgers and tighter pipelines, not faster
   clocks.
3. **One hypothesis per iteration + proof** — test exactly one mechanism at a
   time so the measured Δ is attributable, and keep a change only if it is
   *proven* to give a meaningful gain (>5%, below, gated on sustained).
   - **Big swings are welcome.** The *change* can be as large and ambitious as
     the diagnosed bottleneck warrants — reworking a data structure, the
     flooding/queue path, a hot loop across crate boundaries, an async persist
     pipeline. Do **not** abandon a real, well-diagnosed bottleneck just
     because the fix isn't a one-liner.
   - **Commits stay minimal/atomic.** "Minimal scope" applies to *commits*, not
     ambition: split incidental instrumentation, refactors, and the behavior
     change into separate atomic commits where reasonable; one accepted
     optimization = one focused PR.
4. **Feature policy** (operator-decided, 2026-07-03): features on the
   measurement path may be restructured off the critical path (async/batched)
   AND additionally disabled via a **validator config profile** when core has
   no equivalent (e.g. per-tx SQLite storage, which core dropped in v21).
   Missions run the validator profile; RPC-serving configurations keep full
   features. A profile switch alone is acceptable only with the async
   restructure also available for full-feature nodes.
5. **Efficiency** — short probes to screen, sustained probes only to gate
   accepts, one reused instance. Efficiency governs *load runs*; it does not
   discourage image rebuilds for instrumentation or code changes.

## Definition of done — the network-bound proof (frontier mode)

Frontier mode ends successfully when the run doc contains BOTH:

1. **Network calibration (Phase 0, per instance).** Measure raw pod-to-pod
   capacity on the instance (`iperf3` between two pods, or `dd | nc`), both
   throughput and RTT. Instances drift and differ — calibrate every
   provisioned instance, don't reuse old numbers. From it, derive the tx-byte
   ceiling estimate for the 23-node flood fan-out at the candidate rate
   (~200 B/payment × fan-out + tx-set fetch traffic + SCP traffic).
2. **Cycle-time attribution at the edge.** At the highest passing sustained
   rate AND one failing step above it, decompose the full ledger cycle —
   trigger → build_value → candidate fetch → validate → ballot → externalize
   → apply → persist → next trigger — using the in-tree telemetry
   (`maxtps_cad` trig/ballot/ext, `maxtps_nom`, `maxtps_fetch`, WAL
   `db_write_ctx` holders; extend as needed), plus per-node veth byte/packet
   counters during the binding phase. **Network-bound** means the failing
   step's binding path is dominated (≳90%) by wire time — tx-pull latency at
   the RTT floor or veth throughput at calibrated capacity — with CPU and disk
   demonstrably idle. Anything else *names the next bottleneck*, which becomes
   the next iteration; that is the normal loop, not a failure.

## Accept / reject bar

`current_best` is the **sustained** number (5-min probe,
`SSC_MAXTPS_TXS_MULTIPLIER=300`).

- **Screen** (cheap reject): one short-probe single step at
  `SCREEN = ceil(current_best * 1.05)` (`SSC_MAXTPS_TXS_MULTIPLIER=60`,
  `TX_RATE=SCREEN-7`, `MAX_TX_RATE=SCREEN+8`, `NUM_PREGEN=(SCREEN+8)*65`,
  `SSC_LOADGEN_FASTFAIL_LEDGERS=10`). Fails → **reject immediately** (~2 min
  spent). Never run a sustained probe for a change that can't clear the
  short screen.
- **Gate** (required for accept): screen passed → run the sustained single
  step at the same level: `SSC_MAXTPS_TXS_MULTIPLIER=300`, `TX_RATE=X-7`,
  `MAX_TX_RATE=X+8`, `NUM_PREGEN=(X+8)*305`, `SSC_LOADGEN_FASTFAIL_LEDGERS=10`
  (~8 min pass / ~4 min fail after boot). **Accept iff the sustained step
  passes** and the parity gate is green; then `current_best = X` and
  optionally ladder upward (repeat single steps at +5%) while the image is
  deployed.
- Single-step pass/fail near the edge is stochastic ±3-4%: compare only
  same-instance A/B, and re-baseline `current_best` after any re-provision
  (instance drift is a documented fact — see the 2026-07-02 run doc).
- Rejected changes are reverted; parity-safe diagnostic instrumentation may
  stay.

Why sustained-gated: the 2026-07-02 campaign proved short probes accept
changes that die under sustained load (burst hides WAL compounding, backlog
spirals, and wedge dynamics). The short probe is a screen, never an accept.

## Bottleneck attribution — how to pick the next hypothesis

**Attribute first, then remove the named binder.** Every optimization
iteration starts from evidence naming the current binding constraint (cycle
decomposition, WAL holders, fetch latencies, veth counters, profiler output)
— not from a hunch. The binder is fair game wherever it lives:

- henyey-specific code (historically the richest vein — WAL checkpointing,
  fetch scheduling, churn engine were all here);
- a shared algorithm that henyey can implement better in parallel (apply
  execution, signature verification, persist pipelines) as long as outputs
  stay bit-identical;
- an internal semantic both implementations share, provided wire bytes and
  the PARITY.md surface hold.

Core comparisons are a *diagnostic tool* — run core at the same rate and
compare the same signal to localize where core spends less — never the
target. Do not stop at core's number, and do not reject a lever merely
because core shares it: if removing it needs no wire/parity change, it
counts.

## Stop conditions (the ONLY things that end the run)

End the run **only** when one of these is true — never otherwise (in
particular, never because you're unsure, lack a surgical lever, or want
operator input):

1. **Network-bound proof achieved** (frontier mode; both pieces of evidence in
   the run doc), or `current_best ≥ $TARGET` when a numeric target was given, or
2. completed `$MAX_ITERS` iterations (count **every** iteration, diagnostic or
   code-change), or
3. a hard infra limit you cannot work around (e.g. nsc quota exhausted and
   re-provision fails, or the registry/cluster is down after retries).

"Exhausted all optimizations" is **not** a separate early stop: as long as
iterations remain under the cap, there is always a next iteration — if no code
lever is ready, the next iteration is a deeper diagnostic (see ladder). The
loop runs to the cap (or proof) by construction.

### Diagnosis escalation ladder (use when no >5% code lever is ready)
Each rung is a valid iteration; climb it until a concrete, diagnosed,
parity-safe lever emerges, then implement+measure it:
1. **Scrape existing meters** at passing and failing rates. Two endpoints (via
   `nsc kubectl <inst> exec <pod> -c stellar-core-run -- curl -s localhost:<port>/...`):
   - `:11626` `/metrics` (compat **medida JSON**, `/info`) — curated core-compat
     set (ledger close, tx count, etc.); this is what Supercluster itself reads.
   - `:11628` `/metrics` (native **Prometheus** registry) — the FULL set incl. the
     overlay/SCP propagation metrics absent from the medida JSON:
     `stellar_overlay_tx_pull_latency_seconds` (mean = `_sum`/`_count`),
     `stellar_overlay_demand_timeout_total`, `stellar_scp_timing_nominated_*`.
     Enabled in SSC via the `RS_STELLAR_CORE_NATIVE_METRICS_PORT` env var
     (Supercluster sets `11628`; see `StellarKubeSpecs.fs`).
2. **Add metrics** (`crates/app/src/metrics.rs` catalog + refresh) for the
   suspected subsystem.
3. **Add targeted logs** on the hot path — the `maxtps_cad` / `maxtps_nom` /
   `maxtps_fetch` / `maxtps_ban` tracing targets from the prior campaigns are
   already in-tree; extend them rather than inventing new ones. Revert noisy
   logs after capture.
4. **Profile under load** (uftrace — see `perf-optimize-uftrace` +
   `docs/perf-hypotheses-uftrace.md`) to get function-level hot spots.
5. **Core-vs-henyey comparison**: run stellar-core at matched rates and compare
   the same signals to localize where core spends less on the same path.
A diagnosed bottleneck that needs a substantial (non-surgical) change is still a
lever — implement it (constraint 3), don't stop.

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
6. **Network calibration** (frontier definition-of-done input): measure raw
   pod-to-pod throughput + RTT on THIS instance (`iperf3` between two pods or
   `dd | nc`); record in the run doc header.
7. **Baseline measurement**: build + push the current-HEAD image and measure
   BOTH burst (short probe) and sustained (5-min single steps upward from the
   last known sustained bound). Tag `:opt-$SID-base`. Record sustained as
   `current_best` and note the burst number and `base_commit`. Optionally run
   the same-instance core reference (2×2: burst/sustained × core/henyey) when
   the campaign's claims will be stated relative to core.
8. **Create the run doc** `docs/maxtps-optimization/<UTC-date>-frontier.md`
   (or `-target$TARGET.md`) from the template below; fill the header +
   baseline + the seeded hypothesis backlog (all `pending`).

---

## Phase 1 — Iteration loop

Repeat until a stop condition fires. Each iteration tests exactly **one**
hypothesis.

1. **Pick** the highest-value `pending` hypothesis. If no code lever is
   evidence-backed yet, the iteration is a diagnostic (attribution) rung.
   Mark it `testing` in the doc.
2. **Instrument if needed** — metrics/timers/targeted logs (divergeable). An
   instrumentation-only iteration is valid and cheap; use it to name the
   binder precisely. Record the captured numbers in the doc.
3. **Implement** the change for the hypothesis.
4. **Parity gate (targeted)** — run the impacted crate's tests plus the relevant
   parity tests:
   ```
   CARGO_TARGET_DIR=~/data/$SID/cargo-target cargo test -p <crate> --tests
   ```
   The change MUST NOT alter observable output. If a parity/consistency test
   fails or any observable bytes change → **reject** now (revert), document, next.
5. **Measure** — rebuild the image, deploy, screen (short) then gate
   (sustained) per the Accept/reject bar.
6. **Decide & land**:
   - **Accept**: set `current_best`; mark `accepted`; commit on the branch and
     **open a PR**; rebuild the base image tag so the next iteration stacks on
     top. Ladder upward while deployed to find the new sustained edge.
   - **Reject**: revert the behavior change (keep parity-safe diagnostics);
     mark `rejected`.
7. **Document & re-attribute** — update the doc row; refresh the cycle
   decomposition at the new edge; add NEW hypotheses the evidence suggests as
   `pending`. Loop.

---

## Measurement procedure

1. **Build + push image** from repo root (reuse cargo cache via the build mount):
   ```
   nsc build --push -n nscr.io/k4jkul01t5rr0/henyey:opt-$SID-<label> . -f Dockerfile
   ```
   (`k4jkul01t5rr0` is the workspace registry; confirm with `nsc workspace describe`
   if a push 401s.)
2. **Clear pods** between runs: `nsc kubectl "$INST" -n default delete statefulset --all`,
   wait until 0 pods.
3. **Screen** (short probe single step at `ceil(current_best*1.05)`) — see
   Accept/reject bar for the exact env. Fail → reject, done (~2 min).
4. **Gate** (sustained single step at the same level) — see Accept/reject bar.
   Pass → accept; then ladder single steps upward (+5% each) to find the new
   sustained edge while the image is hot.
5. **Parse**: `grep -E "Found max|Run failed" "$RUN/mission-<label>.log"`.
   Artifacts (`artifacts-<label>/`) keep the LAST step's pod logs — run
   forensics per label; use distinct labels per rung so artifacts persist.
6. **Health**: between polls, `nsc ssh "$INST" -- sh -c 'uptime; free -h | grep Mem; df -h / | tail -1'`
   (flag sustained load >28, mem-available <5Gi, disk >85%).

Short-probe validation note: `SSC_MAXTPS_TXS_MULTIPLIER=60` (1-min offer
window) reproduces the burst ceiling within ~2%; `SSC_LOADGEN_FASTFAIL_LEDGERS=10`
cuts failing-step cost from ~3.5 min to ~1.9 min without changing the converged
max. Sustained (MULT=300, 5-min) is the acceptance currency.

---

## PR per accepted change

Per `AGENTS.md`/`CLAUDE.md`:
```
git add <files> && git commit \
  -m "<imperative summary of the optimization>" \
  -m "maxtps-optimize iter N: <hypothesis>; sustained <before>→<after> tx/s (+X%). Parity: <tests run>." \
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
2. **Finalize the run doc**: summary table (all hypotheses + statuses),
   cumulative gain (baseline → final, burst AND sustained), the list of opened
   PRs, and — mandatory — the **final bottleneck attribution**: "network-bound:
   yes/no" with the evidence (calibration numbers + cycle decomposition at the
   edge). If not network-bound, name the binder for the next campaign.
3. **Teardown**: `nsc instance destroy "$INST" --force` and verify it's gone
   (`nsc instance list`). Revert any uncommitted local harness patches.
4. **Report** to the operator: baseline → final (burst + sustained), accepted vs
   rejected count, PR URLs, the bottleneck attribution, and the top remaining
   `pending` hypotheses.

---

## Seeded hypothesis backlog (2026-07-03)

Ordered; re-rank from evidence each iteration. Items 1-3 are the mandatory
opening moves.

1. **(baseline, do first)** Re-measure burst + sustained on the post-#3712
   image (burst 1510 predates the WAL fix and may rise), fresh same-instance
   core 2×2 reference, and Phase-0 network calibration.
2. **Per-tx persist off the close path** (top known binder; core has no
   per-tx SQL at all): async background writer + `validator` config profile
   that disables per-tx row storage; missions run the profile. The
   whole-ledger `tx_history_entry`/`tx_result_entry` blobs and scphistory must
   keep working (history publish reads them) — consider moving the entire
   persist txn off-path behind an ordering guarantee. Files:
   `crates/app/src/app/ledger_close.rs` (`serialize_and_write_to_db`),
   `crates/db`.
3. **Cycle-time attribution instrumentation** — the definition-of-done
   tooling: full trigger→…→next-trigger decomposition + veth byte/packet
   counters, built on the existing `maxtps_*` targets.
4. **Trigger-skew compensation** (core `HerderImpl::ledgerClosed` arms
   `5s − (now − mLastTrigger)`): retest under the sustained gate. The old
   burst-mode rejection (−1.1%, iter 2 of 2026-07-02) is within noise and
   predates the WAL fix; residual apply-variance skew was still visible in the
   final forensics.
5. **Nomination pipeline latency**: `build_value` 130-250 ms + candidate fetch
   ~150 ms are serialized post-trigger inside the 1 s round-1 budget.
   Staleness-safe prebuild only (rebuild at trigger if the queue grew beyond a
   threshold) — the naive prebuild is a documented #3638 liability, see revert
   1afc810a.
6. **Parallel apply**: execute disjoint-account classic payments in parallel
   with bit-identical serial-equivalent results/meta/hashes (henyey's
   structural advantage; core applies serially).
7. **SCP intake/verify contention**: ~90 ms verify queueing at ballot bursts;
   ~0.75 ms/envelope serial processing on the app loop.
8. **Flood path throughput**: outbound batch coalescing (PR #3701),
   demand-service unfulfilled counters, advert/demand cadence at 2k+ tx/s.
9. **Queue/ban semantics under backlog**: open-loop applied-rate framing from
   #3705; wedge dynamics above the ceiling (age-2 collisions are the loadgen
   death mode).

Refuted dead ends — do NOT re-chase without new evidence: laggard per-node
queues (cross-node spread ~4%), leader-computation divergence (symptom of
trigger skew), tx-set validate-on-fetch cost (15-25 ms), raw-network limits
at current rates (~950× byte headroom measured, but re-calibrate per
instance), `tx_queue_banned` growth (benign by design).

---

## Run-document template

Create at `docs/maxtps-optimization/<UTC-date>-frontier.md`:

```markdown
# maxtps-optimize run — <UTC date>, frontier mode

- Session: <SID> · Instance: <INST> (nsc 32×64) · Base commit: <sha>
- Network calibration: <throughput> / <RTT> pod-to-pod (measured this instance)
- Methodology: MissionMaxTPSClassic, 23-node tier-1;
  screen = short probe (MULT=60), accept gate = sustained (MULT=300, 5-min);
  --install-network-delay false. Accept: Δ > 5% sustained + parity green.
- **Current best (sustained): <N> tx/s** @ <commit/PR> · Burst: <B> tx/s
- Core reference (same instance): burst <X> / sustained <Y>

## Hypotheses

| # | hypothesis | evidence/instrumentation | burst | sustained | Δ vs best | status | notes / next |
|---|------------|--------------------------|-------|-----------|-----------|--------|--------------|
| 0 | baseline   | —                        | <B>   | <N>       | —         | —      | base image |
| 1 | …          | …                        | …     | …         | …         | pending| … |

## Summary (filled at end)
- Baseline → final: burst <A>→<B>, sustained <C>→<D> (+X%, M of K accepted)
- PRs: #… , #…
- **Bottleneck attribution: network-bound <yes/no>** — evidence: …
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
  (network-bound proof / `$TARGET` / `$MAX_ITERS` / hard infra limit) — not on
  uncertainty or lack of a surgical lever. Out of code levers ⇒ run the next
  diagnostic rung, don't stop.
- Parity surface is sacred (`docs/PARITY.md`); 5 s cadence fixed; metrics /
  internal architecture / perf are free. Core is a reference, not a ceiling.
- **Attribute first, then remove the named binder** — henyey-specific or
  shared, it counts if wire bytes and parity hold. Don't optimize on a hunch.
- **Sustained is the acceptance currency.** Screen short (cheap reject), gate
  sustained (5-min single step). Same-instance A/B only; re-baseline after any
  re-provision.
- One hypothesis per iteration for clean attribution; **big swings welcome**
  but commits atomic/minimal — one accepted optimization = one focused PR.
- One reused instance kept alive for the whole run; calibrate its network in
  Phase 0; distinct artifact labels per rung so failure forensics persist.
- Self-drive across long build/run steps via background tasks + `ScheduleWakeup`;
  run-doc is resumable state.
- Tear the instance down **only at the end** (it costs money while alive).
