# Max-TPS ceiling root cause: stale flood budget (2026-06-29)

Session 701dcf0a · instance bn2kosgj8jo3u (nsc 32×64) · MissionMaxTPSClassic.

## TL;DR

There is **no ~3% transaction loss**. Near the ceiling henyey applies **99.98–100%**
of submitted txns, and ledger apply has **~20× headroom** (40–82 ms of a 5 s budget,
`max_tx_set_size=2500` vs ~1135 txns/ledger used). The measured ceiling (~220 tx/s) was
set by a **single un-propagated transaction** per step tripping the loadgen's
all-or-nothing completion check.

Root cause: the **transaction-flood budget was computed from stale static config**
(`config.max_tx_set_size`) instead of the **live ledger header** value. The maxTPS
mission upgrades `maxTxSetSize` on-chain (→ 2500), but the flood budget kept using the
genesis config (~1025), making the per-flush advert budget **~2.4× too small**
(observed `ops_budget=41`, should be ~100). Under load the advert drain was
**100% budget-bound**, the flood queue backed up to ~1400, and a few locally-submitted
txns were **never advertised** before aging out — stranding their accounts and failing
the whole maxTPS step.

This is a **parity bug**: stellar-core's flood budget uses
`LedgerManager::getLastMaxTxSetSizeOps()` (the live LCL header).

## Evidence chain (cross-node per-tx trace, all 23 nodes)

1. **Coverage.** Aged-out/stranded txns reached **coverage=1** (origin only); applied
   txns reached all 23. The loss is a propagation gap, not apply loss.
2. **Magnitude.** rate 227: 13614/13615 applied (1 stranded). rate 250: 14991/14994
   applied (3 stranded). All stranded = coverage 1 + **never-advertised** + aged-out.
3. **Apply headroom.** During load each ledger carried ~1135–1277 txns with
   `max_tx_set_size=2500`, apply time 40–82 ms; full load drained by ~ledger 32.
4. **Completion check.** `wait_till_complete` failed with `unsynced_accounts=1`,
   `expected_vs_onchain=[(1,0)]` — exactly the one un-propagated tx. The all-or-nothing
   check (#3631, parity with core) turns 1 stranded tx into a whole-step failure.
5. **Flush dynamics.** During each stranded tx's 15.5 s life its origin flushed 77×
   (cadence fine, maxgap 0.29 s) but **77/77 budget-bound**; `ops_budget` collapsed
   142→41 as `queue_len` ran to ~1400. Overall only ~18% budget-bound — saturation is
   load-localized.
6. **Stale budget.** `ops_budget=41` ⇒ maxOps≈1025; live ledger `max_tx_set_size=2500`.
   `herder.max_tx_set_size()` returned `self.config.max_tx_set_size` (set once at
   startup, never updated on `LedgerUpgrade::MaxTxSetSize`).

## Fix

- `LedgerManager::last_max_tx_set_size_ops()` — new accessor reading the live LCL header
  (`crates/ledger/src/manager.rs`), mirroring core `getLastMaxTxSetSizeOps()`
  (protocol ≥ 11 returns the field as-is; henyey is 24+).
- `Herder::max_tx_set_size()` now delegates to it instead of static config
  (`crates/herder/src/herder.rs`). All four callers are flood/advert/demand sizing in
  `crates/app/src/app/tx_flooding.rs`; tx-set construction already used the live value.
- Regression test `test_last_max_tx_set_size_ops_tracks_live_header` (manager.rs):
  upgrading the header 100→2500 is reflected by the accessor.

Parity: this aligns henyey with stellar-core; flood volume/cadence is on the divergeable
surface (`docs/PARITY.md`) and does not affect hashes, tx-set selection, or wire format.

## Validation (cloud, nsc 32×64, 23-node MaxTPSClassic)

| Configuration | Max tx rate | Stranded txns | Aged-out (whole run) |
|---|--:|--:|--:|
| Baseline (before fix) | 220 | 1–3 per step | present |
| **Flood-budget fix (this PR)** | **330** | **0** | **0** |
| Flood-budget fix + per-peer rate-limit raise | 340 | 0 | 0 |

The flood-budget fix alone raises the measured ceiling **220 → 330 (+50%)** with **zero**
stranding (all 40,495 unique txns reached coverage 23 / advertised / applied). The
additional per-peer rate-limit raise (a separate, henyey-only flow-control change) added
only 330 → 340 (+3%, below the short-probe noise floor) and is **not** included here.
Above the new ceiling (e.g. 350/360) there is still zero age-out — those steps now fail on
honest apply/throughput, not on a propagation gap.
