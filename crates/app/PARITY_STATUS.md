# stellar-core Parity Status

**Crate**: `henyey-app`
**Upstream**: `stellar-core/src/main/`
**Overall Parity**: 70%
**Last Updated**: 2026-04-27

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Application lifecycle and runtime wiring | Full | Init, run, catchup, shutdown, recovery loops |
| Configuration loading and compat translation | Partial | Core TOML and captive-core translation work; many stellar-core helpers omitted |
| HTTP admin and query surfaces | Partial | Core endpoints exist including generateLoad; several compat admin routes are stubbed or absent |
| Catchup and restart recovery | Full | Archive catchup, replay, restart restore, publish flow wired |
| Persistent state integration | Partial | Critical state persisted through `henyey-db`; some SCP helper APIs absent |
| Transaction flooding | Partial | Pull-mode advert/demand scheduling; classic flood budgets use stellar-core truncate-then-round integer arithmetic with distinct `FLOOD_TX_PERIOD_MS` for broadcast budget; Soroban advert/demand capacity remains a gap; advert flush cadence is consolidated with broadcast (200 ms) rather than separate (100 ms); removed non-parity `base_fee` overlay pre-filter on `GeneralizedTxSet` receive (stellar-core does zero validation at receive layer); overlay receive layer now matches stellar-core (zero structural validation — all validation deferred to herder via `prepare_for_apply`); buffered ledger close validates tx sets via `prepare_for_apply` before application (matching stellar-core `LedgerManagerImpl.cpp:1542`) |
| Background maintenance | Full | Periodic pruning and RPC-retention cleanup implemented; **divergence**: stale publish queue entries are evicted after `MAX_PUBLISH_QUEUE_CHECKPOINT_DISTANCE` (30) intervals to prevent unbounded retention from failing archives — stellar-core does not evict (#2004) |
| Survey and network diagnostics | Partial | Time-sliced surveys implemented; `Diagnostics::bucketStats()` absent |
| Metadata streaming | Full | Main stream, debug rotation, gzip segments supported |
| Logging and runtime controls | Partial | Dynamic log levels work; compat `/ll` behavior is incomplete |
| Banned account persistence | None | No `FILTERED_G_ADDRESSES` / account-ban subsystem yet |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Application.h` / `ApplicationImpl.h` | `src/app/mod.rs` | Main runtime object and subsystem ownership |
| `ApplicationImpl.cpp` | `src/app/lifecycle.rs` | Event loop, startup, shutdown, timers |
| `ApplicationImpl.cpp` | `src/app/ledger_close.rs` | Ledger-close persistence and buffered apply |
| `ApplicationImpl.cpp` | `src/app/catchup_impl.rs` | Catchup orchestration and restart recovery |
| `ApplicationImpl.cpp` | `src/app/consensus.rs` | SCP recovery, timeout, and sync logic |
| `ApplicationImpl.cpp` | `src/app/peers.rs` | Peer inspection, connect/drop/unban helpers |
| `ApplicationImpl.cpp` | `src/app/publish.rs` | History checkpoint publishing |
| `ApplicationImpl.cpp` | `src/app/survey_impl.rs` | Survey command execution and aggregation |
| `ApplicationImpl.cpp`, `TransactionQueue.cpp`, `TxAdverts.cpp`, `TxDemandsManager.cpp` | `src/app/tx_flooding.rs` | Tx advert/demand scheduling; classic flood budget rounding matches stellar-core integer arithmetic; broadcast period uses `flood_tx_period_ms` (200 ms, matching `FLOOD_TX_PERIOD_MS`) |
| `Config.h` / `Config.cpp` | `src/config.rs`, `src/compat_config.rs` | Native TOML config plus stellar-core-format translation |
| `CommandHandler.h` / `CommandHandler.cpp` | `src/http/mod.rs`, `src/http/handlers/`, `src/compat_http/` | Native Axum server plus compat wire-format server |
| `QueryServer.h` / `QueryServer.cpp` | `src/http/mod.rs`, `src/http/handlers/query.rs` | Separate query server with snapshot lookups |
| `Maintainer.h` / `Maintainer.cpp` | `src/maintainer.rs` | Automatic background maintenance |
| `PersistentState.h` / `PersistentState.cpp` | `src/app/mod.rs`, `henyey-db` | App owns usage; storage primitives live in `henyey-db` |
| `ApplicationUtils.h` | `src/run_cmd.rs`, `src/catchup_cmd.rs`, `src/app/*.rs` | Runtime subset only; many CLI utilities live elsewhere |
| `Diagnostics.h` / `Diagnostics.cpp` | — | No Rust equivalent in this crate |
| `BannedAccountsPersistor.h` / `BannedAccountsPersistor.cpp` | — | No Rust equivalent in this crate |

## Component Mapping

### Application core (`src/app/mod.rs`, `src/app/lifecycle.rs`, `src/app/ledger_close.rs`, `src/app/catchup_impl.rs`, `src/app/consensus.rs`)

Corresponds to: `Application.h`, `ApplicationImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Application::create()` | `App::new()` | Full |
| `initialize()` | `App::new()` initialization path | Full |
| `resetLedgerState()` | — | None |
| `timeNow()` | runtime clock usage in `App` | Full |
| `getConfig()` | `App::config()` | Full |
| `getState()` / `getStateHuman()` | `App::state()`, `AppState` display | Full |
| `isStopping()` | shutdown state tracking | Full |
| `getMetrics()` / `clearMetrics()` | ad-hoc counters plus `App::clear_metrics()` | Partial |
| `syncOwnMetrics()` / `syncAllMetrics()` | — | None |

> **Note:** `stellar_ledger_close_time_ms` was removed — its data is captured by the `stellar_ledger_close_duration_seconds` histogram.

| subsystem getters (`getLedgerManager`, `getBucketManager`, `getHerder`, `getOverlayManager`, `getDatabase`) | direct `App` accessors and owned fields | Full |
| `getHistoryArchiveManager()` / `getHistoryManager()` / `getHerderPersistence()` / `getInvariantManager()` / `getPersistentState()` / `getWorkScheduler()` / `getStatusManager()` | distributed across `App` + sibling crates | Partial |
| `getBannedAccountsPersistor()` | — | None |
| `postOnMainThread()` / background posting helpers | Tokio spawning and async tasks | Full |
| `start()` / `gracefulStop()` / `joinAllThreads()` | `App::run()`, `App::shutdown()`, task shutdown | Full |
| `manualClose()` | `App::manual_close_ledger()` | Partial |
| `applyCfgCommands()` / `reportCfgMetrics()` | — | None |
| `getJsonInfo()` / `reportInfo()` | `App::info()`, `print_startup_info()` | Full |
| `scheduleSelfCheck()` | `App::self_check()` only | Partial |
| `getNetworkID()` / `validateNetworkPassphrase()` | `App::network_id()`, startup validation | Full |

### Configuration (`src/config.rs`, `src/compat_config.rs`)

Corresponds to: `Config.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Config()` | `AppConfig::default()` | Full |
| `load(filename)` | `AppConfig::from_file()` | Full |
| `load(istream)` | — | None |
| `adjust()` | — | None |
| `resolveNodeID()` / `toShortString()` / `toStrKey()` / `toString(qset)` | — | None |
| network, overlay, history, maintenance, metadata, diagnostics, query fields | `AppConfig` sub-structs | Full |
| `FORCE_SCP`, `MANUAL_CLOSE`, `CATCHUP_COMPLETE`, `CATCHUP_RECENT` | native fields in `AppConfig` | Full |
| stellar-core flat config parsing | `translate_stellar_core_config()` | Full |
| `PREFERRED_PEER_KEYS`, `PREFERRED_PEERS_ONLY` | Translated via strict helpers in compat config; passed to overlay | Full |
| `INVARIANT_CHECKS`, `INVARIANT_EXTRA_CHECKS`, `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY` | Validated in compat translation; non-default values of the two security-relevant keys are rejected (no InvariantManager subsystem in henyey). The frequency tuning knob is silently accepted as known-but-unsupported. | Partial |
| testing knobs (`ARTIFICIALLY_*`, `LOADGEN_*`, `APPLY_LOAD_*`) | small supported subset only | Partial |
| helper methods such as `modeDoesCatchupWithBucketList()`, `allBucketsInMemory()`, `parallelLedgerClose()`, `setNoListen()`, `setNoPublish()` | — | None |

### Run and catchup orchestration (`src/run_cmd.rs`, `src/catchup_cmd.rs`, `src/app/catchup_impl.rs`, `src/app/publish.rs`)

Corresponds to: `ApplicationUtils.h` runtime subset

| stellar-core | Rust | Status |
|--------------|------|--------|
| `setupApp()` | `App::new()` + `run_node()` setup path | Full |
| `runApp()` | `run_node()` | Full |
| `initializeDatabase()` | DB init inside `App::new()` | Full |
| `selfCheck()` | `App::self_check()` | Partial |
| `catchup()` | `run_catchup()` / `App::catchup_with_mode()` | Full |
| `applyBucketsForLCL()` | catchup bucket-apply flow | Full |
| `publish()` | publish flow in `src/app/publish.rs` | Full |
| `writeCatchupInfo()` | — | None |
| `setForceSCPFlag()` / `httpCommand()` / `mergeBucketList()` / `dumpStateArchivalStatistics()` / `calculateAssetSupply()` / `reportLastHistoryCheckpoint()` | — | None |

### HTTP command surface (`src/http/handlers/`, `src/compat_http/handlers/`)

Corresponds to: `CommandHandler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `info()` / `metrics()` / `peers()` / `quorum()` / `scpInfo()` / `tx()` / `upgrades()` / `dumpProposedSettings()` / `sorobanInfo()` | native and compat handlers | Full |
| `maintenance()` / `clearMetrics()` / `selfCheck()` | native and compat handlers | Full |
| `generateLoad()` | native and compat handlers (feature-gated via `loadgen`) | Full |
| `testAcc()` | compat handler with deterministic key derivation | Full |
| `manualClose()` | works, but explicit seq/time params are rejected in compat/native handlers | Partial |
| `connect()` / `dropPeer()` / `unban()` / `bans()` | native handlers work; compat handlers are placeholders or incomplete | Partial |
| `ll()` | native dynamic log control works; compat handler is minimal | Partial |
| `logRotate()` | placeholder response only | Partial |
| `banaccounts()` / `unbanaccounts()` | — | None |
| legacy survey commands (`surveyTopology()`, `stopSurvey()`, `getSurveyResult()`) | — | None |
| time-sliced survey commands (`startSurveyCollecting()`, `stopSurveyCollecting()`, `surveyTopologyTimeSliced()`) | native handlers implemented; compat endpoints are stubs | Partial |

### Query server (`src/http/mod.rs`, `src/http/handlers/query.rs`)

Corresponds to: `QueryServer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `QueryServer(...)` | `QueryServer::new()` | Full |
| `getLedgerEntryRaw()` | `getledgerentryraw_handler()` | Full |
| `getLedgerEntry()` | `getledgerentry_handler()` | Full |

### Persistent state and maintenance (`src/app/mod.rs`, `src/maintainer.rs`)

Corresponds to: `PersistentState.h`, `Maintainer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState::getState()` / `setMainState()` / `setMiscState()` | `henyey-db::StateQueries` usage from `App` | Full |
| `getSCPStateAllSlots()` | `ScpQueries::get_scp_state_all_slots()` | Full |
| `getTxSetsForAllSlots()` / `setSCPStateV1ForSlot()` | partial `ScpQueries` support | Partial |
| `getTxSetHashesForAllSlots()` / `hasTxSet()` / `deleteTxSets()` / rebuild-offer-table flags | — | None |
| `Maintainer::start()` / `performMaintenance()` | `Maintainer::start()`, `perform_maintenance()`, `perform_maintenance_with_count()` | Full |

### Surveys, metadata, and logging (`src/survey.rs`, `src/meta_stream.rs`, `src/logging.rs`)

Corresponds to: survey parts of `CommandHandler.cpp`, metadata output in `ApplicationImpl.cpp`, and `Diagnostics.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| time-sliced survey collection/reporting | `SurveyDataManager` | Full |
| survey message dedup/rate limiting | `SurveyMessageLimiter` | Full |
| metadata output stream (`METADATA_OUTPUT_STREAM`) | `MetaStreamManager` | Full |
| rotating debug metadata segments | `MetaStreamManager::maybe_rotate_debug_stream()` | Full |
| dynamic partition log levels | `LogLevelHandle` | Full |
| `diagnostics::bucketStats()` | — | None |

### Account-ban persistence

Corresponds to: `BannedAccountsPersistor.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| persisted banned-account store | — | None |
| `FILTERED_G_ADDRESSES` migration | — | None |
| `banaccounts` / `unbanaccounts` HTTP integration | — | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `AppConnector` | Tokio + `Arc`/locks replace thread-isolation helper API |
| `VirtualClock` / explicit `ThreadType` / per-subsystem io_context getters | Runtime uses Tokio tasks instead of ASIO thread pools |
| `TmpDirManager`, `ProcessManager`, raw `LedgerTxnRoot` exposure | Different runtime architecture; not part of `henyey-app`'s public API |
| Protocol-23 corruption verifier/reconciler | Repository targets protocol 24+ only |
| `CommandLine.h`, `SettingsUpgradeUtils.h`, and `dumpxdr.h` utilities | Owned by the `henyey` binary crate, not `henyey-app` |
| `minimalDBForInMemoryMode()` / `canRebuildInMemoryLedgerFromBuckets()` | Test-only upstream helpers not mirrored in this crate |
| `BUILD_TESTS`-only overlay toggle (`getRunInOverlayOnlyMode` / `setRunInOverlayOnlyMode`) | Rust test strategy uses different hooks and feature gates |
| `testTx()` | Test-only wire-format endpoint; no production use |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `BannedAccountsPersistor` and `FILTERED_G_ADDRESSES` flow | High | No persisted banned-account subsystem or admin endpoints |
| Compat `connect` / `droppeer` / `unban` / `bans` behavior | Medium | Compat handlers mostly return placeholders instead of mutating state |
| Compat time-sliced survey admin routes | Medium | Native routes work; compat routes are still stubs |
| `manualclose` explicit sequence/close-time parameters | Medium | Upstream standalone semantics are not exposed by handlers |
| Scheduled online self-check parity | Medium | Manual self-check exists, but upstream periodic scheduling test is unmatched |
| `PersistentState` tx-set hash helpers and rebuild flags | Medium | Several SCP persistence helpers remain absent |
| `diagnostics::bucketStats()` | Low | No bucket statistics offline tool |
| `Config` helper methods (`resolveNodeID`, stringifiers, adjust/no-listen/no-publish) | Low | Native config model omits these convenience APIs |
| `writeCatchupInfo()` | Low | No catchup-info file output helper |

## Architectural Differences

1. **Async Runtime**
   - **stellar-core**: ASIO `io_context` instances split across main, worker, overlay, eviction, and ledger-close threads.
   - **Rust**: A Tokio runtime drives all async work, with blocking work isolated only where needed.
   - **Rationale**: The Rust crate centralizes concurrency around futures/tasks instead of exposing thread-specific interfaces.

2. **Configuration model**
   - **stellar-core**: A large mutable `Config` object with many testing-only knobs and helper methods.
   - **Rust**: Serde-backed typed config structs plus a separate compatibility translator for flat stellar-core TOML.
   - **Rationale**: The crate keeps runtime config strongly typed while still accepting stellar-core captive-core files.

3. **HTTP surfaces**
   - **stellar-core**: One command server defines both behavior and wire format.
   - **Rust**: Native Axum endpoints and a second compatibility server coexist.
   - **Rationale**: The native API is cleaner for henyey callers, while compat routes only cover the stellar-rpc subset currently needed.

4. **Persistence split**
   - **stellar-core**: `PersistentState` lives inside `src/main/`.
   - **Rust**: `henyey-app` owns restart/catchup policy while low-level persistence APIs live in `henyey-db`.
   - **Rationale**: The workspace factors storage concerns into a dedicated crate instead of keeping them inside the app layer.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Config and compat translation | 9 TEST_CASE / 21 SECTION | 64 `#[test]` | Strong coverage for loading, validation, and captive-core translation |
| Command handler / compat HTTP | 5 TEST_CASE / 27 SECTION | 46 `#[test]` | Includes handler helpers, generateLoad, testacc; some compat behaviors are still stubs |
| Query server | 1 TEST_CASE / 9 SECTION | 7 `#[test]` | Good coverage of lookup ordering and validation |
| Run/catchup utilities | 4 TEST_CASE / 5 SECTION | 19 `#[test]` | Target parsing and run-mode helpers are well covered |
| Self-check scheduling | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | No dedicated periodic self-check scheduling tests |
| Maintenance | 0 TEST_CASE / 0 SECTION | 12 `#[test]` | Strong regression coverage for retention thresholds |
| Banned accounts | 4 TEST_CASE / 21 SECTION | 0 `#[test]` | Subsystem not implemented; upstream has comprehensive tests |
| App core types/runtime | — | 35 `#[test]` | App state, recovery bookkeeping, and runtime helpers |
| Metadata and logging | — | 10 `#[test]` | Basic coverage for stream rotation and log-level handling |

### Test Gaps

- Compat admin endpoints lack parity-style integration tests for real side effects (`connect`, `droppeer`, `unban`, survey control).
- There is no Rust equivalent of upstream's scheduled online self-check test in `SelfCheckTests.cpp`.
- Account-ban persistence has no Rust tests because the subsystem is not implemented. Upstream has 4 TEST_CASE / 21 SECTION.
- HTTP threaded server behavior (3 TEST_CASE upstream in `HttpThreadedTests.cpp`) has no direct equivalent.

## Verification Results

- **Testnet verification**: Node successfully syncs and tracks consensus on testnet, closing ledgers in parity with stellar-core validators.
- **Catchup gap recovery**: Successfully bridges 20-30 slot gaps between catchup checkpoint and live consensus (verified January 2026).
- **Event loop stability**: Multiple event loop freeze bugs identified and fixed (February 2026): blocking flood demand sends, unbounded buffered ledger close loops, blocking bucket GC during catchup, and SCP drain starvation.
- **Post-catchup convergence**: Fixed several convergence failures including dead loops targeting stale checkpoints, deadlocks from frozen `latest_externalized`, and SCP EXTERNALIZE envelope emission for validator nodes (March–April 2026).
- **TX queue parity**: Implemented stellar-core `updateQueue` semantics with correct invalidation and revalidation ordering (March 2026).
- **Audit fixes**: Resolved audit findings including config passphrase matching, quorum threshold rounding, unsolicited quorum set rejection, compat config validator entry validation, and compat `SURVEYOR_KEYS` translation (March–April 2026).
- **Survey protocol**: Time-sliced surveys successfully collect and report topology data from testnet peers.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 90 |
| Gaps (None + Partial) | 39 |
| Intentional Omissions | 24 |
| **Parity** | **90 / (90 + 39) = 70%** |
