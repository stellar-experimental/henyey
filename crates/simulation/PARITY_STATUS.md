# stellar-core Parity Status

**Crate**: `henyey-simulation`
**Upstream**: `No direct stellar-core source equivalent`
**Overall Parity**: 85%
**Last Updated**: 2026-04-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Lightweight simulation | Full | Deterministic node state and convergence |
| App-backed simulation | Full | Real `App` lifecycle and restarts |
| Topology builders | Full | Core, cycle, separate, hierarchical variants |
| Connectivity controls | Full | Links, partitions, drops, TCP repair |
| Fault modeling | Partial | Link-level faults only |
| Load generation | Full | Classic and Soroban transaction builders |
| Load scenarios | Partial | Limited end-to-end scenario coverage |
| Direct apply benchmark | Partial | Core benchmark paths implemented |
| Genesis bootstrapping | Full | Standalone app initialization |

`henyey-simulation` is an internal deterministic integration harness. It has no
direct stellar-core source directory to mirror; parity is calculated against the
scoped crate capabilities required by henyey integration tests, load generation,
and apply-load benchmarking.

## File Mapping

| Scoped Component | Rust Module | Notes |
|------------------|-------------|-------|
| Simulation harness | `src/lib.rs` | Node registry, app lifecycle, crank helpers, topologies |
| Lightweight link model | `src/loopback.rs` | Partitions, links, and drop probabilities |
| Load generation | `src/loadgen.rs` | Account pools, classic/Soroban modes, transaction submission |
| Soroban transaction building | `src/loadgen_soroban.rs` | Host-function, SAC, and batch-transfer envelope builders |
| Direct apply benchmarking | `src/applyload.rs` | Ledger-close benchmark and utilization histograms |

## Component Mapping

### simulation harness (`src/lib.rs`)

Corresponds to: scoped deterministic integration harness API.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Harness construction | `Simulation::new()` / `with_network()` | Full |
| Lightweight node registry | `add_node()`, `node_ids()` | Full |
| App-node registration | `add_app_node()`, `populate_app_nodes_from_existing()` | Full |
| App lifecycle | `start_all_nodes()`, `stop_all_nodes()`, `restart_node()`, `remove_node()` | Full |
| App introspection | `app()`, `apps()`, `app_ledger_seq()`, debug/status helpers | Full |

### topology and connectivity (`src/lib.rs`, `src/loopback.rs`)

Corresponds to: scoped topology/fault controls.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Standard topology builders | `Topologies::{pair, core, cycle, separate, hierarchical_quorum, custom_a, asymmetric}` | Full |
| Pending and active links | `add_pending_connection()`, `add_connection()`, `drop_connection()` | Full |
| Partition and packet-drop controls | `partition()`, `heal()`, `set_drop_prob()` | Full |
| Connectivity repair | `repair_app_connectivity()`, `stabilize_app_tcp_connectivity()` | Full |
| Rich network fault scheduling | link-level controls only | Partial |

### cranking and convergence (`src/lib.rs`)

Corresponds to: scoped deterministic progression API.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Per-node and all-node cranking | `crank_node()`, `crank_all_nodes()` | Full |
| Predicate-based waits | `crank_until()`, `crank_for_at_most()`, `crank_for_at_least()` | Full |
| Externalization convergence checks | `have_all_externalized()`, `have_all_app_nodes_externalized()` | Full |

### load generation (`src/loadgen.rs`, `src/loadgen_soroban.rs`)

Corresponds to: scoped deterministic load-generation API.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Load configuration and modes | `GeneratedLoadConfig`, `LoadGenMode` | Full |
| Classic payment load | `LoadGenerator`, `TxGenerator::payment_transaction()` | Full |
| Soroban upload/setup/invoke load | `LoadGenerator`, `SorobanTxBuilder` | Full |
| Upgrade-setup / create-upgrade load (#3297) | `SorobanUpgradeSetup`/`SorobanCreateUpgrade`, `invoke_soroban_create_upgrade_tx`, `config_upgrade::build_config_upgrade_set` | Full |
| Pregenerated payment load (#3297) | `PayPregenerated`, `PregeneratedTxReader` | Full |
| `soroban_invoke_apply_load` (V2 invoke + APPLY_LOAD config) | deferred — explicit unsupported error (#3309) | Missing (tracked #3309) |
| Account pool, retries, and reports | `TestAccount`, `LoadReport`, `LoadResult` | Full |
| End-to-end load scenario breadth | selected tests and helpers | Partial |

### transaction builders and apply-load (`src/loadgen.rs`, `src/loadgen_soroban.rs`, `src/applyload.rs`)

Corresponds to: scoped transaction-construction and benchmark API.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Deterministic transaction generation | `TxGenerator`, `GeneratedTransaction` | Full |
| Direct apply-load benchmarking | `ApplyLoad`, `ApplyLoadConfig`, `ApplyLoadMode`, `Histogram` | Partial |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| Component | Reason |
|-----------|--------|
| Shared manually stepped virtual clock | Lightweight nodes advance explicitly; app-backed nodes run on tokio |
| Simulation-local overlay subclasses | Loopback and TCP transports live in `henyey-overlay` |
| Upgrade-contract workflow for config changes | Apply-load injects config upgrades directly for benchmark isolation |
| Pregenerated transaction-file replay | Current harness focuses on generated deterministic load |
| Mainnet/live network execution | Integration simulation is local and deterministic |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| Component | Priority | Notes |
|-----------|----------|-------|
| Rich network fault scheduling | Medium | Current faults are partition/drop-probability controls, not packet-level scripts |
| End-to-end load scenario breadth | Medium | Unit coverage is strong, but live app load runs are thinner |
| Full apply-load benchmark family | Medium | Direct and SAC paths exist; model-transaction limit search remains partial |

## Architectural Differences

1. **Harness scope**
   - **stellar-core**: Simulation is tied to stellar-core's `Application`, `VirtualClock`, and loopback overlay classes.
   - **Rust**: `henyey-simulation` is an internal integration harness over henyey crates.
   - **Rationale**: It validates Rust-node behavior directly instead of porting upstream's simulator class hierarchy.

2. **Clocking model**
   - **stellar-core**: One virtual clock advances all simulated components.
   - **Rust**: Lightweight nodes advance explicit ledger sequence state; app nodes run as tokio tasks.
   - **Rationale**: The Rust runtime is async-first and avoids a global mutable clock.

3. **Transport model**
   - **stellar-core**: Simulation owns loopback overlay connection objects.
   - **Rust**: `henyey-overlay` provides reusable loopback/TCP connection factories.
   - **Rationale**: Transport code stays shared between tests and runtime code paths.

4. **Benchmark isolation**
   - **stellar-core**: Apply-load includes multiple CLI-oriented benchmark families.
   - **Rust**: The crate exposes implemented benchmark flows directly.
   - **Rationale**: Current use cases need deterministic direct-apply measurements, not every upstream harness mode.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Lightweight simulation | No direct equivalent | 8 `#[tokio::test]` in `tests/simulation.rs` | Convergence, partitions, determinism, topologies |
| App-backed simulation | No direct equivalent | 15 `#[tokio::test]` in `tests/app_simulation.rs` | App startup, connectivity, restart, manual close |
| Long-running fault scenarios | No direct equivalent | 2 `#[tokio::test]` in `tests/serious_simulation.rs` | Seven-node deterministic schedules |
| Load generation | No direct equivalent | 14 `#[test]` across loadgen modules | Classic helpers and Soroban builders |
| Apply-load | No direct equivalent | 9 `#[test]` in `src/applyload.rs` | Unit coverage for benchmark helpers |
| **Total** | **No direct equivalent** | **48 Rust tests** | Scoped harness coverage is good but scenario breadth remains partial |

### Test Gaps

- No Rust integration test drives a long full-rate `LoadGenerator::generate_load()`
  run across every classic and Soroban mode.
- Packet-level fault schedules and model-transaction apply-load search are not
  covered because those capabilities are still partial.
- Batch-transfer SAC TPS paths have helper coverage but limited end-to-end app
  simulation coverage.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 17 |
| Gaps (None + Partial) | 3 |
| Intentional Omissions | 5 |
| **Parity** | **17 / (17 + 3) = 85%** |
