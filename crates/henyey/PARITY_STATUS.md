# stellar-core Parity Status

**Crate**: `henyey`
**Upstream**: `stellar-core/src/main/`
**Overall Parity**: 56%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| CLI entrypoint and dispatch | Full | `clap`-based dispatch covers the core command flow |
| Node lifecycle commands | Full | `run`, `catchup`, `publish`, `new-db`, `upgrade-db` work |
| History and offline admin | Full | `new-hist`, `offline-info`, `verify-checkpoints` implemented |
| HTTP/admin compatibility | Full | `http-command`, `self-check`, `dump-ledger`, `force-scp` implemented |
| Key and ID helpers | Full | `convert-id` and seed generation both exist |
| Quorum intersection analysis | Partial | V1 JSON check only; SAT solver path absent |
| Settings upgrade transactions | Partial | Command exists, but helper behavior diverges in details |
| Load and benchmark tooling | Partial | `apply-load` exists; `pregenerate-loadgen-txs` absent |
| XDR inspection and signing tools | None | `print-xdr`, `dump-xdr`, `sign-transaction` absent |
| Diagnostic maintenance commands | None | Bucket, archival, asset-supply diagnostics absent |
| History/reporting extras | None | Publish-queue and last-checkpoint reporting absent |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `main.cpp` | `main.rs` | Process entrypoint and startup wiring |
| `CommandLine.h` / `CommandLine.cpp` | `main.rs` | CLI parsing, dispatch, and top-level command handlers |
| `ApplicationUtils.h` / `ApplicationUtils.cpp` | `main.rs` | CLI command wrappers delegate into `henyey-app` and sibling crates |
| `SettingsUpgradeUtils.h` / `SettingsUpgradeUtils.cpp` | `settings_upgrade.rs` | Soroban settings-upgrade transaction builder |
| `dumpxdr.h` / `dumpxdr.cpp` | `main.rs`, `settings_upgrade.rs` | Generic XDR tools are missing; only settings-upgrade signing pieces were ported |
| `CommandHandler.h` / `CommandHandler.cpp` | (in `henyey-app` crate) | HTTP command handler lives in the app crate, not the CLI binary |
| `Config.h` / `Config.cpp` | (in `henyey-app::config`) | Config parsing and validation is in the app crate |

## Component Mapping

### CLI Core (`main.rs`)

Corresponds to: `CommandLine.h`, `main.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `main()` | `main()` | Full |
| `handleCommandLine()` | `Cli::parse()` + command dispatch in `main()` | Full |
| `runVersion()` | `cmd_version()` | Full |
| `writeWithTextFlow()` | `clap` help generation | Full |

### Operational Commands (`main.rs`)

Corresponds to: `CommandLine.cpp`, `ApplicationUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `run()` / `setupApp()` / `runApp()` | `cmd_run()` | Full |
| `runCatchup()` / `catchup()` | `cmd_catchup()` | Full |
| `runNewDB()` / `initializeDatabase()` | `cmd_new_db()` | Full |
| `runUpgradeDB()` | `cmd_upgrade_db()` | Full |
| `runPublish()` / `publish()` | `cmd_publish_history()` | Full |
| `runWriteVerifiedCheckpointHashes()` | `cmd_verify_checkpoints()` | Full |
| `runNewHist()` / `initializeHistories()` | `cmd_new_hist()` | Full |
| `runOfflineInfo()` / `showOfflineInfo()` | `cmd_offline_info()` | Full |
| `runHttpCommand()` / `httpCommand()` | `cmd_http_command()` | Full |
| `runSelfCheck()` / `selfCheck()` | `cmd_self_check()` | Full |
| `runDumpLedger()` / `dumpLedger()` | `cmd_dump_ledger()` | Full |
| `runForceSCP()` / `setForceSCPFlag()` | `cmd_force_scp()` | Full |
| `runConvertId()` | `cmd_convert_id()` | Full |
| `runGenSeed()` / `genSeed()` | `cmd_new_keypair()` | Full |
| `runApplyLoad()` | `cmd_apply_load()` | Full |

### Quorum Intersection (`quorum_intersection.rs`)

Corresponds to: `CommandLine.cpp` (`runCheckQuorumIntersection`), `ApplicationUtils.h` (`checkQuorumIntersectionFromJson`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runCheckQuorumIntersection()` / `checkQuorumIntersectionFromJson()` | `check_quorum_intersection_from_json()` | Partial |

### Settings Upgrade Transactions (`settings_upgrade.rs`)

Corresponds to: `SettingsUpgradeUtils.h`, `CommandLine.cpp` (`getSettingsUpgradeTransactions`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getSettingsUpgradeTransactions()` + `getWasmRestoreTx()` + `getUploadTx()` + `getCreateTx()` + `getInvokeTx()` | `settings_upgrade::run()` and helpers | Partial |

### Diagnostics and XDR Tools (`main.rs`)

Corresponds to: `CommandLine.cpp`, `dumpxdr.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runPrintPublishQueue()` | -- | None |
| `runReportLastHistoryCheckpoint()` | -- | None |
| `runReplayDebugMeta()` | -- | None |
| `runGenerateSyntheticLoad()` (`pregenerate-loadgen-txs`) | -- | None |
| `runCalculateAssetSupply()` | -- | None |
| `runMergeBucketList()` | -- | None |
| `runDumpStateArchivalStatistics()` | -- | None |
| `runDumpWasm()` | -- | None |
| `runPrintXdr()` / `printXdr()` | -- | None |
| `runDumpXDR()` / `dumpXdrStream()` | -- | None |
| `runEncodeAsset()` | -- | None |
| `runSecToPub()` / `priv2pub()` | -- | None |
| `runSignTransaction()` / `signtxn()` / `signtxns()` | -- | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `checkXDRFileIdentity()` | Pure Rust build uses one XDR stack, so C++/Rust hash cross-checks are unnecessary |
| `checkStellarCoreMajorVersionProtocolIdentity()` | Cargo versioning and protocol gating replace upstream release-tag validation |
| `loadXdr()` / `rebuildLedgerFromBuckets()` | `BUILD_TESTS`-only utilities; workspace uses regular Rust tests instead |
| `runFuzz()` / `runGenFuzz()` / `runTest()` | Fuzzing and test execution are handled by Cargo tooling, not the binary |
| `minimalDBForInMemoryMode()` / `canRebuildInMemoryLedgerFromBuckets()` | In-memory mode is deprecated and intentionally unsupported |
| `setAuthenticatedLedgerHashPair()` | `--start-at-ledger` and `--start-at-hash` are accepted only as no-op compat flags |
| `applyBucketsForLCL()` | Ledger rebuild internals live in lower crates rather than the CLI binary |
| `getStellarCoreMajorReleaseVersion()` | Helper is unnecessary because `cmd_version()` emits Rust-owned version strings directly |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `runCheckQuorumIntersection()` V2 mode | Medium | Missing `--v2`, SAT solver, result JSON, critical-groups, and resource limits |
| `getSettingsUpgradeTransactions()` parity details | Medium | No upstream-style `ConfigUpgradeSet` validation; contract salt is fixed instead of generated |
| `runSecToPub()` / `priv2pub()` | Medium | No dedicated secret-to-public compatibility command |
| `runSignTransaction()` / `signtxn()` / `signtxns()` | Medium | Generic transaction signing CLI is absent outside settings-upgrade flow |
| `runPrintXdr()` / `runDumpXDR()` / `runEncodeAsset()` | Low | XDR inspection and encoding helpers are not exposed |
| `runDumpWasm()` | Low | No CLI to extract WASM blobs from ledger state |
| `runMergeBucketList()` / `runDumpStateArchivalStatistics()` / `runCalculateAssetSupply()` / `diagBucketStats()` | Low | Diagnostic maintenance commands are still missing |
| `runPrintPublishQueue()` / `runReportLastHistoryCheckpoint()` | Low | Archive/reporting helper commands are not implemented |
| `runReplayDebugMeta()` | Low | No local debug-meta replay path |
| `runGenerateSyntheticLoad()` (`pregenerate-loadgen-txs`) | Low | No standalone transaction pre-generation command |

## Architectural Differences

1. **CLI framework**
   - **stellar-core**: Uses `clara` with manually registered command handlers.
   - **Rust**: Uses `clap` derive parsing and enum-based dispatch.
   - **Rationale**: `clap` is the idiomatic Rust choice and keeps the command surface centralized.

2. **Delegation boundary**
   - **stellar-core**: `src/main/` owns both CLI glue and much of the application bootstrapping logic.
   - **Rust**: `crates/henyey` stays thin and delegates most runtime work to `henyey-app`, `henyey-history`, and related crates.
   - **Rationale**: The workspace splits orchestration, ledger logic, and CLI wiring into separate crates for maintainability.

3. **Startup validation**
   - **stellar-core**: Performs explicit XDR identity and release/protocol checks during process startup.
   - **Rust**: Relies on a single Rust XDR implementation and compile-time workspace versioning (`check_version_protocol_invariant`).
   - **Rationale**: The Rust binary has no mixed-language bridge that needs runtime identity validation.

4. **Runtime model**
   - **stellar-core**: Runs command handlers on a synchronous virtual-clock stack.
   - **Rust**: Uses `tokio` and async command handlers for networked operations.
   - **Rationale**: Async I/O is the natural fit for Rust service code and keeps compatibility servers simple.

5. **Settings-upgrade transaction generation**
   - **stellar-core**: Builds upgrade transactions with upstream validation helpers and a generated contract salt.
   - **Rust**: Bundles the same upgrade WASM and fee structure, but uses a fixed salt and skips upstream validation checks.
   - **Rationale**: The command was ported for practical compatibility first; exact helper parity is still incomplete.

6. **Henyey-only commands**
   - **Rust adds**: `verify-execution`, `debug-bucket-entry`, `bucket-info`, `sample-config`, `verify-history`, `info`, `compare-checkpoint`, `--local` mode.
   - These are development, debugging, and operational tools not present in stellar-core. They do not affect parity %.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| CLI parsing and dispatch | 0 TEST_CASE / 0 SECTION | 5 `#[test]` | Basic `clap` command parsing is covered |
| Load-generation mode parsing | 0 TEST_CASE / 0 SECTION | 13 `#[test]` | Covers compatibility spellings, deprecated `create` mode, and case-insensitive parsing |
| Application-utils style helpers | 4 TEST_CASE / 5 SECTION | 6 `#[test]` | Rust focuses on genesis/database helper behavior |
| Config and command-handler compat | 14 TEST_CASE / 48 SECTION | 0 `#[test]` | Most equivalent coverage lives in `henyey-app`, not this crate |
| Self-check | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | No direct regression tests for the CLI command |
| Quorum intersection | 0 TEST_CASE / 0 SECTION | 5 `#[test]` | Only the V1 JSON path is exercised |
| Settings upgrade transactions | 0 TEST_CASE / 0 SECTION | 0 `#[test]` | No direct coverage for `get-settings-upgrade-txs` yet |

### Test Gaps

- `get-settings-upgrade-txs` has no crate-local regression tests, despite several parity-sensitive fee and XDR details.
- `check-quorum-intersection` only tests the brute-force V1 path; none of the missing V2/JSON-output behavior is covered.
- `self-check`, `print-publish-queue`, and `report-last-history-checkpoint` have no direct CLI-crate tests.
- Compatibility-heavy config and HTTP command behavior is tested mainly in `henyey-app`, leaving this crate light on end-to-end command coverage.

## Verification Results

### Testnet Execution Verification (January 2026)

The `verify-execution` tool compares transaction execution against CDP (Crypto Data Platform) metadata from stellar-core.

**Ledgers 15001-30000:**
| Metric | Count | Rate |
|--------|-------|------|
| Phase 1 fee calculations matched | 30,702 | 100% |
| Phase 1 fee calculations mismatched | 0 | 0% |
| Phase 2 execution matched | 30,176 | 98.3% |
| Phase 2 execution mismatched | 526 | 1.7% |
| Header verifications passed | 15,000 | 100% |

**Ledgers 30001-50000:**
| Metric | Count | Rate |
|--------|-------|------|
| Phase 1 fee calculations matched | 36,510 | 100% |
| Phase 1 fee calculations mismatched | 0 | 0% |
| Phase 2 execution matched | 36,248 | 99.3% |
| Phase 2 execution mismatched | 262 | 0.7% |
| Header verifications passed | 20,000 | 100% |
| Header mismatches | 0 | 0% |

**Ledgers 30001-33000 (after eviction scan fix):**
| Metric | Count | Rate |
|--------|-------|------|
| Phase 1 fee calculations matched | 7,212 | 100% |
| Phase 1 fee calculations mismatched | 0 | 0% |
| Phase 2 execution matched | 6,596 | 91.5% |
| Phase 2 execution mismatched | 616 | 8.5% |
| Header verifications passed | 3,000 | 100% |
| Header mismatches | 0 | 0% |

### Verified Components

- Transaction execution verified against CDP for testnet ledgers
- Bucket list hash computation verified against history archives
- Header hash computation verified against network
- SCP message handling verified through overlay tests
- Publish/verify cycle verified for local and command-based archives
- Phase 1 fee calculation: 100% parity (surge pricing support)
- Classic transaction execution: >99% parity
- Soroban transaction execution: ~98% parity (error code mapping differences)

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 19 |
| Gaps (None + Partial) | 15 |
| Intentional Omissions | 8 |
| **Parity** | **19 / (19 + 15) = 56%** |
