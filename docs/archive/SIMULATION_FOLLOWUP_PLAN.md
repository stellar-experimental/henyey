# Simulation Follow-up Plan: Serious Test Build and Execution

## Goal

Build and execute a serious deterministic simulation test that goes beyond the
initial smoke/regression suite and validates long-run behavior under changing
network conditions.

## Scope for this follow-up

- Add a long-run multi-node scenario test with staged fault injection.
- Add a repeated deterministic replay stress test.
- Execute the serious test suite and record pass/fail criteria.

## Serious test design

### Scenario A: Core7 long-run with staged faults

1. Build a 7-node topology (`Topologies::core(7, OverLoopback)`).
2. Converge to ledger 20.
3. Partition one node and verify the remaining cluster still progresses.
4. Introduce temporary hard link drops on selected edges.
5. Heal partition and restore links.
6. Verify full-network convergence to ledger 60 with bounded spread.

**Acceptance:**
- Cluster makes progress during fault windows.
- Full convergence is restored after healing.
- No timeout in the scripted ledger targets.

### Scenario B: Deterministic replay stress

1. Run Scenario A twice with the same topology and fault schedule.
2. Compare final ledger hashes across all nodes.

**Acceptance:**
- Hash vectors are byte-identical between runs.

## Execution commands

```bash
cargo test -p henyey-simulation --test serious_simulation -- --nocapture
```

## Next follow-up after this one

- Replace harness-only progression with app/overlay-driven loopback execution.
- Add a larger fault matrix (message reorder, delay bands, selective peer churn).
- Run repeated CI stress (e.g., 20x test repetition in nightly jobs).

## Progress Update

- Added initial app-backed simulation engine scaffolding in `crates/simulation`.
- Added genesis bootstrapping and app lifecycle startup for app-backed TCP nodes.
- Added a passing app-backed TCP simulation test for single-node manual close.
- Added a passing app-backed core3 TCP startup/connectivity test.
- Added a passing app-backed core3 TCP ledger-advance test.
- Added passing app-backed TCP close tests for pair and core4 topologies.
- Added parity-surface topology builders for `cycle4`, `branchedcycle`,
  `hierarchical_quorum`, `hierarchical_quorum_simplified`, `custom_a`, and
  `asymmetric`.
- Fixed a consensus progress bug by bootstrapping the herder to track the next
  consensus slot (`LCL + 1`) rather than the last closed ledger.
- Implemented a real in-memory loopback overlay transport and verified
  app-backed pair/core3 loopback ledger closes.
- Added initial deterministic `LoadGenerator` / `TxGenerator` scaffolding to
  begin parity work for `src/simulation/LoadGenerator.*`.
- Connected generated load plans to real transaction submission for pair
  topologies and verified execution over both TCP and loopback.
- Remaining major parity work is real app-backed multi-node loopback/TCP
  networking plus load generation and apply-load equivalents.
