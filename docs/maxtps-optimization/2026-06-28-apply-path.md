# maxtps-optimize run — 2026-06-28, target 1531 tx/s (match core)

- Session: 701dcf0a · Instance: tobbhan629hpo (nsc 32×64) · Base commit: f49a0350 (origin/main)
- Branch: maxtps/opt-701dcf0a
- Methodology: MissionMaxTPSClassic, 23-node tier-1, short-probe
  (SSC_MAXTPS_TXS_MULTIPLIER=60, SSC_LOADGEN_FASTFAIL_LEDGERS=10),
  --install-network-delay false. Accept bar: Δ > 5%.
  **Measurement integrity (new):** a "max" is only valid if a `Run failed` was
  recorded ABOVE the passing step. Never quote band-floors or extrapolations.
- **Current best: TBD (honest baseline pending)** · Target: 1531

## Context (post adversarial audit, 2026-06-28)

The gap is REAL and substantive: core ~1531 vs henyey ~225-250 (~6-7×), both true
binary-searched maxima. EXONERATED (proven healthy, do not re-litigate): overlay
dissemination, nomination, SCP agreement. The cap is the **un-instrumented
apply/consensus path under load** — `applied < offered` then NodeLostSync/loadgen
fast-fail at 250-400, with idle CPU and ledgers 27-49% full (per old baseline doc;
re-verifying with fresh instrumentation).

Refuted by code read this session: the "snapshot_delta O(N²) clone" hypothesis —
`snapshot_delta()` (crates/tx/src/state/mod.rs:807) only records vector lengths +
fee + a small bump-map clone (cheap checkpoint), NOT a full-delta clone.

## Hypotheses

| # | hypothesis | instrumentation | measurement | Δ vs best | status | notes |
|---|------------|-----------------|-------------|-----------|--------|-------|
| 0 | baseline (honest max) | maxtps_diag close+persist logs | ~225 (212 PASS / 243 FAIL / 275 FAIL) | — | done | honest, failure recorded above |
| 1 | localize close/apply/persist/cadence | per-close phase log + persist split | DONE — see below | — | done | apply EXONERATED by data |
| 2 | fill cap: queue (supply) vs nomination (selection)? | maxtps_nominate log (selected/queue_len/max_ops/limited) | DONE — SUPPLY-limited | — | done | selected==queue_len, backlog=0, not limited, fill 35% |
| 3 | admission cost breakdown: is per-tx create_snapshot the cap? | maxtps_snapshot_avg + maxtps_admit_avg | DONE — REFUTED | — | done | try_add ~20µs, snapshot ~3µs; admission NOT the cap |
| 4 | flood-tx ingestion starved by single-msg broadcast drain | bulk-drain overlay msgs (lifecycle.rs) + existing admit windows | pending | — | testing | image opt-701dcf0a-fix1; band [200,600] |

## Iteration 4 — FIX: bulk-drain the overlay broadcast channel

Root cause (from iter-1..3 + flood-path map): the app event loop's `message_rx` arm
(lifecycle.rs:938) handled **one** flooded-tx message per `select!` turn, while the
consensus-tick branch drains SCP+fetch 200-at-a-time AND runs ledger-close orchestration.
Under load (each node receives ~95% of txs via flood) the heavy tick branch starves
single-message tx ingestion → queue under-fills → ~219 cap. FIX: after the first
broadcast msg, bulk-drain up to MAX_DRAIN_PER_TICK more via try_recv (mirrors SCP/fetch).
Parity-safe (ingestion rate only; tx set is built deterministically). Measure: max should
rise above 219 if flood ingestion was the cap; admit-window cadence should speed up.

**Iter 4 result: REFUTED — bulk-drain gave NO throughput gain.** Converged max = ~219
(212 PASS, 225/250/300/400 FAIL) — unchanged from baseline. The earlier "fill 35%→47%" was a
RATE-DENOMINATOR ARTIFACT (compared fill% across different rates; absolute tx_count/ledger
~1400-1500 is unchanged). Why bulk-drain didn't help: the broadcast channel wasn't backing up
(prior lagged-drops=0), so draining it faster fixes a non-problem. REVERTED (kept instrumentation).
LESSON: the cap is flood DELIVERY between nodes (advert→demand→fulfill round-trips), UPSTREAM
of local ingestion — net delivery caps at ~280-300 unique txs/s per node (each needs ~5×rate).

| 5 | localize flood DELIVERY cap (advert/demand/fulfill rates) | flood_rate! counters (advert/fulfill/demand_sent/demand_recv) | DONE — re-demand storm | — | done | demand_sent ~3× need |
| 6 | re-demand cause: fulfill drops vs fulfill latency? | demand_new/demand_retry split + fulfill_dropped | DONE — both REFUTED | — | done | NO retries, NO drops; flood is clean |
| 7 | core comparison (reference flood/arrival) | core medida overlay.flood.* + pending-txs | DONE — decisive | — | done | core queue 89516 vs henyey 1450 |

## Iteration 6 result — flood mechanism is CLEAN (cascade refuted)

demand_new ≈ demand_sent (NO demand_retry lines), NO fulfill_dropped lines. So: no
re-demands, no drops. The congestion-cascade hypothesis is REFUTED. The flood pipeline
operates cleanly; net unique arrival just caps at ~290-500/s per node.

## Iteration 7 result — CORE COMPARISON (decisive, ends the hunt)

Ran stellar-core v27.0.0 in the identical mission. Core max = **1492** (matches the ~1531
baseline). Clean instantaneous comparison at each side's stress point:

| signal | core @1492 | henyey @412 (fails) |
|--------|-----------:|--------------------:|
| ledger.ledger.close (apply) | 55.6ms | ~56ms (IDENTICAL) |
| cadence | ~5s | ~5s (IDENTICAL) |
| **pending-txs.count (queue depth)** | **89,516** | **~1,450** |
| ledger.transaction.count (fill) | 7,459 | ~1,450 |
| overlay.flood.fulfilled / send.transaction | ~2,185/s | ~417/s |
| overlay.flood.advertised | ~12,000/s | ~4,000/s |
| overlay.flood.tx-pull-latency | 44ms | (5-7ms low-rate) |
| overlay.flood.duplicate-recv | huge | n/a |

**CONCLUSION:** apply, persist, admission, nomination, SCP timing, ledger close, and cadence
are all the SAME speed as core (close 55-56ms both). The ENTIRE ~6-7× gap is **overlay flood
ACQUISITION throughput**: core acquires ~1,500 tx/s/node and buffers a deep queue (89,516
pending); henyey acquires ~290-500/s/node and its queue stays shallow (~1,450), so the moment
offered rate exceeds acquisition it falls behind → fast-fail. The flood mechanism is *clean*
on henyey (no drops, no re-demands, no demand timeouts) — it's a raw throughput ceiling.

Likely architectural root (not yet fixed, candidate for a focused effort): henyey runs overlay
flood work (advert/demand/fulfill + tx ingestion) on the single main tokio event loop,
interleaved with SCP and ledger-close orchestration, whereas stellar-core services overlay on
background worker threads. This serializes henyey's overlay throughput at ~300-500/s/node.
Refuted quick-fixes (all by measurement): snapshot_delta O(N²), per-tx admission snapshot,
broadcast bulk-drain, demand congestion-cascade, queue-size cap (queue never hit its limit).

## Summary (final)

- **Baseline → final: 219 → 219 tx/s (NO throughput win landed).** Core = 1492 on this rig.
- The deliverable is a rigorous, fully-measured LOCALIZATION (corrects the prior session's
  retracted "near-parity" claim): the gap is real ~6-7× and lives entirely in overlay flood
  acquisition throughput; everything else matches core exactly.
- Parity-safe diagnostic instrumentation added (maxtps_diag close/persist/nominate logs +
  flood_rate! pipeline counters) — kept on branch maxtps/opt-701dcf0a for future overlay work.
- Accepted: 0. Rejected: bulk-drain (iter-4, no gain). Diagnostics: iters 1,2,3,5,6,7.
- Top remaining hypothesis for a future run: henyey overlay flood serialized on the main event
  loop — move flood advert/demand/fulfill to dedicated worker task(s)/threads (large change).
- Instance tobbhan629hpo torn down.

## Iteration 8 — REWORK: dedicated flood-scheduler task (in progress)

Root confirmed in code: the tx-advert-flush (100ms) and tx-demand (200ms) cycles were
`select!` branches in the main event loop (lifecycle.rs), serialized with the heavy
consensus-tick branch (SCP+fetch drains + ledger-close orchestration). `select!` runs one
branch to completion before re-polling, so under load the flood timers fired late AND blocked
message handling while running → flood cadence throttled → acquisition capped ~300-500/s.
(Consistent with iter-4: bulk-drain of inbound messages didn't help because the cap was the
flood-cycle CADENCE, not message draining.)

CHANGE: `App::run_flood_scheduler(self: Arc<Self>)` (tx_flooding.rs) runs advert-flush +
demand cycles on their own tokio intervals in a dedicated task, spawned in run_cmd.rs
concurrent with the main loop, aborted on shutdown. Removed the two branches + interval decls
from the main `select!`. Parity-safe: identical advert/demand wire messages/content + same
is_tracking gate; only scheduling/rate is decoupled (perf/internal). Added maxtps_flood_tick
log (actual advert period) to confirm on-time ticking. Compiles clean; flood unit tests passed (1135, 0 fail).

**Iter 8 result: REFUTED — the rework gave NO throughput gain.** Max stayed ~219 (300/400
still fail). The dedicated flood task's `maxtps_flood_tick` measured **avg_advert_period =
198-199 ms = on-time** (nominal `flood_tx_period` 200 ms). So the flood timers fire promptly
on the dedicated task, yet acquisition is unchanged (queue ~1,549, fill ~45%). Therefore
**flood-timer serialization on the main loop was NOT the cap** — the consensus tick was not
actually delaying the flood timers. 6th hypothesis refuted by measurement. REVERTED the rework
(parity-clean + 1135 tests passed, but no benefit and it adds a concurrency surface; kept the
diagnostic logging). Instance d9ro0uln49v7u torn down.

**Where the wall stands:** acquisition caps ~290/s/node via a *clean, on-time* flood cycle;
each node is only learning about / demanding ~290/s of the ~396/s its peers originate. The
next unmeasured question is **advert RECEIVE rate** (are ~100/s of adverts not reaching the
node, or being deduped/delayed?) — the demand cycle ticks on-time and isn't demand-size-capped,
so it can only be that fewer unique missing txs are *visible* to demand each cycle. That is the
next diagnostic if the effort continues.

## Iteration 5 result — flood DELIVERY decomposed: re-demand storm

Per-node load-window rates (rate 425): advert ~4000/s (free), demand_recv ~420/s,
fulfill ~413/s, **demand_sent ~1250/s ≈ 3× the actual unique need (~400/s)**. So each node
RE-DEMANDS each tx ~3×, forcing peers to do ~3× redundant fulfill work → effective unique
delivery throttled well below the raw ~420/s fulfill capacity. That is the henyey-specific
flood inefficiency (adverts/budget are NOT the cap; they have huge headroom). Iter 6 splits
demand into new vs retry and counts fulfill drops to determine whether the re-demands are
caused by fulfill DROPS (outbound channel/flow-control full) or fulfill LATENCY (re-demand
fires before the tx arrives) — which decides the fix (flow-control window vs fulfill priority/timing).

## Iteration 3 result — admission internals REFUTED, cap is FLOOD DISSEMINATION

Measured at rate 480-500 (windowed avgs): **try_add = ~20µs/call**, **create_snapshot = ~3-16µs**.
So admission validation (incl. the per-tx double-snapshot) is NOT the bottleneck (could do
~50k/s, no lock contention — the Drop timer includes lock-wait). The per-tx-snapshot
hypothesis is REFUTED by measurement (good thing — same lesson as snapshot_delta).

**Topology (decisive):** MaxTPSClassic runs loadgen on EVERY tier1 node (`loadGenNodes=tier1`,
`RunMultiLoadgen loadGenNodes`). So each node generates only ~rate/23 txs locally (~21/s at
rate 490) and must RECEIVE the other ~rate×22/23 (~469/s) via overlay flooding. Since
try_add≈410/s ≈ flood-in (local only ~21/s), and the node needs ~490/s to keep up, the cap
is **overlay flood dissemination throughput** — delivers ~390/s vs ~469 needed → queue
under-fills → fail. The flood ops budget (~maxTxSetSize/5s ≈ 980/s at rate 490) is ABOVE the
need, so the limiter is the flood MECHANISM (advert→demand→fulfill cadence/batch/processing),
not the flow-control cap. Consistent with the prior session's only real win
(flood_tx_period_ms=100, +13%) and the audit's warning that overlay was wrongly exonerated.
Iter 4: instrument flood receive-rate + send/fulfill-rate per node to find the mechanism limit.

## Iteration 2 result — fill cap is SUPPLY (admission), not selection (decisive)

High-rate run (band [400,600], steps 500/450/425/412 — all fail above max 219).
maxtps_nominate at loaded rounds (max_ops=4120, i.e. rate 412):
- **selected == queue_len in EVERY round** (1956=1956, 1695=1695, 1625=1625, …);
  backlog (queue_len−selected) = **0**; **classic_limited = false**; fill ~**35%**.
- The nominator takes the ENTIRE queue — but the queue only holds ~1558 (median) at
  nomination, while max_ops=4120 allows 2.6× more and the loadgen offers ~2000+/ledger.

**Conclusion: the queue is STARVED — txs don't enter it fast enough.** The cap is
ingress/ADMISSION throughput, NOT nomination/selection (selection has 2.6× headroom)
and NOT apply (implied 17k tx/s headroom). This is the misread the prior tainted
session made: "nominator takes the whole queue" is true but means starvation, not
health. Admission throughput ≈ queue_fill/cadence ≈ 1558/5s ≈ 310/s (vs offered).

Leading mechanism (code-confirmed, cost UNMEASURED): henyey runs **create_snapshot
per admitted tx, twice** (validate_fee_balance → LedgerFeeBalanceProvider, AND the
account provider — types.rs:1141/1166), under the global store.write() lock. Core does
NOT validate fee balance at admission. Iteration 3 measures create_snapshot avg cost +
total try_add cost to confirm before fixing (avoid the snapshot_delta-style wrong lead).

## Iteration 1 result — apply path EXONERATED, cap is FILL (decisive)

At failing rate 275 (11 loaded ledgers, max_tx_set_size 2750):
- **close total_us = 56ms** median (tx_exec 48ms for ~1375 txs), **persist = 55ms**
  (SQLite commit 29ms, bucket flush ~0). Serial critical path close+persist = 116ms
  → implies **~8.6 ledgers/s, ~11,600 tx/s apply capacity.**
- **Loaded-ledger CADENCE = ~5.0s** (seq 21-25 at 22:37:09/14/19/24/29) — identical
  to the empty-network 5s cadence. Apply uses 56ms of each 5s; ~4.94s idle.
- **fill = 49%** (~1375 of 2750); ~1375 ≈ 5×rate (loadgen offers `rate`/s steadily,
  5s ledger accrues 5×rate). Throughput = 1375/5 ≈ 275 tx/s = the rate. So the node
  is apply-idle and cadence-locked.

**Conclusions:**
1. Apply / persist / snapshot_delta / close-pipeline serialization are NOT the
   bottleneck (40-200× headroom). Exonerated by data.
2. Throughput = fill ÷ cadence. **Cadence ~5s is protocol/observable (close_time in
   the SCP value) → parity-locked; henyey must not close faster than core.** So
   cadence is OFF the table.
3. **The only lever is FILL.** To reach core's 1531 at 5s cadence, ledgers must hold
   ~7655 txs. maxTxSetSize = rate×10 (mission `upgradeMaxTxSetSize`), so the ceiling
   isn't maxTxSetSize per se — it's whether henyey can *fill* ledgers to ~5×rate as
   rate climbs. At rate 275 fill≈5×rate (OK-ish); the question is the high-rate cap.
4. NOTE: cadence ~5s for BOTH core and henyey means core hits 1531 by filling
   ~7655 txs/ledger. So henyey's gap = inability to fill that high at high rate.
   Iteration 2 measures selected vs queue_len vs max_ops at rate 400-600 to localize
   supply (admission/flooding into queue) vs selection (nomination) as the fill cap.

REFUTED this iteration: serial close+persist pipeline as the cap (headroom is huge).

## Instrumentation added (parity-safe, logging only)

- `crates/ledger/src/manager.rs` (~6364): `info!(target:"maxtps_diag", "maxtps_close")`
  — per-close: tx_count, max_tx_set_size, total_us + every phase (begin/tx_exec/
  classic_exec/prepare/executor_setup/fee_pre_deduct/post_exec/commit_setup/
  bucket_lock_wait/eviction/add_batch/hot_archive/header/commit_close/meta).
- `crates/app/src/app/persist.rs` (~406): `info!(target:"maxtps_diag", "maxtps_persist")`
  — per-close persist_us split into flush_us (bucket/hot-archive) + commit_us (SQLite).

Decompose: cadence(inter-close) = total_us(close) + persist_us + pipeline_wait/idle.
