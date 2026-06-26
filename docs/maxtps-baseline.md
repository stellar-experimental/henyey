# MaxTPSClassic core-vs-henyey baseline

**Date:** 2026-06-26
**Harness:** Supercluster `MissionMaxTPSClassic` (vendored)
**Infrastructure:** single Namespace (nsc) `32x64` ephemeral VM, k3s, instance `rfre5ompovqt6`
**Images:** core `stellar-core-testing@sha256:bc1de6bc…`, henyey `nscr.io/k4jkul01t5rr0/henyey:ssc-fix5` (fixes #3627–#3631 applied; #3638 unfixed)

## Headline

| Image  | Canonical max sustainable tx rate |
|--------|-----------------------------------|
| **stellar-core** | **1533 tx/s** |
| **henyey**       | **196 tx/s**  |

On this single contended 32-core VM, stellar-core sustains **~7.8× the tx rate henyey can** under the canonical MaxTPSClassic load profile. This is an honest apples-to-apples comparison: both images ran the identical mission, topology, and per-step load on the same VM, back to back.

> ⚠️ This is **not** a pubnet-representative throughput number for either image (see Caveats). It is a *relative baseline* on one machine, useful for tracking henyey's progress toward core parity over time.

## Methodology

- **Mission:** `MissionMaxTPSClassic` — a binary search for the maximum sustainable transaction rate.
  - **Topology:** 23-node tier-1 (`StableApproximateTier1CoreSets`: orgs bd, cq, kb, lo, sp, sdf, wx) on a single k3s node.
  - **Load mode:** `PayPregenerated` (forced for ≤30-node runs) — payment transactions are pre-generated into an XDR file and submitted from a fixed pool.
  - **Per-step load (canonical):** `txs = middle * 1000` transactions per step (≈16.7 min/step at the rates explored), i.e. a genuinely *sustained* load rather than a short burst.
  - **Pass/fail:** a step passes if the network applies the full submitted set within the loadgen timeout; on failure the failing node is restarted (PayPregenerated) before the next step.
  - **Convergence:** binary search halts when the high/low gap ≤ 10; the recorded max is the highest rate that passed.
- **Flags:** `--install-network-delay false` (no inter-node latency injected), `--core-http-via-pod-exec`, `--genesis-test-account-count 23000`, `--probe-timeout 240`.
- **Search range:** core `[0,1700]`, henyey `[0,300]` (henyey's range narrowed to its known operating band to keep the search tractable; both used the canonical `middle*1000` load).
- **Resource monitoring:** machine load / free memory / disk sampled every ~5 min via `nsc ssh` (no metrics-server on single-node k3s, so `kubectl top` is unavailable).

## Per-step results

### stellar-core — `[0,1700]`, converged at **1533**

| tx rate | result |
|--------:|--------|
| 850  | ✅ pass |
| 1275 | ✅ pass |
| 1487 | ✅ pass |
| 1513 | ✅ pass |
| 1526 | ✅ pass |
| 1533 | ✅ pass |
| 1540 | ❌ fail |
| 1593 | ❌ fail |

**Core canonical max = 1533 tx/s.** This matches an earlier independent short-load probe (~1531 tx/s), validating the methodology.

### henyey — `[0,300]`, converged at **196**

| tx rate | result |
|--------:|--------|
| 150 | ✅ pass |
| 187 | ✅ pass |
| 196 | ✅ pass |
| 206 | ❌ fail |
| 225 | ❌ fail |

**Henyey canonical max = 196 tx/s.**

Failing steps (206, 225) exhibited the #3638 over-submission signature: the applied-tx count ran well past the step target (~150–180% of target) and the run eventually tripped the loadgen failure path, followed by a node restart. Passing steps applied the target set steadily without that runaway.

## Discovery story — why this baseline exists

An earlier run of this same comparison produced a striking but **false** result: henyey appeared to *exceed* core, "passing" rates of 2012, 2506, even 2753 tx/s. That looked too good to be true — CPU sat near-idle while henyey supposedly out-ran core by ~1.8×.

A read-only adversarial audit of ground truth on the live nodes (txns-applied-per-ledger ÷ close-time, ledger fill %, run-complete semantics) found the cause: at the "passing" 2753, **>90% of ledgers closed empty** and the loadgen still reported success. The harness "pass" was a measurement artifact, not real throughput.

Root cause was **#3631**: henyey's `wait_till_complete` only set the failure flag on the Soroban path; classic Pay/PayPregenerated **timeouts returned without failing**, so runs whose transactions never applied were reported as successful — inflating the ceiling. stellar-core's `waitTillComplete` calls `emitFailure` unconditionally on timeout (`LoadGenerator.cpp:1399-1404,1430-1433`). After fixing #3631 (and four sibling bugs), the honest ceiling collapsed from a fictional ~2753 to the real **196**.

**Lesson:** for performance claims, verify on-ledger application + CPU; never trust a harness "pass" alone.

## Bugs found and fixed

Five drop-in/parity bugs were found and fixed to make henyey runnable and honestly measurable under this mission; a sixth (loadgen pacing) is filed but unfixed and still depresses henyey's number.

| # | Area | Summary | Status |
|---|------|---------|--------|
| #3627 | loadgen CLI | `pregenerate-loadgen-txs` rejected SSC's space-joined `--count N` arg form that core accepts | Fixed — PR #3637 (ready-for-review) |
| #3628 | overlay config | Ignored `TARGET_PEER_CONNECTIONS` / `MAX_ADDITIONAL_PEER_CONNECTIONS` (default max_outbound=8) → couldn't form a 23-node mesh | Fixed — PR #3633 (ready-for-review) |
| #3629 | loadgen CLI | `PayPregenerated` `/generateload` required an explicit `preloadedTransactionsFile`; core defaults it | Fixed — PR #3634 (ready-for-review) |
| #3630 | metrics | `clearmetrics` was a no-op → loadgen meters never reset; per-step counts accumulated and a stale `run_failed` could corrupt the binary search | Fixed — PR #3635 (ready-for-review) |
| #3631 | loadgen | `wait_till_complete` did not fail classic-load timeouts → inflated the measured ceiling (**the artifact above**) | Fixed — PR #3636 (ready-for-review) |
| #3638 | loadgen | Un-paced over-submission: PayPregenerated dumps the file ~6.7× over target instead of pacing at the rate | Filed (backlog), **unfixed** |

## Caveats

- **Single contended VM, not pubnet.** All 23 nodes share one 32-core machine; there is no real network. Absolute numbers do not transfer to a distributed pubnet deployment for either image.
- **No network delay.** `--install-network-delay false` removes inter-node latency, which inflates both numbers relative to a latency-bound network.
- **Henyey's 196 is a lower bound.** It is still depressed by #3638 (un-paced over-submission), which makes high-rate steps fail on submission-side runaway rather than on genuine apply-side capacity. A paced loadgen would likely raise henyey's measured ceiling somewhat.
- **Henyey shows a serial/consensus bottleneck.** Across runs, henyey under-fills ledgers (~27–49%) with idle CPU and occasional `NodeLostSyncException` near capacity — a sign the limiter is serial/consensus handling, not raw compute. Worth a separate perf investigation.

## Baseline statement

**As of 2026-06-26, on a single nsc 32x64 VM under canonical MissionMaxTPSClassic, stellar-core sustains 1533 tx/s and henyey sustains 196 tx/s (~7.8× gap).** Henyey's figure is a lower bound pending #3638. Use these as the reference point for tracking henyey's throughput progress toward core parity.
