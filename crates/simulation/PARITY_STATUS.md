# stellar-core Parity Status

**Crate**: `henyey-simulation`
**Upstream**: `stellar-core/src/simulation/`
**Overall Parity**: 72%
**Last Updated**: 2026-03-08

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Simulation lifecycle | Full | Core add/start/stop/remove/restart implemented |
| Connection management | Full | add/drop connections, directed disconnect, link queries |
| Crank / time advancement | Full | crankAllNodes, crankNode, crankForAtMost, crankForAtLeast, crankUntil |
| Topology builders | Full | All 10 topology types implemented (incl. separate_with_watchers) |
| Load generation | Partial | Full Pay mode lifecycle; no Soroban modes |
| Transaction generation | Partial | Account cache, payment tx, fee generation; no Soroban tx types |
| ApplyLoad | None | Not implemented (Tier 4 — deferred) |
| Genesis bootstrapping | Full | initialize_genesis_ledger fully sets up standalone nodes |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Simulation.h` / `Simulation.cpp` | `lib.rs` | Core simulation harness |
| `Topologies.h` / `Topologies.cpp` | `lib.rs` (`Topologies`) | All topology builders present |
| `LoadGenerator.h` / `LoadGenerator.cpp` | `loadgen.rs` | Full Pay mode; account pool, rate limiter, retry logic |
| `TxGenerator.h` / `TxGenerator.cpp` | `loadgen.rs` (`TxGenerator`) | Account cache, payment tx, fee generation |
| `ApplyLoad.h` / `ApplyLoad.cpp` | — | Not implemented (deferred) |
| `CoreTests.cpp` | `tests/` | Upstream test file; partial Rust coverage |

## Component Mapping

### Simulation (`lib.rs`)

Corresponds to: `Simulation.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Simulation()` constructor | `new()` / `with_network()` | Full |
| `~Simulation()` destructor | `stop_all_nodes()` | Full |
| `setCurrentVirtualTime(time_point)` | — | Intentional Omission |
| `setCurrentVirtualTime(system_time_point)` | — | Intentional Omission |
| `addNode()` | `add_node()` / `add_app_node()` | Full |
| `getNode()` | `app()` | Full |
| `getNodes()` | `apps()` | Full |
| `getNodeIDs()` | `node_ids()` / `app_node_ids()` | Full |
| `addPendingConnection()` | `add_pending_connection()` | Full |
| `getLoopbackConnection()` | — | None |
| `startAllNodes()` | `start_all_nodes()` / `try_start_all_nodes()` | Full |
| `stopAllNodes()` | `stop_all_nodes()` | Full |
| `removeNode()` | `remove_node()` | Full |
| `getAppFromPeerMap()` | `app_by_port()` | Full |
| `haveAllExternalized()` | `have_all_externalized()` / `have_all_app_nodes_externalized()` | Full |
| `crankNode()` | `crank_node()` | Full |
| `crankAllNodes()` | `crank_all_nodes()` | Full |
| `crankForAtMost()` | `crank_for_at_most()` | Full |
| `crankForAtLeast()` | `crank_for_at_least()` | Full |
| `crankUntil(fn, timeout)` | `crank_until()` | Full |
| `crankUntil(time_point)` | — | Intentional Omission |
| `crankUntil(system_time_point)` | — | Intentional Omission |
| `metricsSummary()` | — | Intentional Omission |
| `addConnection()` | `add_connection()` | Full |
| `dropConnection()` | `drop_connection()` | Full |
| `newConfig()` | `build_app_from_spec()` | Full |
| `stopOverlayTick()` | — | Intentional Omission |
| `getExpectedLedgerCloseTime()` | `expected_ledger_close_time()` | Full |
| `isSetUpForSorobanUpgrade()` | — | None (Soroban-specific) |
| `markReadyForSorobanUpgrade()` | — | None (Soroban-specific) |
| Link query (`hasLoopbackLink`) | `has_loopback_link()` | Full |

### Topologies (`lib.rs`)

Corresponds to: `Topologies.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `pair()` | `pair()` | Full |
| `cycle4()` | `cycle4()` | Full |
| `core()` | `core()` / `core3()` | Full |
| `cycle()` | `cycle()` | Full |
| `branchedcycle()` | `branchedcycle()` | Full |
| `separate()` | `separate()` | Full |
| `separate(n, watchers, mode)` | `separate_with_watchers()` | Full |
| `hierarchicalQuorum()` | `hierarchical_quorum()` | Full |
| `hierarchicalQuorumSimplified()` | `hierarchical_quorum_simplified()` | Full |
| `customA()` | `custom_a()` | Full |
| `asymmetric()` | `asymmetric()` | Full |

### LoadGenerator (`loadgen.rs`)

Corresponds to: `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LoadGenerator()` constructor | `LoadGenerator::new()` | Full |
| `LoadGenMode` enum | `LoadGenMode` (Pay only) | Partial |
| `GeneratedLoadConfig` | `GeneratedLoadConfig` | Partial |
| `GeneratedLoadConfig::txLoad()` | `GeneratedLoadConfig::tx_load()` | Full |
| `GeneratedLoadConfig::isDone()` | `GeneratedLoadConfig::is_done()` | Full |
| `GeneratedLoadConfig::areTxsRemaining()` | `GeneratedLoadConfig::are_txs_remaining()` | Full |
| `generateLoad()` | `generate_load()` | Full |
| `getTxPerStep()` (rate limiter) | `get_tx_per_step()` | Full |
| `getNextAvailableAccount()` | `get_next_available_account()` | Full |
| `cleanupAccounts()` | `cleanup_accounts()` | Full |
| `submitTx()` (with BAD_SEQ retry) | `submit_tx()` | Full |
| `isDone()` | `is_done()` | Full |
| `stop()` | `stop()` | Full |
| `checkAccountSynced()` | `check_account_synced()` | Full |
| `accounts_available` / `accounts_in_use` pool | Same pattern | Full |
| Step plan generation (legacy) | `step_plan()` | Full |
| Load summarization (legacy) | `summarize()` | Full |
| `getConfigUpgradeSetKey()` | — | None (Soroban) |
| `checkSorobanWasmSetup()` | — | None (Soroban) |
| `checkMinimumSorobanSuccess()` | — | None (Soroban) |
| `checkSorobanStateSynced()` | — | None (Soroban) |
| Soroban mode dispatch | — | None (Soroban) |
| Spike interval logic | — | None |

### TxGenerator (`loadgen.rs`)

Corresponds to: `TxGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `TxGenerator()` constructor | `TxGenerator::new()` | Full |
| `mAccounts` cache | `accounts: BTreeMap<u64, TestAccount>` | Full |
| `findAccount()` | `find_account()` | Full |
| `loadAccount()` | `load_account()` | Full |
| `createAccounts()` | `create_accounts()` | Full |
| `createTransactionFramePtr()` | `create_transaction_frame()` | Full |
| `paymentTransaction()` | `payment_transaction()` | Full |
| `generateFee()` | `generate_fee()` | Full |
| `pickAccountPair()` | `pick_account_pair()` | Full |
| Deterministic key derivation | `deterministic_seed()` / `TestAccount::from_name()` | Full |
| `createUploadWasmTransaction()` | — | None (Soroban) |
| `createContractTransaction()` | — | None (Soroban) |
| `createSACTransaction()` | — | None (Soroban) |
| `invokeSorobanLoadTransaction()` | — | None (Soroban) |
| `invokeSorobanLoadTransactionV2()` | — | None (Soroban) |
| `invokeSACPayment()` | — | None (Soroban) |
| `invokeBatchTransfer()` | — | None (Soroban) |
| `invokeSorobanCreateUpgradeTransaction()` | — | None (Soroban) |
| `sorobanRandomWasmTransaction()` | — | None (Soroban) |
| `payment_series()` (legacy stateless) | `TxGenerator::payment_series()` | Full |

### Herder additions

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Herder::sourceAccountPending()` | `Herder::source_account_pending()` | Full |

### App additions

| stellar-core | Rust | Status |
|--------------|------|--------|
| `App::getExpectedLedgerCloseTime()` | `App::expected_ledger_close_time()` | Full |
| `App::loadAccountSequence()` | `App::load_account_sequence()` | Full |
| `App::sourceAccountPending()` | `App::source_account_pending()` | Full |
| `App::baseFee()` | `App::base_fee()` | Full |
| `App::currentLedgerSeq()` | `App::current_ledger_seq()` | Full |

### ApplyLoad (not implemented — deferred)

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
| Medida metrics integration / `metricsSummary()` | Rust uses different metrics approach; no medida dependency |
| `setCurrentVirtualTime()` | Not needed — Rust async model handles time differently |
| `crankUntil(time_point)` / `crankUntil(system_time_point)` | Doesn't map well to Rust async model; predicate-based `crank_until` covers all test needs |
| `stopOverlayTick()` | Overlay tick control managed by tokio runtime, not manual stop |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `getLoopbackConnection()` | Low | No direct loopback connection object exposure |
| `isSetUpForSorobanUpgrade()` / `markReadyForSorobanUpgrade()` | Low | Soroban upgrade coordination |
| Soroban `LoadGenMode` variants | Low | SOROBAN_UPLOAD, SOROBAN_INVOKE, MIXED_CLASSIC_SOROBAN, etc. |
| Soroban TxGenerator methods | Low | Upload, invoke, SAC, batch transfer tx builders |
| Spike interval logic in rate limiter | Low | Periodic burst feature |
| `ApplyLoad` | Medium | Benchmark infrastructure not implemented |
| Soroban LoadGenerator checks | Low | checkSorobanWasmSetup, checkMinimumSorobanSuccess, checkSorobanStateSynced |

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
   - **Rust**: Full Pay mode lifecycle with cumulative-target rate limiter, account pool (available/in-use), `txBAD_SEQ` retry, and sequence refresh. Legacy simple `step_plan()` API retained for manual-close simulations.
   - **Rationale**: Pay mode is the primary mode for consensus parity tests; Soroban modes deferred until needed.

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
| LoadGenerator tests | 8 TEST_CASE / 5 SECTION | 7 `#[test]` (loadgen.rs) | Determinism, config, seed padding, account derivation |

### Test Gaps

- No Rust tests for Soroban load generation (stellar-core has extensive Soroban loadgen tests)
- No Rust tests for `ApplyLoad` benchmarking
- No integration test exercising the new stateful `LoadGenerator::generate_load()` API
- Restart/rejoin tests pass for both TCP and loopback modes

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 52 |
| Gaps (None + Partial) | 20 |
| Intentional Omissions | 5 |
| **Parity** | **52 / (52 + 20) = 72%** |
