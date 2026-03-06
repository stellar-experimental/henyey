# stellar-core Parity Status

**Crate**: `henyey-ledger`
**Upstream**: `stellar-core/src/ledger/`
**Overall Parity**: 65%
**Last Updated**: 2026-03-06

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Ledger Manager (close pipeline) | Full | Core close, genesis, initialize |
| Ledger Header Utilities | Full | Hash, skip list, chain verify |
| Change Tracking (LedgerDelta) | Full | Create/update/delete coalescing |
| Ledger Close Data | Full | Classic + generalized tx sets |
| Ledger Snapshots | Full | Immutable point-in-time views |
| Transaction Execution | Full | Fee charging, Soroban, operations |
| Network Config Loading | Full | All config settings from ledger |
| Fee / Reserve Calculations | Full | Liabilities, sponsorship |
| Offer Sorting / Comparison | Full | OfferDescriptor, AssetPair |
| In-Memory Soroban State | Full | Contract data/code with TTL |
| Config Upgrade Handling | Full | Validation and application |
| LedgerTxn Nested Transactions | Partial | Savepoints cover operation rollback |
| Parallel Apply / Threading | Partial | Parallel cluster execution via tokio; no ApplyState phase machine |
| Soroban Metrics | None | No metrics collection |
| Shared Module Cache | Partial | Per-TX caching via `PersistentModuleCache` in `henyey-tx`; no global shared cache |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `LedgerManager.h` / `LedgerManagerImpl.h` / `LedgerManagerImpl.cpp` | `manager.rs` | Core ledger manager |
| `LedgerHeaderUtils.h` / `LedgerHeaderUtils.cpp` | `header.rs` | Header hash, skip list |
| `LedgerTxn.h` / `LedgerTxn.cpp` | `delta.rs` | Change tracking replaces LedgerTxn |
| `LedgerTxnImpl.h` | `delta.rs` | Impl details not needed |
| `LedgerTxnEntry.h` / `LedgerTxnEntry.cpp` | `delta.rs` | Entry handles simplified |
| `LedgerTxnHeader.h` / `LedgerTxnHeader.cpp` | `delta.rs`, `snapshot.rs` | Header access via snapshot |
| `LedgerStateSnapshot.h` / `LedgerStateSnapshot.cpp` | `snapshot.rs` | Read-only snapshots |
| `LedgerCloseMetaFrame.h` / `LedgerCloseMetaFrame.cpp` | `execution/meta.rs` | Meta generation |
| `NetworkConfig.h` / `NetworkConfig.cpp` | `execution/config.rs`, `config_upgrade.rs` | Config loading and upgrades |
| `InMemorySorobanState.h` / `InMemorySorobanState.cpp` | `soroban_state.rs` | In-memory Soroban cache |
| `LedgerTypeUtils.h` / `LedgerTypeUtils.cpp` | `delta.rs`, `soroban_state.rs` | Entry type utilities |
| `LedgerHashUtils.h` | `delta.rs` | Key hashing via XDR serialization |
| `InternalLedgerEntry.h` / `InternalLedgerEntry.cpp` | `delta.rs` | Simplified to LedgerEntry/LedgerKey |
| `LedgerRange.h` / `LedgerRange.cpp` | `close.rs` | Range utilities inline |
| `TrustLineWrapper.h` / `TrustLineWrapper.cpp` | `lib.rs` (trustlines module) | Balance constraint subset |
| `SorobanMetrics.h` / `SorobanMetrics.cpp` | -- | Not implemented |
| `SharedModuleCacheCompiler.h` / `SharedModuleCacheCompiler.cpp` | -- | Not implemented |
| `LedgerTxnOfferSQL.cpp` | -- | SQL backend not implemented |

## Component Mapping

### manager.rs (`manager.rs`)

Corresponds to: `LedgerManager.h`, `LedgerManagerImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerManager::create()` | `LedgerManager::new()` | Full |
| `LedgerManager::genesisLedger()` | `create_genesis_header()` (in `header.rs`) | Full |
| `LedgerManager::getState()` | `is_initialized()` | Full |
| `LedgerManager::getLastClosedLedgerHeader()` | `current_header()` | Full |
| `LedgerManager::getLastClosedLedgerNum()` | `current_ledger_seq()` | Full |
| `LedgerManager::getLastMinBalance()` | Via `reserves::minimum_balance()` | Full |
| `LedgerManager::getLastReserve()` | Via `snapshot.base_reserve()` | Full |
| `LedgerManager::getLastTxFee()` | Via `snapshot.base_fee()` | Full |
| `LedgerManager::getLastMaxTxSetSize()` | Via header field | Full |
| `LedgerManager::getLastMaxTxSetSizeOps()` | Via header field | Full |
| `LedgerManager::startNewLedger()` | `initialize()` with genesis | Full |
| `LedgerManager::loadLastKnownLedger()` | `initialize()` from buckets | Full |
| `LedgerManager::applyLedger()` | `close_ledger()` | Full |
| `LedgerManager::setLastClosedLedger()` | `initialize()` | Full |
| `LedgerManager::valueExternalized()` | Not applicable (no herder) | Full |
| `LedgerManager::getLastClosedSorobanNetworkConfig()` | `soroban_network_info()` | Full |
| `LedgerManager::hasLastClosedSorobanNetworkConfig()` | Via config check | Full |
| `LedgerManager::maxLedgerResources()` | Via config settings | Full |
| `LedgerManager::maxSorobanTransactionResources()` | Via config settings | Full |
| `LedgerManager::advanceLedgerStateAndPublish()` | Part of `close_ledger()` | Full |
| `LedgerManager::handleUpgradeAffectingSorobanInMemoryStateSize()` | `recompute_contract_code_sizes()` | Full |
| `LedgerManager::manuallyAdvanceLedgerHeader()` | `advance_ledger_seq()` on soroban state | Full |
| `LedgerManagerImpl::processFeesSeqNums()` | `charge_fees()` in execution | Full |
| `LedgerManagerImpl::applyTransactions()` | `run_transactions_on_executor()` | Full |
| `LedgerManagerImpl::sealLedgerTxnAndStoreInBucketsAndDB()` | Part of `close_ledger()` | Full |
| `LedgerManagerImpl::storePersistentStateAndLedgerHeaderInDB()` | Part of `close_ledger()` | Full |
| `LedgerManagerImpl::applyParallelPhase()` | `execute_soroban_parallel_phase()` | Full |
| `LedgerManagerImpl::applySequentialPhase()` | Sequential in `close_ledger()` | Full |
| `LedgerManager::getExpectedLedgerCloseTime()` | Not implemented | None |
| `LedgerManager::secondsSinceLastLedgerClose()` | Not implemented | None |
| `LedgerManager::syncMetrics()` | Not implemented | None |
| `LedgerManager::getDatabase()` | Not applicable (no SQL) | None |
| `LedgerManager::startCatchup()` | Not applicable (handled by app) | None |
| `LedgerManager::getSorobanMetrics()` | Not implemented | None |
| `LedgerManager::getModuleCache()` | Not implemented | None |
| `LedgerManager::isApplying()` | Not applicable (single-threaded) | None |
| `LedgerManager::markApplyStateReset()` | Not applicable (single-threaded) | None |
| `LedgerManagerImpl::setupLedgerCloseMetaStream()` | Not implemented | None |
| `LedgerManagerImpl::applySorobanStages()` | `execute_soroban_parallel_phase()` | Full |
| `LedgerManagerImpl::applySorobanStageClustersInParallel()` | `execute_stage_clusters()` (tokio parallel) | Full |
| `LedgerManagerImpl::applyThread()` | `execute_single_cluster()` (per-cluster executor) | Full |
| `LedgerManagerImpl::prefetchTransactionData()` | Not implemented | None |
| `LedgerManagerImpl::ApplyState` (phase machine) | Not implemented | None |

### header.rs (`header.rs`)

Corresponds to: `LedgerHeaderUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerHeaderUtils::isValid()` | `verify_header_chain()` | Full |
| `LedgerHeaderUtils::decodeFromData()` | Via XDR deserialization | Full |
| `LedgerHeaderUtils::getFlags()` | Via direct field access | Full |
| Header hash computation | `compute_header_hash()` | Full |
| Skip list computation | `calculate_skip_values()` | Full |
| Next header creation | `create_next_header()` | Full |
| Protocol version utilities | `protocol_version()`, `is_before_protocol_version()` | Full |
| Close time extraction | `close_time()` | Full |
| `LedgerHeaderUtils::storeInDatabase()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::loadByHash()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::loadBySequence()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::loadMaxLedgerSeq()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::deleteOldEntries()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::copyToStream()` | Not applicable (no SQL) | None |
| `LedgerHeaderUtils::dropAll()` | Not applicable (no SQL) | None |
| `LedgerManager::ledgerAbbrev()` | Not implemented (logging) | None |

### delta.rs (`delta.rs`)

Corresponds to: `LedgerTxn.h` (change tracking subset)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerTxn::commit()` | `LedgerDelta::merge()` | Full |
| `LedgerTxn::rollback()` | `LedgerDelta::rollback_to_savepoint()` | Full |
| `LedgerTxn::create()` | `LedgerDelta::create_entry()` | Full |
| `LedgerTxn::erase()` | `LedgerDelta::delete_entry()` | Full |
| `LedgerTxn::load()` | Via `SnapshotHandle::get_entry()` | Full |
| `LedgerTxn::loadHeader()` | Via `SnapshotHandle::header()` | Full |
| `LedgerTxn::getChanges()` | `LedgerDelta::to_ledger_entry_changes()` | Full |
| `LedgerTxn::getAllEntries()` | `LedgerDelta::categorize_changes()` | Full |
| `LedgerTxn::unsealHeader()` | Direct header mutation | Full |
| `LedgerTxnDelta` (entry+header diff) | `LedgerDelta` | Full |
| `EntryChange` coalescing | `EntryChange` with coalescing | Full |
| `entry_to_key()` | `entry_to_key()` | Full |
| `key_to_bytes()` | `key_to_bytes()` | Full |

### close.rs (`close.rs`)

Corresponds to: `LedgerCloseData` (in upstream `herder/` but used by `LedgerManager`), `LedgerCloseMetaFrame.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerCloseData` construction | `LedgerCloseData::new()` | Full |
| Transaction set hashing | `TransactionSetVariant::hash()` | Full |
| Transaction ordering | `sorted_transactions()` | Full |
| Generalized tx set phases | `SorobanPhaseStructure` | Full |
| Per-component base fee | Fee extraction per component | Full |
| `LedgerCloseResult` | `LedgerCloseResult` | Full |
| `LedgerCloseStats` | `LedgerCloseStats` | Full |
| `UpgradeContext` | `UpgradeContext` | Full |

### snapshot.rs (`snapshot.rs`)

Corresponds to: `LedgerStateSnapshot.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerSnapshot` construction | `LedgerSnapshot::new()` | Full |
| `BucketSnapshotState` | `SnapshotHandle` with bucket lookup | Full |
| `LedgerTxnReadOnly` | `SnapshotHandle` (read-only by design) | Full |
| `LedgerEntryWrapper` | Direct `LedgerEntry` access | Full |
| `LedgerHeaderWrapper` | Direct `LedgerHeader` access | Full |
| `SnapshotBuilder` | `SnapshotBuilder` | Full |
| Entry lookup | `SnapshotHandle::get_entry()` | Full |
| Account lookup | `SnapshotHandle::get_account()` | Full |
| `CompleteConstLedgerState` | Not implemented as separate type | Partial |

### execution (`execution/mod.rs`, `execution/config.rs`, `execution/meta.rs`, `execution/result_mapping.rs`, `execution/signatures.rs`, `execution/tx_set.rs`)

Corresponds to: `LedgerManagerImpl.h` (transaction execution), `NetworkConfig.h`, `LedgerCloseMetaFrame.h`, `TransactionFrame.h` (apply phases)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `processFeesSeqNums()` | Fee charging in `run_transactions_on_executor()` | Full |
| `applyTransactions()` | `run_transactions_on_executor()` | Full |
| `TransactionFrame::commonPreApply()` | `pre_apply()` (private) | Full |
| `TransactionFrame::preParallelApply()` | `pre_apply()` (private) | Full |
| `TransactionFrame::parallelApply()` | `apply_body()` (private) | Full |
| `SorobanNetworkConfig::loadFromLedger()` | `load_soroban_network_info()` | Full |
| `SorobanNetworkConfig` field getters | `SorobanNetworkInfo` struct fields | Full |
| `SorobanNetworkConfig::maybeSnapshotSorobanStateSize()` | `compute_state_size_window_entry()` | Full |
| `SorobanNetworkConfig::updateEvictionIterator()` | Via delta updates | Full |
| `SorobanNetworkConfig::isValidConfigSettingEntry()` | `ConfigUpgradeSetFrame::validate()` | Full |
| `SorobanNetworkConfig::rustBridgeFeeConfiguration()` | `build_fee_config()` | Full |
| `SorobanNetworkConfig::rustBridgeRentFeeConfiguration()` | `build_rent_fee_config()` | Full |
| `execute_soroban_parallel_phase()` | `execute_soroban_parallel_phase()` | Full |
| Soroban resource fee computation | Via soroban-env-host | Full |
| Rent fee computation | Via soroban-env-host | Full |
| Footprint-based entry loading | `load_soroban_footprint()` | Full |
| TTL entry handling | TTL handling in execution | Full |
| Classic event generation | `prepend_fee_event()` | Full |
| `LedgerCloseMetaFrame` construction | Meta building in `execution/meta.rs` | Full |
| `LedgerCloseMetaFrame::populateTxSet()` | Part of close meta generation | Full |
| `LedgerCloseMetaFrame::populateEvictedEntries()` | Eviction meta in close pipeline | Full |
| `SorobanNetworkConfig::createLedgerEntriesForV20()` | Not implemented | None |
| `SorobanNetworkConfig::createCostTypesForV21()` | Not implemented | None |
| `SorobanNetworkConfig::createCostTypesForV22()` | Not implemented | None |
| `SorobanNetworkConfig::createCostTypesForV25()` | Not implemented | None |
| `SorobanNetworkConfig::createAndUpdateLedgerEntriesForV23()` | Not implemented | None |

### config_upgrade.rs (`config_upgrade.rs`)

Corresponds to: `NetworkConfig.h` (upgrade subset)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SorobanNetworkConfig::isValidConfigSettingEntry()` | `ConfigUpgradeSetFrame::validate()` | Full |
| `SorobanNetworkConfig::isNonUpgradeableConfigSettingEntry()` | `is_non_upgradeable()` | Full |
| `MinimumSorobanNetworkConfig` constants | `min_config` module | Full |
| `MaximumSorobanNetworkConfig` constants | `max_config` module | Full |
| Config upgrade loading from CONTRACT_DATA | `ConfigUpgradeSetFrame::load()` | Full |
| Config upgrade application | `ConfigUpgradeSetFrame::apply()` | Full |

### offer.rs (`offer.rs`)

Corresponds to: `LedgerTxn.h` (offer comparison subset)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `isBetterOffer()` | `is_better_offer()` | Full |
| `OfferDescriptor` | `OfferDescriptor` | Full |
| `IsBetterOfferComparator` | `Ord` impl on `OfferDescriptor` | Full |
| `AssetPair` | `AssetPair` | Full |
| `AssetPairHash` | `Hash` impl on `AssetPair` | Full |

### soroban_state.rs (`soroban_state.rs`)

Corresponds to: `InMemorySorobanState.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `InMemorySorobanState::isInMemoryType()` | `is_soroban_entry()` / `is_in_memory_type()` | Full |
| `InMemorySorobanState::get()` | `InMemorySorobanState::get()` | Full |
| `InMemorySorobanState::hasTTL()` | `InMemorySorobanState::has_ttl()` | Full |
| `InMemorySorobanState::isEmpty()` | `InMemorySorobanState::is_empty()` | Full |
| `InMemorySorobanState::getSize()` | `InMemorySorobanState::total_size()` | Full |
| `InMemorySorobanState::getLedgerSeq()` | `InMemorySorobanState::ledger_seq()` | Full |
| `InMemorySorobanState::updateState()` | `InMemorySorobanState::update_state()` | Full |
| `InMemorySorobanState::initializeStateFromSnapshot()` | Initialization in `manager.rs` | Full |
| `InMemorySorobanState::manuallyAdvanceLedgerHeader()` | `advance_ledger_seq()` | Full |
| `InMemorySorobanState::recomputeContractCodeSize()` | `recompute_code_sizes()` | Full |
| `ContractDataMapEntryT` | `ContractDataMapEntry` | Full |
| `ContractCodeMapEntryT` | `ContractCodeMapEntry` | Full |
| `TTLData` | `TtlData` | Full |
| `InternalContractDataMapEntry` | HashMap-based storage | Full |
| `SharedSorobanState` (thread-safe wrapper) | `SharedSorobanState` | Full |

### lib.rs -- fees module (`lib.rs`)

Corresponds to: Fee/reserve logic in `LedgerManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getLastMinBalance()` | `reserves::minimum_balance()` | Full |
| Fee calculation | `fees::calculate_fee()` | Full |
| Available balance for fees | `fees::available_balance()` | Full |
| Selling liabilities | `reserves::selling_liabilities()` | Full |
| Buying liabilities | `reserves::buying_liabilities()` | Full |
| Available to send | `reserves::available_to_send()` | Full |
| Available to receive | `reserves::available_to_receive()` | Full |
| Sub-entry affordability | `reserves::can_add_sub_entry()` | Full |

### lib.rs -- trustlines module (`lib.rs`)

Corresponds to: `TrustLineWrapper.h` (balance constraint subset)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `TrustLineWrapper::getBalance()` | Direct field access | Full |
| `TrustLineWrapper::addBalance()` | Via tx crate | Full |
| `TrustLineWrapper::getBuyingLiabilities()` | `trustlines::buying_liabilities()` | Full |
| `TrustLineWrapper::getSellingLiabilities()` | `trustlines::selling_liabilities()` | Full |
| `TrustLineWrapper::getAvailableBalance()` | `trustlines::available_to_send()` | Full |
| `TrustLineWrapper::getMaxAmountReceive()` | `trustlines::available_to_receive()` | Full |
| `TrustLineWrapper::isAuthorized()` | Via flag checks in tx crate | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `LedgerTxnRoot` / SQL-based entry management | Bucket list only; no SQL persistence layer |
| `LedgerTxnOfferSQL.cpp` | SQL backend not supported |
| `LedgerHeaderUtils::storeInDatabase()` | No SQL backend |
| `LedgerHeaderUtils::loadByHash/Sequence()` | No SQL backend |
| `LedgerHeaderUtils::deleteOldEntries()` | No SQL backend |
| `LedgerHeaderUtils::copyToStream()` | No SQL backend |
| `LedgerHeaderUtils::dropAll()` | No SQL backend |
| `LedgerManager::getDatabase()` | No SQL backend |
| `LedgerManager::startCatchup()` | Handled by app crate |
| `LedgerTxnRoot::Impl` (SQL commit, prefetch, bulk ops) | No SQL backend |
| `BulkLedgerEntryChangeAccumulator` | SQL batch optimization |
| `TransactionMode` (READ_ONLY_WITHOUT_SQL_TXN) | No SQL backend |
| `LedgerTxnConsistency` (EXACT vs EXTRA_DELETES) | No SQL backend |
| `BEST_OFFER_DEBUGGING` | Debug-only conditional compilation |
| `InMemoryLedgerTxn` / `InMemoryLedgerTxnRoot` (test utilities) | Test-only in-memory backend |
| `InflationWinner` / inflation queries | Inflation disabled in protocol 12+ |
| `P23HotArchiveBug.h/.cpp` | Mainnet-specific historical bug fix, not protocol behavior |
| `FlushAndRotateMetaDebugWork.h/.cpp` | Operational debug tooling |
| `LedgerManager::ledgerAbbrev()` | Logging helper, not protocol behavior |
| Postgres-specific code (`USE_POSTGRES` blocks) | SQLite only per project guidelines |
| `CheckpointRange.h/.cpp` | History checkpoint utilities, handled by history crate |
| `LedgerManager::isApplying()` / `markApplyStateReset()` | Single-threaded; no apply phase machine needed |
| `LedgerManager::partiallyLoadLastKnownLedgerForUtils()` | Diagnostic utility, not protocol behavior |
| `LedgerManager::moveToSynced()` / `beginApply()` / state machine | App-level state managed by app crate |
| `LedgerManager::getLastClosedSnapshot()` / `getLastClosedLedgerHAS()` | Exposed via `LedgerManager` methods with different signatures |
| `EntryPtrState` / entry activation tracking | Concurrency safety mechanism for LedgerTxn; not needed with delta model |
| `RestoredEntries` tracking (separate hot archive vs live BL maps) | Restorations tracked inline in execution |
| `InternalLedgerEntry` / `InternalLedgerKey` (generalized types) | Simplified to direct `LedgerEntry`/`LedgerKey`; sponsorship/seqnum tracked differently |
| `ThreadInvariant` class | Not needed; Rust ownership model provides thread safety |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `SharedModuleCacheCompiler` (WASM module caching) | Low | Per-TX `PersistentModuleCache` in `henyey-tx` provides functionally equivalent caching; architectural difference (per-TX vs global) |
| `LedgerManagerImpl::ApplyState` phase machine | Medium | Multi-threaded apply coordination |
| `SorobanMetrics` class | Low | Observability, not correctness |
| `SorobanNetworkConfig::createLedgerEntriesForV20()` | Medium | Genesis config initialization |
| `SorobanNetworkConfig::createCostTypesForV21/V22/V25()` | Medium | Protocol upgrade config creation |
| `SorobanNetworkConfig::createAndUpdateLedgerEntriesForV23()` | Medium | Protocol 23 upgrade config |
| `LedgerTxn` full nested transaction model | Low | General-purpose nesting beyond savepoints |
| `CompleteConstLedgerState` as unified type | Low | Immutable state wrapper |
| `LedgerManager::getExpectedLedgerCloseTime()` | Low | Timing utility |
| `LedgerManager::secondsSinceLastLedgerClose()` | Low | Timing utility |
| `LedgerManager::syncMetrics()` | Low | Metrics publishing |
| `LedgerManagerImpl::prefetchTransactionData()` | Low | Performance optimization |
| `LedgerManagerImpl::setupLedgerCloseMetaStream()` | Low | Debug meta streaming |

## Architectural Differences

1. **State Management Model**
   - **stellar-core**: Uses `LedgerTxn` with arbitrarily nested parent/child relationships for transactional isolation. Each nested transaction can be independently committed or rolled back. Entries have activation tracking (`EntryPtrState`) to prevent concurrent access. Uses `InternalLedgerEntry`/`InternalLedgerKey` generalized types that extend `LedgerEntry`/`LedgerKey` with sponsorship and sequence number tracking.
   - **Rust**: Uses `LedgerDelta` with change coalescing and per-operation savepoints. Savepoints provide the same operation-level rollback isolation as stellar-core nested `LedgerTxn` for the execution loop. Works directly with `LedgerEntry`/`LedgerKey` XDR types.
   - **Rationale**: The delta+savepoint model is simpler while covering the primary use case (per-operation rollback during transaction execution). General nesting is not needed by the current execution pipeline. The `InternalLedgerEntry` generalization is unnecessary because sponsorship tracking is handled inline during operation execution.

2. **Threading Model**
   - **stellar-core**: Multi-threaded with dedicated apply thread and parallel Soroban execution threads. Uses an `ApplyState` phase machine (SETTING_UP_STATE -> READY_TO_APPLY -> APPLYING -> COMMITTING) to coordinate thread access. `LedgerTxn` enforces same-thread invariant. Transaction execution split into `preParallelApply` (sequential on primary thread) and `parallelApply` (on worker threads).
   - **Rust**: Parallel Soroban cluster execution via `tokio::spawn_blocking` in `execute_stage_clusters()`. Each cluster gets its own `TransactionExecutor` and `LedgerDelta`, merged back into the parent delta after completion. Transaction execution split into `pre_apply()` (validation, fee, seq bump) and `apply_body()` (operations) matching the upstream architecture. No `ApplyState` phase machine — ledger close orchestration is sequential at the stage level.
   - **Rationale**: The `pre_apply`/`apply_body` split mirrors stellar-core's `preParallelApply`/`parallelApply` architecture, enabling correct parallel execution without an explicit phase machine. Tokio's blocking thread pool replaces `std::async` for cluster parallelism.

3. **Persistence Layer**
   - **stellar-core**: Dual SQL + Bucket List with SQL being phased out. Maintains offer tables in SQL with complex bulk upsert/delete operations. Supports read-only and read-write SQL transaction modes. Uses `RandomEvictionCache` for entry caching, `BulkLedgerEntryChangeAccumulator` for batch SQL operations.
   - **Rust**: Bucket List only with in-memory offer index. No SQL persistence layer.
   - **Rationale**: The project targets BucketListDB-only mode, which is the direction stellar-core is moving. SQL support is not needed.

4. **Module Caching**
   - **stellar-core**: Maintains a shared `SorobanModuleCache` with compiled WASM modules. Supports background compilation on multiple threads (`SharedModuleCacheCompiler`) and incremental updates (add/evict).
   - **Rust**: Relies on `PersistentModuleCache` from `henyey-tx` which caches modules at the Soroban host level, but does not have the shared multi-threaded compilation pipeline.
   - **Rationale**: Module caching is a performance optimization that can be added incrementally without affecting correctness.

5. **Entry Lookup Strategy**
   - **stellar-core**: Uses `LedgerTxnRoot` with entry cache, prefetch batching, and best-offer deque caching for efficient database access. Soroban entries served from `InMemorySorobanState`. Complex `WorstBestOfferMap` optimization for nested offer exchange loops.
   - **Rust**: Direct bucket list lookups with in-memory Soroban state. Offer lookups use an in-memory HashMap index populated during initialization.
   - **Rationale**: Without SQL, the complex caching hierarchy is unnecessary. Bucket list lookups are efficient for the entry types that need them.

6. **Snapshot Architecture**
   - **stellar-core**: Uses `AbstractLedgerStateSnapshot` base class with two implementations: `LedgerTxnReadOnly` (SQL-backed) and `BucketSnapshotState` (bucket list-backed). `CompleteConstLedgerState` bundles all state components. Wrapper types (`LedgerEntryWrapper`, `LedgerHeaderWrapper`) provide unified access.
   - **Rust**: Uses `LedgerSnapshot` + `SnapshotHandle` with optional lazy-loading lookup functions. No separate wrapper types needed since all access is bucket list-based.
   - **Rationale**: Single storage backend (bucket list) eliminates the need for an abstract snapshot hierarchy.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| LedgerTxn | 32 TEST_CASE / 251 SECTION | 33 #[test] in `delta.rs` | Rust tests focus on coalescing; nested txn tests not needed |
| Liabilities | 3 TEST_CASE / 37 SECTION | 42 #[test] in `lib.rs` | Good parity on reserve/liability tests |
| Ledger Header | 3 TEST_CASE / 1 SECTION | 3 #[test] in `header.rs` | Covers hash, skip list, chain verify |
| Ledger Close Meta | 3 TEST_CASE / 6 SECTION | 7 #[test] in `close.rs`, 2 in `ledger_close_meta_vectors.rs` | Covers tx set handling, ordering |
| Ledger Close | 1 TEST_CASE / 0 SECTION | 23 #[test] in `manager.rs`, 5 in integration tests | Rust has more close pipeline tests |
| Snapshots | -- | 7 #[test] in `snapshot.rs` | No upstream snapshot-specific tests |
| Offers | -- | 9 #[test] in `offer.rs` | Offer comparison tests |
| Config Upgrades | -- | 5 #[test] in `config_upgrade.rs` | Config validation tests |
| Soroban State | -- | 15 #[test] in `soroban_state.rs` | In-memory state tests |
| Execution | -- | 8 #[test] in `execution/mod.rs`, 39 in integration tests | Transaction execution and classic event tests |
| Meta Vectors | -- | 4 #[test] in vector test files | Hash verification against stellar-core |

### Test Gaps

- **LedgerTxn nested transaction tests** (32 upstream TEST_CASE / 251 SECTION): The Rust crate does not replicate the full LedgerTxn test suite since it uses a different state model (delta vs nested transactions). The coalescing tests in `delta.rs` cover the equivalent behavior.
- **Ledger close meta stream tests**: No equivalent for `LedgerCloseMetaStreamTests.cpp` (3 TEST_CASE / 6 SECTION) which tests meta file rotation and streaming.
- **Entry activation/deactivation tests**: Not applicable since Rust does not use the activation tracking pattern.

## Verification Results

The ledger crate has been verified against testnet for ledger close correctness. Header hash matches, bucket list hash matches, and transaction metadata are compared during online catchup and replay.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 89 |
| Gaps (None + Partial) | 46 |
| Intentional Omissions | 30 |
| **Parity** | **89 / (89 + 46) = 65%** |

The 89 implemented items cover: LedgerManager core operations (30, including `applySorobanStages`, `applySorobanStageClustersInParallel`, `applyThread`), header utilities (8), delta/change tracking (13), close data (8), snapshots (8), execution pipeline (21, including `commonPreApply`→`pre_apply`, `preParallelApply`→`pre_apply`, `parallelApply`→`apply_body`), config upgrade (6), offer utilities (5), in-memory Soroban state (15), fee/reserve calculations (8), trustline utilities (7). Note that some upstream items map to the same Rust function, so the Rust function count is lower.

The 46 gap items include: LedgerManager threading/metrics methods (7 counted as gaps), NetworkConfig creation functions (5), ApplyState phase machine (1), module cache (1), nested transaction model (1), snapshot unification (1), timing utilities (2), metrics (1), prefetch (1), meta streaming (1), and the None items from each Component Mapping that are not classified as omissions.

The 30 intentional omissions are primarily SQL backend features (LedgerTxnRoot, offer SQL, header SQL operations), debug tooling (P23HotArchiveBug, FlushAndRotateMetaDebugWork), deprecated functionality (inflation), C++ implementation artifacts (InternalLedgerEntry, EntryPtrState, ThreadInvariant), and features handled by other crates (catchup, checkpoint ranges, app state machine).
