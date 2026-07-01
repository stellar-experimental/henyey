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
cluster (--install-network-delay false), despite fast per-message processing.

### Additional eliminations (2026-06-30, round 2)
- **Tx-set validation cost**: measured `maxtps_txset_validate` = ~46 ms median / 69 ms for big sets
  (>2000 tx), and only ~0.5–1 validations per node per ledger (906 total / 23 nodes / ~70 ledgers).
  So validation contributes ~40–70 ms/ledger, NOT the 300–1000 ms. REFUTED as the dominant cost.
- **Redundant signature re-verification**: REFUTED — henyey HAS a global ed25519 sig-verify cache
  (`crates/crypto/src/signature.rs`, 250_000 entries, == core's `gVerifySigCache`), far larger than
  the maxtps working set, so sig checks during nomination-validate are cache hits, not re-verifies.
- **SCP queued behind tx-flood**: REFUTED — inbound SCP envelopes use a DEDICATED overlay channel;
  the shared flood `message_rx` arm explicitly skips `ScpMessage` (lifecycle.rs:935–945).

ALL measurable work-units are ruled out (build 30 ms, setup 11 ms, validation 69 ms×~1, sig cached;
no thread >46%, recv ~0 ms). The 300–1000 ms slow round-1 convergence on ~half the ledgers is
therefore genuine SCP nomination message round-trip / convergence dynamics (multi-round adopt-and-
revote as higher-priority leader values propagate, and/or outbound emit latency through the
one-per-iteration `scp_envelope` broadcast arm), not a single fixable work-unit. Both henyey and core
do divergent-tx-set nomination; core converges faster for a still-unidentified reason.

NEXT (deep, fresh-context): trace a single slow slot's nomination envelopes end-to-end with
timestamps — self emit → broadcast (scp_envelope arm) → peer recv (dedicated SCP channel) → process →
re-emit → accept-nominate quorum → confirm → ballot — to find where the 300–1000 ms accrues, and
compare round-trip count + per-hop latency against stellar-core. Instrumentation on throwaway branch:
maxtps_nom_round, maxtps_nom_leaders, maxtps_setup_decomp, maxtps_build_decomp, maxtps_txset_validate.

## Adversarial-architect hypotheses (2026-06-30) — NEW leads, not yet tested

KEY REFRAME (challenges elimination #4): "no thread >46% CPU" does NOT rule out the event loop —
it measures CPU *saturation*, not tokio `select!` *scheduling/queuing latency*. An arm can wait many
ms to be selected while the core is <50% busy. "Every work-unit is fast yet wall-clock isn't" is the
SIGNATURE of scheduling/cross-task-round-trip latency, not of genuine SCP message work. Ranked:

- **H1 (top): inbound SCP is drained one-envelope-per-`select!`-iteration through a NON-biased outer
  select, competing with the saturated tx-flood arm, plus a cross-task verify-worker round-trip.**
  lifecycle.rs:852 (one-per-iter SCP recv), :938 (flood arm; outer select NOT `biased;`), :2353-2495
  (`pump_scp_intake` ships envelope to verify worker and returns; result comes back via verified_rx).
  tokio select picks a uniformly-random ready arm; under flood the SCP arms lose coin-flips → ms of
  scheduling wait per envelope × 22 peers. BIMODAL because flood pressure + envelope size both scale
  with tx-set size, which is network-deterministic per slot → same ~30% slots slow on all nodes.
  TEST: (a) histogram received_at→entry-to-process_verified per envelope; (b) **decisive A/B: add
  `biased;` to the outer select with consensus arms above message_rx, re-run**; (c) count loop
  iterations + flood-arm wins between first-emit and ballot-start. FIX: biased select / batch-drain
  the scp recv arm / verify inline on sig-cache hit (skip the worker round-trip).
- **H2: trigger feedback loop.** consensus.rs:438-441 arms next nomination at `prepare_start(N-1) +
  expected_close` (Instant-relative, not absolute grid) → a slow slot pushes the next trigger late;
  jitter accumulates, can produce a 2-cycle oscillation (matches "~half slow" better than 30%).
  TEST (do FIRST, disambiguates everything): log absolute wall-clocks per slot — timer-armed,
  timer-fired, first-self-emit, ballot-start. `armed_delay=fired-armed`, `convergence=ballot-fired`.
  Late `fired` vs LCL_close+5s ⇒ H2; on-grid `fired` + inflated `convergence` ⇒ H1/H3/H4.
  FIX: anchor trigger to absolute `lcl_close_time + expected_close`, not local prepare_start Instant.
- **H3: verify-worker `reserve().await` (lifecycle.rs:2431) parks the WHOLE event loop** when the
  bounded verify queue is full → stalls trigger/ballot delivery. Co-varies with tx-set size + 22-peer
  burst. TEST (free): bucket existing phase-31 time + verify-queue-depth by fast/slow slot.
- **H4: outbound `scp_envelope` channel (cap 100, mod.rs:1437) drained one-per-iter at :998 with
  `.await` locks (scp_latency/survey_state at :1009/:1013) on the broadcast path** → throttles our
  own re-emit cascade (nomination.rs:807-811) → peers see our votes late → slow quorum. TEST: gauge
  channel depth + emit→broadcast latency on slow slots. FIX: drain-to-exhaustion / locks off hot path.
- **H5 (challenges #3): divergent tx-sets exist for both, but henyey may take more WALL-CLOCK per
  adopt-revote round** (each adoption gated on H1/H4 delivery) even if round COUNT matches core.
  TEST: count adopt-revote rounds per slot vs core; same count → delivery bug (H1/H4), higher count →
  priority-hash divergence (get_node_priority / get_node_weight).
- **H6 (challenges #2's "round-1" claim): `update_round_leaders` bumps `self.round` in a
  leader-growth loop (nomination.rs:1111) WITHOUT a timeout** — "round==1" instrumentation may hide a
  round-1 leader-set recompute cost. TEST: log max_leader_count + round_leaders.len() + growth-bump
  count per slot.
- **H7 (framing): plot per-slot trigger→ballot as a TIME SERIES, not a histogram.** Oscillation
  (period ~2) ⇒ H2; uncorrelated ~30% spikes ⇒ load-driven H1/H3/H4. One log + one plot picks the family.

Recommended cheap iteration order: H2/H7 absolute-clock time-series FIRST (decides direction) →
H1 `biased;` A/B (one-line, parity-safe) → H3 free metric correlation → H4 → H5.

## RESOLVED (2026-06-30): the trigger busy-loop — root cause of slow convergence (PR #3691)

Ran the H2/H7 disambiguator first (per-slot wall-clocks: timer-armed/fired/first-emit/ballot). The
instrumentation immediately surfaced the real bug — NOT in the SCP path but in the **trigger timer**:

- **`maxtps_trigger_fire` fired ~25,000×/ledger/node** on slots being nominated (all `late_us≈0`).
- Mechanism: `handle_scp_timer_event` (consensus.rs ~1531) re-armed the `TriggerNextLedger` timer
  **unconditionally after every fire**. While a slot is still nominating, LCL hasn't advanced, so
  `next_slot` is unchanged and `prepare_start(last)+expectedClose` is already in the past → re-arm
  computes `delay==0` → fires immediately → re-arms → **busy-loop at ~hundreds of Hz for the entire
  nomination→externalize window of every ledger**. Each spin = a bridge-channel event + main-loop
  `select!` wakeup + a `spawn_blocking(trigger_next_ledger)` taking herder locks. This starved the
  single event loop servicing inbound SCP envelopes — exactly the H1 "scheduling/queuing latency"
  mechanism, just sourced from a self-inflicted timer spin rather than the flood arm.
- **Fix: single-shot trigger per ledger** (parity: stellar-core arms `mTriggerTimer` only from
  `lastClosedLedgerIncreased`). Removed the unconditional re-arm; the post-close path arms the next
  trigger; the 1 s maintenance tick backstops gated cases. Regression test
  `test_trigger_event_does_not_rearm_same_slot_spin` (fails pre-fix: 1 spurious self-perpetuating
  trigger).
- **Measurement: 1305 → 1410 tx/s (+8.0%, short-probe).** Verified with a controlled clean A/B (both
  images from current `origin/main`, no instrumentation, same instance): clean-base 1350 → clean-fix
  1437 = **+6.4%** (consistent; the 1305 baseline predated #3690 + carried diag logging). Post-fix
  trigger-fire count = exactly
  1/ledger; trigger fires punctually (fire-lateness mean ~1.3 ms → **H2 ruled out**). Nomination
  convergence at sustainable rates dropped from bimodal 46/300–1000 ms to **~30–60 ms median**; the
  residual p75–p95 tail correlates 1:1 with the over-capacity probe steps (slots 14–22 = the failing
  1440 step), i.e. expected stall when binary-search pushes past capacity, not a steady-state defect.
- **H3/H4/H5/H6 were alternative explanations for the same now-resolved symptom** → no longer needed.
  H7 time-series confirmed the slow window is a single contiguous over-rate probe, not oscillation.

## Remaining architect hypotheses — evaluated post-fix (2026-06-30)

With the spin fixed, convergence is ~30–60 ms median at sustainable rates. H3–H6 were all competing
explanations for the slow convergence the spin actually caused, so they are now largely moot. Verdicts
after code inspection + stellar-core parity check:

- **H2 (late trigger): RULED OUT.** Post-fix `maxtps_trigger_fire` lateness mean ~1.3 ms — the timer
  fires punctually; the trigger is not late.
- **H3 (`reserve().await` parks the event loop): real pattern, latent only.** `pump_scp_intake` does
  run inline in the main `select!` (lifecycle.rs:866), so a full verify-input queue *could* park the
  loop — but the biased branch drains verified output meanwhile, and with the 250k sig-verify cache the
  worker keeps up, so the queue does not fill in steady state. De-inlining the pump is a non-trivial,
  parity-sensitive refactor (more complex, not a simplification) with no measured win → no PR.
- **H4 (metric locks on the outbound broadcast path): minor, no win.** The `scp_envelope` arm takes two
  async `RwLock` writes (`scp_latency`, `survey_state`, both metrics) before `overlay.broadcast()`.
  Pure lock overhead is ~µs vs the 30–60 ms convergence; not a throttle now. Moving them off-path is a
  refactor, not a clear simplification, and unmeasured → no PR.
- **H5 (more wall-clock per adopt-revote round): moot.** Fast ~1-round-trip convergence (30–60 ms)
  shows henyey is not doing excess nomination rounds; this was a symptom of the spin.
- **H6 (`update_round_leaders` bumps `self.round` without timeout): PARITY-CORRECT.** stellar-core does
  the same `mRoundNumber++` inside `updateRoundLeaders` (NominationProtocol.cpp:295), mirrored at
  nomination.rs:1122. Not a bug → no change.
- **H7 (time-series framing): used.** Confirmed the slow window is a single contiguous over-capacity
  probe step, not a period-2 oscillation.

Net: the trigger busy-loop (H1 family) was the single root cause; none of H3–H6 warrant a PR (ruled
out / parity-correct / latent-only with no measured gain and no code simplification).

## 2nd adversarial round + H7 ceiling decomposition + persist A/B (2026-06-30, instance j02eh72amr380)

A second adversarial architect attacked the "no resource exhausted / apply overlaps the wait" framing
and (correctly) noted the close pipeline is **single-flight** (`close_pipeline.rs`: a new close can't
start until the prior persist completes) and classic apply is a **single serial loop** (tx_set.rs:171)
— so <46% aggregate CPU is one saturated critical thread, not spare capacity. It predicted the floor
is `convergence + apply + persist` (load-dependent) and ranked SQLite-persist hypotheses first.

**H7 ceiling decomposition** (existing `maxtps_close`/`maxtps_persist` logs, passing 1430 vs failing 1455):

| phase (ms, median) | 1430 PASS | 1455 FAIL |
|---|--:|--:|
| apply (tx_exec) | 178 | 174 |
| bucket add_batch | 9.5 | 10.3 |
| close-side commit/meta/header | ~0 | ~0 |
| **persist total** | **386** | **796** |
| ↳ **DB commit** | **227** | **522** |
| cadence | 5.83 s | 5.98 s |

Persist DB-commit was the biggest single cost and *doubled* under load at flat tx_count (~7800) — the
signature of fsync/WAL-contention, pointing at SQLite `synchronous=FULL`.

**H1 test — `synchronous=NORMAL` + `mmap_size` (REJECTED).** Mechanism confirmed: commit 227→160 ms,
persist 386→254 ms. But controlled same-instance A/B: **FULL 1492 vs NORMAL+mmap 1481 — no gain**
(marginally lower, within noise). Reverted. **Conclusion: persist OVERLAPS the inter-ledger 5 s wait
and is NOT on the binding critical path** — cutting it doesn't move max TPS. This *refutes* the 2nd
architect's central thesis (and confirms the original "apply/persist overlaps the wait" was right). The
single-flight pipeline (apply+persist ≈ 0.5 s) has ~10× headroom against the 5 s cadence, so its speed
doesn't bind. By the same logic the remaining persist/apply hypotheses (H2 overlap-persist, H3 XDR-off-txn,
H4 bucket, H5 per-tx) are all **moot** for max TPS.

**What actually binds:** the tx-set cap (`max_tx_set_size`=15000) is NOT binding either — fill peaks at
~67% (max ~10098 txns/ledger, loaded median ~8000). The ceiling is the **sustained applied-txns-per-ledger
vs offered load** (loadgen "not complete within 10 ledgers"): at ~1500 tx/s the offered backlog can't fully
drain in the completion window. With apply/persist/cap all ruled out, the residual lever (if any) is in
**tx dissemination / queue-fill / flooding throughput across the 23 nodes**, or load-stretched cadence —
NOT the close pipeline. henyey on this (faster) instance = **1492** vs core ~1531 (~97.5%).

## #3681 flow-control live-maxTxSetSize trim — implemented, tested, REJECTED (regresses ~3%)

Hypothesis (dissemination angle): the overlay outbound-queue trim uses a static
`config.max_tx_set_size_ops=10000` (`flow_control.rs`) instead of the live last-closed-ledger
`maxTxSetSize`; since the maxtps network runs `maxTxSetSize=15000 > 10000`, the static cap might
over-trim and throttle tx dissemination. Sibling of the merged #3679 (herder flood budget).

**Implemented** (branch `fix/3681-flowcontrol-live-maxtxset`, NOT merged): shared `Arc<AtomicU32>`
`max_tx_set_size_ops` app→`OverlayManager`→`SharedPeerState`→`FlowControl`, refreshed on
startup/catchup/each close from `last_max_tx_set_size_ops()` (mirrors `max_tx_size_bytes`); the trim
reads it live (matching core's `getLastMaxTxSetSizeOps()`). Regression test passes.

**Controlled A/B (nsc 32×64, short-probe, interleaved to control run-order) — it REGRESSES:**

| run | image | max tx/s |
|---|---|--:|
| 1 | base (static 10000) | 1489 |
| 2 | #3681 (live 15000) | 1444 |
| 3 | base (static 10000) | 1510 |
| 4 | #3681 (live 15000) | 1455 |

base mean **1500** vs #3681 mean **~1452** = **−3.3%**, non-overlapping (base min 1489 > #3681 max 1455);
instance was stable/improving across runs, so not run-order drift.

**Why it regresses (two findings):** (1) At the ~1500 ceiling, txns/ledger ≈ 7,445 — *below* both 10000
and 15000 — so the trim was never the dissemination cap (the earlier "fill peaks ~10,098 ≈ 10,000"
correlation was coincidence). (2) Raising the clear-on-overflow trim limit lets per-peer outbound queues
grow 50% larger before load-shedding; at the ceiling, holding more backlog instead of shedding earlier
makes the node *less* responsive → lower sustainable throughput. Static 10000 sheds earlier, performs better.

**Verdict: keep static 10000; do NOT land #3681.** Flow-control queue sizing is DIVERGEABLE
(overlay-internal, not observable bytes), so henyey need not match core's live value here — and the static
value is the better-performing tuned choice. Lesson: a parity-hygiene change on a divergeable knob can
hurt perf; test before landing. (Contrast #3679, the merged sibling on the herder flood budget, which helped.)

**Next (per standing mandate):** the close pipeline, tx-set cap, and flow-control trim are all ruled out.
The bottleneck hunt continues toward the dissemination/flooding path and ultimately RAW NETWORKING —
we only stop when pod-to-pod bandwidth / TCP / kernel-network on the single 32-core host is proven the wall.

## T7 event-loop batch-drain — implemented + tested, REFUTED (no gain) (2026-07-01)

T5's aggressive-flooding regression pointed at per-message tokio wakeup/`select!`-iteration overhead on
the flood-ingest path. Confirmed the mechanism: the inbound broadcast (tx-flood) arm processed **one
message per select! wakeup** (lifecycle.rs `message_rx.recv()`), so ~7,500 inbound tx/ledger = ~7,500 full
loop iterations/ledger. Implemented a **batch-drain** (mirroring the existing SCP `MAX_DRAIN_PER_TICK=200`
idiom): on wakeup, process the message then `try_recv()`-drain up to 200 more before yielding, via a new
`dispatch_broadcast_overlay_msg` helper. Compiles, 38 lifecycle tests pass.

**Controlled same-instance A/B (short-probe):**

| config | max tx/s |
|---|--:|
| baseline (no drain) | **1446** |
| T7 batch-drain | **1423** |

**No gain (−1.6%, within noise).** The wakeup overhead is real (T5 showed doubling message rate hurts)
but at the *default* flood rate the ingest wakeups are not the binding constraint — batching them frees
nothing measurable. (This instance ran ~3% low overall — baseline 1446 vs ~1493 on the prior box —
confirming the ±2-3% instance-to-instance variance that now dominates.) The T7 batch-drain is parity-safe
and a reasonable latency hygiene change, but not a maxtps lever; not landed as an optimization.

## FINAL bottleneck verdict — every lever exhausted

One fix landed (trigger busy-loop #3691, +6.4%, the sole real win). Everything else tested and refuted
with measurement: loadgen offer (T1), tx-set selection/surge (T6), advert-flood budget (T3), flow-control
trim value (#3681, regresses ~3%), apply+persist close pipeline (SQLite A/B, 0 gain), tx-set cap, SCP
convergence (post-fix ~30-60 ms), per-message write coalescing (T2, 0 gain), dissemination timing
(T5, aggressive regresses), event-loop batch-drain (T7, 0 gain), and **RAW NETWORKING (T8, the mandate's
terminal condition: ~950× byte / ~150× packet headroom on the 64 KB-MTU single-host loopback — definitively
NOT the wall).**

**Conclusion:** henyey sits at **~1446-1510 vs core ~1531 (~95-98%)**, and the residual gap is now **within
the ±2-3% instance/run measurement noise** — no lever moves it beyond noise, and raw networking (the only
"give up" condition) is proven to have 2-3 orders of magnitude of headroom. The ceiling is the shared 5 s
close-time cadence (a parity constraint) × the txns/ledger the 23-node consensus reliably agrees-and-applies;
henyey matches core there to within noise. There is no identified fixable bottleneck remaining.

## Summary
- Baseline → current: 462 → **1410 tx/s** (5 PRs: #3679, #3682, #3683, #3684, **#3691**). henyey is
  now ≈92% of core's ~1531 on the same rig (was 84%).
- The slow/bimodal SCP nomination convergence — the entire residual "controllable ~0.5 s overhead" —
  was the trigger busy-loop, now fixed. At sustainable rates convergence is ~30–60 ms.
- Remaining gap to core (~1410 vs ~1531) is the txns/ledger ceiling under the shared 5 s close-time
  target (~95% idle CPU); no identified single-bug cause, not a saturated resource. henyey's
  tokio-concurrency advantage stays masked by the 5 s cadence and would surface only at shorter close
  times where per-ledger processing speed dominates over wall-clock waiting.
