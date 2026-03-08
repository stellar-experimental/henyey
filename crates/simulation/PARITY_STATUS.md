# stellar-core Parity Status

**Crate**: `henyey-simulation`
**Upstream**: `stellar-core/src/simulation/`
**Overall Parity**: 95%
**Last Updated**: 2026-03-08

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Simulation lifecycle | Full | Core add/start/stop/remove/restart implemented |
| Connection management | Full | add/drop connections, directed disconnect, link queries |
| Crank / time advancement | Full | crankAllNodes, crankNode, crankForAtMost, crankForAtLeast, crankUntil |
| Topology builders | Full | All 11 topology types implemented |
| Load generation (classic) | Full | Pay mode lifecycle, account pool, rate limiter, retry logic |
| Load generation (Soroban) | Full | Upload, InvokeSetup, Invoke, MixedClassicSoroban modes |
| Transaction generation (classic) | Full | Account cache, payment tx, fee generation |
| Transaction generation (Soroban) | Full | 7 Soroban tx builder methods |
| Soroban state management | Full | State sync checks, success rate checks, state reset |
| ApplyLoad benchmark | Full | close_ledger, benchmark, findMaxSacTps, utilization histograms |
| Config upgrade contract | Partial | Stubs only — henyey uses direct LedgerUpgrade instead |
| Genesis bootstrapping | Full | initialize_genesis_ledger fully sets up standalone nodes |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Simulation.h` / `Simulation.cpp` | `lib.rs` | Core simulation harness |
| `Topologies.h` / `Topologies.cpp` | `lib.rs` (`Topologies`) | All topology builders present |
| `LoadGenerator.h` / `LoadGenerator.cpp` | `loadgen.rs` | Full classic + Soroban load generation |
| `TxGenerator.h` / `TxGenerator.cpp` | `loadgen.rs` (`TxGenerator`) + `loadgen_soroban.rs` | Account cache, classic + Soroban tx builders |
| `ApplyLoad.h` / `ApplyLoad.cpp` | `applyload.rs` | Full benchmark harness |
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
| `isSetUpForSorobanUpgrade()` | `is_setup_for_soroban_upgrade()` | Full |
| `markReadyForSorobanUpgrade()` | `mark_ready_for_soroban_upgrade()` | Full |
| `Mode` enum | `SimulationMode` | Full |
| `hasLoopbackLink()` | `has_loopback_link()` | Full |

### Topologies (`lib.rs`)

Corresponds to: `Topologies.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `pair()` | `pair()` | Full |
| `cycle4()` | `cycle4()` | Full |
| `core()` | `core()` / `core3()` | Full |
| `cycle()` | `cycle()` | Full |
| `branchedcycle()` | `branchedcycle()` | Full |
| `separate(n, threshold, mode, networkID)` | `separate()` | Full |
| `separate(n, threshold, mode, networkID, watchers)` | `separate_with_watchers()` | Full |
| `hierarchicalQuorum()` | `hierarchical_quorum()` | Full |
| `hierarchicalQuorumSimplified()` | `hierarchical_quorum_simplified()` | Full |
| `customA()` | `custom_a()` | Full |
| `asymmetric()` | `asymmetric()` | Full |

### LoadGenMode (`loadgen.rs`)

Corresponds to: `LoadGenMode` enum in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PAY` | `Pay` | Full |
| `SOROBAN_UPLOAD` | `SorobanUpload` | Full |
| `SOROBAN_INVOKE_SETUP` | `SorobanInvokeSetup` | Full |
| `SOROBAN_INVOKE` | `SorobanInvoke` | Full |
| `MIXED_CLASSIC_SOROBAN` | `MixedClassicSoroban` | Full |
| `SOROBAN_UPGRADE_SETUP` | — | Intentional Omission |
| `SOROBAN_CREATE_UPGRADE` | — | Intentional Omission |
| `PAY_PREGENERATED` | — | Intentional Omission |
| `SOROBAN_INVOKE_APPLY_LOAD` | — | Intentional Omission |

### GeneratedLoadConfig (`loadgen.rs`)

Corresponds to: `GeneratedLoadConfig` in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `txLoad()` | `tx_load()` | Full |
| `isDone()` | `is_done()` | Full |
| `areTxsRemaining()` | `are_txs_remaining()` | Full |
| `isSoroban()` | `LoadGenMode::is_soroban()` | Full |
| `isSorobanSetup()` | `LoadGenMode::is_soroban_setup()` | Full |
| `isLoad()` | `LoadGenMode::is_load()` | Full |
| `modeInvokes()` | `LoadGenMode::mode_invokes()` | Full |
| `modeSetsUpInvoke()` | `LoadGenMode::mode_sets_up_invoke()` | Full |
| `SorobanConfig` (nInstances, nWasms) | `n_instances` / `n_wasms` fields | Full |
| `MixClassicSorobanConfig` (weights) | `mix_pay_weight` / `mix_upload_weight` / `mix_invoke_weight` | Full |
| `spikeInterval` / `spikeSize` | `spike_interval` / `spike_size` | Full |
| `minSorobanPercentSuccess` | `min_soroban_percent_success` | Full |
| `maxGeneratedFeeRate` | `max_fee_rate` | Full |
| `skipLowFeeTxs` | `skip_low_fee_txs` | Full |
| `modeUploads()` | — | None |
| `getStatus()` | — | None |
| `createSorobanInvokeSetupLoad()` | — | None |
| `pregeneratedTxLoad()` | — | Intentional Omission |
| `createSorobanUpgradeSetupLoad()` | — | Intentional Omission |
| `copySorobanNetworkConfigToUpgradeConfig()` | — | Intentional Omission |
| `SorobanUpgradeConfig` accessors | — | Intentional Omission |

### LoadGenerator (`loadgen.rs`)

Corresponds to: `LoadGenerator` in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LoadGenerator()` constructor | `LoadGenerator::new()` | Full |
| `getMode()` | Implicit via `LoadGenMode` | Full |
| `isDone()` | `is_done()` | Full |
| `generateLoad()` | `generate_load()` | Full |
| `getTxPerStep()` (with spike logic) | `get_tx_per_step()` | Full |
| `getNextAvailableAccount()` | `get_next_available_account()` | Full |
| `cleanupAccounts()` | `cleanup_accounts()` | Full |
| `submitTx()` (with BAD_SEQ retry) | `submit_tx()` | Full |
| `stop()` | `stop()` | Full |
| `checkAccountSynced()` | `check_account_synced()` | Full |
| `checkMinimumSorobanSuccess()` | `check_minimum_soroban_success()` | Full |
| `checkSorobanStateSynced()` | `check_soroban_state_synced()` | Full |
| `resetSorobanState()` | `reset_soroban_state()` | Full |
| Account pool (available/in_use) | Same pattern | Full |
| Soroban mode dispatch | Mode-aware `generate_load()` | Full |
| Step plan generation (legacy) | `step_plan()` | Full |
| Load summarization (legacy) | `summarize()` | Full |
| `checkSorobanWasmSetup()` | — | None |
| `getConfigUpgradeSetKey()` | — | Intentional Omission |

### TxGenerator (`loadgen.rs` + `loadgen_soroban.rs`)

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
| `getAccounts()` | `accounts()` | Full |
| `getAccount()` | `get_account()` | Full |
| `createUploadWasmTransaction()` | `create_upload_wasm_transaction()` | Full |
| `createContractTransaction()` | `create_contract_transaction()` | Full |
| `createSACTransaction()` | `create_sac_transaction()` | Full |
| `invokeSorobanLoadTransaction()` | `invoke_soroban_load_transaction()` | Full |
| `invokeSACPayment()` | `invoke_sac_payment()` | Full |
| `invokeBatchTransfer()` | `invoke_batch_transfer()` | Full |
| `sorobanRandomWasmTransaction()` | `soroban_random_wasm_transaction()` | Full |
| `payment_series()` (legacy) | `payment_series()` | Full |
| `invokeSorobanLoadTransactionV2()` | — | None |
| `invokeSorobanCreateUpgradeTransaction()` | — | Intentional Omission |
| `getConfigUpgradeSetFromLoadConfig()` | — | Intentional Omission |
| `getConfigUpgradeSetKey()` | — | Intentional Omission |
| `getApplySorobanSuccess/Failure` | — | Intentional Omission |
| `reset()` | — | Intentional Omission |
| `updateMinBalance()` | — | Intentional Omission |
| `isLive()` | — | Intentional Omission |

### SorobanTxBuilder (`loadgen_soroban.rs`)

Dedicated Soroban transaction builder — no direct stellar-core counterpart (logic is inlined in `TxGenerator.cpp`).

| Rust | Upstream Equivalent | Status |
|------|---------------------|--------|
| `SorobanTxBuilder::upload_wasm_tx()` | Part of `createUploadWasmTransaction()` | Full |
| `SorobanTxBuilder::create_contract_tx()` | Part of `createContractTransaction()` | Full |
| `SorobanTxBuilder::invoke_contract_tx()` | Part of `invokeSorobanLoadTransaction()` | Full |
| `SorobanTxBuilder::create_sac_tx()` | Part of `createSACTransaction()` | Full |
| `SorobanTxBuilder::invoke_sac_transfer_tx()` | Part of `invokeSACPayment()` | Full |
| `SorobanTxBuilder::invoke_batch_transfer_tx()` | Part of `invokeBatchTransfer()` | Full |
| `compute_contract_id()` | Inline in `TxGenerator.cpp` | Full |
| `contract_instance_key()` / `contract_code_key()` | Inline in `TxGenerator.cpp` | Full |

### ApplyLoad (`applyload.rs`)

Corresponds to: `ApplyLoad.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ApplyLoad()` constructor | `ApplyLoad::new()` | Full |
| `ApplyLoadMode` enum | `ApplyLoadMode` | Full |
| `closeLedger()` | `close_ledger()` | Full |
| `benchmark()` | `benchmark()` | Full |
| `findMaxSacTps()` | `find_max_sac_tps()` | Full |
| `successRate()` | `success_rate()` | Full |
| `getTxCountUtilization()` | `tx_count_utilization()` | Full |
| `getInstructionUtilization()` | `instruction_utilization()` | Full |
| `getTxSizeUtilization()` | `tx_size_utilization()` | Full |
| `getDiskReadByteUtilization()` | `disk_read_byte_utilization()` | Full |
| `getDiskWriteByteUtilization()` | `disk_write_byte_utilization()` | Full |
| `getDiskReadEntryUtilization()` | `disk_read_entry_utilization()` | Full |
| `getWriteEntryUtilization()` | `write_entry_utilization()` | Full |
| `getKeyForArchivedEntry()` | `key_for_archived_entry()` | Full |
| `calculateRequiredHotArchiveEntries()` | `calculate_required_hot_archive_entries()` | Full |
| `setup()` | `setup()` (private) | Full |
| `setupAccounts()` | `setup_accounts()` | Full |
| `setupLoadContract()` | `setup_load_contract()` | Full |
| `setupXLMContract()` | `setup_xlm_contract()` | Full |
| `setupBatchTransferContracts()` | `setup_batch_transfer_contracts()` | Full |
| `setupBucketList()` | `setup_bucket_list()` | Full |
| `benchmarkSacTps()` | `benchmark_sac_tps()` | Full |
| `generateSacPayments()` | `generate_sac_payments()` | Full |
| `calculateInstructionsPerTx()` | `calculate_instructions_per_tx()` | Full |
| `upgradeSettingsForMaxTPS()` | `upgrade_settings_for_max_tps()` | Full |
| `setupUpgradeContract()` | `setup_upgrade_contract()` | Partial (stub) |
| `upgradeSettings()` | `upgrade_settings()` | Partial (stub) |
| `applyConfigUpgrade()` | `apply_config_upgrade()` | Partial (stub) |
| `warmAccountCache()` | — | None |

### Herder / App additions

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Herder::sourceAccountPending()` | `Herder::source_account_pending()` | Full |
| `App::getExpectedLedgerCloseTime()` | `App::expected_ledger_close_time()` | Full |
| `App::loadAccountSequence()` | `App::load_account_sequence()` | Full |
| `App::sourceAccountPending()` | `App::source_account_pending()` | Full |
| `App::baseFee()` | `App::base_fee()` | Full |
| `App::currentLedgerSeq()` | `App::current_ledger_seq()` | Full |
| `App::ledgerManager()` | `App::ledger_manager()` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `LoopbackOverlayManager` / `ApplicationLoopbackOverlay` | Rust uses `LoopbackConnectionFactory` from henyey-overlay instead |
| Medida metrics integration / `metricsSummary()` | Rust uses internal tracking; no medida dependency |
| `setCurrentVirtualTime()` (both overloads) | Not needed — Rust async model handles time differently |
| `crankUntil(time_point)` / `crankUntil(system_time_point)` | Doesn't map to Rust async model; predicate-based `crank_until` covers all test needs |
| `stopOverlayTick()` | Overlay tick control managed by tokio runtime, not manual stop |
| `SOROBAN_UPGRADE_SETUP` / `SOROBAN_CREATE_UPGRADE` modes | Henyey uses direct `LedgerUpgrade` instead of config-upgrade contract |
| `PAY_PREGENERATED` mode | File-based replay mode not needed |
| `SOROBAN_INVOKE_APPLY_LOAD` mode | Internal ApplyLoad wiring mode |
| `getConfigUpgradeSetKey()` (LoadGenerator + TxGenerator) | Part of config-upgrade-contract approach not used in henyey |
| `invokeSorobanCreateUpgradeTransaction()` | Part of config-upgrade-contract approach |
| `getConfigUpgradeSetFromLoadConfig()` | Part of config-upgrade-contract approach |
| `copySorobanNetworkConfigToUpgradeConfig()` | Part of config-upgrade-contract approach |
| `SorobanUpgradeConfig` accessors | Part of config-upgrade-contract approach |
| `pregeneratedTxLoad()` / `createSorobanUpgradeSetupLoad()` | Factory methods for omitted modes |
| `getApplySorobanSuccess/Failure()` | Medida counter accessors; Rust tracks success internally |
| `reset()` / `updateMinBalance()` / `isLive()` (TxGenerator) | Internal housekeeping; Rust manages state differently |
| `getContractInstanceKeysForTesting()` / `getCodeKeyForTesting()` / `getContactOverheadBytesForTesting()` | Test-only accessors |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `getLoopbackConnection()` | Low | No direct loopback connection object exposure |
| `checkSorobanWasmSetup()` | Low | Validates Wasm deployment before invoke |
| `modeUploads()` | Low | Trivial predicate on GeneratedLoadConfig |
| `getStatus()` | Low | JSON status report for GeneratedLoadConfig |
| `createSorobanInvokeSetupLoad()` | Low | Static factory method |
| `invokeSorobanLoadTransactionV2()` | Low | V2 invoke with data-entry-count parameters |
| `warmAccountCache()` (ApplyLoad) | Low | BucketListDB cache warming |
| `setupUpgradeContract()` full impl | Medium | Currently a stub; blocked on config-upgrade wasm |
| `upgradeSettings()` full impl | Medium | Currently a stub; blocked on setupUpgradeContract |
| `applyConfigUpgrade()` full impl | Medium | Currently a stub; blocked on setupUpgradeContract |

## Architectural Differences

1. **Simulation model**
   - **stellar-core**: Single-process, VirtualClock-driven event loop for all nodes; `crankNode` / `crankAllNodes` advance individual timers.
   - **Rust**: Each app node runs in its own tokio task; lightweight `SimNode` mode uses synchronous ledger-sequence advancement. No shared VirtualClock.
   - **Rationale**: Rust async model with tokio handles concurrency differently; lightweight simulation layer provides fast deterministic tests.

2. **Loopback transport**
   - **stellar-core**: `LoopbackPeer` / `LoopbackPeerConnection` objects with direct method calls between peers.
   - **Rust**: `LoopbackConnectionFactory` from henyey-overlay provides in-memory channels; simulation manages link-level topology via `LoopbackNetwork`.
   - **Rationale**: Decouples transport from simulation; same `ConnectionFactory` trait used by both TCP and loopback.

3. **Soroban transaction building**
   - **stellar-core**: All Soroban tx construction is inline in `TxGenerator.cpp`.
   - **Rust**: Dedicated `SorobanTxBuilder` in `loadgen_soroban.rs` encapsulates all Soroban envelope construction, with `TxGenerator` delegating to it.
   - **Rationale**: Better separation of concerns; easier to test and extend.

4. **Config upgrades**
   - **stellar-core**: Deploys a special `write_bytes` contract to apply Soroban config upgrades via `setupUpgradeContract()` / `applyConfigUpgrade()`.
   - **Rust**: Uses direct `LedgerUpgrade` variants for config changes; the upgrade-contract path is stubbed out.
   - **Rationale**: Direct ledger upgrades are simpler and sufficient for henyey's use cases.

5. **Utilization histograms**
   - **stellar-core**: Uses `medida::Histogram` objects registered in a global metrics registry.
   - **Rust**: Simple `Histogram` struct backed by `Vec<u64>` with mean/count methods.
   - **Rationale**: No medida dependency; lightweight in-memory histograms suffice for benchmark reporting.

6. **Genesis bootstrapping**
   - **stellar-core**: Uses `TestApplication` / test utilities to create genesis state.
   - **Rust**: Standalone `initialize_genesis_ledger()` function constructs genesis ledger header, root account, and bucket list directly in SQLite.
   - **Rationale**: Self-contained genesis avoids dependency on external test utilities.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| CoreTests | 13 TEST_CASE / 13 SECTION | 8 `#[tokio::test]` (simulation.rs) | Core topology convergence, partition recovery, determinism |
| App simulation | (inline in CoreTests) | 15 `#[tokio::test]` (app_simulation.rs) | Single-node, pair, core3, core4, cycle4, load execution |
| Serious scenarios | (inline in CoreTests) | 2 `#[tokio::test]` (serious_simulation.rs) | 7-node fault schedule, deterministic replay |
| LoadGenerator | 9 TEST_CASE / 8 SECTION | 7 `#[test]` (loadgen.rs) | Determinism, config, seed padding, account derivation |
| SorobanTxBuilder | (inline in LoadGeneratorTests) | 7 `#[test]` (loadgen_soroban.rs) | Contract ID, SAC, wasm hash, SorobanTxBuilder roundtrips |
| ApplyLoad | No separate test file | 10 `#[test]` (applyload.rs) | Config defaults, histogram, key derivation, success rate |

### Test Gaps

- No integration test exercising full Soroban load generation through `generate_load()`
- No integration test for `ApplyLoad::benchmark()` or `find_max_sac_tps()` end-to-end
- Upstream LoadGeneratorTests has Soroban-specific sections not yet mirrored

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 113 |
| Gaps (None + Partial) | 10 |
| Intentional Omissions | 28 |
| **Parity** | **113 / (113 + 10) = 92%** |

Note: SorobanTxBuilder methods (8 items) are not counted separately — they are
the implementation of the TxGenerator methods already counted above. The 3
Partial items (`setupUpgradeContract`, `upgradeSettings`, `applyConfigUpgrade`)
are counted as gaps because they exist as stubs but are not functionally
complete. The 28 intentional omissions cover config-upgrade-contract approach
(henyey uses direct LedgerUpgrade), medida metrics, test-only accessors,
VirtualClock time manipulation, and modes not needed (pregenerated, upgrade).
