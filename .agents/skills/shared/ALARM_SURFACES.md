# Alarm Surfaces Policy

This document defines the relationship between the two alarm surfaces in henyey
and the policy for managing overlaps between them.

## Surfaces

### 1. Monitor-tick (`metric-alarms.toml`)

- **Location:** `.claude/skills/shared/metric-alarms.toml`
- **Consumer:** `scripts/lib/eval-alarms.py`, invoked by the `monitor-tick` LLM skill
- **Runs:** Inside the validator process session, every ~20 minutes
- **Capabilities:** In-process action — can file GitHub issues, trigger restarts,
  initiate wipe-recovery, adjust operational state
- **Semantics:** Per-tick delta comparison (current vs previous scrape), gauge
  threshold checks, multi-tick persistence (`for_ticks`)

### 2. Grafana SLO alerts (`henyey-slo-alerts.yaml`)

- **Location:** `metrics/alerts/henyey-slo-alerts.yaml`
- **Consumer:** Prometheus Alertmanager via Grafana provisioning
- **Runs:** Continuously via Prometheus scraping (~15s interval)
- **Capabilities:** Operator paging, fleet aggregation, historical context,
  cross-validator queries, SLO tracking
- **Semantics:** PromQL `rate()`, `increase()` over time windows (5m, 10m),
  `for`-duration persistence

## Ownership Policy

| Alarm class | Primary surface | Rationale |
|---|---|---|
| In-process action (restart, wipe, issue filing) | monitor-tick | Needs process-local context to act |
| SLO/operator paging | Grafana | Needs historical context, fleet-wide view |
| Process health (up/down) | Grafana | Process-down makes in-process alarms unreachable |
| Quorum/consensus health | Both | Monitor-tick files issues; Grafana pages operators |
| Counter spikes (invariant, tx error) | Both | Monitor-tick for immediate filing; Grafana for rate-based SLO |

## Overlap Policy

When an alarm legitimately belongs in both surfaces:

1. Add a `[[mirrored]]` entry to `alarm-surfaces.toml` mapping the TOML alarm
   name to its Grafana UID(s), with summary descriptions of each surface's
   detection semantics.

2. **Thresholds and severities may intentionally differ.** Monitor-tick uses
   per-tick deltas and gauge snapshots; Grafana uses `rate()`/`increase()` over
   time windows. A monitor-tick `threshold = 1` on a counter is not equivalent
   to a Grafana `increase(...[10m]) > 0` — the former detects any single-tick
   increment, the latter smooths over a 10-minute window. These are
   **surface mirrors, not threshold-identical mirrors.**

3. When adding or modifying a mirrored alarm, check the other surface and
   update the corresponding `alarm-surfaces.toml` entry if warranted.

## Reconciliation Table

<!-- BEGIN GENERATED RECONCILIATION TABLE -->
| Alarm | TOML name | Grafana UID(s) | monitor-tick | Grafana | Notes |
|---|---|---|---|---|---|
| Invariant failure | `ledger-invariant-failure` | `henyey-invariant-failure` | counter ≥ 1, ACTION | increase > 0 over 10m, critical | Monitor-tick best-effort for strict (panic before scrape) |
| TX internal error | `tx-internal-error` | `henyey-tx-internal-error` | counter ≥ 1, ACTION | rate > 0.01 over 5m, critical | Different detection semantics (any vs rate) |
| Quorum fail-at | `quorum-fail-at-low` | `henyey-quorum-failat-warn`, `henyey-quorum-failat-crit` | gauge ≤ 0, WARN, 3 ticks | == 1 warn / < 1 critical, 5m | Monitor-tick collapses tiers onto Grafana's critical (< 1); Grafana's == 1 warn tier remains fleet-wide visibility only |
| Post-catchup reset | `post-catchup-hard-reset` | `henyey-post-catchup-hard-reset` | counter ≥ 1, ACTION | increase > 0 over 10m, critical |  |
| Recovery stalled | `recovery-stalled` | `henyey-recovery-stalled` | counter-streak ≥ 3, WARN | increase >= 3 over 10m, warning |  |
| Fetch channel depth | `fetch-channel-deep` | `henyey-overlay-fetch-depth` | gauge > 500, WARN, 2 ticks | > 128, warning, 5m | Different thresholds |
| FD exhaustion | `fd-exhaustion` | `henyey-fd-exhaustion-warn`, `henyey-fd-exhaustion-crit` | ratio > 0.85, WARN | > 0.6 warn / > 0.8 crit, 5m | Different thresholds |
| Ledger age | `ledger-age-high` | `henyey-ledger-age-warn`, `henyey-ledger-age-crit` | gauge > 30, SYNC | > 10 warn / > 30 crit, 5m; guards on herder_state == 2 |  |
| Peer count | `peer-count-low` | `henyey-peer-count-warn`, `henyey-peer-count-crit` | gauge < 8, WARN | < 8 warn / < 3 crit, 5m |  |
| Validator not tracking | `herder-not-tracking` | `henyey-validator-not-tracking` | gauge != 2, SYNC | != bool 2, warning, 20m |  |
| Fragmentation | `jemalloc-frag-high` | `henyey-fragmentation` | gauge > 50, WARN, 2 ticks | > 50%, warning, 15m | Matching thresholds |
| Quorum intersection lost | `quorum-intersection-lost` | `henyey-quorum-intersection-lost` | gauge == 0, ACTION | < 1, critical, 5m | Both surfaces per policy |
<!-- END GENERATED RECONCILIATION TABLE -->

## Intentional Non-Overlaps (Grafana-Only)

These Grafana alerts intentionally have no monitor-tick counterpart:

<!-- BEGIN GENERATED NON-OVERLAPS TABLE -->
| Grafana UID | Alert | Rationale |
|---|---|---|
| `henyey-process-down` | Process Down | Process-down makes in-process alarms unreachable |
| `henyey-recovery-txset-stuck` | Recovery TX Set Stuck | No current monitor-tick action needed |
| `henyey-quorum-missing` | Quorum Missing Peers | Fleet-level visibility needed; monitor-tick covers via quorum-fail-at-low |
<!-- END GENERATED NON-OVERLAPS TABLE -->

## Default for New Alarms

1. New alarms go to `metric-alarms.toml` first (in-process action).
2. Add a Grafana rule when operator paging, fleet-wide visibility, or
   time-windowed rate analysis is needed.
3. Document the overlap by adding a `[[mirrored]]` entry to
   `alarm-surfaces.toml`, then run `gen-alarm-surfaces.py` to update
   this document.
