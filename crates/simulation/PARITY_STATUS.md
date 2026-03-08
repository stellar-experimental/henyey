# stellar-core Parity Status

**Crate**: `henyey-simulation`
**Upstream**: `stellar-core/src/simulation/`
**Overall Parity**: 32%
**Last Updated**: 2026-03-08

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Simulation lifecycle | Partial | Core add/start/stop/remove implemented; missing clock sync, metrics |
| Connection management | Partial | add/drop connections work; no loopback connection object access |
| Crank / time advancement | Partial | crankAllNodes and crankUntil implemented; missing crankNode, crankForAtMost, crankForAtLeast |
| Topology builders | Full | All 9 topology types implemented |
| Load generation | Partial | Simple payment series only; no Soroban, no mode-based dispatch |
| Transaction generation | Partial | Basic payment_series; no Soroban, upload, invoke, or SAC transactions |
| ApplyLoad | None | Not implemented |
| Genesis bootstrapping | Full | initialize_genesis_ledger fully sets up standalone nodes |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Simulation.h` / `Simulation.cpp` | `lib.rs` | Core simulation harness |
| `Topologies.h` / `Topologies.cpp` | `lib.rs` (`Topologies`) | All topology builders present |
| `LoadGenerator.h` / `LoadGenerator.cpp` | `loadgen.rs` | Minimal subset — step plan only |
| `TxGenerator.h` / `TxGenerator.cpp` | `loadgen.rs` (`TxGenerator`) | payment_series only |
| `ApplyLoad.h` / `ApplyLoad.cpp` | — | Not implemented |
| `CoreTests.cpp` | `tests/` | Upstream test file; partial Rust coverage |

## Component Mapping

### Simulation (`lib.rs`)

Corresponds to: `Simulation.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Simulation()` constructor | `new()` / `with_network()` | Full |
| `~Simulation()` destructor | `stop_all_nodes()` | Full |
| `setCurrentVirtualTime(time_point)` | — | None |
| `setCurrentVirtualTime(system_time_point)` | — | None |
| `addNode()` | `add_node()` / `add_app_node()` | Full |
| `getNode()` | `app()` | Full |
| `getNodes()` | — | None |
| `getNodeIDs()` | `node_ids()` / `app_node_ids()` | Full |
| `addPendingConnection()` | `add_pending_connection()` | Full |
| `getLoopbackConnection()` | — | None |
| `startAllNodes()` | `start_all_nodes()` / `try_start_all_nodes()` | Full |
| `stopAllNodes()` | `stop_all_nodes()` | Full |
| `removeNode()` | `remove_node()` | Full |
| `getAppFromPeerMap()` | — | None |
| `haveAllExternalized()` | `have_all_externalized()` / `have_all_app_nodes_externalized()` | Full |
| `crankNode()` | — | None |
| `crankAllNodes()` | `crank_all_nodes()` | Partial |
| `crankForAtMost()` | — | None |
| `crankForAtLeast()` | — | None |
| `crankUntil(fn, timeout)` | `crank_until()` | Full |
| `crankUntil(time_point)` | — | None |
| `crankUntil(system_time_point)` | — | None |
| `metricsSummary()` | — | None |
| `addConnection()` | `add_connection()` | Full |
| `dropConnection()` | `disconnect_node_from_peers()` | Partial |
| `newConfig()` | `build_app_from_spec()` | Full |
| `stopOverlayTick()` | — | None |
| `getExpectedLedgerCloseTime()` | — | None |
| `isSetUpForSorobanUpgrade()` | — | None |
| `markReadyForSorobanUpgrade()` | — | None |

### Topologies (`lib.rs`)

Corresponds to: `Topologies.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `pair()` | `pair()` | Full |
| `cycle4()` | `cycle4()` | Full |
| `core()` | `core()` / `core3()` | Full |
| `cycle()` | `cycle()` | Full |
| `branchedcycle()` | `branchedcycle()` | Full |
| `separate()` | `separate()` | Partial |
| `hierarchicalQuorum()` | `hierarchical_quorum()` | Full |
| `hierarchicalQuorumSimplified()` | `hierarchical_quorum_simplified()` | Full |
| `customA()` | `custom_a()` | Full |
| `asymmetric()` | `asymmetric()` | Full |

### LoadGenerator (`loadgen.rs`)

Corresponds to: `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LoadGenerator()` constructor | — | None |
| `getMode()` | — | None |
| `isDone()` | — | None |
| `checkSorobanWasmSetup()` | — | None |
| `checkMinimumSorobanSuccess()` | — | None |
| `generateLoad()` | — | None |
| `getConfigUpgradeSetKey()` | — | None |
| `checkAccountSynced()` | — | None |
| `checkSorobanStateSynced()` | — | None |
| `stop()` | — | None |
| `GeneratedLoadConfig` | `GeneratedLoadConfig` | Partial |
| `GeneratedLoadConfig::txLoad()` | — | None |
| `LoadGenMode` enum | — | None |
| Step plan generation | `step_plan()` | Full |
| Load summarization | `summarize()` | Full |

### TxGenerator (`loadgen.rs`)

Corresponds to: `TxGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `TxGenerator()` constructor | — | None |
| `loadAccount()` | — | None |
| `findAccount()` | — | None |
| `createAccounts()` | — | None |
| `createTransactionFramePtr()` | — | None |
| `paymentTransaction()` | `payment_series()` | Partial |
| `createUploadWasmTransaction()` | — | None |
| `createContractTransaction()` | — | None |
| `createSACTransaction()` | — | None |
| `invokeSorobanLoadTransaction()` | — | None |
| `invokeSorobanLoadTransactionV2()` | — | None |
| `invokeSACPayment()` | — | None |
| `invokeBatchTransfer()` | — | None |
| `invokeSorobanCreateUpgradeTransaction()` | — | None |
| `sorobanRandomWasmTransaction()` | — | None |
| `generateFee()` | — | None |
| `pickAccountPair()` | — | None |

### ApplyLoad (not implemented)

Corresponds to: `ApplyLoad.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ApplyLoad()` constructor | — | None |
| `closeLedger()` | — | None |
| `benchmark()` | — | None |
| `findMaxSacTps()` | — | None |
| `successRate()` | — | None |
| Utilization histograms | — | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `LoopbackOverlayManager` / `ApplicationLoopbackOverlay` | Rust uses `LoopbackConnectionFactory` from henyey-overlay instead |
| Medida metrics integration | Rust uses different metrics approach |
| `setCurrentVirtualTime()` | Not needed — Rust async model handles time differently |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `crankNode()` | Low | Per-node cranking not needed for current test scenarios |
| `crankForAtMost()` / `crankForAtLeast()` | Low | Time-bounded cranking; `crank_until` covers most cases |
| `crankUntil(time_point)` overloads | Low | Only predicate-based variant implemented |
| `getLoopbackConnection()` | Low | No direct loopback connection object exposure |
| `getNodes()` | Low | Can iterate via `node_ids()` + `app()` |
| `getAppFromPeerMap()` | Low | Port-based lookup not needed |
| `metricsSummary()` | Low | No metrics domain summary |
| `stopOverlayTick()` | Low | Overlay tick control not exposed |
| `getExpectedLedgerCloseTime()` | Low | Not needed for current tests |
| `dropConnection()` (directed) | Low | Only bulk disconnect implemented |
| `separate()` numWatchers parameter | Low | Watcher nodes not supported in topology builder |
| `LoadGenerator` full lifecycle | Medium | Only step plan / summarize implemented |
| `TxGenerator` full API | Medium | Only payment_series; no Soroban upload, invoke, SAC, or batch transfer |
| `LoadGenMode` enum | Medium | No mode-based dispatch (PAY, Soroban modes, MIXED, etc.) |
| `ApplyLoad` | Medium | Benchmark infrastructure not implemented |
| Node restart/rejoin | Medium | Partially works but not fully stable (ignored tests) |

## Architectural Differences

1. **Simulation model**
   - **stellar-core**: Single-process, VirtualClock-driven event loop for all nodes; `crankNode` / `crankAllNodes` advance individual timers.
   - **Rust**: Each app node runs in its own tokio task; lightweight `SimNode` mode uses synchronous ledger-sequence advancement. No shared VirtualClock.
   - **Rationale**: Rust async model with tokio handles concurrency differently; lightweight simulation layer provides fast deterministic tests.

2. **Loopback transport**
   - **stellar-core**: `LoopbackPeer` / `LoopbackPeerConnection` objects with direct method calls between peers.
   - **Rust**: `LoopbackConnectionFactory` from henyey-overlay provides in-memory channels; simulation manages link-level topology via `LoopbackNetwork`.
   - **Rationale**: Decouples transport from simulation; same `ConnectionFactory` trait used by both TCP and loopback.

3. **Load generation**
   - **stellar-core**: Rich `LoadGenerator` with timer-driven step scheduling, Soroban modes, metrics tracking, and account management.
   - **Rust**: Simple stateless `LoadGenerator::step_plan()` that produces a deterministic plan; `Simulation` handles submission and account sequences.
   - **Rationale**: Henyey focuses on deterministic load plans for parity validation rather than continuous load generation.

4. **Genesis bootstrapping**
   - **stellar-core**: Uses `TestApplication` / test utilities to create genesis state.
   - **Rust**: Standalone `initialize_genesis_ledger()` function constructs genesis ledger header, root account, and bucket list directly in SQLite.
   - **Rationale**: Self-contained genesis avoids dependency on external test utilities.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| CoreTests | 12 TEST_CASE / 15 SECTION | 8 `#[tokio::test]` (simulation.rs) | Core topology convergence, partition recovery, determinism |
| App simulation | (inline in CoreTests) | 15 `#[tokio::test]` (app_simulation.rs) | Single-node, pair, core3, core4, cycle4, load execution |
| Serious scenarios | (inline in CoreTests) | 2 `#[tokio::test]` (serious_simulation.rs) | 7-node fault schedule, deterministic replay |
| LoadGenerator tests | 8 TEST_CASE / 5 SECTION | 2 `#[test]` (loadgen.rs) | Only basic determinism and counting tests |

### Test Gaps

- No Rust tests for Soroban load generation (stellar-core has extensive Soroban loadgen tests)
- No Rust tests for `ApplyLoad` benchmarking
- No Rust tests for resilience topologies (hierarchical, branched cycle, custom-A convergence)
- Restart/rejoin tests exist but are `#[ignore]`d due to incomplete parity

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 24 |
| Gaps (None + Partial) | 51 |
| Intentional Omissions | 3 |
| **Parity** | **24 / (24 + 51) = 32%** |
