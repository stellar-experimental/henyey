# maxtps-optimization run logs

Each autonomous max-TPS optimization run (the `/maxtps-optimize` skill) maintains
one document in this directory:

```
docs/maxtps-optimization/<UTC-date>-target<N>.md
```

It is the durable record of that run: the baseline, every hypothesis tested, the
instrumentation added, the measurement, the accept/reject decision, and the
resulting PRs. One row per hypothesis.

## Methodology (shared by all runs)

- Measured with Supercluster `MissionMaxTPSClassic` (23-node tier-1) on a single
  Namespace (nsc) 32×64 VM, `--install-network-delay false`. See
  [`../maxtps-baseline.md`](../maxtps-baseline.md) for the reference numbers and
  why the limiter is apply/consensus-side (idle CPU, under-filled ledgers).
- **Short-probe**: `SSC_MAXTPS_TXS_MULTIPLIER=60` (≈1-min offer window; tracks the
  canonical ceiling within ~2%) + `SSC_LOADGEN_FASTFAIL_LEDGERS=10` (early-abort
  on doomed steps). Both knobs are test-harness-only and parity-irrelevant.
- **Accept bar**: a change is kept only if it improves max TPS by **> 5%** on the
  short-probe and preserves the observable/interop surface in
  [`../PARITY.md`](../PARITY.md) bit-for-bit.

## Document template

```markdown
# maxtps-optimize run — <UTC date>, target <TARGET> tx/s

- Session: <SID> · Instance: <INST> (nsc 32×64) · Base commit: <sha>
- Methodology: short-probe (SSC_MAXTPS_TXS_MULTIPLIER=60,
  SSC_LOADGEN_FASTFAIL_LEDGERS=10). Accept bar: Δ > 5%.
- **Current best: <N> tx/s** @ <commit/PR> · Target: <TARGET>

## Hypotheses

| # | hypothesis | instrumentation added | measurement | Δ vs best | status | notes / next |
|---|------------|-----------------------|-------------|-----------|--------|--------------|
| 0 | baseline   | —                     | <N> tx/s    | —         | —      | base image |
| 1 | …          | …                     | …           | …         | pending| … |

## Summary (filled at end)
- Baseline → final: <A> → <B> tx/s (+X%, M of K accepted)
- PRs: #…
- Top remaining pending hypotheses: …
```

`status ∈ {pending, testing, accepted, rejected}`.
