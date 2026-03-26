# stellar-core Parity Status

**Crate**: `henyey-simulation`
**Upstream**: `stellar-core/src/simulation/`
**Overall Parity**: 86%
**Last Updated**: 2026-03-25

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Simulation lifecycle | Full | Node add/start/stop/remove/restart flows are implemented |
| Connection management | Partial | Missing bulk-connect helper and loopback connection exposure |
| Crank and time advancement | Partial | Predicate-based crank flow exists; explicit time setters do not |
| Topology builders | Partial | Most builders exist; `separateAllHighQuality` is missing |
| Classic load generation | Partial | Core submission loop exists; parser/status helpers are absent |
| Soroban load generation | Partial | Upload/setup/invoke work, but setup validation helpers are missing |
| Transaction generation | Partial | Main classic and Soroban builders exist; V2/padded variants do not |
| ApplyLoad limit benchmarking | Partial | Core benchmark path works; full `execute`/find-limits flow is missing |
| ApplyLoad max SAC TPS | Partial | TPS search works; batch-transfer setup remains placeholder-only |
| Genesis bootstrapping | Full | Standalone genesis and test-account initialization are implemented |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Simulation.h` / `Simulation.cpp` | `src/lib.rs` | Core simulation harness and app-backed node lifecycle |
| `Topologies.h` / `Topologies.cpp` | `src/lib.rs` | Standard topology builders |
| `LoadGenerator.h` / `LoadGenerator.cpp` | `src/loadgen.rs` | Load generation orchestration and account pool handling |
| `TxGenerator.h` / `TxGenerator.cpp` | `src/loadgen.rs`, `src/loadgen_soroban.rs` | Classic and Soroban transaction builders |
| `ApplyLoad.h` / `ApplyLoad.cpp` | `src/applyload.rs` | Direct ledger-apply benchmark harness |

## Component Mapping

### Simulation (`src/lib.rs`)

Corresponds to: `Simulation.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Simulation()` | `Simulation::new()` / `Simulation::with_network()` | Full |
| `~Simulation()` | `stop_all_nodes()` | Full |
| `setCurrentVirtualTime(time_point)` | — | Intentional Omission |
| `setCurrentVirtualTime(system_time_point)` | — | Intentional Omission |
| `addNode()` | `add_node()` / `add_app_node()` | Full |
| `getNode()` | `app()` | Full |
| `getNodes()` | `apps()` | Full |
| `getNodeIDs()` | `node_ids()` / `app_node_ids()` | Full |
| `addPendingConnection()` | `add_pending_connection()` | Full |
| `fullyConnectAllPending()` | — | None |
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
| `newConfig()` | internal `build_app_from_spec()` only | Partial |
| `stopOverlayTick()` | — | Intentional Omission |
| `getExpectedLedgerCloseTime()` | `expected_ledger_close_time()` | Full |
| `isSetUpForSorobanUpgrade()` | `is_setup_for_soroban_upgrade()` | Full |
| `markReadyForSorobanUpgrade()` | `mark_ready_for_soroban_upgrade()` | Full |
| `Mode` | `SimulationMode` | Full |

### Topologies (`src/lib.rs`)

Corresponds to: `Topologies.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `pair()` | `Topologies::pair()` | Full |
| `cycle4()` | `Topologies::cycle4()` | Full |
| `core()` | `Topologies::core()` / `Topologies::core3()` | Full |
| `cycle()` | `Topologies::cycle()` | Full |
| `branchedcycle()` | `Topologies::branchedcycle()` | Full |
| `separate(..., numWatchers=0)` | `Topologies::separate()` | Full |
| `separate(..., numWatchers)` | `Topologies::separate_with_watchers()` | Full |
| `separateAllHighQuality()` | — | None |
| `hierarchicalQuorum()` | `Topologies::hierarchical_quorum()` | Full |
| `hierarchicalQuorumSimplified()` | `Topologies::hierarchical_quorum_simplified()` | Full |
| `customA()` | `Topologies::custom_a()` | Full |
| `asymmetric()` | `Topologies::asymmetric()` | Full |

### LoadGenMode (`src/loadgen.rs`)

Corresponds to: `LoadGenMode` in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PAY` | `LoadGenMode::Pay` | Full |
| `SOROBAN_UPLOAD` | `LoadGenMode::SorobanUpload` | Full |
| `SOROBAN_INVOKE_SETUP` | `LoadGenMode::SorobanInvokeSetup` | Full |
| `SOROBAN_INVOKE` | `LoadGenMode::SorobanInvoke` | Full |
| `MIXED_CLASSIC_SOROBAN` | `LoadGenMode::MixedClassicSoroban` | Full |
| `SOROBAN_UPGRADE_SETUP` | — | Intentional Omission |
| `SOROBAN_CREATE_UPGRADE` | — | Intentional Omission |
| `PAY_PREGENERATED` | — | Intentional Omission |
| `SOROBAN_INVOKE_APPLY_LOAD` | — | Intentional Omission |

### GeneratedLoadConfig (`src/loadgen.rs`)

Corresponds to: `GeneratedLoadConfig` in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `txLoad()` | `GeneratedLoadConfig::tx_load()` | Full |
| `getMutSorobanConfig()` | `n_instances` / `n_wasms` fields | Full |
| `getSorobanConfig()` | `n_instances` / `n_wasms` fields | Full |
| `getMutMixClassicSorobanConfig()` | mix weight fields | Full |
| `getMixClassicSorobanConfig()` | mix weight fields | Full |
| `getMinSorobanPercentSuccess()` | `min_soroban_percent_success` | Full |
| `setMinSorobanPercentSuccess()` | `min_soroban_percent_success` | Full |
| `isSoroban()` | `LoadGenMode::is_soroban()` | Full |
| `isSorobanSetup()` | `LoadGenMode::is_soroban_setup()` | Full |
| `isLoad()` | `LoadGenMode::is_load()` | Full |
| `modeInvokes()` | `LoadGenMode::mode_invokes()` | Full |
| `modeSetsUpInvoke()` | `LoadGenMode::mode_sets_up_invoke()` | Full |
| `isDone()` | `GeneratedLoadConfig::is_done()` | Full |
| `areTxsRemaining()` | `GeneratedLoadConfig::are_txs_remaining()` | Full |
| `spikeInterval` / `spikeSize` | `spike_interval` / `spike_size` | Full |
| `maxGeneratedFeeRate` | `max_fee_rate` | Full |
| `skipLowFeeTxs` | `skip_low_fee_txs` | Full |
| `modeUploads()` | — | None |
| `getStatus()` | — | None |
| `createSorobanInvokeSetupLoad()` | — | None |
| `pregeneratedTxLoad()` | — | Intentional Omission |
| `createSorobanUpgradeSetupLoad()` | — | Intentional Omission |
| `copySorobanNetworkConfigToUpgradeConfig()` | — | Intentional Omission |
| `getMutSorobanUpgradeConfig()` / `getSorobanUpgradeConfig()` | — | Intentional Omission |

### LoadGenerator (`src/loadgen.rs`)

Corresponds to: `LoadGenerator` in `LoadGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LoadGenerator()` | `LoadGenerator::new()` | Full |
| `getMode()` | — | None |
| `isDone()` | `LoadGenerator::is_done()` | Full |
| `checkSorobanWasmSetup()` | — | None |
| `checkMinimumSorobanSuccess()` | `check_minimum_soroban_success()` | Full |
| `generateLoad()` | `generate_load()` | Full |
| `checkAccountSynced()` | `check_account_synced()` | Full |
| `checkSorobanStateSynced()` | `check_soroban_state_synced()` | Full |
| `stop()` | `stop()` | Full |
| `getNextAvailableAccount()` | `get_next_available_account()` | Full |
| `cleanupAccounts()` | `cleanup_accounts()` | Full |
| `submitTx()` | `submit_tx()` | Full |
| `resetSorobanState()` | `reset_soroban_state()` | Full |
| Soroban mode dispatch | mode-aware `generate_tx()` | Full |
| Rate limiting / spike logic | `get_tx_per_step()` | Full |
| `getConfigUpgradeSetKey()` | — | Intentional Omission |
| `getContractInstanceKeysForTesting()` | — | Intentional Omission |
| `getCodeKeyForTesting()` | — | Intentional Omission |
| `getContactOverheadBytesForTesting()` | — | Intentional Omission |

### TxGenerator (`src/loadgen.rs`, `src/loadgen_soroban.rs`)

Corresponds to: `TxGenerator.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `footprintSize()` | — | None |
| `TxGenerator()` | `TxGenerator::new()` | Full |
| `ROOT_ACCOUNT_ID` handling | `ROOT_ACCOUNT_ID` sentinel semantics | Full |
| `SAC_TX_INSTRUCTIONS` / `BATCH_TRANSFER_TX_INSTRUCTIONS` | benchmark constants in `applyload.rs` | Full |
| `loadAccount(TestAccount&)` / `loadAccount(TestAccountPtr)` | `load_account()` | Full |
| `findAccount()` | `find_account()` | Full |
| `createAccounts()` | `create_accounts()` | Full |
| `createTransactionFramePtr(from, ops, fee)` | `create_transaction_frame()` | Full |
| `createTransactionFramePtr(..., byteCount)` | — | None |
| `paymentTransaction()` | `payment_transaction()` | Full |
| `generateFee()` | `generate_fee()` | Full |
| `pickAccountPair()` | `pick_account_pair()` | Full |
| Deterministic account derivation | `deterministic_seed()` / `TestAccount::from_name()` | Full |
| `getAccounts()` | `accounts()` | Full |
| `getAccount()` | `get_account()` | Full |
| `addAccount()` | — | None |
| `createUploadWasmTransaction()` | `create_upload_wasm_transaction()` | Full |
| `createContractTransaction()` | `create_contract_transaction()` | Full |
| `createSACTransaction()` | `create_sac_transaction()` | Full |
| `invokeSorobanLoadTransaction()` | `invoke_soroban_load_transaction()` | Full |
| `invokeSorobanLoadTransactionV2()` | — | None |
| `invokeSACPayment()` | `invoke_sac_payment()` | Full |
| `invokeBatchTransfer()` | `invoke_batch_transfer()` | Full |
| `sorobanRandomWasmTransaction()` | `soroban_random_wasm_transaction()` | Full |
| `invokeSorobanCreateUpgradeTransaction()` | — | Intentional Omission |
| `getConfigUpgradeSetKey()` | — | Intentional Omission |
| `getConfigUpgradeSetFromLoadConfig()` | — | Intentional Omission |
| `getApplySorobanSuccess()` / `getApplySorobanFailure()` | internal counters only | Intentional Omission |
| `reset()` / `updateMinBalance()` / `isLive()` | Rust manages state differently | Intentional Omission |

### ApplyLoad (`src/applyload.rs`)

Corresponds to: `ApplyLoad.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ApplyLoadMode::LIMIT_BASED` | `ApplyLoadMode::LimitBased` | Full |
| `ApplyLoadMode::MAX_SAC_TPS` | `ApplyLoadMode::MaxSacTps` | Full |
| `ApplyLoadMode::FIND_LIMITS_FOR_MODEL_TX` | — | None |
| `ApplyLoad()` | `ApplyLoad::new()` | Full |
| `execute()` | — | None |
| `successRate()` | `success_rate()` | Full |
| `closeLedger()` | `close_ledger()` | Full |
| `getTxCountUtilization()` | `tx_count_utilization()` | Full |
| `getInstructionUtilization()` | `instruction_utilization()` | Full |
| `getTxSizeUtilization()` | `tx_size_utilization()` | Full |
| `getDiskReadByteUtilization()` | `disk_read_byte_utilization()` | Full |
| `getDiskWriteByteUtilization()` | `disk_write_byte_utilization()` | Full |
| `getDiskReadEntryUtilization()` | `disk_read_entry_utilization()` | Full |
| `getWriteEntryUtilization()` | `write_entry_utilization()` | Full |
| `getKeyForArchivedEntry()` | `key_for_archived_entry()` | Full |
| `calculateRequiredHotArchiveEntries()` | `calculate_required_hot_archive_entries()` | Full |
| `setup()` | `setup()` | Full |
| `setupAccounts()` | `setup_accounts()` | Full |
| `setupUpgradeContract()` | stub only | Partial |
| `setupLoadContract()` | `setup_load_contract()` | Full |
| `setupXLMContract()` | `setup_xlm_contract()` | Full |
| `setupBatchTransferContracts()` | placeholder instances only | Partial |
| `setupBucketList()` | `setup_bucket_list()` | Full |
| `benchmarkLimits()` | folded into `benchmark()` only | Partial |
| `findMaxSacTps()` | `find_max_sac_tps()` | Full |
| `benchmarkSacTps()` | `benchmark_sac_tps()` | Full |
| `generateSacPayments()` | `generate_sac_payments()` | Full |
| `calculateInstructionsPerTx()` | `calculate_instructions_per_tx()` | Full |
| `warmAccountCache()` | `warm_account_cache()` | Full |
| `upgradeSettings()` | `upgrade_settings()` | Full |
| `upgradeSettingsForMaxTPS()` | `upgrade_settings_for_max_tps()` | Full |
| `applyConfigUpgrade()` | direct-injection path only | Partial |
| `TARGET_CLOSE_TIME_STEP_MS` | — | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `setCurrentVirtualTime()` overloads | Tokio-based async simulation has no shared `VirtualClock` to mutate |
| `crankUntil(time_point)` / `crankUntil(system_time_point)` | Predicate-based `crank_until()` covers current test usage |
| `metricsSummary()` and medida-backed counters | henyey uses lightweight internal reporting, not medida |
| `stopOverlayTick()` | Overlay retry behavior is owned by runtime tasks, not manual tick control |
| `LoopbackOverlayManager` / `ApplicationLoopbackOverlay` | Loopback transport lives in `henyey-overlay` rather than simulation-local subclasses |
| `SOROBAN_UPGRADE_SETUP` / `SOROBAN_CREATE_UPGRADE` modes | henyey applies config changes directly instead of via upgrade contract |
| `PAY_PREGENERATED` and `pregeneratedTxLoad()` | File-driven replay mode is not used by this crate |
| `SOROBAN_INVOKE_APPLY_LOAD` mode | Apply-load benchmarking is wired directly through `ApplyLoad` |
| `copySorobanNetworkConfigToUpgradeConfig()` and Soroban-upgrade accessors | Upgrade-contract workflow is intentionally skipped |
| `LoadGenerator::getConfigUpgradeSetKey()` | No config-upgrade contract path in Rust |
| `TxGenerator::invokeSorobanCreateUpgradeTransaction()` | No config-upgrade contract path in Rust |
| `TxGenerator::getConfigUpgradeSetKey()` / `getConfigUpgradeSetFromLoadConfig()` | No config-upgrade contract path in Rust |
| `getApplySorobanSuccess()` / `getApplySorobanFailure()` | Rust tracks apply success internally inside `ApplyLoad` |
| `reset()` / `updateMinBalance()` / `isLive()` | Rust handles account and ledger state with different internal bookkeeping |
| Test-only accessors on `LoadGenerator` | Not exposed in the Rust API surface |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `fullyConnectAllPending()` | Medium | No helper to bulk-connect every pending edge before startup |
| `getLoopbackConnection()` | Low | No direct loopback connection object exposure for tests |
| `newConfig()` | Low | Config generation exists only as an internal helper |
| `separateAllHighQuality()` | Low | One topology builder is still missing |
| `LoadGenerator::getMode()` | Low | No string-to-mode parser helper |
| `checkSorobanWasmSetup()` | Medium | Missing guard that uploaded Wasm is fully visible before invoke load |
| `GeneratedLoadConfig::modeUploads()` / `getStatus()` / `createSorobanInvokeSetupLoad()` | Low | Small convenience helpers are absent |
| `footprintSize()`, padded `createTransactionFramePtr(...)`, `addAccount()`, `invokeSorobanLoadTransactionV2()` | Medium | Secondary `TxGenerator` utilities are not implemented |
| `ApplyLoad::execute()` and `FIND_LIMITS_FOR_MODEL_TX` flow | High | No top-level dispatcher or model-tx limit-search mode |
| `setupUpgradeContract()` / `applyConfigUpgrade()` full parity | Medium | Direct config injection works, but upgrade-contract behavior is not mirrored |
| `setupBatchTransferContracts()` full deployment | Medium | Batch-transfer mode uses placeholders instead of real contract deployment |
| `TARGET_CLOSE_TIME_STEP_MS` | Low | Public constant from upstream is not exposed |

## Architectural Differences

1. **Simulation clocking**
   - **stellar-core**: One `VirtualClock` advances all nodes inside a single process.
   - **Rust**: App-backed nodes run as tokio tasks; lightweight nodes use explicit ledger-sequence stepping.
   - **Rationale**: The Rust runtime model is async-first and does not share upstream's central clock abstraction.

2. **Loopback transport**
   - **stellar-core**: Simulation owns loopback overlay subclasses and connection objects.
   - **Rust**: `henyey-overlay` provides reusable loopback connection factories and the simulation crate owns only topology state.
   - **Rationale**: Transport stays shared between simulation and non-simulation code paths.

3. **Soroban builders**
   - **stellar-core**: Most Soroban transaction construction lives inline in `TxGenerator.cpp`.
   - **Rust**: `src/loadgen_soroban.rs` factors Soroban envelope construction into a dedicated builder.
   - **Rationale**: Keeps XDR-heavy construction logic isolated and easier to test.

4. **Config upgrades in ApplyLoad**
   - **stellar-core**: Uses an upgrade-contract workflow to materialize a `ConfigUpgradeSet` and apply it through contract state.
   - **Rust**: Injects synthetic config data directly and closes a ledger with `LedgerUpgrade::Config`.
   - **Rationale**: Benchmarking transaction application does not require reproducing the upgrade-contract plumbing.

5. **Benchmark coverage**
   - **stellar-core**: `ApplyLoad::execute()` dispatches multiple benchmark families, including model-transaction limit search.
   - **Rust**: Exposes the implemented flows directly (`benchmark()`, `find_max_sac_tps()`) and leaves the missing dispatcher path out.
   - **Rationale**: The implemented benchmarks are usable now, while the incomplete dispatcher path remains explicit in gaps.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Core simulation topologies | 13 `TEST_CASE` / 13 `SECTION` in `CoreTests.cpp` | 8 `#[tokio::test]` in `tests/simulation.rs` | Basic convergence, partitions, determinism, topology construction |
| App-backed simulation | covered inside `CoreTests.cpp` | 15 `#[tokio::test]` in `tests/app_simulation.rs` | Real `App` startup, connectivity, restarts, manual close |
| Long-running fault scenarios | covered inside `CoreTests.cpp` | 2 `#[tokio::test]` in `tests/serious_simulation.rs` | 7-node fault schedule and deterministic replay |
| LoadGenerator / TxGenerator | 9 `TEST_CASE` / 8 `SECTION` in `LoadGeneratorTests.cpp` | 7 `#[test]` in `src/loadgen.rs` | Mostly deterministic helpers, not end-to-end load submission |
| Soroban tx building | mixed into `LoadGeneratorTests.cpp` | 7 `#[test]` in `src/loadgen_soroban.rs` | WASM, contract ID, helper construction |
| ApplyLoad | 3 `TEST_CASE` in `LoadGeneratorTests.cpp` (`apply load`, `find max limits`, `MAX_SAC_TPS`) | 9 `#[test]` in `src/applyload.rs` | Unit coverage only; no full benchmark integration |

### Test Gaps

- No Rust integration test drives `LoadGenerator::generate_load()` through full classic and Soroban runs against a live app.
- No Rust equivalent covers upstream's `pregenerated transactions`, invalid-parameter, or explicit `stop loadgen` sections.
- No Rust test mirrors upstream's `find max limits for model tx` ApplyLoad coverage because that mode is not implemented.
- Batch-transfer `MAX_SAC_TPS` paths are not exercised end-to-end in Rust.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 117 |
| Gaps (None + Partial) | 19 |
| Intentional Omissions | 31 |
| **Parity** | **117 / (117 + 19) = 86%** |
