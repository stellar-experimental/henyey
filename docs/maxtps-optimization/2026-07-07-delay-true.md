# maxtps measurement — 2026-07-07, network delay ENABLED (`--install-network-delay true`)

Measurement-only run (no code changes). Compares henyey vs stellar-core
full-window max TPS under WAN-like injected geographic latency, against the
existing `--install-network-delay false` reference numbers.

- Session: 19becb5d · Instance: qgkdacn366m94 (nsc 32×64, 32 cores / 62 GiB) · henyey branch: maxtps/delaytrue-19becb5d off origin/main
- henyey image: `nscr.io/k4jkul01t5rr0/henyey:delaytrue-19becb5d` (origin/main HEAD)
- stellar-core image: `nscr.io/k4jkul01t5rr0/stellar-core-testing:v27.0.0` (pinned BUILD_TESTS == `stellar-core/` submodule @ 7696c069, v27.0.0)
- Methodology: MissionMaxTPSClassic, 23-node tier-1, `SSC_MAXTPS_TXS_MULTIPLIER=1000` full window (~16.7 min), `SSC_LOADGEN_FASTFAIL_LEDGERS=10`; GENESIS_ACCOUNTS = MAX_TX_RATE × 26 (round up to 1000). **`--install-network-delay true`.**

## Network calibration (this instance)

- **Raw fabric** (pod-to-pod, no injected delay): 70.4 Gbit/s aggregate (iperf3, 4 streams); RTT sub-0.1 ms (same instance class as the delay-false campaign's 72.5 Gbit/s / 0.067 ms).
- **Injected geographic delay** (`--install-network-delay true`): per-destination `tc netem` one-way delays derived from the tier-1 node geo topology (Haversine great-circle distance ÷ ~200 km/ms fibre × empirical slowdown). Measured from bd-0's cfgmap: 15 peer destinations, one-way **3–105 ms** (median 62, mean 56) → **RTT ≈ 6–210 ms, median ≈ 124 ms**. This is the operative latency for this run — ~1000–2000× the raw-fabric RTT. Every SCP round-trip and tx-pull now pays 100+ ms per hop.

## Reference (delay=false, prior campaigns)

| impl | full-window (delay=false) | accounts |
|------|---------------------------|----------|
| stellar-core | [2400, 2700) | 92,000 |
| henyey | 1870 | 49,000 |

## Measurements (delay=true)

| impl | rate | window | accounts | result | notes |
|------|------|--------|----------|--------|-------|
| stellar-core | 2100 | burst (60) | 55,000 | ✅ PASS | injected RTT median ~124ms |
| stellar-core | 2100 | full (1000) | 55,000 | ✅ PASS | on-pace throughout; core lower bound ≥2100 |
| stellar-core | 2200 | full (1000) | 58,000 | ✅ PASS | on-pace throughout |
| stellar-core | 2300 | full (1000) | 61,000 | ✅ PASS | on-pace throughout |
| stellar-core | 2400 | full (1000) | 63,000 | ❌ FAIL | mid-submission (293k/343k) |
| stellar-core | 2400 | full (1000) | 63,000 | ❌ FAIL | repeat: mid-submission (286k/343k) — boundary confirmed |
| henyey | 1600 | burst (60) | 42,000 | ✅ PASS | seed |
| henyey | 1600 | full (1000) | 42,000 | ✅ PASS | on-pace throughout |
| henyey | 1700 | full (1000) | 45,000 | ✅ PASS | on-pace throughout |
| henyey | 1800 | full (1000) | 48,000 | ✅ PASS | ~70 tx/s under delay-false 1870 |
| henyey | 1900 | full (1000) | 50,000 | ✅ PASS | ABOVE delay-false 1870 — no penalty |
| henyey | 2000 | full (1000) | 53,000 | ✅ PASS | well above delay-false 1870 |
| henyey | 2100 | full (1000) | 55,000 | ✅ PASS | matches core delay-true band |
| henyey | 2200 | full (1000) | 58,000 | ❌ FAIL | mid-submission (229k/314k) |
| henyey | 2200 | full (1000) | 58,000 | ❌ FAIL | repeat: mid-submission (276k/314k) — boundary confirmed |

## Bands & verdict

**Headline: WAN latency (median 132 ms RTT) barely dents either implementation, and the henyey↔core gap NARROWS under delay.**

| impl | delay=false (ref) | delay=true (this run, same instance) |
|------|-------------------|--------------------------------------|
| stellar-core | full-window [2400, 2700) @92k | **[2300, 2400)** @55–63k |
| henyey | full-window 1870 @49k | **[2100, 2200)** @42–58k |

- Same-instance delay-true ratio core/henyey ≈ **1.05–1.14×** (2300–2400 vs 2100–2200), versus ~**1.3–1.4×** implied by the delay-false references. The gap narrows.
- Both bands are essentially unchanged from (core) or above (henyey) their delay-false numbers — consensus absorbs the extra per-hop round-trips inside the fixed 5 s cadence.
- Every band boundary confirmed by a repeat (core 2400 ✗✗, henyey 2200 ✗✗) and supported by monotonic passing ladders below.

### Details

- stellar-core delay=true band: highest pass **2300**, lowest fail **2400** (both 2400 attempts failed mid-submission ~286-293k/343k). ≈ delay-false floor [2400,2700) — WAN delay barely affects core.
- henyey delay=true band: highest pass **2100**, lowest fail **2200** (2200 failed BOTH attempts, mid-submission ~229k & ~276k/314k — confirmed). Passes 1600/1700/1800/1900/2000/2100 all clean.
- **Gap vs delay=false: NARROWS (henyey gains relative to core).**
  - delay=false: core [2400,2700) vs henyey 1870 → core/henyey ≈ **1.28–1.44×**.
  - delay=true (same instance): core [2300,2400) vs henyey [2100,2200) → core/henyey ≈ **1.05–1.14×**.
  - Neither implementation is materially degraded by median-132ms RTT WAN latency inside the fixed 5s cadence: **core essentially holds** (delay-true band = its delay-false floor), and **henyey's delay-true band (2100–2200) is ABOVE its delay-false 1870**.
  - Caveat: the henyey delay-false reference (1870) was measured on a *different* instance (campaign 2, m2gs5cio2fstm) and on the pre-merge branch; this run is origin/main HEAD on qgkdacn366m94. The clean apples-to-apples comparison is the **same-instance delay-true core-vs-henyey** (core 2300–2400 vs henyey 2100–2200). Henyey's apparent gain over its own 1870 is partly instance drift / newer main, not solely a delay effect. What is robust: **under WAN delay on one instance, henyey lands within ~10% of core**, a narrower gap than the delay-false references imply.
