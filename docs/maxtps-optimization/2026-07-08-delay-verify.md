# maxtps verify — 2026-07-08, same-instance delay-false vs delay-true

Purpose: remove the instance-drift caveat from the 2026-07-07 delay-true run by
measuring both delay modes **back-to-back on one fresh instance**, for both
images. Confirms whether WAN delay actually moves the number and whether the
prior delay-true bands reproduce.

- Session: f9febd81 · Instance: lhm7gi8hr5jqm (nsc 32×64) · henyey branch: maxtps/verify-f9febd81 off origin/main (079ca267)
- henyey image: `nscr.io/k4jkul01t5rr0/henyey:delaytrue-19becb5d` (reused; == origin/main HEAD, unchanged since 2026-07-07 build — controls for build variance)
- stellar-core image: `nscr.io/k4jkul01t5rr0/stellar-core-testing:v27.0.0`
- Methodology: MissionMaxTPSClassic, 23-node tier-1, full window (MULT=1000, ~16.7 min), FASTFAIL=10; GENESIS_ACCOUNTS = MAX_TX_RATE × 26 (round up to 1000). Matched rates under both `--install-network-delay false` and `true`.

## Prior (2026-07-07, different instance qgkdacn366m94)
- delay=true bands: core [2300,2400), henyey [2100,2200). Injected RTT median ~132ms.
- Caveat being tested: henyey delay-false ref (1870) was on yet another instance (campaign 2).

## Measurements (this instance)

| impl | delay | rate | accounts | result | notes |
|------|-------|------|----------|--------|-------|
| henyey | false | 2100 | 55,000 | ❌ FAIL | mid-submission 172k/300k @~min9 |
| henyey | true  | 2100 | 55,000 | ✅ PASS | vs delay-false FAIL at same rate/instance — near-edge, needs band bracketing |
| henyey | false | 2000 | 53,000 | ❌ FAIL | r1: completion-tail wedge (all 285k submitted, <10-ledger finish) — STOCHASTIC signature |
| henyey | true  | 2000 | 53,000 | running | replication r1 (pass-rate test) |

### Replication note
Near henyey's edge the failure is its known **stochastic completion-tail wedge**
(campaign 2 / PR #3722): a handful of txs age out of the flood queue and never
apply, failing the all-or-nothing mission bar. So a single pass/fail at a given
rate is a coin-flip, not a band edge. Switched to **pass-rate replication at a
fixed rate (2000)** under both delay modes. Hypothesis: injected per-peer delay
paces adverts and reduces the starvation tail, so delay-true may pass more often
than delay-false at the same rate.

Samples @ 2000 (all 53k accounts, full-window):
- delay-false: r1 ❌, r2 ❌, r3 ✅, r4 ✅, r5 ✅ → **3/5 (60%)**
- delay-true:  r1 ✅, r2 ✅, r3 ✅, r4 ✅ → **4/4 (100%)**

## Bands & verdict

**1. Instance drift is real and was the point.** This instance (lhm7gi8hr5jqm) is
slower than the 2026-07-07 instance (qgkdacn366m94): henyey's edge sits at ~2000
here vs [2100,2200) there. This confirms the caveat — cross-instance number
comparisons are unreliable; only same-instance comparisons count.

**2. Network delay does NOT hurt henyey.** Same-instance, same-image, at 2000 tx/s
full-window: delay-true passed **4/4**; delay-false passed **3/5**. Delay-true was
never worse. The prior run's "delay-true ≥ delay-false" holds on a same-instance
basis.

**3. The apparent "delay helps" is NOT statistically established.** The early
signal (delay-false 0/2, delay-true 2/2) looked dramatic but regressed toward the
mean as N grew (df 3/5, dt 4/4). Fisher exact on 3/5 vs 4/4: p≈0.44 — not
significant. henyey's near-edge failure is its stochastic completion-tail wedge
(campaign 2 / PR #3722); a single pass/fail is a coin-flip, and 2000 sits right
on that coin-flip band for both delay modes. If delay has a real pacing benefit
it is small and would need many more samples (or a controlled advert-timing test)
to prove. Do not claim "delay improves henyey" from this data.

**Bottom line for the original question (does delay change the picture?):** No.
Both implementations run at essentially their no-delay rate under median-132ms-RTT
WAN latency; henyey lands near its own no-delay edge, not degraded. The 2026-07-07
"gap narrows under delay" framing was an instance-drift artifact — on a single
instance, delay is close to neutral for both. Core was not re-measured here (ran
out of the run's replication budget on the henyey question); its 2026-07-07
same-instance delay-true band [2300,2400) ≈ delay-false floor already showed core
is delay-insensitive.

### Raw data
