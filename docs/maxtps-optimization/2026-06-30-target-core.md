# maxtps-optimize run — 2026-06-29/30, target: match/surpass stellar-core (~1531 tx/s)

- Session: b4a0fd10 (cont. of 701dcf0a) · Instances: nsc 32×64 (several, all torn down)
- Methodology: MissionMaxTPSClassic, 23-node tier-1, short-probe binary search
  (SSC_MAXTPS_TXS_MULTIPLIER 60–150, SSC_LOADGEN_FASTFAIL_LEDGERS=0),
  `--install-network-delay false`. Accept bar: Δ > 5% over current best.
- **Current best: ~1300 tx/s** (clean, stable, 0 loss) · core reference: ~1531 on same rig.
- Big theme: the original measurement was artifact-laden (read 462–540); fixing the
  loadgen/overlay measurement bugs revealed henyey's true ~1300 (≈84% of core).

## Hypotheses

| # | hypothesis | instrumentation | measurement | Δ vs best | status | notes |
|---|------------|-----------------|-------------|-----------|--------|-------|
| 0 | baseline (artifact-laden) | — | 462 | — | — | depressed by heavy per-tx logging + loadgen bugs |
| 1 | flood budget uses stale config maxTxSetSize, not live ledger | maxtps_txtrace cross-node coverage | 220→350 (clean-light) | +59% | **accepted PR #3679** | parity: core getLastMaxTxSetSizeOps |
| 2 | per-peer rate limits drop flood under load | scrape :11628 + ratelimit_drop | 350→462 | +32% | **accepted PR #3682** | henyey-only flow control; coarse Sybil backstop |
| 3 | loadgen resets curr_preloaded each run → cross-run account misattribution → false on-chain-ahead unsynced | txtrace + maxtps_unsynced | 462→615 | +33% | **accepted PR #3683** | parity: core never resets mCurrPreloadedTransaction |
| 4 | loadgen reloads sentinel account 0 seq per PayPregenerated submit → 1 false-unsynced caps step | maxtps_unsynced direction | 615→1276 | +107% | **accepted PR #3684** | parity: core readTransactionFromFile skips maybeLoad |
| 5 | single tokio event-loop serialization caps throughput | per-thread /proc CPU | refuted | — | rejected | no thread >46%; work spread across ~8 workers |
| 6 | apply (tx_exec, 90% of close) is the lever | maxtps_close phase breakdown | refuted | — | rejected | apply (224ms) overlaps the inter-ledger 5s wait → NOT on cadence critical path |
| 7 | parallelize build_starting_seq_map (~serial account loads) | maxtps_build_decomp | 1300→1293 (no-op) | 0% | rejected/reverted | get_account is in-memory (~ms); wrong target |
| 8 | inter-ledger build/setup overhead (snapshot/cfg-upgrade) caps cadence | maxtps_setup_decomp (snapshot/frozen/seqmap/cfgctx) | pending | — | testing | the ~118ms unattributed setup in build_value_ms |

## Root-cause picture (deep profiling, 2026-06-30)

The MaxTPSClassic ceiling is **ledger-cadence-bound, not resource-bound**:
- cadence ≈ 5.5 s/ledger, only ~250 ms of work → node **idle ~95%** per ledger.
- throughput = txns_per_ledger ÷ cadence. The ~5 s is the network target close time
  (`ledger_target_close_time_ms`, parity — both henyey and core wait ~5 s). It is ~91% of the cadence.
- The next-ledger trigger = `prepare_start(N-1) + expected_ledger_close_duration`. **Apply runs
  after externalize, overlapping the 5 s wait → not on the cadence critical path** (so apply speed
  does NOT change maxTPS).
- The only inter-ledger critical-path cost is `build_value_ms` ≈ 265 ms (trigger→nominate tx-set build):
  - trim_invalid ~105 ms — **already parallelized** (`std::thread::scope`), faster than core's serial path.
  - select ~21 ms, build+hash ~4 ms.
  - **~118 ms unattributed setup** in `build_and_cache_candidate_tx_set` step 1
    (create_snapshot / frozen-key / config-upgrade-ctx) — decomposing now (#8).
- Failing-rate cross-node trace: 0 loss, 0 stranding, 100% applied (coverage median 23). The cap is
  pure completion-timing (sustainable drain), not loss.

## Honest assessment

henyey ≈ 1300 (≈84% of core). The measurement fixes (#1–#4) were the dominant wins (462→1300).
The addressable critical-path build overhead is ~5–9% (→ ~1400). The residual txns/ledger gap
(henyey ~6500 vs core ~7655 at similar cadence) has no identified single-bug cause and is not a
saturated resource. henyey's tokio-concurrency advantage is masked by the 5 s cadence (95% idle) —
it would surface at shorter close times where per-ledger processing speed dominates over wall-clock waiting.

## Full instrumented critical-path decomposition (2026-06-30)

Cadence ≈ 5.5 s/ledger vs the 5.0 s network target (MaxTPSTest assumes 5 s) ⇒ ~0.5 s controllable
overhead. Instrumented every stage of the inter-ledger critical path (trigger → nominate → ballot):

| stage | instrument | steady (median) | tail (p90 / max) |
|---|---|--:|--:|
| create_snapshot | maxtps_setup_decomp | 0 ms | 9 ms |
| load_frozen_key_config | maxtps_setup_decomp | 0 ms | 3 ms |
| build_starting_seq_map | maxtps_setup_decomp | 0 ms | 32 / 176 ms |
| config-upgrade ctx | maxtps_setup_decomp | 0 ms | 0 ms (none armed) |
| select (surge pricing) | maxtps_build_decomp | 13 ms | 177 ms |
| trim_invalid (parallel) | maxtps_build_decomp | 13 ms | 459 ms |
| build + hash | maxtps_build_decomp | 2.5 ms | 52 ms |
| **nomination round (trigger→ballot)** | **maxtps_nom_round** | **46 ms** | **866 ms** |

ROOT CAUSE of the cadence overhead = the **bimodal nomination round**: ~67% of loaded ledgers
converge in ~46 ms, but **~30% take 400–866 ms** (p75 680, p90 866, max 1391; 30% > 400 ms,
16% > 800 ms). That slow third drags the average per-ledger cadence to ~5.4–5.5 s. The slow
values sit below the 1000 ms round-1 nomination timeout, so they are slow *convergence*, not a
clean full timeout — most consistent with round-1 not reaching quorum quickly (peers not echoing
the leader's nomination in time), pushing toward the round timeout before round-2 converges.

Refuted/no-op along the way: single-event-loop saturation (no thread >46%), apply path (overlaps
the 5 s wait), tx-set-build setup (snapshot/frozen/cfgctx all ~0), parallelize build_starting_seq_map
(reverted no-op — get_account is in-memory).

Open lever (next session): why ~30% of nominations converge slowly. Apply-contention hypothesis
NOT supported — slow vs fast nominations have ~equal prior-ledger apply time (close_ms ~270 both),
so it is not "busy applying delays nomination". More likely SCP nomination round-1 not reaching
quorum quickly (peers not echoing the round-1 leader's nominated value in time), pushing toward the
round timeout before round-2 converges — i.e. SCP convergence/leader timing, which is parity-sensitive.
Next step is deeper SCP-nomination instrumentation (per-node: when each node nominates vs when it sees
quorum-accepted; round counter at ballot start) to see whether round-1 genuinely fails quorum or a
specific node/leader lags. Eliminating the slow tail ≈ cadence 5.5→~5.1 s ≈ **+8%** (→ ~1400), still
gated by the 5 s close-time target (network parameter, shared with core).

## Slow-nomination deep-dive (2026-06-30, hypotheses tested on cloud)

The cadence overhead = **slow SCP round-1 nomination convergence on ~half the ledgers**
(per-slot median nom_round: ~46 ms for the fast half, 300–1000 ms for the slow half;
network-wide — every node shows the same ~30% >300 ms). Decisively NARROWED by eliminating:
- **Loadgen leader** (the in-process loadgen node leading the round): REFUTED — slow ledgers
  have a loadgen (*-0) leader 48% vs fast 55% (no correlation; the 7/23≈30% match was coincidence).
- **Timeout escalation**: REFUTED for the majority — 24/30 slow ledgers stay in round 1 (slow
  *convergence*, not the 1000 ms round-1 timeout); only 6/30 escalate to round ≥2.
- **Per-node random tx-set seed → divergent nominations**: REFUTED — stellar-core also uses
  `rand_uniform` for the surge-pricing seed (SurgePricingUtils.cpp), so divergent per-node tx-sets
  are normal for both impls.
- **CPU / single-thread / message-processing**: REFUTED — no thread >46%, recv_message ~0 ms,
  apply overlaps the wait.

So: round-1 nomination takes 300–1000 ms to reach quorum on ~half the ledgers on a fast local
cluster (--install-network-delay false), despite fast per-message processing — a subtle SCP
convergence-dynamics question (likely multi-round re-voting as higher-priority leader values arrive,
and/or close-time/ctValidityOffset acceptance gating). This is the well-scoped next step: trace the
per-message nomination flow (leader emit → vote → accept-nominate quorum → confirm → ballot) on a
slow slot to see where the 300–1000 ms is spent. Instrumentation added (throwaway branch): maxtps_
nom_round (scp_driver.rs record_ballot_start), maxtps_nom_leaders (nomination.rs), maxtps_setup_decomp
+ maxtps_build_decomp.

## Summary
- Baseline → current: 462 → ~1300 tx/s (4 PRs accepted: #3679, #3682, #3683, #3684); measurement
  is now trustworthy (the artifacts were the big wins).
- Bottleneck fully localized: cadence-bound (5.5 s/ledger vs 5 s target, ~95% idle); the controllable
  ~0.5 s overhead is slow SCP round-1 nomination convergence on ~half the ledgers (300–1000 ms vs
  46 ms). Not resource-bound, not loss, not apply, not loadgen, not seed-divergence, not timeout.
- Next (well-scoped): trace per-message nomination convergence on a slow slot to find why round-1
  takes 300–1000 ms; eliminating it ≈ +8% (→ ~1400), still gated by the 5 s close-time target.
