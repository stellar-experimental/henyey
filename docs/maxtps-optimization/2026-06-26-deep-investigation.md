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

