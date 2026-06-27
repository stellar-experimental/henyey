# maxtps deep investigation — 2026-06-26 (iter 9)

- Session `c66d03b4` · Instance `93i67d9uejjg0` (nsc 32×64) · Branch `maxtps/opt-c66d03b4`
- Goal: find the real henyey↔core throughput gap (core ~1531 vs henyey ~230 tx/s, ~6.6×) via live profiling, then a big-swing parity-safe fix.
- Method: instrumented images add parity-free `tracing::info!(target:"maxtps_diag")` logs — `nominate_build` (selected/max_ops), `agreed_set` (agreed), `txset_validate` (tx_count/us). Per-thread CPU via `/proc/PID/task/*/stat` deltas. All measurements on one reused instance.

## What was ruled out (live-measured)

| Hypothesis | Verdict | Evidence |
|---|---|---|
| Serial event-loop CPU bound | **Refuted** | Process CPU **1–19% of one core** during load; nothing pegged. Node *waits*, not computes. |
| Agreement-convergence loss (combineCandidates) | **Refuted at low rate** | At rate 230, `agreed == nominated` (~1150). |
| Inclusion/surge trim | **Refuted** | `selected` (~1150) ≪ `max_ops` budget (2300+). |
| Cadence stretch under load | **Refuted** | Cadence stable ~5s even at rate 650. |
| Propagation-throughput plateau | **Refuted** | Nomination *scales* with rate (650 → nominated ~3000–3447 ≈ offered×5s). |
| Propagation skew between nodes | **Refuted** | At rate 462, all 7 orgs nominate *similar* sizes (~2100–2700). |

## Root cause: agreement efficiency under load

`agreed` falls below `nominated` as offered rate rises:

| offered | nominated (per node) | agreed | applied ≈ agreed/5s | gap |
|---|---|---|---|---|
| 230 | ~1150 | ~1150 | ~230/s | 0% |
| 462 | ~2100–2700 | ~2000–2300 | ~440/s | ~8% |
| 525 | ~2500–2850 | ~1840–2340 | ~440/s | ~20% |
| 650 | ~3000–3447 | ~2300–2820 | ~500/s | ~20–25% |

`combineCandidates` picks the biggest candidate tx-set that reaches **quorum-acceptance**. A node accepts a peer's candidate only after **validating** it — full per-tx stateful checks (`get_invalid_hashed_core`: account load via O(log n) bucket-list traversal + ed25519 sig verify), run **serially** in a `for htx in txs` loop, scaling with set size. Under load the biggest candidate sets validate too slowly to win quorum within the round → a smaller set wins → `agreed < nominated`. Idle CPU = the serial validation waits while 31 cores sit unused. Core runs the same logic (`TxSetUtils.cpp:199`) but isn't capped — henyey's serial-on-one-thread validation is the divergence.

**Why this is the lever:** `agreed = nominated = offered×cadence ⇒ applied = offered` — sustainable at any rate up to propagation/apply limits (not hit at 650). Closing the agreement deficit (efficiency→1) could push max TPS from ~230 toward ~650+ — multiplicative, toward core's range.

## Fix (iter 9): parallelize per-tx tx-set validation

`crates/herder/src/tx_set_utils.rs` — `get_invalid_hashed_core` pass-1 is independent per tx (generalized sets forbid multiple txs per source ⇒ no intra-set seq/fee deps). Run it across cores with `std::thread::scope`; merge outcomes in tx order. **Parity-exact**: invalid-set membership and `account_fee_map` sums are order-independent; observable consensus output unchanged. Both ledger-state providers are `Send + Sync`; snapshot `get_entry` is concurrent-read-safe. 120 `tx_set_utils` tests pass.

## Result — NEGATIVE (validation speed was not the gate)

Built `opt-c66d03b4-pval` (parallel validation + `txset_validate` timing). Live results:
- Validation got **16× faster**: `txset_validate` ~10–25 ms for 1300–2600-tx sets (parallel) vs ~400 ms serial (~150 µs/tx).
- **But max TPS = 238**, vs same-instance baseline ~230 — **no meaningful change** (+3.5%, within noise; below the 5% bar).
- `agreed/nominated` ratio (~85–100%) **unchanged** from the serial run.

**Conclusion:** validation speed / event-loop blocking by validation is **not** the gate. The agreement deficit persists with fast validation.

## Revised root cause: an agreement-efficiency cliff (~238)

`eff = agreed/nominated` is ~1.0 at R ≤ 238 and drops below 1.0 just above it (cliff, not gradual). At R ≤ 238 all nodes converge on the full offered set within the 5 s round; just above, intra-round tx **propagation completeness** breaks — nodes nominate slightly divergent sets, so the SCP-agreed (round-leader) set is smaller than offered×cadence, `applied < offered`, backlog grows, run fails. At overload the network still *applies* up to ~500/s, but can only *sustain* ~238 (where eff≈1). This is consistent with the earlier `flood_tx_period_ms=100` win (+13%): faster propagation raised the eff-cliff. Persist/close ruled out (cadence stable ~5 s even at overload).

The remaining lever is **intra-round propagation completeness / SCP nomination timing** — a faithful-port flood path plus parity-sensitive SCP timing. No further *clean, parity-safe, henyey-specific* lever was found; flood-period tuning is shared-config (already exploited).

## Disposition

The parallel-validation change is **parity-exact and a genuine 16× validation speedup** (exploits the idle cores; helps catchup/sync and higher-limit networks), but it is **not a max-TPS win** and is therefore rejected against the >5% maxtps bar. Kept on branch `maxtps/opt-c66d03b4` as a standalone perf optimization for separate consideration.

## Core baseline comparison (same rig, instance `ml0bf06bdq43k`)

Ran stellar-core (`stellar-core-testing@sha256:bc1de6bc…`) through MaxTPSClassic at the rates henyey *fails*:

- **Core PASSES 500, 600, and 650 tx/s** (henyey caps at ~238).
- At rate 600, core: applied **~3000–3200 tx/ledger** (`ledger.transaction.count` mean 3010, 99% 3228), close duration ~83 ms, cadence ~5 s → ~600 tx/s, **eff ≈ 1**.
- `overlay.flood.tx-pull-latency` **mean 17 ms / median 14.6 ms / 99% 78 ms**; `overlay.demand.timeout = 0`; `scp.timing.nominated` ~166–222 ms; `scp.timing.ballot-blocked-on-txset = 0`; `herder.pending-txs.count = 35994`.

So **core's cadence is also ~5 s** (confirming the gap is *bigger agreed sets*, not faster cadence) and core's intra-round tx propagation **completes** (17 ms pull latency, zero demand timeouts), letting it agree on ~3000-tx sets at 600 tx/s with eff≈1. henyey's eff drops below 1 above ~238.

### Divergences found, and the decisive one

The core comparison surfaced several config keys henyey's compat layer silently ignored that Supercluster/core set:
1. **`FLOOD_DEMAND_PERIOD_MS=100`** — henyey didn't map it → ran at default 200 (2× slower demand). **Fixed** (commit `6765988d`; also maps `FLOOD_DEMAND_BACKOFF_DELAY_MS`; defaults unchanged 200/500 = core parity). **Measured: max 240 vs 238 — no max-TPS change.** Demand cadence is not the gate.
2. **`TRANSACTION_QUEUE_SIZE_MULTIPLIER_FOR_TESTING=3`** — henyey hardcodes 2 (minor; both large).
3. **`EXPERIMENTAL_BACKGROUND_TX_SIG_VERIFICATION=true`** — unmapped.
4. **`EXPERIMENTAL_TX_BATCH_MAX_SIZE=500` — tx-batching. THE decisive one.**

**The decisive finding: tx-batching is `#ifdef BUILD_TESTS`-only in stellar-core.** `TxDemandsManager::recvTxDemand` fulfils a demand by sending **individual** `tx->toStellarMessage()` (TxDemandsManager.cpp:344) — *except* under `#ifdef BUILD_TESTS` with `EXPERIMENTAL_TX_BATCH_MAX_SIZE>0`, where it packs up to 500 txs into one `TX_SET`-typed message with sentinel `previousLedgerHash=TX_BATCH_HASH` (`OverlayManager::createTxBatch`, only treated as flood under `#ifdef BUILD_TESTS`). The MaxTPSClassic benchmark runs the **BUILD_TESTS** core image (`stellar-core-testing`), so its core nodes batch tx-flooding ≤500×; **production core floods individual tx messages — exactly like henyey.**

### Conclusion

A large part of the measured henyey-vs-core max-TPS gap on this benchmark is **core's BUILD_TESTS-only tx-batching**, a test-harness optimization production core does not have. henyey (production code) floods individually like production core. Two targeted parity-safe henyey fixes (parallel validation `ccec4c3b`, demand-period mapping `6765988d`) are correct but max-TPS-neutral, consistent with the gate being **message volume** (which only batching addresses) rather than per-op speed. 

**Implication:** closing the *benchmark* gap would require giving henyey an equivalent (test-only / experimental) tx-batching path — benchmark-matching, not a v27 production-parity requirement (production core lacks it too). Whether to add a test/experimental feature to production code to match the benchmark is a product decision. This echoes the broader maxtps lesson: this benchmark contains core-favoring test-harness artifacts (cf. the earlier `wait_till_complete`/pacing measurement bugs).

## CORRECTION + definitive disproof of the batching hypothesis

Two errors in the section above, both caught and corrected by measurement:

1. **Core was NOT batching in the comparison.** I claimed the gap was core's BUILD_TESTS tx-batching, but my own core metrics showed `overlay.flood.tx-batch-size` **count = 0** — core fulfilled demands with individual tx messages. And `MissionMaxTPSClassic` does **not** set `runForMaxTps`, so the deployed config has **none** of those knobs (verified on-pod: no `EXPERIMENTAL_TX_BATCH_MAX_SIZE`, no `FLOOD_DEMAND_PERIOD_MS`). The benchmark compares **default-config** core (≥650 tx/s here) vs **default-config** henyey (~238), both flooding individually.

2. **Batching does not help henyey either.** I implemented tx-batching anyway and ran a controlled test: patched Supercluster to force `EXPERIMENTAL_TX_BATCH_MAX_SIZE=500` on all nodes (verified present in the deployed cfg) and re-ran. **henyey max = 215** — no improvement over the ~238 baseline (slightly lower, within noise). So tx-batching is **not** the lever, in either direction. The tx-batching commit was **reverted** (`1a6ff055`); the Supercluster patch was reverted.

### Net conclusion

The henyey↔core max-TPS gap on this benchmark is a **default-behavior** difference: at the same ~5 s cadence and idle CPU, core's intra-round tx propagation **completes** (agrees on ~3000-tx sets, eff≈1) while henyey's does not (eff<1 above ~238). It is **not** CPU, apply speed, cadence, tx-set validation speed, demand cadence, *or* tx-batching — all measured and ruled out. The precise remaining cause is in henyey's default overlay/consensus propagation efficiency and was not pinned: henyey's detailed overlay metrics (`stellar_overlay_tx_pull_latency_seconds`, demand timeouts) are not reachable in the Supercluster deployment (the compat `/metrics` on :11626 returns a curated medida-JSON without them; the native prometheus port collides on :11626), which blocked the final henyey-vs-core pull-latency comparison.

### Kept (genuine parity-correctness fixes, max-TPS-neutral here)
- `ccec4c3b` — parallelize per-tx tx-set validation (parity-exact, ~16× faster validation; useful for catchup/high-limit, not a maxtps win).
- `6765988d` — honor `FLOOD_DEMAND_PERIOD_MS`/`FLOOD_DEMAND_BACKOFF_DELAY_MS` compat config (real bug: henyey ignored config keys core honors; defaults unchanged = parity).


