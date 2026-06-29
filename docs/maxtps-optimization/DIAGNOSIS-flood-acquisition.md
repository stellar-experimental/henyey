# Max-TPS bottleneck diagnosis: overlay flood acquisition throughput

**Status:** diagnosis complete (2026-06-29). **Outcome:** the henyey-vs-stellar-core
classic-payment max-TPS gap is real (~6-7×) and localized — *entirely* — to overlay
**flood acquisition throughput**. Apply, persist, admission, nomination, SCP timing,
ledger-close and cadence all match stellar-core. No throughput change was shipped; this
document is the measured localization that scopes the fix.

- Rig: Supercluster `MissionMaxTPSClassic`, 23-node tier-1, single nsc 32×64 VM.
- Method: short-probe binary search (`SSC_MAXTPS_TXS_MULTIPLIER=60`,
  `SSC_LOADGEN_FASTFAIL_LEDGERS=10`), `--install-network-delay false`.
- Measured maxima (both binary-searched to a recorded failure above): **henyey 219 tx/s,
  stellar-core v27.0.0 1492 tx/s** on this rig.

---

## 1. Executive summary

henyey caps at ~219 classic payments/s; stellar-core does ~1492 on the same rig. Through
seven measured iterations (per-close phase timers, persist split, nomination
decomposition, admission-cost timers, a full flood-pipeline rate breakdown, and a direct
stellar-core comparison) the gap was narrowed to a single layer:

> **Each henyey node can only acquire ~290-500 transactions/s via overlay flooding and
> keeps a shallow tx queue (~1,450 pending). A stellar-core node acquires ~1,500 tx/s and
> buffers a deep queue (~89,500 pending). Both apply a ledger in ~56 ms at a ~5 s cadence.**
> So henyey under-fills every ledger (~1,450 vs ~7,500 tx) and falls behind the moment the
> offered rate exceeds its acquisition rate.

The flood mechanism on henyey is **clean** — no dropped fulfills, no demand re-tries, no
demand timeouts. It is a *raw throughput ceiling*, not a correctness or congestion failure.

The likely architectural root (the subject of the follow-up rework): henyey runs overlay
flood work — advert flushing, demand issuing, demand fulfillment, and inbound-tx ingestion
— on the **single main tokio event loop**, interleaved with SCP and ledger-close
orchestration, whereas stellar-core services overlay on background worker threads.

---

## 2. The decomposition (throughput = fill ÷ cadence)

Sustained TPS = (transactions applied per ledger) ÷ (seconds per ledger). Measured facts:

- **Cadence ≈ 5.0 s under load**, identical to the empty-network cadence and to
  stellar-core. This is protocol/observable (`close_time` in the SCP value) → **parity-locked;
  henyey must not close faster than core.** Cadence is off the table.
- Therefore TPS is governed entirely by **fill** (txs per ledger). To reach core's 1492 at
  5 s, a ledger must hold ~7,500 txs. henyey holds ~1,450.

So the whole investigation reduces to: **why can't henyey fill ledgers higher?**

---

## 3. Layer-by-layer evidence (what was exonerated, by measurement)

Instrumentation added (parity-safe, `tracing` + atomics only, target `maxtps_diag`):
per-close phase breakdown (`manager.rs`), persist flush/commit split (`persist.rs`),
nomination `selected`/`queue_len`/`max_ops`/limit-flags (`selection.rs`), `try_add` and
`create_snapshot` windowed timers, and `flood_rate!` advert/demand/fulfill counters
(`tx_flooding.rs`).

| Layer | Measurement | Verdict |
|-------|-------------|---------|
| Tx apply | close `total_us` ≈ 56 ms for ~1,375 tx; `tx_exec` 48 ms | **healthy** — ~11k tx/s headroom |
| Persist (SQLite) | persist 55 ms (commit 29 ms, flush ~0); serial close+persist ~116 ms | **healthy** — implies ~8-12 ledgers/s |
| Ledger cadence | ~5.0 s loaded = empty = core; parity-locked | **not a lever** |
| Nomination/selection | `selected == queue_len` every round; `classic_limited=false`; fill ~35-47% | **healthy** — takes the whole queue, 2.6× headroom |
| Admission internals | `try_add` ~20 µs; `create_snapshot` ~3 µs (×2/tx); no lock contention | **healthy** — ~50k/s capacity |
| Queue size cap | observed queue (max ~2,590) never reached its limit (~8,240 ops) | **not binding** |
| **Overlay flood acquisition** | net unique arrival ~290-500/s/node; queue stays ~1,450 | **THE bottleneck** |

The CPU is idle and ledgers close 35-47% full — the signature of a supply/acquisition cap
upstream of execution, exactly as found.

---

## 4. The decisive stellar-core comparison

Ran stellar-core v27.0.0 in the identical mission (max = 1492). Clean instantaneous
medida readings at each side's stress point:

| signal | core @1492 | henyey @412 (fails) |
|--------|-----------:|--------------------:|
| `ledger.ledger.close` (apply) | 55.6 ms | ~56 ms — **identical** |
| cadence | ~5 s | ~5 s — **identical** |
| `herder.pending-txs.count` (queue depth) | **89,516** | **~1,450** |
| `ledger.transaction.count` (fill) | 7,459 | ~1,450 |
| `overlay.flood.fulfilled` / `send.transaction` | ~2,185/s | ~417/s |
| `overlay.flood.advertised` | ~12,000/s | ~4,000/s |
| `overlay.flood.tx-pull-latency` (mean) | 44 ms | 5-7 ms (low-rate) |
| `overlay.flood.duplicate-recv` / abandoned / timeout | present / 0 / 0 | n/a / 0 / 0 |

Reading: apply and consensus are the *same speed*. Core simply moves transactions across
the overlay ~4-5× faster per node and buffers them deeply (60× the queue depth), so its
ledgers fill to ~7,500 while henyey's starve at ~1,450.

Topology note (why flooding dominates): `MaxTPSClassic` runs loadgen on **every** tier-1
node in a full 22-peer mesh, so each node generates only ~rate/23 txs locally and must
acquire ~95% of all txs via flooding. The flood path *is* the throughput path.

---

## 5. Hypotheses refuted by measurement

The method that distinguishes this diagnosis from the earlier (retracted) "near-parity"
conclusion: **every candidate fix was measured before being accepted, and five were killed.**

1. **`snapshot_delta` O(N²) clone** — refuted by code read: it records vector lengths, not a
   clone.
2. **Per-tx admission snapshot cost** — refuted by measurement: `create_snapshot` ≈ 3 µs,
   not the assumed ~ms.
3. **Broadcast single-message drain starving ingestion** — *built and measured*: bulk-drain
   (200/turn like SCP/fetch) gave **no** max change; reverted. (Also tells us inbound
   message *processing* is not the cap.)
4. **Demand congestion-cascade** — refuted: zero `demand_retry`, zero `fulfill_dropped`; the
   flood pipeline is clean.
5. **Tx-queue size cap** — refuted: the queue never reached its limit; it is
   acquisition-rate-limited, not size-limited.

Notably, the demand backoff (`flood_demand_backoff_delay_ms = 500`) and flood periods
(advert 100 ms, demand 200 ms) **match stellar-core**, so config divergence is not the cause.

---

## 6. Conclusion and the path forward

The gap is a raw overlay flood-acquisition throughput ceiling (~300-500 tx/s/node), with a
clean mechanism, while every other layer matches stellar-core. The leading architectural
hypothesis is that henyey serializes overlay flood work on the main event loop.

**Caveat for the rework (honest):** the bulk-drain experiment (#3) showed that inbound
*message processing* is not starved on the event loop. So the serialization, if present, is
more likely in the **periodic flood scheduling** (advert-flush / demand-issue timers firing
late behind consensus-tick work) or the **per-cycle demand/fulfill batch sizing** than in raw
message draining. The rework therefore begins by **pinning the exact serialization point**
(instrument flood-timer actual-vs-nominal period and per-cycle demand/fulfill counts under
load) before moving work off the loop — same measure-first discipline.

Parity guard for the rework: the observable surface is overlay **wire bytes** and consensus
ordering. Moving flood work to worker task(s)/threads must not change message formats,
content, or consensus-relevant ordering — only the *rate/scheduling* of dissemination, which
is performance/internal and divergeable.

### Artifacts
- Run log: `docs/maxtps-optimization/2026-06-28-apply-path.md` (iteration-by-iteration).
- Diagnostic instrumentation: branch `maxtps/opt-701dcf0a` (commit "Add maxtps flood/apply
  diagnostic instrumentation"), parity-safe, reusable for the rework.
</content>
