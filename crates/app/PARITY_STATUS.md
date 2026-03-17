# stellar-core Parity Status

**Crate**: `henyey-app`
**Upstream**: `stellar-core/src/main/`
**Overall Parity**: 73%
**Last Updated**: 2026-03-17

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Application lifecycle | Full | Init, run, shutdown, state machine |
| Application subsystem accessors | Partial | Core subsystems wired; some accessors missing |
| Configuration loading and validation | Partial | TOML-based; many testing/advanced fields omitted |
| HTTP command handler | Full | All production endpoints implemented |
| CLI commands | Partial | Core commands present; several utility commands missing |
| Maintainer | Full | Automatic DB cleanup with configurable period |
| PersistentState | Partial | Covered by `henyey-db`; some operations missing |
| Survey system | Full | Time-sliced collecting, reporting, rate limiting |
| Metadata stream | Full | XDR output stream with debug rotation |
| Logging and diagnostics | Full | Dynamic levels, partition mapping |
| ApplicationUtils | Partial | Core utils present; several utility functions missing |
| QueryServer | Full | getledgerentryraw, getledgerentry with TTL/hot-archive |
| AppConnector | None | Thread-isolation helper not needed (Tokio model) |
| SettingsUpgradeUtils | None | Soroban upgrade transaction builders |
| dumpxdr utilities | Partial | Basic XDR decode; full dump/sign suite incomplete |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Application.h` / `Application.cpp` | `app/mod.rs` | App struct with lifecycle management |
| `ApplicationImpl.h` / `ApplicationImpl.cpp` | `app/mod.rs`, `app/lifecycle.rs` | Merged into App struct and submodules |
| `ApplicationUtils.h` / `ApplicationUtils.cpp` | `app/mod.rs`, `app/catchup_impl.rs`, `catchup_cmd.rs`, `run_cmd.rs` | Distributed across modules |
| `Config.h` / `Config.cpp` | `config.rs` | TOML-based with serde |
| `CommandHandler.h` / `CommandHandler.cpp` | `run_cmd.rs` (StatusServer) | Axum-based HTTP server |
| `CommandLine.h` / `CommandLine.cpp` | `run_cmd.rs`, `catchup_cmd.rs`, `henyey` crate | CLI in separate binary crate |
| `Maintainer.h` / `Maintainer.cpp` | `maintainer.rs` | Async background task |
| `PersistentState.h` / `PersistentState.cpp` | `henyey-db` crate | Handled by database crate |
| `Diagnostics.h` / `Diagnostics.cpp` | — | Not implemented |
| `ErrorMessages.h` | — | Inline error strings |
| `QueryServer.h` / `QueryServer.cpp` | `http/mod.rs` (QueryServer), `http/handlers/query.rs` | Full implementation with both endpoints |
| `AppConnector.h` / `AppConnector.cpp` | — | Not needed (Tokio model) |
| `SettingsUpgradeUtils.h` / `SettingsUpgradeUtils.cpp` | — | Not implemented |
| `dumpxdr.h` / `dumpxdr.cpp` | `henyey` crate (partial) | Partial decode-xdr support |
| `StellarCoreVersion.h` | `Cargo.toml` version | Via CARGO_PKG_VERSION |

## Component Mapping

### Application (`app/mod.rs`, `app/lifecycle.rs`, `app/ledger_close.rs`, `app/catchup_impl.rs`, `app/consensus.rs`)

Corresponds to: `Application.h`, `ApplicationImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Application::create()` | `App::new()` | Full |
| `initialize()` | `App::new()` (inline) | Full |
| `resetLedgerState()` | — | None |
| `timeNow()` | System time via `std::time` | Full |
| `getConfig()` | `App::config()` | Full |
| `getState()` | `App::state()` | Full |
| `getStateHuman()` | `AppState::Display` impl | Full |
| `isStopping()` | `AppState::ShuttingDown` check | Full |
| `getClock()` | — | None |
| `getMetrics()` | — (Prometheus via HTTP) | Partial |
| `syncOwnMetrics()` | — | None |
| `syncAllMetrics()` | — | None |
| `clearMetrics()` | `App::clear_metrics()` | Partial |
| `getTmpDirManager()` | — | None |
| `getLedgerManager()` | `App::ledger_manager` field | Full |
| `getBucketManager()` | `App::bucket_manager` field | Full |
| `getLedgerApplyManager()` | — (merged into LedgerManager) | Full |
| `getHistoryArchiveManager()` | — (in catchup flow) | Partial |
| `getHistoryManager()` | — (in history crate) | Partial |
| `getMaintainer()` | `Maintainer` struct | Full |
| `getProcessManager()` | — | None |
| `getHerder()` | `App::herder` field | Full |
| `getHerderPersistence()` | — (in db crate) | Partial |
| `getInvariantManager()` | — (partial in ledger) | Partial |
| `getOverlayManager()` | `App::overlay` field | Full |
| `getDatabase()` | `App::database()` | Full |
| `getPersistentState()` | — (in db crate) | Partial |
| `getCommandHandler()` | `StatusServer` | Full |
| `getWorkScheduler()` | — (in work crate) | Partial |
| `getBanManager()` | — (in overlay crate) | Full |
| `getStatusManager()` | — (HTTP endpoints) | Partial |
| `getProtocol23CorruptionDataVerifier()` | — | None |
| `getProtocol23CorruptionEventReconciler()` | — | None |
| `getWorkerIOContext()` | — (Tokio runtime) | Full |
| `getEvictionIOContext()` | — | None |
| `getOverlayIOContext()` | — (Tokio runtime) | Full |
| `getLedgerCloseIOContext()` | — (Tokio runtime) | Full |
| `postOnMainThread()` | Tokio task spawning | Full |
| `postOnBackgroundThread()` | `tokio::spawn_blocking` | Full |
| `postOnEvictionBackgroundThread()` | — | None |
| `postOnOverlayThread()` | Tokio task spawning | Full |
| `postOnLedgerCloseThread()` | Tokio task spawning | Full |
| `start()` | `App::run()` | Full |
| `gracefulStop()` | `App::shutdown()` | Full |
| `joinAllThreads()` | — (Tokio handles) | Full |
| `manualClose()` | `App::manual_close_ledger()` | Full |
| `applyCfgCommands()` | — | None |
| `reportCfgMetrics()` | — | None |
| `getJsonInfo()` | `App::info()` | Full |
| `reportInfo()` | `print_startup_info()` | Full |
| `scheduleSelfCheck()` | `App::self_check()` | Full |
| `getNetworkID()` | `App::network_id()` | Full |
| `getLedgerTxnRoot()` | — (different architecture) | None |
| `validateAndLogConfig()` | `AppConfig::validate()` | Full |
| `threadIsType()` | — (not needed with Tokio) | None |
| `getAppConnector()` | — (not needed with Tokio) | None |
| `validateNetworkPassphrase()` | `AppConfig::validate()` | Full |

### Configuration (`config.rs`)

Corresponds to: `Config.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Config()` constructor | `AppConfig::default()` | Full |
| `load(filename)` | `AppConfig::from_file()` | Full |
| `load(istream)` | — | None |
| `adjust()` | — | None |
| `toShortString()` | — | None |
| `toStrKey()` | — | None |
| `resolveNodeID()` | — | None |
| `modeDoesCatchupWithBucketList()` | — | None |
| `allBucketsInMemory()` | — | None |
| `logBasicInfo()` | `print_startup_info()` | Full |
| `parallelLedgerClose()` | — | None |
| `setNoListen()` | — | None |
| `setNoPublish()` | — | None |
| `getSorobanByteAllowance()` | `surge_pricing.soroban_byte_allowance` | Full |
| `getClassicByteAllowance()` | `surge_pricing.classic_byte_allowance` | Full |
| `toString(qset)` | — | None |
| Core config fields (FORCE_SCP, RUN_STANDALONE, etc.) | — | Partial |
| Network/overlay/peer config fields | `OverlayConfig`, `NetworkConfig` | Full |
| `METADATA_OUTPUT_STREAM` | `MetadataConfig::output_stream` | Full |
| `METADATA_DEBUG_LEDGERS` | `MetadataConfig::debug_ledgers` | Full |
| `CATCHUP_COMPLETE` / `CATCHUP_RECENT` | `CatchupConfig` | Full |
| `AUTOMATIC_MAINTENANCE_PERIOD` / `COUNT` | `MaintenanceConfig` | Full |
| `MANUAL_CLOSE` | `NodeConfig::manual_close` | Full |
| `ARTIFICIALLY_*_FOR_TESTING` fields | — | None |
| `LOADGEN_*` fields | — | None |
| `APPLY_LOAD_*` fields | — | None |
| `BUCKETLIST_DB_*` fields | `BucketConfig::bucket_list_db` | Full |
| `EXPERIMENTAL_*` fields | — | None |
| `processOpApplySleepTimeForTestingConfigs()` | — | None |
| `getExpectedLedgerCloseTimeTestingOverride()` | — | None |

### HTTP Command Handler (`run_cmd.rs`)

Corresponds to: `CommandHandler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CommandHandler()` | `StatusServer::new()` | Full |
| `manualCmd()` | — | None |
| `bans()` | `bans_handler` | Full |
| `connect()` | `connect_handler` | Full |
| `dropPeer()` | `droppeer_handler` | Full |
| `info()` | `info_handler` | Full |
| `ll()` | `ll_handler` | Full |
| `logRotate()` | `logrotate_handler` | Full |
| `maintenance()` | `maintenance_handler` | Full |
| `manualClose()` | `manualclose_handler` | Full |
| `metrics()` | `metrics_handler` | Full |
| `clearMetrics()` | `clearmetrics_handler` | Full |
| `peers()` | `peers_handler` | Full |
| `selfCheck()` | `self_check_handler` | Full |
| `quorum()` | `quorum_handler` | Full |
| `scpInfo()` | `scp_handler` | Full |
| `tx()` | `submit_tx_handler` | Full |
| `unban()` | `unban_handler` | Full |
| `upgrades()` | `upgrades_handler` | Full |
| `dumpProposedSettings()` | `dumpproposedsettings_handler` | Full |
| `surveyTopology()` (legacy) | — | None |
| `stopSurvey()` (legacy) | — | None |
| `getSurveyResult()` (legacy) | — | None |
| `sorobanInfo()` | `sorobaninfo_handler` | Full |
| `startSurveyCollecting()` | `start_survey_collecting_handler` | Full |
| `stopSurveyCollecting()` | `stop_survey_collecting_handler` | Full |
| `surveyTopologyTimeSliced()` | `survey_topology_handler` | Full |
| `generateload()` | `generateload_handler` | Full |
| `checkBooted()` | `survey_booted()` | Full |

### Maintainer (`maintainer.rs`)

Corresponds to: `Maintainer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Maintainer()` | `Maintainer::new()` | Full |
| `start()` | `Maintainer::start()` | Full |
| `performMaintenance()` | `Maintainer::perform_maintenance()` | Full |

### ApplicationUtils (`app/mod.rs`, `app/catchup_impl.rs`, `catchup_cmd.rs`, `run_cmd.rs`)

Corresponds to: `ApplicationUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `setupApp()` | `App::new()` | Full |
| `runApp()` | `run_node()` | Full |
| `setForceSCPFlag()` | — | None |
| `initializeDatabase()` | DB init in `App::new()` | Full |
| `httpCommand()` | — | None |
| `selfCheck()` | `App::self_check()` | Full |
| `mergeBucketList()` | — | None |
| `dumpStateArchivalStatistics()` | — | None |
| `dumpLedger()` | `dump-ledger` in henyey binary | Full |
| `dumpWasmBlob()` | — | None |
| `showOfflineInfo()` | `info` command in henyey binary | Partial |
| `reportLastHistoryCheckpoint()` | — | None |
| `checkQuorumIntersectionFromJson()` | `check-quorum-intersection` in henyey | Full |
| `loadXdr()` | — | None |
| `rebuildLedgerFromBuckets()` | — | None |
| `genSeed()` | `new-keypair` in henyey binary | Full |
| `initializeHistories()` | — | None |
| `writeCatchupInfo()` | — | None |
| `catchup()` | `App::catchup()` | Full |
| `applyBucketsForLCL()` | In catchup flow | Full |
| `publish()` | `publish-history` in henyey binary | Full |
| `minimalDBForInMemoryMode()` | — | None |
| `canRebuildInMemoryLedgerFromBuckets()` | — | None |
| `setAuthenticatedLedgerHashPair()` | — | None |
| `getStellarCoreMajorReleaseVersion()` | — | None |

### PersistentState (in `henyey-db` crate)

Corresponds to: `PersistentState.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState()` | DB init in henyey-db | Full |
| `dropAll()` | Schema migration | Full |
| `getState()` | `StateQueries::get_state()` | Full |
| `setState()` | `StateQueries::set_state()` | Full |
| `getSCPStateAllSlots()` | `ScpQueries::get_scp_state_all_slots()` | Full |
| `getTxSetsForAllSlots()` | `ScpQueries::get_tx_sets_for_all_slots()` | Partial |
| `getTxSetHashesForAllSlots()` | — | None |
| `setSCPStateV1ForSlot()` | `ScpQueries::set_scp_state_for_slot()` | Partial |
| `shouldRebuildForOfferTable()` | — | None |
| `clearRebuildForOfferTable()` | — | None |
| `setRebuildForOfferTable()` | — | None |
| `hasTxSet()` | — | None |
| `deleteTxSets()` | — | None |
| `migrateToSlotStateTable()` | — | None |

### Logging (`logging.rs`)

Corresponds to: Logging subsystem in stellar-core (no single header)

| stellar-core | Rust | Status |
|--------------|------|--------|
| Log initialization | `init_with_handle()` | Full |
| Per-partition log levels | `LogLevelHandle::set_partition_level()` | Full |
| Dynamic level changes | `LogLevelHandle::set_level()` | Full |
| Log partitions | `LOG_PARTITIONS` constant | Full |
| Progress tracking | `ProgressTracker`, `CatchupProgress` | Full |

### Metadata Stream (`meta_stream.rs`)

Corresponds to: `METADATA_OUTPUT_STREAM` in `ApplicationImpl.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| Main XDR output stream | `MetaStreamManager::emit_meta()` | Full |
| Debug meta stream with rotation | `MetaStreamManager::maybe_rotate_debug_stream()` | Full |
| fd: syntax support | `MetaStreamManager::open_stream()` | Full |
| Gzip compression of segments | `MetaStreamManager::gzip_file()` | Full |

### Survey System (`survey.rs`)

Corresponds to: Survey logic in `CommandHandler.cpp` and overlay

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SurveyDataManager` | `SurveyDataManager` | Full |
| `SurveyMessageLimiter` | `SurveyMessageLimiter` | Full |
| Time-sliced collecting phase | `start_collecting()` / `stop_collecting()` | Full |
| Time-sliced reporting phase | `fill_survey_data()` | Full |
| Latency histograms | `LatencyHistogram` | Full |
| Phase timeout enforcement | `update_phase()` | Full |

### QueryServer (`http/mod.rs`, `http/handlers/query.rs`)

Corresponds to: `QueryServer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `QueryServer()` constructor | `QueryServer::new()` | Full |
| `getLedgerEntryRaw()` | `getledgerentryraw_handler()` | Full |
| `getLedgerEntry()` | `getledgerentry_handler()` | Full |

### Catchup Command (`catchup_cmd.rs`)

Corresponds to: `catchup()` in `ApplicationUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| Catchup orchestration | `run_catchup()` | Full |
| Target parsing (ledger/count) | `parse_target_with_mode()` | Full |
| Catchup modes (minimal/complete/recent) | `CatchupMode` enum | Full |
| Progress callbacks | `CatchupProgressCallback` trait | Full |
| Verification | `verify_catchup()` | Partial |

### Run Command (`run_cmd.rs`)

Corresponds to: `runApp()` in `ApplicationUtils.h`, `CommandHandler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| Main run loop | `run_main_loop()` | Full |
| Signal handling (SIGTERM, Ctrl+C) | `wait_for_shutdown_signal()` | Full |
| HTTP status server | `StatusServer` | Full |
| State restoration from disk | `load_last_known_ledger()` | Full |
| Sync detection | `check_needs_catchup()` | Full |
| NodeRunner wrapper | `NodeRunner` struct | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `VirtualClock` | Replaced by Tokio async runtime; not needed for real-time operation |
| `AppConnector` | Thread-isolation helper; unnecessary with Tokio's shared-nothing async model |
| `ThreadType` / `threadIsType()` | Tokio manages task scheduling; explicit thread typing not needed |
| Worker/Eviction/Overlay IO contexts | Replaced by Tokio runtime with task-based concurrency |
| `TmpDirManager` | Uses standard `tempfile` crate |
| `ProcessManager` | External process spawning handled by `tokio::process` where needed |
| `ARTIFICIALLY_*_FOR_TESTING` configs | Testing-only knobs; Rust tests use different patterns |
| `LOADGEN_*` / `APPLY_LOAD_*` config fields | Load generation uses HTTP query params, not config fields |
| `BUILD_TESTS`-only endpoints | Rust test framework provides equivalent capabilities |
| Legacy (non-time-sliced) survey | Deprecated in stellar-core v25 |
| `Protocol23CorruptionDataVerifier` | Protocol 23 bug workaround; not needed for p24+ only |
| `Protocol23CorruptionEventReconciler` | Protocol 23 bug workaround; not needed for p24+ only |
| `BEST_OFFER_DEBUGGING` | Debug-only feature; not applicable |
| `MODE_USES_IN_MEMORY_LEDGER` | Test-only mode; Rust uses different testing approach |
| `EXPERIMENTAL_PARALLEL_LEDGER_APPLY` | Future feature; not in stable upstream |
| `EXPERIMENTAL_BACKGROUND_TX_SIG_VERIFICATION` | Experimental; not in stable upstream |
| `setNoListen()` / `setNoPublish()` | Test convenience setters; config handled differently in Rust |
| `allBucketsInMemory()` / `modeDoesCatchupWithBucketList()` | In-memory test mode helpers; not applicable |
| `getLedgerTxnRoot()` | Different architecture; Rust does not expose raw LedgerTxn root |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `SettingsUpgradeUtils` (getWasmRestoreTx, getUploadTx, etc.) | Medium | Soroban config upgrade transaction builders |
| `Diagnostics::bucketStats()` | Low | Bucket statistics diagnostic tool |
| `dumpXdrStream()` / `printXdr()` full support | Low | Full XDR dump and print utilities |
| `signtxns()` / `signtxn()` full support | Low | Multi-transaction signing |
| `setForceSCPFlag()` | Low | Force SCP on startup |
| `httpCommand()` CLI utility | Low | Send HTTP commands from CLI |
| `mergeBucketList()` | Low | Offline bucket list merge tool |
| `dumpStateArchivalStatistics()` | Low | Archival statistics reporting |
| `reportLastHistoryCheckpoint()` | Low | Report last checkpoint to file |
| `initializeHistories()` | Low | Initialize new history archives |
| `showOfflineInfo()` full support | Low | Complete offline info display |
| `Config::adjust()` | Low | Connection setting adjustment |
| `Config::resolveNodeID()` | Low | Node ID resolution from names |
| `Config::toShortString()` / `toStrKey()` | Low | Node ID display helpers |
| PersistentState: `migrateToSlotStateTable()` | Low | SCP state table migration |
| PersistentState: rebuild-for-offer-table flags | Low | Offer table rebuild tracking |
| `resetLedgerState()` | Low | Pre-bucket-application state reset |
| `applyCfgCommands()` | Low | Execute config-embedded commands at startup |
| `reportCfgMetrics()` | Low | Report configured metrics to logs |
| `syncOwnMetrics()` / `syncAllMetrics()` | Low | Medida metrics sync (Prometheus model differs) |
| `getStellarCoreMajorReleaseVersion()` | Low | Version string parsing utility |
| `writeCatchupInfo()` | Low | Write catchup JSON info to file |
| `Config::load(istream)` | Low | Loading config from stream (only file loading supported) |
| `Config::toString(qset)` | Low | Quorum set display helper |
| `dumpWasmBlob()` | Low | Wasm blob dump utility |
| `loadXdr()` | Low | Generic XDR loading utility |
| `rebuildLedgerFromBuckets()` | Low | Rebuild ledger state from bucket snapshots |
| `setAuthenticatedLedgerHashPair()` | Low | Set authenticated ledger hash for catchup verification |
| PersistentState: `getTxSetHashesForAllSlots()` | Low | Retrieve tx set hashes for SCP state |
| PersistentState: `hasTxSet()` / `deleteTxSets()` | Low | Tx set existence check and deletion |

## Architectural Differences

1. **Async Runtime**
   - **stellar-core**: ASIO io_context with explicit worker thread pools (main, worker, eviction, overlay, ledger-close)
   - **Rust**: Tokio async runtime with task-based concurrency
   - **Rationale**: Tokio provides equivalent concurrency with simpler code and no manual thread management

2. **Configuration Format**
   - **stellar-core**: Custom cpptoml parser with SCREAMING_SNAKE_CASE field names
   - **Rust**: Standard `toml` crate with serde deserialization, snake_case fields, environment variable overrides
   - **Rationale**: Leverages Rust ecosystem for type-safe config parsing with less custom code

3. **HTTP Server**
   - **stellar-core**: Custom `lib/http/server.hpp` with query string routing
   - **Rust**: Axum framework with typed extractors and tower middleware
   - **Rationale**: Production-grade async HTTP framework with built-in graceful shutdown

4. **Metrics System**
   - **stellar-core**: libmedida MetricsRegistry with JSON export via `/metrics`
   - **Rust**: Prometheus text format with direct gauge/counter computation in handlers
   - **Rationale**: Prometheus is the industry standard for metrics; eliminates medida dependency

5. **Logging**
   - **stellar-core**: Custom logging system with per-partition levels
   - **Rust**: `tracing` ecosystem with `tracing-subscriber` reload layers for dynamic levels
   - **Rationale**: `tracing` provides structured logging, span context, and dynamic filtering

6. **Application Structure**
   - **stellar-core**: Abstract `Application` interface with `ApplicationImpl` concrete class; subsystems accessed via virtual getters
   - **Rust**: Single `App` struct with direct field ownership; subsystems accessed via typed fields behind `Arc`/`RwLock`
   - **Rationale**: Rust ownership model makes virtual interface pattern unnecessary; direct field access is idiomatic

7. **Thread Isolation (AppConnector)**
   - **stellar-core**: `AppConnector` wraps `Application&` to provide thread-safe subset of methods
   - **Rust**: Not needed; Tokio's `Send + Sync` bounds and `Arc<RwLock<T>>` provide compile-time thread safety
   - **Rationale**: Rust's type system enforces thread safety at compile time

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Config | 8 TEST_CASE / 18 SECTION | 32 `#[test]` | Good coverage of config loading, validation, and BucketListDB wiring |
| CommandHandler | 3 TEST_CASE / 25 SECTION | 6 `#[test]` | Rust tests cover run_cmd options; less endpoint testing |
| ApplicationUtils | 4 TEST_CASE / 5 SECTION | 13 `#[test]` (catchup_cmd) | Catchup target parsing well tested |
| SelfCheck | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | No dedicated self-check tests |
| QueryServer | 1 TEST_CASE / 9 SECTION | 7 `#[test]` | Basic query endpoint coverage |
| App core | — | 26 `#[test]` | App-level tests |
| Compat config | — | 21 `#[test]` | stellar-core config format translation |
| Maintainer | — | 12 `#[test]` | Good coverage (threshold correctness, RPC retention, config) |
| GenerateLoad | — | 8 `#[test]` | Handler and type tests |
| Logging | — | 5 `#[test]` | Basic coverage |
| Meta stream | — | 5 `#[test]` | Covers emit, rotation, error handling |

### Test Gaps

- **HTTP endpoint integration tests**: stellar-core has 25 SECTION entries testing various command handler scenarios (manual close with different parameters, overlay-only mode toggle, transaction envelope bridge). The Rust crate lacks HTTP endpoint integration tests.
- **QueryServer tests**: 1 TEST_CASE with 9 SECTION entries in stellar-core for getLedgerEntry; Rust has 7 tests but doesn't cover all edge cases.
- **Self-check scheduling tests**: stellar-core tests online self-check scheduling; Rust only has the self-check function, not its scheduling.
- **Config edge cases**: stellar-core has extensive tests for validator config validation (bad validators, nesting levels, operation filters, domain quality). Rust tests are simpler.

## Verification Results

- **Testnet verification**: Node successfully syncs and tracks consensus on testnet, closing ledgers in parity with stellar-core validators.
- **Catchup gap recovery**: Successfully bridges 20-30 slot gaps between catchup checkpoint and live consensus (verified January 2026).
- **Event loop stability**: Multiple event loop freeze bugs identified and fixed (February 2026): blocking flood demand sends, unbounded buffered ledger close loops, blocking bucket GC during catchup, and SCP drain starvation.
- **Survey protocol**: Time-sliced surveys successfully collect and report topology data from testnet peers.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 116 |
| Gaps (None + Partial) | 44 |
| Intentional Omissions | 27 |
| **Parity** | **116 / (116 + 44) = 73%** |
