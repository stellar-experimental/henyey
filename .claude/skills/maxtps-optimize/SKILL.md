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

**Core is not the ceiling.** stellar-core's numbers (full-window [2400, 2700),
5-min ~3018 @92k accounts on this rig, 2026-07-03) are a diagnostic reference,
not the target. henyey's networking and apply stack are more parallel than
core's single-threaded main loop; within the fixed overlay/SCP wire protocols
henyey should be able to *beat* core. The run is done when the bottleneck is
provably the simulated network itself — not any henyey code path.

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
  runs (~10 min screens / ~25-30 min full-window gate) are long; launch them in
  the background and use `ScheduleWakeup` to resume. The run-doc is your durable
  state — write enough to it each step that any resume can continue without
  operator input. Operator messages may arrive, but you must never *wait* for
  them.
- **Keep the instance alive for the whole run.** Provision ONE nsc instance and
  reuse it across every iteration. Do NOT tear it down between iterations or to
  "pause" — only at a true Stop condition (extend its duration if it nears
  expiry, or re-provision if it died).

Baseline context (2026-07-03, post-#3714..#3720 and the account-count
experiments on issue #3719): with rate-scaled accounts, henyey's 5-min edge is
**2156** tx/s (46k accounts) and burst ≥1673; its **full-window edge is
< 1560** — every full-window run from 1560 to 2100 died at ~7-9 min on
storestate write-volume compounding (`scp-persist-purge` WAL holds 21-34 s,
24 s WAL checkpoints, an 87 s close). stellar-core on the same rig:
full-window **[2400, 2700)**, 5-min ~3018 (92k accounts). ALL older
23k-account numbers (henyey 1560 / core 1526 and earlier) are deprecated as
account-cadence-bound — do not compare against them. Prior campaigns' root
causes and refuted dead ends are in
`docs/maxtps-optimization/2026-07-02-target2000.md`,
`docs/maxtps-optimization/2026-07-03-frontier.md`, issue #3719's closing
comment, and memory `project_maxtps_frontier_result.md` — read them before
re-chasing anything.

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
   - **Accounts scale with load** (2026-07-03, issue #3719): the genesis
     account count encodes a hidden inclusion-latency SLO — each account
     resubmits every `accounts/rate` seconds and PayPregenerated fatal-rejects
     at ~2-3 ledgers pending. The old static 23000 corrupted every number
     above ~1500 tx/s (core measured +98% once unbound). Set
     `GENESIS_ACCOUNTS = MAX_TX_RATE × 26`, rounded UP to the nearest 1000
     (≥4.7 ledgers of cadence slack at the window ceiling; sized to
     `MAX_TX_RATE` because genesis happens once per network boot).
   - **Cadence-artifact diagnostic**: a run-fatal
     `pay_pregenerated fatal reject … pending_age=2-3` while ledgers are full
     and closes are fast (~200 ms) is the account-cadence artifact, NOT a node
     limit — raise accounts and re-measure before chasing code.
   - **Comparability**: numbers are only comparable at the same account count
     (and same instance). Record `GENESIS_ACCOUNTS` with every measurement;
     the run-doc table has an `accounts` column.
3. **One hypothesis per iteration + proof** — test exactly one mechanism at a
   time so the measured Δ is attributable, and keep a change only if it is
   *proven* to give a meaningful gain (>5%, below, gated on the full window).
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
5. **Efficiency** — cheap probes screen (burst, then 5-min), the full-window
   probe only gates accepts, one reused instance. Efficiency governs *load
   runs*; it does not discourage image rebuilds for instrumentation or code
   changes.

## Definition of done — the network-bound proof (frontier mode)

Frontier mode ends successfully when the run doc contains BOTH:

1. **Network calibration (Phase 0, per instance).** Measure raw pod-to-pod
   capacity on the instance (`iperf3` between two pods, or `dd | nc`), both
   throughput and RTT. Instances drift and differ — calibrate every
   provisioned instance, don't reuse old numbers. From it, derive the tx-byte
   ceiling estimate for the 23-node flood fan-out at the candidate rate
   (~200 B/payment × fan-out + tx-set fetch traffic + SCP traffic).
2. **Cycle-time attribution at the edge.** At the highest passing full-window
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

`current_best` is the **full-window** sustained number
(`SSC_MAXTPS_TXS_MULTIPLIER=1000`, the canonical ~16.7-min offer window).

Every step below is a single step at `X = ceil(current_best * 1.05)`
(`TX_RATE=X-7`, `MAX_TX_RATE=X+8`, `SSC_LOADGEN_FASTFAIL_LEDGERS=10`,
`GENESIS_ACCOUNTS = MAX_TX_RATE × 26` rounded up to the nearest 1000):

- **Screen 1 — burst** (cheap reject): `SSC_MAXTPS_TXS_MULTIPLIER=60`,
  `NUM_PREGEN=(X+8)*65`. Fails → **reject immediately** (~2 min spent).
- **Screen 2 — 5-min** (mid reject): burst passed →
  `SSC_MAXTPS_TXS_MULTIPLIER=300`, `NUM_PREGEN=(X+8)*305` (~8 min pass /
  ~4 min fail after boot). Fails → **reject** (~10 min spent). Never run the
  full-window gate for a change that can't clear both screens.
- **Accept gate — full window** (required for accept): both screens passed →
  `SSC_MAXTPS_TXS_MULTIPLIER=1000`, `NUM_PREGEN=(X+8)*1005` (~25 min pass
  incl. boot; failures typically show by ~7-15 min). **Accept iff the
  full-window step passes** and the parity gate is green; then
  `current_best = X` and optionally ladder upward (repeat at +5%) while the
  image is deployed — ladder with the full-window step, since that is the
  currency.
- Single-step pass/fail near the edge is stochastic ±3-4%: compare only
  same-instance A/B at the same account count, and re-baseline `current_best`
  after any re-provision (instance drift is a documented fact — see the
  2026-07-02 run doc).
- Rejected changes are reverted; parity-safe diagnostic instrumentation may
  stay.

Why full-window-gated: the 2026-07-02 campaign proved burst probes accept
changes that die under sustained load, and the 2026-07-03 experiments proved
the 5-min probe over-reports BOTH implementations — henyey rates that passed
5-min gates (1560-2100) all died at ~7-9 min of the canonical window
(storestate write compounding), and core drops ~20% (5-min ~3018 vs
full-window [2400, 2700)). Shorter probes are screens, never accepts.

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
   --probe-timeout 240`, and `--genesis-test-account-count` driven by the
   `GENESIS_ACCOUNTS` env var — always set it per the account-scaling rule).
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
   all three tiers — burst (MULT=60), 5-min (MULT=300), and full-window
   (MULT=1000) — stepping upward from the last known full-window bound, with
   `GENESIS_ACCOUNTS` scaled to each window's `MAX_TX_RATE`. Tag
   `:opt-$SID-base`. Record the full-window number as `current_best`; note
   burst + 5-min numbers and `base_commit` (they contextualize which tier a
   later change moves). Optionally run the same-instance core reference at
   matching account counts when the campaign's claims will be stated relative
   to core.
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
5. **Measure** — rebuild the image, deploy, run the two screens (burst,
   5-min) then the full-window accept gate per the Accept/reject bar.
6. **Decide & land**:
   - **Accept**: set `current_best`; mark `accepted`; commit on the branch and
     **open a PR**; rebuild the base image tag so the next iteration stacks on
     top. Ladder upward (full-window steps) while deployed to find the new
     edge.
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
3. **Screens** (burst then 5-min single steps at `ceil(current_best*1.05)`,
   `GENESIS_ACCOUNTS` scaled) — see Accept/reject bar for the exact env.
   Either fails → reject, done (~2-10 min).
4. **Accept gate** (full-window single step at the same level) — see
   Accept/reject bar. Pass → accept; then ladder full-window steps upward
   (+5% each) to find the new edge while the image is hot.
5. **Parse**: `grep -E "Found max|Run failed" "$RUN/mission-<label>.log"`.
   Artifacts (`artifacts-<label>/`) keep the LAST step's pod logs — run
   forensics per label; use distinct labels per rung so artifacts persist.
6. **Health**: between polls, `nsc ssh "$INST" -- sh -c 'uptime; free -h | grep Mem; df -h / | tail -1'`
   (flag sustained load >28, mem-available <5Gi, disk >85%).

Probe validation notes: `SSC_MAXTPS_TXS_MULTIPLIER=60` (1-min offer window)
reproduces the burst ceiling within ~2%; MULT=300 (5-min) catches backlog
spirals that burst hides but still over-reports vs the canonical window
(2026-07-03: henyey 5-min-passing rates died at ~7-9 min; core -20%);
`SSC_LOADGEN_FASTFAIL_LEDGERS=10` cuts failing-step cost without changing the
converged max. The full window (MULT=1000, ~16.7 min) is the acceptance
currency.

---

## PR per accepted change

Per `AGENTS.md`/`CLAUDE.md`:
```
git add <files> && git commit \
  -m "<imperative summary of the optimization>" \
  -m "maxtps-optimize iter N: <hypothesis>; full-window sustained <before>→<after> tx/s (+X%). Parity: <tests run>." \
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
   cumulative gain (baseline → final, burst AND full-window), the list of opened
   PRs, and — mandatory — the **final bottleneck attribution**: "network-bound:
   yes/no" with the evidence (calibration numbers + cycle decomposition at the
   edge). If not network-bound, name the binder for the next campaign.
3. **Teardown**: `nsc instance destroy "$INST" --force` and verify it's gone
   (`nsc instance list`). Revert any uncommitted local harness patches.
4. **Report** to the operator: baseline → final (burst + 5-min + full-window),
   accepted vs rejected count, PR URLs, the bottleneck attribution, and the
   top remaining `pending` hypotheses.

---

## Seeded hypothesis backlog (2026-07-03, post-#3719 experiments)

Ordered; re-rank from evidence each iteration. Items 1-2 are the mandatory
opening moves.

1. **(baseline, do first)** Full three-tier baseline (burst / 5-min /
   full-window) with rate-scaled accounts on current HEAD, same-instance core
   reference at matching account counts, and Phase-0 network calibration.
2. **Overwrite-style SCP state persistence** (THE named full-window binder,
   2026-07-03): henyey accumulates every slot's multi-MB candidate tx sets in
   `storestate` and purges periodically; stellar-core overwrites ~2 rows per
   slot parity. At 10k+ tx ledgers this compounds until `scp-persist-purge`
   holds the WAL 21-34 s, passive checkpoints take 24 s, and a close takes
   87 s (~7-9 min into every full-window run ≥1560 tx/s). The chunked purge
   (PR #3720) bounds the lock holds but not the volume — redesign the
   persistence shape (bounded slots, overwrite semantics, or off-WAL
   storage). Forensics: issue #3719 closing comment, artifacts
   `full-henyey{1560,1700,1950,2100}*`. Files:
   `crates/db/src/scp_persistence.rs`, `crates/db/src/queries/scp.rs`,
   `crates/herder/src/persistence.rs` (`persist_scp_state`).
3. **Cycle-time attribution instrumentation** — the definition-of-done
   tooling: full trigger→…→next-trigger decomposition + veth byte/packet
   counters, built on the existing `maxtps_*` targets (incl. `maxtps_tail`).
4. **Nomination pipeline latency**: `build_value` + candidate fetch are
   serialized post-trigger inside the 1 s round-1 budget. Staleness-safe
   prebuild only — the naive prebuild is a documented #3638 liability, see
   revert 1afc810a.
5. **SCP intake/verify contention**: verify queueing at ballot bursts;
   serial envelope processing on the app loop.
6. **Trigger-skew compensation** (core `HerderImpl::ledgerClosed` arms
   `5s − (now − mLastTrigger)`): retest under the full-window gate.
7. **Per-tx inclusion turnaround** (submit→apply, currently ~2 ledgers
   worst-case per seq-chain link): shrinking it raises the measurable ceiling
   at any account count and helps real chained-submitter latency.

Resolved / refuted — do NOT re-chase without new evidence:
- **Account-cadence wedge** (`pending_age=2-3` fatal with healthy ledgers) —
  harness artifact, fixed by the account-scaling rule (issue #3719).
- **Demand-path stranding** — fixed: owned/banned demand discard + deferred
  responses (#3717), same-peer re-demand + send-failure unmark (#3718),
  stale-pending self-heal (#3720).
- **Per-tx SQL write volume** — removed on validators (#3714, #3716:
  `store_rpc_data`); maintenance-delete WAL storm chunked (#3715).
- **Apply-path optimizations** — operator directive 2026-07-03: do not
  pursue (apply is ~200-400 ms of a 5 s cadence; highest parity risk).
- Laggard per-node queues, leader-computation divergence, tx-set
  validate-on-fetch cost, raw-network limits (~45× byte headroom measured
  2026-07-03 — re-calibrate per instance), `tx_queue_banned` growth (benign).
- Forensics gotcha: `sent_to` in the stranded-tx diag counts BIDIRECTIONAL
  advert marks — peer echo-adverts prove the peers HAVE the tx, not that the
  origin delivered it.

---

## Run-document template

Create at `docs/maxtps-optimization/<UTC-date>-frontier.md`:

```markdown
# maxtps-optimize run — <UTC date>, frontier mode

- Session: <SID> · Instance: <INST> (nsc 32×64) · Base commit: <sha>
- Network calibration: <throughput> / <RTT> pod-to-pod (measured this instance)
- Methodology: MissionMaxTPSClassic, 23-node tier-1;
  screens = burst (MULT=60) then 5-min (MULT=300); accept gate = full window
  (MULT=1000, ~16.7 min); GENESIS_ACCOUNTS = MAX_TX_RATE × 26 (round up to
  1000); --install-network-delay false. Accept: Δ > 5% full-window + parity
  green.
- **Current best (full-window): <N> tx/s** @ <commit/PR> · 5-min: <M> · Burst: <B>
- Core reference (same instance, matching accounts): full-window <X> / 5-min <Y>

## Hypotheses

| # | hypothesis | evidence/instrumentation | accounts | burst | 5-min | full-window | Δ vs best | status | notes / next |
|---|------------|--------------------------|----------|-------|-------|-------------|-----------|--------|--------------|
| 0 | baseline   | —                        | <A>      | <B>   | <M>   | <N>         | —         | —      | base image |
| 1 | …          | …                        | …        | …     | …     | …           | …         | pending| … |

## Summary (filled at end)
- Baseline → final: burst <A>→<B>, full-window <C>→<D> (+X%, M of K accepted)
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
#   GENESIS_ACCOUNTS SSC_MAXTPS_TXS_MULTIPLIER SSC_LOADGEN_FASTFAIL_LEDGERS)
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
  --num-pregenerated-txs "${NUM_PREGEN:-100000}" \
  --genesis-test-account-count "${GENESIS_ACCOUNTS:?set per account-scaling rule}"
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
- **The full window is the acceptance currency.** Screen with burst then
  5-min (cheap rejects — never accepts), gate with the full-window single step
  (MULT=1000). Accounts scale with the probe rate (MAX_TX_RATE × 26); a
  pending_age fatal with healthy ledgers = cadence artifact, raise accounts.
  Same-instance A/B at the same account count only; re-baseline after any
  re-provision.
- One hypothesis per iteration for clean attribution; **big swings welcome**
  but commits atomic/minimal — one accepted optimization = one focused PR.
- One reused instance kept alive for the whole run; calibrate its network in
  Phase 0; distinct artifact labels per rung so failure forensics persist.
- Self-drive across long build/run steps via background tasks + `ScheduleWakeup`;
  run-doc is resumable state.
- Tear the instance down **only at the end** (it costs money while alive).
